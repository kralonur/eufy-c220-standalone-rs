//! Camera-local RTSP media service.
//!
//! The server accepts RTSP 1.0 control requests and sends the encoded Annex-B
//! video access units and optional PCMA microphone frames received from the
//! camera media engine as RTP interleaved over TCP. The RTP packetization
//! follows RFC 3551 (PCMA), RFC 6184 (H.264), and RFC 7798 (H.265).
//!
//! This module is part of the custom camera firmware. It has no Eufy/P2P
//! protocol handling. The 2K dimensions and the H.265 default are based on the
//! reverse-engineered and measured camera media path documented in
//! `docs/hardware-video.md` from the reference project.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use bytes::Bytes;
use rtp::codecs::h264::H264Payloader;
use rtp::packetizer::Payloader;
use rtsp_runtime::{InterleavedFrame, LowerTransport, ServerEvent, ServerSession, Transport};
use rtsp_types::{Message, Method, Request, Response, StatusCode, Version, headers};
use std::io;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify, broadcast};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::digest::{Algorithm, DigestAuth, Lockout, Outcome};
use crate::media::{MediaRecord, RecordKind};

/// A boxed TCP or TLS stream used by one RTSP session.
pub trait SessionTransport: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> SessionTransport for T {}
type Session = Box<dyn SessionTransport>;

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

/// Static RTP payload type for G.711 A-law audio (RFC 3551).
pub const RTP_AUDIO_PAYLOAD_TYPE: u8 = 8;

/// RTP clock rate for the validated microphone path.
pub const RTP_AUDIO_CLOCK_RATE: u32 = 8_000;

/// RTP timestamp increment for one 20 ms microphone frame.
const RTP_AUDIO_TIMESTAMP_INCREMENT: u32 = 160;

/// One native microphone record contains 20 ms of 8 kHz mono PCMA.
const AUDIO_FRAME_LEN: usize = 160;

/// Duration of one native microphone frame, in microseconds.
const AUDIO_FRAME_DURATION_US: u64 = 20_000;

/// Maximum audio payload accepted from the native companion.
const MAX_AUDIO_RECORD_LEN: usize = 4 * 1024;

/// Maximum RTP payload before the codec payloader fragments a NAL unit.
/// 1,200 bytes keeps interleaved RTP packets below the common Ethernet MTU.
const RTP_PAYLOAD_MTU: usize = 1_200;

/// Number of records retained for clients that are briefly slower than the
/// camera. A lagging client is resynchronized at the next keyframe.
const MEDIA_BUS_CAPACITY: usize = 256;

/// Maximum RTSP request/control buffer accepted from one client.
const MAX_REQUEST_BUFFER: usize = 64 * 1024;

/// Maximum PCMA payload accepted in one incoming RTP backchannel packet.
const MAX_TALKBACK_PCMA_PAYLOAD: usize = 4 * 1024;

/// Maximum complete RTP packet accepted from one RTSP client.
const MAX_TALKBACK_RTP_PACKET_LEN: usize = 8 * 1024;

/// Fixed RTP header size before CSRC, extension, and padding fields.
const RTP_HEADER_LEN: usize = 12;

/// Maximum time DESCRIBE waits for the encoder's parameter sets.
const CODEC_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval while waiting for parameter sets or an idle media source.
const CODEC_READY_POLL: Duration = Duration::from_millis(100);

/// H.264 profile-level-id used only if an unusually short SPS lacks the three
/// profile bytes. Normal camera SPS records always provide these bytes.
const DEFAULT_H264_PROFILE_LEVEL_ID: &str = "42e01f";

/// Initial deterministic RTP SSRC. Each client receives a distinct value.
const FIRST_RTP_SSRC: u32 = 0xc220_0001;

/// Methods implemented by the camera-local RTSP endpoint.
const SUPPORTED_METHODS: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, GET_PARAMETER, TEARDOWN";

/// Server name included in responses generated outside the session engine.
const RTSP_SERVER_NAME: &str = "c220-firmware";

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

/// Sink for incoming RTSP backchannel audio.
///
/// The server decodes RTP PCMA and expands it to mono 16 kHz signed PCM16
/// before calling this sink. Implementations must keep each method bounded;
/// RTSP session tasks call them from blocking workers.
pub trait TalkbackSink: Send + Sync {
    fn start_talkback(&self) -> io::Result<()>;
    fn send_pcm(&self, pcm_bytes: &[u8]) -> io::Result<()>;
    fn stop_talkback(&self) -> io::Result<()>;
}

#[derive(Clone, Default)]
struct TalkbackSinkSlot {
    sink: Arc<StdMutex<Option<Arc<dyn TalkbackSink>>>>,
}

impl TalkbackSinkSlot {
    fn set(&self, sink: Arc<dyn TalkbackSink>) -> io::Result<()> {
        let mut current = self
            .sink
            .lock()
            .map_err(|_| io::Error::other("RTSP talkback sink lock poisoned"))?;
        *current = Some(sink);
        Ok(())
    }

    fn clear(&self) -> io::Result<()> {
        let mut current = self
            .sink
            .lock()
            .map_err(|_| io::Error::other("RTSP talkback sink lock poisoned"))?;
        *current = None;
        Ok(())
    }

    fn current(&self) -> io::Result<Option<Arc<dyn TalkbackSink>>> {
        self.sink
            .lock()
            .map_err(|_| io::Error::other("RTSP talkback sink lock poisoned"))
            .map(|current| current.clone())
    }
}

/// RTSPS listener configuration. It uses the same RTSP session protocol over
/// a TLS stream and therefore also encrypts interleaved RTP media.
#[derive(Clone)]
pub struct RtspsConfig {
    pub port: u16,
    pub acceptor: TlsAcceptor,
}

impl std::fmt::Debug for RtspsConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RtspsConfig")
            .field("port", &self.port)
            .field("acceptor", &"[CONFIGURED]")
            .finish()
    }
}

