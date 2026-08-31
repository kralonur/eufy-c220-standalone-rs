//! Camera-local HTTP control and talkback service.
//!
//! This service is deliberately small and accepts bounded HTTP/1.1 requests.
//! It forwards only validated speaker/night-mode commands through the shared
//! `C220` media socket. Digest authentication is applied before endpoint
//! dispatch, and production startup requires the listener to use TLS.

use crate::digest::{DigestAuth, Lockout, Outcome};
use crate::media_engine::MediaEngineHandle;
use crate::ptz::{PAN_MAX_DEGREES, PtzController, PtzDirection};
use std::io;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio_rustls::TlsAcceptor;

/// A boxed plaintext or TLS control connection.
pub trait ControlTransport: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> ControlTransport for T {}
type ControlStream = Box<dyn ControlTransport>;

/// Default camera-local HTTP control port.
pub const DEFAULT_CONTROL_PORT: u16 = 8_080;

/// Maximum header bytes accepted from one control client.
const MAX_HEADER_LEN: usize = 8 * 1024;

/// Maximum request body accepted by the control service.
const MAX_BODY_LEN: usize = 64 * 1024;

/// Existing factory telnetd invocation used for a deliberate one-boot shell.
const TELNETD_PATH: &str = "/usr/sbin/telnetd";
const TELNETD_PORT: &str = "23";
const MAINTENANCE_SHELL_PATH: &str = "/bin/sh";

/// AAC talkback access units use the validated 64 ms native cadence.
const AAC_FRAME_DURATION_US: u64 = 64_000;

/// PCM talkback is supplied as mono 16 kHz signed PCM16.
const PCM_SAMPLE_RATE: u64 = 16_000;

/// Native AO volume starts at its validated full-scale default.
const DEFAULT_VOLUME: u32 = 100;

/// A slot that is populated once the media companion has connected.
#[derive(Clone, Default)]
pub struct MediaEngineSlot {
    handle: Arc<Mutex<Option<MediaEngineHandle>>>,
}

impl MediaEngineSlot {
    /// Publish the current media-engine command handle.
    pub fn set(&self, handle: MediaEngineHandle) -> io::Result<()> {
        let mut current = self
            .handle
            .lock()
            .map_err(|_| io::Error::other("media engine slot lock poisoned"))?;
        *current = Some(handle);
        Ok(())
    }

    /// Remove the handle when the media companion is shutting down.
    pub fn clear(&self) -> io::Result<()> {
        let mut current = self
            .handle
            .lock()
            .map_err(|_| io::Error::other("media engine slot lock poisoned"))?;
        *current = None;
        Ok(())
    }

    fn current(&self) -> io::Result<Option<MediaEngineHandle>> {
        self.handle
            .lock()
            .map_err(|_| io::Error::other("media engine slot lock poisoned"))
            .map(|current| current.clone())
    }
}

/// HTTP listener configuration.
#[derive(Clone)]
pub struct ControlServerConfig {
    /// Local address on which the control API listens.
    pub bind_address: SocketAddr,
    /// Digest credentials. `/healthz` remains public; production startup
    /// requires this field to be configured.
    pub auth: Option<Arc<DigestAuth>>,
    /// Shared per-client failed-login lockout.
    pub lockout: Arc<Lockout>,
    /// TLS acceptor for the control listener in production.
    pub tls_acceptor: Option<TlsAcceptor>,
}

impl std::fmt::Debug for ControlServerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlServerConfig")
            .field("bind_address", &self.bind_address)
            .field("auth", &self.auth.as_ref().map(|_| "[CONFIGURED]"))
            .field("lockout", &"[CONFIGURED]")
            .field("tls", &self.tls_acceptor.as_ref().map(|_| "[CONFIGURED]"))
            .finish()
    }
}

impl ControlServerConfig {
    pub fn new(bind_address: SocketAddr) -> Self {
        Self {
            bind_address,
            auth: None,
            lockout: Arc::new(Lockout::new()),
            tls_acceptor: None,
        }
    }
}

