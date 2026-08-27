#!/bin/sh
# Shared rescue-network helper. Sourced by the button watcher and the boot
# recovery script so both agree on one definition of "the rescue network".
#
# The camera cannot host an access point - the factory rootfs ships no hostapd
# and no DHCP server - so rescue always means *joining* a known hotspot, which is
# also what the vendor firmware did. Defaults are the vendor's own rescue
# credentials so a phone hotspot created by the Eufy app works unchanged.
#
# This file defines functions only; sourcing it must have no side effects.

RESCUE_CONFIG=/home/nvram/system/standalone_rescue.conf
RESCUE_SSID_DEFAULT=RescueAP
RESCUE_PSK_DEFAULT=12345678
# Where the running supplicant reads its configuration from.
RESCUE_RUNTIME=/var/run/wpa_supplicant/wpa_supplicant.conf
# The stored copy the Wi-Fi bootstrap reads on the next boot.
RESCUE_STORED=/home/nvram/fs_config/wpa_supplicant.conf

# Read one key from the override file. The file is parsed, never sourced: a
# writable NVRAM file must not be able to run commands as root.
rescue_value_of() {
    key=$1
    [ -r "$RESCUE_CONFIG" ] || return 1
    grep -E "^[[:space:]]*${key}[[:space:]]*=" "$RESCUE_CONFIG" 2>/dev/null \
        | head -n 1 \
        | sed -e 's/^[^=]*=//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' \
            -e 's/^"//' -e 's/"$//'
}

# Reject anything that could break out of the quoted wpa_supplicant fields.
rescue_value_is_safe() {
    case "$1" in
        *'"'*|*\\*) return 1 ;;
    esac
    control_bytes=$(printf '%s' "$1" | LC_ALL=C tr -dc '\000-\037\177' | wc -c | tr -d ' ')
    [ "$control_bytes" -eq 0 ]
}

rescue_ssid() {
    ssid=$(rescue_value_of SSID) || ssid=
    if [ -z "$ssid" ] || ! rescue_value_is_safe "$ssid"; then
        ssid=$RESCUE_SSID_DEFAULT
    fi
    printf '%s' "$ssid"
}

rescue_psk() {
    psk=$(rescue_value_of PSK) || psk=
    if [ -z "$psk" ] || ! rescue_value_is_safe "$psk"; then
        psk=$RESCUE_PSK_DEFAULT
    fi
    printf '%s' "$psk"
}

# Write the rescue network as the active supplicant profile. `persist` is 1 to
# also store it in NVRAM (so the next boot reconnects) and 0 to keep it runtime
# only.
rescue_write_profile() {
    persist=${1:-0}
    ssid=$(rescue_ssid)
    psk=$(rescue_psk)
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
    } >"$RESCUE_RUNTIME" || return 1
    if [ "$persist" = 1 ]; then
        mkdir -p /home/nvram/fs_config || return 1
        cp -f "$RESCUE_RUNTIME" "$RESCUE_STORED" || return 1
    fi
    return 0
}

# Restart the link so a new profile takes effect, instead of waiting for the
# bootstrap to notice the old association still looks healthy.
rescue_restart_wifi() {
    killall wpa_supplicant_nl80211 2>/dev/null
    killall udhcpc 2>/dev/null
    return 0
}
