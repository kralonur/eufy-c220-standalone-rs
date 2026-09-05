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
OEM_LIB="$CAMERA_ROOT/rootfs/oem/usr/lib"
HOME_LIB="$CAMERA_ROOT/home/lib"
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
# `libgcc_s` is named for the same reason a C toolchain would pass `-lgcc_s`: the
# precompiled `core` and `alloc` this binary uses carry landing pads that
# reference the unwinder's entry points, even though nothing in this process can
# unwind (the panic handler aborts). The camera's own copy is used, which is the
# one the vendor libraries already load.
#
# The vendor libraries are named for the same reason the reference engine links
# them: the engine calls into them, and they are not on a library path a
# cross-link can find by accident. `--as-needed`, which rustc passes, is what
# keeps a library out of NEEDED (and its constructors from running) until the
# engine actually references one of its symbols.
#
# -e _start selects the engine's own entry stub, the dynamic loader and library
# search path are the ones the reference engine is deployed with, and -no-pie
# overrides the -pie rustc asks for: the stub loads the entry address absolutely,
# like the reference engine, which is also a non-PIE executable.
exec clang --target=arm-linux-musleabihf -fuse-ld="$LLD" \
    -nostartfiles -nodefaultlibs \
    -L"$SHIM" -L"$ROOTFS_LIB" -L"$OEM_LIB" -L"$HOME_LIB" \
    -Wl,-e,_start \
    -Wl,--dynamic-linker=/lib/ld-uClibc.so.0 \
    -Wl,-rpath,/home/lib:/oem/usr/lib:/lib \
    "$@" \
    -no-pie \
    -l:libc.so.0 -l:libgcc_s.so.1 -lrockit -lrkaiq -lsmartIr -l:libfdk-aac.so.1