/// Camera-local HTTP control server.
pub struct ControlServer {
    inner: Arc<ControlInner>,
    local_addr: SocketAddr,
}

impl std::fmt::Debug for ControlServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlServer")
            .field("bind_address", &self.local_addr)
            .finish()
    }
}

impl ControlServer {
    /// Start the HTTP control listener on the current Tokio runtime.
    pub fn start(
        config: ControlServerConfig,
        media: MediaEngineSlot,
        ptz: Option<Arc<PtzController>>,
    ) -> io::Result<Self> {
        let listener = StdTcpListener::bind(config.bind_address)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let listener = TcpListener::from_std(listener)?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            io::Error::other(format!("control server requires a Tokio runtime: {error}"))
        })?;
        let maintenance_available = config.tls_acceptor.is_some() && config.auth.is_some();
        let tls_acceptor = config.tls_acceptor;
        let inner = Arc::new(ControlInner {
            media,
            ptz,
            auth: config.auth,
            lockout: config.lockout,
            maintenance_available,
            maintenance_telnet_started: AtomicBool::new(false),
            active: AtomicBool::new(false),
            next_timestamp_us: AtomicU64::new(0),
            volume: AtomicU32::new(DEFAULT_VOLUME),
            command_lock: Mutex::new(()),
            ptz_lock: Mutex::new(()),
            shutdown: Notify::new(),
        });
        let accept_inner = Arc::clone(&inner);
        runtime.spawn(async move { accept_loop(listener, accept_inner, tls_acceptor).await });

        Ok(Self { inner, local_addr })
    }

    /// The shared control plane, so another transport can issue the same commands
    /// the HTTP routes do rather than duplicating their logic.
    pub(crate) fn inner(&self) -> Arc<ControlInner> {
        Arc::clone(&self.inner)
    }

    /// Address selected by the operating system, useful when binding port 0.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.inner.shutdown.notify_waiters();
    }
}

pub(crate) struct ControlInner {
    media: MediaEngineSlot,
    ptz: Option<Arc<PtzController>>,
    auth: Option<Arc<DigestAuth>>,
    lockout: Arc<Lockout>,
    /// The shell route is exposed only on the authenticated TLS listener.
    maintenance_available: bool,
    maintenance_telnet_started: AtomicBool,
    active: AtomicBool,
    next_timestamp_us: AtomicU64,
    volume: AtomicU32,
    command_lock: Mutex<()>,
    ptz_lock: Mutex<()>,
    shutdown: Notify,
}

async fn accept_loop(listener: TcpListener, inner: Arc<ControlInner>, tls: Option<TlsAcceptor>) {
    loop {
        tokio::select! {
            _ = inner.shutdown.notified() => break,
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { break };
                let client_inner = Arc::clone(&inner);
                let acceptor = tls.clone();
                tokio::spawn(async move {
                    let result = match acceptor {
                        Some(acceptor) => match acceptor.accept(stream).await {
                            Ok(stream) => handle_connection(Box::new(stream), peer, client_inner).await,
                            Err(error) => Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("TLS handshake failed: {error}"),
                            )),
                        },
                        None => handle_connection(Box::new(stream), peer, client_inner).await,
                    };
                    if let Err(error) = result {
                        tracing::debug!(%peer, %error, "control client connection ended");
                    }
                });
            }
        }
    }
}

async fn handle_connection(
    mut stream: ControlStream,
    peer: SocketAddr,
    inner: Arc<ControlInner>,
) -> io::Result<()> {
    let response = match read_request(&mut stream).await {
        Ok(request) => match authorize_request(&inner, &request, peer).await {
            Ok(()) => dispatch(&inner, request).await,
            Err(response) => response,
        },
        Err(error) => error_response(400, &error.to_string()),
    };
    stream.write_all(&response).await
}

