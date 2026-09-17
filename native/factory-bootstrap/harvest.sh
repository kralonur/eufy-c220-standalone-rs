#!/bin/sh
# Collect the vendor build inputs that this repository does not redistribute.
#
# Run this on the camera, from the root shell the card opens (native/factory-bootstrap/iq.sh),
# and it writes everything the build needs onto the same card:
#
#   <dest>/camera-rootfs/   the ARM libraries and ISP tuning data the media
#                           engine links against, and the sensor blob the
#                           package script ships (this is CAMERA_ROOT)
#   <dest>/base-home/       the factory /home tree, which package-c220.sh takes
#                           as BASE_HOME
#
# The live NVRAM is deliberately not copied. It holds the camera's credentials
# and Wi-Fi profile, the packaging script replaces it with an empty directory
# anyway, and iq.sh already keeps a backup of it.
#
# The factory /home tree is an input, not an output: building from this firmware's
# own /home would fold our files back in as if they were the vendor's, and the
# image would be missing the vendor boot and Wi-Fi helpers it needs. The harvest
# detects that case and leaves BASE_HOME alone; take it from a camera that is
# still factory, or from your own full-chip dump on a computer.
#
# Usage, in the telnet shell:
#   sh /mnt/sdcard/harvest.sh [destination]

set -u

DEST=${1:-${HARVEST_DIR:-/mnt/sdcard/c220-harvest}}
# SRC_ROOT prefixes every source path so this can be exercised against an
# extracted rootfs off the camera. Leave it unset on the device.
SRC_ROOT=${SRC_ROOT:-}

ROOTFS="$DEST/camera-rootfs"
BASE_HOME="$DEST/base-home"
LOG="$DEST/harvest.log"
failures=0
base_home_skipped=0

log() {
    echo "$*"
    echo "$*" >>"$LOG"
}

copy() {
    source_path="$SRC_ROOT$1"
    if [ ! -e "$source_path" ] && [ ! -L "$source_path" ]; then
        log "MISSING  $1"
        failures=$((failures + 1))
        return
    fi
    cp -a "$source_path" "$2/" || {
        log "FAILED   $1"
        failures=$((failures + 1))
    }
}

# The layout mirrors the camera so the result is usable as CAMERA_ROOT as-is.
mkdir -p "$DEST" \
    "$ROOTFS/rootfs/lib" \
    "$ROOTFS/rootfs/oem/usr/lib" \
    "$ROOTFS/rootfs/oem/usr/share/iqfiles" \
    "$ROOTFS/rootfs/etc" \
    "$ROOTFS/home/lib" \
    "$BASE_HOME"

log "harvesting vendor inputs from $SRC_ROOT/ into $DEST"

# The camera's own uClibc, its loader and libstdc++. The engine links the first
# two directly (native/arm-engine-linker.sh) and the vendor libraries need the
# rest at run time; the symlinks are kept because the loader resolves by soname.
for name in ld-uClibc-1.0.31.so ld-uClibc.so.0 ld-uClibc.so.1 libc.so.0 \
    libgcc_s.so.1 libstdc++.so.6 libstdc++.so.6.0.25 libuClibc-1.0.31.so; do
    copy "/lib/$name" "$ROOTFS/rootfs/lib"
done

# Rockchip media and ISP libraries, which the engine calls through FFI.
for name in librockit.so librkaiq.so libsmartIr.so librga.so \
    librockchip_mpp.so librockchip_mpp.so.0 librockchip_mpp.so.1; do
    copy "/oem/usr/lib/$name" "$ROOTFS/rootfs/oem/usr/lib"
done

# The ISP tuning bundle. /etc/iqfiles is a symlink into /oem and is kept as one,
# so the copy stays the same shape as the camera's own rootfs.
copy "/etc/iqfiles" "$ROOTFS/rootfs/etc"
copy "/oem/usr/share/iqfiles/cac_map_hw_2560x1440.bin" \
    "$ROOTFS/rootfs/oem/usr/share/iqfiles"