/// RTSP listener configuration.
#[derive(Clone)]
pub struct RtspServerConfig {
    /// Local address on which the camera serves plaintext RTSP.
    pub bind_address: SocketAddr,
    /// Whether to bind the plaintext listener. A disabled listener keeps the
    /// address for diagnostics while allowing port 0 to remain an ephemeral
    /// port for library callers and tests.
    pub plaintext_enabled: bool,
    /// Resource path advertised by DESCRIBE.
    pub path: String,
    /// Encoded video width advertised in SDP.
    pub video_width: u32,
    /// Encoded video height advertised in SDP.
    pub video_height: u32,
    /// Whether the validated PCMA microphone track is advertised and served.
    pub audio_enabled: bool,
    /// Optional Digest credentials for the RTSP control channel.
    pub auth: Option<Arc<DigestAuth>>,
    /// Shared per-client failed-login lockout.
    pub lockout: Arc<Lockout>,
    /// Optional encrypted listener, normally on port 322.
    pub rtsps: Option<RtspsConfig>,
}

impl std::fmt::Debug for RtspServerConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RtspServerConfig")
            .field("bind_address", &self.bind_address)
            .field("plaintext_enabled", &self.plaintext_enabled)
            .field("path", &self.path)
            .field("video_width", &self.video_width)
            .field("video_height", &self.video_height)
            .field("audio_enabled", &self.audio_enabled)
            .field("auth", &self.auth.as_ref().map(|_| "[CONFIGURED]"))
            .field("lockout", &"[CONFIGURED]")
            .field("rtsps", &self.rtsps)
            .finish()
    }
}

impl RtspServerConfig {
    pub fn new(bind_address: SocketAddr) -> Self {
        Self {
            bind_address,
            plaintext_enabled: true,
            path: DEFAULT_RTSP_PATH.to_string(),
            video_width: DEFAULT_VIDEO_WIDTH,
            video_height: DEFAULT_VIDEO_HEIGHT,
            audio_enabled: false,
            auth: None,
            lockout: Arc::new(Lockout::new()),
            rtsps: None,
        }
    }
}

/// Counters for the camera-local RTSP service.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtspStats {
    pub video_access_units_published: u64,
    pub video_access_units_sent: u64,
    pub audio_frames_published: u64,
    pub audio_frames_sent: u64,
}

#[derive(Clone, Debug)]
struct VideoFrame {
    data: Arc<[u8]>,
    keyframe: bool,
    codec: VideoCodec,
    timestamp_us: Option<u64>,
}

#[derive(Clone, Debug)]
struct AudioFrame {
    data: Arc<[u8]>,
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
    audio_enabled: bool,
    auth: Option<Arc<DigestAuth>>,
    lockout: Arc<Lockout>,
    media_tx: broadcast::Sender<VideoFrame>,
    audio_tx: broadcast::Sender<AudioFrame>,
    talkback: TalkbackSinkSlot,
    codec: RwLock<CodecState>,
    codec_ready: Notify,
    shutdown: Notify,
    next_session: AtomicU64,
    next_ssrc: AtomicU32,
    video_access_units_published: AtomicU64,
    video_access_units_sent: AtomicU64,
    audio_frames_published: AtomicU64,
    audio_frames_sent: AtomicU64,
}

