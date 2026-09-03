//! Camera-side media engine for the Eufy C220.
//!
//! This crate will own the Rockchip sensor/ISP/VENC/AI/AO hardware in one
//! process, replacing `native/c220-media-engine.c`, and speaks the `C220`
//! record protocol over a private Unix socket to the Rust firmware service.
//!
//! # Platform constraints
//!
//! The engine links the camera's own uClibc runtime and the Rockchip libraries,
//! so it must be built for `armv7-unknown-linux-musleabihf` with dynamic linking
//! against the board's uClibc:
//!
//! * The board's `ld-uClibc.so.0` faults in `_dl_get_ready_to_run` when the main
//!   executable carries a `PT_TLS` segment, so this crate must stay free of
//!   thread-local storage. That rules out `std` (its runtime always emits
//!   thread-locals) and is why the crate is `no_std` with `alloc`.
//! * uClibc-ng lacks `__libc_start_main`, so binaries use a small entry stub
//!   that calls `__uClibc_main` instead of musl's CRT objects.
//!
//! The module set is intentionally small: pure, hardware-independent logic
//! lives here and is tested on the host, while the hardware bindings and the
//! platform layer are added alongside the pipelines that need them.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod cli;
pub mod keyframe;
pub mod platform;
pub mod pts;
