# Eufy C220 standalone firmware - setup, reset and recovery

This camera has been converted to run a standalone firmware: no Eufy account, no
cloud, no phone app. It speaks **RTSPS** (encrypted RTSP) to your NVR or media
server and exposes a small **HTTPS control API** for pan/tilt, night mode and
talkback. Everything except `/healthz` requires authentication.

This document covers the three things you will actually do in practice:

- **Set up a camera** that is new or has just been reset ([Part 1](#part-1-set-up-a-camera))
- **Reset a camera** back to a blank state, or put it into rescue mode ([Part 2](#part-2-reset-and-rescue))
- **Get out of trouble** when the camera is unreachable ([Part 3](#part-3-recovery-playbook))

If you only read one thing: **a camera with no credentials is not reachable on
your network** - it waits for you to provision it. Setting it up means giving it
your Wi-Fi details and the passwords you want, and the easiest way to do that is
a microSD card with one text file on it.

---

## What you need

| item | notes |
| --- | --- |
| The camera | powered from its USB-C lead, nothing else needed |
| A microSD card | **FAT32**, any size, any label. It does not need to be empty |
| A card reader for your computer | this is what makes setup a one-step job |
| A plain text editor | Notepad, TextEdit, VS Code - anything that saves plain text |
| Your Wi-Fi name and password | a 2.4 GHz network - this model's radio is 2.4 GHz |
| A phone that can share a hotspot | **only** needed if you have no card reader (see [Route B](#route-b-no-card-reader-use-the-rescue-hotspot)) |

Decide now which route you will use:

- **You have a card reader** -> [Route A: SD card](#route-a-sd-card-recommended). Recommended. No hotspot, no terminal, no shell.
- **You have no card reader** -> [Route B: rescue hotspot](#route-b-no-card-reader-use-the-rescue-hotspot). Works, but you will type commands.

### Facts worth knowing before you start

| | |
| --- | --- |
| Ports after setup | **322** RTSPS (video), **8080** HTTPS (control API). Plaintext 554 is off, port 23 is closed |
| Time to come up | about **17 seconds** to start serving, about **22 seconds** until video flows |
| Certificate | the camera makes its own self-signed one on first boot, unless you supply one. Your client has to trust it once |
| Name | the camera answers to `<name>.local` over mDNS, `camera-<serial>.local` by default, so you never have to remember its address |
| Where the card goes | the microSD slot. **Insert the card before powering on** - the camera has no card-detect line and cannot notice a card inserted while running |
| What the card contains | your Wi-Fi password and camera passwords, in plain text. **Take it out when you are done** and keep it somewhere sensible |

---

## Part 1: set up a camera

### Step 1: choose your credentials

You need two username/password pairs. They are separate on purpose: your NVR gets
the RTSP pair, your control scripts get the HTTP pair.

| setting | used by | guidance |
| --- | --- | --- |
| `RTSP_USER` / `RTSP_PASSWORD` | the video stream (port 322) | digest auth, sent on every request, so keep it long. Lowercase letters, digits and `-` are safest for NVR URL fields |
| `HTTP_USER` / `HTTP_PASSWORD` | the control API (port 8080) | same rules |

Rules the camera enforces: usernames are 1-32 characters of `A-Z a-z 0-9 . _ @ -`;
passwords are 1-128 characters with no control characters, quotes or backslashes.
Passwords may contain spaces and symbols such as `!#%` - just not quotes or `\`.

Generate them rather than inventing them, for example `openssl rand -base64 24`.

### Step 2: decide about the certificate

The camera must serve TLS, so either way something has to make your clients trust
the certificate it presents. Choose by what you already run:

| you have | do this | client work afterwards |
| --- | --- | --- |
| a private CA on your network (Home Assistant, pfSense, step-ca, mkcert) | sign a certificate for the camera's address, put `cert.pem` and `key.pem` on the provisioning card | **none** - your clients already trust that CA |
| a computer with openssl | generate the pair yourself with the command below, put it on the card | add that one certificate to your client's trusted certificates, once |
| neither | nothing: the camera generates its own on first boot | fetch it from the camera and trust it once ([A5](#a5-trust-the-certificate)) |

All three give the same TLS 1.3 encryption; they differ only in who vouches for
the certificate. A self-signed certificate - the camera's or your own - cannot be
verified by anyone else, so it always ends in a one-time client step. A
certificate signed by a CA your clients already trust is the only option that
removes that step, so it is worth doing if you already run one.

Generating the pair yourself is not more secure than letting the camera do it. It
gives you control over the names and the lifetime, and it keeps the private key on
your machine instead of generating it on the camera. The camera never overwrites a
pair you installed.

To make a pair for a camera at `192.168.1.55`, valid for that address and for the
name `c220.local`:

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout key.pem -out cert.pem -days 3650 \
  -subj "/O=eufy-c220-standalone/CN=c220 camera" \
  -addext "subjectAltName=IP:192.168.1.55,DNS:c220.local"
```

The SAN must contain the address or name your client will connect to - that is
what the client checks. If the camera uses DHCP, either reserve its address or
include the name you actually use. The same rule applies if you sign with your own
CA.

None of this is required for the camera to work: with no certificate files it
makes its own on first boot and says so in the log. The rest of this section is
for when you want to supply one yourself.

The full walkthrough, including signing with your own CA and the mistakes that
make a camera refuse to start, is [Using your own certificate](#using-your-own-certificate)
further down.

### Step 3: know your camera's name

Every camera answers to a name of its own over **mDNS**, derived from the board's
serial number, so it is `camera-<serial>.local` - the serial printed on the camera,
which means the name you see on the network can be matched to the device in your
hand. Nothing needs configuring, and no two cameras can collide.

The name comes from the first identifier the board offers: the vendor PCB serial
(`.pcbsn`, which a factory reset keeps), then the SoC serial, then the Wi-Fi MAC
address. If a camera later loses its NVRAM identity, the name changes with it.

It is worth knowing about for three reasons:

- **You can stop remembering addresses.** `ping c220.local`, `telnet c220.local`,
  and `rtsps://frigate:password@c220.local:322/live` all work from any device on the
  same network that speaks mDNS (macOS, iOS, Linux, Windows 10+).
- **The certificate follows the name.** A camera-generated certificate is issued for
  `<name>.local` plus its current addresses, so dialling the name verifies even if
  DHCP moves the camera. The name is the part that never goes stale.
- **Two cameras need two names.** Derived names are unique because serial numbers
  are, so a second camera introduces no conflict and needs no configuration.

To choose a name yourself, put one line in the provisioning file:

```sh
HOST_NAME=front-door
```

The camera then answers `front-door.local` and stops using the derived name.
Camera-generated certificates name whichever is in force. If you sign your own
certificates, put the same name in the SAN.

Caveats worth knowing: **Docker containers cannot resolve `.local` names by
themselves** - the container has its own resolver and no mDNS (see
[your own certificate](#using-your-own-certificate) for the one-line hosts entry that
solves it), Android apps generally cannot either, and mDNS is local to one network
segment: it does not cross VLANs or routers.

### Route A: SD card (recommended)

#### A1. Write one file on the card

Put the card in your computer and create a file named exactly
`provision.txt` in the **root** of the card (not inside a folder):

```sh
SSID=MyNetwork
PSK=my-wifi-password
RTSP_USER=frigate
RTSP_PASSWORD=the-rtsp-password-you-generated
HTTP_USER=admin
HTTP_PASSWORD=the-http-password-you-generated
```

That is the minimum. `MODE` defaults to `WPA`, and `RTSPS_PORT` defaults to `322`.
Optional lines, if you want them:

```sh
MODE=WPA
RTSPS_PORT=322
NTP_HOST=192.168.1.1
```

| line | required | meaning |
| --- | --- | --- |
| `SSID` | yes | your Wi-Fi network name, up to 32 characters |
| `PSK` | yes for `WPA` | your Wi-Fi password |
| `MODE` | no | `WPA` (default), `WEP`, or `NONE` for an open network |
| `RTSP_USER`, `RTSP_PASSWORD` | yes | video credentials |
| `HTTP_USER`, `HTTP_PASSWORD` | yes | control API credentials |
| `RTSPS_PORT` | no | video port, default `322` |
| `NTP_HOST` | no | a LAN time server. Optional, but this board has no clock battery, so the time is wrong after every power cut until it syncs. Your router is usually fine here; if it does not answer SNTP, use any always-on machine |

Notes:

- One line per setting, no quotes needed, no spaces around `=`. Values keep
  everything after the `=`.
- If anything fails validation the whole file is rejected and **nothing** is
  written - the camera will not end up half configured.
- On Windows, make sure the file is really named `provision.txt` and not
  `provision.txt.txt`.

#### A2. Put your certificate on the card

If you decided to supply a certificate, copy it to the card as `cert.pem` and the
matching private key as `key.pem`. Both are required: if only one is present the
camera ignores both, logs the problem and generates its own instead. Skip this step
if you are letting the camera generate its own.

#### A3. Insert and power on

1. Insert the card into the camera's microSD slot.
2. Power the camera on.
3. Wait about 20 seconds.

The camera writes the configuration **before starting any service**, so it comes
up configured on that same boot. In order it: mounts the card read-only, installs
your certificate if you supplied one, writes both password files, writes the
configuration, writes the Wi-Fi profile, then starts serving. If it has no
certificate it generates one at this point, using its current IP address as a
name in the certificate.

#### A4. Check it is alive

Find the camera's address in your router's DHCP client list (or your hotspot
client list). Then, from a computer on the same network:

```sh
# is the video port listening?
nc -vz <camera-ip> 322

# is the control API answering? (healthz needs no password)
curl -k https://<camera-ip>:8080/healthz
# -> {"status":"ok","ready":true}
```

`-k` is only to skip certificate verification for this one check; the next step
does it properly.

#### A5. Trust the certificate

The camera's self-signed certificate is public information, so fetch it from the
camera itself and give it to your client:

```sh
openssl s_client -connect <camera-ip>:322 -servername <camera-ip> </dev/null 2>/dev/null \
  | openssl x509 -outform PEM > c220.pem
```

That file is both the certificate and its own issuer, so it works as a CA file:

```sh
# control API, verified properly
curl --cacert c220.pem --digest -u admin:<http-password> https://<camera-ip>:8080/healthz

# video, 3 seconds of it
ffprobe -v error -rtsp_transport tcp -ca_file c220.pem \
  -i "rtsps://frigate:<rtsp-password>@<camera-ip>:322/live" \
  -select_streams v:0 -show_entries stream=codec_name,width,height -of csv=p=0
```

In your NVR, point the camera's CA file at `c220.pem`; the stream URL is
`rtsps://frigate:<rtsp-password>@<camera-ip>:322/live`. If a client cannot take a
CA file, turning off certificate verification for that one camera is the usual
fallback (`-tls_verify 0` in ffmpeg) - the stream is still encrypted, you just
lose protection against something on your LAN pretending to be the camera.

If you would rather not pin a certificate, put a real one on the provisioning
card ([A2](#a2-put-your-certificate-on-the-card)) - the camera will never
overwrite it.

#### A6. Remove the card

The card now holds your Wi-Fi password and both camera passwords in plain text.
Take it out. Keep `provision.txt` somewhere safe if you like - it is also your
"re-provision this camera" card, and the camera reads it again on any boot where
the card is present.

### Route B: no card reader, use the rescue hotspot

The camera cannot create its own hotspot - the factory firmware has no access
point software in it - so instead it **joins** one. This is the same mechanism
the vendor firmware used.

#### B1. Create the hotspot

On your phone, share a hotspot with:

- **Name:** `RescueAP`
- **Password:** `12345678`
- Security: WPA2

(You can change these later with `standalone_rescue.conf` on a card or over the
shell, but a camera that has just been reset uses these defaults.)

A Linux PC with NetworkManager can host the same hotspot, which is convenient if
you are already sitting at one:

```sh
# the interface needs to support AP mode: iw list | grep -A5 'Supported interface modes'
nmcli device wifi hotspot ifname wlan0 ssid RescueAP password 12345678 band bg
```

Do this on a machine that reaches the camera by Ethernet or not at all - the
camera is about to leave your normal network. The camera appears as a client as
soon as it joins (`ip neigh show dev wlan0`), and when you are finished:
`nmcli connection delete Hotspot`.

#### B2. Power on the camera and find it

Power the camera on and wait about 30 seconds. Look in your phone's hotspot
client list for a new device - that is the camera. Note its address
(something like `192.168.43.x`).

#### B3. Open a shell

The camera opens a root shell on **port 23** when it has no configuration, which
is exactly the state you are in:

```sh
telnet <camera-ip>
```

macOS removed `telnet`; install it (`brew install telnet`) or use PuTTY on
Windows, which speaks telnet too.

#### B4. Type the configuration

Paste these commands into the shell, replacing the values between the angle
brackets. Be careful with `<` `>` - replace them, do not keep them.

```sh
# 1. Credentials and configuration
mkdir -p /home/nvram/system
(umask 077; printf '%s\n' '<rtsp-password>' > /home/nvram/system/standalone_rtsp_password)
(umask 077; printf '%s\n' '<http-password>' > /home/nvram/system/standalone_http_password)
(umask 077; cat > /home/nvram/system/standalone_auth.conf <<'EOF'
STANDALONE_RTSP_USERNAME=<rtsp-user>
STANDALONE_RTSP_PASSWORD_FILE=/home/nvram/system/standalone_rtsp_password
STANDALONE_HTTP_USERNAME=<http-user>
STANDALONE_HTTP_PASSWORD_FILE=/home/nvram/system/standalone_http_password
STANDALONE_RTSPS_PORT=322
STANDALONE_RTSP_CERT_FILE=/home/nvram/system/standalone_rtsp_cert.pem
STANDALONE_RTSP_KEY_FILE=/home/nvram/system/standalone_rtsp_key.pem
EOF
)
chmod 600 /home/nvram/system/standalone_auth.conf

# 2. Your Wi-Fi network
mkdir -p /var/run/wpa_supplicant
cat > /var/run/wpa_supplicant/wpa_supplicant.conf <<'EOF'
ctrl_interface=/var/run/wpa_supplicant
update_config=1
network={
ssid="<your-wifi-name>"
key_mgmt=WPA-PSK
psk="<your-wifi-password>"
pairwise=CCMP TKIP
group=CCMP TKIP WEP104 WEP40
scan_ssid=1
}
EOF
cp /var/run/wpa_supplicant/wpa_supplicant.conf /home/nvram/fs_config/wpa_supplicant.conf

# 3. Check what you just wrote, then restart
cat /home/nvram/system/standalone_auth.conf
sync
reboot
```

The certificate paths do not need to exist - the camera generates the
certificate on the next boot. After the reboot the camera joins your Wi-Fi and
starts serving; the hotspot is no longer needed. Turn it off, then follow
[A4](#a4-check-it-is-alive) and [A5](#a5-trust-the-certificate) above.

### Using your own certificate

Everything here is optional. Skip it and the camera generates a self-signed pair on
first boot that names itself for the address it actually has.

#### What the camera needs

| | |
| --- | --- |
| Files | `cert.pem` and `key.pem`, in the root of the provisioning card, or at `/home/nvram/system/standalone_rtsp_cert.pem` and `standalone_rtsp_key.pem` on the camera |
| Both or neither | with only one present the camera refuses to replace the pair and refuses to serve, because the missing half may be the one you were about to copy in |
| Certificate format | PEM, leaf certificate first, then any intermediates. The file is sent as the chain, so include an intermediate if your CA uses one (not the root) |
| Key format | PEM, any of PKCS#8 (`PRIVATE KEY`), PKCS#1 (`RSA PRIVATE KEY`) or SEC1 (`EC PRIVATE KEY`) |
| Key types | RSA, ECDSA P-256/P-384 and Ed25519 all work. Sign with SHA-256 or better: the camera speaks TLS 1.3 only |
| Permissions | the key must be root-owned and mode `0600` or stricter, or the camera refuses to start. The card path sets this for you; over a shell you must do it yourself |
| Names | the SAN must contain every address or name a client will dial - the client checks that, not the camera |
| Lifetime | use years, not weeks. Replacing the certificate means re-provisioning the camera, and clients that pin it need updating |

#### Signing with your own CA

If you already run a CA that your clients trust, this is the option that removes the
client-side step completely. Make the key and the request on your machine, with the
names clients will use:

```sh
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout key.pem -out c220.csr \
  -subj "/O=eufy-c220-standalone/CN=c220 camera" \
  -addext "subjectAltName=IP:192.168.1.55,DNS:camera.home.arpa"
```

Then sign it. **The `-copy_extensions copy` is not optional**: without it OpenSSL
signs the request but drops the SAN, and you get a certificate that names nothing -
it loads on the camera and is then rejected by every client. This is the single most
common way to get this wrong:

```sh
openssl x509 -req -in c220.csr -CA ca.pem -CAkey ca-key.pem -CAcreateserial \
  -out cert.pem -days 3650 -sha256 -copy_extensions copy
```

Some CA tooling does copy the SAN from the request (that is the whole point of the
request), some needs the extension given again at signing time. Whichever you use,
check the result rather than assuming - the next section is exactly that check.

#### Check before you boot the camera

Four commands, all on your own machine, that catch every mistake the camera would
otherwise punish you for:

```sh
# 1. the chain verifies against your CA
openssl verify -CAfile ca.pem cert.pem

# 2. the names are actually in there, and the dates are sane
openssl x509 -in cert.pem -noout -subject -issuer -dates -ext subjectAltName

# 3. the key belongs to the certificate (a mismatch stops the camera serving)
openssl x509 -in cert.pem -noout -pubkey | openssl md5
openssl pkey -in key.pem -pubout | openssl md5

# 4. it is a key the camera can read, not an encrypted or PKCS#12 blob
head -1 key.pem
```

Steps 3 and 4 are the ones people skip. The two hashes must be identical, and step 4
must print one of `-----BEGIN PRIVATE KEY-----`, `-----BEGIN RSA PRIVATE KEY-----` or
`-----BEGIN EC PRIVATE KEY-----`. If it says `ENCRYPTED PRIVATE KEY` the camera cannot
use it: create the key with `-nodes`, or decrypt it first.

#### What your clients need

| you used | client configuration |
| --- | --- |
| a CA-signed certificate | point the client's CA file at your CA, and use a name or address that is in the SAN. Nothing per-camera |
| a self-signed certificate of your own | add `cert.pem` to the client's trusted certificates, or accept/pin it. Same work as accepting the camera's own certificate |

The camera does not care which of these you did: it sends whatever you installed.

#### When it goes wrong

The camera is fail-closed about certificates, so a bad pair means it serves nothing
rather than falling back to plaintext. The firmware exits and the supervisor retries,
which looks like a camera that is up on Wi-Fi and answers on no port at all.

| what you see | cause | fix |
| --- | --- | --- |
| no `322`, no `8080`, log says `certificate and key were rejected` | the key does not match the certificate | supply the matching pair |
| same, but the log says `must be root-owned and mode 0600 or stricter` | key permissions, usually after copying it by hand over a shell | `chmod 600` the key file, owned by root |
| log says `refusing to replace a half-present pair` | only one of the two files exists | copy both, or delete both to let the camera generate its own |
| serves fine, but clients say it is not valid for the name | the SAN lacks the name or address the client dialled | re-issue with the right SAN, or dial something that is in it |
| serves fine, one client works and another does not | the URLs use different names (`camera.home.arpa` in one, the IP in the other) | put every name you use in the SAN |
| client says the issuer is untrusted | a self-signed certificate, or a CA that client does not have | add your CA, or the certificate itself, to that client |

You can always get back to a working camera from here: put a card in with a
corrected pair (or with no certificate at all, to let the camera generate one), or
hold the button 5 seconds and fix the files over the shell. Neither needs the camera
to be serving.

#### Replacing it later

The camera never overwrites a certificate you installed, so replacing one is
deliberate:

1. put the new `cert.pem` and `key.pem` on a provisioning card and boot with it, or
2. write both files to the NVRAM paths over the maintenance shell and reboot

Clients that trust your CA carry on unchanged. Clients that **pinned** the old
certificate - anything configured with its fingerprint rather than with a CA - need
the new one:

```sh
# the fingerprint of whatever the camera is serving now
openssl s_client -connect <camera>:322 -servername <camera> </dev/null 2>/dev/null \
  | openssl x509 -noout -fingerprint -sha256 | cut -d= -f2 | tr -d ':'
```

If you would rather hand the job back to the camera, delete both files and reboot: it
generates a new pair naming itself for the address it has at that moment. Anything
pinning the old certificate stops working at that point, by design: a pin that
survived a new certificate would not be a pin. The two cases that regenerate a
certificate on their own are a 10-second factory reset, which wipes the pair, and
deleting the files, which is the documented way to ask for a new one.

#### What the camera generates for itself

For comparison, when nothing is installed:

| | |
| --- | --- |
| Subject | `CN=c220 camera, O=eufy-c220-standalone` |
| Issuer | itself |
| Key | ECDSA P-256, generated on the camera, stored mode `0600` |
| Names | `<name>.local` (the `HOST_NAME` setting, or `camera-<serial>` derived from the board), loopback, and every IPv4 address it holds at that moment |
| Validity | 1975 to 4096, so it cannot expire because this board has no clock battery |
| Rotation | none: it is written once and left alone, so a client that pinned it keeps working |

It waits up to 30 seconds for an address before writing, because the address is what
you will type into a client. That is why it should be generated on the network the
camera will actually live on.

### Changing things later

**Move the camera to another Wi-Fi network.** Put a card in the camera with a
`wifi.txt` containing just the network:

```sh
SSID=NewNetwork
PSK=new-wifi-password
MODE=WPA
```

Boot the camera with the card in, wait 20 seconds, remove the card.

**Change the passwords.** Put a full `provision.txt` on a card again, exactly as
in [A1](#a1-write-one-file-on-the-card), with the new values. It replaces the
stored credentials. If you also leave the Wi-Fi lines out, the network is left
alone - a `provision.txt` without `SSID` only changes credentials.

**Change the port.** Add or change `RTSPS_PORT` in `provision.txt` (or edit
`/home/nvram/system/standalone_auth.conf` over the shell) and reboot.

---

## Part 2: reset and rescue

There is one button, and how long you hold it decides what happens. The
destructive action deliberately costs twice the hold of the safe one.

| hold the reset button | what happens | wipes anything? |
| --- | --- | --- |
| **5 seconds** | joins the rescue hotspot and opens the maintenance shell for this boot | **no** |
| **10 seconds** | factory reset: wipes credentials, certificate, keys, Wi-Fi and opt-in settings, then reboots | yes |

Timings are measured on real hardware: the 5-second action fires at 5 seconds and
the 10-second action fires at 10 - you will be holding the button while both
count down, so **release at 6 seconds for rescue and after 10 for a reset**. If
you hold 12 seconds you get rescue *and then* the reset.

### The repair tier: 5 seconds

Use it when the camera is not reachable but you do not want to lose anything:
a network change, a broken configuration, a forgotten password, or just wanting
a shell without pulling the camera down.

1. Hold the button about 6 seconds, then release.
2. The camera joins `RescueAP` / `12345678` and starts a root shell on port 23.
3. Log in to that hotspot and `telnet <camera-ip>`.
4. Fix what you need to - the usual command is
   `/home/script/wificonnect.sh "<ssid>" WPA "<psk>"` to move it to another
   network. Note that this vendor helper prints the password to the console.
5. `reboot`. The rescue network is not used again unless the button is pressed
   again; the shell only lives for that boot.

Both actions only fire **once per boot**, so a stuck button cannot put the camera
into a reset loop.

### The destructive tier: 10 seconds (factory reset)

1. Hold the button for at least 10 seconds, then release.
2. The camera wipes the deployment's provisioning and reboots.
3. It comes up with no credentials, which means it joins `RescueAP` /
   `12345678` and opens the shell on port 23 - the same state a new camera
   arrives in.

**What a factory reset removes**

| removed | why |
| --- | --- |
| `standalone_auth.conf`, both password files | the credentials |
| `standalone_rtsp_cert.pem`, `standalone_rtsp_key.pem` | the certificate and key |
| `standalone_rescue.conf` | your rescue network override, so defaults apply again |
| `standalone_onvif*`, `standalone_mqtt*`, `.standalone_audio_enable`, `.standalone_speaker_enable`, `.standalone_insecure_bringup`, `.maintenance_enable` | optional features you had switched on |
| `.last_known_time` | the remembered clock |
| the Wi-Fi profile (`fs_config/wpa_supplicant.conf`, `config/eufy_wifi*`, `/var/run/wpa_supplicant/wpa_supplicant.conf`) | your network, so it cannot rejoin on its own |

**What it keeps**

| kept | why |
| --- | --- |
| `.pcbsn`, `.country` | hardware identity - the board's serial and region, which cannot be regenerated |
| `set_rate_power_*.txt` | per-region radio calibration |
| `eufy.db` | the factory database |
| `/home/nvram/log/` | logs |

If you want to see the list without changing anything, run
`DRY_RUN=1 /home/script/c220-factory-reset.sh` in a shell: it prints exactly what
it would remove and keeps.

### After a reset: getting back to normal

A reset camera is a new camera, so go back to [Part 1](#part-1-set-up-a-camera).
With a card that is one step; without one it is the hotspot and shell route.

While it is unprovisioned, the camera joins the rescue hotspot and opens the
shell **on every boot**, not just once, so you can always get back in without
pressing the button again.

### This sequence was rehearsed on real hardware

A factory reset followed by one boot with a provisioning card was run on a live
camera, to check the path a new owner actually takes:

- the reset removed the credentials, the certificate and key, the Wi-Fi profile
  and the opt-in markers, and kept the radio calibration and board identity
- one boot with the card rewrote the configuration, both passwords, the
  certificate and key, and the Wi-Fi profile - every file byte-identical to what
  was there before the reset
- the camera rejoined its network and served RTSPS, the control API, video and
  both audio tracks, and the card was mounted 3 seconds into the boot

The card was then wiped of credentials again. If you rehearse this yourself, do
the same: a card left in the slot keeps re-applying whatever it carries on every
boot.

### Handing the camera to someone else

1. Hold the button 10 seconds to factory reset it. The camera now has no
   credentials, no certificate, no Wi-Fi and no trace of your network.
2. Remove the card, and keep your own `cert.pem`/`key.pem` if you had installed
   one - the new owner should not inherit your keys.
3. What stays on the camera is the hardware identity: the board serial
   (`.pcbsn`), country and radio calibration. Nothing you configured remains.
4. The new owner follows [Part 1](#part-1-set-up-a-camera) with their own
   network and their own passwords.

A reset camera will still trust anybody who can physically reach it: whoever can
insert a card or press the button can provision it. That is the same trust model
as any camera with a reset button, and it is why physical access is the boundary.

---

## Part 3: recovery playbook

| symptom | what to do |
| --- | --- |
| Camera is offline after you changed router, SSID or Wi-Fi password | Card with `wifi.txt` ([Changing things later](#changing-things-later)), boot with it in, remove it. No card? 5-second rescue and `wificonnect.sh` |
| Camera was working, now nothing answers on 322/8080 | Hold the button 5 seconds, join `RescueAP`, `telnet` in and read `/tmp/c220-recovery.log`. Check that `/home/nvram/system/standalone_auth.conf` exists, is mode `600`, and that the password files it names exist and are mode `600` |
| You forgot the RTSP or HTTP password | A card with a full `provision.txt` sets new ones (nothing else is lost). No card? Factory reset, then set it up again - or 5-second rescue to get a shell |
| Clients complain the certificate is not valid | The camera generated a new one (for example because you deleted the old files). Re-fetch `c220.pem` with [A5](#a5-trust-the-certificate) and update the client |
| Camera does not appear on your network at all | It has no valid configuration or cannot see the SSID. Use the 5-second rescue and check `/tmp/c220-wifi.log`, or boot with a `wifi.txt` card |
| The card seems to be ignored | Was it in **before** power-on? Is it FAT32? Are the file names exactly `provision.txt` / `wifi.txt`, in the card's root, with no `.txt.txt`? Read `/tmp/c220-recovery.log` - it records what it found and why it refused |
| The camera joined `RescueAP` instead of your network | Its Wi-Fi profile is missing or was wiped (for example after a factory reset), or the stored network cannot be reached. Provision it again |
| You need a shell but have no card and cannot hold the button | `POST /maintenance/enable` on the control API opens port 23 for one boot. It needs the HTTP Digest password, and closes again after a reboot. If it reports that the shell did not start listening, ask again - the firmware clears its one-shot flag for that case |
| Nothing works at all | The recovery paths above cover everything except a corrupted flash. Chip-level recovery with a CH341A programmer (partition map, offsets, read-before-write) is documented in `native/README.md`, under chip-level recovery |

### Logs

| file | what is in it |
| --- | --- |
| `/tmp/c220-recovery.log` | what the card recovery found and did on this boot, including why it refused a file |
| `/tmp/c220-button.log` | button presses, which tier fired, and whether it was a dry run |
| `/tmp/c220-wifi.log` | Wi-Fi association attempts |

`/tmp` lives in RAM, so these are cleared on every reboot. Read them in the same
boot if you are diagnosing something.

---

## Part 4 - Reference

### Card file formats

Files are read from the **root** of the card only. The card is mounted read-only
with `noexec,nodev,nosuid`, and every file is **parsed as data, never executed** -
a card cannot run scripts on the camera.

| file | purpose |
| --- | --- |
| `provision.txt` | full setup: network, credentials, and optionally the port, time server and camera name (`HOST_NAME`, which overrides the derived `camera-<serial>`) |
| `wifi.txt` | network only: `SSID=`, `PSK=`, `MODE=WPA\|WEP\|NONE`. Applied on every boot the card is present |
| `cert.pem` + `key.pem` | install your own certificate instead of the generated one. Both required |
| `maintenance.txt` | start a root shell on port 23 for that boot. Contents are ignored - the file's presence is the signal |

Validation, so you know what a rejection looks like: usernames 1-32 characters of
`A-Z a-z 0-9 . _ @ -`; passwords 1-128 characters without control characters,
quotes or backslashes; `SSID` at most 32 characters; `PSK` at most 64, and at
least 8 for `WPA`; `RTSPS_PORT` 1-65535; `NTP_HOST` at most 64 characters of
`A-Z a-z 0-9 . : -`; `HOST_NAME` a single DNS label of lowercase letters, digits and
inner hyphens, up to 63 characters, with a trailing `.local` accepted and stripped. A file that fails validation is rejected as a whole and the
reason is written to `/tmp/c220-recovery.log`.

### Where things live on the camera

| path | contents | mode |
| --- | --- | --- |
| `/home/nvram/system/standalone_auth.conf` | which usernames, which password files, ports, certificate paths, time server | `600` |
| `/home/nvram/system/standalone_rtsp_password` | video password | `600` |
| `/home/nvram/system/standalone_http_password` | control API password | `600` |
| `/home/nvram/system/standalone_rtsp_cert.pem` | certificate (yours or generated) | `644` |
| `/home/nvram/system/standalone_rtsp_key.pem` | private key | `600` |
| `/home/nvram/system/standalone_rescue.conf` | rescue network override: `SSID=`, `PSK=` | `600` |
| `/home/nvram/system/standalone_mqtt.conf` | broker host, port, credentials, topic prefix, `STANDALONE_MQTT_DISCOVERY=0\|1` | `600` |
| `/home/nvram/fs_config/wpa_supplicant.conf` | your Wi-Fi profile | - |

`/home/nvram` is the only writable storage that survives a reboot - `/` and
`/home` are read-only SquashFS images and `/tmp` is RAM - so that is where all
configuration lives. Password and key files must be root-owned and mode `600` or
the firmware refuses to start: that is deliberate, and the refusal is logged.

Home Assistant discovery is **opt-in**: the camera publishes its entities to
`homeassistant/<component>/<prefix>/<object>/config` only when
`STANDALONE_MQTT_DISCOVERY=1` is set. Without that line the broker still gets
telemetry and commands under the camera's own topic prefix, but Home Assistant
has no entities to drive - so a dashboard control that does nothing is worth
checking for this first.

### The control API

All routes are on `https://<camera-ip>:8080` and need HTTP Digest authentication,
except `/healthz`.

| route | what it does |
| --- | --- |
| `GET /healthz` | liveness, no password |
| `GET /ptz/position` | current pan and tilt |
| `POST /ptz/move` | `{"direction":"left\|right\|up\|down","degrees":1..355,"speed":0..10}` |
| `POST /ptz/stop` | stop moving |
| `POST /ptz/center` | re-home and park: both axes are driven onto both of their travel ends first (left, right, up, down), then to the centre. Takes about thirty seconds, so it answers `202` immediately and reports progress as `"centering":true` on `GET /ptz/position`; a second call while it runs answers `409` |

Pan runs over 0..355 degrees and tilt over 0..75, with the park position at pan 178 /
tilt 38. A command that would move *past* one of those ends is answered
`{"status":"ok"}` and does nothing - the position is simply already at its limit.
This is worth knowing because a fresh boot reports pan 0, which is the left end, so
the first `left` command of a boot looks like it was ignored. Read
`GET /ptz/position` when the answer matters, or move the other way first.

Centring is not a plain move to 178/38: the driver counts its own steps, so a bare
move lands wherever its arithmetic believes the centre is, which drifts once the head
has been nudged by hand or has lost steps. `POST /ptz/center` therefore sweeps to each
end - a mechanical reference - and parks from there, which is what makes the result
repeatable. Measured: 179/39 against the 178/38 target.
| `POST /night_mode` | `{"mode":0\|1\|2}` - `0` automatic, `1` force night (IR on), `2` force day (IR off) |
| `POST /talkback/status`, `/talkback/start`, `/talkback/stop`, `/talkback/clear`, `/talkback/pcm`, `/talkback/audio`, `/talkback/volume` | two-way audio |
| `POST /maintenance/enable` | opens the root shell on port 23 for the current boot |
| `POST /reboot` | restarts the camera; authenticated like every other route |

### Last resort: plaintext bring-up mode

If you are stuck and need to see whether the hardware works at all, creating the
file `/home/nvram/system/.standalone_insecure_bringup` and rebooting makes the
firmware run **one boot** with plaintext RTSP on 554, an unencrypted control API
and no authentication. The marker is consumed by that boot, so the next boot is
back to normal.

Do not do this on a network you do not fully trust: for that one boot, anyone who
can reach the camera sees the video without a password. It exists for debugging on
a bench, not for daily use.

### Rescue network

Defaults are `RescueAP` / `12345678`, the same values the vendor firmware used. To
use your own, write `/home/nvram/system/standalone_rescue.conf` (mode `600`):

```sh
SSID=MyPhoneHotspot
PSK=my-hotspot-password
```

Note that a factory reset deletes this file, so a reset camera is back on the
defaults - worth remembering if you changed them.

### Boot sequence, in one table

| time | what happens |
| --- | --- |
| 0-3 s | kernel, then the startup script |
| ~3 s | card recovery looks for a card: up to 2 seconds for the controller to publish it, then up to 4 seconds of mount retries |
| ~3-5 s | if there is no usable configuration, the camera joins the rescue network and opens the shell |
| ~5 s | Wi-Fi bootstrap associates and gets an address |
| ~6 s | firmware starts, generates a certificate if none exists (waiting up to 30 seconds for an address to name it with) |
| ~17 s | RTSPS on 322 and HTTPS on 8080 are serving |
| ~22 s | video is flowing |
| any time | clock syncs over SNTP, if a time server answers |
