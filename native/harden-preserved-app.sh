#!/bin/sh
set -eu

# Remove root-execution escape hatches from the preserved vendor/custom app.sh.
# These hooks are useful during development, but a mounted SD card or writable
# NVRAM must not be able to turn a LAN-connected camera into an unauthenticated
# root shell in the production image.
APP_SH=${1:?usage: harden-preserved-app.sh /path/to/app.sh}

if [ ! -f "$APP_SH" ]; then
    echo "app script not found: $APP_SH" >&2
    exit 1
fi

remove_section() {
    start_marker=$1
    end_marker=$2
    temp="$APP_SH.tmp.$$"
    app_mode=$(stat -c '%a' "$APP_SH")
    if ! awk -v start="$start_marker" -v end="$end_marker" '
        index($0, start) {
            if (inside || found_start) {
                exit 2
            }
            inside = 1
            found_start = 1
            next
        }
        inside && index($0, end) {
            inside = 0
            found_end = 1
            print
            next
        }
        !inside { print }
        END {
            if (inside || !found_start || !found_end) {
                exit 3
            }
        }
    ' "$APP_SH" >"$temp"; then
        rm -f "$temp"
        echo "could not remove app.sh section: $start_marker" >&2
        exit 1
    fi
    chmod "$app_mode" "$temp"
    mv -f "$temp" "$APP_SH"
}

remove_line() {
    target_line=$1
    temp="$APP_SH.tmp.$$"
    app_mode=$(stat -c '%a' "$APP_SH")
    if ! awk -v target="$target_line" '
        $0 == target {
            found += 1
            next
        }
        { print }
        END {
            if (found != 1) {
                exit 3
            }
        }
    ' "$APP_SH" >"$temp"; then
        rm -f "$temp"
        echo "could not remove app.sh line: $target_line" >&2
        exit 1
    fi
    chmod "$app_mode" "$temp"
    mv -f "$temp" "$APP_SH"
}

remove_to_end() {
    start_marker=$1
    temp="$APP_SH.tmp.$$"
    app_mode=$(stat -c '%a' "$APP_SH")
    if ! awk -v start="$start_marker" '
        index($0, start) {
            found += 1
            next
        }
        !found { print }
        END {
            if (found != 1) {
                exit 3
            }
        }
    ' "$APP_SH" >"$temp"; then
        rm -f "$temp"
        echo "could not remove app.sh tail: $start_marker" >&2
        exit 1
    fi
    chmod "$app_mode" "$temp"
    mv -f "$temp" "$APP_SH"
}

# The section boundaries are deliberately checked by remove_section: packaging
# fails rather than silently accepting a changed base app without hardening it.
remove_section \
    '#-------------install tfCard---------------' \
    '# Production: disable core dumps to prevent SD card / RAM exhaustion'
remove_section \
    '# 1. Maintenance shell:' \
    '# 2. Block cloud telemetry domains locally'
remove_section \
    '# 4. Optional user override script from NVRAM' \
    '# 5. Native Factory Reset / Unconfigured Auto-Rescue:'
remove_section \
    '# 5. Native Factory Reset / Unconfigured Auto-Rescue:' \
    '# 5. Check for optional SD card override'
remove_section \
    '# 5. Check for optional SD card override' \
    '# 6. Start the standalone Rust RTSP service.'
remove_line '/home/app/sn_write file /tmp/sd/devinfo.json'
remove_to_end '# 6. Start the standalone Rust RTSP service.'

if grep -Eq 'telnetd|/tmp/sd/(custom\.sh|maintenance\.txt|wifi\.txt|iq\.sh|Factory/factory_config\.sh)|/home/nvram/custom\.sh|auto_rescue\.sh' "$APP_SH"; then
    echo "unsafe root-execution hook remains in $APP_SH" >&2
    exit 1
fi

printf 'hardened preserved app script: %s\n' "$APP_SH"
