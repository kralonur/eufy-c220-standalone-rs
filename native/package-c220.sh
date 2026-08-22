#!/bin/sh
set -eu

# Build both camera-side executables and stage the files that belong in /home.
# A preserved factory /home can be supplied with BASE_HOME; without it, the
# result is a minimal overlay rather than a bootable replacement image.
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
TARGET=${TARGET:-armv7-unknown-linux-musleabihf}
STAGE=${1:-"${STAGE:-$REPO_ROOT/dist/c220-home}"}
BASE_HOME=${BASE_HOME:-}
case "$STAGE" in
    /*) ;;
    *) STAGE="$REPO_ROOT/$STAGE" ;;
esac
STRIP=${STRIP:-llvm-strip}
SQUASHFS_OUT=${SQUASHFS_OUT:-}
INSTALL_STARTUP_HOOK=${INSTALL_STARTUP_HOOK:-0}
REMOVE_LEGACY_MEDIA=${REMOVE_LEGACY_MEDIA:-1}
HOME_PARTITION_BYTES=4915200

if [ -n "$BASE_HOME" ]; then
    if [ ! -d "$BASE_HOME" ]; then
        echo "BASE_HOME is not a directory: $BASE_HOME" >&2
        exit 1
    fi
fi

rm -rf "$STAGE"
mkdir -p "$STAGE/script" "$STAGE/lib"
if [ -n "$BASE_HOME" ]; then
    cp -a "$BASE_HOME"/. "$STAGE"/
fi

if [ "$REMOVE_LEGACY_MEDIA" = 1 ]; then
    # These standalone helper names belong to the superseded split daemon;
    # retaining them wastes /home space and can cause an old process to race
    # the unified companion.
    rm -f "$STAGE"/c220-daemon \
        "$STAGE"/c220-hw-rtsp \
        "$STAGE"/c220-hw-producer \
        "$STAGE"/c220-hw-audio \
        "$STAGE"/c220-hw-speaker
fi

(
    cd "$REPO_ROOT"
    cargo build --release --locked --target "$TARGET"
)
cp "$REPO_ROOT/target/$TARGET/release/c220-firmware" "$STAGE/c220-firmware"
"$STRIP" -s "$STAGE/c220-firmware"

"$SCRIPT_DIR/build-c220-media-engine.sh" "$STAGE/c220-media-engine"
cp "$SCRIPT_DIR/c220-startup.sh" "$STAGE/script/c220-startup.sh"
chmod 755 "$STAGE/c220-firmware" "$STAGE/c220-media-engine" "$STAGE/script/c220-startup.sh"

# These are the only files from the local factory bundle that belong in /home.
# The loader, libc, Rockchip, and C++ libraries stay in their preserved factory
# rootfs locations and are linked by the companion through its camera RPATH.
cp -a "$SCRIPT_DIR/camera-rootfs/home/lib/libfdk-aac.so.1" "$STAGE/lib/"
cp -a "$SCRIPT_DIR/camera-rootfs/home/os03b10_CMK-OT2117-PC1_30IRC-F16.bin" "$STAGE/"

if [ "$INSTALL_STARTUP_HOOK" = 1 ]; then
    if [ -z "$BASE_HOME" ] || [ ! -f "$STAGE/script/app.sh" ]; then
        echo "INSTALL_STARTUP_HOOK=1 requires BASE_HOME with script/app.sh" >&2
        exit 1
    fi
    "$SCRIPT_DIR/install-startup-hook.sh" "$STAGE/script/app.sh"
fi

if [ -n "$SQUASHFS_OUT" ]; then
    if [ -z "$BASE_HOME" ]; then
        echo "SQUASHFS_OUT requires BASE_HOME so factory /home files are preserved" >&2
        exit 1
    fi
    mkdir -p "$(dirname -- "$SQUASHFS_OUT")"
    mksquashfs "$STAGE" "$SQUASHFS_OUT" -comp xz -b 131072 -noappend
    squashfs_size=$(stat -c '%s' "$SQUASHFS_OUT")
    if [ "$squashfs_size" -gt "$HOME_PARTITION_BYTES" ]; then
        echo "refusing oversized /home image: $squashfs_size > $HOME_PARTITION_BYTES bytes" >&2
        exit 1
    fi
fi

printf 'staged camera /home files in %s\n' "$STAGE"
find "$STAGE" -maxdepth 2 -type f -o -type l | sort
if [ -n "$SQUASHFS_OUT" ]; then
    file "$SQUASHFS_OUT"
    ls -lh "$SQUASHFS_OUT"
fi
