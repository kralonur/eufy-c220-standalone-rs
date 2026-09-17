# Recovery card

A card the camera reads at boot to recover a setup that cannot be reached over the
network. Nothing on it is executed: these are data files, parsed by
`native/c220-recovery.sh` on the camera, and the card itself is mounted read-only
with `nosuid,nodev,noexec`.

Copy the files you need to the root of a FAT32 card, edit the values, power the
camera off and on, and **remove the card when you are done**. `wifi.txt` and
`provision.txt` are re-applied on every boot the card is present, so a card left in
the slot keeps overriding the network and the credentials.

| file | effect |
| --- | --- |
| `wifi.txt` | `SSID`, `PSK`, `MODE` — moves the camera to another network. Nothing else changes. |
| `provision.txt` | network **plus** `RTSP_USER`, `RTSP_PASSWORD`, `HTTP_USER`, `HTTP_PASSWORD`, and optionally `RTSPS_PORT`, `NTP_HOST`, `HOST_NAME`. This is the whole setup for a new camera. |
| `maintenance.txt` | read for its presence only: starts the root shell on port 23 for that boot. |
| `cert.pem` + `key.pem` | install a certificate and key instead of letting the firmware generate a self-signed pair. Both are required together. |

## Format

Plain text, one `KEY=value` per line. Comment lines starting with `#` are ignored,
quotes around a value are optional, surrounding spaces are trimmed, and only the
first line for a key is read. The rules for each value - what it may contain and what
the camera does with it - are in the
[card file formats reference](../../docs/setup-and-recovery.md#card-file-formats) of the
setup and recovery guide, which is the one place they are written down. A file that fails
validation is rejected as a whole rather than applied in part, and the reason is
written to `/tmp/c220-recovery.log` on the camera.

## An unprovisioned camera needs no card

A camera with no stored configuration is already reachable: it joins the rescue
network and opens the same root shell by itself, and `provision.txt` on a card is
the way to provision it without using that shell. Either route works; the card is
the one that needs no serial console and no knowledge of the rescue network.

## Security

- The card slot is a credential. Anyone who can insert a card can open a root shell
  or point the camera at another network.
- Passwords are never written to the log, but they are plain text on the card:
  format it afterwards, or keep it somewhere the credentials deserve.
- `cert.pem`/`key.pem` install a key onto the camera. Installing the same pair on
  several cameras makes them indistinguishable to a client that pins it.
