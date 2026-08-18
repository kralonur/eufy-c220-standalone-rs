//! Camera-local RTSP video service.
//!
//! The server accepts RTSP 1.0 control requests and sends the encoded Annex-B
//! access units received from the camera media engine as RTP interleaved over
//! TCP. The RTP packetization follows RFC 6184 (H.264) and RFC 7798 (H.265).
//!
//! This module is part of the custom camera firmware. It has no Eufy/P2P
//! protocol handling. The 2K dimensions and the H.265 default are based on the
//! reverse-engineered and measured camera media path documented in
//! `docs/hardware-video.md` from the reference project.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bytes::Bytes;
use rtp::codecs::h264::H264Payloader;
use rtp::packetizer::Payloader;
use std::io;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify, broadcast};
use tokio::task::JoinHandle;

use crate::media::{MediaRecord, RecordKind};

/// Default RTSP listener port for the camera-local endpoint.
pub const DEFAULT_RTSP_PORT: u16 = 554;

/// Default RTSP resource path.
pub const DEFAULT_RTSP_PATH: &str = "/live";

/// Validated sensor frame width from the camera's 2K self-path.
pub const DEFAULT_VIDEO_WIDTH: u32 = 2_304;

/// Validated sensor frame height from the camera's 2K self-path.
pub const DEFAULT_VIDEO_HEIGHT: u32 = 1_296;

/// RTP clock rate for H.264 and H.265 video.
pub const RTP_VIDEO_CLOCK_RATE: u32 = 90_000;

/// Fallback RTP timestamp increment for sources without capture timestamps.
/// 15 FPS is the measured camera mode and the declared encoder rate in the
/// reverse-engineered media engine.
pub const RTP_VIDEO_TIMESTAMP_INCREMENT: u32 = 4_500;

/// Dynamic RTP payload type used for the single video track.
pub const RTP_VIDEO_PAYLOAD_TYPE: u8 = 96;

/// Maximum RTP payload before the codec payloader fragments a NAL unit.
/// 1,200 bytes keeps interleaved RTP packets below the common Ethernet MTU.
const RTP_PAYLOAD_MTU: usize = 1_200;

/// Number of records retained for clients that are briefly slower than the
/// camera. A lagging client is resynchronized at the next keyframe.
const MEDIA_BUS_CAPACITY: usize = 256;

/// Maximum RTSP request/control buffer accepted from one client.
const MAX_REQUEST_BUFFER: usize = 64 * 1024;

/// Maximum time DESCRIBE waits for the encoder's parameter sets.
const CODEC_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval while waiting for parameter sets or an idle media source.
const CODEC_READY_POLL: Duration = Duration::from_millis(100);

/// H.264 profile-level-id used only if an unusually short SPS lacks the three
/// profile bytes. Normal camera SPS records always provide these bytes.
const DEFAULT_H264_PROFILE_LEVEL_ID: &str = "42e01f";

/// Initial deterministic RTP SSRC. Each client receives a distinct value.
const FIRST_RTP_SSRC: u32 = 0xc220_0001;

const STATUS_OK: u16 = 200;
const STATUS_NOT_FOUND: u16 = 404;
const STATUS_SESSION_NOT_FOUND: u16 = 454;
const STATUS_METHOD_NOT_VALID: u16 = 455;
const STATUS_UNSUPPORTED_TRANSPORT: u16 = 461;
const STATUS_SERVICE_UNAVAILABLE: u16 = 503;
const STATUS_NOT_IMPLEMENTED: u16 = 501;

/// Codec reported by a camera-local video record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VideoCodec {
    #[default]
    H264,
    H265,
}

impl VideoCodec {
    /// Decode the video codec byte from the local `C220` record ABI.
    pub const fn from_record_byte(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::H264),
            2 => Some(Self::H265),
            _ => None,
        }
    }

    /// Return the codec name used by SDP.
    pub const fn sdp_name(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::H265 => "H265",
        }
    }
}

/// RTSP listener configuration.
#[derive(Clone, Debug)]
pub struct RtspServerConfig {
    /// Local address on which the camera serves RTSP.
    pub bind_address: SocketAddr,
    /// Resource path advertised by DESCRIBE.
    pub path: String,
    /// Encoded video width advertised in SDP.
    pub video_width: u32,
    /// Encoded video height advertised in SDP.
    pub video_height: u32,
}

impl RtspServerConfig {
    pub fn new(bind_address: SocketAddr) -> Self {
        Self {
            bind_address,
            path: DEFAULT_RTSP_PATH.to_string(),
            video_width: DEFAULT_VIDEO_WIDTH,
            video_height: DEFAULT_VIDEO_HEIGHT,
        }
    }
}

/// Counters for the camera-local RTSP service.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtspStats {
    pub video_access_units_published: u64,
    pub video_access_units_sent: u64,
}

#[derive(Clone, Debug)]
struct VideoFrame {
    data: Arc<[u8]>,
    keyframe: bool,
    codec: VideoCodec,
    timestamp_us: Option<u64>,
}

