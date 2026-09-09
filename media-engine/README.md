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

## Bringing it up on the camera without flashing

The engine can be validated on the real hardware without touching flash. The
deployed `/home` is a read-only squashfs, so the working firmware cannot be
modified from a shell, and `/tmp` is tmpfs: put the engine and the probe there,
run them by hand, and a reboot restores everything.

`examples/media_probe.rs` is the other half of that: a static musl binary that
speaks the engine's socket, since the firmware is the engine's only real client
and a second connection waits while the deployed firmware holds the session. It
prints the greeting, counts what the engine produces per record kind (`drain` is
the only way to see microphone records before the RTSP path can carry them),
plays a tone or an ADTS file through the speaker, and drives volume and clear.
It is built with the workspace's default target linker:

```sh
cargo build --release --target armv7-unknown-linux-musleabihf \
    -p c220-media-engine --example media_probe
```

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
| Vendor bindings (Rockchip MPI, AIQ, SmartIR, FDK AAC) | Done, ABI-asserted against the factory engine |
| Speaker path: amplifier pin, AO device, PCM and AAC talkback | Done, awaiting hardware |
| Microphone path: AI + AENC, capture thread, G.711 records | Done, awaiting hardware |
| V4L2 capture interface, at the camera's 32-bit ABI | Done |
| Video path: MMZ input buffer, VENC channel, capture thread | Done, awaiting hardware |
| Image path (AIQ), night-mode monitor | Not started |

The daemon-facing behavior already matches the reference engine: the private
socket with mode `0600`, one session at a time, the night-mode greeting on
connect, the reference payload limits per record kind, a malformed record ending
the session without stopping the engine, and the same `media_*` log lines.

Records reach the daemon through one shared sink: the session thread writes the
greeting, night-mode changes and its own replies, and the microphone thread writes
captured audio, all under one lock so a record never interleaves with another. A
record produced while no daemon is connected is dropped rather than queued, which
is what the reference engine's `client_current() >= 0` check does, and the sink is
detached before a session's socket closes so a late writer cannot reach a
descriptor that has been reused.

## The video path

One caller-supplied buffer is queued to `/dev/video12`, each frame the ISP fills
is handed to the encoder in place, and the access unit that comes back becomes a
video record. Three things about it are not obvious:

* **The frame rate is the rate the sensor really delivers** - about 15.17 fps,
  measured, unchanged whether 20 or 30 fps is requested. Constant-bitrate mode
  divides the budget by the declared frame rate, so declaring 20 fps starves every
  frame of bits: 12.5 KB per frame is 1545 kbps, not the configured 2048. The
  capture node is asked for the configured rate anyway, and both the interval it
  reports and the one it settles on are logged.
* **Cache maintenance is part of the data path.** The ISP writes the frame past
  the CPU cache and the encoder writes the packs the same way, so the input buffer
  is written back before the encoder reads it and each pack is invalidated before
  the engine copies it into an access unit.
* **The encoder reports no keyframe flag**, so one is decided in software by
  scanning the access unit for an IRAP NAL type - [`crate::keyframe`], ported
  with the original's exact scan bounds.

The V4L2 declarations live in `platform::v4l2` and are the camera's 32-bit ABI,
not the host's: a request number encodes `sizeof(struct)` in its bits and
`struct v4l2_buffer` contains an `unsigned long`, so an engine built with a
64-bit host's numbers fails every ioctl with `ENOTTY` on the board. The numbers
and layouts were read out of the same headers and the same
`arm-linux-musleabihf` target the reference engine compiles with, and are held
by compile-time assertions; reading those offsets back caught three mistakes in
the first draft that no host test could have.

What is still missing is the image path: the reference engine starts AIQ (the ISP
tuning, SmartIR and the IR-cut) right before it queues the buffers. Capture and
encoding do not depend on it, but the picture stays untuned until it lands, and
`backend::video` marks the spot where it goes.

## Verification against the factory engine

The camera's own media engine - `/home/c220-media-engine` in the preserved
partition, the binary this project's reference C engine was modelled on - can be
read instruction by instruction, and every device attribute the audio paths use
was checked against it. They match exactly, which is as close to hardware
verification as this port can get before it runs on the camera:

| What | Factory engine | This engine |
| --- | --- | --- |
| AI attributes | channels 2, 8 kHz, 16-bit, mono, frames 4, 320 samples/frame, 2 channels, card name at offset 40 of 104 | identical |
| AI channel parameters | frame depth 1, loopback none | identical |
| AI track mode | 8 (`AUDIO_TRACK_FRONT_LEFT`) | identical |
| AENC attributes | A-law `0x40000002`, 4 buffers, 16-bit, 1 channel, 8 kHz, 8 bits/sample, 64 kbit/s, depth 4 | identical |
| AENC channels | 1 | identical |
| AO attributes | channels 2, 8 kHz, 16-bit, mono, frames 4, 1024 samples/frame, 2 channels | identical |
| AO resampler / track | 16 kHz, track 10 (`AUDIO_TRACK_OUT_STEREO`) | identical |
| External block | pointer at 0, size at 24 as a 64-bit value, opaque at 36, 40 bytes | identical |
| AAC decoder | `aacDecoder_Open(TT_MP4_ADTS, 1)`, 4096-sample output, 64 decode attempts | identical |
| Rockit logging | `rt_log_level=0`, `rt_log_path=/tmp/c220-media-rockit.log` before `RK_MPI_SYS_Init` | identical |

