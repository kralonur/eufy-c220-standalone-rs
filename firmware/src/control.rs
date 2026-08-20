//! Camera-local HTTP control and talkback service.
//!
//! This service is deliberately small and plaintext for the first camera
//! bring-up slice. It accepts bounded HTTP/1.1 requests and forwards only
//! validated speaker/night-mode commands through the shared `C220` media
//! socket. Authentication and TLS belong to a later deployment module.

use crate::media_engine::MediaEngineHandle;
use crate::motor::{HardwareMotor, PAN_MAX_DEGREES, PtzDirection};
use std::io;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// Default camera-local HTTP control port.
pub const DEFAULT_CONTROL_PORT: u16 = 8_080;

/// Maximum header bytes accepted from one control client.
const MAX_HEADER_LEN: usize = 8 * 1024;

/// Maximum request body accepted by the control service.
const MAX_BODY_LEN: usize = 64 * 1024;

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
#[derive(Clone, Debug)]
pub struct ControlServerConfig {
    /// Local address on which the plaintext control API listens.
    pub bind_address: SocketAddr,
}

impl ControlServerConfig {
    pub const fn new(bind_address: SocketAddr) -> Self {
        Self { bind_address }
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
        motor: Option<Arc<HardwareMotor>>,
    ) -> io::Result<Self> {
        let listener = StdTcpListener::bind(config.bind_address)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let listener = TcpListener::from_std(listener)?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            io::Error::other(format!("control server requires a Tokio runtime: {error}"))
        })?;
        let inner = Arc::new(ControlInner {
            media,
            motor,
            active: AtomicBool::new(false),
            next_timestamp_us: AtomicU64::new(0),
            volume: AtomicU32::new(DEFAULT_VOLUME),
            command_lock: Mutex::new(()),
            motor_lock: Mutex::new(()),
            shutdown: Notify::new(),
        });
        let accept_inner = Arc::clone(&inner);
        runtime.spawn(async move { accept_loop(listener, accept_inner).await });

        Ok(Self { inner, local_addr })
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

struct ControlInner {
    media: MediaEngineSlot,
    motor: Option<Arc<HardwareMotor>>,
    active: AtomicBool,
    next_timestamp_us: AtomicU64,
    volume: AtomicU32,
    command_lock: Mutex<()>,
    motor_lock: Mutex<()>,
    shutdown: Notify,
}

async fn accept_loop(listener: TcpListener, inner: Arc<ControlInner>) {
    loop {
        tokio::select! {
            _ = inner.shutdown.notified() => break,
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else { break };
                let client_inner = Arc::clone(&inner);
                tokio::spawn(async move {
                    let _ = handle_connection(stream, client_inner).await;
                });
            }
        }
    }
}

async fn handle_connection(mut stream: TcpStream, inner: Arc<ControlInner>) -> io::Result<()> {
    let response = match read_request(&mut stream).await {
        Ok(request) => dispatch(&inner, request).await,
        Err(error) => error_response(400, &error.to_string()),
    };
    stream.write_all(&response).await
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

async fn read_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
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
        body,
    })
}

fn find_bytes(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len())
        .position(|window| window == needle)
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
        ("GET", "/ptz/position") | ("GET", "/ptz/status") => ptz_position(inner).await,
        ("POST", "/ptz/move") => ptz_move(inner, &request.body).await,
        ("POST", "/ptz/stop") => ptz_stop(inner).await,
        ("POST", "/ptz/center") => ptz_center(inner).await,
        _ => error_response(404, "control endpoint not found"),
    }
}

async fn talkback_start(inner: &Arc<ControlInner>) -> Vec<u8> {
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

async fn talkback_stop(inner: &Arc<ControlInner>) -> Vec<u8> {
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

async fn night_mode(inner: &Arc<ControlInner>, body: &[u8]) -> Vec<u8> {
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

async fn ptz_position(inner: &Arc<ControlInner>) -> Vec<u8> {
    match send_motor(inner, |motor| motor.position()).await {
        Ok((pan, tilt)) => json_response(
            200,
            "OK",
            &format!(r#"{{"status":"ok","pan":{pan},"tilt":{tilt}}}"#),
        ),
        Err(error) => motor_error_response(error),
    }
}

async fn ptz_move(inner: &Arc<ControlInner>, body: &[u8]) -> Vec<u8> {
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

    let operation = move |motor: &HardwareMotor| {
        if direction == PtzDirection::Stop {
            motor.stop()
        } else {
            motor.move_step(direction, degrees, speed)
        }
    };
    match send_motor(inner, operation).await {
        Ok(()) => json_response(
            200,
            "OK",
            &format!(
                r#"{{"status":"ok","direction":"{direction}","degrees":{degrees},"speed":{speed}}}"#
            ),
        ),
        Err(error) => motor_error_response(error),
    }
}

async fn ptz_stop(inner: &Arc<ControlInner>) -> Vec<u8> {
    match send_motor(inner, |motor| motor.stop()).await {
        Ok(()) => json_response(200, "OK", r#"{"status":"ok"}"#),
        Err(error) => motor_error_response(error),
    }
}

async fn ptz_center(inner: &Arc<ControlInner>) -> Vec<u8> {
    match send_motor(inner, |motor| {
        motor.center(5)?;
        motor.position()
    })
    .await
    {
        Ok((pan, tilt)) => json_response(
            200,
            "OK",
            &format!(r#"{{"status":"ok","pan":{pan},"tilt":{tilt}}}"#),
        ),
        Err(error) => motor_error_response(error),
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

async fn send_motor<F, T>(inner: &Arc<ControlInner>, operation: F) -> Result<T, MotorFailure>
where
    F: FnOnce(&HardwareMotor) -> io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let motor = inner
        .motor
        .clone()
        .ok_or_else(|| MotorFailure::Unavailable("PTZ motor is not available".to_string()))?;
    let motor_inner = Arc::clone(inner);
    tokio::task::spawn_blocking(move || {
        let _lock = motor_inner
            .motor_lock
            .lock()
            .map_err(|_| MotorFailure::Unavailable("PTZ motor lock poisoned".to_string()))?;
        operation(&motor).map_err(|error| match error.kind() {
            io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
                MotorFailure::Rejected(error.to_string())
            }
            _ => MotorFailure::Unavailable(error.to_string()),
        })
    })
    .await
    .map_err(|error| MotorFailure::Unavailable(format!("PTZ worker failed: {error}")))?
}

#[derive(Debug)]
enum MotorFailure {
    Rejected(String),
    Unavailable(String),
}

fn motor_error_response(error: MotorFailure) -> Vec<u8> {
    match error {
        MotorFailure::Rejected(message) => error_response(400, &message),
        MotorFailure::Unavailable(message) => error_response(503, &message),
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

fn health_response(inner: &ControlInner) -> Vec<u8> {
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

fn talkback_status_response(inner: &ControlInner) -> Vec<u8> {
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
        404 => "Not Found",
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