#[derive(Clone, Debug, Default)]
struct CodecState {
    codec: VideoCodec,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl CodecState {
    fn has_parameters(&self) -> bool {
        match self.codec {
            VideoCodec::H264 => self.sps.is_some() && self.pps.is_some(),
            VideoCodec::H265 => self.vps.is_some() && self.sps.is_some() && self.pps.is_some(),
        }
    }
}

struct RtspServerInner {
    path: String,
    video_width: u32,
    video_height: u32,
    media_tx: broadcast::Sender<VideoFrame>,
    codec: RwLock<CodecState>,
    codec_ready: Notify,
    shutdown: Notify,
    next_session: AtomicU64,
    next_ssrc: AtomicU32,
    video_access_units_published: AtomicU64,
    video_access_units_sent: AtomicU64,
}

/// Multi-client camera-local RTSP server.
///
/// The server never reads the sensor itself. The media engine owns capture and
/// encoding; callers publish complete Annex-B access units with
/// [`RtspServer::push_video_frame`]. Each client receives an independent RTP
/// sequence space and packetizer, so a slow client cannot stall another one.
pub struct RtspServer {
    inner: Arc<RtspServerInner>,
    handles: Arc<AtomicUsize>,
    local_addr: SocketAddr,
}

impl Clone for RtspServer {
    fn clone(&self) -> Self {
        self.handles.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
            handles: Arc::clone(&self.handles),
            local_addr: self.local_addr,
        }
    }
}

impl std::fmt::Debug for RtspServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RtspServer")
            .field("bind_address", &self.local_addr)
            .field("path", &self.inner.path)
            .finish()
    }
}

impl RtspServer {
    /// Start the camera-local RTSP listener.
    ///
    /// This function must be called from a Tokio runtime because client
    /// sessions and the accept loop are asynchronous.
    pub fn start(config: RtspServerConfig) -> io::Result<Self> {
        let path = normalize_path(&config.path)?;
        let listener = StdTcpListener::bind(config.bind_address)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let listener = TcpListener::from_std(listener)?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            io::Error::other(format!("RTSP server requires a Tokio runtime: {error}"))
        })?;

        let (media_tx, _) = broadcast::channel(MEDIA_BUS_CAPACITY);
        let inner = Arc::new(RtspServerInner {
            path,
            video_width: config.video_width,
            video_height: config.video_height,
            media_tx,
            codec: RwLock::new(CodecState::default()),
            codec_ready: Notify::new(),
            shutdown: Notify::new(),
            next_session: AtomicU64::new(1),
            next_ssrc: AtomicU32::new(FIRST_RTP_SSRC),
            video_access_units_published: AtomicU64::new(0),
            video_access_units_sent: AtomicU64::new(0),
        });
        let handles = Arc::new(AtomicUsize::new(1));
        let accept_inner = Arc::clone(&inner);
        runtime.spawn(async move { accept_loop(listener, accept_inner).await });

        Ok(Self {
            inner,
            handles,
            local_addr,
        })
    }

    /// Address selected by the operating system, useful when binding port 0.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn path(&self) -> &str {
        &self.inner.path
    }

    /// Publish one encoded Annex-B access unit.
    pub fn push_video_frame(
        &self,
        annex_b: &[u8],
        keyframe: bool,
        codec: VideoCodec,
        timestamp_us: Option<u64>,
    ) -> io::Result<()> {
        if annex_b.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "video access unit must not be empty",
            ));
        }

        let codec_changed = self
            .inner
            .codec
            .write()
            .map_err(|_| io::Error::other("RTSP codec state lock poisoned"))
            .map(|mut state| update_codec_state(&mut state, codec, annex_b))?;
        if codec_changed {
            self.inner.codec_ready.notify_waiters();
        }

        let frame = VideoFrame {
            data: Arc::<[u8]>::from(annex_b.to_vec()),
            keyframe: keyframe || detected_keyframe(codec, annex_b),
            codec,
            timestamp_us,
        };
        self.inner
            .video_access_units_published
            .fetch_add(1, Ordering::Relaxed);
        let _ = self.inner.media_tx.send(frame);
        Ok(())
    }

    /// Publish a video record directly from the camera-local media ABI.
    pub fn push_video_record(&self, record: &MediaRecord) -> io::Result<()> {
        if record.kind != RecordKind::VIDEO && record.kind != RecordKind::VIDEO_SUB {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media record is not a video record",
            ));
        }
        let codec = VideoCodec::from_record_byte(record.codec).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported video codec byte {}", record.codec),
            )
        })?;
        self.push_video_frame(
            &record.payload,
            record.keyframe,
            codec,
            Some(record.timestamp_us),
        )
    }

    /// Return the current SDP once the encoder's parameter sets are known.
    pub fn sdp(&self) -> Option<String> {
        let codec = self.inner.codec.read().ok()?.clone();
        build_sdp(&codec, self.inner.video_width, self.inner.video_height)
    }

    pub fn video_codec(&self) -> Option<VideoCodec> {
        self.inner.codec.read().ok().map(|state| state.codec)
    }

    pub fn stats(&self) -> RtspStats {
        RtspStats {
            video_access_units_published: self
                .inner
                .video_access_units_published
                .load(Ordering::Relaxed),
            video_access_units_sent: self.inner.video_access_units_sent.load(Ordering::Relaxed),
        }
    }
}

impl Drop for RtspServer {
    fn drop(&mut self) {
        if self.handles.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.inner.shutdown.notify_waiters();
        }
    }
}

async fn accept_loop(listener: TcpListener, inner: Arc<RtspServerInner>) {
    loop {
        tokio::select! {
            _ = inner.shutdown.notified() => break,
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else { break };
                if stream.set_nodelay(true).is_err() {
                    continue;
                }
                let client_inner = Arc::clone(&inner);
                tokio::spawn(async move {
                    let _ = handle_connection(stream, client_inner).await;
                });
            }
        }
    }
}

struct ClientState {
    session_id: Option<String>,
    video_channel: Option<u8>,
    media_task: Option<JoinHandle<()>>,
}

impl ClientState {
    fn stop_media(&mut self) {
        if let Some(task) = self.media_task.take() {
            task.abort();
        }
    }

