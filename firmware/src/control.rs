//! Camera-local HTTP control and talkback service.
//!
//! This service is deliberately small and accepts bounded HTTP/1.1 requests.
//! It forwards only validated speaker/night-mode commands through the shared
//! `C220` media socket. Digest authentication is applied before endpoint
//! dispatch, and production startup requires the listener to use TLS.

use crate::digest::{DigestAuth, Lockout, Outcome};
use crate::media_engine::MediaEngineHandle;
use crate::ptz::{PAN_MAX_DEGREES, PtzController, PtzDirection};
use crate::security;
use std::io;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};
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

/// Port the one-boot maintenance shell listens on.
///
/// One number, because it is both the argument `telnetd` is given and the address
/// the engine probes to prove that shell exists.
const MAINTENANCE_PORT: u16 = 23;
const MAINTENANCE_SHELL_PATH: &str = "/bin/sh";

/// How long a freshly spawned `telnetd` is given to bind its port before the
/// window is called a failure.
///
/// It daemonises, so the spawn only proves the launcher exited cleanly. Measured
/// on the camera the listener appears in well under a second, and the margin is
/// there for a loaded one-core board, not for the usual case.
const MAINTENANCE_LISTEN_TIMEOUT: Duration = Duration::from_secs(3);

/// Reboot command, and the argument it is given.
///
/// The bare name is deliberate: it is what the factory-reset script uses on this
/// camera, resolved through the PATH the boot script exports. `-f` reboots without
/// asking init to unwind, which is the form this board is known to complete - the
/// graceful path has been seen to leave a camera that answers ping and runs
/// nothing.
const REBOOT_PROGRAM: &str = "reboot";
const REBOOT_ARGUMENT: &str = "-f";

/// Delay between accepting a reboot request and pulling the board down.
///
/// The client that asked for the reboot should get its answer first, and one round
/// trip on a LAN is well inside this.
const REBOOT_DELAY: Duration = Duration::from_millis(500);

/// One connection attempt while waiting for that listener, and the pause between
/// attempts. Both are short: a local connect either succeeds at once or fails at
/// once, and the loop around them carries the deadline.
const LISTEN_PROBE_TIMEOUT: Duration = Duration::from_millis(300);
const LISTEN_PROBE_INTERVAL: Duration = Duration::from_millis(200);

/// AAC talkback access units use the validated 64 ms native cadence.
const AAC_FRAME_DURATION_US: u64 = 64_000;

/// PCM talkback is supplied as mono 16 kHz signed PCM16.
const PCM_SAMPLE_RATE: u64 = 16_000;

/// One piece of audio waiting for the camera speaker.
///
/// Audio is queued rather than written inline: the handler must answer the client as
/// soon as the audio is accepted, otherwise the engine's playback pacing becomes the
/// client's pacing, and one slow write stretches the following chunk into silence.
/// The reference bridge queued it the same way.
enum SpeakerFrame {
    Pcm { data: Vec<u8>, timestamp_us: u64 },
    Aac { data: Vec<u8>, timestamp_us: u64 },
}

/// Frames that may wait before the writer task drains them. A frame is a client-side
/// chunk, typically 20-200 ms of audio, so this is a couple of seconds of slack:
/// enough to absorb jitter, small enough that a client which stops sending cannot
/// accumulate audio in the camera's scarce memory.
const AUDIO_QUEUE_FRAMES: usize = 32;

/// Native AO volume starts at its validated full-scale default.
const DEFAULT_VOLUME: u32 = 100;

/// A slot that is populated once the media companion has connected.
#[derive(Clone, Default)]
pub struct MediaEngineSlot {
    handle: Arc<Mutex<Option<MediaEngineHandle>>>,
    /// What the companion last reported the IR hardware to be doing: 0 until it
    /// has reported at all, 1 for day (IR off), 2 for night (IR on). The
    /// commanded mode cannot answer this, because in automatic mode the command
    /// stays `auto` all night while the LED is on.
    night_vision: Arc<AtomicU8>,
}

impl MediaEngineSlot {
    /// Record the companion's own day/night report.
    pub fn set_night_vision(&self, night: bool) {
        self.night_vision
            .store(if night { 2 } else { 1 }, Ordering::Release);
    }

