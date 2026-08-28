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
# The card is read-only and the recognised files are data:
#
#   wifi.txt         SSID=... PSK=... MODE=WPA|WEP|NONE
#                    Writes the supplicant profile the Wi-Fi bootstrap uses, so
#                    the camera can be moved to a new network with no console.
#   provision.txt    SSID=... PSK=... MODE=... plus RTSP_USER, RTSP_PASSWORD,
#                    HTTP_USER, HTTP_PASSWORD and optionally RTSPS_PORT and
#                    NTP_HOST. Writes the network and the credentials, which is
#                    the whole setup for a new owner and needs no shell at all.
#   cert.pem,        Optional certificate and key to install instead of letting
#   key.pem          the firmware generate a self-signed pair.
#   maintenance.txt  Starts the root shell on port 23 for this boot only.
#
# Security: whoever can insert a card can open a root shell or point the camera
# at another network. That is the same physical access this device already
# cannot defend against, so the card slot is a credential - keep it somewhere
# the camera's credentials deserve.
set -u

# The mount point is overridable so the provisioning path can be exercised with a
# directory of card files and no card slot, the same way the button script allows
# its shared library to be replaced.
SD_MOUNT=${SD_MOUNT:-/tmp/sd}
# A card may be a partitioned volume (mmcblk1p1) or a bare filesystem on the
# whole device (mmcblk1), and the slot it lands on is not guaranteed, so every
# plausible name is tried instead of assuming one.
SD_DEVICES="/dev/mmcblk1p1 /dev/mmcblk1 /dev/mmcblk0p1 /dev/mmcblk0"
# The MMC controller can enumerate the card a little after this runs, so keep
# looking for a while rather than skipping recovery for the whole boot. The wait
# is overridable so the paths below can be exercised without a card slot.
CARD_WAIT_SECONDS=${CARD_WAIT_SECONDS:-20}
WIFI_TXT="$SD_MOUNT/wifi.txt"
PROVISION_TXT="$SD_MOUNT/provision.txt"
MAINTENANCE_TXT="$SD_MOUNT/maintenance.txt"
CARD_CERT="$SD_MOUNT/cert.pem"
CARD_KEY="$SD_MOUNT/key.pem"
PROFILE=/home/nvram/fs_config/wpa_supplicant.conf
RUNTIME=/var/run/wpa_supplicant/wpa_supplicant.conf
SYSTEM_DIR=/home/nvram/system
SECURITY_CONFIG=$SYSTEM_DIR/standalone_auth.conf
RTSP_PASSWORD_FILE=$SYSTEM_DIR/standalone_rtsp_password
HTTP_PASSWORD_FILE=$SYSTEM_DIR/standalone_http_password
CERT_FILE=$SYSTEM_DIR/standalone_rtsp_cert.pem
KEY_FILE=$SYSTEM_DIR/standalone_rtsp_key.pem
LOG_FILE=/tmp/c220-recovery.log

# Bounds for values accepted from the card. They are generous for real use and
# small enough that a malformed file cannot fill NVRAM or hide in a log line.
MAX_USERNAME_LENGTH=32
MAX_SECRET_LENGTH=128
MAX_HOST_LENGTH=64
DEFAULT_RTSPS_PORT=322

# Shared definition of the rescue network, used for the unprovisioned state below.
RESCUE_LIB=${RESCUE_LIB:-/home/script/c220-rescue.sh}
. "$RESCUE_LIB"

log_message() {
    printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG_FILE"
}

# Read one key from a data file. Files are parsed, never sourced: sourcing one
# would let a card run arbitrary commands as root.
value_of() {
    key=$1
    file=$2
    grep -E "^[[:space:]]*${key}[[:space:]]*=" "$file" 2>/dev/null \
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

# Validate a network description from either data file and write the profile the
# Wi-Fi bootstrap reads. The label names the source file so a rejection in the log
# says which card file to fix.
apply_network() {
    label=$1
    ssid=$2
    psk=$3
    mode=$4

    [ -n "$ssid" ] || { log_message "$label rejected: SSID is empty"; return 1; }
    [ "${#ssid}" -le 32 ] || { log_message "$label rejected: SSID is longer than 32 characters"; return 1; }
    if ! value_is_safe "$ssid" || ! value_is_safe "$psk"; then
        log_message "$label rejected: control characters, quotes and backslashes are not allowed"
        return 1
    fi
    if [ "$mode" != "NONE" ]; then
        [ -n "$psk" ] || { log_message "$label rejected: PSK is required for MODE=$mode"; return 1; }
        [ "${#psk}" -le 64 ] || { log_message "$label rejected: PSK is longer than 64 characters"; return 1; }
        if [ "$mode" = "WPA" ] && [ "${#psk}" -lt 8 ]; then
            log_message "$label rejected: a WPA PSK is 8 to 63 characters"
            return 1
        fi
    fi

    if write_profile "$mode" "$ssid" "$psk"; then
        log_message "$label applied: SSID=$ssid MODE=$mode"
    else
        log_message "$label could not be written to $PROFILE"
        return 1
    fi
    return 0
}

# Read the SSID, PSK and MODE keys from one card file and apply them.
apply_network_file() {
    label=$1
    file=$2
    ssid=$(value_of SSID "$file")
    psk=$(value_of PSK "$file")
    mode=$(value_of MODE "$file")
    [ -n "$mode" ] || mode=WPA
    mode=$(printf '%s' "$mode" | tr 'a-z' 'A-Z')
    case "$mode" in
        WPA|WEP|NONE) ;;
        *) log_message "$label rejected: MODE must be WPA, WEP or NONE"; return 1 ;;
    esac
    apply_network "$label" "$ssid" "$psk" "$mode"
}