    fn session_matches(&self, request: &RtspRequest) -> bool {
        let Some(expected) = self.session_id.as_deref() else {
            return false;
        };
        request
            .header("session")
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            == Some(expected)
    }
}

async fn handle_connection(stream: TcpStream, inner: Arc<RtspServerInner>) -> io::Result<()> {
    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut state = ClientState {
        session_id: None,
        video_channel: None,
        media_task: None,
    };
    let mut buffer = Vec::new();

    loop {
        let request = tokio::select! {
            _ = inner.shutdown.notified() => break,
            result = read_request(&mut reader, &mut buffer) => result?,
        };
        let Some(request) = request else { break };
        if handle_request(&inner, &writer, &mut state, request).await? {
            break;
        }
    }

    state.stop_media();
    Ok(())
}

async fn handle_request(
    inner: &Arc<RtspServerInner>,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    state: &mut ClientState,
    request: RtspRequest,
) -> io::Result<bool> {
    let cseq = request.cseq();
    match request.method.as_str() {
        "OPTIONS" => {
            send_response(
                writer,
                cseq,
                STATUS_OK,
                "OK",
                &[(
                    "Public",
                    "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, GET_PARAMETER, TEARDOWN",
                )],
                &[],
            )
            .await?;
        }
        "DESCRIBE" => {
            if uri_path(&request.uri) != inner.path {
                send_response(writer, cseq, STATUS_NOT_FOUND, "Not Found", &[], &[]).await?;
            } else if !wait_for_codec(inner).await {
                send_response(
                    writer,
                    cseq,
                    STATUS_SERVICE_UNAVAILABLE,
                    "Service Unavailable",
                    &[],
                    &[],
                )
                .await?;
            } else if let Some(sdp) = build_sdp_from_inner(inner) {
                let content_base = content_base(&request.uri);
                send_response(
                    writer,
                    cseq,
                    STATUS_OK,
                    "OK",
                    &[
                        ("Content-Base", content_base.as_str()),
                        ("Content-Type", "application/sdp"),
                    ],
                    sdp.as_bytes(),
                )
                .await?;
            } else {
                send_response(
                    writer,
                    cseq,
                    STATUS_SERVICE_UNAVAILABLE,
                    "Service Unavailable",
                    &[],
                    &[],
                )
                .await?;
            }
        }
        "SETUP" => {
            if uri_path(&request.uri) != format!("{}/trackID=0", inner.path) {
                send_response(writer, cseq, STATUS_NOT_FOUND, "Not Found", &[], &[]).await?;
            } else if state.session_id.is_some() && !state.session_matches(&request) {
                send_response(
                    writer,
                    cseq,
                    STATUS_SESSION_NOT_FOUND,
                    "Session Not Found",
                    &[],
                    &[],
                )
                .await?;
            } else {
                let Some(transport_header) = request.header("transport") else {
                    send_response(
                        writer,
                        cseq,
                        STATUS_UNSUPPORTED_TRANSPORT,
                        "Unsupported Transport",
                        &[],
                        &[],
                    )
                    .await?;
                    return Ok(false);
                };
                let channel = match parse_interleaved_transport(transport_header) {
                    Ok(channel) => channel,
                    Err(()) => {
                        send_response(
                            writer,
                            cseq,
                            STATUS_UNSUPPORTED_TRANSPORT,
                            "Unsupported Transport",
                            &[],
                            &[],
                        )
                        .await?;
                        return Ok(false);
                    }
                };
                let session_id = state.session_id.get_or_insert_with(|| {
                    format!("{:08x}", inner.next_session.fetch_add(1, Ordering::Relaxed))
                });
                state.video_channel = Some(channel);
                let transport =
                    format!("RTP/AVP/TCP;unicast;interleaved={channel}-{}", channel + 1);
                send_response(
                    writer,
                    cseq,
                    STATUS_OK,
                    "OK",
                    &[
                        ("Session", session_id.as_str()),
                        ("Transport", transport.as_str()),
                    ],
                    &[],
                )
                .await?;
            }
        }
        "PLAY" => {
            if !state.session_matches(&request) {
                send_response(
                    writer,
                    cseq,
                    STATUS_SESSION_NOT_FOUND,
                    "Session Not Found",
                    &[],
                    &[],
                )
                .await?;
            } else if state.video_channel.is_none() {
                send_response(
                    writer,
                    cseq,
                    STATUS_METHOD_NOT_VALID,
                    "Method Not Valid in This State",
                    &[],
                    &[],
                )
                .await?;
            } else {
                let session_id = state.session_id.as_deref().unwrap_or_default();
                send_response(
                    writer,
                    cseq,
                    STATUS_OK,
                    "OK",
                    &[("Session", session_id)],
                    &[],
                )
                .await?;
                state.stop_media();
                let channel = state.video_channel.unwrap_or(0);
                let receiver = inner.media_tx.subscribe();
                let media_inner = Arc::clone(inner);
                let media_writer = Arc::clone(writer);
                state.media_task = Some(tokio::spawn(async move {
                    media_loop(media_inner, media_writer, channel, receiver).await;
                }));
            }
        }
        "PAUSE" => {
            if !state.session_matches(&request) {
                send_response(
                    writer,
                    cseq,
                    STATUS_SESSION_NOT_FOUND,
                    "Session Not Found",
                    &[],
                    &[],
                )
                .await?;
            } else {
                state.stop_media();
                let session_id = state.session_id.as_deref().unwrap_or_default();
                send_response(
                    writer,
                    cseq,
                    STATUS_OK,
                    "OK",
                    &[("Session", session_id)],
                    &[],
                )
                .await?;
            }
        }
        "GET_PARAMETER" | "SET_PARAMETER" => {
            if state.session_id.is_some() && !state.session_matches(&request) {
                send_response(
                    writer,
                    cseq,
                    STATUS_SESSION_NOT_FOUND,
                    "Session Not Found",
                    &[],
                    &[],
                )
                .await?;
            } else {
                let session = state.session_id.as_deref().unwrap_or_default();
                let headers = if session.is_empty() {
                    Vec::new()
                } else {
                    vec![("Session", session)]
                };
                send_response(writer, cseq, STATUS_OK, "OK", &headers, &[]).await?;
            }
        }
        "TEARDOWN" => {
            if state.session_id.is_some() && !state.session_matches(&request) {
                send_response(
                    writer,
                    cseq,
                    STATUS_SESSION_NOT_FOUND,
                    "Session Not Found",
                    &[],
                    &[],
                )
                .await?;
            } else {
                state.stop_media();
                let session = state.session_id.as_deref().unwrap_or_default();
                let headers = if session.is_empty() {
                    Vec::new()
                } else {
                    vec![("Session", session)]
                };
                send_response(writer, cseq, STATUS_OK, "OK", &headers, &[]).await?;
                return Ok(true);
            }
        }
        _ => {
            send_response(
                writer,
                cseq,
                STATUS_NOT_IMPLEMENTED,
                "Not Implemented",
                &[],
                &[],
            )
            .await?;
        }
    }
    Ok(false)
}

