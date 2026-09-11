#!/bin/sh
set -eu

# Build both camera-side executables and stage the files that belong in /home.
# A preserved factory /home can be supplied with BASE_HOME; without it, the
# result is a minimal overlay rather than a bootable replacement image. Secure
# deployment is the default; set SECURE_DEPLOYMENT=0 only for board bring-up.
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
SECURE_DEPLOYMENT=${SECURE_DEPLOYMENT:-1}
HOME_PARTITION_BYTES=4915200

if [ -n "$BASE_HOME" ]; then
    if [ ! -d "$BASE_HOME" ]; then
        echo "BASE_HOME is not a directory: $BASE_HOME" >&2
        exit 1
    fi
fi
case "$SECURE_DEPLOYMENT" in
    0|1) ;;
    *) echo "SECURE_DEPLOYMENT must be 0 or 1" >&2; exit 1 ;;
esac
case "$REMOVE_LEGACY_MEDIA" in
    0|1) ;;
    *) echo "REMOVE_LEGACY_MEDIA must be 0 or 1" >&2; exit 1 ;;
esac
if [ "$SECURE_DEPLOYMENT" = 1 ] && [ "$REMOVE_LEGACY_MEDIA" != 1 ]; then
    echo "SECURE_DEPLOYMENT=1 requires REMOVE_LEGACY_MEDIA=1" >&2
    exit 1
fi

rm -rf "$STAGE"
mkdir -p "$STAGE/script" "$STAGE/lib"
if [ -n "$BASE_HOME" ]; then
    cp -a "$BASE_HOME"/. "$STAGE"/
    if [ "$SECURE_DEPLOYMENT" = 1 ] && [ -f "$STAGE/script/app.sh" ]; then
        "$SCRIPT_DIR/harden-preserved-app.sh" "$STAGE/script/app.sh"
    fi
fi

# `/home/nvram` must stay a real directory: the preserved boot script mounts the
# writable jffs2 NVRAM partition onto that exact path. Replacing it with a
# symlink breaks the mount (the factory rootfs has no `/nvram` to point at), and
# the camera then boots with no credentials, no Wi-Fi profile and no services.
# Mounting over a directory inside a read-only squashfs is fine, so an empty
# directory is the correct placeholder.
rm -rf "$STAGE/nvram"
mkdir -p "$STAGE/nvram"

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

if [ "$SECURE_DEPLOYMENT" = 1 ] && [ -n "$BASE_HOME" ]; then
    # The standalone stack does not use the vendor cloud/P2P applications or
    # their bundled certificate/private key. Keep tinymix because the preserved
    # boot setup still uses that hardware utility; sn_write is not needed after
    # the secure app hardening removes its SD-card device-info export.
    rm -f "$STAGE"/app/GoDaddyCA.crt \
        "$STAGE"/app/anker_t8w11 \
        "$STAGE"/app/anker_t8w11_systemd \
        "$STAGE"/app/anker_t8w11_watchdog \
        "$STAGE"/app/cert.pem \
        "$STAGE"/app/ipclink_eufy \
        "$STAGE"/app/priv.key \
        "$STAGE"/app/sn_write \
        "$STAGE"/tools/iwpriv \
        "$STAGE"/script/auto_rescue.sh \
        "$STAGE"/watchdog_cfg.json \
        "$STAGE"/lib/libaec_bf_process.so \
        "$STAGE"/lib/libcore.so \
        "$STAGE"/lib/librkaudio_common.so \
        "$STAGE"/lib/librkaudio_detect.so \
        "$STAGE"/lib/model_cfg.json
    rm -rf "$STAGE"/audio_file "$STAGE"/font "$STAGE"/logo \
        "$STAGE"/lib/model_zoo "$STAGE"/vqefiles
fi

(
    cd "$REPO_ROOT"
    # rustls' ring provider is compiled by cc-rs as part of the ARM build.
    # Use the same clang-based ABI wrapper as the Rust linker; without this,
    # cc-rs looks for an uninstalled arm-linux-musleabihf-gcc.
    CC_armv7_unknown_linux_musleabihf="${CC_armv7_unknown_linux_musleabihf:-$REPO_ROOT/native/arm-cc.sh}" \
        cargo build --release --locked --target "$TARGET" -p c220-firmware
)
cp "$REPO_ROOT/target/$TARGET/release/c220-firmware" "$STAGE/c220-firmware"
"$STRIP" -s "$STAGE/c220-firmware"

# The deployed companion is the Rust media engine; the legacy C builder remains
# available as a reference but is not part of the package anymore.
"$SCRIPT_DIR/build-rust-media-engine.sh" "$STAGE/c220-media-engine"
cp "$SCRIPT_DIR/c220-startup.sh" "$STAGE/script/c220-startup.sh"
cp "$SCRIPT_DIR/c220-wifi.sh" "$STAGE/script/c220-wifi.sh"
cp "$SCRIPT_DIR/c220-recovery.sh" "$STAGE/script/c220-recovery.sh"
cp "$SCRIPT_DIR/c220-button.sh" "$STAGE/script/c220-button.sh"
cp "$SCRIPT_DIR/c220-rescue.sh" "$STAGE/script/c220-rescue.sh"
cp "$SCRIPT_DIR/c220-factory-reset.sh" "$STAGE/script/c220-factory-reset.sh"
chmod 755 "$STAGE/c220-firmware" "$STAGE/c220-media-engine" \
    "$STAGE/script/c220-startup.sh" "$STAGE/script/c220-wifi.sh" \
    "$STAGE/script/c220-recovery.sh" "$STAGE/script/c220-button.sh" \
    "$STAGE/script/c220-rescue.sh" "$STAGE/script/c220-factory-reset.sh"

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
    # The extracted factory tree may have been unpacked by an unprivileged
    # builder. Firmware files must still be root-owned on the target.
    mksquashfs "$STAGE" "$SQUASHFS_OUT" -comp xz -b 131072 -noappend -all-root
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
