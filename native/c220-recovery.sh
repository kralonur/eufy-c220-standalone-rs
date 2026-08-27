#!/bin/sh
# Bounded SD-card recovery for a camera that cannot be reached any other way.
#
# Why this exists: this camera is headless and its only management path is the
# network. Physical access is slow and was the cause of two lockouts, so the
# device keeps one narrow, card-based path that works with no network, no valid
# configuration, and no running service. It deliberately does NOT look at
# anything else on the card: no scripts are executed, nothing from NVRAM is
# touched, and there are no vendor factory/SD script hooks.
#
# The card is read-only and the two recognised files are data:
#
#   wifi.txt         SSID=... PSK=... MODE=WPA|WEP|NONE
#                    Writes the supplicant profile the Wi-Fi bootstrap uses, so
#                    the camera can be moved to a new network with no console.
#   maintenance.txt  Starts the root shell on port 23 for this boot only.
#
# Security: whoever can insert a card can open a root shell or point the camera
# at another network. That is the same physical access this device already
# cannot defend against, so the card slot is a credential - keep it somewhere
# the camera's credentials deserve.
set -u

SD_MOUNT=/tmp/sd
# A card may be a partitioned volume (mmcblk1p1) or a bare filesystem on the
# whole device (mmcblk1), and the slot it lands on is not guaranteed, so every
# plausible name is tried instead of assuming one.
SD_DEVICES="/dev/mmcblk1p1 /dev/mmcblk1 /dev/mmcblk0p1 /dev/mmcblk0"
# The MMC controller can enumerate the card a little after this runs, so keep
# looking for a while rather than skipping recovery for the whole boot.
CARD_WAIT_SECONDS=20
WIFI_TXT="$SD_MOUNT/wifi.txt"
MAINTENANCE_TXT="$SD_MOUNT/maintenance.txt"
PROFILE=/home/nvram/fs_config/wpa_supplicant.conf
RUNTIME=/var/run/wpa_supplicant/wpa_supplicant.conf
SECURITY_CONFIG=/home/nvram/system/standalone_auth.conf
LOG_FILE=/tmp/c220-recovery.log

# Shared definition of the rescue network, used for the unprovisioned state below.
RESCUE_LIB=${RESCUE_LIB:-/home/script/c220-rescue.sh}
. "$RESCUE_LIB"

log_message() {
    printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG_FILE"
}

# Read one key from the card's data file. The file is parsed, never sourced:
# sourcing it would let a card run arbitrary commands as root.
value_of() {
    key=$1
    grep -E "^[[:space:]]*${key}[[:space:]]*=" "$WIFI_TXT" 2>/dev/null \
        | head -n 1 \
        | sed -e 's/^[^=]*=//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' \
            -e 's/^"//' -e 's/"$//'
}

# Reject values that could break out of the quoted wpa_supplicant fields or
# inject another directive. Quotes and backslashes are refused outright; control
# characters are refused by counting them. The count is compared as a number
# because command substitution strips trailing newlines, which would otherwise
# make a value ending in a newline look empty. Bytes above ASCII are allowed so
# a legitimate non-ASCII SSID still works.
value_is_safe() {
    case "$1" in
        *'"'*|*\\*) return 1 ;;
    esac
    # Count the control characters; the value is safe when there are none.
    control_bytes=$(printf '%s' "$1" | LC_ALL=C tr -dc '\000-\037\177' | wc -c | tr -d ' ')
    [ "$control_bytes" -eq 0 ]
}

write_profile() {
    mode=$1
    ssid=$2
    psk=$3
    mkdir -p /var/run/wpa_supplicant || return 1
    {
        echo 'ctrl_interface=/var/run/wpa_supplicant'
        echo 'update_config=1'
        echo 'network={'
        printf 'ssid="%s"\n' "$ssid"
        case "$mode" in
            WPA)
                printf 'key_mgmt=WPA-PSK\npsk="%s"\n' "$psk"
                echo 'pairwise=CCMP TKIP'
                echo 'group=CCMP TKIP WEP104 WEP40'
                ;;
            WEP)
                printf 'key_mgmt=NONE\nwep_key0="%s"\nwep_tx_keyidx=0\n' "$psk"
                ;;
            NONE)
                echo 'key_mgmt=NONE'
                ;;
        esac
        echo 'scan_ssid=1'
        echo '}'
    } >"$RUNTIME" || return 1
    # The bootstrap reads the NVRAM copy, so that is the one that must survive.
    cp -f "$RUNTIME" "$PROFILE" || return 1
    return 0
}

