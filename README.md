# Pluto Airband — multichannel VHF airband receiver on the ADALM-Pluto

Turn an [Analog Devices ADALM-Pluto](https://www.analog.com/en/resources/evaluation-hardware-and-software/evaluation-boards-kits/adalm-pluto.html)
into a **21-channel VHF airband (AM aircraft voice) receiver**. A single wideband
capture is split into many narrow channels entirely inside the Pluto's FPGA; each
channel is AM-demodulated to audio on-chip and streamed off the device over the
network — suitable for feeding multiple [LiveATC](https://www.liveatc.net/) audio
streams from one SDR.

> **Status:** live on hardware. All 21 channels stream gap-free; the receiver
> auto-starts on boot. See `PROGRESS.md` for the full history.

## How it works

```
            ADALM-Pluto (Zynq-7010: FPGA + ARM)                         Host / Raspberry Pi
 ┌──────────────────────────────────────────────────────┐        ┌────────────────────────────┐
 │  AD9361 RF  ──IQ──▶  FPGA (PL)                         │        │  airband-reader (Rust)     │
 │  LO 123.438 MHz      ┌───────────────────────────────┐│  TCP   │  • demux by channel        │
 │  Fs  14 MHz          │ ReceiverTop:                   ││ :30000 │  • drop detection (seq)    │
 │                      │  channelizer (21 ch, TDM DDC)  ││──────▶ │  • 24→16-bit scale         │
 │                      │  → AM demod (|I+jQ|, DC block) ││ framed │  • WAV / raw / live stats  │
 │                      │  → audio decimate → framer     ││ 64-bit └────────────────────────────┘
 │                      │  → cyclic DMA → DDR ring       ││ records
 │                      └───────────────────────────────┘│         each record (LE u64):
 │  maia-httpd (ARM): configures AD9361 + NCOs, starts   │           [31:0]  audio sample (s24)
 │  the DMA, drains the DDR ring, serves it over TCP     │           [39:32] channel index 0..20
 └──────────────────────────────────────────────────────┘           [63:40] per-channel seq
```

- One AD9361 capture (LO **123.438 MHz**, Fs **14 MHz**) covers the 118.05–128.5 MHz
  band. The FPGA channelizer tunes a numerically-controlled oscillator per channel,
  filters/decimates to a narrow channel, and AM-demodulates to **15 625 sps** audio
  (`Fs / 128 / 7`).
- Audio for all channels is packed into 8-byte records written to a DDR ring by a
  cyclic DMA, drained by `maia-httpd`, and served as a raw TCP byte stream.
- The host reader demuxes the records back into per-channel audio and detects any
  dropped samples via the per-channel sequence counter.

This is built **on top of** [Maia SDR](https://maia-sdr.org/); the airband DSP,
DMA, and control live in our fork (`github.com/juchong/maia-sdr`, `pluto-airband`
branch). The Maia base (spectrometer/recorder/DDC) is preserved.

## Quick start

### 1. Flash a Pluto

Prebuilt images are produced by the build server (see `firmware/README.md` and
`DEV-SETUP.md`). Put the Pluto in DFU mode (power on while holding the button
until the LED blinks slowly), then flash **both** partitions:

```bash
cd firmware/build
dfu-util -a boot.dfu     -D boot.dfu     # FPGA bitstream + FSBL + bootloader (mtd0)
dfu-util -a firmware.dfu -D pluto.dfu    # kernel + devicetree + rootfs       (mtd3)
dfu-util -e                               # reboot
```

> If you only changed the channel plan / software (not the FPGA), reflash just
> `pluto.dfu`. After any FPGA change you **must** reflash `boot.dfu` too — the
> bitstream lives there, and a mismatch hangs the receiver. See `firmware/README.md`.

The receiver starts automatically on boot. By default the Pluto is reachable at
`192.168.2.1` over USB.

### 2. Listen

Build and run the host reader (Rust):

```bash
cargo build --release --manifest-path host/airband-reader/Cargo.toml
BIN=host/airband-reader/target/release/airband-reader

# live link health: per-channel sample rate, dropped samples, peak level
$BIN 192.168.2.1:30000

# record one WAV per channel (16-bit, 15625 sps)
$BIN 192.168.2.1:30000 --mode wav --out-dir caps

# raw s16le per channel (chNN.s16) to pipe into an encoder/feeder
$BIN 192.168.2.1:30000 --mode raw --out-dir pcm
```

`--shift` scales the 24-bit demod sample into 16-bit before output: **positive
right-shifts (attenuate), negative left-shifts (makeup gain)**. Airband AM audio
is quiet (often only tens of LSB at 24-bit), so the default is **`-6`** (≈ +36 dB
makeup). Make it more negative if voice is too quiet, less negative (toward `0`,
then positive) if loud signals clip.

### Audition channels live (testing)

To listen to a channel on your speakers and flip between frequencies in real time:

```bash
cargo build --release --manifest-path host/airband-listen/Cargo.toml
host/airband-listen/target/release/airband-listen 192.168.2.1:30000
```

Interactive keys: `↑/↓` (or `j`/`k`, `[`/`]`) step channels, type a number then
`Enter` to jump, `+`/`-` adjust gain (airband audio is quiet — start by raising it),
`m` mutes, `q` quits. The display shows a live level meter and cumulative dropped
samples per channel, so it doubles as a quick link-health check.

### Web UI (Maia spectrometer) — front-end is read-only

The Pluto still serves the Maia SDR web UI at `http://192.168.2.1:8000`, and its
waterfall is handy for seeing live activity across the **118–128 MHz** airband
window. Because the airband receiver owns the single AD9361 front-end (its
channelizer is built for **123.438 MHz / 14 Msps**), those controls — RX freq,
sampling freq, RF bandwidth, gain, AGC — are **locked read-only** while the
receiver is running. This is deliberate: the web UI used to silently retune the
radio to its 2.4 GHz / 61.44 Msps defaults on page load, which moved the
front-end off-band and made every channel demodulate noise. The lock is enforced
server-side (`/api/ad9361` is a no-op under `--airband`), so the radio stays on
the airband band no matter what the browser does.

## Customizing the channel plan

The receiver reads `/root/airband.json` on the Pluto at startup; if absent it uses
the same built-in defaults. A template is at `firmware/airband.json`:

```jsonc
{
  "center_hz":   123438000,   // AD9361 RX LO (capture center) — keep within the built window
  "samp_rate":   14000000,    // MUST stay 14 MHz (the rate the channelizer was built for)
  "rf_bandwidth":14000000,
  "gain_db":     71.0,        // used when agc = "manual"; near-max for weak airband
  "agc":         "manual",    // "manual" | "slow_attack" | "fast_attack" | "hybrid"
  "poll_ms":     20,
  "channels_hz": [ 118050000, 119200000, /* … up to 21 … */ 128500000 ]
}
```

Rules:
- **`samp_rate` must remain `14000000`.** The channelizer's filters/decimation are
  baked into the FPGA bitstream for this rate; changing it requires an HDL rebuild.
- Up to **21** entries in `channels_hz`, each within `center_hz ± samp_rate/2`
  (i.e. 116.438–130.438 MHz). Out-of-window channels are rejected at startup.
- Changing `center_hz` re-tunes the whole window; keep all desired channels inside it.
- **Gain:** airband signals are weak and intermittent. The AD9361 AGC modes
  (`slow_attack`/`fast_attack`/`hybrid`) settle to ~55 dB on the *wideband* power
  and starve weak channels, so the default is fixed `agc: "manual"` at `gain_db:
  71.0` (near max). Lower `gain_db` only if a strong local signal overloads the
  front-end (audible distortion across channels).

Apply a new plan:

```bash
scp firmware/airband.json root@192.168.2.1:/root/airband.json   # password: analog
ssh root@192.168.2.1 /etc/init.d/S60maia-httpd restart
```

## Repository layout

| Path | What |
|---|---|
| `hdl/` | Amaranth HDL DSP blocks (channelizer, AM demod, framer) + sims; see `hdl/README.md` |
| `firmware/` | Build scripts, devicetree patch, channel-plan template, flashing guide (`firmware/README.md`) |
| `host/airband-reader/` | Rust host reader: demux, drop detection, WAV/raw output |
| `host/airband-listen/` | Rust interactive listener: play one channel live, switch on the fly |
| `maia-sdr/` | our Maia SDR fork (gitignored here; the airband HDL + `maia-httpd` integration) |
| `plutosdr-fw/` | Pluto firmware assembler (gitignored; pinned upstream) |
| `pluto-airband-fpga.md` | the authoritative project spec |
| `PROGRESS.md` | running engineering log + decisions |
| `DEV-SETUP.md` | dev environment, build server, and bitstream/firmware build details |

## Building from source

Firmware/bitstream are built on an x86-64 Linux server with Vivado 2023.2 (the Maia
Docker images are amd64-only). HDL authoring and simulation run natively on macOS.
Full setup — Mac dev env, build server, Vivado volume, `libiio` — is in
`DEV-SETUP.md`; the firmware build + flash specifics are in `firmware/README.md`.

## Credits

Built on [Maia SDR](https://maia-sdr.org/) by Daniel Estévez and the Maia SDR
project. The Pluto is an Analog Devices ADALM-Pluto (Zynq-7010 + AD9363, unlocked
to AD9364).