struct HttpRequest {
    method: String,
    /// Request path without the query string, used for routing.
    path: String,
    /// Exact request target, used for Digest URI binding.
    uri: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut ControlStream) -> io::Result<HttpRequest> {
    let mut data = Vec::new();
    let header_end = loop {
        if let Some(index) = find_bytes(&data, b"\r\n\r\n") {
            break index;
        }
        if data.len() >= MAX_HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP headers exceed the configured limit",
            ));
        }
        let mut chunk = [0_u8; 4 * 1024];
        let count = stream.read(&mut chunk).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "HTTP request ended before its headers",
            ));
        }
        data.extend_from_slice(&chunk[..count]);
    };

    let header_len = header_end + 4;
    let header = std::str::from_utf8(&data[..header_end])
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "HTTP headers are not UTF-8"))?;
    let mut lines = header.split("\r\n");
    let request_line = lines.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "HTTP request line is missing")
    })?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "HTTP method is missing"))?;
    let target = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "HTTP target is missing"))?;
    let version = request_parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "HTTP version is missing"))?;
    if version != "HTTP/1.1" || request_parts.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "only HTTP/1.1 request syntax is supported",
        ));
    }
    if !target.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control request target must be an absolute path",
        ));
    }

    let mut content_length = None;
    let mut authorization = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP header is missing its colon",
            ));
        };
        if name.eq_ignore_ascii_case("content-length") {
            let length = value.trim().parse::<usize>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP Content-Length")
            })?;
            if content_length
                .replace(length)
                .is_some_and(|old| old != length)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "conflicting HTTP Content-Length headers",
                ));
            }
        } else if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value.trim().to_string());
        } else if name.eq_ignore_ascii_case("transfer-encoding") && !value.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunked HTTP requests are not supported",
            ));
        }
    }

    let content_length = content_length.unwrap_or(0);
    if content_length > MAX_BODY_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "HTTP request body exceeds the configured limit",
        ));
    }
    let mut body = data[header_len..].to_vec();
    if body.len() > content_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "HTTP pipelining is not supported",
        ));
    }
    let body_len = body.len();
    body.resize(content_length, 0);
    stream.read_exact(&mut body[body_len..]).await?;

    Ok(HttpRequest {
        method: method.to_string(),
        path: target.split('?').next().unwrap_or(target).to_string(),
        uri: target.to_string(),
        authorization,
        body,
    })
}

fn find_bytes(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len())
        .position(|window| window == needle)
}

async fn authorize_request(
    inner: &ControlInner,
    request: &HttpRequest,
    peer: SocketAddr,
) -> Result<(), Vec<u8>> {
    let Some(auth) = inner.auth.as_ref() else {
        return Ok(());
    };
    if request.method == "GET" && request.path == "/healthz" {
        return Ok(());
    }
    if inner.lockout.is_locked(peer.ip()).await {
        return Err(error_response(403, "authentication temporarily locked"));
    }

    let authorization = request.authorization.as_deref().unwrap_or("");
    let outcome = auth.authorize(
        &request.method,
        &request.uri,
        authorization,
        SystemTime::now(),
    );
    if outcome == Outcome::Authorized {
        inner.lockout.record_success(peer.ip()).await;
        Ok(())
    } else {
        if outcome == Outcome::Denied && !authorization.is_empty() {
            inner.lockout.record_failure(peer.ip()).await;
        }
        Err(unauthorized_response(auth, outcome == Outcome::Stale))
    }
}

fn unauthorized_response(auth: &DigestAuth, stale: bool) -> Vec<u8> {
    let challenges = auth.challenges(SystemTime::now());
    let stale = if stale { ", stale=true" } else { "" };
    let body = b"authentication required";
    format!(
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: {}{}\r\nWWW-Authenticate: {}{}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        challenges[0],
        stale,
        challenges[1],
        stale,
        body.len()
    )
    .into_bytes()
    .into_iter()
    .chain(body.iter().copied())
    .collect()
}