    /// The reported IR state, or `None` until the companion has reported one.
    fn night_vision(&self) -> Option<bool> {
        match self.night_vision.load(Ordering::Acquire) {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        }
    }

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
        // The writer task owns the speaker's pacing; the handlers only enqueue.
        let (audio_tx, mut audio_rx) = mpsc::channel::<SpeakerFrame>(AUDIO_QUEUE_FRAMES);
        let inner = Arc::new(ControlInner {
            media,
            ptz,
            auth: config.auth,
            lockout: config.lockout,
            maintenance_available,
            maintenance_telnet_started: AtomicBool::new(false),
            centering: AtomicBool::new(false),
            active: AtomicBool::new(false),
            next_timestamp_us: AtomicU64::new(0),
            volume: AtomicU32::new(DEFAULT_VOLUME),
            audio: audio_tx,
            night_mode: AtomicU8::new(0),
            command_lock: Mutex::new(()),
            ptz_lock: Mutex::new(()),
            shutdown: Notify::new(),
        });
        let accept_inner = Arc::clone(&inner);
        runtime.spawn(async move { accept_loop(listener, accept_inner, tls_acceptor).await });

        let writer_inner = Arc::clone(&inner);
        runtime.spawn(async move {
            while let Some(frame) = audio_rx.recv().await {
                let result = match frame {
                    SpeakerFrame::Pcm { data, timestamp_us } => {
                        send_command(&writer_inner, true, move |handle| {
                            handle.send_speaker_pcm(&data, timestamp_us)
                        })
                        .await
                    }
                    SpeakerFrame::Aac { data, timestamp_us } => {
                        send_command(&writer_inner, true, move |handle| {
                            handle.send_speaker_aac(&data, timestamp_us)
                        })
                        .await
                    }
                };
                if let Err(error) = result {
                    tracing::warn!(?error, "camera speaker write failed");
                }
            }
        });

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
    /// Whether a centring sweep is running. It takes about thirty seconds, so it is
    /// accepted and run outside the request that asked for it.
    centering: AtomicBool,
    active: AtomicBool,
    next_timestamp_us: AtomicU64,
    volume: AtomicU32,
    /// Audio waiting for the speaker writer task, bounded so that a client sending
    /// faster than real time is refused instead of growing the camera's memory use.
    audio: mpsc::Sender<SpeakerFrame>,
    /// Last commanded night mode: 0 automatic, 1 night, 2 day. The engine owns the
    /// real state; this is what the camera was last told to do, which is what a
    /// status reader can act on.
    night_mode: AtomicU8,
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
        ("GET", "/status") => status_response(inner).await,
        ("GET", "/talkback/status") => talkback_status_response(inner),
        ("POST", "/talkback/start") => talkback_start(inner).await,
        ("POST", "/talkback/stop") => talkback_stop(inner).await,
        ("POST", "/talkback/clear") => talkback_clear(inner).await,
        ("POST", "/talkback/pcm") => talkback_pcm(inner, request.body).await,
        ("POST", "/talkback/audio") => talkback_audio(inner, request.body).await,
        ("POST", "/talkback/volume") => talkback_volume(inner, &request.body).await,
        ("POST", "/night_mode") => night_mode(inner, &request.body).await,
        ("POST", "/maintenance/enable") => maintenance_enable(inner).await,
        ("POST", "/reboot") => reboot().await,
        ("GET", "/ptz/position") | ("GET", "/ptz/status") => ptz_position(inner).await,
        ("POST", "/ptz/move") => ptz_move(inner, &request.body).await,
        ("POST", "/ptz/rotate") => ptz_rotate(inner, &request.body).await,
        ("POST", "/ptz/stop") => ptz_stop(inner).await,
        ("POST", "/ptz/center") => ptz_center(inner).await,
        _ => error_response(404, "control endpoint not found"),
    }
}

