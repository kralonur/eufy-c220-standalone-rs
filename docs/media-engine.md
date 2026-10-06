# Media engine

`media-engine/` owns Rockchip capture/ISP/encoding, microphone and speaker hardware.
The Rust firmware owns network services and controls; `media-protocol/` defines
the private 20-byte-header `C220` record protocol. The engine serves one daemon
session at a time over a mode-600 Unix socket. Malformed input ends the session,
not the process. No detection sub-stream is implemented.

## Platform constraints

The camera engine is **`no_std` + `alloc`**, dynamically linked to the board's
uClibc and vendor libraries. Do not replace this with static musl or Rust `std`:

- The factory loader faults on a main executable's `PT_TLS`; `std` emits one.
  Both hardware/mock ARM builds must have no TLS segment (`readelf -lW`).
- Entry uses `__uClibc_main`, not musl CRT. The camera lacks `getauxval`.
- uClibc `off_t`/`sigaction` differ from musl bindings. The allocator uses
  `mmap64`; signal setup uses `signal`. Socket timeouts use target-width fields.
- V4L2 layouts/ioctl numbers are the camera's 32-bit ABI, guarded by assertions,
  not structures taken from a 64-bit host.
- Vendor attribute arguments must be writable: some calls modify them.
  Microphone teardown must unbind AI→AENC before destroying the encoder.

`native/arm-engine-linker.sh` links the camera's libc and rejects static output.
The engine's runtime library path is `/home/lib:/oem/usr/lib:/lib`.

## Build and debug

See [building](building.md) for vendor inputs and deployment packaging.
From the repository root:

```sh
CAMERA_ROOT="$PWD/native/camera-rootfs" ./native/build-rust-media-engine.sh
# ARM hardware-free variant; still needs camera uClibc
ENGINE_FEATURES= ./native/build-rust-media-engine.sh /tmp/engine-mock
# Native host mock
cargo run -p c220-media-engine -- --socket /tmp/c220-media.sock
```

A hardware-free ARM build can run with `qemu-arm` (or `qemu-arm-static`) and a
matching sysroot. Hardware-linked vendor constructors may fault before `main`
without the camera; `--mock-backend` cannot prevent that loader-time failure.
ARM checks need the release profile because a `no_std` binary cannot unwind.

`media-engine/examples/media_probe.rs` exercises records/audio on a bench. Build:

```sh
cargo build --release --target armv7-unknown-linux-musleabihf \
  -p c220-media-engine --example media_probe
```

Do not run a second engine against active hardware. Bench runs should use RAM
artifacts, a controlled supervisor stop/restart, clear motor travel and explicit
audio consent. Engine fatal signals are logged as `media_signal=<n>` before
re-raising; check those and `/tmp/c220-media-rockit.log` when diagnosing crashes.

## Runtime policies

- Capture queues copies without socket blocking. One writer uses vectored sends
  and a two-second monotonic deadline. Limits: six pending video/four other
  records and six MiB including in-flight wire cost. Overload resynchronizes at
  a keyframe, evicts audio and coalesces night state. Detach shuts down the old
  socket, clears queues and advances session ownership.
- Firmware payload allocations share a separate six-MiB backing-capacity budget;
  slices retain their allocation's permit. History holds 64 video/256 audio
  records. These are policies, **not total RSS or throughput guarantees**.
- RTP uses reusable whole-access-unit batches; Annex-B inspection is shared and
  allocation-free. Parameter sizes/amplification and network write waits are
  bounded. Session shutdown cleans up media and speaker ownership.
- Sensor capture measured about 15.2 FPS even when requested higher. Cache
  maintenance is required between ISP/encoder access; HEVC IRAP detection is
  software-based. Do not claim host packetization speedups as ARM FPS gains.
- SmartIR keeps factory tuning, five-sample confirmation at 1 Hz and a ten-sample
  settle window. Explicit day/night commands override detection.
- Microphone is opt-in PCMA 8 kHz (160 bytes/20 ms). Speaker accepts PCM16 16 kHz
  or AAC, volume 0–100. Talkback needs real-time delivery through a persistent
  connection; slow per-chunk TLS/Digest requests cause audible gaps.

Manual host packetization benchmark (not camera performance):

```sh
cargo test -p c220-firmware --release benchmark_media_packetization -- --ignored --nocapture
```

For the current hardware acceptance limits, see [README status](../README.md#status).
