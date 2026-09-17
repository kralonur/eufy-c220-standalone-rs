# eufy C220 / T8W11 camera — standalone firmware

Runs a Eufy C220 (T8W11) indoor camera as a plain LAN device: RTSP over TLS, an
authenticated HTTPS control API, MQTT control and telemetry, talkback, PTZ, night
mode, and mDNS discovery — with **no Eufy P2P, no cloud, and no vendor app**.

The factory bootloader, kernel, drivers and sensor calibration are left untouched. The
Rust firmware owns the services and control plane, and the Rust `media-engine`
companion owns the Rockchip media hardware (VI/VENC/AO/AI plus the ISP tuning
library).

## What it does

- **RTSPS** on port 322, H.265 2304x1296 plus G.711 microphone audio (opt-in, see
  below) and an ONVIF-style talkback backchannel, Digest authentication.
- **HTTPS API** on 8080 (TLS + Digest): `/status`, `/ptz/*`, `/night_mode`,
  `/talkback/*`, `/healthz`, and the privileged `/maintenance/enable` and `/reboot`.
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

## Where to go

Installing is the [section below](#install) - this page
carries the whole procedure, start to finish. Everything around it is one of these:

| what you are doing | read |
| --- | --- |
| installing on a camera, or handing one to someone else | [Install](#install) below, then the [setup and recovery guide](docs/setup-and-recovery.md) |
| credentials, certificates, Wi-Fi, the camera's name | [setup and recovery guide](docs/setup-and-recovery.md) |
| using it from Home Assistant, Frigate, go2rtc or MediaMTX | [setup and recovery guide](docs/setup-and-recovery.md) and the client notes in [camera internals](docs/camera-internals.md#receiving-rtsps-sdp-direction-attributes-and-certificate-verification) |
| resetting a camera, or recovering one that is unreachable | [setup and recovery guide](docs/setup-and-recovery.md) |
| flashing an image, going back to the factory firmware, or a camera that will not boot | [camera internals](docs/camera-internals.md) |
| the card files, and what each one does | [recovery card](docs/recovery-card/README.md) |
| building the firmware, the engine and the image | [Build](#build) below, then [building](docs/building.md) |
| contributing | [CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md) |

## Supported hardware

Eufy C220 / T8W11 indoor camera - the revision whose factory firmware carries the
`os03b10` sensor (`CMK-OT2117-PC1`, 30IRC F16). The package script expects that
sensor's calibration blob by name, and the ISP tuning data and the media timings were
validated against it, so a different revision is not a supported target yet. Check
yours from the card shell before writing anything to flash:

```sh
ls /home/os03b10_*.bin
```

A file there means the harvest and the build work as documented. Nothing there means
the camera is a different revision: read [`docs/camera-internals.md`](docs/camera-internals.md) first.

### Other Eufy cameras

This is built and verified for the C220 / T8W11, and only for that. But the vendor does
not appear to write a firmware stack per model: the way in, the shape of the image and
the boot script this project drives are all *theirs*, and they are shared across a line
of Rockchip-based cameras. Since the entry point is that shared path rather than anything
C220-specific, another camera from the same family is a plausible target rather than a
hopeless one - untested, and not claimed.

What would have to be re-measured is in
[camera internals](docs/camera-internals.md#which-parts-are-bound-to-this-board): the sensor
calibration blob, the capture and PTZ device nodes, the amplifier pin and button ladder,
the audio devices, the ISP tuning files, and the versions of the vendor libraries. Take a
full-chip dump and a factory `/home` before writing anything, and read
[Recovery](#recovery) first.

## Install

Nine steps, once. Steps 1-5 put this firmware on the camera, steps 6-9 give it your
Wi-Fi and passwords. The detail behind any step - card file rules, certificate choices,
what to do when something goes wrong - is in
[the setup and recovery guide](docs/setup-and-recovery.md).

### Before you start

- the camera, powered from its USB-C lead;
- a microSD card, FAT32, any size, and a reader for your computer - the same card
  carries the scripts, the harvested files and the firmware image;
- a computer with this repository and its build environment on it: `nix develop`
  brings the pinned toolchain, or the manual prerequisites are under [Build](#build);
- your Wi-Fi name and password (the radio is 2.4 GHz only);
- nothing else - no account, no phone app, no programmer.

### 1. Put two scripts on a card

Format a microSD card as FAT32 (the vendor boot script mounts it `-t vfat`), then copy
both scripts from `native/factory-bootstrap/` to its root: `iq.sh`, which opens a root
shell on the camera, and `harvest.sh`, which collects the vendor files the build needs.

### 2. Power on, and get that shell

Insert the card and power the camera on. The **vendor** firmware mounts the card at
`/mnt/sdcard` and, after starting its own application, runs `/mnt/sdcard/iq.sh` as
root. That starts `telnetd` on port 23 and writes `iq_boot.log` to the card with the
network configuration, the process list and `/proc/mtd`. Read the card to learn the
camera's address, then:

```sh
telnet <camera-ip>
```

The same script copies `/home/nvram` to `eufy_nvram_backup/` on the card the first
time it runs. That is the cheapest copy of the camera's identity, Wi-Fi profile and
credentials you will ever get; keep it.

### 3. Harvest the vendor inputs

In that shell:

```sh
sh /mnt/sdcard/harvest.sh
```

It writes `camera-rootfs/` (the vendor libraries, ISP tuning data and sensor blob,
which this repository does not redistribute) and `base-home/` (the factory `/home`
tree) to the card, checks what it collected, and prints `HARVEST OK`. It skips the
live NVRAM on purpose, and it refuses to make `base-home` from a camera that already
runs this firmware, because that is not the factory tree.

### 4. Build the image

On your computer, in the repository. Run `nix develop` first for the pinned toolchain,
or install the manual prerequisites listed under [Build](#build); then:

```sh
cp -a /path/to/card/camera-rootfs native/camera-rootfs

# media engine (Rust no_std + alloc; links the camera's librockit + librkaiq)
CAMERA_ROOT="$PWD/native/camera-rootfs" ./native/build-rust-media-engine.sh

# firmware + /home overlay + squashfs image
BASE_HOME=/path/to/card/base-home INSTALL_STARTUP_HOOK=1 \
SQUASHFS_OUT=dist/c220-home.squashfs ./native/package-c220.sh
```

### 5. Write it, from the camera

Copy the image to the same card. It has to be padded first, and the padding is not
optional - the write discards its last erase block:

```sh
# on the computer: pad to a 64 KiB multiple, plus the one block the write drops
cp dist/c220-home.squashfs /tmp/write.img
truncate -s $(( ($(stat -c %s /tmp/write.img) + 65535) / 65536 * 65536 + 65536 )) /tmp/write.img
echo "$(( $(stat -c %s /tmp/write.img) / 65536 )) blocks to write"
```

Then, in the camera shell, with the card mounted at `/mnt/sdcard` (use the block count
printed above, and read the whole thing back before trusting it):

```sh
dd if=/mnt/sdcard/write.img of=/dev/block/by-name/home bs=65536 count=<blocks> conv=notrunc
dd if=/dev/mtd6 bs=65536 count=<blocks - 1> | md5sum     # must equal the image
```

If the read-back does not match, **do not reboot**. A `/home` written without a
verified read-back is a camera that mounts nothing on the next boot; the traps and the
way out are in
[camera internals](docs/camera-internals.md#flashing-the-home-partition-on-this-board).

### 6. Reboot: installed, not yet provisioned

The firmware fails closed. With no credentials it starts no service and instead joins
the `RescueAP` hotspot and opens the same root shell, waiting to be provisioned. Step 7
is the easier way to do that.

### 7. Give it your Wi-Fi and passwords

Put a file named exactly `provision.txt` in the root of the card:

```sh
SSID=MyNetwork
PSK=my-wifi-password
RTSP_USER=frigate
RTSP_PASSWORD=the-rtsp-password-you-generated
HTTP_USER=admin
HTTP_PASSWORD=the-http-password-you-generated
```

Insert the card, power the camera on, and it provisions itself on that boot. All four
credentials are required; `MODE` defaults to `WPA` and `RTSPS_PORT` to `322`. The full
key list, the validation rules, installing your own certificate, and the route for a
camera with no card reader are in
[`docs/setup-and-recovery.md`](docs/setup-and-recovery.md).

### 8. Check it is alive

```sh
nc -vz <camera-ip> 322                        # video port listening
curl -k https://<camera-ip>:8080/healthz      # -> {"status":"ok","ready":true}
```

The camera also answers to `<name>.local` over mDNS (`camera-<serial>.local` by
default), so you rarely need the address. Then point your NVR at it:

```text
rtsps://<RTSP_USER>:<RTSP_PASSWORD>@<camera-ip>:322/live
```

Until you install your own, the certificate is the self-signed one the camera made on
first boot, so a client either trusts it, pins it, or you supply your own - all three
are in the [setup and recovery guide](docs/setup-and-recovery.md), and the client traps
that waste an afternoon otherwise are in
[camera internals](docs/camera-internals.md#receiving-rtsps-sdp-direction-attributes-and-certificate-verification).

### 9. Remove the card

While it is in the slot, the camera boots with an unauthenticated root shell on port
23, every time. That is the vendor's behaviour, not something this project adds, and it
is why the recovery paths in this firmware are data-only. Take the card out and keep it
somewhere a credential deserves.

## Recovery

- **The camera boots but the services misbehave**: arm the one-boot maintenance shell
  through the authenticated API, or put the recovery files on a card. Templates for
  that card, with the format and the rules, are in
  [recovery card templates](docs/recovery-card/README.md); the owner's playbook is
  [`docs/setup-and-recovery.md`](docs/setup-and-recovery.md).
- **The camera does not boot**: a `/home` that is unreadable cannot be repaired from
  the network or from the card, so the fallback is the UART console
  (`ttyFIQ0`, 1500000 baud) or programming the flash chip with a CH341A. Keep a
  full-chip dump taken while the camera worked — that dump is the undo for everything
  else. The layout, the exact `flashrom` commands and the traps are in
  [camera internals](docs/camera-internals.md#chip-level-recovery-ch341a-uart).
- **Going back to the factory firmware**: restore the `/home` image you dumped before
  installing this one. It is the same write as any update, with one caveat about the
  last erase block - see
  [camera internals](docs/camera-internals.md#going-back-to-the-factory-firmware).

The vendor partitions below `/home` are never written by this project.

## Build

The whole environment is pinned in `flake.nix` (requires flakes):

```sh
nix develop          # rustc 1.98.1, clang 22.1.8, ARM target, qemu, squashfs-tools
cargo test --workspace
cargo build --release --target armv7-unknown-linux-musleabihf -p c220-firmware
```

The same tasks are available without entering the shell, from anywhere in the
repository. `nix run .` lists them, and each runs the script or cargo command the
documentation names:

```sh
nix run .#check     # fmt, clippy, tests, ARM build, size budget
nix run .#build     # the ARM firmware binary
nix run .#size      # section sizes of that binary
nix run .#bloat     # what is taking the space, per crate and per function
nix run .#engine    # the media engine (needs native/camera-rootfs)
nix run .#package   # the /home overlay and image (needs BASE_HOME)
nix run .#verify    # the post-write gate, against a camera
```

**Build in one environment.** Cargo fingerprints the compiler by its version string,
so the pinned rustc and a distribution `1.98.1` are indistinguishable to it: a host
build and a Nix build sharing one `target/` reuse each other's objects, and the result
is a binary from two compilers with a size that means nothing. `native/check.sh`
records which toolchain built there and says so when it changes.

Without Nix the same environment is: rustup plus
`rustup target add armv7-unknown-linux-musleabihf`; clang with the ARM backend,
which is both the linker driver and the C compiler for `ring`; `mksquashfs` and
`unsquashfs`; `flashrom`; `openssl`; and `cargo-bloat`, `cargo-binutils` and
`cargo-machete` for the size checks.

Three toolchain details are pinned deliberately, because each one changes the
produced binary:

- the shell uses clang **unwrapped**, because nixpkgs' `clang` is a cc-wrapper
  that adds host hardening flags (`-fzero-call-used-regs=used-gpr`) which the ARM
  target rejects;
- `-fstack-protector-strong` is named explicitly, because Debian and Ubuntu build
  clang with it on and nixpkgs does not, which would otherwise compile `ring`'s C
  without stack canaries;
- `rust-src` is deliberately **not** installed: with it present, rustc rewrites
  the `/rustc/<hash>` std paths in panic locations to the local source tree, so
  the firmware would embed the toolchain directory of whoever built it.

Building the two deployable artifacts also needs the proprietary vendor inputs, which
this repository does not redistribute (see the licensing note below). The card step
above is how you get them, and it puts them where the build expects to find them:

```sh
cp -a /path/to/card/camera-rootfs native/camera-rootfs

# media engine (Rust no_std + alloc; links the camera's librockit + librkaiq)
CAMERA_ROOT="$PWD/native/camera-rootfs" ./native/build-rust-media-engine.sh

# firmware + /home overlay + squashfs image
BASE_HOME=/path/to/card/base-home INSTALL_STARTUP_HOOK=1 \
SQUASHFS_OUT=dist/c220-home.squashfs ./native/package-c220.sh
```

The image must fit the 4,915,200-byte `/home` partition. `docs/building.md` says what
each input is for, and `harvest.sh` documents the layout it writes.

The shell cannot supply what this repository does not redistribute. Without
`native/camera-rootfs` the tests and the ARM firmware build work, but the media
engine and a `/home` image do not; the shell says which is missing when it starts.
Its emulator is `qemu-arm`, which Debian ships as `qemu-arm-static`.

## Design rules this project follows

- **Fail closed.** `RTSP_PORT=0`, `RTSPS_PORT=322`, `API_TLS=1` by default. There is no
  firmware upload or flash endpoint. Two privileged routes exist, both over TLS with
  Digest auth: `POST /maintenance/enable` opens a one-boot Telnet shell, and
  `POST /reboot` restarts the camera.
- **The card slot is a credential.** Recovery reads data; it never executes card
  content.
- **Trust is out of band.** mDNS publishes names, never fingerprints; verification
  comes from a certificate, a CA, or a pin you installed yourself.
- **A pin that survives a new certificate is not a pin.** Rotation is the documented
  path, not a workaround.

## Status

Working and hardware-verified: RTSPS with verified TLS, the control API, MQTT,
talkback (with a queued speaker path), continuous PTZ, night mode, mDNS, factory
reset and SD-card recovery.

The card-based install is the path the reference work on this camera used: the script
in `native/factory-bootstrap/iq.sh` is the one that was run, unchanged, and the vendor
hook it relies on is read from the camera's own `app.sh`.

Known gaps, stated plainly: talkback is smooth only when the client feeds it at real
time, because the camera does not invent audio it never received; the UART console
expectations in [`docs/camera-internals.md`](docs/camera-internals.md) have not been verified with an adapter; and
long-term stability over weeks is unproven rather than assumed.

## Contributing

`native/check.sh` runs the gate a change should pass: formatting,
clippy with warnings denied, the tests, the ARM release build and the size budget.
None of it touches a camera, so a change that claims hardware behaviour needs a
device test described in the change itself. [CONTRIBUTING.md](CONTRIBUTING.md) covers
the size discipline, what not to commit, and what to attach to a bug report;
[SECURITY.md](SECURITY.md) covers what is deliberate on this device and how to report
something that is not.

The firmware is written into a 4,915,200-byte `/home` partition, so footprint is part
of correctness rather than a curiosity, and the gate refuses a build that grows past
its budget.

## Licence

The code is dual licensed under either of

- the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- the MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option, which is the usual Rust arrangement and the one contributions are
accepted under.

That covers the code only. This repository intentionally contains no vendor firmware,
no sensor calibration data and no proprietary libraries: the factory libraries the
media engine links against are collected from your own camera by
`native/factory-bootstrap/harvest.sh`, stay on your machine, and are not covered by
this licence or redistributed here.

**There are no prebuilt images, for the same reason**: a built `/home` image embeds
the vendor AAC decoder and your camera's sensor calibration blob, so publishing one
would redistribute vendor files. Building from your own harvest is the supported
route, and [`native/check.sh`](native/check.sh) is what makes it repeatable.
