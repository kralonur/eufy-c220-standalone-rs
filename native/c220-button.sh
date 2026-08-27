#!/bin/sh
# Physical button: a short hold joins the rescue network, a long hold wipes the
# camera back to an unprovisioned state.
#
# Why this exists: this camera cannot host an access point - the factory rootfs
# ships no hostapd or DHCP server - and reaching it physically to insert a card is
# slow. The button gives a no-tools path that needs neither the network nor a
# working configuration.
#
#   hold  5s  rescue: join the rescue hotspot and open the maintenance shell, so
#             the stored Wi-Fi can be repaired from a phone or laptop.
#   hold 10s  factory reset: wipe credentials, certificate, key and Wi-Fi, then
#             reboot into the unprovisioned state a new owner starts from.
#
# The two tiers are separate on purpose: a five-second press must never destroy
# the credentials of a working camera, so the destructive action costs twice the
# hold and reports every second it counts.
#
# This board routes the button to a SARADC key ladder rather than a GPIO, and this
# kernel has no input/evdev subsystem at all, so the channel is sampled directly:
# measured idle 1020-1023, held 557-561, threshold 800 with wide margin.
set -u

# Overridable only so the shipping script can be exercised from /tmp on a
# running camera before it is flashed; the image path is the default.
RESCUE_LIB=${RESCUE_LIB:-/home/script/c220-rescue.sh}
. "$RESCUE_LIB"

ADC=/sys/bus/iio/devices/iio:device0/in_voltage0_raw
# Below this raw value the button counts as pressed.
PRESSED_BELOW=${PRESSED_BELOW:-800}
RESCUE_SECONDS=${RESCUE_SECONDS:-5}
RESET_SECONDS=${RESET_SECONDS:-10}
POLL_SECONDS=${POLL_SECONDS:-0.2}
POLLS_PER_SECOND=5
RESCUE_POLLS=$((RESCUE_SECONDS * POLLS_PER_SECOND))
RESET_POLLS=$((RESET_SECONDS * POLLS_PER_SECOND))
FACTORY_RESET=/home/script/c220-factory-reset.sh
LOG_FILE=/tmp/c220-button.log
DRY_RUN=${DRY_RUN:-0}

log_message() {
    printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG_FILE"
}

trigger_rescue() {
    log_message "button held for ${RESCUE_SECONDS}s: rescue mode"
    if [ "$DRY_RUN" = 1 ]; then
        log_message "dry run: would join the rescue network and open the shell"
        return 0
    fi
    if rescue_write_profile 1; then
        log_message "rescue profile for $(rescue_ssid) written; joining it"
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
    rescue_restart_wifi
    log_message "Wi-Fi restarted for the rescue network"
    return 0
}

trigger_factory_reset() {
    log_message "button held for ${RESET_SECONDS}s: factory reset"
    if [ "$DRY_RUN" = 1 ]; then
        log_message "dry run: would wipe provisioning and reboot"
        return 0
    fi
    if [ ! -x "$FACTORY_RESET" ]; then
        log_message "factory reset script missing; leaving provisioning alone"
        return 1
    fi
    # The script syncs and reboots, so the log line above is the last thing this
    # boot records. Rescuing first is harmless: the reset removes that profile.
    DRY_RUN=0 "$FACTORY_RESET"
    return 0
}

if [ ! -r "$ADC" ]; then
    log_message "no SARADC channel at $ADC; button recovery unavailable"
    exit 0
fi

log_message "button watcher started (dry_run=$DRY_RUN, rescue=${RESCUE_SECONDS}s, reset=${RESET_SECONDS}s)"

held=0
rescue_fired=0
reset_fired=0
while :; do
    raw=$(cat "$ADC" 2>/dev/null) || raw=
    if [ -n "$raw" ] && [ "$raw" -lt "$PRESSED_BELOW" ] 2>/dev/null; then
        held=$((held + 1))
        # Report progress once per second so the log shows the hold building.
        if [ $((held % POLLS_PER_SECOND)) -eq 0 ]; then
            log_message "button held for $((held / POLLS_PER_SECOND))s"
        fi
        if [ "$rescue_fired" -eq 0 ] && [ "$held" -ge "$RESCUE_POLLS" ]; then
            rescue_fired=1
            trigger_rescue
        fi
        if [ "$reset_fired" -eq 0 ] && [ "$held" -ge "$RESET_POLLS" ]; then
            # Once per boot: a second long hold after a failed reset must not
            # fight whatever the operator has started doing from the shell.
            reset_fired=1
            trigger_factory_reset
        fi
    else
        if [ "$held" -gt 0 ] && [ "$rescue_fired" -eq 0 ]; then
            log_message "button released before the ${RESCUE_SECONDS}s hold"
        fi
        held=0
    fi
    sleep "$POLL_SECONDS"
done
