# Install, setup and recovery

## Installation at a glance

This is a roadmap, not a substitute for the checks and commands below.

```mermaid
flowchart TD
    hardware["Confirm reference hardware"] --> backup["Back up all flash + NVRAM<br/>Establish recovery first"]
    backup --> harvest["Factory bootstrap + harvest"]
    harvest --> build["Build with private vendor inputs"]
    build --> secured{"Secure packaging succeeds?"}
    secured -->|No| stop["Stop · do not bypass hardening"]
    secured -->|Yes| write["Check flash map + image size<br/>Pad, write and read back"]
    write --> match{"Readback hash matches?"}
    match -->|No| recover["Do not reboot<br/>Keep shell and use recovery plan"]
    match -->|Yes| provision["Reboot + provision on isolated LAN"]
    provision --> verify["Trust certificate out of band<br/>Verify authentication + video"]
    verify --> done["Remove provisioning card<br/>Confirm normal boot"]
```

## 1. Check hardware and back up

Only the reference **C220 / T8W11**, `os03b10_CMK-OT2117-PC1_30IRC-F16.bin`
sensor revision is targeted. Similar names are not proof of compatibility.
Compare your sensor, `/proc/mtd` and mounts with the
[hardware reference](camera-internals.md). Stop on a difference.

Use stable USB power, a FAT32 microSD card with enough space for backups, a
computer with the [build environment](building.md), and an isolated trusted LAN.
Bootstrap and rescue Telnet give unauthenticated root access. Do not forward
ports or expose the camera to the Internet.

Before writing, save **all raw flash partitions**, boot information and NVRAM
from your own camera. Read twice, compare sizes/hashes and keep two private
copies off-device. An extracted `/home` tree or the bootstrap NVRAM directory
is not a raw rollback/full-chip backup. Never publish dumps, harvested vendor
files, Wi-Fi profiles or keys.