async fn dispatch(inner: &Arc<ControlInner>, request: HttpRequest) -> Vec<u8> {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/healthz") => health_response(inner),
        ("GET", "/talkback/status") => talkback_status_response(inner),
        ("POST", "/talkback/start") => talkback_start(inner).await,
        ("POST", "/talkback/stop") => talkback_stop(inner).await,
        ("POST", "/talkback/clear") => talkback_clear(inner).await,
        ("POST", "/talkback/pcm") => talkback_pcm(inner, request.body).await,
        ("POST", "/talkback/audio") => talkback_audio(inner, request.body).await,
        ("POST", "/talkback/volume") => talkback_volume(inner, &request.body).await,
        ("POST", "/night_mode") => night_mode(inner, &request.body).await,
        ("POST", "/maintenance/enable") => maintenance_enable(inner),
        ("GET", "/ptz/position") | ("GET", "/ptz/status") => ptz_position(inner).await,
        ("POST", "/ptz/move") => ptz_move(inner, &request.body).await,
        ("POST", "/ptz/stop") => ptz_stop(inner).await,
        ("POST", "/ptz/center") => ptz_center(inner).await,
        _ => error_response(404, "control endpoint not found"),
    }
}

fn maintenance_enable(inner: &Arc<ControlInner>) -> Vec<u8> {
    if !inner.maintenance_available {
        return error_response(
            404,
            "maintenance endpoint requires TLS and Digest authentication",
        );
    }
    if inner
        .maintenance_telnet_started
        .swap(true, Ordering::AcqRel)
    {
        return json_response(
            200,
            "OK",
            r#"{"status":"ok","maintenance":"telnet","enabled":true,"port":23}"#,
        );
    }

    let result = Command::new(TELNETD_PATH)
        .args(["-p", TELNETD_PORT, "-l", MAINTENANCE_SHELL_PATH])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match result {
        Ok(mut child) => {
            let pid = child.id();
            // telnetd daemonises: the process spawned here exits almost
            // immediately, so without this wait it stays a zombie for the rest
            // of the boot. Nothing depends on the result.
            tokio::task::spawn_blocking(move || {
                let _ = child.wait();
            });
            tracing::warn!(
                pid,
                "TLS/Digest-authorized maintenance Telnet enabled for the current boot"
            );
            json_response(
                202,
                "Accepted",
                r#"{"status":"ok","maintenance":"telnet","enabled":true,"port":23}"#,
            )
        }
        Err(error) => {
            inner
                .maintenance_telnet_started
                .store(false, Ordering::Release);
            tracing::error!(%error, "could not start maintenance Telnet");
            error_response(503, "maintenance Telnet could not be started")
        }
    }
}

