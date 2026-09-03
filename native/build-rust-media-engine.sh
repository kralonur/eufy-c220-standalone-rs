#!/bin/sh
set -eu

# Build the Rust media engine for the camera.
#
# The engine links the board's own uClibc instead of musl's static CRT, because
# the Rockchip and FDK libraries that own the hardware must run in its process.
# An extracted camera rootfs supplies the libc, the loader and (once the vendor
# bindings land) the vendor libraries, so CAMERA_ROOT is a build input even
# before the hardware code exists.
#
# See media-engine/README.md for the platform constraints this build encodes.

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
CAMERA_ROOT=${CAMERA_ROOT:-"$REPO_ROOT/native/camera-rootfs"}
TARGET=armv7-unknown-linux-musleabihf
OUT=${1:-"${OUT:-$REPO_ROOT/dist/c220-media-engine-rust}"}
SHIM_DIR=${SHIM_DIR:-"$REPO_ROOT/target/native/c220-media-engine/shim"}
ROOTFS_LIB="$CAMERA_ROOT/rootfs/lib"

for required in "$ROOTFS_LIB/libc.so.0" "$ROOTFS_LIB/libgcc_s.so.1"; do
    if [ ! -e "$required" ]; then
        echo "missing native build input: $required" >&2
        echo "set CAMERA_ROOT to an extracted C220 rootfs" >&2
        exit 1
    fi
done

# rustc links `-lc` and `-lgcc_s` for this target. These symlinks make those
# names resolve to the camera's own uClibc objects, whose SONAMEs are what ends
# up in the binary's NEEDED entries (libc.so.0, libgcc_s.so.1).
mkdir -p "$SHIM_DIR" "$(dirname -- "$OUT")"
ln -sf "$ROOTFS_LIB/libc.so.0" "$SHIM_DIR/libc.so"
ln -sf "$ROOTFS_LIB/libgcc_s.so.1" "$SHIM_DIR/libgcc_s.so"

RUSTFLAGS="-C target-feature=-crt-static" \
CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER="$SCRIPT_DIR/arm-engine-linker.sh" \
C220_UCLIBC_SHIM="$SHIM_DIR" \
CAMERA_ROOT="$CAMERA_ROOT" \
    cargo build --release --locked --target "$TARGET" -p c220-media-engine \
        --bin c220-media-engine

cp "$REPO_ROOT/target/$TARGET/release/c220-media-engine" "$OUT"
file "$OUT"
ls -la "$OUT"
