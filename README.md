# eufy C220 / T8W11 camera — standalone firmware

Runs a Eufy C220 (T8W11) indoor camera as a plain LAN device: RTSP over TLS, an
authenticated HTTPS control API, MQTT control and telemetry, talkback, PTZ, night
mode, and mDNS discovery — with **no Eufy P2P, no cloud, and no vendor app**.

The factory bootloader, kernel, drivers and sensor calibration are left untouched. The
Rust firmware owns the services and the control plane; a small C engine owns the
Rockchip media hardware (VI/VENC/AO/AI plus the ISP tuning library).

## What it does

- **RTSPS** on port 322, H.265 2304x1296 plus G.711 audio, Digest authentication.
- **HTTPS API** on 8080 (TLS + Digest): `/status`, `/ptz/*`, `/night_mode`,
  `/talkback/*`, `/snapshot`, `/healthz`.
- **MQTT** with Home Assistant discovery: availability, status, PTZ, night mode,
  talkback state; commands reuse the same handlers as the HTTP API.
- **mDNS**: each camera answers to `<name>.local` and advertises `_rtsp._tcp`, with the
  name derived from the board's own serial so two cameras cannot collide.
- **Its own certificate**: generated on first boot (ECDSA P-256), naming the host and
  the camera's current addresses. Rotating it is documented, because verified clients
  pin it.
- **Two-tier physical button**: 5 s hold joins a rescue network for a maintenance
  shell; 10 s hold performs a full factory reset of everything this firmware added.
- **SD-card recovery** that is deliberately data-only: `wifi.txt`, `provision.txt`,
  `cert.pem`/`key.pem`, `maintenance.txt`, parsed and never sourced, from a card
  mounted `ro,nosuid,nodev,noexec`.

## Design rules this project follows

- **Fail closed.** `RTSP_PORT=0`, `RTSPS_PORT=322`, `API_TLS=1` by default. There is no
  firmware upload, flash or reboot endpoint. Only `POST /maintenance/enable` — over TLS
  with Digest auth — opens a one-boot Telnet shell.
- **The card slot is a credential.** Recovery reads data; it never executes card
  content.
- **Trust is out of band.** mDNS publishes names, never fingerprints; verification
  comes from a certificate, a CA, or a pin you installed yourself.
- **A pin that survives a new certificate is not a pin.** Rotation is the documented
  path, not a workaround.

## Build and deploy

```sh
# media engine (C, links librockit + librkaiq from the camera's own rootfs)
CAMERA_ROOT="$PWD/native/camera-rootfs" ./native/build-c220-media-engine.sh

# firmware + /home overlay + squashfs image
BASE_HOME=/path/to/preserved/home INSTALL_STARTUP_HOOK=1 \
SQUASHFS_OUT=dist/c220-home.squashfs ./native/package-c220.sh
```

The image must fit the 4,915,200-byte `/home` partition. See `native/README.md` for
flashing: `flashcp` is a stub on this board, and writes to the mounted `/home` block
device must be erase-block aligned (64 KiB) or the final erase block is silently
dropped.

## Documentation

- `docs/onboarding-and-reset.md` — the owner's guide: setup, reset, recovery
  playbook, certificate rotation.
- `native/README.md` — hardware notes, discovery, chip-level recovery, TLS
  verification and pinning, and the operational traps found on this board.

## Status

Working and hardware-verified: RTSPS with verified TLS, the control API, MQTT,
talkback (with a queued speaker path), continuous PTZ, night mode, mDNS, factory
reset and SD-card recovery.

Known gaps, stated plainly: the first command after a boot (audio or motor) can be
lost; speaker audio is not yet smooth under load; and long-term stability over weeks
is unproven rather than assumed.

## Licensing note

This repository intentionally contains no vendor firmware, no sensor calibration data
and no proprietary libraries. You supply those from your own camera dump; the paths
are documented in `native/README.md`.
