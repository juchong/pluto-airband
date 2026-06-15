# Pluto airband firmware + host reader

End-to-end instructions to flash the airband multichannel receiver onto the
ADALM-Pluto and pull per-channel audio on the host.

## What's in the image

- **FPGA bitstream**: Maia SDR base (spectrometer + recorder + DDC) **plus** the
  21-channel airband receiver (`maia_hdl` `ReceiverTop`) and a **cyclic**
  `DmaStreamWrite` that streams 64-bit framed audio records into the top 16 MiB
  of DDR as a hardware ring. Timing-closed at 62.5 MHz (WNS +0.305 ns).
- **Devicetree**: a `maia_sdr_airband` reserved-memory region
  (`0x1f000000`, 16 MiB) + a `maia-sdr,rxbuffer` node → `/dev/maia-sdr-airband`.
- **maia-httpd**: configures the AD9361 front-end + per-channel NCOs, starts the
  cyclic DMA, drains the ring and serves the **raw framed-audio records over TCP**
  (default `0.0.0.0:30000`).

Frame layout (little-endian 64-bit word, see `hdl/audio_framer.py`):

```
bits [31:0]  audio sample (signed, sign-extended to 32 bits; 24-bit content)
bits [39:32] channel index (0..20)
bits [63:40] per-channel sequence counter (wraps at 2**24; gap = dropped samples)
```

## 1. Build the firmware (on the x86-64 build server)

Prereqs (already provisioned on `xilinx-builder`, see `DEV-SETUP.md`):
`plutosdr-fw` clone, the `maia-sdr-devel` Docker image, the `vivado2023_2`
volume, and a prebuilt cyclic-DMA `system_top.xsa`
(`make -C maia-sdr/maia-hdl/projects/pluto`).

```bash
# this repo checked out at ~/pluto-build/airband, maia-sdr fork at its maia-sdr/
bash firmware/build_firmware.sh
# -> ~/pluto-build/plutosdr-fw/build/pluto.frm  and  pluto.dfu
```

The script splices the `pluto-airband` maia-sdr fork into `plutosdr-fw`, patches
the devicetree, and builds with `HAVE_VIVADO=0` using the prebuilt bitstream (so
the container needs neither Vivado nor numpy/scipy).

## 2. Flash the Pluto

```bash
# DFU (recommended), Pluto in DFU mode:
dfu-util -a firmware.dfu -D pluto.dfu
dfu-util -a firmware.dfu -R -D pluto.dfu     # -R reboots

# or mass-storage: copy pluto.frm onto the PlutoSDR USB drive, then eject
```

The bootloader (`boot.frm`) is **not** changed by this image, so a standard
`v0.8.x` Maia/Pluto bootloader is required (already present on a Maia-flashed
Pluto).

## 3. (optional) Channel plan

`maia-httpd` ships a built-in default plan (21 channels around 123.438 MHz,
Fs 14 MHz → 15625 sps audio). To override, copy `firmware/airband.json` to the
Pluto and reboot (or restart `maia-httpd`):

```bash
scp firmware/airband.json root@192.168.2.1:/root/airband.json
```

`samp_rate` MUST stay at the value the channelizer was built for (14 MHz) and
every `channels_hz` entry must be within `center_hz ± samp_rate/2`.

## 4. Read audio on the host

```bash
cargo build --release --manifest-path host/airband-reader/Cargo.toml

# live link check (per-channel sample rate, dropped-sample count, peak level):
host/airband-reader/target/release/airband-reader 192.168.2.1:30000

# record per-channel WAV (16-bit, 15625 sps):
airband-reader 192.168.2.1:30000 --mode wav --out-dir caps

# raw s16le per channel (chNN.s16), e.g. to pipe into an encoder:
airband-reader 192.168.2.1:30000 --mode raw --out-dir pcm --shift 6
```

Tune `--shift` on a live signal (it right-shifts the 24-bit sample to 16-bit:
too small clips, too large is quiet). The reader reconnects automatically and
flags any FPGA/transport drops via the per-channel sequence counter.

## Notes / addressing invariants

The three "sources of truth" for the airband DDR ring must agree:

| Source | Value |
|---|---|
| FPGA `MaiaSDRConfig.airband_address_range` | `(0x1f000000, 0x20000000)` |
| Devicetree `maia_sdr_airband` `reg` | `<0x1f000000 0x01000000>` |
| kmod `rxbuffer` `buffer-size` | `0x10000` (→ 256 ring slots) |

`apply_airband_devicetree.py` sets the DT side; `firmware/airband.json` and the
HDL constants set the rest.
