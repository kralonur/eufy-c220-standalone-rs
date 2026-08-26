#!/bin/sh
# Keep wlan0 associated with the saved network.
#
# The standalone image removes the factory vendor application, which used to be
# the component that started the supplicant and the DHCP client. Without this
# wrapper the camera boots with the Wi-Fi driver loaded and no client, so it
# never joins the LAN and cannot be reached or repaired over the network.
#
# The preserved /home/script/wificonnect_c.sh does the association work; this
# script supplies the stored profile and retries until an address exists. One
# failed attempt must never be final, because the driver loads asynchronously
# and the access point can be slow or briefly absent at boot.

PROFILE=/home/nvram/fs_config/wpa_supplicant.conf
RUNTIME=/var/run/wpa_supplicant/wpa_supplicant.conf
MODULE_DIR=/oem/usr/ko
LOG_FILE=/tmp/c220-wifi.log
MAX_LOG_BYTES=65536
# Longest we wait for the asynchronously loaded driver to create wlan0.
MODULE_WAIT_SECONDS=90
# Delay between association attempts and between health checks.
RETRY_SECONDS=15
# Delay before retrying while no Wi-Fi profile exists at all.
UNCONFIGURED_SECONDS=30

log_message() {
    printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG_FILE"
}

trim_log() {
    if [ -f "$LOG_FILE" ] && [ "$(wc -c <"$LOG_FILE" 2>/dev/null || echo 0)" -gt "$MAX_LOG_BYTES" ]; then
        tail -n 200 "$LOG_FILE" >"$LOG_FILE.tmp" 2>/dev/null &&
            mv -f "$LOG_FILE.tmp" "$LOG_FILE" 2>/dev/null || true
    fi
}

# Bringing up Wi-Fi deliberately does not depend on the service configuration.
# This camera is headless: the network is the only management path, so a
# missing or invalid standalone_auth.conf must not also remove the ability to
# repair it. Nothing listens on that network until the firmware accepts its own
# configuration, so there is no exposure in starting the link first.

wait_for_interface() {
    waited=0
    while [ ! -e /sys/class/net/wlan0 ]; do
        [ "$waited" -ge "$MODULE_WAIT_SECONDS" ] && return 1
        sleep 3
        waited=$((waited + 3))
    done
    return 0
}

wlan0_has_address() {
    ifconfig wlan0 2>/dev/null | grep -q 'inet addr'
}

supplicant_running() {
    ps 2>/dev/null | grep -q '[w]pa_supplicant'
}

trim_log
log_message "Wi-Fi bootstrap started"

while :; do
    if ! wait_for_interface; then
        log_message "wlan0 missing after ${MODULE_WAIT_SECONDS}s; reloading the driver"
        (cd "$MODULE_DIR" && ./insmod_wifi.sh) >>"$LOG_FILE" 2>&1
        trim_log
        sleep "$RETRY_SECONDS"
        continue
    fi

    if [ -s "$PROFILE" ]; then
        source_profile=$PROFILE
    else
        log_message "no saved Wi-Fi profile at $PROFILE; waiting for provisioning"
        sleep "$UNCONFIGURED_SECONDS"
        continue
    fi

    if wlan0_has_address && supplicant_running; then
        sleep "$RETRY_SECONDS"
        continue
    fi

    mkdir -p /var/run/wpa_supplicant
    cp -f "$source_profile" "$RUNTIME"
    log_message "connecting with $source_profile"
    /home/script/wificonnect_c.sh >>"$LOG_FILE" 2>&1
    if wlan0_has_address; then
        log_message "wlan0 up: $(ifconfig wlan0 | awk '/inet addr/ {print $2, $4}')"
    else
        log_message "no address yet after this attempt; DHCP may still be settling"
    fi
    trim_log
    sleep "$RETRY_SECONDS"
done
