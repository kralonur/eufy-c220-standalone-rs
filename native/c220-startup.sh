#!/bin/sh
# Launch the standalone Rust service after the factory boot script has
# initialized networking, GPIO, kernel modules, and the sensor stack.
#
# This script deliberately does not replace the factory bootloader, kernel, or
# init sequence. Install it as the final service step in the preserved
# /home/script/app.sh.

FIRMWARE=/home/c220-firmware
ENGINE=/home/c220-media-engine
SOCKET=/tmp/c220-media.sock
SECURITY_CONFIG=/home/nvram/system/standalone_auth.conf
INSECURE_MARKER=/home/nvram/system/.standalone_insecure_bringup
LOG_FILE=/tmp/c220-firmware.log
RESTART_DELAY_SECONDS=1
umask 077
MAX_LOG_BYTES=524288

log_message() {
    printf '%s\n' "$*" >>"$LOG_FILE"
}

trim_log() {
    if [ -f "$LOG_FILE" ] && [ "$(wc -c <"$LOG_FILE" 2>/dev/null || echo 0)" -gt "$MAX_LOG_BYTES" ]; then
        tail -n 1000 "$LOG_FILE" >"$LOG_FILE.tmp" 2>/dev/null &&
            mv -f "$LOG_FILE.tmp" "$LOG_FILE" 2>/dev/null || true
    fi
}

private_file_permissions_ok() {
    file=$1
    # The target BusyBox does not provide `stat`; parse only the stable mode,
    # UID, and GID columns emitted by `ls -ldn` instead.
    listing=$(ls -ldn "$file" 2>/dev/null) || return 1
    set -- $listing
    [ "$#" -ge 4 ] || return 1
    [ "$3" = 0 ] && [ "$4" = 0 ] || return 1
    case "$1" in
        -r--------|-rw-------) return 0 ;;
        *) return 1 ;;
    esac
}

# A per-board label, so two cameras never answer to the same name and the name
# survives a factory reset (which keeps the vendor identity but removes everything
# this deployment provisioned). The vendor PCB serial is preferred because it is the
# serial printed on the camera, so the advertised name can be matched to the device
# in your hand; the SoC serial in /proc/cpuinfo survives a wiped NVRAM, and the Wi-Fi
# MAC is the last resort because it always exists.
board_label() {
    for candidate in \
        "$(cat /home/nvram/system/.pcbsn 2>/dev/null)" \
        "$(sed -n 's/^Serial[[:space:]]*:[[:space:]]*//p' /proc/cpuinfo 2>/dev/null | head -n 1)" \
        "$(cat /sys/class/net/wlan0/address 2>/dev/null | tr -d ':')"
    do
        label=$(printf '%s' "$candidate" | tr 'A-Z' 'a-z' | tr -cd 'a-z0-9' | cut -c1-40)
        if [ -n "$label" ]; then
            printf '%s' "$label"
            return 0
        fi
    done
    return 1
}

consume_insecure_marker() {
    if ! private_file_permissions_ok "$INSECURE_MARKER"; then
        log_message "insecure permissions on bring-up marker; refusing it"
        rm -f "$INSECURE_MARKER"
        return 1
    fi
    rm -f "$INSECURE_MARKER"
    return 0
}

if [ ! -x "$FIRMWARE" ]; then
    log_message "standalone firmware missing: $FIRMWARE"
    exit 1
fi
if [ ! -x "$ENGINE" ]; then
    log_message "standalone media engine missing: $ENGINE"
    exit 1
fi

# NVRAM carries every credential, the TLS key, and the Wi-Fi profile. The
# preserved boot script mounts it at /home/nvram; if that mount did not happen,
# the only symptom is a camera that silently has no network and no listeners, so
# state it plainly here instead of letting it look like a dead device.
if ! grep -qs ' /home/nvram ' /proc/mounts; then
    log_message "WARNING: /home/nvram is not mounted; credentials and Wi-Fi profile are unavailable"