/// Multi-client camera-local RTSP server.
///
/// The server never reads the sensor itself. The media engine owns capture and
/// encoding; callers publish complete Annex-B access units with
/// [`RtspServer::push_video_frame`] and validated PCMA frames with
/// [`RtspServer::push_audio_frame`]. Each client receives independent RTP
/// sequence spaces and packetizers, so a slow client cannot stall another one.
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
        let (listener, local_addr) = if config.plaintext_enabled {
            let listener = StdTcpListener::bind(config.bind_address)?;
            listener.set_nonblocking(true)?;
            let local_addr = listener.local_addr()?;
            (Some(TcpListener::from_std(listener)?), local_addr)
        } else {
            (None, config.bind_address)
        };
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            io::Error::other(format!("RTSP server requires a Tokio runtime: {error}"))
        })?;

        let (media_tx, _) = broadcast::channel(MEDIA_BUS_CAPACITY);
        let (audio_tx, _) = broadcast::channel(MEDIA_BUS_CAPACITY);
        let inner = Arc::new(RtspServerInner {
            path,
            video_width: config.video_width,
            video_height: config.video_height,
            audio_enabled: config.audio_enabled,
            auth: config.auth,
            lockout: config.lockout,
            media_tx,
            audio_tx,
            talkback: TalkbackSinkSlot::default(),
            codec: RwLock::new(CodecState::default()),
            codec_ready: Notify::new(),
            shutdown: Notify::new(),
            next_session: AtomicU64::new(1),
            next_ssrc: AtomicU32::new(FIRST_RTP_SSRC),
            video_access_units_published: AtomicU64::new(0),
            video_access_units_sent: AtomicU64::new(0),
            audio_frames_published: AtomicU64::new(0),
            audio_frames_sent: AtomicU64::new(0),
        });
        let handles = Arc::new(AtomicUsize::new(1));
        if let Some(listener) = listener {
            let accept_inner = Arc::clone(&inner);
            runtime.spawn(async move { accept_loop(listener, accept_inner, None).await });
        }
        if let Some(rtsps) = config.rtsps {
            if rtsps.port == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "RTSPS port must be non-zero",
                ));
            }
            let rtsps_listener = StdTcpListener::bind(("0.0.0.0", rtsps.port))?;
            rtsps_listener.set_nonblocking(true)?;
            let rtsps_listener = TcpListener::from_std(rtsps_listener)?;
            let tls_inner = Arc::clone(&inner);
            runtime.spawn(async move {
                accept_loop(rtsps_listener, tls_inner, Some(rtsps.acceptor)).await
            });
        }

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

    /// Attach the camera speaker used by RTSP backchannel sessions.
    pub fn set_talkback_sink(&self, sink: Arc<dyn TalkbackSink>) -> io::Result<()> {
        self.inner.talkback.set(sink)
    }

    /// Detach the speaker before the media engine is shut down.
    pub fn clear_talkback_sink(&self) -> io::Result<()> {
        self.inner.talkback.clear()
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
        if record.kind != RecordKind::VIDEO {
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

    /// Publish one 20 ms block of G.711 A-law microphone samples.
    pub fn push_audio_frame(&self, pcma: &[u8], timestamp_us: Option<u64>) -> io::Result<()> {
        if !self.inner.audio_enabled {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RTSP audio track is disabled",
            ));
        }
        if pcma.len() != AUDIO_FRAME_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "PCMA frame has {} samples; expected {AUDIO_FRAME_LEN}",
                    pcma.len()
                ),
            ));
        }

        let frame = AudioFrame {
            data: Arc::<[u8]>::from(pcma.to_vec()),
            timestamp_us,
        };
        self.inner
            .audio_frames_published
            .fetch_add(1, Ordering::Relaxed);
        let _ = self.inner.audio_tx.send(frame);
        Ok(())
    }

    /// Publish one or more native PCMA frames from the camera-local media ABI.
    pub fn push_audio_record(&self, record: &MediaRecord) -> io::Result<()> {
        if record.kind != RecordKind::AUDIO_CAPTURE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "media record is not an audio-capture record",
            ));
        }
        if record.payload.is_empty()
            || record.payload.len() > MAX_AUDIO_RECORD_LEN
            || !record.payload.len().is_multiple_of(AUDIO_FRAME_LEN)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "audio record payload must contain 20 ms PCMA frames and be at most {MAX_AUDIO_RECORD_LEN} bytes"
                ),
            ));
        }

        for (index, frame) in record.payload.chunks(AUDIO_FRAME_LEN).enumerate() {
            let timestamp_us = record
                .timestamp_us
                .saturating_add((index as u64).saturating_mul(AUDIO_FRAME_DURATION_US));
            self.push_audio_frame(frame, Some(timestamp_us))?;
        }
        Ok(())
    }

    /// Return the current SDP once the encoder's parameter sets are known.
    pub fn sdp(&self) -> Option<String> {
        let codec = self.inner.codec.read().ok()?.clone();
        build_sdp(
            &codec,
            self.inner.video_width,
            self.inner.video_height,
            self.inner.audio_enabled,
            self.inner.talkback.current().ok().flatten().is_some(),
        )
    }

    pub fn stats(&self) -> RtspStats {
        RtspStats {
            video_access_units_published: self
                .inner
                .video_access_units_published
                .load(Ordering::Relaxed),
            video_access_units_sent: self.inner.video_access_units_sent.load(Ordering::Relaxed),
            audio_frames_published: self.inner.audio_frames_published.load(Ordering::Relaxed),
            audio_frames_sent: self.inner.audio_frames_sent.load(Ordering::Relaxed),
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

async fn accept_loop(listener: TcpListener, inner: Arc<RtspServerInner>, tls: Option<TlsAcceptor>) {
    loop {
        tokio::select! {
            _ = inner.shutdown.notified() => break,
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { break };
                if stream.set_nodelay(true).is_err() {
                    continue;
                }
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
                        tracing::debug!(%peer, %error, "RTSP client connection ended");
                    }
                });
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MediaTrack {
    Video,
    Audio,
    Backchannel,
}

struct ClientState {
    video_channel: Option<u8>,
    audio_channel: Option<u8>,
    backchannel_channel: Option<u8>,
    backchannel_sink: Option<Arc<dyn TalkbackSink>>,
    media_task: Option<JoinHandle<()>>,
    audio_task: Option<JoinHandle<()>>,
}

impl ClientState {
    async fn stop_media(&mut self) {
        if let Some(task) = self.media_task.take() {
            task.abort();
        }
        if let Some(task) = self.audio_task.take() {
            task.abort();
        }
        if let Some(sink) = self.backchannel_sink.take() {
            let _ = tokio::task::spawn_blocking(move || sink.stop_talkback()).await;
        }
    }
}

async fn handle_connection(
    stream: Session,
    peer: SocketAddr,
    inner: Arc<RtspServerInner>,
) -> io::Result<()> {
    let (mut reader, writer) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(writer));
    let session_seed = inner.next_session.fetch_add(1, Ordering::Relaxed);
    let mut session = ServerSession::new().with_session_seed(session_seed);
    let mut state = ClientState {
        video_channel: None,
        audio_channel: None,
        backchannel_channel: None,
        backchannel_sink: None,
        media_task: None,
        audio_task: None,
    };
    let mut buffer = Vec::new();

    loop {
        let message = tokio::select! {
            _ = inner.shutdown.notified() => break,
            result = read_request(&mut reader, &mut buffer) => result?,
        };
        let Some(message) = message else { break };
        match message {
            InboundMessage::Request(request) => {
                if handle_request(&inner, &writer, &mut session, &mut state, request, peer).await? {
                    break;
                }
            }
            InboundMessage::Interleaved(frame) => {
                handle_interleaved(&mut state, frame).await?;
            }
        }
    }

    state.stop_media().await;
    Ok(())
}

async fn handle_request(
    inner: &Arc<RtspServerInner>,
    writer: &Arc<Mutex<WriteHalf<Session>>>,
    session: &mut ServerSession,
    state: &mut ClientState,
    request: InboundRequest,
    peer: SocketAddr,
) -> io::Result<bool> {
    let method = request.parsed.method().clone();

    if let Some(auth) = inner.auth.as_ref() {
        if inner.lockout.is_locked(peer.ip()).await {
            send_error(writer, &request.parsed, StatusCode::Forbidden).await?;
            return Ok(false);
        }
        let uri = request
            .parsed
            .request_uri()
            .map(|value| value.as_str())
            .unwrap_or("");
        let authorization = authorization_header(&request.raw).unwrap_or_default();
        let outcome = auth.authorize(
            &method_name(&method),
            uri,
            &authorization,
            SystemTime::now(),
            peer.ip(),
        );
        if outcome != Outcome::Authorized {
            if outcome == Outcome::Denied && !authorization.is_empty() {
                inner.lockout.record_failure(peer.ip()).await;
            }
            send_unauthorized(writer, &request.parsed, auth, outcome == Outcome::Stale).await?;
            return Ok(false);
        }
        inner.lockout.record_success(peer.ip()).await;
    }

    if !method_supported(&method) {
        send_error(writer, &request.parsed, StatusCode::NotImplemented).await?;
        return Ok(false);
    }
    let talkback_enabled = inner.talkback.current().ok().flatten().is_some();
    if !request_path_allowed(
        &request.parsed,
        &method,
        &inner.path,
        inner.audio_enabled,
        talkback_enabled,
    ) {
        send_error(writer, &request.parsed, StatusCode::NotFound).await?;
        return Ok(false);
    }
    if session_required(&method, session) && !session_matches(session, &request.parsed) {
        send_error(writer, &request.parsed, StatusCode::SessionNotFound).await?;
        return Ok(false);
    }

    let setup = if method == Method::Setup {
        let Some(track) = setup_track(
            &request.parsed,
            &inner.path,
            inner.audio_enabled,
            talkback_enabled,
        ) else {
            send_error(writer, &request.parsed, StatusCode::NotFound).await?;
            return Ok(false);
        };
        match parse_setup_transport(&request.parsed) {
            Ok((channel, transport)) => Some((track, channel, transport)),
            Err(()) => {
                send_error(writer, &request.parsed, StatusCode::UnsupportedTransport).await?;
                return Ok(false);
            }
        }
    } else {
        None
    };

    let sdp = if method == Method::Describe {
        if !wait_for_codec(inner).await {
            send_error(writer, &request.parsed, StatusCode::ServiceUnavailable).await?;
            return Ok(false);
        }
        match build_sdp_from_inner(inner) {
            Some(sdp) => Some(sdp),
            None => {
                send_error(writer, &request.parsed, StatusCode::ServiceUnavailable).await?;
                return Ok(false);
            }
        }
    } else {
        None
    };

    let (response, events) = session
        .handle_request(&request.raw)
        .map_err(runtime_io_error)?;
    let accepted = events
        .iter()
        .any(|event| matches!(event, ServerEvent::RequestAccepted { .. }));

    if accepted && method == Method::Play {
        state.stop_media().await;
        if state.backchannel_channel.is_some() {
            let Some(sink) = inner.talkback.current().ok().flatten() else {
                send_error(writer, &request.parsed, StatusCode::ServiceUnavailable).await?;
                return Ok(false);
            };
            let start_sink = Arc::clone(&sink);
            let start_result = tokio::task::spawn_blocking(move || start_sink.start_talkback())
                .await
                .map_err(|error| {
                    io::Error::other(format!("RTSP talkback worker failed: {error}"))
                })?;
            if let Err(error) = start_result {
                tracing::warn!(%error, "RTSP talkback start failed");
                send_error(writer, &request.parsed, StatusCode::ServiceUnavailable).await?;
                return Ok(false);
            }
            state.backchannel_sink = Some(sink);
        }
    }

    let response = customize_response(
        response,
        &method,
        &request.parsed,
        sdp.as_deref(),
        setup.as_ref().map(|(_, _, transport)| transport.as_str()),
    )?;
    {
        let mut writer = writer.lock().await;
        writer.write_all(&response).await?;
    }

    if !accepted {
        return Ok(false);
    }

    match method {
        Method::Setup => {
            if let Some((track, channel, _)) = setup {
                match track {
                    MediaTrack::Video => state.video_channel = Some(channel),
                    MediaTrack::Audio => state.audio_channel = Some(channel),
                    MediaTrack::Backchannel => state.backchannel_channel = Some(channel),
                }
            }
        }
        Method::Play => {
            if let Some(channel) = state.video_channel {
                let receiver = inner.media_tx.subscribe();
                let media_inner = Arc::clone(inner);
                let media_writer = Arc::clone(writer);
                state.media_task = Some(tokio::spawn(async move {
                    media_loop(media_inner, media_writer, channel, receiver).await;
                }));
            }
            if let Some(channel) = state.audio_channel {
                let receiver = inner.audio_tx.subscribe();
                let media_inner = Arc::clone(inner);
                let media_writer = Arc::clone(writer);
                state.audio_task = Some(tokio::spawn(async move {
                    audio_media_loop(media_inner, media_writer, channel, receiver).await;
                }));
            }
        }
        Method::Pause => state.stop_media().await,
        Method::Teardown => {
            state.stop_media().await;
            state.video_channel = None;
            state.audio_channel = None;
            state.backchannel_channel = None;
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

async fn handle_interleaved(state: &mut ClientState, frame: InterleavedFrame) -> io::Result<()> {
    let Some(channel) = state.backchannel_channel else {
        return Ok(());
    };
    if frame.channel != channel {
        return Ok(());
    }
    let Some(sink) = state.backchannel_sink.clone() else {
        return Ok(());
    };
    let pcm = decode_pcma_rtp(&frame.payload)?;
    tokio::task::spawn_blocking(move || sink.send_pcm(&pcm))
        .await
        .map_err(|error| io::Error::other(format!("RTSP talkback worker failed: {error}")))?
}

/// Decode one RTP/PCMA packet to the 16 kHz PCM16 format expected by AO.
fn decode_pcma_rtp(packet: &[u8]) -> io::Result<Vec<u8>> {
    if packet.len() < RTP_HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RTSP talkback RTP packet is shorter than its header",
        ));
    }
    if packet.len() > MAX_TALKBACK_RTP_PACKET_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RTSP talkback RTP packet exceeds the configured limit",
        ));
    }
    if packet[0] >> 6 != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RTSP talkback RTP version is not 2",
        ));
    }
    if packet[1] & 0x7f != RTP_AUDIO_PAYLOAD_TYPE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RTSP talkback RTP payload is not PCMA",
        ));
    }

    let csrc_len = usize::from(packet[0] & 0x0f)
        .checked_mul(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid RTP CSRC count"))?;
    let mut payload_start = RTP_HEADER_LEN
        .checked_add(csrc_len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid RTP header length"))?;
    if payload_start > packet.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RTSP talkback RTP CSRC list is truncated",
        ));
    }

    if packet[0] & 0x10 != 0 {
        if payload_start + 4 > packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RTSP talkback RTP extension header is truncated",
            ));
        }
        let extension_words = usize::from(u16::from_be_bytes([
            packet[payload_start + 2],
            packet[payload_start + 3],
        ]));
        let extension_len = 4_usize
            .checked_add(extension_words.checked_mul(4).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid RTP extension length")
            })?)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid RTP extension length")
            })?;
        payload_start = payload_start.checked_add(extension_len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid RTP header length")
        })?;
        if payload_start > packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RTSP talkback RTP extension is truncated",
            ));
        }
    }

    let mut payload_end = packet.len();
    if packet[0] & 0x20 != 0 {
        let padding_len = usize::from(*packet.last().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "RTSP talkback RTP padding is empty",
            )
        })?);
        if padding_len == 0 || padding_len > payload_end.saturating_sub(payload_start) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "RTSP talkback RTP padding is invalid",
            ));
        }
        payload_end -= padding_len;
    }

    let pcma = &packet[payload_start..payload_end];
    if pcma.is_empty() || pcma.len() > MAX_TALKBACK_PCMA_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RTSP talkback PCMA payload has an invalid length",
        ));
    }

    let mut pcm = Vec::with_capacity(pcma.len().saturating_mul(4));
    for &sample in pcma {
        let sample = alaw_to_pcm(sample).to_le_bytes();
        // The native AO path is configured for 16 kHz input. Duplicate each
        // decoded 8 kHz sample rather than silently playing back at half speed.
        pcm.extend_from_slice(&sample);
        pcm.extend_from_slice(&sample);
    }
    Ok(pcm)
}

