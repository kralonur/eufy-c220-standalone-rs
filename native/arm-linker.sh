#!/bin/sh
set -eu

# Cargo supplies the ARM Rust objects and musl startup files. Link them with
# clang driving rust-lld, so the link contains exactly the startup objects rustc
# passes: a self-contained cross toolchain would add its own crt1.o and libc.
RUST_SYSROOT=$(rustc --print sysroot)
RUST_HOST=$(rustc -vV | awk '/^host: / { print $2 }')
LLD="$RUST_SYSROOT/lib/rustlib/$RUST_HOST/bin/gcc-ld/ld.lld"

if [ ! -x "$LLD" ]; then
    echo "rust-lld linker not found: $LLD" >&2
    exit 1
fi

exec clang --target=arm-linux-musleabihf \
    -Wno-unused-command-line-argument -fuse-ld="$LLD" "$@"