fi

# The Rust owner normally reaps this child. The kill also handles a previous
# SIGKILL or a power-loss recovery where the native companion was orphaned.
# `telnetd` is deliberately NOT in this list: nothing survives a reboot, so
# killing it here only ever destroyed a maintenance shell that the card recovery
# had just started for this boot.
for process in c220-firmware c220-media-engine c220-daemon \
    c220-hw-rtsp c220-hw-producer c220-hw-audio c220-hw-speaker; do
    killall -9 "$process" 2>/dev/null || true
done
rm -f "$SOCKET"
ulimit -c 0 2>/dev/null || true

# The vendor application that used to start the Wi-Fi supplicant is removed
# from this image, so without this the camera would boot with a loaded driver
# and no network. The bootstrap retries on its own and must never block the
# firmware supervisor, so it runs in the background.
if [ -x /home/script/c220-wifi.sh ]; then
    /home/script/c220-wifi.sh &
fi

# The physical button is a recovery path: it has to work when the network, the
# configuration or the firmware are broken, so it is started here, outside the
# configuration loop, and never blocks anything.
if [ -x /home/script/c220-button.sh ]; then
    /home/script/c220-button.sh &
fi

# The board's name is derived once: it identifies the hardware, so it cannot change
# while this process runs, and reading it every iteration would only repeat work.
if DERIVED_HOST_LABEL=$(board_label); then
    log_message "camera name derived from the board identity"
else
    DERIVED_HOST_LABEL=""
    log_message "no board identity found; the camera will use the default name"
fi