# Digest usernames are compared as strings by the firmware, but they are also
# written into a file the boot script sources, so only characters that cannot
# start an expansion or a new statement are accepted.
username_is_valid() {
    [ -n "$1" ] || return 1
    [ "${#1}" -le "$MAX_USERNAME_LENGTH" ] || return 1
    case "$1" in
        *[!A-Za-z0-9._@-]*) return 1 ;;
    esac
    return 0
}

# A password is written to a file the firmware reads, never to the sourced config,
# so it only has to survive being one line of text.
secret_is_valid() {
    [ -n "$1" ] || return 1
    [ "${#1}" -le "$MAX_SECRET_LENGTH" ] || return 1
    value_is_safe "$1"
}

port_is_valid() {
    [ "${#1}" -le 5 ] || return 1
    case "$1" in
        *[!0-9]*) return 1 ;;
    esac
    [ "$1" -ge 1 ] 2>/dev/null || return 1
    [ "$1" -le 65535 ] 2>/dev/null || return 1
    return 0
}

# An address or host name for the LAN time server: enough for IPv4, a zone id or a
# DNS name, and nothing that could add a second config line.
host_is_valid() {
    [ "${#1}" -le "$MAX_HOST_LENGTH" ] || return 1
    case "$1" in
        *[!A-Za-z0-9.:-]*) return 1 ;;
    esac
    return 0
}

write_secret() {
    path=$1
    value=$2
    mkdir -p "$SYSTEM_DIR" || return 1
    # Created with restrictive permissions from the first byte rather than
    # narrowed afterwards, so the password is never briefly world readable.
    (umask 077; printf '%s\n' "$value" >"$path") || return 1
    chmod 600 "$path" || return 1
    return 0
}

# Install a certificate pair supplied on the card. The firmware generates a
# self-signed pair when none is installed, so this is only for an owner who wants
# their own certificate authority.
install_card_certificate() {
    if [ -f "$CARD_CERT" ] && [ -f "$CARD_KEY" ]; then
        (umask 077; cp "$CARD_KEY" "$KEY_FILE") || { log_message "card key could not be installed"; return 1; }
        chmod 600 "$KEY_FILE" || return 1
        cp "$CARD_CERT" "$CERT_FILE" || { log_message "card certificate could not be installed"; return 1; }
        chmod 644 "$CERT_FILE" || return 1
        log_message "card certificate installed from cert.pem and key.pem"
        return 0
    fi
    if [ -f "$CARD_CERT" ] || [ -f "$CARD_KEY" ]; then
        log_message "card has only one of cert.pem and key.pem; both are required, so neither was installed"
    fi
    return 0
}

