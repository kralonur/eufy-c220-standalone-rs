# Camera internals and operational notes

How the device behaves, what this firmware replaces, and the traps found on this
board: flashing, going back to the factory firmware, chip-level recovery, discovery,
the clock, the talkback path, and the deployment check that a write ends with.

This is the reference to read when something is wrong or when you are about to do
something irreversible. It is not the way in:

- setting a camera up, resetting it, and the recovery playbook are in
  [`setup-and-recovery.md`](setup-and-recovery.md);
- building the firmware and the media engine, and the vendor inputs they need, are in
  [`building.md`](building.md).

## Wi-Fi bootstrap

The factory vendor application was the component that started the supplicant and
the DHCP client, and secure packaging removes it. `script/c220-wifi.sh` now owns
that job: it waits for the asynchronously loaded driver to create `wlan0`, copies
the saved profile from `/home/nvram/fs_config/wpa_supplicant.conf` (falling back
to `/home/nvram/config/eufy_wifi.conf`) into
`/var/run/wpa_supplicant/wpa_supplicant.conf`, runs the preserved
`script/wificonnect_c.sh`, and retries until the interface holds an address. It
runs in the background from `script/c220-startup.sh`, so a slow or absent access
point delays the network but never the media service, and it waits for
`/home/nvram/system/standalone_auth.conf` before starting Wi-Fi at all.

This path replaces the removed `auto_rescue.sh`. Factory-reset detection, the
RescueAP fallback, and the SD/NVRAM root hooks are deliberately not restored;
the camera therefore needs a reachable access point, or the console, rather than
a rescue SSID. The bootstrap has not yet been exercised on hardware: verify it
over the serial console before flashing an image whose only management path is
the network.

## Runtime security configuration

Credentials and private keys are read at boot from writable NVRAM, not stored
in the image. Create `/home/nvram/system/standalone_auth.conf` with root-only
permissions and map the service settings as follows:

```sh
STANDALONE_RTSP_USERNAME=frigate
STANDALONE_RTSP_PASSWORD_FILE=/home/nvram/system/standalone_rtsp_password
STANDALONE_HTTP_USERNAME=admin
STANDALONE_HTTP_PASSWORD_FILE=/home/nvram/system/standalone_http_password
STANDALONE_RTSPS_PORT=322
STANDALONE_RTSP_CERT_FILE=/home/nvram/system/standalone_rtsp_cert.pem
STANDALONE_RTSP_KEY_FILE=/home/nvram/system/standalone_rtsp_key.pem
```

Use mode `0600` for the config and both password/key files. The startup hook
maps these values to the firmware's environment. In secure mode, missing or
invalid configuration stops the service rather than opening a fallback:
`RTSPS_PORT` must be nonzero, TLS 1.3 protects both RTSPS and the HTTP control
API, and both listeners require Digest credentials. `/healthz` is the only
intentionally unauthenticated endpoint.

### The certificate is generated on first boot

A new camera is not left needing a certificate authority: when neither
`STANDALONE_RTSP_CERT_FILE` nor `STANDALONE_RTSP_KEY_FILE` exists, the firmware
generates a self-signed pair itself (ECDSA P-256, one year-and-a-decade-long
validity that does not depend on the board's clockless boot) and installs it with
the key at mode 0600. Its subject alternative names are `c220.local`, loopback
and every IPv4 address the camera holds, so the address the owner types into a
client is in the certificate. Encryption stays mandatory; only the CA ceremony is
removed.

A certificate the owner installs is never overwritten, and an owner who wants
proper verification writes `cert.pem` and `key.pem` onto the provisioning card,
or places them at the configured paths. To replace a generated certificate with a
newly generated one, delete both files and reboot.

Because a self-signed certificate cannot be verified against a public CA, clients
either pin it or trust it explicitly. It is public information, so it can be
fetched from the camera itself:

```sh
openssl s_client -connect <camera>:322 -servername <camera> </dev/null 2>/dev/null \
  | openssl x509 -outform PEM > c220.pem
```

Then point the client's CA file at `c220.pem` (a self-signed certificate is its
own issuer) or at the LAN CA when the owner issued the certificate properly. The
fingerprint to pin is `openssl x509 -in c220.pem -noout -fingerprint -sha256`.