# The AAC decoder and the sensor calibration blob that package-c220.sh stages
# into the image. The blob name carries the sensor revision, so it is globbed;
# package-c220.sh expects os03b10_CMK-OT2117-PC1_30IRC-F16.bin. The glob is
# prefixed with SRC_ROOT so it expands inside the tree being harvested rather
# than in the running shell's own /home.
copy "/home/lib/libfdk-aac.so.1" "$ROOTFS/home/lib"
for blob_path in "$SRC_ROOT"/home/os03b10_*.bin; do
    copy "${blob_path#"$SRC_ROOT"}" "$ROOTFS/home"
done

# The factory /home tree: the boot script, the Wi-Fi helper, tinymix, the
# region tables and the rest of the preserved files package-c220.sh builds on.
# NVRAM is the one excluded entry, and it is a mount point in any case. As with
# the blob above, the glob is prefixed so that it expands in the harvested tree.
#
# These two names only exist once this firmware is installed; the superseded
# c220-daemon and c220-hw-* helpers are deliberately not used as markers, because
# an older install of ours carries those too.
not_factory=""
for marker in /home/c220-firmware /home/script/c220-startup.sh; do
    if [ -e "$SRC_ROOT$marker" ]; then
        not_factory="$not_factory $marker"
    fi
done
if [ -n "$not_factory" ]; then
    base_home_skipped=1
    failures=$((failures + 1))
    log "NOT A FACTORY /home:$not_factory"
    log "  this camera already runs this firmware, so /home is our tree and not the"
    log "  vendor's. Copying it in as BASE_HOME would produce an image missing the"
    log "  vendor boot and Wi-Fi helpers, so BASE_HOME was left empty."
    log "  Take it from a camera that is still on factory firmware, or from your own"
    log "  full-chip dump on a computer (offsets are in docs/camera-internals.md):"
    log "    dd if=<virgin-dump>.bin of=mtd6-home.bin bs=1 skip=\$((0x9d0000)) count=4915200"
    log "    unsquashfs -d base-home mtd6-home.bin"
else
    copy "/home/.appver" "$BASE_HOME"
    for entry_path in "$SRC_ROOT"/home/*; do
        entry=${entry_path#"$SRC_ROOT"}
        case "$entry" in
            /home/nvram) continue ;;
        esac
        copy "$entry" "$BASE_HOME"
    done
    # The Wi-Fi bootstrap in this firmware runs the vendor's association helper,
    # so a tree without it produces an image that cannot join a network.
    for marker in /home/script/app.sh /home/script/wificonnect_c.sh; do
        if [ ! -e "$SRC_ROOT$marker" ]; then
            log "MISSING  $marker - is this the factory /home?"
            failures=$((failures + 1))
        fi
    done
fi

# Verify against what the build scripts refuse to start without, so a card that
# comes back incomplete says so here rather than three commands later.
for required in \
    "$ROOTFS/rootfs/lib/libc.so.0" \
    "$ROOTFS/rootfs/lib/libgcc_s.so.1" \
    "$ROOTFS/rootfs/oem/usr/lib/librockit.so" \
    "$ROOTFS/rootfs/oem/usr/lib/librkaiq.so" \
    "$ROOTFS/rootfs/oem/usr/lib/libsmartIr.so" \
    "$ROOTFS/home/lib/libfdk-aac.so.1"; do
    if [ ! -e "$required" ]; then
        log "INCOMPLETE  missing $required"
        failures=$((failures + 1))
    fi
done
if [ "$base_home_skipped" -eq 0 ] && [ ! -e "$BASE_HOME/script/app.sh" ]; then
    log "INCOMPLETE  missing $BASE_HOME/script/app.sh"
    failures=$((failures + 1))
fi

log ""
log "free space on the card: $(df -h "$DEST" 2>/dev/null | tail -n 1)"
if [ "$failures" -eq 0 ]; then
    log "HARVEST OK"
    log ""
    log "next, on the computer:"
    log "  1. copy camera-rootfs/ to native/camera-rootfs/ in this repository"
    log "  2. build with BASE_HOME=<card>/base-home and CAMERA_ROOT=<repo>/native/camera-rootfs"
    log "  3. copy dist/c220-home.squashfs back to this card and write it as"
    log "     described in docs/camera-internals.md, then remove the card"
else
    log "HARVEST INCOMPLETE: $failures problem(s), see the lines above"
    if [ "$base_home_skipped" -eq 1 ]; then
        log "camera-rootfs/ is still usable; BASE_HOME is the part that is missing"
    fi
fi

exit "$failures"
