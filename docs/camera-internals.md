# Hardware and flash reference

For normal installation, configuration and recovery use the
[owner guide](setup-and-recovery.md). Values below describe the **reference
C220/T8W11 board**, not other Eufy cameras.

## Board

![Mainboard](images/c220-mainboard.jpg)
![Flash chip](images/c220-flash-gd25q128e.jpg)

| Item | Reference value |
| --- | --- |
| SoC / flash | Rockchip RV1103G1 / 16 MiB GD25Q128E |
| Sensor | `os03b10_CMK-OT2117-PC1_30IRC-F16.bin` |
| Capture / PTZ | `/dev/video12` / `/dev/motor` |
| Amplifier enable | GPIO 102 |
| Audio devices | `hw:0,0`, `hw:1,0` |
| Button | SARADC IIO ladder: idle 1020–1023, held 557–561, threshold 800 |
| PTZ travel / park | pan 0–355°, tilt 0–75° / approximately 178°, 38° |
| ISP inputs | `cac_map_hw_2560x1440.bin`, `iqfiles/`, matching sensor blob |

GPIO/device nodes, tuning and vendor library versions must be re-measured for
another board. The shared-looking vendor boot hook does not establish support.

## Flash layout

| mtd | Name | Size | Chip offset |
| --- | --- | --- | --- |
| 0 | env | 0x10000 | 0x000000 |
| 1 | idblock | 0x20000 | 0x010000 |
| 2 | uboot | 0x20000 | 0x030000 |
| 3 | boot | 0x200000 | 0x050000 |
| 4 | rootfs | 0x400000 | 0x250000 |
| 5 | recovery | 0x380000 | 0x650000 |
| 6 | home | 0x4b0000 | 0x9d0000 |
| 7 | nvram | 0x160000 | 0xe80000 |
| 8 | misc | 0x10000 | 0xfe0000 |
| 9 | DID | 0x10000 | 0xff0000 |

Normal installation writes only `home`; runtime settings live in writable
`/home/nvram`. `/` and `/home` are SquashFS, `/tmp` is RAM.

The factory card mount is `/mnt/sdcard`; trusted standalone recovery uses
`/tmp/sd`. On the reference board, `flashcp` is a nonworking stub, small block
writes can leave an incomplete erase block, and the by-name block device drops
the final block of a write. Rewriting the same image does not repair that.
Use the [aligned extra-block write and character-device verification](setup-and-recovery.md#4-write-and-verify),
not a generic flash tutorial. Direct character-device writes without erasing
produce old/new bitwise-AND data on NOR flash.

## No-boot recovery

An unreadable `/home` cannot run card/button/network recovery. Programmer recovery
is the fallback. UART is expected on `ttyFIQ0`, **1500000 baud, 8N1**, but has not
been verified with an adapter; do not depend on it as a proven recovery route.

Disconnect camera USB power before attaching a programmer. Never power from both
sources. Verify chip voltage, actual programmer output/logic levels and pin-1
orientation; some CH341A boards expose unsafe levels. UART needs appropriate TTL
levels, not RS-232; do not connect its VCC. Stop if uncertain.

Pin the known chip name; read twice before any write:

```sh
CHIP='GD25Q128E/GD25B128E/GD25R128E/GD25Q127C'
flashrom -p ch341a_spi -c "$CHIP" -r full-before.bin
flashrom -p ch341a_spi -c "$CHIP" -r full-before-second.bin
cmp full-before.bin full-before-second.bin
stat -c %s full-before.bin             # 16777216
sha256sum full-before.bin
```

Do not proceed on errors or mismatch. Keep copies off-device. For a same-board
full-image restore:

```sh
flashrom -p ch341a_spi -c "$CHIP" -w full-before.bin
flashrom -p ch341a_spi -c "$CHIP" -r verify.bin
cmp full-before.bin verify.bin
```

This overwrites **all partitions**, including identity and NVRAM: use only a
verified dump from this exact board, and preserve a current backup first. Never
restore another owner's boot environment, identity or calibration.

## Factory rollback

Use your own raw original `home` image, not the extracted `base-home/` tree.
A full-partition factory image cannot fit the extra sacrificial block. The
network full-partition rollback write is **not verified**; if its last block
fails readback, do not reboot—use the programmer fallback.

To replace the home region in a same-board full-chip work image, first verify
`home-before.bin` is exactly **4,915,200 bytes**:

```sh
cp full-before.bin work.bin
dd if=home-before.bin of=work.bin bs=65536 seek=157 count=75 conv=notrunc
```

Then use the whole-image programmer write/readback above with `work.bin`.
This is a whole-chip restore, not an isolated home write; all other regions
come from `full-before.bin`. Do not use a stale dump unless that is intended.

NVRAM is separate. Standalone credentials/configuration remain there after a
home-only rollback, although factory code does not use them. Remove your own
leftovers privately without erasing vendor identity, radio calibration or the
factory Wi-Fi settings. A reset is not secure erasure.
