# Media engine (Rust port of `native/c220-media-engine.c`)

This crate will own the Rockchip sensor/ISP/VENC/AI/AO hardware in one process,
replacing the C engine, and speak the shared `C220` record protocol over the
private Unix socket. It is being built up in phases; only hardware-independent,
host-tested logic exists so far.

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

Verified by building a trivial TLS-free Rust binary against the rootfs and
running it under `qemu-arm`: it follows the same syscall sequence and reaches
the same vendor-constructor failure as the shipped C engine, which is the
expected behavior without camera hardware.

## Verification without the camera

The pure logic in this crate is tested on the host (`cargo test -p
c220-media-engine`, no hardware and no rootfs needed). ARM behavior can be
checked under an ARM userspace emulator, for example:

```sh
qemu-arm-static -L /path/to/sysroot ./c220-media-engine --socket /tmp/media.sock
```

A sysroot for that check is the factory `rootfs` tree plus `home/lib`; the
engine's RPATH (`/home/lib:/oem/usr/lib:/lib`) is how it finds the vendor
libraries on the camera. Vendor constructor code segfaults under emulation
because the hardware is absent, so emulation can verify loading, framing and
control flow, not media output.

## Status

| Area | State |
| --- | --- |
| Record framing (shared with the firmware) | Done, in `../media-protocol` |
| CLI contract | Done, reference options minus the abandoned sub-stream ones |
| PTS counter, HEVC keyframe detection | Done |
| Platform layer (sockets, threads, allocator, logging) | Not started |
| Vendor bindings (V4L2, Rockchip MPI, AIQ, FDK AAC) | Not started |
| Video, microphone, speaker, night-mode pipelines | Not started |

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
