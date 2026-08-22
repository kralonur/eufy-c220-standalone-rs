#!/bin/sh
set -eu

# Cargo supplies the ARM Rust objects and musl startup files. Link them with
# rust-lld directly instead of Zig's driver, which would add a second crt1.o.
RUST_SYSROOT=$(rustc --print sysroot)
RUST_HOST=$(rustc -vV | awk '/^host: / { print $2 }')
LLD="$RUST_SYSROOT/lib/rustlib/$RUST_HOST/bin/gcc-ld/ld.lld"

if [ ! -x "$LLD" ]; then
    echo "rust-lld linker not found: $LLD" >&2
    exit 1
fi

exec clang --target=arm-linux-musleabihf \
    -Wno-unused-command-line-argument -fuse-ld="$LLD" "$@"
