#!/bin/sh
# Return the camera to an unprovisioned state, as if it had never been set up.
#
# For a community build the camera changes hands, so the reset has to leave
# nothing of the previous owner behind: no Wi-Fi credentials, no TLS key or
# certificate, no Digest passwords, no MQTT or ONVIF settings, no opt-in markers.
# What it deliberately keeps is the vendor's own NVRAM identity - the board
# serial, country, the factory database and logs - because those identify the
# hardware rather than the deployment, and nothing here can regenerate them.
#
# After a reset the camera has no Wi-Fi profile and no security config, so the
# boot recovery starts the provisioning shell and the Wi-Fi bootstrap joins the
# rescue network. That is the vendor's out-of-the-box state: a new owner connects
# to the rescue hotspot, reaches the shell and provisions the camera.
#
# DRY_RUN=1 lists what would be removed and changes nothing.
set -u

NV=/home/nvram/system
DRY_RUN=${DRY_RUN:-0}

# Everything this deployment provisioned. Glob-free and explicit so the list can
# be reviewed, with the MQTT backups handled separately below.
PROVISIONED_FILES="
$NV/standalone_auth.conf
$NV/standalone_rtsp_cert.pem
$NV/standalone_rtsp_key.pem
$NV/standalone_rtsp_password
$NV/standalone_http_password
$NV/standalone_onvif_password
$NV/standalone_onvif.conf
$NV/standalone_mqtt.conf
$NV/standalone_mqtt_password
$NV/standalone_mqtt_ca.pem
$NV/standalone_rescue.conf
$NV/.standalone_audio_enable
$NV/.standalone_speaker_enable
$NV/.standalone_insecure_bringup
$NV/.maintenance_enable
$NV/.last_known_time
$NV/standalone_mqtt.conf.bak-r17
/home/nvram/fs_config/wpa_supplicant.conf
/home/nvram/config/eufy_wifi.conf
/home/nvram/config/eufy_wifi_configured
/var/run/wpa_supplicant/wpa_supplicant.conf
"

if [ "$DRY_RUN" = 1 ]; then
    printf 'factory reset dry run; would remove:\n'
    for file in $PROVISIONED_FILES; do
        [ -e "$file" ] && printf '  %s\n' "$file"
    done
    exit 0
fi

for file in $PROVISIONED_FILES; do
    rm -f "$file"
done
# Any other MQTT configuration backup the previous owner left behind.
for file in "$NV"/standalone_mqtt.conf.bak-*; do
    [ -e "$file" ] && rm -f "$file"
done

# Make sure the removals are on the flash before the reboot.
sync

# The card recovery and the button watcher live in the read-only image, so they
# survive this: a reset camera can still be provisioned from a card or by holding
# the button again.
reboot -f
