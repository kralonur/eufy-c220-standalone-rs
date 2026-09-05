# Media engine (Rust port of `native/c220-media-engine.c`)

This crate will own the Rockchip sensor/ISP/VENC/AI/AO hardware in one process,
replacing the C engine, and speak the shared `C220` record protocol over the
private Unix socket. It is being built up in phases: it already runs as a
process - entry stub, platform layer, session loop and command handling - with a
mock backend where the vendor bindings will go.

## Why this crate is `no_std`

The engine must run on the camera's own uClibc runtime, in the same process as
the Rockchip libraries (`librockit`, `librkaiq`, `libsmartIr`, `libfdk-aac`),
because those libraries drive the hardware and their pthread/TLS state cannot be
shared with a second libc. Measurements taken against the factory rootfs in
`native/camera-rootfs`:

* **The loader faults on `PT_TLS` in the main executable.** A main binary that
  carries a thread-local segment makes `ld-uClibc.so.0` segfault inside
  `_dl_get_ready_to_run`, before any program code runs. Reproduced with a
  three-line C program that only declares one unused `__thread` variable.
  Thread-locals inside shared libraries (libc, libstdc++) are fine: the loader
  handles those. This is why the C engine, which has no thread-locals, works.
* **`std` always emits `PT_TLS`**, so Rust `std` cannot be used in the main
  executable. `-crt-static` does not help, because a static musl binary cannot
  host the uClibc vendor libraries (two libcs, two thread-pointer owners).
  Hence `no_std` plus `alloc`, and no `thread_local!` anywhere in this process.
* **uClibc-ng has no `__libc_start_main`** - musl's `crt1.o` cannot be used.
  Binaries instead call `__uClibc_main`, the uClibc equivalent, from a small
  entry stub.
* **uClibc-ng has no `getauxval`.** The C engine never needed it; the no_std
  engine must not use it either.
* A TLS-free binary needs only `libc.so.0` plus the vendor libraries at link
  time. `__tls_get_addr` is resolved from the loader at runtime for the vendor
  libraries, exactly as `librkaiq`'s own `NEEDED` entry does.
* **uClibc's `off_t` is 32-bit**, while the `libc` crate's musl bindings use a
  64-bit one. Calling `mmap` through those bindings therefore lays out the
  arguments differently and the kernel answers `EINVAL`; the engine's allocator
  calls `mmap64`, whose 64-bit offset matches the bindings. The same care is
  needed for any libc call that takes an `off_t`.
* **`struct sigaction` is another layout the two libcs disagree on** (the signal
  mask is smaller in uClibc), so the engine installs its handlers with `signal`,
  whose prototype is an integer and a handler.
* **Socket timeouts are sized by the kernel's word size**: 32-bit seconds on the
  camera, 64-bit on a development host. `platform::net` defines that structure
  rather than borrowing `libc::timeval`.
* The `libc` crate's default `std` feature emits no link directive (it assumes
  `std` links libc), so `native/arm-engine-linker.sh` names the camera's
  `libc.so.0` explicitly.

Verified by building a trivial TLS-free Rust binary against the rootfs and
running it under `qemu-arm`: it follows the same syscall sequence and reaches
the same vendor-constructor failure as the shipped C engine, which is the
expected behavior without camera hardware.

## Building

```sh
CAMERA_ROOT=../native/camera-rootfs ./native/build-rust-media-engine.sh
```

The device paths are linked by default, because that is what the camera runs.
`ENGINE_FEATURES= ./native/build-rust-media-engine.sh /tmp/engine` builds the
hardware-free variant instead: it links no vendor libraries, so it can start
under an emulator, which is how the daemon protocol is checked on the target ABI.

The camera rootfs is a build input either way: the engine links the camera's own
uClibc instead of musl's static CRT.
`native/arm-engine-linker.sh` does that link, and refuses to produce a
statically linked binary, which would not be able to host the vendor libraries.

The camera target builds in the release profile, which aborts on panic. A
`no_std` binary cannot unwind, so `cargo check --target
armv7-unknown-linux-musleabihf` needs `--release` as well; the host build has no
such requirement.

## Running without a camera

On a development host the same engine runs with the mock backend, which is
enough for the firmware to be pointed at it and for the protocol to be
exercised end to end:

```sh
cargo run -p c220-media-engine -- --socket /tmp/c220-media.sock
```

The ARM binary runs under an ARM userspace emulator, which checks the camera ABI
(loader, signal handling, allocator) rather than just the logic. That has to be
the hardware-free build, because the vendor libraries' constructors run as soon
as the binary is loaded and fault where the hardware is absent:

```sh
ENGINE_FEATURES= ./native/build-rust-media-engine.sh /tmp/engine-mock
qemu-arm-static -L /path/to/sysroot /tmp/engine-mock --socket /tmp/media.sock
```

A sysroot for that check is the factory `rootfs` tree plus `home/lib`; the
engine's RPATH (`/home/lib:/oem/usr/lib:/lib`) is how it finds the vendor
libraries on the camera. On the hardware build, `--mock-backend` selects the
hardware-free backend at run time, which keeps the daemon path usable on a camera
whose speaker is being diagnosed - but it cannot help under emulation, where the
libraries fault before `main` runs.

Both builds are checked for the one thing that would make the binary unrunnable
on the camera: `readelf -l` must report **no `PT_TLS` segment**.

## Status

| Area | State |
| --- | --- |
| Record framing (shared with the firmware) | Done, in `../media-protocol` |
| CLI contract | Done, reference options minus the abandoned sub-stream ones |
| PTS counter, HEVC keyframe detection | Done |
| Platform layer (sockets, signals, allocator, logging) | Done |
| Engine process: entry stub, session loop, command handling, mock backend | Done |
| Vendor bindings (Rockchip MPI, AIQ, SmartIR, FDK AAC) | Done, ABI-asserted |
| Speaker path: amplifier pin, AO device, PCM and AAC talkback | Done, awaiting hardware |
| V4L2 capture bindings | Not started |
| Video, microphone, night-mode pipelines | Not started |

The daemon-facing behavior already matches the reference engine: the private
socket with mode `0600`, one session at a time, the night-mode greeting on
connect, the reference payload limits per record kind, a malformed record ending
the session without stopping the engine, and the same `media_*` log lines.

## Sub-stream: dropped

The detection sub-stream is deliberately not implemented. The reference path is
not a working feature to port: `open_sub_encoder()` configures the encoder with
`u32MaxPicWidth` and `u32PicWidth` both at 640x360 while the comment above it
claims the encoder scales, the code that actually runs is a CPU downscale that
is off by default (`sub_enabled = 0`), and that downscale writes 640 bytes past
a correctly sized NV12 sub-frame buffer (measured: 346240 bytes written into a
345600-byte buffer) and scrambles chroma. The firmware also has no delivery path
for it: every video record goes to the single RTSP video track.

Consequently the abandoned sub-stream options (`--sub`, `--sub-dry`,
`--no-sub`, `--sub-fps`, `--sub-bitrate`) are not part of this engine's command
line, and ABI record kind 5 decodes as an unknown kind so an older engine cannot
inject a second video resolution into the main stream. If a low-resolution
detection stream is wanted later, the work starts with a hardware gate (VENC
multi-stream scaling or the RGA 2D engine), not with the reference CPU code.
