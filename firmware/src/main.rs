use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use media::RecordKind;
use media_engine::{MediaEngine, MediaEngineConfig};
use rtsp::{RtspServer, RtspServerConfig};

pub mod media;
pub mod media_engine;
pub mod rtsp;

/// Default camera-local RTSP bind address.
const DEFAULT_RTSP_BIND_ADDRESS: &str = "0.0.0.0:554";

/// Environment override for development and board-specific port selection.
const RTSP_BIND_ADDRESS_ENV: &str = "RTSP_BIND_ADDRESS";

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
    let rtsp = RtspServer::start(RtspServerConfig::new(bind_address))?;
    tracing::info!(address = %rtsp.local_addr(), path = rtsp.path(), "RTSP service listening");

    let stop = Arc::new(AtomicBool::new(false));
    let media_stop = Arc::clone(&stop);
    let media_rtsp = rtsp.clone();
    let mut media_task = tokio::task::spawn_blocking(move || {
        run_media_loop(media_rtsp, MediaEngineConfig::default(), media_stop)
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

fn run_media_loop(
    rtsp: RtspServer,
    config: MediaEngineConfig,
    stop: Arc<AtomicBool>,
) -> std::io::Result<()> {
    let mut engine = MediaEngine::start(config)?;
    while !stop.load(Ordering::Acquire) {
        match engine.read_record()? {
            Some(record) if record.kind == RecordKind::Video => {
                rtsp.push_video_record(&record)?;
            }
            Some(_) => {}
            None => break,
        }
    }
    engine.shutdown()
}
