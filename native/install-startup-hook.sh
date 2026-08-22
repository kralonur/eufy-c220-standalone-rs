#!/bin/sh
set -eu

# Append the standalone launcher to a preserved factory/custom app.sh. This is
# deliberately explicit: the caller must choose the base image and verify its
# existing media-start branch before installing the hook.
APP_SH=${1:?usage: install-startup-hook.sh /path/to/app.sh}
STARTUP_PATH=/home/script/c220-startup.sh

if [ ! -f "$APP_SH" ]; then
    echo "startup script not found: $APP_SH" >&2
    exit 1
fi
if grep -Fq "$STARTUP_PATH" "$APP_SH"; then
    echo "startup hook already present in $APP_SH"
    exit 0
fi

TEMP="$APP_SH.tmp.$$"
trap 'rm -f "$TEMP"' EXIT HUP INT TERM
cp -p "$APP_SH" "$TEMP"
cat >>"$TEMP" <<'EOF'

# C220 standalone firmware services. Keep the factory boot setup above this
# hook; c220-startup supervises the Rust daemon and its native media child.
if [ -x /home/script/c220-startup.sh ]; then
    /home/script/c220-startup.sh &
fi
EOF
mv -f "$TEMP" "$APP_SH"
trap - EXIT HUP INT TERM
printf 'installed standalone startup hook in %s\n' "$APP_SH"