/// Decode one ITU-T G.711 A-law byte to signed linear PCM16.
pub fn alaw_to_pcm(sample: u8) -> i16 {
    let sample = sample ^ 0x55;
    let sign = sample & 0x80 != 0;
    let exponent = (sample >> 4) & 0x07;
    let mantissa = sample & 0x0f;
    let magnitude = if exponent == 0 {
        (i32::from(mantissa) << 4) + 8
    } else {
        ((i32::from(mantissa) << 4) + 0x108) << (exponent - 1)
    };
    if sign {
        magnitude as i16
    } else {
        -(magnitude as i16)
    }
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
    writer: Arc<Mutex<WriteHalf<Session>>>,
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

struct AudioPacketizer {
    sequence: u16,
    timestamp_value: u32,
    timestamp_base: u32,
    source_base_us: Option<u64>,
    last_source_us: Option<u64>,
    ssrc: u32,
}

impl AudioPacketizer {
    fn new(ssrc: u32) -> Self {
        Self {
            sequence: 0,
            timestamp_value: 0,
            timestamp_base: 0,
            source_base_us: None,
            last_source_us: None,
            ssrc,
        }
    }

    fn packetize(&mut self, frame: &AudioFrame) -> Vec<u8> {
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let timestamp = timestamp_from_source_at_rate(
            &mut self.timestamp_value,
            &mut self.timestamp_base,
            &mut self.source_base_us,
            &mut self.last_source_us,
            frame.timestamp_us,
            RTP_AUDIO_CLOCK_RATE,
            RTP_AUDIO_TIMESTAMP_INCREMENT,
        );
        rtp_packet(
            RTP_AUDIO_PAYLOAD_TYPE,
            false,
            sequence,
            timestamp,
            self.ssrc,
            &frame.data,
        )
    }
}

async fn audio_media_loop(
    inner: Arc<RtspServerInner>,
    writer: Arc<Mutex<WriteHalf<Session>>>,
    channel: u8,
    mut receiver: broadcast::Receiver<AudioFrame>,
) {
    let ssrc = inner.next_ssrc.fetch_add(1, Ordering::Relaxed);
    let mut packetizer = AudioPacketizer::new(ssrc);

    loop {
        let frame = tokio::select! {
            _ = inner.shutdown.notified() => break,
            received = receiver.recv() => match received {
                Ok(frame) => frame,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        };
        let packet = packetizer.packetize(&frame);
        if write_interleaved(&writer, channel, &packet).await.is_err() {
            break;
        }
        inner.audio_frames_sent.fetch_add(1, Ordering::Relaxed);
    }
}

async fn write_interleaved(
    writer: &Arc<Mutex<WriteHalf<Session>>>,
    channel: u8,
    payload: &[u8],
) -> io::Result<()> {
    let frame = InterleavedFrame::new(channel, payload.to_vec());
    let bytes = frame
        .to_bytes()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

    let mut writer = writer.lock().await;
    writer.write_all(&bytes).await
}

#[derive(Debug)]
enum InboundMessage {
    Request(InboundRequest),
    Interleaved(InterleavedFrame),
}

#[derive(Debug)]
struct InboundRequest {
    raw: Vec<u8>,
    parsed: Request<Vec<u8>>,
}

/// Recover the raw Authorization header before rtsp-types discards headers.
fn authorization_header(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("authorization")
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Return the exact RTSP method token used in the Digest calculation.
fn method_name(method: &Method) -> String {
    match method {
        Method::Describe => "DESCRIBE".to_string(),
        Method::GetParameter => "GET_PARAMETER".to_string(),
        Method::Options => "OPTIONS".to_string(),
        Method::Pause => "PAUSE".to_string(),
        Method::Play => "PLAY".to_string(),
        Method::PlayNotify => "PLAY_NOTIFY".to_string(),
        Method::Redirect => "REDIRECT".to_string(),
        Method::Setup => "SETUP".to_string(),
        Method::SetParameter => "SET_PARAMETER".to_string(),
        Method::Announce => "ANNOUNCE".to_string(),
        Method::Record => "RECORD".to_string(),
        Method::Teardown => "TEARDOWN".to_string(),
        Method::Extension(name) => name.clone(),
    }
}

async fn read_request(
    reader: &mut ReadHalf<Session>,
    buffer: &mut Vec<u8>,
) -> io::Result<Option<InboundMessage>> {
    loop {
        if buffer.first() == Some(&b'$') {
            match InterleavedFrame::parse(buffer) {
                Ok(Some((frame, consumed))) => {
                    buffer.drain(..consumed);
                    return Ok(Some(InboundMessage::Interleaved(frame)));
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        error.to_string(),
                    ));
                }
            }
        } else if !buffer.is_empty() {
            match Message::<Vec<u8>>::parse(buffer) {
                Ok((Message::Request(parsed), consumed)) => {
                    let raw = buffer.drain(..consumed).collect();
                    return Ok(Some(InboundMessage::Request(InboundRequest {
                        raw,
                        parsed,
                    })));
                }
                Ok((_, _)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "RTSP control stream contained a non-request message",
                    ));
                }
                Err(rtsp_types::ParseError::Incomplete(_)) => {}
                Err(rtsp_types::ParseError::Error) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "malformed RTSP request",
                    ));
                }
            }
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

