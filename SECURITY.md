# Security

## Trust boundaries

- Factory firmware executes card `iq.sh` as root. Secure standalone packaging
  removes executable SD/NVRAM hooks; restoring factory `/home` restores vendor
  behaviour. Trusted recovery parses data on a `ro,nosuid,nodev,noexec` card.
- Physical access is a credential: a card or reset button can reprovision the
  camera and open root Telnet. The authenticated maintenance API can also open
  port 23 for one boot. Unprovisioned/rescue states expose that shell too. Keep
  setup isolated, remove cards and reboot out of maintenance before normal use.
- RTSPS/HTTPS require TLS and credentials; only `/healthz` is public. A one-boot
  `.standalone_insecure_bringup` marker exists for isolated bench debugging,
  never production. There is no firmware-upload endpoint.
- Certificate discovery is not trust. mDNS publishes names, never fingerprints.
  Establish trust out of band/isolated setup; reject unexplained pin changes.
- Reset removes known deployment files, not arbitrary private files or vendor
  logs. See [handover](docs/setup-and-recovery.md#recovery-updates-and-handover).

## Authentication and resource limits

Digest retains zeroizing password-equivalent HA1 hashes. SHA-256 control auth
and RTSP MD5 compatibility run over TLS. `qop=auth` nonce counts must increase;
the bounded 256-entry replay cache loses history on eviction. Legacy no-qop
clients have no nonce-count replay protection. Five failures from one address
cause a 30-minute in-memory lockout. RTSP session IDs are unpredictable;
missing/mismatched IDs fail session-scoped methods.

Minimal ONVIF uses TLS and WS-Security PasswordDigest, retaining a zeroizing copy
of the HTTP password. PasswordText is refused; timestamp, nonce and replay limits
apply. Client TLS verification remains the client's responsibility.

Password/key files must be regular, owned by root/effective service UID and mode
`0600` or stricter. MQTT requires verified TLS and an absolute readable CA path;
configured username/password must be paired. There is no plaintext fallback.
The MQTT library retains its own credential copy while operating.

One RTSP/RTSPS slot includes pre-auth/TLS work; eight control connections are
admitted independently. TLS handshakes have a five-second deadline, control
read/write and RTSP writes fifteen seconds, RTSP pre-PLAY thirty seconds.
Allocation/queue limits and cleanup are in [media engine](docs/media-engine.md#runtime-policies).
These are bounded-service policies, not a throughput guarantee.

## Reporting

Report suspected vulnerabilities privately through repository vulnerability
reporting if enabled, or the maintainer address in commit history. Include source
or firmware hash, board revision, reproduction and redacted logs. Never attach
credentials, private keys, dumps or private recordings publicly. There is no bug
bounty or guaranteed response time.

Factory bootloader/kernel/drivers and vendor cloud/app are outside this
project's control. Physical access is trusted by design, but credentialed API
behaviour that exceeds its intended permissions can still be a valid bug.
