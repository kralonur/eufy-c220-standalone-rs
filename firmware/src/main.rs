use std::error::Error;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use control::{ControlServer, ControlServerConfig, MediaEngineSlot};
use digest::Lockout;
use media::RecordKind;
use media_engine::{MediaEngine, MediaEngineConfig, MediaEngineStopHandle};
use ptz::PtzController;
use rtsp::{RtspServer, RtspServerConfig, RtspsConfig};

pub mod clock;
pub mod control;
pub mod digest;
pub mod identity;
pub mod media;
pub mod media_engine;
pub mod ptz;
pub mod rtsp;
pub mod security;
pub mod tls;

/// Environment override for the plaintext RTSP port. Port 0 retires it.
const RTSP_PORT_ENV: &str = "RTSP_PORT";

/// Environment override for development and board-specific bind selection.
const RTSP_BIND_ADDRESS_ENV: &str = "RTSP_BIND_ADDRESS";

/// Environment override for the camera-local HTTP control listener.
const CONTROL_BIND_ADDRESS_ENV: &str = "CONTROL_BIND_ADDRESS";

/// Environment override for the encrypted RTSP listener.
const RTSPS_PORT_ENV: &str = "RTSPS_PORT";

/// PEM certificate and private-key paths shared by RTSPS and the control API.
const RTSP_CERT_FILE_ENV: &str = "RTSP_CERT_FILE";
const RTSP_KEY_FILE_ENV: &str = "RTSP_KEY_FILE";
/// Whether the control API should use the shared TLS acceptor when available.
const API_TLS_ENV: &str = "API_TLS";
/// Explicit, one-boot escape hatch for board bring-up. Production startup never
/// sets this variable; without it the listener policy below is fail-closed.
const INSECURE_BRINGUP_ENV: &str = "STANDALONE_INSECURE_BRINGUP";
/// Digest credential environment names populated by the boot hook.
const RTSP_USERNAME_ENV: &str = "RTSP_USERNAME";
const RTSP_PASSWORD_FILE_ENV: &str = "RTSP_PASSWORD_FILE";
const HTTP_USERNAME_ENV: &str = "HTTP_USERNAME";
const HTTP_PASSWORD_FILE_ENV: &str = "HTTP_PASSWORD_FILE";

/// Explicit audio enable override used during board bring-up.
const STANDALONE_AUDIO_ENV: &str = "STANDALONE_AUDIO";

