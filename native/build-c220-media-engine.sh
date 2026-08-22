#!/bin/sh
set -eu

# Build the unified camera-side media engine (video + microphone + speaker).
#
# The application source and linker layout live in this repository. The camera
# rootfs is a local build input because it contains the factory ARM libraries
# that the engine must preserve and use at runtime.
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
CAMERA_ROOT=${CAMERA_ROOT:-"$REPO_ROOT/native/camera-rootfs"}
OUT=${1:-"${OUT:-$REPO_ROOT/dist/c220-media-engine}"}
CC=${CC:-zig}
STRIP=${STRIP:-llvm-strip}
BUILD_DIR=${BUILD_DIR:-"$REPO_ROOT/target/native/c220-media-engine"}
FDK_INCLUDE=${FDK_INCLUDE:-"$REPO_ROOT/native/include/fdk-aac"}

ROOTFS="$CAMERA_ROOT/rootfs"
ROOTFS_LIB="$ROOTFS/lib"
OEM_LIB="$ROOTFS/oem/usr/lib"
HOME_LIB="$CAMERA_ROOT/home/lib"

for required in \
    "$FDK_INCLUDE/aacdecoder_lib.h" \
    "$ROOTFS_LIB/libc.so.0" \
    "$OEM_LIB/librockit.so" \
    "$OEM_LIB/librkaiq.so" \
    "$OEM_LIB/libsmartIr.so" \
    "$HOME_LIB/libfdk-aac.so.1"; do
    if [ ! -e "$required" ]; then
        echo "missing native build input: $required" >&2
        echo "set CAMERA_ROOT to an extracted C220 rootfs, or install the FDK headers" >&2
        exit 1
    fi
done

mkdir -p "$BUILD_DIR" "$(dirname -- "$OUT")"

"$CC" cc -target arm-linux-musleabihf -std=c11 -O3 -fno-stack-protector \
  -Wall -Wextra -Werror -Wno-unused-function -I"$FDK_INCLUDE" \
  -c "$SCRIPT_DIR/c220-media-engine.c" \
  -o "$BUILD_DIR/c220-media-engine.o"
"$CC" cc -target arm-linux-musleabihf -c "$SCRIPT_DIR/c220-start.S" \
  -o "$BUILD_DIR/c220-start.o"
"$CC" cc -target arm-linux-musleabihf -nostdlib \
  -Wl,-e,_start -Wl,--dynamic-linker,/lib/ld-uClibc.so.0 \
  -Wl,-rpath,/home/lib:/oem/usr/lib:/lib -Wl,--no-as-needed \
  "$BUILD_DIR/c220-start.o" "$BUILD_DIR/c220-media-engine.o" \
  -L"$OEM_LIB" -L"$ROOTFS_LIB" -L"$HOME_LIB" \
  "$ROOTFS_LIB/libc.so.0" -lrockit -lrkaiq -lsmartIr \
  "$HOME_LIB/libfdk-aac.so.1" -o "$OUT"
"$STRIP" -s "$OUT"
file "$OUT"
ls -la "$OUT"