fn request_path(request: &Request<Vec<u8>>) -> Option<&str> {
    request.request_uri().map(|uri| uri_path(uri.as_str()))
}

fn setup_track(
    request: &Request<Vec<u8>>,
    path: &str,
    audio_enabled: bool,
    talkback_enabled: bool,
) -> Option<MediaTrack> {
    let value = request_path(request)?;
    if value == format!("{path}/trackID=0") {
        Some(MediaTrack::Video)
    } else if audio_enabled && value == format!("{path}/trackID=1") {
        Some(MediaTrack::Audio)
    } else if talkback_enabled && value == format!("{path}/backchannel") {
        Some(MediaTrack::Backchannel)
    } else {
        None
    }
}

fn request_path_allowed(
    request: &Request<Vec<u8>>,
    method: &Method,
    path: &str,
    audio_enabled: bool,
    talkback_enabled: bool,
) -> bool {
    match method {
        Method::Describe => request_path(request) == Some(path),
        Method::Setup => setup_track(request, path, audio_enabled, talkback_enabled).is_some(),
        _ => true,
    }
}

fn method_supported(method: &Method) -> bool {
    matches!(
        method,
        Method::Options
            | Method::Describe
            | Method::Setup
            | Method::Play
            | Method::Pause
            | Method::GetParameter
            | Method::SetParameter
            | Method::Teardown
    )
}

