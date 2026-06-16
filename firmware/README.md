# Pluto airband firmware + host reader

End-to-end instructions to flash the airband multichannel receiver onto the
ADALM-Pluto and pull per-channel audio on the host.

## What's in the image

- **FPGA bitstream**: Maia SDR base (spectrometer + recorder + DDC) **plus** the
  21-channel airband receiver (`maia_hdl` `ReceiverTop`) and a **cyclic**
  `DmaStreamWrite` that streams 64-bit framed audio records into a 16 MiB DDR
  hardware ring. Timing-closed at 62.5 MHz.
- **Devicetree**: a `maia_sdr_airband` reserved-memory region
  (`0x19000000`, 16 MiB) + a `maia-sdr,rxbuffer` node → `/dev/maia-sdr-airband`.
  (Relocated from the original `0x1f000000`, which collided with the kernel CMA
  pool and bricked the device — see `DEV-SETUP.md`.)
- **maia-httpd**: configures the AD9361 front-end + per-channel NCOs, starts the
  cyclic DMA, drains the ring and serves the **raw framed-audio records over TCP**
  (default `0.0.0.0:30000`). Auto-starts on boot with `--airband`. While
  `--airband` is set the AD9361 front-end is **locked read-only** — the
  `/api/ad9361` HTTP endpoint is a no-op and the Maia web UI disables the RF
  controls — so the web UI can't retune the radio off the airband band (123.438
  MHz / 14 Msps). See `DEV-SETUP.md` ("the AD9361 front-end is locked read-only").

Frame layout (little-endian 64-bit word, see `hdl/audio_framer.py`):

```
bits [31:0]  audio sample (signed, sign-extended to 32 bits; 24-bit content)
bits [39:32] channel index (0..20)
bits [63:40] per-channel sequence counter (wraps at 2**24; gap = dropped samples)
```

## 1. Build the firmware (on the x86-64 build server)

Prereqs (already provisioned on `xilinx-builder`, see `DEV-SETUP.md`):
`plutosdr-fw` clone, the `maia-sdr-devel` Docker image, the `vivado2023_2`
volume, and this repo at `~/pluto-build/airband` (maia-sdr fork at its
`maia-sdr/`).

One build script produces flashable images:

```bash
# Push your commits to origin first. This pulls both repos, builds ONLY from
# committed git state, bakes the fork commit hash into the bitstream (USERID +
# USR_ACCESS), and produces BOTH partitions: boot.{frm,dfu} (mtd0) and
# pluto.{frm,dfu} (mtd3). Use for ANY change. ~20 min (Vivado is the long pole).
bash firmware/build_firmware_full.sh
# -> ~/pluto-build/plutosdr-fw/build/{boot,pluto}.{frm,dfu}
```

It clones the `pluto-airband` maia-sdr fork into `plutosdr-fw` at the committed
HEAD, patches the devicetree (airband reserved-memory at `0x19000000`), and
patches `S60maia-httpd` so the receiver auto-starts with `--airband`. For a fast
synthesis/timing check without producing firmware, use
`firmware/build_bitstream.sh` (host Vivado, not flashable).

> **Why two partitions matter:** the **bitstream and FSBL live in `BOOT.BIN`
> (`boot.frm`, `mtd0`)**, with a copy in `pluto.frm`. Flashing only `pluto.dfu`
> after an HDL change can leave the old bitstream / HP0-disabled FSBL running →
> airband register aliasing + DMA hang + watchdog reset. After an HDL change,
> flash **both** and verify the bitstream's embedded UserID equals your committed
> fork HEAD (the build prints this). See `DEV-SETUP.md`.

## 2. Flash the Pluto

Put the Pluto in DFU mode (power on holding the button until the LED blinks
slowly), then flash. After an HDL change flash **both** partitions, `boot` first:

```bash
dfu-util -a boot.dfu     -D boot.dfu     # mtd0: FSBL + bitstream + u-boot
dfu-util -a firmware.dfu -D pluto.dfu    # mtd3: kernel + DT + rootfs
dfu-util -e                               # leave DFU / reboot
```

For a FIT-only change (no HDL change) flash just `pluto.dfu`. Mass-storage
alternative: copy the `.frm` file(s) onto the PlutoSDR USB drive and eject.

## 3. (optional) Channel plan

`maia-httpd` ships a built-in default plan (21 channels around 123.438 MHz,
Fs 14 MHz → 15625 sps audio). To override, copy `firmware/airband.json` to the
Pluto and reboot (or restart `maia-httpd`):

```bash
scp firmware/airband.json root@192.168.2.1:/root/airband.json
```

`samp_rate` MUST stay at the value the channelizer was built for (14 MHz) and
every `channels_hz` entry must be within `center_hz ± samp_rate/2`. The default
uses fixed `agc: "manual"` at `gain_db: 71.0` — airband signals are weak and the
AD9361 AGC modes settle to ~55 dB and starve weak channels. At 71 dB the wideband
ADC can clip (~15%) at strong-signal sites; lower `gain_db` if you hear
distortion (it trades weak-signal sensitivity and does not remove the RF-spur
"buzz" — see `diagnostics/README.md`).

## 4. Read audio on the host

```bash
cargo build --release --manifest-path host/airband-reader/Cargo.toml

# live link check (per-channel sample rate, dropped-sample count, peak level):
host/airband-reader/target/release/airband-reader 192.168.2.1:30000

# record per-channel WAV (16-bit, 15625 sps):
airband-reader 192.168.2.1:30000 --mode wav --out-dir caps

# raw s16le per channel (chNN.s16), e.g. to pipe into an encoder:
airband-reader 192.168.2.1:30000 --mode raw --out-dir pcm
```

`--shift` scales the 24-bit sample into 16-bit: **positive = attenuate
(right-shift), negative = makeup gain (left-shift)**. Airband AM audio is quiet,
so the default is `-6` (≈ +36 dB). More negative = louder; positive if loud
signals clip. The reader reconnects automatically and flags any FPGA/transport
drops via the per-channel sequence counter.

## Notes / addressing invariants

The three "sources of truth" for the airband DDR ring must agree:

| Source | Value |
|---|---|
| FPGA `MaiaSDRConfig.airband_address_range` | `(0x19000000, 0x1a000000)` |
| Devicetree `maia_sdr_airband` `reg` | `<0x19000000 0x01000000>` |
| kmod `rxbuffer` `buffer-size` | `0x10000` (→ 256 ring slots) |

`apply_airband_devicetree.py` sets the DT side; `firmware/airband.json` and the
HDL constants set the rest.