# Provision the camera from the card: network, credentials and optionally a
# certificate. Everything is written before the firmware starts, so this takes
# effect on this boot without a second reboot.
apply_provision_file() {
    rtsp_user=$(value_of RTSP_USER "$PROVISION_TXT")
    rtsp_password=$(value_of RTSP_PASSWORD "$PROVISION_TXT")
    http_user=$(value_of HTTP_USER "$PROVISION_TXT")
    http_password=$(value_of HTTP_PASSWORD "$PROVISION_TXT")
    rtsps_port=$(value_of RTSPS_PORT "$PROVISION_TXT")
    ntp_host=$(value_of NTP_HOST "$PROVISION_TXT")

    # Both credential pairs are required: secure mode refuses to start without
    # either, so writing only half of a configuration would leave a camera that
    # serves nothing while looking provisioned.
    username_is_valid "$rtsp_user" || { log_message "provision.txt rejected: RTSP_USER must be 1 to $MAX_USERNAME_LENGTH characters of A-Z a-z 0-9 . _ @ -"; return 1; }
    username_is_valid "$http_user" || { log_message "provision.txt rejected: HTTP_USER must be 1 to $MAX_USERNAME_LENGTH characters of A-Z a-z 0-9 . _ @ -"; return 1; }
    secret_is_valid "$rtsp_password" || { log_message "provision.txt rejected: RTSP_PASSWORD must be 1 to $MAX_SECRET_LENGTH characters with no control characters, quotes or backslashes"; return 1; }
    secret_is_valid "$http_password" || { log_message "provision.txt rejected: HTTP_PASSWORD must be 1 to $MAX_SECRET_LENGTH characters with no control characters, quotes or backslashes"; return 1; }
    if [ -z "$rtsps_port" ]; then
        rtsps_port=$DEFAULT_RTSPS_PORT
    elif ! port_is_valid "$rtsps_port"; then
        log_message "provision.txt rejected: RTSPS_PORT must be between 1 and 65535"
        return 1
    fi
    if [ -n "$ntp_host" ] && ! host_is_valid "$ntp_host"; then
        log_message "provision.txt rejected: NTP_HOST is not a valid address or host name"
        return 1
    fi

    if ! write_secret "$RTSP_PASSWORD_FILE" "$rtsp_password" \
        || ! write_secret "$HTTP_PASSWORD_FILE" "$http_password"; then
        log_message "provision.txt rejected: password files could not be written"
        return 1
    fi
    install_card_certificate

    # The config is written last: it is what makes the firmware accept the files
    # above, so it never points at a password file that does not exist yet.
    if ! (umask 077; {
        printf 'STANDALONE_RTSP_USERNAME=%s\n' "$rtsp_user"
        printf 'STANDALONE_RTSP_PASSWORD_FILE=%s\n' "$RTSP_PASSWORD_FILE"
        printf 'STANDALONE_HTTP_USERNAME=%s\n' "$http_user"
        printf 'STANDALONE_HTTP_PASSWORD_FILE=%s\n' "$HTTP_PASSWORD_FILE"
        printf 'STANDALONE_RTSPS_PORT=%s\n' "$rtsps_port"
        printf 'STANDALONE_RTSP_CERT_FILE=%s\n' "$CERT_FILE"
        printf 'STANDALONE_RTSP_KEY_FILE=%s\n' "$KEY_FILE"
        if [ -n "$ntp_host" ]; then
            printf 'STANDALONE_NTP_HOST=%s\n' "$ntp_host"
        fi
    } >"$SECURITY_CONFIG"); then
        log_message "provision.txt rejected: $SECURITY_CONFIG could not be written"
        return 1
    fi
    chmod 600 "$SECURITY_CONFIG" || return 1

    # Passwords are deliberately not logged, not even in part.
    log_message "security config written: RTSP_USER=$rtsp_user HTTP_USER=$http_user RTSPS_PORT=$rtsps_port"

    if [ -n "$(value_of SSID "$PROVISION_TXT")" ]; then
        apply_network_file "provision.txt" "$PROVISION_TXT"
    fi
    log_message "provision.txt applied"
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
elif [ -f "$PROVISION_TXT" ] || [ -f "$WIFI_TXT" ] || [ -f "$MAINTENANCE_TXT" ]; then
    # Files are already available at the mount point, which is how this script is
    # exercised without a card slot. A boot with no card leaves this directory
    # empty, so the mount below is still the normal path.
    log_message "using the card files already present in $SD_MOUNT"
else
    # No card. This does not end the script: an unprovisioned camera still has to
    # leave a way in, and that happens below. Every card file is read from the
    # mount point, so with nothing mounted the card sections do nothing.
    mount_card || true
fi

if [ -f "$MAINTENANCE_TXT" ]; then
    if [ -x /usr/sbin/telnetd ]; then
        /usr/sbin/telnetd -p 23 -l /bin/sh &
        log_message "maintenance.txt present: root shell started on port 23 for this boot"
    else
        log_message "maintenance.txt present but /usr/sbin/telnetd is missing"
    fi
fi

# Provisioning runs first so a card that carries a full configuration applies it
# before the firmware starts, and so the narrower wifi.txt below still wins when
# someone uses one card for both.
provision_applied=0
if [ -f "$PROVISION_TXT" ]; then
    if apply_provision_file; then
        provision_applied=1
    fi
fi

if [ -f "$WIFI_TXT" ]; then
    apply_network_file "wifi.txt" "$WIFI_TXT"
elif [ "$provision_applied" = 0 ]; then
    log_message "no wifi.txt or provision.txt on the card"
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
