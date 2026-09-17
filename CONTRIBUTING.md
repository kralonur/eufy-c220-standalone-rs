# Contributing

The most useful contributions here come from people who own the camera, because the
interesting claims are hardware claims and no machine can check them for you.

## What can be checked without a camera

`nix develop` (or the manual prerequisites in [README.md](README.md)), then:

```sh
./native/check.sh
```

or `nix run .#check` from anywhere in the repository, which carries the same pinned
toolchain without entering a shell first. `nix run .` lists the other tasks.

That runs formatting, clippy with warnings denied, the tests, the ARM release build
and the size budget. The tests cover the daemon protocol, framing, Digest
authentication, the lockout and the control API. They cover nothing that touches the
camera: not the media path, not Wi-Fi, not the button, not flashing.

## Size is part of correctness

The firmware is written into a 4,915,200-byte `/home` partition, so footprint is not
a curiosity. `native/check.sh` fails a build over the budget and prints how it moved:

```sh
size -A target/armv7-unknown-linux-musleabihf/release/c220-firmware
cargo bloat --release --target armv7-unknown-linux-musleabihf -p c220-firmware --crates
```

Build in one environment. Cargo fingerprints the compiler by its version string, and
the pinned rustc and a distribution `1.98.1` report the same one, so alternating
between them in a single `target/` mixes objects from both and the size stops meaning
anything. `native/check.sh` records the toolchain that built there and warns when the
next run uses a different one.

Measure before enabling a feature or adding a dependency. `tracing-subscriber`'s
`env-filter` pulled in a regex engine for 253 KB, 11% of the binary, to parse
`RUST_LOG` - and nothing in the manifest said so.

## Commits

Conventional-commit subjects (`fix(firmware): ...`, `docs(native): ...`), imperative
first line, and a body that says *why*: what was measured, what was rejected, and
what is still unverified. The history is this project's design record, so a change
that needs hardware to be meaningful should say what was and was not run on one.

## What not to send

- **Vendor files.** `native/camera-rootfs/`, the sensor calibration blob, the FDK AAC
  decoder and anything else proprietary are gitignored on purpose. They must not
  appear in a commit, a pull request, or a test fixture.
- **Credentials.** Wi-Fi profiles, `standalone_auth.conf`, certificates, keys, or
  anything out of `/home/nvram`.
- **Dumps.** Full-chip and partition dumps carry a camera's identity and flash
  contents; the ones under `dist/` are gitignored for that reason.

## Reporting something broken

A report about camera behaviour is only actionable with the camera's own state
attached. Worth including:

- the camera revision and the sensor blob it carries (`ls /home/os03b10_*.bin`);
- `md5sum /home/c220-firmware` from the camera, and whether it matches the build you
  produced;
- `native/verify-c220-deployment.sh` output, which is the gate a deployment is meant
  to pass;
- `/tmp/c220-recovery.log` for anything about a card or provisioning, and
  `iq_boot.log` for anything about getting in at all;
- what you expected, what happened, and whether it survives a reboot.

## Licence

Contributions are accepted under the repository's dual licence, MIT OR Apache-2.0, at
the user's option.