pub(crate) async fn talkback_start(inner: &Arc<ControlInner>) -> Vec<u8> {
    inner.active.store(false, Ordering::Release);
    inner.next_timestamp_us.store(0, Ordering::Release);
    match send_command(inner, false, |handle| handle.clear_speaker()).await {
        Ok(()) => {
            inner.active.store(true, Ordering::Release);
            json_response(200, "OK", r#"{"status":"ok","talking":true}"#)
        }
        Err(error) => command_error_response(error),
    }
}

pub(crate) async fn talkback_stop(inner: &Arc<ControlInner>) -> Vec<u8> {
    inner.active.store(false, Ordering::Release);
    match send_command(inner, false, |handle| handle.clear_speaker()).await {
        Ok(()) => json_response(200, "OK", r#"{"status":"ok","talking":false}"#),
        Err(error) => command_error_response(error),
    }
}

async fn talkback_clear(inner: &Arc<ControlInner>) -> Vec<u8> {
    match send_command(inner, false, |handle| handle.clear_speaker()).await {
        Ok(()) => json_response(200, "OK", r#"{"status":"ok"}"#),
        Err(error) => command_error_response(error),
    }
}

async fn talkback_pcm(inner: &Arc<ControlInner>, body: Vec<u8>) -> Vec<u8> {
    if body.is_empty() {
        return json_response(200, "OK", r#"{"status":"ok","bytes_sent":0}"#);
    }
    let duration_us = (body.len() as u64 / 2)
        .saturating_mul(1_000_000)
        .checked_div(PCM_SAMPLE_RATE)
        .unwrap_or(0)
        .max(1);
    let timestamp_us = reserve_timestamp(inner, duration_us);
    let byte_count = body.len();
    match send_command(inner, true, move |handle| {
        handle.send_speaker_pcm(&body, timestamp_us)
    })
    .await
    {
        Ok(()) => json_response(
            200,
            "OK",
            &format!(r#"{{"status":"ok","bytes_sent":{byte_count}}}"#),
        ),
        Err(error) => command_error_response(error),
    }
}

async fn talkback_audio(inner: &Arc<ControlInner>, body: Vec<u8>) -> Vec<u8> {
    if body.is_empty() {
        return json_response(200, "OK", r#"{"status":"ok","bytes_sent":0}"#);
    }
    let timestamp_us = reserve_timestamp(inner, AAC_FRAME_DURATION_US);
    let byte_count = body.len();
    match send_command(inner, true, move |handle| {
        handle.send_speaker_aac(&body, timestamp_us)
    })
    .await
    {
        Ok(()) => json_response(
            200,
            "OK",
            &format!(r#"{{"status":"ok","bytes_sent":{byte_count}}}"#),
        ),
        Err(error) => command_error_response(error),
    }
}

async fn talkback_volume(inner: &Arc<ControlInner>, body: &[u8]) -> Vec<u8> {
    let volume = match parse_json_u32_field(body, "volume") {
        Ok(volume) => volume,
        Err(error) => return error_response(400, &error),
    };
    match send_command(inner, false, move |handle| {
        handle.set_speaker_volume(volume)
    })
    .await
    {
        Ok(()) => {
            inner.volume.store(volume, Ordering::Release);
            json_response(
                200,
                "OK",
                &format!(r#"{{"status":"ok","volume":{volume},"max_volume":100}}"#),
            )
        }
        Err(error) => command_error_response(error),
    }
}

pub(crate) async fn night_mode(inner: &Arc<ControlInner>, body: &[u8]) -> Vec<u8> {
    let mode = match parse_json_u32_field(body, "mode").and_then(|mode| {
        u8::try_from(mode).map_err(|_| "night mode must be 0, 1, or 2".to_string())
    }) {
        Ok(mode @ 0..=2) => mode,
        Ok(_) => return error_response(400, "night mode must be 0, 1, or 2"),
        Err(error) => return error_response(400, &error),
    };
    match send_command(inner, false, move |handle| handle.set_night_mode(mode)).await {
        Ok(()) => json_response(200, "OK", &format!(r#"{{"status":"ok","mode":{mode}}}"#)),
        Err(error) => command_error_response(error),
    }
}

pub(crate) async fn ptz_position(inner: &Arc<ControlInner>) -> Vec<u8> {
    match send_ptz(inner, |ptz| ptz.position()).await {
        Ok((pan, tilt)) => json_response(
            200,
            "OK",
            &format!(r#"{{"status":"ok","pan":{pan},"tilt":{tilt}}}"#),
        ),
        Err(error) => ptz_error_response(error),
    }
}

pub(crate) async fn ptz_move(inner: &Arc<ControlInner>, body: &[u8]) -> Vec<u8> {
    let direction = match parse_json_string_field(body, "direction")
        .ok()
        .and_then(|value| PtzDirection::parse(&value))
    {
        Some(direction) => direction,
        None => return error_response(400, "PTZ direction must be left, right, up, or down"),
    };
    let degrees = match parse_optional_json_u32_field(body, "degrees", 1) {
        Ok(degrees) if (1..=PAN_MAX_DEGREES as u32).contains(&degrees) => degrees as i32,
        Ok(_) => return error_response(400, "PTZ degrees must be between 1 and 355"),
        Err(error) => return error_response(400, &error),
    };
    let speed = match parse_optional_json_u32_field(body, "speed", 5) {
        Ok(speed) if speed <= 10 => speed as u8,
        Ok(_) => return error_response(400, "PTZ speed must be between 0 and 10"),
        Err(error) => return error_response(400, &error),
    };

    let operation = move |ptz: &PtzController| {
        if direction == PtzDirection::Stop {
            ptz.stop()
        } else {
            ptz.move_step(direction, degrees, speed)
        }
    };
    match send_ptz(inner, operation).await {
        Ok(()) => json_response(
            200,
            "OK",
            &format!(
                r#"{{"status":"ok","direction":"{direction}","degrees":{degrees},"speed":{speed}}}"#
            ),
        ),
        Err(error) => ptz_error_response(error),
    }
}

pub(crate) async fn ptz_stop(inner: &Arc<ControlInner>) -> Vec<u8> {
    match send_ptz(inner, |ptz| ptz.stop()).await {
        Ok(()) => json_response(200, "OK", r#"{"status":"ok"}"#),
        Err(error) => ptz_error_response(error),
    }
}

pub(crate) async fn ptz_center(inner: &Arc<ControlInner>) -> Vec<u8> {
    match send_ptz(inner, |ptz| {
        ptz.center(5)?;
        ptz.position()
    })
    .await
    {
        Ok((pan, tilt)) => json_response(
            200,
            "OK",
            &format!(r#"{{"status":"ok","pan":{pan},"tilt":{tilt}}}"#),
        ),
        Err(error) => ptz_error_response(error),
    }
}

fn reserve_timestamp(inner: &ControlInner, duration_us: u64) -> u64 {
    loop {
        let current = inner.next_timestamp_us.load(Ordering::Acquire);
        let next = current.saturating_add(duration_us);
        if inner
            .next_timestamp_us
            .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return current;
        }
    }
}

async fn send_command<F>(
    inner: &Arc<ControlInner>,
    require_active: bool,
    operation: F,
) -> Result<(), CommandFailure>
where
    F: FnOnce(&MediaEngineHandle) -> io::Result<()> + Send + 'static,
{
    let handle = inner
        .media
        .current()
        .map_err(|error| CommandFailure::Unavailable(error.to_string()))?
        .ok_or_else(|| CommandFailure::Unavailable("media engine is not connected".to_string()))?;
    let command_inner = Arc::clone(inner);
    tokio::task::spawn_blocking(move || {
        let _lock = command_inner.command_lock.lock().map_err(|_| {
            CommandFailure::Unavailable("control command lock poisoned".to_string())
        })?;
        if require_active && !command_inner.active.load(Ordering::Acquire) {
            return Err(CommandFailure::Inactive);
        }
        operation(&handle).map_err(|error| match error.kind() {
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
                CommandFailure::Rejected(error.to_string())
            }
            _ => CommandFailure::Unavailable(error.to_string()),
        })
    })
    .await
    .map_err(|error| CommandFailure::Unavailable(format!("command worker failed: {error}")))?
}

async fn send_ptz<F, T>(inner: &Arc<ControlInner>, operation: F) -> Result<T, PtzFailure>
where
    F: FnOnce(&PtzController) -> io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let ptz = inner
        .ptz
        .clone()
        .ok_or_else(|| PtzFailure::Unavailable("PTZ controller is not available".to_string()))?;
    let ptz_inner = Arc::clone(inner);
    tokio::task::spawn_blocking(move || {
        let _lock = ptz_inner
            .ptz_lock
            .lock()
            .map_err(|_| PtzFailure::Unavailable("PTZ lock poisoned".to_string()))?;
        operation(&ptz).map_err(|error| match error.kind() {
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
                PtzFailure::Rejected(error.to_string())
            }
            _ => PtzFailure::Unavailable(error.to_string()),
        })
    })
    .await
    .map_err(|error| PtzFailure::Unavailable(format!("PTZ worker failed: {error}")))?
}

#[derive(Debug)]
enum PtzFailure {
    Rejected(String),
    Unavailable(String),
}

fn ptz_error_response(error: PtzFailure) -> Vec<u8> {
    match error {
        PtzFailure::Rejected(message) => error_response(400, &message),
        PtzFailure::Unavailable(message) => error_response(503, &message),
    }
}

#[derive(Debug)]
enum CommandFailure {
    Inactive,
    Rejected(String),
    Unavailable(String),
}

fn command_error_response(error: CommandFailure) -> Vec<u8> {
    match error {
        CommandFailure::Inactive => error_response(409, "talkback is not active"),
        CommandFailure::Rejected(message) => error_response(400, &message),
        CommandFailure::Unavailable(message) => error_response(503, &message),
    }
}

pub(crate) fn health_response(inner: &ControlInner) -> Vec<u8> {
    let ready = inner.media.current().ok().flatten().is_some();
    let (status, code, reason) = if ready {
        ("ok", 200, "OK")
    } else {
        ("not_ready", 503, "Service Unavailable")
    };
    json_response(
        code,
        reason,
        &format!(r#"{{"status":"{status}","ready":{ready}}}"#),
    )
}

pub(crate) fn talkback_status_response(inner: &ControlInner) -> Vec<u8> {
    let ready = inner.media.current().ok().flatten().is_some();
    let talking = inner.active.load(Ordering::Acquire);
    let volume = inner.volume.load(Ordering::Acquire);
    json_response(
        200,
        "OK",
        &format!(r#"{{"status":"ok","talking":{talking},"ready":{ready},"volume":{volume}}}"#),
    )
}

fn parse_optional_json_u32_field(body: &[u8], field: &str, default: u32) -> Result<u32, String> {
    let text = std::str::from_utf8(body).map_err(|_| "JSON body is not UTF-8".to_string())?;
    if !text.contains(&format!("\"{field}\"")) {
        Ok(default)
    } else {
        parse_json_u32_field(body, field)
    }
}

fn parse_json_string_field(body: &[u8], field: &str) -> Result<String, String> {
    let text = std::str::from_utf8(body).map_err(|_| "JSON body is not UTF-8".to_string())?;
    let key = format!("\"{field}\"");
    let value = text
        .split_once(&key)
        .ok_or_else(|| format!("JSON field {field} is missing"))?
        .1
        .trim_start()
        .strip_prefix(':')
        .ok_or_else(|| format!("JSON field {field} must have a value"))?
        .trim_start();
    let value = value
        .strip_prefix('"')
        .ok_or_else(|| format!("JSON field {field} must be a string"))?;
    let end = value
        .find('"')
        .ok_or_else(|| format!("JSON field {field} has an unterminated string"))?;
    if end == 0 {
        return Err(format!("JSON field {field} must not be empty"));
    }
    Ok(value[..end].to_string())
}

fn parse_json_u32_field(body: &[u8], field: &str) -> Result<u32, String> {
    let text = std::str::from_utf8(body).map_err(|_| "JSON body is not UTF-8".to_string())?;
    let key = format!("\"{field}\"");
    let value = text
        .split_once(&key)
        .ok_or_else(|| format!("JSON field {field} is missing"))?
        .1
        .trim_start()
        .strip_prefix(':')
        .ok_or_else(|| format!("JSON field {field} must have a value"))?
        .trim_start();
    let digit_end = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    if digit_end == 0 {
        return Err(format!("JSON field {field} must be an unsigned integer"));
    }
    value[..digit_end]
        .parse::<u32>()
        .map_err(|_| format!("JSON field {field} is out of range"))
}

fn json_response(status: u16, reason: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn error_response(status: u16, message: &str) -> Vec<u8> {
    let body = format!(
        r#"{{"status":"error","message":"{}"}}"#,
        escape_json(message)
    );
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        409 => "Conflict",
        503 => "Service Unavailable",
        _ => "Error",
    };
    json_response(status, reason, &body)
}

fn escape_json(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{MediaRecord, RecordKind};
    use std::os::unix::net::UnixStream;
    use tokio::net::TcpStream;

    async fn post(server: SocketAddr, path: &str, body: &[u8]) -> String {
        let mut socket = TcpStream::connect(server)
            .await
            .expect("control client should connect");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: camera\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        socket
            .write_all(request.as_bytes())
            .await
            .expect("control request headers should write");
        socket
            .write_all(body)
            .await
            .expect("control request body should write");
        let mut response = Vec::new();
        socket
            .read_to_end(&mut response)
            .await
            .expect("control response should read");
        String::from_utf8(response).expect("control response should be UTF-8")
    }

    #[test]
    fn parses_bounded_numeric_control_fields() {
        assert_eq!(parse_json_u32_field(br#"{"volume":42}"#, "volume"), Ok(42));
        assert!(parse_json_u32_field(br#"{"volume":101}"#, "volume").is_ok());
        assert!(parse_json_u32_field(br#"{"volume":-1}"#, "volume").is_err());
    }

    #[tokio::test]
    async fn maintenance_route_is_hidden_without_tls() {
        let server = ControlServer::start(
            ControlServerConfig::new(SocketAddr::from(([127, 0, 0, 1], 0))),
            MediaEngineSlot::default(),
            None,
        )
        .expect("control server should start");
        let response = post(server.local_addr(), "/maintenance/enable", &[]).await;
        assert!(response.starts_with("HTTP/1.1 404 Not Found"));
    }

    #[tokio::test]
    async fn authenticated_control_leaves_only_health_public() {
        let mut config = ControlServerConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)));
        config.auth = Some(Arc::new(
            DigestAuth::new("admin", "test-password").expect("test entropy should be available"),
        ));
        let server = ControlServer::start(config, MediaEngineSlot::default(), None)
            .expect("control server should start");

        let response = post(server.local_addr(), "/maintenance/enable", &[]).await;
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized"));

        let mut socket = TcpStream::connect(server.local_addr())
            .await
            .expect("control client should connect");
        socket
            .write_all(
                b"GET /talkback/status HTTP/1.1\r\nHost: camera\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("protected request should write");
        let mut response = Vec::new();
        socket
            .read_to_end(&mut response)
            .await
            .expect("protected response should read");
        let response = String::from_utf8(response).expect("response should be UTF-8");
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized"));
        assert_eq!(response.matches("WWW-Authenticate:").count(), 2);

        let mut socket = TcpStream::connect(server.local_addr())
            .await
            .expect("health client should connect");
        socket
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: camera\r\n\r\n")
            .await
            .expect("health request should write");
        let mut response = Vec::new();
        socket
            .read_to_end(&mut response)
            .await
            .expect("health response should read");
        let response = String::from_utf8(response).expect("health response should be UTF-8");
        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
    }

    #[tokio::test]
    async fn pcm_endpoint_forwards_framed_command() {
        let (mut companion, daemon) = UnixStream::pair().expect("Unix pair should create");
        let slot = MediaEngineSlot::default();
        slot.set(MediaEngineHandle::from_socket_for_test(daemon))
            .expect("media handle should publish");
        let server = ControlServer::start(
            ControlServerConfig::new(SocketAddr::from(([127, 0, 0, 1], 0))),
            slot,
            None,
        )
        .expect("control server should start");

        let response = post(server.local_addr(), "/talkback/start", &[]).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        let start_record = MediaRecord::read_from(&mut companion)
            .expect("start command should read")
            .expect("start command should be present");
        assert_eq!(start_record.kind, RecordKind::SPEAKER_CLEAR);

        let response = post(server.local_addr(), "/talkback/pcm", &[1, 2]).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        let pcm_record = MediaRecord::read_from(&mut companion)
            .expect("PCM command should read")
            .expect("PCM command should be present");
        assert_eq!(pcm_record.kind, RecordKind::SPEAKER_PCM);
        assert!(pcm_record.keyframe);
        assert_eq!(pcm_record.payload, vec![1, 2]);
    }
}
