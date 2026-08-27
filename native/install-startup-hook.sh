#!/bin/sh
set -eu

# Install the standalone hooks into a preserved factory/custom app.sh:
#
#   * card recovery goes at the very top, so a card still yields a shell even if
#     something later in the factory script hangs or fails - which is the whole
#     point of having it;
#   * the service supervisor goes at the end, so it starts after the factory
#     setup it depends on.
#
# This is deliberately explicit: the caller must choose the base image and verify
# its existing media-start branch before installing the hooks.
APP_SH=${1:?usage: install-startup-hook.sh /path/to/app.sh}
STARTUP_PATH=/home/script/c220-startup.sh
RECOVERY_PATH=/home/script/c220-recovery.sh

if [ ! -f "$APP_SH" ]; then
    echo "startup script not found: $APP_SH" >&2
    exit 1
fi
if grep -Fq "$STARTUP_PATH" "$APP_SH" || grep -Fq "$RECOVERY_PATH" "$APP_SH"; then
    echo "startup hooks already present in $APP_SH"
    exit 0
fi

mode=$(stat -c '%a' "$APP_SH")
TEMP="$APP_SH.tmp.$$"
trap 'rm -f "$TEMP"' EXIT HUP INT TERM

# Insert the recovery call directly after the shebang line, so nothing above it
# can run first. A file without a shebang gets it at the top.
first_line=$(head -n 1 "$APP_SH")
{
    printf '%s\n' "$first_line"
    cat <<'EOF'

# Bounded card recovery, deliberately first: this is the only path that works
# with no network, no valid configuration and no service running, so nothing in
# the factory script below may be able to prevent it.
if [ -x /home/script/c220-recovery.sh ]; then
    /home/script/c220-recovery.sh
fi
EOF
    if [ "${first_line#\#!}" != "$first_line" ]; then
        tail -n +2 "$APP_SH"
    else
        cat "$APP_SH"
    fi
} >"$TEMP"

cat >>"$TEMP" <<'EOF'

# C220 standalone firmware services. Keep the factory boot setup above this
# hook; c220-startup supervises the Rust daemon and its native media child.
if [ -x /home/script/c220-startup.sh ]; then
    /home/script/c220-startup.sh &
fi
EOF

chmod "$mode" "$TEMP"
mv -f "$TEMP" "$APP_SH"
trap - EXIT HUP INT TERM
printf 'installed card recovery and standalone startup hooks in %s\n' "$APP_SH"
