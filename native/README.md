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

The source and build logic are contained in this repository, but the binary
must link against the factory ARM libraries that remain on the camera. Place
a copy of the extracted camera library tree at `native/camera-rootfs/`, or set
`CAMERA_ROOT` to a directory with this layout:

```text
<CAMERA_ROOT>/rootfs/lib/libc.so.0
<CAMERA_ROOT>/rootfs/oem/usr/lib/librockit.so
<CAMERA_ROOT>/rootfs/oem/usr/lib/librkaiq.so
<CAMERA_ROOT>/rootfs/oem/usr/lib/libsmartIr.so
<CAMERA_ROOT>/home/lib/libfdk-aac.so.1
```

The complete transitive library closure must be present in the same extracted
rootfs. These factory binaries are local build inputs and are ignored by Git;
they are not reimplemented or replaced by the companion.

Build the companion with:

```sh
CAMERA_ROOT="$PWD/native/camera-rootfs" ./native/build-c220-media-engine.sh
```

The output defaults to `dist/c220-media-engine`. Copy that binary into the
camera image as `/home/c220-media-engine`; the Rust firmware uses that path by
default.