Plaintext RTSP and a plaintext control API are not configurable, because secure
mode cannot allow them: they are fixed off, and only the one-boot bring-up marker
below can reopen them. `STANDALONE_RTSP_PORT` and `STANDALONE_API_TLS` therefore
no longer exist as settings and are ignored if an older configuration still sets
them - having them as variables only provided a way to make the firmware refuse
to start. `STANDALONE_RTSPS_PORT` remains a setting because choosing a different
port is a legitimate preference.

For offline board bring-up only, create the one-boot marker
`/home/nvram/system/.standalone_insecure_bringup`; it permits the legacy
plaintext/anonymous defaults for that boot and is consumed by the startup
hook. Do not create it in production. Secure packaging also removes the
unauthenticated Telnet and arbitrary SD/NVRAM root-script hooks.

For deliberate maintenance, the authenticated TLS control API exposes
`POST /maintenance/enable`. It starts the existing `/usr/sbin/telnetd` on port
23 with `/bin/sh` for the current boot. The route is unavailable unless both
Digest credentials and the control-plane TLS acceptor are configured; the
startup script kills stale Telnet processes at the next boot. It proves the shell
is really listening before reporting success, and clears its one-shot flag when it
is not, so a spawn that never bound its port can be asked for again. Use it only
from a trusted LAN and close the window when finished.

`POST /reboot` restarts the board. It exists because this firmware has no other
remote way to refresh the one-boot shell window, which made every deployment step
cost physical access to the camera; it is authenticated exactly like the
maintenance route, and it answers before the network goes away.

## Chip-level recovery (CH341A, UART)

Everything the running camera can do for itself needs no programmer: credentials,
Wi-Fi, configuration and the whole `/home` image are recoverable from a card, the
button, or the rescue network. A factory camera can be entered the same way before
this firmware is on it at all: the vendor boot script runs `iq.sh` from a FAT32 card
as root, which is what `native/factory-bootstrap/iq.sh` is for and how the first
install is done (see README.md). `flashrom` on a CH341A is for the states where the
camera cannot run at all - a damaged bootloader, kernel or rootfs - and for a flash
write that failed part way. This project only ever writes `mtd6` (the `home`
partition, which holds the firmware itself); the vendor partitions below it are
never touched.

