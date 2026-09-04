//! Camera-side media engine for the Eufy C220.
//!
//! This crate owns the Rockchip sensor/ISP/VENC/AI/AO hardware in one process,
//! replacing `native/c220-media-engine.c`, and speaks the `C220` record protocol
//! over a private Unix socket to the Rust firmware service.
//!
//! # Platform constraints
//!
//! The engine links the camera's own uClibc runtime and the Rockchip libraries,
//! so it is built for `armv7-unknown-linux-musleabihf` with dynamic linking
//! against the board's uClibc:
//!
//! * The board's `ld-uClibc.so.0` faults in `_dl_get_ready_to_run` when the main
//!   executable carries a `PT_TLS` segment, so this crate must stay free of
//!   thread-local storage. That rules out `std`, whose runtime always emits
//!   thread-locals, and is why the crate is `no_std` with `alloc`.
//! * uClibc-ng lacks `__libc_start_main`, so binaries enter through
//!   `__uClibc_main` with the stub in the binary's entry module instead of
//!   musl's CRT objects.
//! * Structures crossing the libc boundary are defined in [`platform`] to the
//!   kernel's layout where the `libc` crate's musl definitions would differ.
//!
//! # Layout
//!
//! [`engine`] owns the private socket and the daemon session loop, [`commands`]
//! holds the daemon command rules, [`client`] is the record I/O around them, and
//! [`backend`] is the device side: the mock backend here, the vendor bindings
//! ([`vendor`], which exists only for the camera target) next. [`platform`] is
//! the small POSIX layer the rest is built on, and
//! [`cli`], [`keyframe`] and [`pts`] are the ported pieces of the reference
//! engine that need no hardware.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod backend;
pub mod cli;
pub mod client;
pub mod commands;
pub mod engine;
pub mod keyframe;
pub mod platform;
pub mod pts;
#[cfg(target_arch = "arm")]
pub mod vendor;

use crate::backend::Backend;
use crate::cli::EngineConfig;
use crate::engine::Engine;
use crate::platform::{Error, media_log};
use alloc::boxed::Box;

/// Run the engine until a stop signal arrives.
///
/// The engine, and therefore the shutdown state the signal handler points at, is
/// deliberately kept for the lifetime of the process: the handler must never see
/// freed memory, and this function is the last thing a normal run does.
pub fn run(config: EngineConfig, backend: &mut dyn Backend) -> Result<(), Error> {
    let engine: &'static mut Engine = Box::leak(Box::new(Engine::bind(&config)?));
    platform::signal::install(engine.shutdown())?;
    media_log!(
        "media_engine_ready=1 socket={} device={} codec=h265",
        config.socket_path,
        config.video_device
    );

    engine.serve(backend);
    backend.shutdown();
    engine.remove_socket_file();
    media_log!("media_engine_stopped=1");
    Ok(())
}