async fn wait_for_codec(inner: &RtspServerInner) -> bool {
    if inner
        .codec
        .read()
        .map(|state| state.has_parameters())
        .unwrap_or(false)
    {
        return true;
    }

    tokio::time::timeout(CODEC_READY_TIMEOUT, async {
        loop {
            if inner
                .codec
                .read()
                .map(|state| state.has_parameters())
                .unwrap_or(false)
            {
                return true;
            }
            tokio::select! {
                _ = inner.codec_ready.notified() => {}
                _ = tokio::time::sleep(CODEC_READY_POLL) => {}
                _ = inner.shutdown.notified() => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

async fn media_loop(
    inner: Arc<RtspServerInner>,
    writer: Arc<Mutex<OwnedWriteHalf>>,
    channel: u8,
    mut receiver: broadcast::Receiver<VideoFrame>,
) {
    let mut packetizer: Option<VideoPacketizer> = None;
    let mut waiting_for_keyframe = true;

    loop {
        let frame = tokio::select! {
            _ = inner.shutdown.notified() => break,
            received = receiver.recv() => match received {
                Ok(frame) => frame,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    waiting_for_keyframe = true;
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        };

        if waiting_for_keyframe && !frame.keyframe {
            continue;
        }

        if packetizer.as_ref().map(|value| value.codec()) != Some(frame.codec) {
            let ssrc = inner.next_ssrc.fetch_add(1, Ordering::Relaxed);
            packetizer = Some(VideoPacketizer::new(frame.codec, ssrc));
        }
        let Some(packetizer) = packetizer.as_mut() else {
            continue;
        };
        let packets = packetizer.packetize(&frame);
        if packets.is_empty() {
            waiting_for_keyframe = true;
            continue;
        }

        for packet in packets {
            if write_interleaved(&writer, channel, &packet).await.is_err() {
                return;
            }
        }
        waiting_for_keyframe = false;
        inner
            .video_access_units_sent
            .fetch_add(1, Ordering::Relaxed);
    }
}

async fn write_interleaved(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    channel: u8,
    payload: &[u8],
) -> io::Result<()> {
    let length = u16::try_from(payload.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "RTP packet exceeds RTSP interleaved length",
        )
    })?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&[b'$', channel]);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);

    let mut writer = writer.lock().await;
    writer.write_all(&frame).await
}

#[derive(Debug)]
struct RtspRequest {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
}

impl RtspRequest {
    fn cseq(&self) -> u32 {
        self.header("cseq")
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

async fn read_request(
    reader: &mut OwnedReadHalf,
    buffer: &mut Vec<u8>,
) -> io::Result<Option<RtspRequest>> {
    loop {
        if buffer.first() == Some(&b'$') {
            if buffer.len() >= 4 {
                let length = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;
                let total = 4 + length;
                if buffer.len() >= total {
                    buffer.drain(..total);
                    continue;
                }
            }
        } else if let Some(end) = find_header_end(buffer) {
            let raw: Vec<u8> = buffer.drain(..end).collect();
            return parse_request(&raw).map(Some);
        }

        if buffer.len() >= MAX_REQUEST_BUFFER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RTSP request exceeds configured limit",
            ));
        }
        let mut chunk = [0_u8; 4 * 1024];
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            if buffer.is_empty() {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "RTSP request truncated",
            ));
        }
        buffer.extend_from_slice(&chunk[..count]);
    }
}

fn parse_request(raw: &[u8]) -> io::Result<RtspRequest> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "RTSP request is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "RTSP request has no request line",
        )
    })?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "RTSP request has no method"))?
        .to_ascii_uppercase();
    let uri = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "RTSP request has no URI"))?
        .to_string();
    let version = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "RTSP request has no version"))?;
    if version != "RTSP/1.0" || parts.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported RTSP request line",
        ));
    }

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "RTSP header has no colon")
        })?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    Ok(RtspRequest {
        method,
        uri,
        headers,
    })
}