# Credentials, ports, and certificate paths live on writable NVRAM. Re-read the
# file for every supervisor iteration so an administrator can repair a bad
# configuration and reboot/restart without rebuilding the SquashFS image.
while :; do
    unset RTSP_USERNAME RTSP_PASSWORD_FILE HTTP_USERNAME HTTP_PASSWORD_FILE
    unset RTSP_PORT RTSPS_PORT RTSP_CERT_FILE RTSP_KEY_FILE API_TLS NTP_HOST
    unset STANDALONE_RTSP_USERNAME STANDALONE_RTSP_PASSWORD_FILE
    unset STANDALONE_HTTP_USERNAME STANDALONE_HTTP_PASSWORD_FILE
    unset STANDALONE_RTSP_PORT STANDALONE_RTSPS_PORT
    unset STANDALONE_RTSP_CERT_FILE STANDALONE_RTSP_KEY_FILE STANDALONE_API_TLS
    unset STANDALONE_NTP_HOST STANDALONE_HOST_NAME STANDALONE_INSECURE_BRINGUP
    unset HOST_NAME
    insecure_bringup=0
    if [ -r "$SECURITY_CONFIG" ]; then
        # Reject a writable/non-root config before the shell can source it.
        if ! private_file_permissions_ok "$SECURITY_CONFIG"; then
            log_message "insecure permissions on standalone security config; refusing to source it"
            trim_log
            sleep "$RESTART_DELAY_SECONDS"
            continue
        fi
        # This file is root-owned configuration, not untrusted request data.
        if ! . "$SECURITY_CONFIG"; then
            log_message "invalid standalone security config: $SECURITY_CONFIG"
            trim_log
            sleep "$RESTART_DELAY_SECONDS"
            continue
        fi

        # Only the prefixed names above are accepted from NVRAM. Discard any
        # direct service variables a stale shell environment or config tried to
        # inject before translating the documented settings.
        unset RTSP_USERNAME RTSP_PASSWORD_FILE HTTP_USERNAME HTTP_PASSWORD_FILE
        unset RTSP_PORT RTSPS_PORT RTSP_CERT_FILE RTSP_KEY_FILE API_TLS NTP_HOST
        unset STANDALONE_INSECURE_BRINGUP
        if [ -f "$INSECURE_MARKER" ]; then
            # A physical/local bring-up override is deliberately one boot only.
            if consume_insecure_marker; then
                insecure_bringup=1
            fi
        fi
        [ -n "${STANDALONE_RTSP_USERNAME:-}" ] && \
            export RTSP_USERNAME="$STANDALONE_RTSP_USERNAME"
        [ -n "${STANDALONE_RTSP_PASSWORD_FILE:-}" ] && \
            export RTSP_PASSWORD_FILE="$STANDALONE_RTSP_PASSWORD_FILE"
        [ -n "${STANDALONE_HTTP_USERNAME:-}" ] && \
            export HTTP_USERNAME="$STANDALONE_HTTP_USERNAME"
        [ -n "${STANDALONE_HTTP_PASSWORD_FILE:-}" ] && \
            export HTTP_PASSWORD_FILE="$STANDALONE_HTTP_PASSWORD_FILE"
        if [ "$insecure_bringup" = 1 ]; then
            # Board bring-up: the one-boot marker is the only thing that may
            # retire TLS and reopen plaintext RTSP, so the values are fixed here
            # and cannot come from a configuration file.
            export RTSP_PORT=554
            export RTSPS_PORT="${STANDALONE_RTSPS_PORT:-0}"
            export API_TLS=0
            export STANDALONE_INSECURE_BRINGUP=1
        else
            # Secure mode: plaintext RTSP is retired and the control plane must
            # be TLS, so neither is configurable. Leaving these two as settings
            # only created a way to make the firmware refuse to start; a stale
            # STANDALONE_RTSP_PORT or STANDALONE_API_TLS is now ignored.
            export RTSP_PORT=0
            export RTSPS_PORT="${STANDALONE_RTSPS_PORT:-322}"
            export API_TLS=1
        fi
        [ -n "${STANDALONE_RTSP_CERT_FILE:-}" ] && \
            export RTSP_CERT_FILE="$STANDALONE_RTSP_CERT_FILE"
        [ -n "${STANDALONE_RTSP_KEY_FILE:-}" ] && \
            export RTSP_KEY_FILE="$STANDALONE_RTSP_KEY_FILE"
        # Optional LAN time server. The default gateway is tried anyway, but many
        # routers do not answer SNTP, so an explicit host is what actually sets
        # this board's clock on such a network.
        [ -n "${STANDALONE_NTP_HOST:-}" ] && \
            export NTP_HOST="$STANDALONE_NTP_HOST"
        # The camera's own name: what it answers for over mDNS, and the name a
        # generated certificate is issued for. Two cameras need two names, so an
        # explicit setting wins and the board identity fills in otherwise.
        if [ -n "${STANDALONE_HOST_NAME:-}" ]; then
            export HOST_NAME="$STANDALONE_HOST_NAME"
        elif [ -n "$DERIVED_HOST_LABEL" ]; then
            export HOST_NAME="camera-$DERIVED_HOST_LABEL"
        fi
    elif [ -f "$INSECURE_MARKER" ]; then
        # No security file is accepted only with this explicit one-boot marker.
        if ! consume_insecure_marker; then
            trim_log
            sleep "$RESTART_DELAY_SECONDS"
            continue
        fi
        export STANDALONE_INSECURE_BRINGUP=1
        export RTSP_PORT=554 RTSPS_PORT=0 API_TLS=0
        log_message "security config absent; explicit insecure bring-up marker consumed"
    else
        log_message "security config missing; refusing to start network services"
        trim_log
        sleep "$RESTART_DELAY_SECONDS"
        continue
    fi

    log_message "starting standalone c220-firmware"
    "$FIRMWARE" >>"$LOG_FILE" 2>&1
    status=$?
    log_message "c220-firmware exited status=$status; restarting"
    for process in c220-firmware c220-media-engine c220-daemon \
        c220-hw-rtsp c220-hw-producer c220-hw-audio c220-hw-speaker; do
        killall -9 "$process" 2>/dev/null || true
    done
    rm -f "$SOCKET"
    trim_log
    sleep "$RESTART_DELAY_SECONDS"
done