fn session_matches(session: &ServerSession, request: &Request<Vec<u8>>) -> bool {
    let Some(expected) = session.session_id() else {
        return false;
    };
    request
        .header(&headers::SESSION)
        .map(|value| value.as_str().split(';').next().unwrap_or_default().trim())
        == Some(expected)
}

fn session_required(method: &Method, session: &ServerSession) -> bool {
    match method {
        Method::Play | Method::Pause => true,
        Method::Setup | Method::GetParameter | Method::SetParameter | Method::Teardown => {
            session.session_id().is_some()
        }
        _ => false,
    }
}

fn parse_setup_transport(request: &Request<Vec<u8>>) -> Result<(u8, String), ()> {
    let value = request.header(&headers::TRANSPORT).ok_or(())?.as_str();
    let transport = Transport::parse(value).map_err(|_| ())?;
    let spec = transport.first().ok_or(())?;
    if spec.lower_transport != Some(LowerTransport::Tcp) {
        return Err(());
    }
    let (rtp_channel, rtcp_channel) = spec.interleaved.ok_or(())?;
    if rtp_channel.checked_add(1) != Some(rtcp_channel) {
        return Err(());
    }

    Ok((
        rtp_channel,
        format!("RTP/AVP/TCP;unicast;interleaved={rtp_channel}-{rtcp_channel}"),
    ))
}

fn runtime_io_error(error: rtsp_runtime::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn serialize_message(message: &Message<Vec<u8>>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    message
        .write(&mut bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    Ok(bytes)
}

fn customize_response(
    bytes: Vec<u8>,
    method: &Method,
    request: &Request<Vec<u8>>,
    sdp: Option<&str>,
    setup_transport: Option<&str>,
) -> io::Result<Vec<u8>> {
    let (message, consumed) = Message::<Vec<u8>>::parse(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("RTSP session returned an invalid response: {error}"),
        )
    })?;
    if consumed != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RTSP session returned trailing response bytes",
        ));
    }
    let Message::Response(mut response) = message else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RTSP session did not return a response",
        ));
    };

    response.insert_header(headers::SERVER, RTSP_SERVER_NAME);
    if response.status() == StatusCode::Ok {
        response.set_reason_phrase("OK");
    }
    match method {
        Method::Options => {
            response.insert_header(headers::PUBLIC, SUPPORTED_METHODS);
        }
        Method::Describe => {
            let sdp = sdp.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "DESCRIBE response has no SDP")
            })?;
            let uri = request.request_uri().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "DESCRIBE request has no URI")
            })?;
            response.insert_header(headers::CONTENT_BASE, content_base(uri));
            response.insert_header(headers::CONTENT_TYPE, "application/sdp");
            response = response.replace_body(sdp.as_bytes().to_vec());
        }
        Method::Setup => {
            let transport = setup_transport.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SETUP response has no transport",
                )
            })?;
            response.insert_header(headers::TRANSPORT, transport);
        }
        _ => {}
    }

    serialize_message(&Message::from(response))
}

async fn send_error(
    writer: &Arc<Mutex<WriteHalf<Session>>>,
    request: &Request<Vec<u8>>,
    status: StatusCode,
) -> io::Result<()> {
    let cseq = request
        .header(&headers::CSEQ)
        .map(|value| value.as_str())
        .unwrap_or("0");
    let response = Response::builder(Version::V1_0, status)
        .header(headers::CSEQ, cseq)
        .header(headers::SERVER, RTSP_SERVER_NAME)
        .build(Vec::<u8>::new());
    let bytes = serialize_message(&Message::from(response))?;
    let mut writer = writer.lock().await;
    writer.write_all(&bytes).await
}

async fn send_unauthorized(
    writer: &Arc<Mutex<WriteHalf<Session>>>,
    request: &Request<Vec<u8>>,
    auth: &DigestAuth,
    stale: bool,
) -> io::Result<()> {
    let cseq = request
        .header(&headers::CSEQ)
        .map(|value| value.as_str())
        .unwrap_or("0");
    let challenge = auth.challenge(SystemTime::now(), Algorithm::Md5);
    let challenge = if stale {
        format!("{challenge}, stale=true")
    } else {
        challenge
    };
    let response = Response::builder(Version::V1_0, StatusCode::Unauthorized)
        .header(headers::CSEQ, cseq)
        .header(headers::SERVER, RTSP_SERVER_NAME)
        .header(headers::WWW_AUTHENTICATE, challenge)
        .build(Vec::<u8>::new());
    let bytes = serialize_message(&Message::from(response))?;
    let mut writer = writer.lock().await;
    writer.write_all(&bytes).await
}

fn content_base(uri: &rtsp_types::Url) -> String {
    if uri.as_str().ends_with('/') {
        uri.as_str().to_string()
    } else {
        format!("{}/", uri.as_str())
    }
}

fn build_sdp_from_inner(inner: &RtspServerInner) -> Option<String> {
    let codec = inner.codec.read().ok()?.clone();
    build_sdp(
        &codec,
        inner.video_width,
        inner.video_height,
        inner.audio_enabled,
        inner.talkback.current().ok().flatten().is_some(),
    )
}