async fn send_response(
    writer: &Arc<Mutex<OwnedWriteHalf>>,
    cseq: u32,
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    let mut response = format!(
        "RTSP/1.0 {status} {reason}\r\nCSeq: {cseq}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");

    let mut writer = writer.lock().await;
    writer.write_all(response.as_bytes()).await?;
    writer.write_all(body).await
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn normalize_path(path: &str) -> io::Result<String> {
    let path = path.trim_end_matches('/');
    if path.is_empty() || !path.starts_with('/') || path.contains(' ') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "RTSP path must be an absolute resource path",
        ));
    }
    Ok(path.to_string())
}

fn uri_path(uri: &str) -> &str {
    let path = if let Some(authority) = uri.split_once("://").map(|(_, rest)| rest) {
        authority
            .find('/')
            .map(|position| &authority[position..])
            .unwrap_or("/")
    } else {
        uri
    };
    path.split('?').next().unwrap_or(path)
}

fn content_base(uri: &str) -> String {
    if uri.ends_with('/') {
        uri.to_string()
    } else {
        format!("{uri}/")
    }
}

fn parse_interleaved_transport(value: &str) -> Result<u8, ()> {
    let mut parts = value.split(';');
    let profile = parts.next().ok_or(())?;
    if !profile.eq_ignore_ascii_case("RTP/AVP/TCP") {
        return Err(());
    }

    let channel_pair = parts
        .find_map(|part| part.strip_prefix("interleaved="))
        .unwrap_or("0-1");
    let mut channels = channel_pair.split('-');
    let rtp_channel: u8 = channels.next().ok_or(())?.parse().map_err(|_| ())?;
    let rtcp_channel: u8 = channels.next().ok_or(())?.parse().map_err(|_| ())?;
    if channels.next().is_some()
        || rtp_channel == u8::MAX
        || rtcp_channel != rtp_channel.saturating_add(1)
    {
        return Err(());
    }
    Ok(rtp_channel)
}

fn build_sdp_from_inner(inner: &RtspServerInner) -> Option<String> {
    let codec = inner.codec.read().ok()?.clone();
    build_sdp(&codec, inner.video_width, inner.video_height)
}

fn build_sdp(codec: &CodecState, width: u32, height: u32) -> Option<String> {
    if !codec.has_parameters() {
        return None;
    }

    let mut sdp = format!(
        concat!(
            "v=0\r\n",
            "o=- 0 0 IN IP4 0.0.0.0\r\n",
            "s=C220 Camera\r\n",
            "t=0 0\r\n",
            "a=control:*\r\n",
            "m=video 0 RTP/AVP {}\r\n",
            "a=rtpmap:{} {}/90000\r\n",
            "a=framesize:{} {}-{}\r\n",
        ),
        RTP_VIDEO_PAYLOAD_TYPE,
        RTP_VIDEO_PAYLOAD_TYPE,
        codec.codec.sdp_name(),
        RTP_VIDEO_PAYLOAD_TYPE,
        width,
        height,
    );

    match codec.codec {
        VideoCodec::H264 => {
            let sps = codec.sps.as_ref()?;
            let pps = codec.pps.as_ref()?;
            let profile_level_id = sps
                .get(1..4)
                .map(hex_lower)
                .unwrap_or_else(|| DEFAULT_H264_PROFILE_LEVEL_ID.to_string());
            sdp.push_str(&format!(
                "a=fmtp:{RTP_VIDEO_PAYLOAD_TYPE} packetization-mode=1;profile-level-id={profile_level_id};sprop-parameter-sets={},{}\r\n",
                BASE64.encode(sps),
                BASE64.encode(pps),
            ));
        }
        VideoCodec::H265 => {
            let vps = codec.vps.as_ref()?;
            let sps = codec.sps.as_ref()?;
            let pps = codec.pps.as_ref()?;
            sdp.push_str(&format!(
                "a=fmtp:{RTP_VIDEO_PAYLOAD_TYPE} sprop-vps={};sprop-sps={};sprop-pps={}\r\n",
                BASE64.encode(vps),
                BASE64.encode(sps),
                BASE64.encode(pps),
            ));
        }
    }
    sdp.push_str("a=control:trackID=0\r\na=sendonly\r\n");
    Some(sdp)
}

fn update_codec_state(state: &mut CodecState, codec: VideoCodec, annex_b: &[u8]) -> bool {
    let mut changed = false;
    if state.codec != codec {
        *state = CodecState {
            codec,
            ..CodecState::default()
        };
        changed = true;
    }

    for nal in annex_b_nalus(annex_b) {
        match codec {
            VideoCodec::H264 => {
                if !is_valid_h264_nal(nal) {
                    continue;
                }
                match nal[0] & 0x1f {
                    7 => {
                        changed |= state.sps.as_deref() != Some(nal);
                        state.sps = Some(nal.to_vec());
                    }
                    8 => {
                        changed |= state.pps.as_deref() != Some(nal);
                        state.pps = Some(nal.to_vec());
                    }
                    _ => {}
                }
            }
            VideoCodec::H265 => {
                if nal.len() < 2 {
                    continue;
                }
                match (nal[0] >> 1) & 0x3f {
                    32 => {
                        changed |= state.vps.as_deref() != Some(nal);
                        state.vps = Some(nal.to_vec());
                    }
                    33 => {
                        changed |= state.sps.as_deref() != Some(nal);
                        state.sps = Some(nal.to_vec());
                    }
                    34 => {
                        changed |= state.pps.as_deref() != Some(nal);
                        state.pps = Some(nal.to_vec());
                    }
                    _ => {}
                }
            }
        }
    }
    changed
}

fn detected_keyframe(codec: VideoCodec, annex_b: &[u8]) -> bool {
    annex_b_nalus(annex_b).into_iter().any(|nal| match codec {
        VideoCodec::H264 => is_valid_h264_nal(nal) && nal[0] & 0x1f == 5,
        VideoCodec::H265 => nal.len() >= 2 && matches!((nal[0] >> 1) & 0x3f, 16..=23),
    })
}

