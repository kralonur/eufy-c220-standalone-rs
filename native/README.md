# Native media companion

`c220-media-engine.c` is the only native runtime source required by the Rust
firmware. It owns the camera hardware path that cannot be replaced by the
Rust service layer:

- Rockchip AIQ/V4L2/VENC video capture and encoding;
- Rockchip AI/AENC microphone capture;
- Rockchip AO speaker playback and FDK AAC decoding; and
- the shared `C220` Unix-socket record protocol.

`c220-start.S` is the minimal ARM entry point used by the camera's uClibc
runtime. The other native files from the reference project are probes or old
split helper daemons and are not part of the deployed runtime.

## Build inputs

The source, build logic, and minimum factory ARM library/IQ bundle are
contained in this repository under `native/camera-rootfs/`. Set `CAMERA_ROOT`
to another compatible extraction when building for a different factory
firmware revision. The expected layout is:

```text
<CAMERA_ROOT>/rootfs/lib/libc.so.0
<CAMERA_ROOT>/rootfs/oem/usr/lib/librockit.so
<CAMERA_ROOT>/rootfs/oem/usr/lib/librkaiq.so
<CAMERA_ROOT>/rootfs/oem/usr/lib/libsmartIr.so
<CAMERA_ROOT>/home/lib/libfdk-aac.so.1
```

The checked-in bundle contains the complete transitive library closure for
the validated camera revision. These factory libraries are not reimplemented
or replaced by the companion; they are linked dynamically and must remain at
their matching paths in the deployed camera image.

Build the companion with:

```sh
CAMERA_ROOT="$PWD/native/camera-rootfs" ./native/build-c220-media-engine.sh
```

The output defaults to `dist/c220-media-engine`. Copy that binary into the
camera image as `/home/c220-media-engine`; the Rust firmware uses that path by
default.

## Camera package

The repository includes the Rust target linker in `.cargo/config.toml` and
`native/arm-linker.sh`. Build both executables and create a `/home` overlay with:

```sh
./native/package-c220.sh
```

The default output is `dist/c220-home`. It contains `c220-firmware`,
`c220-media-engine`, `lib/libfdk-aac.so.1`, the sensor calibration blob, and
`script/c220-startup.sh`. When `BASE_HOME` is supplied, the superseded
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
boot-image changes explicit. `SQUASHFS_OUT` refuses images larger than the
camera's 4,915,200-byte `/home` partition. Set `REMOVE_LEGACY_MEDIA=0` only
for a deliberate compatibility/debug image. Do not flash an image until its
size, checksum, factory rollback path, and camera boot behavior have been
checked.
