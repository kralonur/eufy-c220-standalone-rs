//! The engine process: one private socket, one daemon connection at a time.

use crate::backend::Backend;
use crate::cli::EngineConfig;
use crate::client::{self, HeaderRead, Outcome, RecordSink};
use crate::commands;
use crate::platform::net::{UnixListener, UnixStream};
use crate::platform::signal::Shutdown;
use crate::platform::{self, Error, media_log};
use alloc::ffi::CString;
use alloc::sync::Arc;
use alloc::vec;

/// Retry delay after a failed accept, matching the reference engine.
const ACCEPT_RETRY_US: u32 = 10_000;

/// Receive timeout for the daemon connection, matching the reference engine.
const RECEIVE_TIMEOUT_US: i32 = 1_000_000;

/// Owns the private socket and serves the daemon on it.
pub struct Engine {
    shutdown: Arc<Shutdown>,
    sink: Arc<RecordSink>,
    listener: UnixListener,
    socket_path: CString,
}

impl Engine {
    /// Bind the private socket and register it with the engine's shutdown state.
    ///
    /// The state is owned by the engine, so the socket a stop wakes and the flag
    /// a signal sets belong to the same value for the whole run.
    pub fn bind(config: &EngineConfig) -> Result<Self, Error> {
        let listener = UnixListener::bind(&config.socket_path)?;
        let socket_path = CString::new(config.socket_path.as_str())
            .map_err(|_| Error::from_errno(libc::EINVAL))?;
        let shutdown = Arc::new(Shutdown::new());
        shutdown.register_listener(listener.raw_fd());
        let sink = Arc::new(RecordSink::new()?);
        media_log!("media_socket_ready={}", config.socket_path);
        Ok(Self {
            shutdown,
            sink,
            listener,
            socket_path,
        })
    }

    /// The state a signal handler wakes and another thread can stop.
    pub fn shutdown(&self) -> &Shutdown {
        &self.shutdown
    }

    /// A handle that can stop the engine from another thread - the signal
    /// handler uses [`Engine::shutdown`] directly.
    pub fn stop_handle(&self) -> Arc<Shutdown> {
        Arc::clone(&self.shutdown)
    }

    /// Serve the daemon until a stop is requested.
    ///
    /// The backend is started first, so a device that fails to open is reported
    /// before any daemon connects - and the engine still serves, because the parts
    /// that did come up are worth talking to. Its producer threads share the
    /// engine's sink and shutdown state, so they reach the daemon without knowing
    /// which session is being served.
    ///
    /// One session at a time: while a daemon is connected this blocks in the
    /// session loop, and a new connection is only accepted after it ends. A stop
    /// request shuts the listening socket down, which is what makes the blocked
    /// `accept` return.
    pub fn serve(&mut self, backend: &mut dyn Backend) {
        if let Err(error) = backend.start(Arc::clone(&self.sink), Arc::clone(&self.shutdown)) {
            media_log!("media_backend_start_error={error}");
        }
        while !self.shutdown.requested() {
            let client = match self.listener.accept() {
                Ok(client) => client,
                Err(error) if error.is_interrupted() => continue,
                Err(error) => {
                    if self.shutdown.requested() {
                        break;
                    }
                    media_log!("media_accept_error={}", error);
                    platform::sleep_us(ACCEPT_RETRY_US);
                    continue;
                }
            };
            self.serve_client(&client, backend);
        }
    }

    /// Serve one connected daemon until it goes away or stops behaving.
    fn serve_client(&self, client: &UnixStream, backend: &mut dyn Backend) {
        self.shutdown.register_client(client.raw_fd());
        // Producers write through the sink, so the session tells it which socket
        // is current before anything is sent on it.
        if let Err(error) = self.sink.attach(client) {
            media_log!("media_client_attach_error={error}");
        }
        // Best effort, like the reference engine: a read that never times out is
        // still woken when the daemon closes the socket or a stop is requested.
        let _ = client.set_receive_timeout(RECEIVE_TIMEOUT_US);
        media_log!("media_client_connected=1");

        let stop = || self.shutdown.requested();
        let (night, manual) = backend.night_mode();
        if let Err(error) = self.sink.send_night_mode(night, manual) {
            media_log!("media_greeting_error={}", error);
        }

        loop {
            let header = match client::read_header(client, stop) {
                Ok(HeaderRead::Header(header)) => header,
                Ok(HeaderRead::Ended) => break,
                Ok(HeaderRead::Malformed) => {
                    media_log!("media_client_bad_magic=1");
                    break;
                }
                Err(error) => {
                    media_log!("media_client_read_error={}", error);
                    break;
                }
            };

            // The kind decides how large the payload may be, so an unreasonable
            // length is refused before anything is allocated for it.
            let limit = commands::payload_limit(header.kind(), header.keyframe());
            if header.payload_len() > limit {
                media_log!(
                    "media_client_payload_too_large={} kind={}",
                    header.payload_len(),
                    header.kind().as_u8()
                );
                break;
            }

            let mut payload = vec![0_u8; header.payload_len()];
            if !payload.is_empty() {
                match client::read_exact(client, &mut payload, stop) {
                    Ok(Outcome::Filled) => {}
                    Ok(Outcome::Ended) => break,
                    Err(error) => {
                        media_log!("media_client_read_error={}", error);
                        break;
                    }
                }
            }

            match commands::interpret(
                header.kind(),
                header.keyframe(),
                &payload,
                header.timestamp_us(),
            ) {
                Ok(Some(command)) => {
                    if let Err(error) = backend.command(command, &self.sink) {
                        media_log!("media_command_error={}", error);
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    media_log!("media_client_protocol_error=1");
                    break;
                }
            }
        }

        // The sink is detached before the socket closes, so a producer cannot
        // write to a descriptor that has already been reused.
        if let Err(error) = self.sink.detach() {
            media_log!("media_client_detach_error={error}");
        }
        self.shutdown.clear_client();
        media_log!("media_client_disconnected=1");
    }

    /// Remove the socket file, so a stopped engine leaves nothing behind.
    ///
    /// A normal run has to call this explicitly: the engine is kept for the
    /// lifetime of the process because the signal handler points at its shutdown
    /// state, so [`Drop`] does not run for it. The next engine would unlink the
    /// file before binding anyway; this keeps the filesystem honest.
    pub fn remove_socket_file(&self) {
        unsafe { libc::unlink(self.socket_path.as_ptr()) };
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.remove_socket_file();
    }
}
