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
default. The boot/startup integration is still a separate step.
