# Building

## Environment

```sh
nix develop
./native/check.sh
```

The pinned shell supplies Rust/ARM target, unwrapped Clang/LLVM, QEMU and
SquashFS tools. Without Nix, install Rust with target
`armv7-unknown-linux-musleabihf`, ARM-capable Clang, `mksquashfs`/`unsquashfs`,
OpenSSL and the Cargo size tools used by `native/check.sh`.

Keep `.cargo/config.toml` and native linker/compiler wrappers. Build in one
environment: mixing distribution and Nix compilers in a shared `target/` can reuse
incompatible objects even when version strings match. The wrappers explicitly
set ARM-compatible hardening flags; the shell omits `rust-src` to avoid embedding
local toolchain paths in panic locations.

`native/check.sh` runs fmt, strict Clippy, tests, ARM firmware build and size gate.
It does not exercise hardware. Firmware-only build:

```sh
cargo build --release --locked --target armv7-unknown-linux-musleabihf -p c220-firmware
```

## Engine and image

First [harvest your own camera](setup-and-recovery.md#2-enter-factory-firmware-and-harvest).
No vendor libraries or calibration data are redistributed. The required bundle
includes `rootfs/lib/libc.so.0`, Rockchip libraries under `rootfs/oem/usr/lib/`,
`home/lib/libfdk-aac.so.1`, sensor calibration and ISP tuning files.

From the repository root:

```sh
cp -a /path/to/card/c220-harvest/camera-rootfs native/camera-rootfs
CAMERA_ROOT="$PWD/native/camera-rootfs" ./native/build-rust-media-engine.sh

BASE_HOME=/path/to/card/c220-harvest/base-home \
INSTALL_STARTUP_HOOK=1 \
SQUASHFS_OUT=dist/c220-home.squashfs \
./native/package-c220.sh
```

Engine output: `dist/c220-media-engine-rust`. Packaging installs it as
`/home/c220-media-engine`, stages `dist/c220-home`, preserves required factory
helpers/calibration and removes superseded media binaries. `BASE_HOME` is a
preserved factory tree, not your live NVRAM. Startup changes are explicit through
`INSTALL_STARTUP_HOOK=1`.

**Stop if `app.sh` hardening fails.** The current hardener expects a known
customised layout; fresh-factory conversion is not yet verified. Do not bypass
it with `SECURE_DEPLOYMENT=0`. See [installation](setup-and-recovery.md#3-build).

The image must fit **4,915,200 bytes**, including alignment and the extra write
block required by the reference board. Packaging's size check alone is not a
safe-flash check. Use only the [write/readback procedure](setup-and-recovery.md#4-write-and-verify).

`nix run .` lists tasks; `.#check`, `.#build`, `.#engine`, `.#package`, `.#verify`,
`.#size` and `.#bloat` wrap the corresponding scripts/tools. Engine/package still
need your private vendor inputs. Platform constraints and mock runs are in
[media-engine.md](media-engine.md).