async fn maintenance_enable(inner: &Arc<ControlInner>) -> Vec<u8> {
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
            &format!(
                r#"{{"status":"ok","maintenance":"telnet","enabled":true,"port":{MAINTENANCE_PORT}}}"#
            ),
        );
    }

    let port = MAINTENANCE_PORT.to_string();
    let result = Command::new(TELNETD_PATH)
        .args(["-p", port.as_str(), "-l", MAINTENANCE_SHELL_PATH])
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
            if !maintenance_listener_is_up(MAINTENANCE_PORT).await {
                // Seen on this camera: a clean spawn whose shell never listened.
                // Keeping the flag armed would make that permanent for the boot,
                // so it is cleared and the caller may ask again.
                inner
                    .maintenance_telnet_started
                    .store(false, Ordering::Release);
                tracing::error!(pid, "maintenance Telnet was spawned but never listened");
                return error_response(503, "maintenance Telnet did not start listening");
            }
            tracing::warn!(
                pid,
                "TLS/Digest-authorized maintenance Telnet enabled for the current boot"
            );
            json_response(
                202,
                "Accepted",
                &format!(
                    r#"{{"status":"ok","maintenance":"telnet","enabled":true,"port":{MAINTENANCE_PORT}}}"#
                ),
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

/// Whether something is listening on the maintenance port.
///
/// The probe is what separates "telnetd was spawned" from "a shell can actually be
/// reached", which is the difference between a window that works and one the
/// operator only discovers is missing when they need it.
async fn maintenance_listener_is_up(port: u16) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + MAINTENANCE_LISTEN_TIMEOUT;
    loop {
        let attempt = tokio::time::timeout(
            LISTEN_PROBE_TIMEOUT,
            tokio::net::TcpStream::connect(address),
        )
        .await;
        if matches!(attempt, Ok(Ok(_))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(LISTEN_PROBE_INTERVAL).await;
    }
}

/// Reboot the board, after the answer to this request has been sent.
///
/// This is the second privileged route, beside the maintenance shell, and it exists
/// because everything the deployment needs a reboot for - a new certificate, a new
/// network, refreshing the one-boot shell - otherwise costs physical access. Like
/// the shell it is reachable only over TLS with Digest credentials.
async fn reboot() -> Vec<u8> {
    tokio::spawn(async {
        tokio::time::sleep(REBOOT_DELAY).await;
        // NVRAM is journalled and the clock's stored time is deliberately cheap to
        // lose, but syncing first costs nothing and an abrupt reset is still worse.
        unsafe { libc::sync() };
        match Command::new(REBOOT_PROGRAM).arg(REBOOT_ARGUMENT).spawn() {
            Ok(_) => tracing::warn!("rebooting on request through the control API"),
            Err(error) => tracing::error!(%error, "could not reboot"),
        }
    });
    json_response(202, "Accepted", r#"{"status":"ok","rebooting":true}"#)
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
    match inner.audio.try_send(SpeakerFrame::Pcm {
        data: body,
        timestamp_us,
    }) {
        Ok(()) => queued_response(byte_count),
        Err(error) => queue_error_response(error),
    }
}

async fn talkback_audio(inner: &Arc<ControlInner>, body: Vec<u8>) -> Vec<u8> {
    if body.is_empty() {
        return json_response(200, "OK", r#"{"status":"ok","bytes_sent":0}"#);
    }
    let timestamp_us = reserve_timestamp(inner, AAC_FRAME_DURATION_US);
    let byte_count = body.len();
    match inner.audio.try_send(SpeakerFrame::Aac {
        data: body,
        timestamp_us,
    }) {
        Ok(()) => queued_response(byte_count),
        Err(error) => queue_error_response(error),
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
        Ok(()) => {
            inner.night_mode.store(mode, Ordering::Release);
            json_response(200, "OK", &format!(r#"{{"status":"ok","mode":{mode}}}"#))
        }
        Err(error) => command_error_response(error),
    }
}

pub(crate) async fn ptz_position(inner: &Arc<ControlInner>) -> Vec<u8> {
    let centering = inner.centering.load(Ordering::Acquire);
    match send_ptz(inner, |ptz| ptz.position()).await {
        Ok((pan, tilt)) => json_response(
            200,
            "OK",
            &format!(r#"{{"status":"ok","pan":{pan},"tilt":{tilt},"centering":{centering}}}"#),
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

/// Rotate until told to stop.
///
/// A held button wants this, not steps: every step carries its own acceleration and
/// settling, so stepping a held direction starts and stops the motor several times a
/// second and reads as a stutter rather than a pan.
pub(crate) async fn ptz_rotate(inner: &Arc<ControlInner>, body: &[u8]) -> Vec<u8> {
    let direction = match parse_json_string_field(body, "direction")
        .ok()
        .and_then(|value| PtzDirection::parse(&value))
    {
        Some(direction) => direction,
        None => return error_response(400, "PTZ direction must be left, right, up, or down"),
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
            ptz.start_move(direction, speed)
        }
    };
    match send_ptz(inner, operation).await {
        Ok(()) => json_response(
            200,
            "OK",
            &format!(r#"{{"status":"ok","direction":"{direction}","speed":{speed}}}"#),
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

/// Sweep both axes onto their travel ends and park the head at the centre.
///
/// Accepted rather than performed inline: the sequence takes about thirty seconds,
/// which is longer than a client's request timeout and longer than this camera's
/// control plane should hold a connection open for. The response says the work has
/// started, `/ptz/position` reports whether it is still running, and a second
/// request while one is in flight is refused with 409 rather than starting a second
/// sweep on the same motors.
pub(crate) async fn ptz_center(inner: &Arc<ControlInner>) -> Vec<u8> {
    let Some(ptz) = inner.ptz.clone() else {
        return ptz_error_response(PtzFailure::Unavailable(
            "PTZ controller is not available".to_string(),
        ));
    };
    if inner.centering.swap(true, Ordering::AcqRel) {
        return error_response(409, "PTZ centring is already running");
    }
    let task_inner = Arc::clone(inner);
    tokio::task::spawn_blocking(move || {
        // Deliberately not holding the PTZ lock for the whole sweep: it takes half
        // a minute, and holding it would make `/ptz/position` - the one way to watch
        // progress, and the reason the flag above exists - block until the sweep
        // finished, with clients timing out against it in the meantime. Each ioctl
        // locks the device on its own, so the camera stays answerable; a move or a
        // stop from another client interleaves, and the park is still the last
        // thing that happens.
        match ptz.center() {
            Ok(()) => match ptz.position() {
                Ok((pan, tilt)) => tracing::info!(pan, tilt, "PTZ centred"),
                Err(error) => {
                    tracing::warn!(%error, "PTZ centred but its position could not be read")
                }
            },
            Err(error) => tracing::warn!(%error, "PTZ centring did not finish"),
        }
        task_inner.centering.store(false, Ordering::Release);
    });
    json_response(202, "Accepted", r#"{"status":"ok","centering":true}"#)
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

/// Telemetry for the Home Assistant integration, which polls this instead of asking
/// several endpoints. Field names follow the payload the integration already parses,
/// so the page that was written against the older firmware keeps working: it reads
/// `stream.active`, `speaker`, `night_vision`, `ptz.pan`/`ptz.tilt`, `memory` and
/// `temperature_c` from here.
pub(crate) async fn status_response(inner: &Arc<ControlInner>) -> Vec<u8> {
    let ready = inner.media.current().ok().flatten().is_some();
    let position = body_of(&ptz_position(inner).await);
    let talkback = body_of(&talkback_status_response(inner));
    let volume = inner.volume.load(Ordering::Acquire);
    let mode = inner.night_mode.load(Ordering::Acquire);
    let mode_name = ["auto", "night", "day"]
        .get(mode as usize)
        .unwrap_or(&"auto");
    // `active` is what the hardware is doing. A forced mode answers it on the
    // spot, because the companion applies it in the same call that accepts the
    // command; in automatic mode only the companion knows, so its report is used
    // and the last resort is "off". Deriving it from the commanded mode alone
    // reports the infrared as off for the whole of every automatic night.
    let night_vision = match mode {
        1 => true,
        2 => false,
        _ => inner.media.night_vision().unwrap_or(false),
    };
    let (total_kb, free_kb, available_kb, cached_kb) = memory_kb();
    let uptime = uptime_seconds();
    let temperature = temperature_celsius();

    // The stream fields the integration shows, assembled from the encoder's fixed
    // geometry and the engine's readiness rather than from a stats counter this
    // firmware does not have.
    let mut payload = format!(
        r#"{{"status":"{}","ready":{},"stream":{{"active":{},"resolution":"2304x1296","codec":"H265","uptime_sec":{uptime}}},"endpoints":{{"rtsp":"rtsps://{host}:322/live","api":"https://{host}:8080"}},"ptz":{{"pan":{},"tilt":{}}},"speaker":{{"active":{},"ready":true,"volume":{volume}}},"night_vision":{{"active":{},"mode":"{mode_name}"}},"memory":{{"total_kb":{total_kb},"free_kb":{free_kb},"available_kb":{available_kb},"cached_kb":{cached_kb}}},"api_tls":{{"enabled":true}}"#,
        if ready { "streaming" } else { "starting" },
        ready,
        ready,
        pan_of(&position),
        tilt_of(&position),
        talkback.contains(r#""talking":true"#),
        night_vision,
        host = security::host_label("HOST_NAME", "c220")
            .map(|label| format!("{label}.local"))
            .unwrap_or_else(|_| "c220.local".to_string()),
    );
    if let Some(temperature) = temperature {
        payload.push_str(&format!(r#","temperature_c":{temperature}"#));
    }
    payload.push('}');
    json_response(200, "OK", &payload)
}

/// The JSON body of a response produced by one of the shared handlers.
fn body_of(response: &[u8]) -> String {
    let text = String::from_utf8_lossy(response).to_string();
    match text.find("\r\n\r\n") {
        Some(index) => text[index + 4..].trim().to_string(),
        None => text.trim().to_string(),
    }
}

/// Pull one integer field out of a small JSON body without a JSON library: the
/// bodies read here are built by this file and have a fixed shape.
fn integer_field(body: &str, field: &str) -> i64 {
    let needle = format!("\"{field}\":");
    body.find(&needle)
        .and_then(|start| {
            let rest = &body[start + needle.len()..];
            let end = rest.find([',', '}']).unwrap_or(rest.len());
            rest[..end].trim().parse::<i64>().ok()
        })
        .unwrap_or(0)
}

fn pan_of(body: &str) -> i64 {
    integer_field(body, "pan")
}

fn tilt_of(body: &str) -> i64 {
    integer_field(body, "tilt")
}

/// Memory in kilobytes, in the units `/proc/meminfo` reports them.
fn memory_kb() -> (u64, u64, u64, u64) {
    let mut total = 0;
    let mut free = 0;
    let mut available = 0;
    let mut cached = 0;
    if let Ok(contents) = std::fs::read_to_string("/proc/meminfo") {
        for line in contents.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let Ok(kb) = value.trim().trim_end_matches(" kB").trim().parse::<u64>() else {
                continue;
            };
            match name {
                "MemTotal" => total = kb,
                "MemFree" => free = kb,
                "MemAvailable" => available = kb,
                "Cached" => cached = kb,
                _ => {}
            }
        }
    }
    (total, free, available, cached)
}

fn uptime_seconds() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| {
            text.split_whitespace()
                .next()
                .and_then(|value| value.parse::<f64>().ok())
        })
        .map(|seconds| seconds as u64)
        .unwrap_or(0)
}

/// SoC temperature, when the board exposes one.
fn temperature_celsius() -> Option<f32> {
    let text = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
    let milli = text.trim().parse::<f32>().ok()?;
    Some((milli / 1000.0 * 10.0).round() / 10.0)
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

/// The client's audio is accepted and will be written at the speaker's own pace.
fn queued_response(byte_count: usize) -> Vec<u8> {
    json_response(
        200,
        "OK",
        &format!(r#"{{"status":"ok","bytes_sent":{byte_count},"queued":true}}"#),
    )
}

/// A full queue means the client is ahead of real time. Saying so lets it pace itself;
/// silently blocking would turn its audio into stutter instead.
fn queue_error_response(error: mpsc::error::TrySendError<SpeakerFrame>) -> Vec<u8> {
    match error {
        mpsc::error::TrySendError::Full(_) => error_response(
            503,
            "speaker queue is full; send audio at or below real time",
        ),
        mpsc::error::TrySendError::Closed(_) => {
            error_response(503, "speaker writer is not running")
        }
    }
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
    use crate::media::{RecordKind, read_record};
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
    async fn the_maintenance_probe_tells_a_listener_from_a_free_port() {
        // The one-shot window is kept only when a shell is really reachable, so a
        // probe that answered "up" for anything would defeat the fix.
        let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("test listener");
        let bound_port = listener.local_addr().expect("bound address").port();
        assert!(maintenance_listener_is_up(bound_port).await);
        drop(listener);

        let free = StdTcpListener::bind(("127.0.0.1", 0)).expect("test listener");
        let free_port = free.local_addr().expect("bound address").port();
        drop(free);
        assert!(!maintenance_listener_is_up(free_port).await);
    }

    #[tokio::test]
    async fn the_reboot_route_is_not_public() {
        // Only the unauthorized path is exercised on purpose: a test that got past
        // the digest check here would ask the machine running the tests to reboot.
        let mut config = ControlServerConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)));
        config.auth = Some(Arc::new(
            DigestAuth::new("admin", "test-password").expect("test entropy should be available"),
        ));
        let server = ControlServer::start(config, MediaEngineSlot::default(), None)
            .expect("control server should start");
        let response = post(server.local_addr(), "/reboot", &[]).await;
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized"));
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
        let start_record = read_record(&mut companion)
            .expect("start command should read")
            .expect("start command should be present");
        assert_eq!(start_record.kind, RecordKind::SPEAKER_CLEAR);

        let response = post(server.local_addr(), "/talkback/pcm", &[1, 2]).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        let pcm_record = read_record(&mut companion)
            .expect("PCM command should read")
            .expect("PCM command should be present");
        assert_eq!(pcm_record.kind, RecordKind::SPEAKER_PCM);
        assert!(pcm_record.keyframe);
        assert_eq!(pcm_record.payload, vec![1, 2]);
    }
}