The same reading found two things this engine had been missing and now does: the
`RK_MPI_SYS_Init()` call every MPI entry point depends on, and those two logging
variables, which have to be set before the media system starts or Rockit chooses
its own level and destination. `HDR_MODE=0` belongs to the same set and is set by
the image path (`start_aiq`), because it selects which IQ files load - the factory
sets it in the same place.

## Library check

Before writing infrastructure by hand, the state of published Rust support for
this hardware was checked, and there is none that fits:

* No crate binds Rockchip's MPI/rockit or the AIQ camera engine. The vendor
  libraries are C only, so `vendor/` stays a hand-written FFI - now with the
  factory's own usage as the ABI reference rather than the C engine alone.
* `linux-raw-sys` is `no_std`-compatible but contains no V4L2 types at all
  (no `v4l2_buffer`, no `VIDIOC_QBUF`).
* `v4l` and `v4l2r` do exist and are maintained, but both are `std` crates that
  generate bindings with `bindgen` at build time, which needs kernel headers and
  libclang for a uClibc cross build and would put a `std`-shaped dependency in a
  `no_std` binary.

So the V4L2 structures for the capture path will be declared here, with the size
and offset assertions the vendor bindings already use, exactly as the reference
engine declares them.

## The factory application as a reference

The camera's own application, `app/anker_t8w11`, is the authority on how this
hardware is meant to be driven, and it is available locally: the preserved
`/home` partition is in the main workspace's `dist/camera-backup-*/mtd6-home.bin`,
a squashfs that `unsquashfs` opens without root. Its night-mode branch is where
`FACTORY_TUNING` came from.

Its vendor API surface is a useful checklist. It imports 120 `RK_MPI_`/`rk_aiq_`/
`rk_smart_ir_`/`aacDecoder_` entry points; the engine binds 49 of them, which is
what the reference C engine needs. Most of the rest is functionality this project
does not want (motion detection, OSD regions, voice enhancement, AAC playback,
JPEG stills), but three are worth remembering when the video path is written:

* `RK_MPI_VENC_RequestIDR` forces a keyframe, which is how an encoder avoids
  making a newly connected viewer wait for the next group of pictures.
* `RK_MPI_VENC_SetChnAttr` and `ResetChn` reconfigure a running channel.
* `RK_MPI_VENC_SetChnRefBufShareAttr` shares reference buffers between channels,
  which only matters with more than one encoder channel.

`rk_aiq_uapi2_setBrightness`, `setMirrorFlip` and the exposure-switch calls behind
`rk_aiq_user_api2_ae_setExpSwAttr` are image controls the factory app exposes and
this firmware does not; the night path here uses scene switching alone, which is
the behavior verified on the camera.

## Night mode: what the port has to keep

The night path was tuned on hardware after the branch this crate was started from
(see `33122d2` on `main`), and the Rust version has to keep all of it:

* The detector runs on its own defaults unless it is configured, and one default
  is actively harmful: with a non-zero AWB radius the block set it averages comes
  up empty in a monochrome infrared frame, so it measures the origin against the
  baseline gains, reads daylight while the IR LED is on, and flips the LED off -
  which drops the frame back to dark and repeats, 517 times in one 45 minute boot
  on the camera. `vendor::aiq::FACTORY_TUNING` carries the values the factory
  application applies in auto mode, read out of its `anker_t8w11` binary.
* One verdict is one statistics block wide, so a change has to hold for five
  samples at the 1 Hz poll, and the sensor is left alone for ten samples after a
  switch so the IR-cut and the AE can converge before the next opinion.
* A commanded mode owns the hardware: the detector is not consulted, and the
  command arms the settle window too, so a stale verdict cannot fight it.
* A rejected night or day sub-scene and a failing detector configuration are both
  logged. A rejected scene name otherwise looks like a bad night image rather
  than a rejected name.

## Known differences from the reference engine

These are deliberate, and each one is small enough that it can be reverted
without touching anything around it:

* **One session at a time.** The reference engine accepts in its own thread and
  replaces a client that is still connected (`media_client_replaced=1`). Here the
  session loop serves one daemon at a time and accepts the next after it ends, so
  a daemon that connects twice without closing the first connection waits. The
  firmware opens one connection and closes it on restart, so this only changes
  behavior in a case the daemon does not produce.
* **The AO device is disabled on shutdown.** The reference clears the queue,
  disables the resampler and the channel, and leaves the device enabled; it never
  imports `RK_MPI_AO_Disable`. This engine disables it as well, which is the
  counterpart of the `RK_MPI_AO_Enable` that was called on the way up.
* **The keyframe byte is read as a flag.** The reference compares it with `1` to
  decide between PCM and AAC; the shared framing layer treats any non-zero byte as
  true. The firmware writes `0` or `1`.
* **A partial AAC fill resumes where it stopped.** When the decoder's internal
  buffer cannot take a whole access unit, the reference feeds the access unit from
  the start again, which duplicates what it already consumed; this engine keeps
  the bytes the decoder reported as not yet copied, which is what FDK's
  documentation asks for. With one access unit per record - what the daemon
  sends - both behave identically.
* **An empty night-mode record ends the session** in both engines; this engine
  reads it the same way after a review pass found it had been treating it as
  `auto`.
* **Errors name the right cause.** Thread and mutex failures report the code the
  pthread call returned rather than a stale `errno`, and a record header that is
  malformed for a reason other than its magic says so in the log.

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