![The camera's mainboard with the enclosure removed](images/c220-mainboard.jpg)

*The board with the cover off: the Rockchip **RV1103G1**, the `POWER`, `SPEAKER` and
`MIC1` connectors, the Wi-Fi module in its shield, and where the vendor's serial and part
label sits - redacted here, because it identifies one camera and the firmware derives its
mDNS name from that serial. Nothing on this board is touched for a normal install: the
card is the way in, and this photo is here so you know what you are looking at if you do
open it.*

The flash chip is a 16 MiB GD25Q128E. **Pin the chip name**, because autoprobe can
select a different 16 MiB part and write the wrong thing:

```sh
CHIP='GD25Q128E/GD25B128E/GD25R128E/GD25Q127C'
```

![The GD25Q128E flash chip next to the lens](images/c220-flash-gd25q128e.jpg)

*The chip itself, marked `25Q128ESIG`: SOIC-8 next to the lens, which is the awkward
part of getting a clip onto it. The blue dot marks pin 1. That green wire is the
vendor's antenna lead, not a modification of ours.*

A full-chip read or write takes about two to three minutes. The layout, computed
from `/proc/mtd` and confirmed against a full dump:

| mtd | name | size | chip offset | contents |
| --- | --- | --- | --- | --- |
| 0 | `env` | 0x10000 | 0x0000000 | vendor environment |
| 1 | `idblock` | 0x20000 | 0x0010000 | vendor |
| 2 | `uboot` | 0x20000 | 0x0030000 | vendor bootloader |
| 3 | `boot` | 0x200000 | 0x0050000 | vendor kernel |
| 4 | `rootfs` | 0x400000 | 0x0250000 | vendor rootfs |
| 5 | `recovery` | 0x380000 | 0x0650000 | vendor recovery |
| 6 | `home` | 0x4b0000 | 0x09d0000 | **the only region this project writes** |
| 7 | `nvram` | 0x160000 | 0x0e80000 | Wi-Fi, credentials, board identity |
| 8 | `misc` | 0x10000 | 0x0fe0000 | vendor |
| 9 | `DID` | 0x10000 | 0x0ff0000 | vendor |

**Read before you write anything.** A full dump is the undo for everything below:

```sh
flashrom -p ch341a_spi -c "$CHIP" -r full-before.bin
```

To restore only `/home`, which is the usual case, the image has to be placed at its
offset inside a full-size image because flashrom validates against the whole chip:

```sh
cp full-before.bin work.bin
dd if=eufy_c220_standalone_home.squashfs of=work.bin bs=4096 \
   seek=$((0x9d0000 / 4096)) conv=notrunc
flashrom -p ch341a_spi -c "$CHIP" -w work.bin
```

Then read it back and compare, rather than trusting the write:

```sh
flashrom -p ch341a_spi -c "$CHIP" -r verify.bin
cmp -n 4915200 -i $((0x9d0000)) work.bin verify.bin && echo identical
```

Do not write `env`, `uboot`, `boot` or `rootfs` from anything but a dump taken from
the same board, and never write `nvram` from another camera: it holds that camera's
Wi-Fi profile, credentials and board identity.

**UART console.** The vendor inittab spawns a shell on `ttyFIQ0` at 1500000 baud,
8N1, which is the fastest way to see why a boot failed. This project has not
verified it - it needs a USB-TTL adapter that was not available while the rest of
this was tested - so treat the baud rate and the shell as expected rather than
proven.

## Receiving RTSPS: SDP direction attributes and certificate verification

Two things about consuming this camera's RTSPS are worth knowing, both discovered the
hard way while wiring it into a containerised NVR.

**Direction attributes are a trap.** The SDP marks forward tracks only with
`a=control`, and marks the talkback track `a=sendonly`. That looks backwards next to
RFC 4566, where `sendonly` describes what the *server* sends, and the instinct is to
mark the video and microphone tracks `sendonly` too. Do not: the Go RTSP clients in
wide use read `sendonly` as "this media is a backchannel the client sends on" -
gortsplib decides with a one-line `isBackChannel()` that looks for exactly that
attribute, and MediaMTX skips backchannels when it pulls a stream. A camera that
marks its forward tracks `sendonly` therefore looks, to those clients, like a camera
that offers nothing but a backchannel. ffmpeg is unbothered, which is what makes the
failure look like a client bug rather than an SDP one.

**Certificate verification is the client's problem, and the obvious clients do not do
it.** ffmpeg cannot verify an `rtsps://` peer at all: `tls_verify` defaults to 0 and
the option is a TLS-protocol option that the RTSP demuxer rejects (`Option tls_verify
not found`), as does `-ca_file`. go2rtc's native client connected to this camera with
the certificate deliberately removed from its trust store. MediaMTX does verify, and
can pin the camera's own certificate without any CA:

```yaml
paths:
  eufy_c220:
    source: rtsps://frigate:PASSWORD@192.168.50.55:322/live
    sourceFingerprint: "5d7143b9b86d7dcf17842f13c19705ef6872659e62c988dc0bc463aa703bcde1"
    sourceOnDemand: no
```

The fingerprint goes in without colons, or the comparison fails with both values
printed, and it is the anchor: a wrong fingerprint stops the connection rather than
producing a warning. Publish it only on the host loopback, and let the NVR read that
plain local stream, because a second listener on the LAN is a second way in.

### When the camera's certificate changes

A pin is tied to one certificate, so it stops working when the certificate is
replaced. That happens when:

- a 10-second factory reset wipes the certificate and key, so the next boot generates
  a new pair with a new key;
- the two certificate files are deleted (which is the documented way to ask the camera
  for a new one) and it reboots;
- a certificate is installed from a card or over the maintenance shell.

The failure is explicit rather than silent, and it prints the value to paste:

```
ERR [path eufy_c220] [RTSP source] source fingerprint does not match:
    expected 5d71...bcde1, got 3a04...77c2
```

`got` is the new fingerprint, in the colon-free form the option wants - so the fix is
to replace the pinned value with it and restart the relay. To fetch the current
fingerprint straight from the camera instead of reading it out of an error:

```sh
openssl s_client -connect <camera>:322 -servername <camera> </dev/null 2>/dev/null \
  | openssl x509 -noout -fingerprint -sha256 | cut -d= -f2 | tr -d ':'
```

Anything else that pinned the same certificate - a monitoring script, a phone app,
another NVR - needs the same update. Clients that trust a CA instead are unaffected by
this, which is the one advantage a CA keeps: the anchor outlives every certificate
that is issued from it.

## Flashing the home partition on this board

The image has to reach the camera on a card first: there is no `scp`, no usable `wget`
and no mtd-utils on the board, and the mount point depends on what is running.
`native/c220-recovery.sh` mounts a card read-only at `/tmp/sd`, and factory firmware
booted from a card (`native/factory-bootstrap/iq.sh`) has it at `/mnt/sdcard`. Pad the
image before it travels, because the write discards its last erase block (below):

```sh
# on the computer: pad to a 64 KiB multiple, then add the one block the write
# itself discards, and note the block count for the dd below
cp dist/c220-home.squashfs /tmp/write.img
truncate -s $(( ($(stat -c %s /tmp/write.img) + 65535) / 65536 * 65536 + 65536 )) /tmp/write.img
echo "$(( $(stat -c %s /tmp/write.img) / 65536 )) blocks to write"
```

Two things that cost an hour and are worth knowing before touching mtd6:

- `flashcp` on the camera is a stub: it answers "This doesn't seem to be a valid MTD
  flash device!" and writes nothing (`flash rc=1`). There are no mtd-utils on the
  board, and the SD card is not mounted. The working path is the vendor's by-name
  block node: `/dev/block/by-name/home` -> `/dev/mtdblock6`.
- Writes to that mounted block device must be **erase-block aligned**. `dd bs=4096`
  reports "312+0 records out", returns success, and silently drops the final partial
  erase block - leaving the last 32 KiB of a new image unwritten, which is enough to
  make the squashfs unmountable. Use `bs=65536` (the mtd erase size from
  `/proc/mtd`) over an image padded to a 64 KiB multiple:

  ```sh
  dd if=/tmp/blocks.img of=/dev/block/by-name/home bs=65536 conv=notrunc
  dd if=/dev/mtd6 bs=65536 count=20 | md5sum     # reads flash, not the block cache
  ```

  Verify through the *character* device: it bypasses the block cache, so a matching
  md5 there is evidence about the flash itself.

- Verify the **whole** image, not just that `dd` reported success. Observed on this
  board twice: after a write of a 64 KiB-multiple image through
  `/dev/block/by-name/home`, blocks 0..18 read back byte-identical and the *last*
  erase block of the write holds something else, `sync` notwithstanding. The
  squashfs metadata lives at the end of the image, so rebooting there mounts
  nothing - a camera with a readable superblock and an unreadable id table, which
  looks like a dead device and cannot be repaired over the network.

  **Rewriting does not fix it.** This block device drops the final erase block of
  every write, so a second identical write drops it again. Write one extra 64 KiB
  block instead, which makes the real last block interior and gets it programmed:

  ```sh
  # 21 blocks for a 20-block image: the 21st is padding and may be anything.
  dd if=/tmp/blocks-plus-one.img of=/dev/block/by-name/home bs=65536 count=21 conv=notrunc
  dd if=/dev/mtd6 bs=65536 count=20 | md5sum     # must equal the image
  ```

  The character device is not the answer either: `dd of=/dev/mtd6` programs
  without erasing, and NOR flash can only clear bits, so every block whose content
  differs comes back as the AND of the old and new bytes. Read through
  `/dev/mtd6`, write through the block device.

  Two writes are therefore mandatory around any `/home` change: read back the
  whole written range through `/dev/mtd6` and compare it against the image, then
  reboot and re-check that the services came up. Skipping the comparison is what
  bricked this camera once; the programmer is the fallback, and a full-chip dump
  taken while the camera last worked is what makes that fallback quick.

## Going back to the factory firmware

The undo is the `/home` image that was on the camera before this firmware, and it is
written exactly like any other image: same `dd`, same 64 KiB alignment, same read-back
through `/dev/mtd6`. Two things are different, and both matter.

- **The factory image fills the partition**, because it was the whole partition to
  begin with. Take it from your own dump of `mtd6` (taken before this firmware was
  installed), or cut the `home` region out of a full-chip dump with `dd` at the offset
  and size in the layout table above; `unsquashfs` reads it if you want to look
  before writing.
- **The padding block does not fit.** Writing one block past the end of a
  full-partition image would run past the partition, so the trick that makes the real
  last block interior is not available - and the last block is exactly where the
  squashfs id table lives. So write it, read the whole range back through
  `/dev/mtd6`, and if the last block does not match, **stop and use the programmer**
instead of rebooting. This path has not been exercised for a full-partition write;
the smaller images this project ships are, and that is why they use the extra block.

What that write does *not* touch is NVRAM, which is a separate partition mounted at
`/home/nvram` and is never written by this project. The files this firmware keeps
there (credentials, its certificate and key, the `standalone_*` configuration and
markers) are simply unread once the vendor `/home` is back, so they are inert; remove
them for a clean camera:

```sh
rm -rf /home/nvram/system/standalone_* /home/nvram/system/.standalone_*
rm -f /home/nvram/custom.sh
```

The vendor's own Wi-Fi profile and region tables live in the same partition and are
not ours to delete. With the factory `/home` restored and those leftovers gone, the
camera behaves like a factory one from the outside: the vendor partitions below
`/home` were never touched.

## Verifying a deployment

`native/verify-c220-deployment.sh` is the gate a `/home` write ends with. It checks
the camera in layers, so whatever it can reach is still useful:

- `health` and the port map need nothing but the network;
- `/status` and the RTSPS probe need the HTTP and RTSP credentials;
- the engine, firmware and `/dev/mtd6` hashes, the fatal-signal count, the engine
  process count and the amplifier pin need the one-boot maintenance shell.

The shell is armed through `POST /maintenance/enable` and closed again before the
script exits, so it never leaves a root shell listening. That window is one boot
only: a second run in the same boot cannot reopen it, which is what `POST /reboot`
is for. When the window does not open, the on-camera group is reported as a failure
carrying the API's HTTP status instead of a pass.

The image is compared through the character device, which is the only read that
answers for the flash rather than for a cache.

```sh
C220_NVRAM=/path/to/nvram/system \
C220_IMAGE=/tmp/c220-home-padded.img \
C220_ENGINE_MD5=<engine md5> C220_FIRMWARE_MD5=<firmware md5> \
  native/verify-c220-deployment.sh
```

Exit status is 0 only when nothing failed; skipped checks are counted and printed,
because "not verified" is not the same as "verified". `--network-only` runs the
reachability and configuration checks without credentials.

## Talkback: what the speaker path does, and what the client owes it

The speaker path is queued, not inline: `talkback_pcm` and `talkback_audio` reserve a
timestamp and `try_send` a `SpeakerFrame` into a bounded channel
(`AUDIO_QUEUE_FRAMES` = 32, so a couple of seconds of slack at the usual chunk size),
a writer task drains it, and a full queue is answered with `503 "speaker queue is
full"` instead of blocking. The write itself runs on `spawn_blocking`, so the engine's
socket write never occupies a Tokio worker - that was the earlier flaw, and the fix
is in the code, not pending.

Verified by ear on 2026-09-16, at volume 60, with both paths: a three-tone pattern
(440/880/1760 Hz) came out of the speaker twice, once from raw PCM and once through
the AAC decoder, and a ten-second 500-2000 Hz sweep was smooth end to end.

Measured as well, by recording the camera's own microphone over RTSPS (PCMA, 8 kHz)
while the speaker played: an ascending set (500/1000/2000 Hz) sent as raw PCM and a
descending set (2000/1000/500 Hz) sent as AAC both came back in the intended order,
with the peaks on the expected frequencies and the expected 0.6 s tone / 0.4 s gap
spacing. What the recording also shows is that this speaker is not flat: 1 kHz
arrives around ten times stronger than 500 Hz or 2 kHz (rms 2500 against 200-350 in
the same capture). That is the driver in a small sealed enclosure, not the path, and
it is the reason a low-frequency test tone sounds faint however high the volume is.

What the camera does **not** do is invent audio it never received. Fed at real time it
plays continuously; fed more slowly it runs dry and the gaps are audible. That is the
whole of the "stutter" story, and it is measurement, not theory:

| how the client sent 10 s of audio | wall time | result |
| --- | --- | --- |
| one curl per 100 ms chunk, paced for a 100 ms budget | 17.8 s | choppy - the feed was 1.8x slower than the audio |
| the same, paced to compensate for the per-request cost | 9.6 s | smooth |

A fresh TLS connection with a Digest handshake costs about 78 ms, so a client that
opens one connection per chunk has already spent most of a 100 ms budget before the
camera sees anything. A real client holds the connection open and streams; that is the
case these numbers say nothing about, and the one that matters.

## Discovery by name (mDNS)

The camera answers for its own name on the local network and advertises its stream,
so a client does not have to be told an address and a generated certificate can be
verified without an address exception: the certificate names the same host.

- Name: `HOST_NAME`, else derived from the board identity as `camera-<serial>`: the
  vendor PCB serial (kept by a factory reset), then the SoC serial from
  `/proc/cpuinfo` (survives a wiped NVRAM), then the Wi-Fi MAC. Serial numbers are
  unique, so two cameras cannot collide and neither needs configuring.

  The firmware starts before Wi-Fi has an address, and registering with no address
  left the camera unanswered for minutes while the responder refreshed: it now waits
  in a background task for an address (up to 30 seconds, without delaying the
  services) and registers with it. Measured: first answer 17 seconds after a reboot,
  one second after the network came up.
- Service: `_rtsp._tcp.local`, instance `<name>`, TXT `tls=1` and `path=/live`. The
  advertised port is the RTSPS port, because plaintext RTSP is retired.
- Addresses are not fixed at registration: the responder tracks the interfaces, so
  the advertised address follows a DHCP change and follows the camera onto the
  rescue network. This matters here more than on a mains-powered server, because
  this camera changes networks whenever rescue mode or a new lease happens.
- Discovery is best-effort by design. If the responder cannot start, the firmware
  logs a warning and serves anyway: a camera that cannot advertise is still a
  camera, and refusing to serve over it would trade a working device for a tidy log.

`mdns-sd` implements it (RFC 6762/6763). It runs its own thread - no async runtime
integration - and both default features are disabled, which drops the `log` bridge
(Cargo.lock keeps `base64` 0.23 only for the certificate `pem` writer, not for this).
Cost: 212 KB of binary, 86 KB of the compressed image, for name resolution and
service discovery.

Clients still need mDNS themselves. Docker containers do not have it: a container
that must resolve `.local` wants a hosts entry (one line in compose) or host
networking, and Android apps generally cannot resolve `.local` at all.

## Card recovery

A camera with no working network has no other management path, and reaching it
physically is slow. `script/c220-recovery.sh` therefore keeps a narrowly bounded
card-based path that runs before the services start. The card is mounted
read-only with `nosuid,nodev,noexec`, and only these files are recognised:

| file | effect |
| --- | --- |
| `wifi.txt` | `SSID=...`, `PSK=...`, `MODE=WPA\|WEP\|NONE`. Parsed as data, never sourced. Writes the supplicant profile the Wi-Fi bootstrap reads, so the camera can be moved to a new network with no console |
| `provision.txt` | network plus credentials: `SSID`, `PSK`, `MODE`, `RTSP_USER`, `RTSP_PASSWORD`, `HTTP_USER`, `HTTP_PASSWORD`, and optionally `RTSPS_PORT` and `NTP_HOST`. This is the whole setup for a new owner and needs no shell |
| `cert.pem`, `key.pem` | install a certificate and key instead of letting the firmware generate a self-signed pair; both are required |
| `maintenance.txt` | starts the root shell on port 23 for that boot only |

### Setting up a camera with a card

Write `provision.txt` in the card's root, put the card in the camera and power it
on. The camera writes the Wi-Fi profile, both password files, the configuration
and - if the card carries one - the certificate, all before the firmware starts,
so the camera is serving on that same boot:

```sh
SSID=MyNetwork
PSK=my-wifi-password
RTSP_USER=frigate
RTSP_PASSWORD=a-long-random-password
HTTP_USER=admin
HTTP_PASSWORD=another-long-random-password
NTP_HOST=192.168.1.1
```

Values are validated before anything is written (usernames are restricted to
`A-Z a-z 0-9 . _ @ -`, passwords to 128 characters with no control characters,
quotes or backslashes, the port to 1-65535) and a file that fails validation is
rejected as a whole rather than half applied. Passwords are never written to the
log. `provision.txt` deliberately replaces an existing configuration: it is the
handover path, and the card slot is already a physical credential.

**Remove the card after provisioning.** It contains the Wi-Fi password and the
camera credentials in plain text.

Nothing else on the card is read or executed: there are no `custom.sh`, factory,
SD or NVRAM script hooks, and `maintenance.txt` is the only way to a shell that
does not need the network. `wifi.txt` and `provision.txt` are re-applied on every
boot the card is present, so a card left in the slot keeps overriding the stored
network and credentials.

Treat the card slot as a credential. Anyone who can insert a card can open a root
shell, point the camera at another network or replace its credentials - the same
physical access the device cannot defend against - so keep the recovery card
where the camera's credentials deserve to be, and leave it out of the camera in
normal operation. The recovery log is written to `/tmp/c220-recovery.log` on each
boot.

## Button recovery and factory reset

The physical reset button has two tiers, separated on purpose so a short press can
never destroy a working camera's credentials:

| hold | effect |
| --- | --- |
| 5 s | joins the rescue hotspot and opens the maintenance shell for this boot |
| 10 s | factory reset: wipes credentials, certificate, key, Wi-Fi and markers, then reboots |

This board routes the button to a SARADC key ladder rather than a GPIO, and this
kernel has no input/evdev subsystem, so `script/c220-button.sh` samples the IIO
channel directly (measured: 1020-1023 idle, 557-561 held, threshold 800).

The camera cannot host an access point: the factory rootfs ships no hostapd or
DHCP server, so there is nothing to hand out addresses. It therefore *joins* a
hotspot instead, which is what the vendor's rescue mode did. The defaults are
`RescueAP` / `12345678`; override them with

```sh
SSID=MyPhoneHotspot
PSK=my-hotspot-password
```

in `/home/nvram/system/standalone_rescue.conf` (mode 0600, parsed as data and
never sourced).

## Unprovisioned state

A camera with no `standalone_auth.conf` cannot start any service - that is the
fail-closed rule - so `script/c220-recovery.sh` treats it as unprovisioned and
provisions the way in: with no stored Wi-Fi profile it joins the rescue network,
and it opens the maintenance shell on port 23. This is the state a factory reset
leaves behind and the state a new owner receives, matching the vendor firmware's
out-of-the-box behaviour. It happens on every boot until the camera is
provisioned and needs no card and no button: with a card that
`provision.txt` provisions, the configuration exists afterwards and neither the
shell nor the rescue network is opened. Provision from that shell with
`/home/script/wificonnect.sh "<ssid>" WPA "<psk>"` and by writing
`standalone_auth.conf` plus the certificate and key as described above.

The factory reset deliberately keeps the vendor's own NVRAM identity - board
serial, country, rate-power tables, the factory database and logs - because those
identify the hardware rather than the deployment and cannot be regenerated. See
`script/c220-factory-reset.sh` for the exact list; `DRY_RUN=1` prints it without
changing anything.

Both the button and the card are physical controls, so they are physical-access
paths by design: whoever can reach them can move the camera to their own network
and open a root shell. That is the same trust level as the card slot, and it is
the point - they are the recovery routes that need no tools and no working
network.

Microphone capture is opt-in, and nothing in this repository or the image creates
the switch: a freshly deployed camera has no microphone track until you ask for one.
Create the root-owned marker `/home/nvram/system/.standalone_audio_enable` before
starting the service when RTSP microphone audio is required; leave it absent to keep
capture disabled. Absent means the firmware also launches the engine with
`--no-audio`, so the capture device is never opened - this is not a mute, it is the
microphone not being started. `STANDALONE_AUDIO=1` in the service's environment
overrides the marker either way, which is what a bench run wants; production scripts
set neither.

How to tell which state a running camera is in: read its SDP. The microphone is
`trackID=1` (`m=audio` ... `a=control:trackID=1`), and it is absent when capture is
disabled. The backchannel (`a=control:backchannel`, marked `a=sendonly`) is a
separate media for audio *going to* the speaker and stays advertised either way, so a
client that counts audio tracks will see two when the microphone is on and one when
it is off.

## Clock

This board has no RTC. The boot script restores the last known time from
`/home/nvram/system/.last_known_time`, which is plausible by construction and can
be hours stale, so the firmware also runs a best-effort SNTP client at startup.
It asks the operator's server first and the default gateway second, retries while
Wi-Fi and the default route come up, adopts the answer only when it differs by
more than 30 seconds, and refreshes the stored timestamp at most every 12 hours
to limit flash wear. No public NTP pool is contacted; add

```sh
STANDALONE_NTP_HOST=192.168.1.7
```

to `standalone_auth.conf` to name a LAN time server explicitly. That matters
because many routers do not answer SNTP at all - on the tested network the
gateway stayed silent and only a LAN host could set the clock. Digest nonces and
TLS do not depend on an accurate clock; log timestamps and any future timestamp
signing do.

## Which parts are bound to this board

Everything above is written for the C220 / T8W11. This section exists because the vendor's
cameras look like a family rather than a stack per model: if another Rockchip-based Eufy
camera is tried, this is what has to be measured again, and what probably carries over as
it is. None of it is claimed to work elsewhere.

Comes from the vendor's own firmware, so likely the same on a sibling model:

- **The way in.** The vendor boot script runs `iq.sh` from a FAT32 card as root, which is
  the vendor's hook and not ours - see [Install](../README.md#install) in the README.
- **The shape of the image.** `/home` is a squashfs on `mtd6`, written through
  `/dev/block/by-name/home`, with the erase-block behaviour described above.
- **The framework around it.** `/home/script/app.sh`, `/home/app/*` (`anker_t8w11*`,
  `ipclink_eufy`, `tinymix`), `/home/nvram`, and the vendor libraries under
  `/oem/usr/lib`.

Bound to this board, and therefore to be measured first:

| what | where it is set | value here |
| --- | --- | --- |
| sensor calibration blob | `native/package-c220.sh` | `os03b10_CMK-OT2117-PC1_30IRC-F16.bin` |
| capture device | the engine and the firmware | `/dev/video12` |
| PTZ device | `firmware/src/ptz.rs` | `/dev/motor` |
| amplifier enable pin | `media-engine/src/backend/hardware.rs` | GPIO 102 |
| ALSA devices | the engine | `hw:0,0`, `hw:1,0` |
| button ladder | `native/c220-button.sh` | SARADC, pressed below 800 (idle 1020-1023, held 557-561) |
| ISP tuning | the harvested bundle | `cac_map_hw_2560x1440.bin`, `iqfiles/` |
| vendor libraries and versions | `native/camera-rootfs`, harvested per camera | `librockit`, `librkaiq`, `libsmartIr`, `librga`, `librockchip_mpp`, `libfdk-aac` |

The IP address of the wireless module and the sensor itself are the other two things that
differ in practice: both are inside the harvested bundle rather than assumed, which is why
the harvest exists.

Treat a first write on an untried model as an experiment: a full-chip dump and a factory
`/home` first, and the programmer as the fallback.
