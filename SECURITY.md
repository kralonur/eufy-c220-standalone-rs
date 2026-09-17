# Security

## Deliberate, and not a vulnerability

Each of these is a design decision with a reason. Please read them before reporting.

- **The card hook in [README.md](README.md#install)
  (`iq.sh`) is the vendor firmware's, not this project's.** Before this firmware is
  installed, a card in the slot makes the factory firmware run a script as root on
  every boot. That is how the camera is entered without a programmer. This project
  cannot remove it and does not add it; it exists on every camera of this model,
  installed or not, and it goes away when the factory `/home` is restored.
- **The maintenance shell** (root `telnetd` on port 23) is one boot at a time and is
  armed either through the authenticated API (`POST /maintenance/enable`) or by a
  card carrying `maintenance.txt`. Both need physical access or credentials, and the
  startup script kills a stale shell at the next boot. This firmware never ships an
  always-on unauthenticated shell.
- **Card files are data, never executed.** Recovery parses `wifi.txt`,
  `provision.txt`, `cert.pem` and `key.pem` as key/value text from a card mounted
  `ro,nosuid,nodev,noexec`, and `maintenance.txt` is read for its presence only.
  There are no `custom.sh`, factory, SD or NVRAM script hooks in this firmware.
- **A card in the slot is treated as a credential.** Whoever can insert one can open
  a shell or move the camera to another network. That is the same physical access the
  device cannot defend against, and the reason the card is documented as something to
  remove and store like a password.
- **Everything that requires the programmer, the UART console or the card slot** is
  physical access by design.
- **`/healthz` is unauthenticated on purpose**, and answers only whether the service
  is up. Every other endpoint needs Digest credentials over TLS.
- **Digest with MD5 is offered for RTSP** alongside SHA-256, because several common
  RTSP clients refuse a challenge that lists both. SHA-256 is what the control API
  and MQTT use.
- **The certificate is self-signed and public.** It is generated on first boot so a
  new camera needs no CA ceremony; clients pin it or trust it explicitly. Fetching it
  from the camera is expected behaviour, not a leak.
- **mDNS publishes names, never fingerprints**, so discovery cannot be used to make a
  client trust the wrong certificate.

## Reporting a vulnerability

Report it privately rather than in a public issue: use the repository's private
vulnerability reporting if it is enabled, or write to the maintainer address in the
commit history. Please include the firmware hash (`md5sum /home/c220-firmware`), the
camera revision, what triggers it, and any log line that shows it - a claim about
this device is only actionable with the device's state attached.

There is no bug bounty. Expect an acknowledgement, a fix if it is real, and credit
unless you would rather not be named.

## Out of scope

- Physical access of any kind: the card slot, the UART console, the flash programmer,
  and anything that follows from having the camera in your hands.
- The vendor firmware, the vendor app and the vendor cloud, which this project does
  not modify or interact with.
- The factory bootloader, kernel and drivers, which are left untouched.
- Anything that requires the credentials, and clients that ignore certificate
  verification.
