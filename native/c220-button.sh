#!/bin/sh
# Physical recovery button: hold it to put the camera on a known network.
#
# Why this exists: this camera cannot host an access point - the factory rootfs
# ships no hostapd or DHCP server, so there is nothing to hand out addresses -
# and reaching it physically to insert a card is slow. The button gives a
# no-tools path that needs neither the network nor a working configuration: hold
# it, and the camera joins a known hotspot instead, with the maintenance shell
# open, so the stored Wi-Fi can be repaired from a phone or laptop.
#
# This is the vendor's rescue design: the camera joins an AP named RescueAP with
# the documented password rather than creating one. Point your phone's hotspot at
# those values, or override them in
# /home/nvram/system/standalone_rescue.conf with SSID= and PSK= lines.
#
# The board routes the button to a SARADC key ladder rather than a GPIO, which is
# why this samples the IIO channel: measured idle is 1020-1023 and a held button
# sits at 557-561, so a threshold of 800 separates them with wide margin.
set -u

ADC=/sys/bus/iio/devices/iio:device0/in_voltage0_raw
# Below this raw value the button counts as pressed.
PRESSED_BELOW=${PRESSED_BELOW:-800}
# Hold time before the rescue triggers. Deliberately longer than a stray press.
HOLD_SECONDS=${HOLD_SECONDS:-10}
# Poll interval; busybox sleep accepts fractions.
POLL_SECONDS=${POLL_SECONDS:-0.2}
# Polls per second, used to count the hold in whole seconds below.
POLLS_PER_SECOND=5
HOLD_POLLS=$((HOLD_SECONDS * POLLS_PER_SECOND))
RUNTIME=/var/run/wpa_supplicant/wpa_supplicant.conf
PROFILE=/home/nvram/fs_config/wpa_supplicant.conf
RESCUE_CONFIG=/home/nvram/system/standalone_rescue.conf
LOG_FILE=/tmp/c220-button.log
# Vendor defaults, so a phone hotspot named RescueAP just works.
DEFAULT_SSID=RescueAP
DEFAULT_PSK=12345678
# Set to 1 to detect and log without changing the network. Used to validate the
# sampling on hardware before the rescue actions are allowed to run.
DRY_RUN=${DRY_RUN:-0}

log_message() {
    printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG_FILE"
}

# Parse one key out of the optional override file. The file is data, never
# sourced: a writable NVRAM file must not be able to run commands as root.
value_of() {
    key=$1
    [ -r "$RESCUE_CONFIG" ] || return 1
    grep -E "^[[:space:]]*${key}[[:space:]]*=" "$RESCUE_CONFIG" 2>/dev/null \
        | head -n 1 \
        | sed -e 's/^[^=]*=//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' \
            -e 's/^"//' -e 's/"$//'
}

rescue_profile() {
    ssid=$(value_of SSID) || ssid=
    psk=$(value_of PSK) || psk=
    [ -n "$ssid" ] || ssid=$DEFAULT_SSID
    [ -n "$psk" ] || psk=$DEFAULT_PSK
    # Keep the values inside their quoted wpa_supplicant fields.
    case "$ssid$psk" in
        *'"'*|*\\*) log_message "rescue credentials contain quotes or backslashes; using defaults"
            ssid=$DEFAULT_SSID
            psk=$DEFAULT_PSK
            ;;
    esac
    mkdir -p /var/run/wpa_supplicant || return 1
    {
        echo 'ctrl_interface=/var/run/wpa_supplicant'
        echo 'update_config=1'
        echo 'network={'
        printf 'ssid="%s"\n' "$ssid"
        printf 'key_mgmt=WPA-PSK\npsk="%s"\n' "$psk"
        echo 'pairwise=CCMP TKIP'
        echo 'group=CCMP TKIP WEP104 WEP40'
        echo 'scan_ssid=1'
        echo '}'
    } >"$RUNTIME" || return 1
    cp -f "$RUNTIME" "$PROFILE" || return 1
    return 0
}

trigger_rescue() {
    log_message "button held for ${HOLD_SECONDS}s"
    if [ "$DRY_RUN" = 1 ]; then
        log_message "dry run: would join the rescue network and open the shell"
        return 0
    fi
    if rescue_profile; then
        log_message "rescue profile written; joining the rescue hotspot"
    else
        log_message "could not write the rescue profile"
        return 1
    fi
    # A shell for this boot only: the reader is holding the camera, which is the
    # same trust level as inserting a card.
    if [ -x /usr/sbin/telnetd ]; then
        /usr/sbin/telnetd -p 23 -l /bin/sh >/dev/null 2>&1 &
        log_message "maintenance shell started on port 23 for this boot"
    fi
    # Drop the current association so c220-wifi.sh reconnects with the profile
    # just written; otherwise a healthy link would keep the old network forever.
    killall wpa_supplicant_nl80211 2>/dev/null
    killall udhcpc 2>/dev/null
    log_message "Wi-Fi restarted for the rescue network"
    return 0
}

if [ ! -r "$ADC" ]; then
    log_message "no SARADC channel at $ADC; button recovery unavailable"
    exit 0
fi

log_message "button watcher started (dry_run=$DRY_RUN, hold=${HOLD_SECONDS}s)"

held=0
fired=0
while :; do
    raw=$(cat "$ADC" 2>/dev/null) || raw=
    if [ -n "$raw" ] && [ "$raw" -lt "$PRESSED_BELOW" ] 2>/dev/null; then
        held=$((held + 1))
        # Report progress once per second so the log shows the hold building.
        if [ $((held % POLLS_PER_SECOND)) -eq 0 ]; then
            log_message "button held for $((held / POLLS_PER_SECOND))s"
        fi
        if [ "$fired" -eq 0 ] && [ "$held" -ge "$HOLD_POLLS" ]; then
            # Once per boot: a second hold after a failed rescue must not fight
            # whatever the operator has started doing from the new shell.
            fired=1
            trigger_rescue
        fi
    else
        if [ "$held" -gt 0 ] && [ "$fired" -eq 0 ]; then
            log_message "button released before the ${HOLD_SECONDS}s hold"
        fi
        held=0
    fi
    sleep "$POLL_SECONDS"
done
