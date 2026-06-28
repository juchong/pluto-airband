# Pluto airband firmware — image contents & invariants

What the flashable image contains and the addressing invariants the airband DDR
ring depends on. For the actual procedures see:

- **Build + flash + first-boot + troubleshooting:** [`../BUILD.md`](../BUILD.md)
- **Quick start, channel plan, host reader:** [`../README.md`](../README.md)

## What's in the image

- **FPGA bitstream**: Maia SDR base (spectrometer + recorder + DDC) **plus** the
  18-channel airband receiver (`maia_hdl` `ReceiverTop`) and a **cyclic**
  `DmaStreamWrite` that streams 64-bit framed audio records into a 16 MiB DDR
  hardware ring. Timing-closed at 65.278 MHz.
- **Devicetree**: a `maia_sdr_airband` reserved-memory region
  (`0x19000000`, 16 MiB) + a `maia-sdr,rxbuffer` node → `/dev/maia-sdr-airband`.
  (Relocated from the original `0x1f000000`, which collided with the kernel CMA
  pool and bricked the device — see [`../BUILD.md`](../BUILD.md).)
- **maia-httpd**: configures the AD9361 front-end + per-channel NCOs, starts the
  cyclic DMA, drains the ring and serves the **raw framed-audio records over TCP**
  (default `0.0.0.0:30000`). Auto-starts on boot with `--airband`. While
  `--airband` is set the AD9361 front-end is **locked read-only** — the
  `/api/ad9361` HTTP endpoint is a no-op and the Maia web UI disables the RF
  controls — so the web UI can't retune the radio off the airband band (126.4
  MHz / 16 Msps). See [`../BUILD.md`](../BUILD.md) ("the AD9361 front-end is locked read-only").

Frame layout (little-endian 64-bit word, see `hdl/audio_framer.py`):

```
bits [23:0]  audio sample (signed, 24-bit two's complement)
bits [31:24] carrier level (8-bit minifloat of the AM carrier; 0 = none/old bitstream)
bits [39:32] channel index (0..17)
bits [63:40] per-channel sequence counter (wraps at 2**24; gap = dropped samples)
```

## Build & flash (summary)

The single build script `build_firmware_full.sh` (run on the x86-64 build server)
pulls both repos, builds **only** from committed git state, bakes the fork commit
hash into the bitstream (USERID + USR_ACCESS), and produces **both** partitions:
`boot.{frm,dfu}` (mtd0: FSBL + bitstream + u-boot) and `$TARGET.{frm,dfu}` (mtd3:
kernel + DT + rootfs). `build_bitstream.sh` is a fast, non-flashable host-Vivado
synthesis/timing check.

### Hardware variants (`TARGET`)

The deployed target is the **Pluto+** (`TARGET=plutoplus`, the open
`plutoplus/plutoplus` board: `xc7z010clg400-1`, Gigabit Ethernet, microSD, 0.5 ppm
VCTCXO). The build *default* is `TARGET=pluto` (the USB-only ADALM-Pluto,
`xc7z010clg225-1`), so pass `TARGET=plutoplus` explicitly. The airband design is
unchanged across variants —
the Pluto+ project shares the pluto `system_top.v` and sources its
`system_bd.tcl` (so the airband HP0 DMA wiring carries over), and its
`zynq-plutoplus-maiasdr.dts` `#include`s the shared `zynq-pluto-sdr-maiasdr.dtsi`
overlay (so the airband reserved-memory + rxbuffer nodes carry over). The FIT
image is named after the target: `plutoplus.{frm,dfu}` instead of
`pluto.{frm,dfu}`. See `../BUILD.md` ("Pluto+ variant") for build/flash/jumper/
Ethernet details.

> **Why two partitions matter:** the **bitstream and FSBL live in `BOOT.BIN`
> (`boot.frm`, `mtd0`)**. Flashing only `pluto.dfu` after an HDL change can leave
> the old bitstream / HP0-disabled FSBL running → airband register aliasing + DMA
> hang + watchdog reset. After an HDL change, flash **both** and verify the
> bitstream's embedded UserID equals your committed fork HEAD (the build prints it).

Full commands, prereqs, DFU/MTD details, and the u-boot env re-apply step are in
[`../BUILD.md`](../BUILD.md).

## Addressing invariants

The three "sources of truth" for the airband DDR ring must agree:

| Source | Value |
|---|---|
| FPGA `MaiaSDRConfig.airband_address_range` | `(0x19000000, 0x1a000000)` |
| Devicetree `maia_sdr_airband` `reg` | `<0x19000000 0x01000000>` |
| kmod `rxbuffer` `buffer-size` | `0x10000` (→ 256 ring slots) |

`apply_airband_devicetree.py` sets the DT side; `firmware/airband.json` and the
HDL constants set the rest.
