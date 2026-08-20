use std::error::Error;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use control::{ControlServer, ControlServerConfig, MediaEngineSlot};
use media::RecordKind;
use media_engine::{MediaEngine, MediaEngineConfig};
use ptz::PtzController;
use rtsp::{RtspServer, RtspServerConfig};

pub mod control;
pub mod media;
pub mod media_engine;
pub mod ptz;
pub mod rtsp;

/// Default camera-local RTSP bind address.
const DEFAULT_RTSP_BIND_ADDRESS: &str = "0.0.0.0:554";

/// Environment override for development and board-specific port selection.
const RTSP_BIND_ADDRESS_ENV: &str = "RTSP_BIND_ADDRESS";

/// Environment override for the camera-local HTTP control listener.
const CONTROL_BIND_ADDRESS_ENV: &str = "CONTROL_BIND_ADDRESS";

/// Explicit audio enable override used during board bring-up.
const STANDALONE_AUDIO_ENV: &str = "STANDALONE_AUDIO";

/// Persistent marker that opts deployed camera boots into microphone RTSP.
const STANDALONE_AUDIO_MARKER: &str = "/home/nvram/system/.standalone_audio_enable";

fn main() {
    initialize_logging();
    if let Err(error) = run() {
        tracing::error!(%error, "camera firmware stopped");
        std::process::exit(1);
    }
}

fn initialize_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .try_init();
}

fn run() -> Result<(), Box<dyn Error + Send + Sync>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_firmware())
}

async fn run_firmware() -> Result<(), Box<dyn Error + Send + Sync>> {
    let bind_address = std::env::var(RTSP_BIND_ADDRESS_ENV)
        .unwrap_or_else(|_| DEFAULT_RTSP_BIND_ADDRESS.to_string())
        .parse::<SocketAddr>()?;
    let mut rtsp_config = RtspServerConfig::new(bind_address);
    let audio_enabled = audio_enabled();
    rtsp_config.audio_enabled = audio_enabled;
    let rtsp = RtspServer::start(rtsp_config)?;
    tracing::info!(
        address = %rtsp.local_addr(),
        path = rtsp.path(),
        audio_enabled,
        "RTSP service listening"
    );

    let control_address = std::env::var(CONTROL_BIND_ADDRESS_ENV)
        .unwrap_or_else(|_| format!("0.0.0.0:{}", control::DEFAULT_CONTROL_PORT))
        .parse::<SocketAddr>()?;
    let media_slot = MediaEngineSlot::default();
    let ptz = match PtzController::open() {
        Ok(ptz) => Some(Arc::new(ptz)),
        Err(error) => {
            tracing::warn!(%error, "PTZ device unavailable; PTZ control disabled");
            None
        }
    };
    let control = ControlServer::start(
        ControlServerConfig::new(control_address),
        media_slot.clone(),
        ptz,
    )?;
    tracing::info!(address = %control.local_addr(), "HTTP control service listening");

    let stop = Arc::new(AtomicBool::new(false));
    let media_stop = Arc::clone(&stop);
    let media_rtsp = rtsp.clone();
    let media_slot_for_task = media_slot.clone();
    let mut media_task = tokio::task::spawn_blocking(move || {
        run_media_loop(
            media_rtsp,
            MediaEngineConfig::default(),
            media_stop,
            media_slot_for_task,
        )
    });

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let media_result = tokio::select! {
        result = &mut media_task => result,
        _ = terminate.recv() => {
            tracing::info!("received SIGTERM; stopping camera firmware");
            stop.store(true, Ordering::Release);
            // The media source is expected to deliver at the camera cadence;
            // this join lets its owner drop and gracefully release hardware.
            media_task.await
        }
        _ = interrupt.recv() => {
            tracing::info!("received SIGINT; stopping camera firmware");
            stop.store(true, Ordering::Release);
            media_task.await
        }
    };

    match media_result {
        Ok(result) => result?,
        Err(error) => return Err(Box::new(error)),
    }
    Ok(())
}

fn audio_enabled() -> bool {
    if let Ok(value) = std::env::var(STANDALONE_AUDIO_ENV) {
        return matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        );
    }
    Path::new(STANDALONE_AUDIO_MARKER).is_file()
}

fn run_media_loop(
    rtsp: RtspServer,
    config: MediaEngineConfig,
    stop: Arc<AtomicBool>,
    media_slot: MediaEngineSlot,
) -> std::io::Result<()> {
    let mut engine = MediaEngine::start(config)?;
    let command_handle = engine.command_handle();
    media_slot.set(command_handle.clone())?;
    if let Err(error) = rtsp.set_talkback_sink(Arc::new(command_handle)) {
        let _ = media_slot.clear();
        return Err(error);
    }
    let result = (|| -> std::io::Result<()> {
        while !stop.load(Ordering::Acquire) {
            match engine.read_record()? {
                Some(record) if record.kind.is_video() => {
                    rtsp.push_video_record(&record)?;
                }
                Some(record) if record.kind == RecordKind::AudioCapture => {
                    rtsp.push_audio_record(&record)?;
                }
                Some(_) => {}
                None => break,
            }
        }
        Ok(())
    })();
    let talkback_clear_result = rtsp.clear_talkback_sink();
    let clear_result = media_slot.clear();
    let shutdown_result = engine.shutdown();
    result?;
    talkback_clear_result?;
    clear_result?;
    shutdown_result
}
