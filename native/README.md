# Native media companion

`c220-media-engine.c` is the only native runtime source required by the Rust
firmware. It owns the camera hardware path that cannot be replaced by the
Rust service layer:

- Rockchip AIQ/V4L2/VENC video capture and encoding;
- Rockchip AI/AENC microphone capture;
- Rockchip AO speaker playback and FDK AAC decoding; and
- the shared `C220` Unix-socket record protocol.

`c220-start.S` is the minimal ARM entry point used by the camera's uClibc
runtime. The other native files from the reference project are probes or old
split helper daemons and are not part of the deployed runtime.

## Build inputs

The source, build logic, and minimum factory ARM library/IQ bundle are
contained in this repository under `native/camera-rootfs/`. Set `CAMERA_ROOT`
to another compatible extraction when building for a different factory
firmware revision. The expected layout is:

```text
<CAMERA_ROOT>/rootfs/lib/libc.so.0
<CAMERA_ROOT>/rootfs/oem/usr/lib/librockit.so
<CAMERA_ROOT>/rootfs/oem/usr/lib/librkaiq.so
<CAMERA_ROOT>/rootfs/oem/usr/lib/libsmartIr.so
<CAMERA_ROOT>/home/lib/libfdk-aac.so.1
```

The checked-in bundle contains the complete transitive library closure for
the validated camera revision. These factory libraries are not reimplemented
or replaced by the companion; they are linked dynamically and must remain at
their matching paths in the deployed camera image.

Build the companion with:

```sh
CAMERA_ROOT="$PWD/native/camera-rootfs" ./native/build-c220-media-engine.sh
```

The output defaults to `dist/c220-media-engine`. Copy that binary into the
camera image as `/home/c220-media-engine`; the Rust firmware uses that path by
default.

## Camera package

The repository includes the Rust target linker in `.cargo/config.toml` and
`native/arm-linker.sh`. Build both executables and create a `/home` overlay with:

```sh
./native/package-c220.sh
```

The default output is `dist/c220-home`. It contains `c220-firmware`,
`c220-media-engine`, `lib/libfdk-aac.so.1`, the sensor calibration blob, and
`script/c220-startup.sh`. When `BASE_HOME` is supplied, the superseded
`c220-daemon` and split `c220-hw-*` helpers are removed by default so they
cannot race the unified service and the image fits the `/home` partition. To
preserve an existing factory `/home` tree and optionally create a squashfs
image:

```sh
BASE_HOME=/path/to/preserved/home \\
INSTALL_STARTUP_HOOK=1 \\
SQUASHFS_OUT=dist/c220-home.squashfs \\
./native/package-c220.sh
```

The startup hook is appended only when `INSTALL_STARTUP_HOOK=1`; this keeps
boot-image changes explicit. `SECURE_DEPLOYMENT=1` is the default and removes
legacy media binaries plus vendor/custom SD-card, NVRAM root-script, RescueAP,
and Telnet hooks from a preserved `app.sh`. `SECURE_DEPLOYMENT=0` is only for
a deliberate offline board bring-up. `SQUASHFS_OUT` refuses images larger than
the camera's 4,915,200-byte `/home` partition. Set `REMOVE_LEGACY_MEDIA=0` only
for a deliberate compatibility/debug image. Do not flash an image until its
size, checksum, factory rollback path, and camera boot behavior have been
checked.

### Wi-Fi bootstrap

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
STANDALONE_RTSP_PORT=0
STANDALONE_RTSPS_PORT=322
STANDALONE_RTSP_CERT_FILE=/home/nvram/system/standalone_rtsp_cert.pem
STANDALONE_RTSP_KEY_FILE=/home/nvram/system/standalone_rtsp_key.pem
STANDALONE_API_TLS=1
```

Use mode `0600` for the config and both password/key files. The startup hook
maps these values to the firmware's environment. In secure mode, missing or
invalid configuration stops the service rather than opening a fallback:
`RTSP_PORT=0` disables plaintext RTSP, `RTSPS_PORT` must be nonzero, TLS 1.3
protects both RTSPS and the HTTP control API, and both listeners require Digest
credentials. `/healthz` is the only intentionally unauthenticated endpoint.
The certificate must contain the camera address in its IP SAN, and clients
must verify it against the LAN CA.

For offline board bring-up only, create the one-boot marker
`/home/nvram/system/.standalone_insecure_bringup`; it permits the legacy
plaintext/anonymous defaults for that boot and is consumed by the startup
hook. Do not create it in production. Secure packaging also removes the
unauthenticated Telnet and arbitrary SD/NVRAM root-script hooks.

For deliberate maintenance, the authenticated TLS control API exposes only
`POST /maintenance/enable`. It starts the existing `/usr/sbin/telnetd` on port
23 with `/bin/sh` for the current boot. The route is unavailable unless both
Digest credentials and the control-plane TLS acceptor are configured; the
startup script kills stale Telnet processes at the next boot. Use it only from
a trusted LAN and close/reboot the camera when finished.

### Card recovery

A camera with no working network has no other management path, and reaching it
physically is slow. `script/c220-recovery.sh` therefore keeps a narrowly bounded
card-based path that runs before the services start. The card is mounted
read-only with `nosuid,nodev,noexec`, and only two files are recognised:

| file | effect |
| --- | --- |
| `wifi.txt` | `SSID=...`, `PSK=...`, `MODE=WPA\|WEP\|NONE`. Parsed as data, never sourced. Writes the supplicant profile the Wi-Fi bootstrap reads, so the camera can be moved to a new network with no console |
| `maintenance.txt` | starts the root shell on port 23 for that boot only |

Nothing else on the card is read or executed: there are no `custom.sh`, factory,
SD or NVRAM script hooks, and `maintenance.txt` is the only way to a shell that
does not need the network. `wifi.txt` is re-applied on every boot the card is
present, so a card left in the slot keeps overriding the stored network.

Treat the card slot as a credential. Anyone who can insert a card can open a root
shell or point the camera at another network - the same physical access the
device cannot defend against - so keep the recovery card where the camera's
credentials deserve to be, and leave it out of the camera in normal operation.
The recovery log is written to `/tmp/c220-recovery.log` on each boot.

Microphone capture is opt-in. Create the root-owned marker
`/home/nvram/system/.standalone_audio_enable` before starting the service when
RTSP microphone audio is required; leave it absent to keep capture disabled.

### Clock

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