/// Optional LAN time server for the SNTP clock sync. Without it the default
/// gateway is asked, which is the only other thing guaranteed to be on the LAN.
const NTP_HOST_ENV: &str = "NTP_HOST";

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
    // The board has no RTC and boots with a time restored from NVRAM, which can
    // be hours stale. Start the sync first: it retries while Wi-Fi and the
    // default route come up, and it must never delay the services below.
    tokio::spawn(clock::sync_until_set(std::env::var(NTP_HOST_ENV).ok()));

    let rtsp_port = security::port(RTSP_PORT_ENV, rtsp::DEFAULT_RTSP_PORT)?;
    let bind_address = std::env::var(RTSP_BIND_ADDRESS_ENV)
        .unwrap_or_else(|_| format!("0.0.0.0:{rtsp_port}"))
        .parse::<SocketAddr>()?;
    let rtsps_port = security::port(RTSPS_PORT_ENV, 0)?;
    let api_tls = security::flag(API_TLS_ENV, true)?;
    let insecure_bringup = security::flag(INSECURE_BRINGUP_ENV, false)?;
    let cert_file = security::path(RTSP_CERT_FILE_ENV);
    let key_file = security::path(RTSP_KEY_FILE_ENV);

    if insecure_bringup {
        tracing::warn!(
            "explicit insecure bring-up mode enabled; plaintext/anonymous services are permitted"
        );
    } else {
        if rtsp_port != 0 {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "secure mode requires RTSP_PORT=0; use RTSPS instead",
            )));
        }
        if rtsps_port == 0 {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "secure mode requires a nonzero RTSPS_PORT",
            )));
        }
        if !api_tls {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "secure mode requires API_TLS=1",
            )));
        }
    }

    // Build one acceptor and share it. Secure mode requires a valid acceptor
    // for both media and control; only the explicit bring-up escape hatch may
    // fall back to plaintext control.
    let mut tls_acceptor = None;
    if rtsps_port != 0 || api_tls {
        match (cert_file.as_deref(), key_file.as_deref()) {
            (Some(cert_file), Some(key_file)) => {
                // First boot: the camera creates its own certificate rather than
                // requiring the owner to run a certificate authority. An installed
                // certificate is never overwritten, and a generation failure is
                // reported by the acceptor below, which is the authority on whether
                // the pair is usable.
                if !insecure_bringup {
                    match identity::ensure_self_signed(cert_file, key_file) {
                        Ok(identity::Outcome::Generated(names)) => tracing::info!(
                            names = %names.join(", "),
                            "generated a self-signed certificate; clients trust it on first use"
                        ),
                        Ok(identity::Outcome::Existing) => {}
                        Err(error) => {
                            tracing::warn!(%error, "could not generate a self-signed certificate")
                        }
                    }
                }
                match tls::build_acceptor(cert_file, key_file) {
                    Ok(acceptor) => tls_acceptor = Some(acceptor),
                    Err(error) if rtsps_port == 0 => {
                        tracing::warn!(%error, "control API TLS unavailable; serving plaintext")
                    }
                    Err(error) => return Err(Box::new(error)),
                }
            }
            _ if rtsps_port != 0 => {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "RTSPS requires RTSP_CERT_FILE and RTSP_KEY_FILE",
                )));
            }
            _ => tracing::warn!("control API TLS not configured; serving plaintext"),
        }
    }

    let rtsps = if rtsps_port == 0 {
        None
    } else {
        Some(RtspsConfig {
            port: rtsps_port,
            acceptor: tls_acceptor
                .clone()
                .ok_or_else(|| std::io::Error::other("RTSPS TLS acceptor was not built"))?,
        })
    };

    let rtsp_auth = security::load_digest_auth(RTSP_USERNAME_ENV, RTSP_PASSWORD_FILE_ENV)?;
    let http_auth = security::load_digest_auth(HTTP_USERNAME_ENV, HTTP_PASSWORD_FILE_ENV)?;
    if !insecure_bringup && rtsp_auth.is_none() {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "secure mode requires RTSP Digest credentials",
        )));
    }
    if !insecure_bringup && http_auth.is_none() {
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "secure mode requires HTTP Digest credentials",
        )));
    }
    if insecure_bringup && rtsp_auth.is_none() {
        tracing::warn!("RTSP Digest credentials are not configured; RTSP is anonymous");
    }
    if insecure_bringup && http_auth.is_none() {
        tracing::warn!("HTTP Digest credentials are not configured; control API is anonymous");
    }
    let lockout = Arc::new(Lockout::new());
    let audio_enabled = audio_enabled();
    let mut rtsp_config = RtspServerConfig::new(bind_address);
    rtsp_config.plaintext_enabled = rtsp_port != 0;
    rtsp_config.audio_enabled = audio_enabled;
    rtsp_config.auth = rtsp_auth;
    rtsp_config.lockout = Arc::clone(&lockout);
    rtsp_config.rtsps = rtsps;
    let rtsp = RtspServer::start(rtsp_config)?;
    if rtsp_port == 0 {
        tracing::info!("plaintext RTSP listener disabled");
    } else {
        tracing::info!(address = %rtsp.local_addr(), "RTSP service listening");
    }
    if rtsps_port != 0 {
        tracing::info!(port = rtsps_port, "RTSPS service listening");
    }

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
    let control_tls_enabled = api_tls && tls_acceptor.is_some();
    let mut control_config = ControlServerConfig::new(control_address);
    control_config.auth = http_auth;
    control_config.lockout = lockout;
    control_config.tls_acceptor = if api_tls { tls_acceptor } else { None };
    let control = ControlServer::start(control_config, media_slot.clone(), ptz)?;
    tracing::info!(
        address = %control.local_addr(),
        tls = control_tls_enabled,
        "HTTP control service listening"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let media_stop = Arc::clone(&stop);
    let media_stop_handle = MediaEngineStopHandle::default();
    let media_stop_handle_for_task = media_stop_handle.clone();
    let media_rtsp = rtsp.clone();
    let media_slot_for_task = media_slot.clone();
    let mut media_task = tokio::task::spawn_blocking(move || {
        let media_config = MediaEngineConfig {
            audio_enabled,
            ..MediaEngineConfig::default()
        };
        run_media_loop(
            media_rtsp,
            media_config,
            media_stop,
            media_slot_for_task,
            media_stop_handle_for_task,
            audio_enabled,
        )
    });

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let media_result = tokio::select! {
        result = &mut media_task => result,
        _ = terminate.recv() => {
            tracing::info!("received SIGTERM; stopping camera firmware");
            stop.store(true, Ordering::Release);
            media_stop_handle.request_shutdown();
            media_task.await
        }
        _ = interrupt.recv() => {
            tracing::info!("received SIGINT; stopping camera firmware");
            stop.store(true, Ordering::Release);
            media_stop_handle.request_shutdown();
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
    stop_handle: MediaEngineStopHandle,
    audio_enabled: bool,
) -> std::io::Result<()> {
    let mut engine = MediaEngine::start(config, stop_handle)?;
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
                    // The native companion currently captures audio regardless
                    // of the RTSP advertisement; discard it when microphone
                    // streaming is not opted in instead of stopping the loop.
                    if audio_enabled {
                        rtsp.push_audio_record(&record)?;
                    }
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