fn annex_b_nalus(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            starts.push((index, 3));
            index += 3;
        } else if index + 4 <= data.len() && data[index..].starts_with(&[0, 0, 0, 1]) {
            starts.push((index, 4));
            index += 4;
        } else {
            index += 1;
        }
    }

    starts
        .iter()
        .enumerate()
        .filter_map(|(position, (start, prefix_len))| {
            let end = starts
                .get(position + 1)
                .map(|(next, _)| *next)
                .unwrap_or(data.len());
            let nal_start = start + prefix_len;
            (nal_start < end).then_some(&data[nal_start..end])
        })
        .collect()
}

fn is_valid_h264_nal(nal: &[u8]) -> bool {
    !nal.is_empty() && nal[0] & 0x80 == 0 && matches!(nal[0] & 0x1f, 1..=23)
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

enum VideoPacketizer {
    H264(H264Packetizer),
    H265(H265Packetizer),
}

impl VideoPacketizer {
    fn new(codec: VideoCodec, ssrc: u32) -> Self {
        match codec {
            VideoCodec::H264 => Self::H264(H264Packetizer::new(ssrc)),
            VideoCodec::H265 => Self::H265(H265Packetizer::new(ssrc)),
        }
    }

    fn codec(&self) -> VideoCodec {
        match self {
            Self::H264(_) => VideoCodec::H264,
            Self::H265(_) => VideoCodec::H265,
        }
    }

    fn packetize(&mut self, frame: &VideoFrame) -> Vec<Vec<u8>> {
        let payloads = match self {
            Self::H264(packetizer) => packetizer.payloads(&frame.data, frame.keyframe),
            Self::H265(packetizer) => packetizer.payloads(&frame.data, frame.keyframe),
        };
        let timestamp = match self {
            Self::H264(packetizer) => packetizer.timestamp(frame.timestamp_us),
            Self::H265(packetizer) => packetizer.timestamp(frame.timestamp_us),
        };
        let ssrc = match self {
            Self::H264(packetizer) => packetizer.ssrc,
            Self::H265(packetizer) => packetizer.ssrc,
        };

        let count = payloads.len();
        payloads
            .into_iter()
            .enumerate()
            .map(|(index, payload)| {
                let current_sequence = self.next_sequence();
                rtp_packet(
                    RTP_VIDEO_PAYLOAD_TYPE,
                    index + 1 == count,
                    current_sequence,
                    timestamp,
                    ssrc,
                    &payload,
                )
            })
            .collect()
    }

    fn next_sequence(&mut self) -> u16 {
        match self {
            Self::H264(packetizer) => {
                let sequence = packetizer.sequence;
                packetizer.sequence = packetizer.sequence.wrapping_add(1);
                sequence
            }
            Self::H265(packetizer) => {
                let sequence = packetizer.sequence;
                packetizer.sequence = packetizer.sequence.wrapping_add(1);
                sequence
            }
        }
    }
}

struct H264Packetizer {
    sequence: u16,
    timestamp_value: u32,
    timestamp_base: u32,
    source_base_us: Option<u64>,
    last_source_us: Option<u64>,
    ssrc: u32,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl H264Packetizer {
    fn new(ssrc: u32) -> Self {
        Self {
            sequence: 0,
            timestamp_value: 0,
            timestamp_base: 0,
            source_base_us: None,
            last_source_us: None,
            ssrc,
            sps: None,
            pps: None,
        }
    }

    fn payloads(&mut self, data: &[u8], keyframe: bool) -> Vec<Bytes> {
        let nalus = annex_b_nalus(data);
        let mut vcl_data = Vec::new();
        let mut has_vcl = false;
        for nal in nalus {
            if !is_valid_h264_nal(nal) {
                continue;
            }
            match nal[0] & 0x1f {
                7 => self.sps = Some(nal.to_vec()),
                8 => self.pps = Some(nal.to_vec()),
                1..=5 => {
                    has_vcl = true;
                    vcl_data.extend_from_slice(&[0, 0, 0, 1]);
                    vcl_data.extend_from_slice(nal);
                }
                _ => {}
            }
        }
        if !has_vcl {
            return Vec::new();
        }

        let mut payloads = Vec::new();
        if keyframe
            && let (Some(sps), Some(pps)) = (self.sps.as_deref(), self.pps.as_deref())
            && let Some(parameter_payload) = aggregate_h264(sps, pps)
            && parameter_payload.len() <= RTP_PAYLOAD_MTU
        {
            payloads.push(Bytes::from(parameter_payload));
        }

        let mut payloader = H264Payloader::default();
        if let Ok(vcl_payloads) = payloader.payload(RTP_PAYLOAD_MTU, &Bytes::from(vcl_data)) {
            payloads.extend(vcl_payloads);
        }
        payloads
    }

    fn timestamp(&mut self, source_us: Option<u64>) -> u32 {
        timestamp_from_source(
            &mut self.timestamp_value,
            &mut self.timestamp_base,
            &mut self.source_base_us,
            &mut self.last_source_us,
            source_us,
        )
    }
}

struct H265Packetizer {
    sequence: u16,
    timestamp_value: u32,
    timestamp_base: u32,
    source_base_us: Option<u64>,
    last_source_us: Option<u64>,
    ssrc: u32,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl H265Packetizer {
    fn new(ssrc: u32) -> Self {
        Self {
            sequence: 0,
            timestamp_value: 0,
            timestamp_base: 0,
            source_base_us: None,
            last_source_us: None,
            ssrc,
            vps: None,
            sps: None,
            pps: None,
        }
    }

    fn payloads(&mut self, data: &[u8], keyframe: bool) -> Vec<Bytes> {
        let nalus = annex_b_nalus(data);
        let mut vcl_data = Vec::new();
        let mut has_vcl = false;
        let mut has_irap = false;
        for nal in nalus {
            if nal.len() < 2 {
                continue;
            }
            let nal_type = (nal[0] >> 1) & 0x3f;
            match nal_type {
                32 => self.vps = Some(nal.to_vec()),
                33 => self.sps = Some(nal.to_vec()),
                34 => self.pps = Some(nal.to_vec()),
                0..=31 => {
                    has_vcl = true;
                    has_irap |= (16..=23).contains(&nal_type);
                    vcl_data.extend_from_slice(&[0, 0, 0, 1]);
                    vcl_data.extend_from_slice(nal);
                }
                _ => {}
            }
        }
        if !has_vcl {
            return Vec::new();
        }

        let mut payloads = Vec::new();
        if (keyframe || has_irap)
            && let (Some(vps), Some(sps), Some(pps)) = (
                self.vps.as_deref(),
                self.sps.as_deref(),
                self.pps.as_deref(),
            )
            && let Some(parameter_payload) = aggregate_h265(vps, sps, pps)
            && parameter_payload.len() <= RTP_PAYLOAD_MTU
        {
            payloads.push(Bytes::from(parameter_payload));
        }

        payloads.extend(fragment_h265_vcl(&vcl_data));
        payloads
    }

    fn timestamp(&mut self, source_us: Option<u64>) -> u32 {
        timestamp_from_source(
            &mut self.timestamp_value,
            &mut self.timestamp_base,
            &mut self.source_base_us,
            &mut self.last_source_us,
            source_us,
        )
    }
}

fn timestamp_from_source(
    current: &mut u32,
    base_rtp: &mut u32,
    source_base_us: &mut Option<u64>,
    last_source_us: &mut Option<u64>,
    source_us: Option<u64>,
) -> u32 {
    let Some(source_us) = source_us else {
        let timestamp = *current;
        *current = current.wrapping_add(RTP_VIDEO_TIMESTAMP_INCREMENT);
        return timestamp;
    };

    let previous = *last_source_us;
    let monotonic_source = previous
        .map(|last| source_us.max(last))
        .unwrap_or(source_us);
    *last_source_us = Some(monotonic_source);
    let base = *source_base_us.get_or_insert(monotonic_source);
    let elapsed = monotonic_source.saturating_sub(base);
    let ticks = elapsed / 100 * 9 + (elapsed % 100) * 9 / 100;
    let mut timestamp = base_rtp.wrapping_add(ticks as u32);
    if previous.is_some() && timestamp <= *current {
        timestamp = current.wrapping_add(1);
    }
    *current = timestamp;
    timestamp
}

fn aggregate_h264(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    let sps_len = u16::try_from(sps.len()).ok()?;
    let pps_len = u16::try_from(pps.len()).ok()?;
    let mut payload = Vec::with_capacity(1 + 2 + sps.len() + 2 + pps.len());
    payload.push((sps.first().copied().unwrap_or(0) & 0x60) | 24);
    payload.extend_from_slice(&sps_len.to_be_bytes());
    payload.extend_from_slice(sps);
    payload.extend_from_slice(&pps_len.to_be_bytes());
    payload.extend_from_slice(pps);
    Some(payload)
}

fn fragment_h265_vcl(data: &[u8]) -> Vec<Bytes> {
    let mut payloads = Vec::new();
    for nal in annex_b_nalus(data) {
        if nal.len() < 2 {
            continue;
        }
        if nal.len() <= RTP_PAYLOAD_MTU {
            payloads.push(Bytes::copy_from_slice(nal));
            continue;
        }

        let nal_type = (nal[0] >> 1) & 0x3f;
        let max_fragment_len = RTP_PAYLOAD_MTU.saturating_sub(3);
        if max_fragment_len == 0 {
            continue;
        }
        let mut offset = 2;
        while offset < nal.len() {
            let end = (offset + max_fragment_len).min(nal.len());
            let mut fragment = Vec::with_capacity(3 + end - offset);
            // RFC 7798 FU indicator: preserve F/layer-id high bit and TID,
            // replace the NAL type with 49 (fragmentation unit).
            fragment.push((nal[0] & 0x81) | (49 << 1));
            fragment.push(nal[1]);
            let mut fu_header = nal_type;
            if offset == 2 {
                fu_header |= 0x80;
            }
            if end == nal.len() {
                fu_header |= 0x40;
            }
            fragment.push(fu_header);
            fragment.extend_from_slice(&nal[offset..end]);
            payloads.push(Bytes::from(fragment));
            offset = end;
        }
    }
    payloads
}

fn aggregate_h265(vps: &[u8], sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    let mut payload = Vec::with_capacity(2 + 2 + vps.len() + 2 + sps.len() + 2 + pps.len());
    payload.extend_from_slice(&[0x60, 0x01]);
    for nal in [vps, sps, pps] {
        let length = u16::try_from(nal.len()).ok()?;
        payload.extend_from_slice(&length.to_be_bytes());
        payload.extend_from_slice(nal);
    }
    Some(payload)
}

fn rtp_packet(
    payload_type: u8,
    marker: bool,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut packet = Vec::with_capacity(12 + payload.len());
    packet.extend_from_slice(&[
        0x80,
        (if marker { 0x80 } else { 0 }) | (payload_type & 0x7f),
    ]);
    packet.extend_from_slice(&sequence.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn h264_parameter_frame() -> Vec<u8> {
        [
            vec![0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f],
            vec![0, 0, 1, 0x68, 0xce, 0x06, 0xe2],
            vec![0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00],
        ]
        .concat()
    }

    fn h265_parameter_frame() -> Vec<u8> {
        [
            vec![0, 0, 0, 1, 0x40, 0x01, 0xaa],
            vec![0, 0, 1, 0x42, 0x01, 0xbb],
            vec![0, 0, 0, 1, 0x44, 0x01, 0xcc],
            vec![0, 0, 0, 1, 0x26, 0x01, 0xdd],
        ]
        .concat()
    }

    #[test]
    fn parses_mixed_annex_b_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 4];
        let nalus = annex_b_nalus(&data);
        assert_eq!(nalus, [vec![0x67, 1, 2].as_slice(), &[0x68, 3, 4]]);
    }

    #[test]
    fn h264_packetizer_emits_parameter_set_and_fragmented_rtp() {
        let mut packetizer = H264Packetizer::new(0x1234_5678);
        let mut frame = h264_parameter_frame();
        frame.extend_from_slice(&[0; 3_000]);
        let packets = packetizer.payloads(&frame, true);

        assert!(packets.len() > 2);
        assert_eq!(packets[0][0] & 0x1f, 24);
        assert_eq!(packets[1][0] & 0x1f, 28);
    }

    #[test]
    fn h265_packetizer_keeps_vcl_after_parameter_aggregation() {
        let mut packetizer = H265Packetizer::new(0x1234_5678);
        let packets = packetizer.payloads(&h265_parameter_frame(), true);

        assert_eq!((packets[0][0] >> 1) & 0x3f, 48);
        assert!(packets.len() >= 2);
        assert_eq!((packets[1][0] >> 1) & 0x3f, 19);
    }

    #[test]
    fn sdp_includes_codec_parameter_sets() {
        let config = RtspServerConfig::new(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            DEFAULT_RTSP_PORT,
        )));
        let state = CodecState {
            codec: VideoCodec::H264,
            sps: Some(vec![0x67, 0x42, 0x00, 0x1f]),
            pps: Some(vec![0x68, 0xce, 0x06, 0xe2]),
            ..CodecState::default()
        };
        let sdp = build_sdp(&state, config.video_width, config.video_height)
            .expect("complete parameter sets should produce SDP");

        assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"));
        assert!(sdp.contains("a=rtpmap:96 H264/90000\r\n"));
        assert!(sdp.contains("sprop-parameter-sets="));
        assert!(sdp.contains("a=control:trackID=0\r\n"));
    }

    #[tokio::test]
    async fn rtsp_control_handshake_uses_interleaved_tcp() {
        let config =
            RtspServerConfig::new(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)));
        let server = RtspServer::start(config).expect("RTSP server should start");
        server
            .push_video_frame(
                &h264_parameter_frame(),
                true,
                VideoCodec::H264,
                Some(1_000_000),
            )
            .expect("video frame should publish");

        let mut socket = tokio::net::TcpStream::connect(server.local_addr())
            .await
            .expect("client should connect");
        socket
            .write_all(b"OPTIONS rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .expect("OPTIONS should write");
        let response = read_response(&mut socket).await;
        assert!(response.starts_with("RTSP/1.0 200 OK"));
        assert!(response.contains("Public: OPTIONS"));

        socket
            .write_all(b"DESCRIBE rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 2\r\n\r\n")
            .await
            .expect("DESCRIBE should write");
        let response = read_response(&mut socket).await;
        assert!(response.starts_with("RTSP/1.0 200 OK"));
        assert!(response.contains("a=control:trackID=0"));

        socket
            .write_all(b"SETUP rtsp://127.0.0.1/live/trackID=0 RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;unicast;interleaved=4-5\r\n\r\n")
            .await
            .expect("SETUP should write");
        let response = read_response(&mut socket).await;
        assert!(response.starts_with("RTSP/1.0 200 OK"));
        assert!(response.contains("interleaved=4-5"));
        let session = response
            .lines()
            .find_map(|line| line.strip_prefix("Session: "))
            .expect("SETUP should return a session")
            .to_string();

        let play =
            format!("PLAY rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 4\r\nSession: {session}\r\n\r\n");
        socket
            .write_all(play.as_bytes())
            .await
            .expect("PLAY should write");
        let response = read_response(&mut socket).await;
        assert!(response.starts_with("RTSP/1.0 200 OK"));
        server
            .push_video_frame(
                &h264_parameter_frame(),
                true,
                VideoCodec::H264,
                Some(1_066_000),
            )
            .expect("video frame should publish after PLAY");

        let mut interleaved_header = [0_u8; 4];
        socket
            .read_exact(&mut interleaved_header)
            .await
            .expect("PLAY should receive RTP");
        assert_eq!(interleaved_header[0], b'$');
        assert_eq!(interleaved_header[1], 4);
    }

    async fn read_response(socket: &mut tokio::net::TcpStream) -> String {
        let mut data = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            socket.read_exact(&mut byte).await.expect("response byte");
            data.push(byte[0]);
            if data.ends_with(b"\r\n\r\n") {
                let header = String::from_utf8(data.clone()).expect("RTSP response headers");
                let length = header
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                if length > 0 {
                    let mut body = vec![0_u8; length];
                    socket.read_exact(&mut body).await.expect("response body");
                    data.extend_from_slice(&body);
                }
                return String::from_utf8(data).expect("RTSP response");
            }
            assert!(data.len() < MAX_REQUEST_BUFFER);
        }
    }
}