Establish a recovery method **before** changing flash. If `/home` cannot mount,
card/button recovery cannot run. A programmer may be required; UART is not
hardware-verified. See [no-boot recovery](camera-internals.md#no-boot-recovery).
Stop if you cannot accept or recover from that risk.

## 2. Enter factory firmware and harvest

On a factory camera with a reachable network, copy `iq.sh` and `harvest.sh` from
`native/factory-bootstrap/` to the card root. Insert before power-on. The vendor
boot hook runs `iq.sh` as root, opens Telnet on port 23 and writes `iq_boot.log`
with network information. It also saves `eufy_nvram_backup/` once. Find the
address from your router/log and connect:

```sh
telnet <camera-ip>
sh /mnt/sdcard/harvest.sh /mnt/sdcard/c220-harvest
```

Expect `HARVEST OK` and `c220-harvest/camera-rootfs/` plus `base-home/` on the card.
Harvest refuses to use an already-custom `/home` as a factory tree. This entry
point needs working vendor startup and network access; it is not no-boot recovery.

On the reference layout only, a raw original `/home` backup is:

```sh
dd if=/dev/mtd6 of=/mnt/sdcard/home-before.bin bs=65536 count=75
```

Check for read/write errors and exactly **4,915,200 bytes**, then copy it off the
card. Save the other partitions too; this one file is not a full backup.

## 3. Build

Follow [building](building.md) using your harvested files.

**Known blocker:** `harden-preserved-app.sh` expects exact sections from a
previously customised `app.sh`, including `# 1. Maintenance shell:` and
`# Production: disable core dumps to prevent SD card / RAM exhaustion`.
A fresh factory harvest does not establish those exist; there is no verified
factory-to-that-layout transformation here. If packaging rejects it, **stop**.
Do not invent markers or set `SECURE_DEPLOYMENT=0` to bypass hardening.

## 4. Write and verify

Proceed only with a successfully secured, reviewed image and a working recovery
plan. Your own map must show `home` as **mtd6**, size **004b0000**, erase size
**00010000**, and `/dev/block/by-name/home` resolving to mtdblock6.

The reference block device drops the final erase block written. Align the real
image to 64 KiB, then add one sacrificial block. The **whole write** must fit the
partition. On a GNU/Linux computer, run this block together:

```sh
(
set -eu
image=dist/c220-home.squashfs
aligned=$(( ($(stat -c %s "$image") + 65535) / 65536 * 65536 ))
write_bytes=$((aligned + 65536))
test "$aligned" -gt 0
test "$write_bytes" -le 4915200
cp "$image" /tmp/readback.img
truncate -s "$aligned" /tmp/readback.img
md5sum /tmp/readback.img                 # expected flash hash, not raw squashfs hash
cp /tmp/readback.img /tmp/write.img
truncate -s "$write_bytes" /tmp/write.img
printf 'write blocks: %s; verify blocks: %s\n' "$((write_bytes / 65536))" "$((aligned / 65536))"
)
```

Copy `/tmp/write.img` to the card root as `write.img`, safely eject, then use the
factory shell with the card at `/mnt/sdcard`. Replace both counts with the values
printed above:

```sh
dd if=/mnt/sdcard/write.img of=/dev/block/by-name/home bs=65536 count=<write-blocks> conv=notrunc
sync
dd if=/dev/mtd6 bs=65536 count=<verify-blocks> | md5sum
```

The hash must equal the host's **aligned `/tmp/readback.img`**, excluding the
sacrificial block. Inspect `dd` errors too. Read through the character device
(`/dev/mtd6`), not the block cache. **On any mismatch, do not reboot.** Preserve
the shell and use your recovery plan. Never write directly to `/dev/mtd6` without
an erase procedure: NOR programming cannot set cleared bits back to 1.

After a match, reboot. Without security config, video/HTTPS stay closed but
recovery opens root Telnet. It keeps a stored Wi-Fi profile if one exists;
otherwise it joins the rescue network. Keep setup isolated.

## 5. Provision

Remove the factory scripts and use a clean FAT32 card. Create `provision.txt`
in its root, replacing all example credentials:

```text
SSID=MyNetwork
PSK=my-wifi-password
RTSP_USER=frigate
RTSP_PASSWORD=your-generated-video-password
HTTP_USER=admin
HTTP_PASSWORD=your-different-generated-admin-password
HOST_NAME=c220
```

Use 2.4 GHz WPA/WPA2-Personal. `MODE=WPA` and `RTSPS_PORT=322` are defaults;
`HOST_NAME` and `NTP_HOST` are optional. Without an explicit name, the camera uses
`camera-<board-identity>.local`. Reserve DHCP or use the name in your certificate.

Insert **before power-on**, wait about 30 seconds and find the camera through
your router or mDNS. Remove the card after success: its plaintext credentials
reapply on later boots. Docker/VLAN clients may need explicit DNS or hosts entries.

### Card file formats

| File | Purpose |
| --- | --- |
| `provision.txt` | Both credential pairs, optional network, name, port and LAN time server. Replaces security config; include optional settings you want to retain. |
| `wifi.txt` | Network only: `SSID`, `PSK`, optional `MODE`. Preserves logins. If both files exist, its network settings win. |
| `cert.pem` + `key.pem` | Imported only while applying a valid full `provision.txt`; certificate-only cards are not imported. |
| `maintenance.txt` | Presence opens root Telnet for this boot. |

One `KEY=value` per line. First occurrence wins; surrounding spaces and enclosing
double quotes are stripped. Usernames: 1–32 of `A-Z a-z 0-9 . _ @ -`.
Passwords: 1–128 characters, no control characters, double quotes or backslashes.
SSID: 1–32; WPA passphrase: normally 8–63 characters. Port: 1–65535.
`HOST_NAME`: one DNS label, at most 63 characters, letters/digits/inner hyphens;
`.local` is stripped. `NTP_HOST`: at most 64 of `A-Z a-z 0-9 . : -`.

Files are parsed, never executed, on a read-only `nosuid,nodev,noexec` mount.
Provisioning is **not transactional**: credential/config writes can precede a
network validation or I/O failure. Read `/tmp/c220-recovery.log` and verify both
network and authentication. Templates are in `docs/recovery-card/`.

### Certificate and connection

TLS is mandatory. With no existing pair, the camera generates a self-signed
ECDSA P-256 certificate naming its hostname/current IPs. It keeps that pair until
you replace/delete it or reset. Trust its certificate out of band or during
physically isolated initial setup—not merely because a network peer presented it.

Alternatively supply your own PEM pair with the provisioning file. For example:

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout key.pem -out cert.pem -days 3650 -subj '/CN=c220' \
  -addext 'subjectAltName=DNS:c220.local,IP:192.168.1.55'
```

Use your real names/addresses. A private-CA certificate also works; include any
intermediate chain, leaf first. The key must be unencrypted and match the
certificate. Protect host/card copies. On camera, password/key files require
root/service ownership and mode `0600` or stricter. An incomplete or invalid
stored pair fails closed; copy both or delete both through maintenance.

To inspect the presented certificate on an isolated setup network:

```sh
openssl s_client -connect <camera>:322 -servername <camera> </dev/null 2>/dev/null \
  | openssl x509 -outform PEM > c220.pem
openssl x509 -in c220.pem -noout -fingerprint -sha256 -ext subjectAltName
```

After establishing trust, use a certificate-matching host:

```sh
curl --cacert c220.pem https://<camera>:8080/healthz
curl --cacert c220.pem --digest -u admin https://<camera>:8080/status
```

`/healthz` is public; the second command prompts for the HTTP password. Expect
ready health and a working video stream. Do not disable verification to fix a
hostname/CA error. Passwords in stream URLs must be URL-encoded and kept out of logs.

### NVR, MQTT and controls

Use **one direct video consumer**; release the relay connection before direct
camera probes. The tested ffmpeg/go2rtc paths did not verify the RTSPS peer.
MediaMTX supports certificate pinning; use an independently verified SHA-256
fingerprint without colons, then let the NVR consume the relay locally:

```yaml
paths:
  c220:
    source: rtsps://frigate:YOUR_PASSWORD@c220.local:322/live
    sourceFingerprint: "YOUR_VERIFIED_SHA256_FINGERPRINT"
    sourceOnDemand: no
```

Bind the relay's plaintext output to host loopback or otherwise secure it.
A changed certificate breaks pins intentionally: confirm the rotation and verify
the new certificate before updating clients, not just copying an error value.

MQTT config lives at `/home/nvram/system/standalone_mqtt.conf`. Set
`STANDALONE_MQTT_HOST`, an absolute `STANDALONE_MQTT_CA_FILE` path and, if needed,
paired `STANDALONE_MQTT_USERNAME` / `STANDALONE_MQTT_PASSWORD_FILE`. TLS is verified,
default port **8883**, with no plaintext fallback. Migrate old port-1883 setups
before updating. `STANDALONE_MQTT_DISCOVERY=1` opts into Home Assistant discovery.

HTTPS controls on **8080** use the HTTP Digest credentials:
`GET /status`, `GET /ptz/position`, `POST /ptz/move`, `/ptz/stop`, `/ptz/center`,
`/night_mode`, `/talkback/*`, `/maintenance/enable`, `/reboot`.
PTZ move body: `{"direction":"right","degrees":10,"speed":1}`; night mode:
`{"mode":0}` (0 auto, 1 night, 2 day). Center sweeps both axes before parking;
allow clear travel. The same left → right → up → down → center sweep now runs
on every firmware startup, with status/stop available throughout. It uses driver
positions; physical alignment still needs on-device verification.
Microphone is off unless the root-owned mode-600 marker
`/home/nvram/system/.standalone_audio_enable` exists at startup. Remove to disable;
reboot after either change. Test audio only with consent.

Minimal ONVIF uses the same HTTPS listener and HTTP password for cardinal manual
PTZ/stop—not autotracking, zoom or presets. Frigate 0.18.0 configuration:

```yaml
onvif:
  host: https://c220.local
  port: 8080
  user: admin
  password: "{FRIGATE_C220_HTTP_PASSWORD}"
  ignore_time_mismatch: true
  autotracking:
    enabled: false
```

That Frigate client's ONVIF connection disables certificate verification itself;
keep it LAN-restricted. Video pinning is independent.

## Recovery, updates and handover

```mermaid
flowchart TD
    problem{"Does camera startup still run?"}
    problem -->|No| hardware["Hardware recovery<br/>Card/button recovery cannot run"]
    problem -->|Yes| issue{"What needs repair?"}
    issue -->|Wi-Fi| wifi["Boot with wifi.txt<br/>Preserves logins"]
    issue -->|Credentials| credentials["Boot with full provision.txt<br/>Replaces security config"]
    issue -->|Maintenance shell| shell["Authenticated maintenance API<br/>or maintenance.txt on card"]
    issue -->|Offline, no usable card| rescue["Create rescue hotspot<br/>Hold reset about 6 seconds, release"]
    rescue --> telnet["Camera joins hotspot as a client<br/>Root Telnet on isolated trusted LAN"]
    issue -->|Owner reset| reset["Remove provisioning cards<br/>Hold reset at least 10 seconds"]
    reset --> erased["Known owner settings removed<br/>Reboots · not secure erasure"]
```

| Situation | Action |
| --- | --- |
| Wi-Fi changed | Boot with `wifi.txt`, then remove it. |
| Lost password | Boot with a full corrected `provision.txt`. |
| Need a shell | Authenticated `POST /maintenance/enable`, or card `maintenance.txt`; reboot closes it. |
| Offline, no usable card | Create 2.4 GHz WPA2 hotspot `RescueAP` / `12345678`, hold reset about 6 seconds then release; find its DHCP address and Telnet in. |
| Full owner reset | Remove provisioning cards, hold at least 10 seconds. Removes known credentials, TLS, Wi-Fi and opt-in settings, then reboots. |
| TLS mismatch | Check SAN/CA/key match; replace both files via a full provisioning card or maintenance. |
| Auth locked out | Five failed attempts from an address trigger a 30-minute lockout. Stop retries; wait or reboot. |
| Card ignored | Insert before power-on; FAT32, exact filenames at root, not `.txt.txt`. Read recovery log. |
| No boot / unreadable home | Card/button cannot repair missing startup code. Use [hardware recovery](camera-internals.md#no-boot-recovery). |

The 5-second action fires while holding; continuing to 10 seconds resets too.
Rescue is a **client network**, not an AP hosted by the camera. Without a card,
use its shell to repair protected NVRAM configuration and the Wi-Fi profile at
`/home/nvram/fs_config/wpa_supplicant.conf`; `/home/script/wificonnect.sh` is the
vendor helper, but prints passwords—do not record its terminal output.
Rescue override: mode-600 `/home/nvram/system/standalone_rescue.conf` with
`SSID=` / `PSK=`; reset deletes it and restores defaults.

Collect RAM logs before reboot: `/tmp/c220-recovery.log`, `/tmp/c220-button.log`,
`/tmp/c220-wifi.log`. Redact before posting. Report revision, source/build hash,
failing step and whether recovery still works; never attach full dumps/config.

For an update, back up current raw `/home` and NVRAM, build from compatible private
inputs, repeat write/readback above, then run `native/verify-c220-deployment.sh`
with your explicit camera, private NVRAM directory, aligned image and binary
hashes (`--help` lists flags). Skipped checks are not passes. Power-cycle without
the card, check verified video/authentication and ensure port 23 is closed.
Rehearse rescue/reset before leaving an unattended installation. There is no OTA
upload route. Factory rollback is in the [flash reference](camera-internals.md#factory-rollback).

Reset is **not secure erasure**: factory database/logs, arbitrary added files,
custom key/CA paths and backups may remain. Before handover, review/remove your
private leftovers without erasing board identity or radio calibration, revoke
broker credentials, clear retained discovery and remove cards. The next owner
must use their own credentials and TLS keys.
