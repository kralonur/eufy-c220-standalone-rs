#!/bin/sh
set -eu

# cc-rs builds rustls' ring provider as part of an ARM firmware build. The
# camera uses the musl ARM ABI, and the host has clang rather than a named
# arm-linux-musleabihf-gcc binary.
exec clang --target=arm-linux-musleabihf "$@"