fn build_sdp(
    codec: &CodecState,
    width: u32,
    height: u32,
    audio_enabled: bool,
    talkback_enabled: bool,
) -> Option<String> {
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
    // No direction attribute on the tracks the camera sends. RFC 4566 would accept
    // `a=sendonly` here, but the RTSP clients in practice read `sendonly` as "this
    // media is a backchannel I send on" and skip it: gortsplib decides with
    // `isBackChannel()` by looking for exactly that attribute, and go2rtc's client
    // behaves the same way. Marking forward tracks sendonly therefore made MediaMTX
    // pull only the backchannel, and go2rtc negotiate audio with no video at all.
    // Omitting the attribute means sendrecv, which every client treats as readable.
    sdp.push_str("a=control:trackID=0\r\n");
    if audio_enabled {
        sdp.push_str(&format!(
            "m=audio 0 RTP/AVP {RTP_AUDIO_PAYLOAD_TYPE}\r\n\
             a=rtpmap:{RTP_AUDIO_PAYLOAD_TYPE} PCMA/{RTP_AUDIO_CLOCK_RATE}/1\r\n\
             a=ptime:20\r\n\
             a=control:trackID=1\r\n"
        ));
    }
    if talkback_enabled {
        // The talkback track is the one media that is marked `sendonly`, which is the
        // convention clients use to recognise a backchannel and leave it alone. This
        // is inverted from RFC 4566's server perspective, and it is what the field
        // expects: ONVIF cameras advertise their backchannel the same way.
        sdp.push_str(&format!(
            "m=audio 0 RTP/AVP {RTP_AUDIO_PAYLOAD_TYPE}\r\n\
             a=rtpmap:{RTP_AUDIO_PAYLOAD_TYPE} PCMA/{RTP_AUDIO_CLOCK_RATE}/1\r\n\
             a=ptime:20\r\n\
             a=control:backchannel\r\n\
             a=sendonly\r\n"
        ));
    }
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
    timestamp_from_source_at_rate(
        current,
        base_rtp,
        source_base_us,
        last_source_us,
        source_us,
        RTP_VIDEO_CLOCK_RATE,
        RTP_VIDEO_TIMESTAMP_INCREMENT,
    )
}

