#!/bin/sh
# Host-side gate for a change: formatting, lints, tests, the ARM release build and
# the size budget. Run it before sending a change.
#
# It checks nothing about the camera. Hardware behaviour - flashing, the media
# engine against the vendor libraries, the button, Wi-Fi - cannot be verified from
# here, and a change that claims any of it needs a device test described in the
# change itself.
set -eu

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$REPO_ROOT"

TARGET=armv7-unknown-linux-musleabihf
ARTIFACT="target/$TARGET/release/c220-firmware"

# Size of the release firmware when this gate was written: 1,999,360 bytes built
# with the pinned devshell, 2,002,796 with the distribution toolchain. The budget
# is the smaller figure plus 10%, which leaves room for a dependency bump and
# still catches what this is for: the tracing-subscriber `env-filter` feature cost
# 253 KB, 11%, before it was removed.
REFERENCE_SIZE=1999360
SIZE_BUDGET=${SIZE_BUDGET:-2200000}

failed=0

step() {
    printf '\n== %s\n' "$1"
}

check() {
    label=$1
    shift
    if "$@"; then
        printf '%s: ok\n' "$label"
    else
        printf '%s: FAILED\n' "$label"
        failed=$((failed + 1))
    fi
}

step "formatting"
check "cargo fmt" cargo fmt --all -- --check

step "lints"
check "cargo clippy" cargo clippy --workspace --all-targets -- -D warnings

step "tests"
check "cargo test" cargo test --workspace

step "release build for $TARGET"
check "cargo build" cargo build --release -p c220-firmware --target "$TARGET"

step "size"
# Cargo fingerprints the compiler by its version string, and rustup's rustc 1.98.1
# and the pinned one this repository builds with report the same string. Switching
# between a host build and `nix develop`/`nix run` in one target directory
# therefore reuses objects built by the other toolchain: the binary becomes a mix
# of two compilers and the number below stops meaning anything. Record which
# toolchain built here, and say so when it changes.
stamp="target/.check-sysroot"
sysroot=$(rustc --print sysroot)
if [ -f "$stamp" ] && [ "$(cat "$stamp")" != "$sysroot" ]; then
    printf 'warning: a different toolchain built in this target directory last time\n'
    printf '  was %s\n  now %s\n' "$(cat "$stamp")" "$sysroot"
    printf '  cargo cannot tell those apart, so this size may be a mix of both.\n'
    printf '  Build releases in one environment, or `cargo clean` before measuring.\n'
fi
mkdir -p target
printf '%s\n' "$sysroot" >"$stamp"

if [ ! -f "$ARTIFACT" ]; then
    printf 'no artifact at %s: the build above did not produce one\n' "$ARTIFACT"
    failed=$((failed + 1))
else
    size=$(stat -c %s "$ARTIFACT")
    printf '%s bytes, %+d against the recorded %s\n' \
        "$size" "$((size - REFERENCE_SIZE))" "$REFERENCE_SIZE"
    if [ "$size" -gt "$SIZE_BUDGET" ]; then
        printf 'over the %s budget; the /home partition holds 4,915,200 bytes\n' "$SIZE_BUDGET"
        printf 'find what grew with: cargo bloat --release --target %s --crates\n' "$TARGET"
        failed=$((failed + 1))
    else
        printf 'size: ok\n'
    fi
fi

printf '\n'
if [ "$failed" -eq 0 ]; then
    printf 'CHECK OK - nothing here was verified on a camera\n'
else
    printf 'CHECK FAILED: %s step(s)\n' "$failed"
fi
exit "$failed"
