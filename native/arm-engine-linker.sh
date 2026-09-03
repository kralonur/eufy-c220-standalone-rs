#!/bin/sh
set -eu

# Linker for the Rust media engine.
#
# The engine runs on the camera's own uClibc, because the Rockchip and FDK
# libraries that drive the hardware live in that process, so it cannot use
# musl's static CRT: its entry stub calls __uClibc_main and libc comes from the
# camera rootfs. The build therefore passes -C target-feature=-crt-static, and
# this script refuses the static form instead of producing a binary that would
# fail on the camera.
#
# rustc gives GNU linker options the -Wl, prefix for this target, so they are
# forwarded untouched.
#
# C220_UCLIBC_SHIM must point at a directory containing libc.so and libgcc_s.so
# symlinks to the camera's uClibc objects, which is how rustc's -lc and -lgcc_s
# resolve to the camera's libc instead of musl's.

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
CAMERA_ROOT=${CAMERA_ROOT:-"$REPO_ROOT/native/camera-rootfs"}
ROOTFS_LIB="$CAMERA_ROOT/rootfs/lib"
SHIM=${C220_UCLIBC_SHIM:-}

for argument in "$@"; do
    case "$argument" in
        -static|*/crt1.o|*/crti.o|*/crtbegin.o|*/crtend.o|*/crtn.o)
            echo "arm-engine-linker: refusing to link musl's static CRT into the engine" >&2
            echo "build it with -C target-feature=-crt-static (native/build-rust-media-engine.sh)" >&2
            exit 1
            ;;
    esac
done

if [ ! -e "$ROOTFS_LIB/libc.so.0" ]; then
    echo "arm-engine-linker: missing $ROOTFS_LIB/libc.so.0" >&2
    echo "set CAMERA_ROOT to an extracted C220 rootfs" >&2
    exit 1
fi
if [ -z "$SHIM" ] || [ ! -e "$SHIM/libc.so" ]; then
    echo "arm-engine-linker: C220_UCLIBC_SHIM must point at a directory with libc.so" >&2
    echo "and libgcc_s.so symlinks; native/build-rust-media-engine.sh creates it" >&2
    exit 1
fi

RUST_SYSROOT=$(rustc --print sysroot)
RUST_HOST=$(rustc -vV | awk '/^host: / { print $2 }')
LLD="$RUST_SYSROOT/lib/rustlib/$RUST_HOST/bin/gcc-ld/ld.lld"

if [ ! -x "$LLD" ]; then
    echo "arm-engine-linker: rust-lld not found at $LLD" >&2
    exit 1
fi

# The engine is `no_std`, so nothing else links libc for it: the libc crate's
# default `std` feature assumes `std` does, and musl's CRT objects are not part of
# this link. The camera's own uClibc is therefore named here, which is also what
# the reference engine does.
#
# -e _start selects the engine's own entry stub, the dynamic loader and library
# search path are the ones the reference engine is deployed with, and -no-pie
# overrides the -pie rustc asks for: the stub loads the entry address absolutely,
# like the reference engine, which is also a non-PIE executable.
exec clang --target=arm-linux-musleabihf -fuse-ld="$LLD" \
    -nostartfiles -nodefaultlibs \
    -L"$SHIM" -L"$ROOTFS_LIB" \
    -Wl,-e,_start \
    -Wl,--dynamic-linker=/lib/ld-uClibc.so.0 \
    -Wl,-rpath,/home/lib:/oem/usr/lib:/lib \
    "$@" \
    -no-pie \
    -l:libc.so.0