fn timestamp_from_source_at_rate(
    current: &mut u32,
    base_rtp: &mut u32,
    source_base_us: &mut Option<u64>,
    last_source_us: &mut Option<u64>,
    source_us: Option<u64>,
    clock_rate: u32,
    fallback_increment: u32,
) -> u32 {
    let Some(source_us) = source_us else {
        let timestamp = *current;
        *current = current.wrapping_add(fallback_increment);
        return timestamp;
    };

    let previous = *last_source_us;
    let monotonic_source = previous
        .map(|last| source_us.max(last))
        .unwrap_or(source_us);
    *last_source_us = Some(monotonic_source);
    let base = *source_base_us.get_or_insert(monotonic_source);
    let elapsed = monotonic_source.saturating_sub(base);
    let seconds = elapsed / 1_000_000;
    let remainder = elapsed % 1_000_000;
    let ticks = seconds
        .saturating_mul(u64::from(clock_rate))
        .saturating_add(remainder.saturating_mul(u64::from(clock_rate)) / 1_000_000);
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
    use std::sync::Mutex as TestMutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

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

    struct TestTalkbackSink {
        starts: AtomicUsize,
        pcm: TestMutex<Vec<u8>>,
        ready: Arc<tokio::sync::Notify>,
    }

    impl Default for TestTalkbackSink {
        fn default() -> Self {
            Self {
                starts: AtomicUsize::new(0),
                pcm: TestMutex::new(Vec::new()),
                ready: Arc::new(tokio::sync::Notify::new()),
            }
        }
    }

    impl TalkbackSink for TestTalkbackSink {
        fn start_talkback(&self) -> io::Result<()> {
            self.starts.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(())
        }

        fn send_pcm(&self, pcm_bytes: &[u8]) -> io::Result<()> {
            self.pcm
                .lock()
                .expect("test sink lock")
                .extend_from_slice(pcm_bytes);
            self.ready.notify_one();
            Ok(())
        }

        fn stop_talkback(&self) -> io::Result<()> {
            Ok(())
        }
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
    fn pcma_decoder_expands_to_16khz_pcm16() {
        let packet = rtp_packet(RTP_AUDIO_PAYLOAD_TYPE, false, 1, 0, 0x1234_5678, &[0, 128]);
        let pcm = decode_pcma_rtp(&packet).expect("valid PCMA RTP should decode");

        assert_eq!(alaw_to_pcm(0), -5504);
        assert_eq!(alaw_to_pcm(128), 5504);
        let expected = [-5504_i16, -5504, 5504, 5504]
            .into_iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(pcm, expected);
    }

    #[test]
    fn pcma_packetizer_uses_8khz_rtp_timestamps() {
        let mut packetizer = AudioPacketizer::new(0x1234_5678);
        let first = AudioFrame {
            data: Arc::<[u8]>::from(vec![0xaa; AUDIO_FRAME_LEN]),
            timestamp_us: Some(1_000_000),
        };
        let second = AudioFrame {
            data: Arc::<[u8]>::from(vec![0xbb; AUDIO_FRAME_LEN]),
            timestamp_us: Some(1_020_000),
        };

        let first_packet = packetizer.packetize(&first);
        let second_packet = packetizer.packetize(&second);

        assert_eq!(first_packet.len(), 12 + AUDIO_FRAME_LEN);
        assert_eq!(first_packet[1], RTP_AUDIO_PAYLOAD_TYPE);
        assert_eq!(u16::from_be_bytes([first_packet[2], first_packet[3]]), 0);
        assert_eq!(
            u32::from_be_bytes(first_packet[4..8].try_into().unwrap()),
            0
        );
        assert_eq!(
            u32::from_be_bytes(second_packet[4..8].try_into().unwrap()),
            160
        );
        assert_eq!(&first_packet[12..], &[0xaa; AUDIO_FRAME_LEN]);
    }

    #[test]
    fn sdp_includes_codec_parameter_sets() {
        let mut config = RtspServerConfig::new(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            DEFAULT_RTSP_PORT,
        )));
        config.audio_enabled = true;
        let state = CodecState {
            codec: VideoCodec::H264,
            sps: Some(vec![0x67, 0x42, 0x00, 0x1f]),
            pps: Some(vec![0x68, 0xce, 0x06, 0xe2]),
            ..CodecState::default()
        };
        let sdp = build_sdp(
            &state,
            config.video_width,
            config.video_height,
            config.audio_enabled,
            true,
        )
        .expect("complete parameter sets should produce SDP");

        assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"));
        assert!(sdp.contains("a=rtpmap:96 H264/90000\r\n"));
        assert!(sdp.contains("sprop-parameter-sets="));
        assert!(sdp.contains("a=control:trackID=0\r\n"));
        assert!(sdp.contains("a=rtpmap:8 PCMA/8000/1\r\n"));
        assert!(sdp.contains("a=control:trackID=1\r\n"));
        assert!(sdp.contains("a=control:backchannel\r\n"));
        // Forward tracks must not carry a direction attribute: clients read
        // `sendonly` as "backchannel" and would skip the video entirely.
        assert!(!sdp.contains("a=sendonly\r\ntrackID") && sdp.contains("a=control:trackID=0\r\n"));
        assert_eq!(
            sdp.matches("a=sendonly\r\n").count(),
            1,
            "only the talkback track"
        );
        assert!(
            !sdp.contains("a=recvonly"),
            "no forward track may look like a backchannel"
        );
    }

    #[tokio::test]
    async fn rtsp_audio_setup_and_play_uses_negotiated_channel() {
        let mut config =
            RtspServerConfig::new(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)));
        config.audio_enabled = true;
        let server = RtspServer::start(config).expect("RTSP server should start");
        server
            .push_video_frame(
                &h264_parameter_frame(),
                true,
                VideoCodec::H264,
                Some(1_000_000),
            )
            .expect("video parameters should publish");

        let mut socket = tokio::net::TcpStream::connect(server.local_addr())
            .await
            .expect("client should connect");
        socket
            .write_all(b"DESCRIBE rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .expect("DESCRIBE should write");
        assert!(
            read_response(&mut socket)
                .await
                .starts_with("RTSP/1.0 200 OK")
        );

        socket
            .write_all(b"SETUP rtsp://127.0.0.1/live/trackID=1 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;unicast;interleaved=6-7\r\n\r\n")
            .await
            .expect("audio SETUP should write");
        let response = read_response(&mut socket).await;
        assert!(response.starts_with("RTSP/1.0 200 OK"));
        let session = response
            .lines()
            .find_map(|line| line.strip_prefix("Session: "))
            .expect("audio SETUP should return a session")
            .to_string();

        let play =
            format!("PLAY rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n");
        socket
            .write_all(play.as_bytes())
            .await
            .expect("PLAY should write");
        assert!(
            read_response(&mut socket)
                .await
                .starts_with("RTSP/1.0 200 OK")
        );
        tokio::time::sleep(Duration::from_millis(10)).await;

        server
            .push_audio_frame(&[0x55; AUDIO_FRAME_LEN], Some(1_000_000))
            .expect("audio frame should publish after PLAY");
        let mut interleaved_header = [0_u8; 4];
        socket
            .read_exact(&mut interleaved_header)
            .await
            .expect("PLAY should receive audio RTP");
        assert_eq!(interleaved_header[0], b'$');
        assert_eq!(interleaved_header[1], 6);
        let payload_len =
            u16::from_be_bytes([interleaved_header[2], interleaved_header[3]]) as usize;
        assert_eq!(payload_len, 12 + AUDIO_FRAME_LEN);
        let mut packet = vec![0_u8; payload_len];
        socket
            .read_exact(&mut packet)
            .await
            .expect("audio RTP packet should be complete");
        assert_eq!(packet[1], RTP_AUDIO_PAYLOAD_TYPE);
        assert_eq!(&packet[12..], &[0x55; AUDIO_FRAME_LEN]);
    }

    #[tokio::test]
    async fn rtsp_backchannel_decodes_and_forwards_pcma() {
        let config =
            RtspServerConfig::new(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)));
        let server = RtspServer::start(config).expect("RTSP server should start");
        let sink = Arc::new(TestTalkbackSink::default());
        server
            .set_talkback_sink(sink.clone())
            .expect("talkback sink should attach");
        server
            .push_video_frame(
                &h264_parameter_frame(),
                true,
                VideoCodec::H264,
                Some(1_000_000),
            )
            .expect("video parameters should publish");

        let mut socket = tokio::net::TcpStream::connect(server.local_addr())
            .await
            .expect("client should connect");
        socket
            .write_all(b"DESCRIBE rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .expect("DESCRIBE should write");
        let description = read_response(&mut socket).await;
        assert!(description.contains("a=control:backchannel\r\n"));

        socket
            .write_all(b"SETUP rtsp://127.0.0.1/live/backchannel RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n\r\n")
            .await
            .expect("backchannel SETUP should write");
        let response = read_response(&mut socket).await;
        assert!(response.starts_with("RTSP/1.0 200 OK"));
        let session = response
            .lines()
            .find_map(|line| line.strip_prefix("Session: "))
            .expect("backchannel SETUP should return a session")
            .to_string();

        let play =
            format!("PLAY rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n");
        socket
            .write_all(play.as_bytes())
            .await
            .expect("PLAY should write");
        assert!(
            read_response(&mut socket)
                .await
                .starts_with("RTSP/1.0 200 OK")
        );
        assert_eq!(sink.starts.load(AtomicOrdering::Relaxed), 1);

        let packet = rtp_packet(RTP_AUDIO_PAYLOAD_TYPE, false, 1, 0, 0x1234_5678, &[0, 128]);
        let mut interleaved = vec![b'$', 2];
        interleaved.extend_from_slice(&(packet.len() as u16).to_be_bytes());
        interleaved.extend_from_slice(&packet);
        socket
            .write_all(&interleaved)
            .await
            .expect("backchannel RTP should write");
        tokio::time::timeout(Duration::from_secs(1), sink.ready.notified())
            .await
            .expect("backchannel PCM should reach the sink");

        let pcm = sink.pcm.lock().expect("test sink lock").clone();
        let expected = [-5504_i16, -5504, 5504, 5504]
            .into_iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(pcm, expected);
    }

    #[tokio::test]
    async fn authenticated_rtsp_challenges_anonymous_clients() {
        let mut config =
            RtspServerConfig::new(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)));
        config.auth = Some(Arc::new(
            DigestAuth::new("camera", "test-password").expect("test entropy should be available"),
        ));
        let server = RtspServer::start(config).expect("RTSP server should start");
        let mut socket = tokio::net::TcpStream::connect(server.local_addr())
            .await
            .expect("client should connect");
        socket
            .write_all(b"OPTIONS rtsp://127.0.0.1/live RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .expect("OPTIONS should write");
        let response = read_response(&mut socket).await;
        assert!(response.starts_with("RTSP/1.0 401 Unauthorized"));
        assert!(response.contains("WWW-Authenticate: Digest"));
        assert!(response.contains("algorithm=MD5"));
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
