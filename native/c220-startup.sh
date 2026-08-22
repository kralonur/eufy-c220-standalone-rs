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
LOG_FILE=${C220_LOG_FILE:-/tmp/c220-firmware.log}
RESTART_DELAY_SECONDS=1
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

if [ ! -x "$FIRMWARE" ]; then
    log_message "standalone firmware missing: $FIRMWARE"
    exit 1
fi
if [ ! -x "$ENGINE" ]; then
    log_message "standalone media engine missing: $ENGINE"
    exit 1
fi

# The Rust owner normally reaps this child. The kill also handles a previous
# SIGKILL or a power-loss recovery where the native companion was orphaned.
for process in c220-firmware c220-media-engine c220-daemon \
    c220-hw-rtsp c220-hw-producer c220-hw-audio c220-hw-speaker; do
    killall -9 "$process" 2>/dev/null || true
done
rm -f "$SOCKET"
ulimit -c 0 2>/dev/null || true

while :; do
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
