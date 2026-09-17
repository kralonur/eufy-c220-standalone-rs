# Native media companion

`media-engine/` is the native runtime companion built and deployed by this
repository. It owns the camera hardware path that cannot be replaced by the
Rust service layer:

- Rockchip AIQ/V4L2/VENC video capture and encoding;
- Rockchip AI/AENC microphone capture;
- Rockchip AO speaker playback and FDK AAC decoding; and
- the shared `C220` Unix-socket record protocol.

The Rust engine is `no_std` + `alloc` on the camera so its executable has no
`PT_TLS` segment, and it uses the camera's uClibc entry point and vendor
libraries through hand-written FFI, including its own entry stub. The legacy C
engine and its builder have been removed now that the Rust engine owns the
hardware.

## Build inputs

The build needs the factory ARM libraries and ISP tuning data, which this
repository does not redistribute (see the licensing note in README.md). Extract
them from your own camera - `native/factory-bootstrap/harvest.sh` does that from the
root shell a boot card opens, and writes exactly the layout below - and put the
result at `native/camera-rootfs/`. Set `CAMERA_ROOT` to a different extraction when
building for another factory firmware revision. The expected layout is:

```text
<CAMERA_ROOT>/rootfs/lib/libc.so.0
<CAMERA_ROOT>/rootfs/oem/usr/lib/librockit.so
<CAMERA_ROOT>/rootfs/oem/usr/lib/librkaiq.so
<CAMERA_ROOT>/rootfs/oem/usr/lib/libsmartIr.so
<CAMERA_ROOT>/home/lib/libfdk-aac.so.1
```

The bundle has to contain the complete transitive library closure for the
validated camera revision. These factory libraries are not reimplemented
or replaced by the companion; they are linked dynamically and must remain at
their matching paths in the deployed camera image.

The same harvest also collects the factory `/home` tree, which `package-c220.sh`
takes as `BASE_HOME`: the vendor boot script, the Wi-Fi helper, `tinymix`, the
region tables and the rest of the preserved files the overlay is built on. It comes
from a camera that is still on factory firmware; once this firmware is installed,
`/home` is our tree, so the harvest leaves `BASE_HOME` alone there. The factory tree
can also be cut out of a full-chip dump taken beforehand - `mtd6` in the layout table
below is the `/home` squashfs - and `unsquashfs` on a computer reads it.

Build the companion with:

```sh
CAMERA_ROOT="$PWD/native/camera-rootfs" ./native/build-rust-media-engine.sh
```

The output defaults to `dist/c220-media-engine-rust`. Copy that binary into the
camera image as `/home/c220-media-engine`; the Rust firmware uses that path by
default. The package script performs this rename while staging the `/home`
overlay.

## Camera package

The repository includes the Rust target linker in `.cargo/config.toml` and
`native/arm-linker.sh`. Build both executables and create a `/home` overlay with:

```sh
./native/package-c220.sh
```

The default output is `dist/c220-home`. It contains the Rust `c220-firmware`,
the Rust `c220-media-engine`, `lib/libfdk-aac.so.1`, the sensor calibration
blob, and `script/c220-startup.sh`. When `BASE_HOME` is supplied, the superseded
`c220-daemon` and split `c220-hw-*` helpers are removed by default so they
cannot race the unified service and the image fits the `/home` partition. To
preserve an existing factory `/home` tree and optionally create a squashfs
image:

```sh
BASE_HOME=/path/to/preserved/home \\
INSTALL_STARTUP_HOOK=1 \\
SQUASHFS_OUT=dist/c220-home.squashfs \\
./native/package-c220.sh
```

The startup hook is appended only when `INSTALL_STARTUP_HOOK=1`; this keeps
boot-image changes explicit. `SECURE_DEPLOYMENT=1` is the default and removes
legacy media binaries plus vendor/custom SD-card, NVRAM root-script, RescueAP,
and Telnet hooks from a preserved `app.sh`. `SECURE_DEPLOYMENT=0` is only for
a deliberate offline board bring-up. `SQUASHFS_OUT` refuses images larger than
the camera's 4,915,200-byte `/home` partition. Set `REMOVE_LEGACY_MEDIA=0` only
for a deliberate compatibility/debug image. Do not flash an image until its
size, checksum, factory rollback path, and camera boot behavior have been
checked.
## Where the rest is documented

This file covers building and packaging. How the device behaves at run time, and the
operational traps found on this board, are in
[`docs/camera-internals.md`](camera-internals.md); setting the camera up,
resetting it and recovering it are in the
[setup and recovery guide](setup-and-recovery.md).