apply_wifi_file() {
    ssid=$(value_of SSID)
    psk=$(value_of PSK)
    mode=$(value_of MODE)
    [ -n "$mode" ] || mode=WPA
    mode=$(printf '%s' "$mode" | tr 'a-z' 'A-Z')

    case "$mode" in
        WPA|WEP|NONE) ;;
        *) log_message "wifi.txt rejected: MODE must be WPA, WEP or NONE"; return 1 ;;
    esac
    [ -n "$ssid" ] || { log_message "wifi.txt rejected: SSID is empty"; return 1; }
    [ "${#ssid}" -le 32 ] || { log_message "wifi.txt rejected: SSID is longer than 32 characters"; return 1; }
    if ! value_is_safe "$ssid" || ! value_is_safe "$psk"; then
        log_message "wifi.txt rejected: control characters, quotes and backslashes are not allowed"
        return 1
    fi
    if [ "$mode" != "NONE" ]; then
        [ -n "$psk" ] || { log_message "wifi.txt rejected: PSK is required for MODE=$mode"; return 1; }
        [ "${#psk}" -le 64 ] || { log_message "wifi.txt rejected: PSK is longer than 64 characters"; return 1; }
        if [ "$mode" = "WPA" ] && [ "${#psk}" -lt 8 ]; then
            log_message "wifi.txt rejected: a WPA PSK is 8 to 63 characters"
            return 1
        fi
    fi

    if write_profile "$mode" "$ssid" "$psk"; then
        log_message "wifi.txt applied: SSID=$ssid MODE=$mode"
    else
        log_message "wifi.txt could not be written to $PROFILE"
        return 1
    fi
    return 0
}

mkdir -p "$SD_MOUNT"

# Read-only, noexec, nosuid: the card supplies data and nothing else.
# /proc/mounts is used instead of `mountpoint`, which this BusyBox may not ship.
mount_card() {
    waited=0
    while :; do
        for device in $SD_DEVICES; do
            [ -b "$device" ] || continue
            if mount -t vfat -o ro,nosuid,nodev,noexec "$device" "$SD_MOUNT" 2>>"$LOG_FILE"; then
                log_message "card mounted read-only from $device"
                return 0
            fi
            log_message "could not mount $device"
        done
        if [ "$waited" -ge "$CARD_WAIT_SECONDS" ]; then
            # Record what the kernel actually offered, so a future card problem
            # can be diagnosed without another lockout.
            log_message "no card after ${CARD_WAIT_SECONDS}s; block devices: $(ls /dev/mmcblk* 2>/dev/null | tr '\n' ' ')"
            log_message "partitions: $(grep mmcblk /proc/partitions 2>/dev/null | tr '\n' ' ')"
            return 1
        fi
        sleep 2
        waited=$((waited + 2))
    done
}

if grep -qs " $SD_MOUNT " /proc/mounts; then
    log_message "card already mounted at $SD_MOUNT"
elif ! mount_card; then
    exit 0
fi

if [ -f "$MAINTENANCE_TXT" ]; then
    if [ -x /usr/sbin/telnetd ]; then
        /usr/sbin/telnetd -p 23 -l /bin/sh &
        log_message "maintenance.txt present: root shell started on port 23 for this boot"
    else
        log_message "maintenance.txt present but /usr/sbin/telnetd is missing"
    fi
fi

if [ -f "$WIFI_TXT" ]; then
    apply_wifi_file
else
    log_message "no wifi.txt on the card"
fi

# No security config means the firmware refuses to start any service, so the shell
# is the only way in and this is the moment an owner has to provision the camera.
# That is also exactly the state a factory reset leaves behind, so it is treated
# as the unprovisioned state: without a stored profile the camera also joins the
# rescue network, which is the out-of-the-box behaviour the vendor firmware had.
if [ ! -r "$SECURITY_CONFIG" ]; then
    if [ ! -s "$PROFILE" ]; then
        if rescue_write_profile 1; then
            log_message "unprovisioned: joining the rescue network $(rescue_ssid)"
        else
            log_message "unprovisioned: could not write the rescue profile"
        fi
    else
        log_message "unprovisioned: keeping the stored network, no security config yet"
    fi
    if [ -x /usr/sbin/telnetd ]; then
        /usr/sbin/telnetd -p 23 -l /bin/sh >/dev/null 2>&1 &
        log_message "unprovisioned: provisioning shell started on port 23"
    fi
fi
