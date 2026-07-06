# Pluto Airband — multichannel VHF airband receiver on the Pluto+

Turn a [Pluto+](https://github.com/plutoplus/plutoplus) (an open-hardware
ADALM-Pluto derivative: same Zynq-7010 + AD9363, plus Gigabit Ethernet, microSD,
and a 0.5 ppm VCTCXO) into an **18-channel VHF airband (AM aircraft voice)
receiver**. A single wideband capture is split into many narrow channels entirely
inside the Pluto+'s FPGA; each channel is AM-demodulated to audio on-chip and
streamed off the device over the network — enough to feed many
[LiveATC](https://www.liveatc.net/) audio streams from one SDR.

A small **Rust host workspace** consumes that stream: it demuxes the channels,
cleans up the audio (squelch, AGC, optional DeepFilterNet), and can record WAVs,
play channels live, push MP3 to Icecast/LiveATC, or hand raw PCM to your own
program.

> The same firmware also builds for the USB-only Analog Devices ADALM-Pluto
> (`TARGET=pluto`); the Pluto+ is the primary target and what these docs assume.

> **Status:** runs on hardware with all 18 channels streaming gap-free and the
> receiver auto-starting on boot. Known constraints are in
> [Known limitations](#known-limitations).

## What you need

- A **Pluto+** (or an ADALM-Pluto) and `dfu-util` to flash it.
- The two firmware images — build them yourself per [`BUILD.md`](BUILD.md), or
  use a prebuilt image if you have one.
- A **host** to run the receiver tools: any Linux/macOS machine or a Raspberry Pi
  (a Pi 5 comfortably runs all 18 channels with DeepFilterNet). It needs a Rust
  toolchain to build `host/`.
- A VHF **antenna** for the airband. One capture covers a 16 MHz window (channels
  within ~118.4–134.4 MHz; see [How it works](#how-it-works)). An airband band-pass
  filter + a low-noise amplifier ahead of the device gives the biggest quality win
  (see [Known limitations](#known-limitations)).
- A **microSD card** (FAT32) if you want a persistent, custom channel plan on a
  Pluto+; without one the firmware falls back to a single built-in channel.

## Documentation

This README is the hub. Each topic has a single home:

| If you want to… | Go to |
|---|---|
| Flash a device and listen (fastest path) | [Get started](#get-started) (below) |
| Consume the audio stream from your own program | [Audio output interface](#audio-output-interface-write-your-own-client) (below) |
| Run the Icecast feeder unattended on a Pi (systemd) | **[`deploy/README.md`](deploy/README.md)** |
| Understand the design, constraints, and rationale | **[`SPEC.md`](SPEC.md)** — authoritative design spec |
| Build firmware, flash, set up the dev/build env, troubleshoot | **[`BUILD.md`](BUILD.md)** |
| Know what each FPGA/DSP block does | **[`hdl/README.md`](hdl/README.md)** |
| Diagnose audio artifacts / understand the channel "buzz" | **[`firmware/diagnostics/README.md`](firmware/diagnostics/README.md)** |
| Read the full spur/"buzz" root-cause investigation (with plots) | **[`SPUR-INVESTIGATION.md`](SPUR-INVESTIGATION.md)** |
| Know the firmware image contents + DDR addressing invariants | **[`firmware/README.md`](firmware/README.md)** |
| See the engineering history and decisions | **[`PROGRESS.md`](PROGRESS.md)** |

## How it works

One wideband AD9361 capture is split into many narrow AM-voice channels
**entirely inside the Zynq-7010's FPGA**, AM-demodulated to audio on-chip, and
pushed off the device as a single TCP byte stream that the Rust host fans out to
recordings, live audio, and Icecast feeds. Every block in that path:

```
                       Pluto+  (Zynq-7010 = FPGA "PL" + ARM "PS")
 ┌─────────────────────────────────────────────────────────────────────────────┐
 │ antenna ─▶ AD9361 ── 12-bit IQ, 16 Msps ──▶  FPGA channelizer (PL)            │
 │            LO 126.4 MHz                                                        │
 │                                                                               │
 │   per-channel DDC, time-multiplexed — 6 lanes × 3 channels = 18:              │
 │      NCO mix ─▶ CIC decimate (÷160) ─▶ complex cleanup FIR (63-tap)           │
 │                                  │  (~100 ksps complex per channel)           │
 │                                  ▼                                            │
 │   round-robin collector ─▶ AM back-end (one datapath, folded over channels):  │
 │      CORDIC |I+jQ| ─▶ DC block ─▶ audio CIC (÷5) ─▶ 20 000 sps mono           │
 │                                  │                                            │
 │                                  ▼                                            │
 │   AudioFramer (8-byte records) ─▶ cyclic DMA ─▶ DDR ring (0x1900_0000)        │
 │                                                                               │
 │   maia-httpd (ARM): programs the AD9361 + per-channel NCOs, starts the DMA,   │
 │   drains the DDR ring, and serves the records over TCP :30000                 │
 └─────────────────────────────────────────────────────────────────────────────┘
                                    │  TCP :30000  (stream of 8-byte LE records)
                                    ▼
                Host / Raspberry Pi — Rust workspace (host/)
 ┌─────────────────────────────────────────────────────────────────────────────┐
 │ airband-reader:  router thread (demux by channel, drop detection via seq #)   │
 │    └─▶ one worker thread per channel:                                         │
 │          squelch (VOX | carrier) ─▶ [band-pass · denoise] ─▶ AGC ─▶           │
 │          DeepFilterNet ─▶ presence ─▶  WAV · Icecast/MP3 · UDP · metrics      │
 │ airband-listen:  same DSP, live speaker playback + scanner / meters / FFT     │
 │   shared libs:  airband-dsp (filters · squelch · denoise · AGC) · airband-dfn │
 └─────────────────────────────────────────────────────────────────────────────┘

   each TCP record (little-endian u64):
     [23:0]  audio sample (signed 24-bit)    [39:32] channel index 0..17
     [31:24] carrier level (8-bit minifloat)  [63:40] per-channel sequence #
```

**On the FPGA (the heart of the project).** A single AD9361 capture (LO
**126.4 MHz**, Fs **16 MHz**) covers the **119.2–133.65 MHz** airband window. A
brute-force receiver would need one full DDC per channel — far more DSP/BRAM than
the small XC7Z010 has. Instead the channelizer is **time-multiplexed**: each of
**6 lanes** sweeps **3 channels** one-per-clock through a single shared datapath,
so all **18** channels are produced with one set of arithmetic. Per channel the
pipeline is `NCO mix → CIC decimate (÷160) → complex cleanup FIR` (the FIR both
undoes the CIC's passband droop and gives sharp adjacent-channel selectivity). A
round-robin collector feeds a **shared AM back-end** — a ripple-free **CORDIC
`|I+jQ|`** envelope detector, a one-pole **DC block**, and an **audio CIC (÷5)** —
yielding **20 000 sps** (`16 MHz / 160 / 5`) mono audio per channel. The
`AudioFramer` packs each audio sample into an 8-byte record (sample + carrier
level + channel index + per-channel sequence number) that a **cyclic DMA** writes
into a DDR ring buffer. All of this is multiplier-lean (the AM back-end uses **zero
DSP48s**) so it fits alongside the stock Maia spectrometer.

**On the ARM (`maia-httpd`).** The Linux side configures the AD9361 front-end and
programs each channel's NCO tuning word at startup, starts and continuously drains
the DDR ring, and serves the raw record stream over **TCP `:30000`**. It also keeps
serving the Maia SDR web UI/waterfall (with the front-end controls **locked**, so a
browser can't retune the radio off-band).

**On the host (Rust workspace `host/`).** `airband-reader` is a **router +
per-channel-worker** pipeline: the router reads the socket, demuxes records by
channel index, and flags drops from the sequence counter — never blocking the
socket — while one worker per channel runs the full audio chain in parallel:
**squelch** (voice-VOX or carrier-based) → optional **band-pass / spectral
denoise** → **AGC** → **DeepFilterNet** neural speech enhancement → **presence**
brightness, then fans the result out to **WAV** recordings, **Icecast/MP3** feeds,
**UDP** PCM, and **Prometheus** metrics. `airband-listen` runs the identical DSP for
live speaker playback with a scanner, per-channel activity meters, and a live FFT.
The DSP and DFN stages are shared libraries (`airband-dsp`, `airband-dfn`) so both
binaries enhance audio identically.

**Why this is novel.** A full **multichannel channelizer + AM demodulator running
inside a $200-class SDR's tiny FPGA**, emitting finished *audio* (not raw IQ) over
the network, means **one** Pluto+ replaces a rack of single-frequency receivers (or
a host CPU churning through 16 MHz of IQ per channel) and feeds **18 LiveATC
streams at once**. The whole DSP is time-multiplexed and DSP48-frugal so it
coexists with the stock Maia spectrometer on the same chip, and it is built **on
top of** [Maia SDR](https://maia-sdr.org/) — the airband channelizer, DMA, and
control live in a fork (`github.com/juchong/maia-sdr`, `pluto-airband` branch)
while the Maia base (spectrometer/recorder/DDC) is preserved. The deep dive on each
block is in [`hdl/README.md`](hdl/README.md) and [`SPEC.md`](SPEC.md).

## Known limitations

These are inherent to the platform — worth understanding before you judge the
audio or plan an install.

- **Channel "buzz" is an internal (conducted) spur comb — not fixable in firmware
  or by antenna-side filtering.** Proven by capturing with the RX input terminated
  (**50 Ω, antenna disconnected**): a dense band-wide comb of on-board harmonics
  (the 120.000 MHz = 40 MHz-reference 3rd harmonic is one tooth) remains. It is
  independent of the HDL/DSP/demod, so an external enclosure, an antenna band-pass,
  and a notch on the strong local carrier do **not** remove it — but it **is**
  amplified by RX gain. Fs/LO-shift tests pin each tooth: the **dominant tooth is
  the ADC sample-clock harmonic** that lands in-band (9×14 = 126.000 on the original
  14 MHz build; relocated to 8×16 = **128.000** on the 16 MHz build, where it sits in
  a guard gap clear of every channel), plus a **125 MHz GbE clock** (Pluto+) and the
  **120 MHz reference 3rd harmonic** — all at fixed *absolute* frequencies. Effective
  levers: **lower RX gain**, **frequency planning** (keep channels off those fixed
  lines when you build your plan), and an **external
  reference** (for the 120 MHz line); input power, enclosure shielding, and a switcher
  bead do not help. Full root-cause analysis and a diagnostic toolkit:
  [`firmware/diagnostics/README.md`](firmware/diagnostics/README.md).
- **Single shared, adjustable RX gain.** One fixed manual RX gain serves all 18
  channels (the AD9361 AGC modes settle on *wideband* power and starve weak
  narrowband channels). It is **adjustable** — edit `gain_db` in the SD-card
  `airband.json` and restart the receiver; the firmware's **baked-in default (no SD
  card) is 0 dB**. The receiver is **internal-noise-limited**: the audio floor is the
  receiver's own internal noise (ADC quantization + the conducted spur comb), the same
  with the antenna or a 50 Ω load, so more gain raises the comb without improving SNR
  past a point and a quieter site cannot lower the floor. Tune `gain_db` to your
  front end — with an **external LNA** a low value (~12 dB) is plenty; a **bare**
  front end needs more (up to the ~48 dB ADC-clipping knee). The biggest quality win
  is front-end **dynamic range** (a SAW airband band-pass + low-NF LNA), not more
  gain. See [Channel plan](#channel-plan).
- **Transmitter quieted on boot.** This is a pure receiver, but the Pluto powers up
  in FDD with the TX LO running (2.45 GHz, ~10 dB attenuation) — radiated EMI to
  nearby radios. The firmware now powers down the TX LO and floors TX attenuation at
  boot (`firmware/patch_tx_quiet.py` → `S60maia-httpd`); RX is unaffected (the
  in-band noise floor is identical with TX on/off — measured). See `SPEC.md` §5.1.
- **Reference-oscillator calibration is per unit.** The Pluto+'s 0.5 ppm VCTCXO
  usually needs **no** correction; re-measure only if a known carrier shows drift.
  (A bare ADALM-Pluto's uncalibrated 40 MHz XO does need it — its ppm error shifts
  every channel.) Correct it once with the `ad936x_ext_refclk_override` u-boot var:
  measure coarsely against a known AM carrier
  ([`measure_offset.py`](firmware/diagnostics/README.md)), then precisely against an
  LTE downlink (`lte_calibrate.py`, ~0.01 ppm, GPS-disciplined). See `SPEC.md` §5.2.

## Get started

### 1. Flash the firmware

Build the two firmware images per [`BUILD.md`](BUILD.md) (or use a prebuilt
image): `boot.dfu` (FPGA bitstream + FSBL + bootloader) and the rootfs image
(`plutoplus.dfu` for a Pluto+, `pluto.dfu` for an ADALM-Pluto). Put the device in
DFU mode (power on while holding the button until the LED blinks slowly), then
flash **both** partitions (after any FPGA change you must reflash `boot.dfu` too —
the bitstream lives there and a mismatch hangs the receiver):

```bash
cd firmware/build
dfu-util -a boot.dfu     -D boot.dfu       # FPGA bitstream + FSBL + bootloader (mtd0)
dfu-util -a firmware.dfu -D plutoplus.dfu  # kernel + devicetree + rootfs       (mtd3)
dfu-util -a firmware.dfu -e                # detach + reboot (plain `-e` errors with >1 alt)
```

Set the USB-PHY-reset jumper to **URST↔MIO46** (MIO52 carries Ethernet MDIO). The
receiver starts automatically on boot and is reachable over USB at `192.168.2.1`
**and** on the Ethernet `eth0` (DHCP) IP. The firmware pins a **deterministic
Ethernet MAC** in the devicetree (a stock Pluto+ invents a random one each boot,
churning the DHCP lease/IP); override per unit with `PLUTO_MAC`. Full build +
flash + first-boot details (incl. the u-boot env that a `boot.dfu` flash wipes)
are in **[`BUILD.md`](BUILD.md)** and [`firmware/README.md`](firmware/README.md).

> **ADALM-Pluto variant.** The USB-only Analog Devices ADALM-Pluto (same XC7Z010
> die, `clg225` package, no Ethernet/microSD, bare XO) is also supported: build
> with `TARGET=pluto` (the build default) and flash `pluto.dfu` in place of
> `plutoplus.dfu`. The channel plan/gain then fall back to the built-in default
> (no microSD), and per-unit clock calibration is needed (no VCTCXO).

### 2. Set your channels

Put the frequencies you want and your RX gain in `airband.json` on the device's
microSD card, or use the browser config page — see [Channel plan](#channel-plan).
With no card the firmware uses a single built-in channel at 0 dB gain, which is
enough to confirm the receiver works before you commit a full plan.

### 3. Listen

The host tools are a Cargo workspace (`host/`): two binaries (`airband-reader`,
`airband-listen`) over shared libraries — `airband-dsp` (filters, squelch, AGC)
and `airband-dfn` (the DeepFilterNet enhancer + presence brightness, shared so
both binaries enhance audio identically). Build everything at once:

```bash
cargo build --release --manifest-path host/Cargo.toml
BIN=host/target/release/airband-reader

# live link health + stats: sample rate, drops, audio peak (dBFS), carrier (dB·c), transmissions
$BIN 192.168.2.1:30000

# record one WAV per *transmission* (squelch-gated, timestamped, no dead air)
$BIN 192.168.2.1:30000 --mode wav --out-dir caps

# one continuous WAV per channel instead (no squelch)
$BIN 192.168.2.1:30000 --mode wav --no-split --squelch off

# raw s16le per channel (chNN.s16) to pipe into an encoder/feeder
$BIN 192.168.2.1:30000 --mode raw --no-agc --shift 6
```

**Shared DSP chain** (in `airband-dsp`, ported from RTLSDR-Airband and adapted to
the FPGA's already-demodulated, DC-blocked audio): per-channel **squelch**, a
300–3400 Hz voice **band-pass**, a standalone **2.5 kHz low-pass** (`--lpf-hz`,
airband AM voice has no usable energy above ~3.4 kHz so a wider corner only passes
hiss), an optional **notch**, an STFT **noise reduction** stage that attenuates the
broadband hiss under the voice, and an **AGC** that normalizes loudness and
soft-clips peaks. **Both binaries now default to a DFN-centric chain**: AGC +
**DeepFilterNet** + presence brightness **on**, while the band-pass, 2.5 kHz LPF,
notch, and spectral denoise start **off** so DFN does the cleanup in isolation
(enable them with `--filter`, `--lpf-hz`, `--notch`, `--denoise`).
`airband-reader` and **`airband-listen` both default to squelch `auto`** (audio
VOX, see below); a carrier-level squelch proved fragile against the conducted
spur comb's per-channel offsets, so detection keys on in-band voice modulation.
(`airband-listen`'s activity meter still reads out carrier level — that is a
display metric, independent of the squelch.)

Because the FPGA DC-blocks the audio before the host sees it, the default squelch
works on voice energy (VOX) and uses a **hang time** (`--squelch-hang-ms`, default
1000 ms) to ride over the pauses in continuous speech (AWOS/ATIS).
RTLSDR-Airband's carrier-loss fast-close is therefore disabled here — with no
carrier it would just re-introduce chatter. Lower the hang for snappier closes on
push-to-talk traffic; raise it if a feed still chatters between words.

A bitstream built from the current `hdl/` also ships a per-channel **carrier
level** in each audio frame; with it, `--squelch carrier` gates on carrier power
instead of voice energy. All channels of one receiver see the same wideband
noise, so the host takes the **median** (50th percentile) of the live per-channel
carrier levels as a shared **noise reference** and opens any channel whose carrier sits
`--squelch-snr` dB above it. Because that threshold comes from the *other*
channels' noise (not a channel's own level), it holds a continuous carrier
(AWOS/ATIS) open with no hang and no chatter, while empty channels stay shut. The
new bitstream and host tools must be deployed together.

- `--squelch off|auto|manual|carrier` (`--squelch-snr`, `--squelch-level` dBFS,
  `--squelch-hang-ms`) — gating, threshold, and hang time.
- `--denoise` (off by default), `--denoise-floor-db <dB>` — spectral noise
  reduction (more negative floor = deeper, more aggressive cut).
- `--filter` (off by default), `--filter-low`/`--filter-high` — voice band-pass
  (300–3400 Hz).
- `--lpf-hz <Hz>` — standalone low-pass −3 dB corner applied after the band-pass
  (default `0` = off; e.g. `2500` to enable). In `airband-listen` toggle live with `l`.
- `--notch <Hz>` / `--notch-q` — tonal-spur notch.
- `--no-agc` falls back to fixed-gain output, where `--shift` scales the 24-bit
  sample to 16-bit (**positive = attenuate, negative = makeup gain**; airband AM
  is quiet so a negative value is usual).
- Recording defaults to **split-on-transmission** (one timestamped file per keyed
  transmission); `--no-split` writes one continuous file, `--min-transmission-ms`
  discards blips.

**Live outputs** (any mode, independent of recording):

```bash
# Icecast MP3 feed for LiveATC.net (16 kbps mono 22050 Hz by default)
$BIN 192.168.2.1:30000 --icecast-channel 0 \
  --icecast-host feed.example.net --icecast-port 8000 \
  --icecast-mount /KXXX.mp3 --icecast-password secret

# stream ALL configured channels to one or more servers (see "Icecast feeds")
$BIN 192.168.2.1:30000 --feeds feeds.json

# UDP s16le PCM of one channel to another host
$BIN 192.168.2.1:30000 --udp-channel 0 --udp-dest 10.0.0.5:7355

# Prometheus metrics + health/status (per-channel samples/drops/transmissions/level/
# floor/carrier-dBc/open, plus pipeline-health gauges and per-feed connect/bytes).
$BIN 192.168.2.1:30000 --metrics-port 9100   # /metrics /healthz /status on :9100

# Low-latency debug listen on one channel (raw PCM/WAV; no Icecast, ~100-200 ms lag).
# Channel is per-request; tap=pre is raw demod (continuous), tap=post is the gated
# enhanced audio LiveATC gets. Coexists with --feeds (no second Pluto connection).
$BIN 192.168.2.1:30000 --feeds feeds.json --monitor-port 8081
ffplay -fflags nobuffer -flags low_delay -probesize 32 -analyzeduration 0 \
  http://host:8081/listen/3.wav?tap=pre
# or an interactive TUI over this endpoint (channel plan auto-read from the Pluto):
#   cd host/airband-monitor && uv run airband-monitor host:8081   (see its README)

# Publish curated health/metrics to MQTT for a Home Assistant dashboard (auto-
# discovered entities + availability via Last Will). See deploy/README.md.
$BIN 192.168.2.1:30000 --feeds feeds.json --mqtt-broker homeassistant.local
```

**Health at a glance.** `/healthz` returns 200 only when the Pluto is reachable,
the `:30000` stream is up, samples are flowing, and every feed is connected (503
otherwise). The same signals answer the three questions directly — *is the Pluto
connected?* (a periodic TCP probe of the Pluto web port), *is the application
running?* (the stream is established), *is data flowing?* (a recent sample) — and
roll up into two headline booleans, `system_healthy` (capture side) and
`liveatc_healthy` (every output feed connected), exported to both Prometheus and
MQTT. Running under systemd, the reader uses `Type=notify`/`WatchdogSec` so a hung
reader is restarted; see [`deploy/README.md`](deploy/README.md) for the full
monitoring/alerting setup.

### Icecast feeds (stream every channel; one or more servers)

The single `--icecast-channel` flags above feed exactly one channel to one
mount. To feed **many channels** — and optionally the **same channel to several
servers** — pass a JSON **feeds file** with `--feeds feeds.json`. Each entry
binds one channel index to one mount on one server; repeat a `channel` to fan it
out to a backup/second server, and list every channel to feed them all. A feeds
file and the `--icecast-*` flags can be used together (the flags add one more
feed). Each feed runs its own encoder + auto-reconnecting source thread, and
falls behind (drops audio) rather than blocking the others if a server stalls.

The committed [`feeds.json`](feeds.json) is an **example** you adapt: it maps each
channel to a mount on a local Icecast and fans the public channels out to a
LiveATC ingest. **Keep passwords out of the file** — write them as `${ENV_VAR}`
(e.g. `${AIRBAND_ICECAST_PASSWORD}`) and they are expanded from the environment at
load time (`${NAME}` works in `server`, `mountpoint`, `username`, `password`,
`name`, `genre`, and `description`; a referenced-but-unset variable is a hard
error). The file is then safe to commit, with the secrets supplied only at run
time (e.g. a systemd `EnvironmentFile` — see
[`deploy/README.md`](deploy/README.md)).

The file is **strict JSON — no comments**; the `//` annotations below are
illustrative only and must be removed in the real file:

```jsonc
{
  "feeds": [
    // 118.050 AWOS (channel 0) to the primary LiveATC server, plain source port
    { "channel": 0, "server": "feed.liveatc.net", "port": 8000,
      "mountpoint": "/KXXX_AWOS.mp3", "username": "source", "password": "secret",
      "name": "KXXX AWOS 118.050", "genre": "ATC", "description": "KXXX AWOS" },

    // same channel mirrored to a private server over TLS
    { "channel": 0, "server": "icecast.example.net", "port": 8443,
      "mountpoint": "/KXXX_AWOS.mp3", "password": "secret2", "tls": "transport" },

    // tower (channel 13) to the primary server
    { "channel": 13, "server": "feed.liveatc.net", "port": 8000,
      "mountpoint": "/KXXX_TWR.mp3", "password": "secret", "name": "KXXX Tower" }
  ]
}
```

Per-feed options (defaults in parentheses):

| Key | Meaning |
|---|---|
| `channel` | **Required.** Channel index `0..N-1` into the receiver's channel plan (the order in `channels_hz`). |
| `server` | **Required.** Icecast server hostname. |
| `port` | Server port (`8000`). |
| `mountpoint` | **Required.** Mount path, e.g. `/KXXX.mp3`. |
| `username` | Source username (`"source"`). |
| `password` | Source password (`""`). Supports `${ENV_VAR}` expansion — keep real passwords out of the committed file (see [`deploy/README.md`](deploy/README.md)). |
| `name` | ICY stream name shown in directories (`"Pluto airband chN"`). |
| `genre` | ICY genre tag (unset). |
| `description` | ICY description tag (unset). |
| `bitrate` | MP3 bitrate in kbps (`16`; LiveATC uses 16). |
| `samplerate` | MP3 output sample rate in Hz (`22050`; the 20000 sps audio is resampled to this). |
| `tls` | TLS negotiation (`"disabled"`): `disabled` plain TCP; `transport` implicit TLS (a TLS-only listener); `upgrade` RFC 2817 in-band upgrade; `auto` try TLS then fall back to plain; `auto_no_plain` TLS only, no fallback. |
| `tls_insecure` | **Testing only** (`false`): accept invalid/self-signed certs and hostname mismatches. This disables protection against man-in-the-middle interception (which can capture your source password) — never set it for a public feed. |

The same options exist as single-stream flags: `--icecast-host`, `--icecast-port`,
`--icecast-mount`, `--icecast-user`, `--icecast-password`, `--icecast-name`,
`--icecast-genre`, `--icecast-description`, `--icecast-bitrate`,
`--icecast-samplerate`, `--icecast-tls`, `--icecast-tls-insecure`.

**Plain vs. TLS.** Icecast's native source port speaks plain HTTP, so the
default `tls: "disabled"` is correct for a direct source port (e.g. `8000`, or a
dedicated source port). If your server is only reachable through an HTTPS reverse
proxy that accepts the `SOURCE` method, use `tls: "transport"` against the
HTTPS port. If the source connection is rejected (wrong password, bad mount,
etc.) the reader now logs the server's HTTP status and reconnects, rather than
silently going quiet.

#### Run the feeder as a service

To keep the all-channel feeder running unattended (auto-start on boot, restart on
crash) — for example on a Raspberry Pi at a tower site — run it under **systemd**.
The unit and a secrets-env template ship in `deploy/`, and the full setup/update
runbook is in **[`deploy/README.md`](deploy/README.md)**.

> **One reader per Pluto.** The Pluto serves a **single client** on `:30000`;
> running several `airband-reader` instances against it at once makes them fight
> over the socket and can wedge `maia-httpd` (recover with
> `ssh root@<pluto> /etc/init.d/S60maia-httpd restart`). When cleaning up stray
> readers, match by process **name** (`pkill -x airband-reader`), not `pkill -f`
> (which self-matches the shell running it).

### Multi-channel enhancement on the Pi 5 (DeepFilterNet for every feed)

`airband-reader` runs the **same enhancement chain as `airband-listen`** on
**every** streamed channel — VOX squelch (voice-band) → AGC → **DeepFilterNet** →
**presence brightness** → soft clip, with the band-pass/LPF/notch/spectral-denoise
**off by default** (DFN-centric, enable per the flags above) — so all 18 Icecast
mounts sound like the tuned single-channel listener, and the *identical* enhanced
audio goes to Icecast, UDP, and WAV recordings.

To keep this real-time across 18 channels on the Raspberry Pi 5 (4× Cortex-A76)
the reader is a **router + per-channel worker** pipeline:

- A **router thread** reads the TCP stream, detects drops, maintains the
  carrier-mode noise reference, and hands each sample to its channel's worker
  over an **unbounded queue** — it never blocks the socket and **never drops** a
  sample.
- One **worker thread per channel** runs that channel's full DSP + DFN + presence
  and fans the result out to its outputs, so the Pi's cores process channels in
  parallel and a slow DFN inference never stalls the stream.
- **DeepFilterNet models load eagerly at startup** (the router waits on a barrier
  until all workers' models are ready before reading the stream), so no
  transmission is ever missed to a cold model. This costs a fixed ~10–15 MB per
  streamed channel of RAM — trivial on the Pi 5's 8 GB.
- A global **DFN concurrency cap** (`--dfn-max-active`, default **3**) bounds how
  many NN **inferences** run simultaneously, leaving a core for I/O. The permit is
  acquired **per inference hop** (around each forward pass) and released right
  after, not held for a whole transmission — so a continuously-keyed channel
  (AWOS/ATIS) can never hold a slot long enough to **starve** the others; permits
  free between hops and rotate among all open channels. The cap **never bypasses
  or drops**: a hop that can't get a permit *waits*, so that channel's audio is
  buffered (adding latency) and still fully filtered. For real airband traffic —
  only a few channels keyed at once, DFN running faster than real time on one A76
  — the cap rarely engages; raise it if you have headroom, lower it if you see
  drops or thermal throttling.

DFN tuning and presence flags are shared with the listener (see below):
`--no-dfn`, `--dfn-min-snr` (−20), `--dfn-atten-lim` (15), `--dfn-pf-beta`
(0.02), `--presence-db` (8), `--presence-hz` (1600), `--presence-q` (0.707), and
`--dfn-max-active` (3).

### Audition channels live (testing)

To listen to a channel on your speakers and flip between frequencies in real time:

```bash
cargo build --release --manifest-path host/Cargo.toml
host/target/release/airband-listen 192.168.2.1:30000
```

`airband-listen` runs the `airband-dsp` chain on the played channel with a
**DeepFilterNet-centric default**: VOX squelch + AGC + **DeepFilterNet** on, while
the host band-pass, 2.5 kHz LPF, notch, and spectral denoise start **off** so DFN
does the cleanup in isolation (each is still runtime-toggleable). It runs the
squelch on *every* channel so the meter shows which frequencies are active. Interactive
keys: `↑/↓` (or `j`/`k`, `[`/`]`) step channels, type a number then `Enter` to jump,
`+`/`-` adjust volume, `m` mutes, `s` toggles squelch, `a` toggles AGC, `f` toggles
the band-pass, `l` toggles the **2.5 kHz low-pass**, `n` toggles a configured notch,
`d` toggles **noise reduction**, `D` toggles **DeepFilterNet** (see below), `p`
toggles the **post-DFN brightness boost**, `g` toggles a **live FFT window** (see
below), `F` toggles **follow** (scanner) mode, `q` quits.
`--monitor single|follow|mix` selects single-channel, scanner, or sum-of-open-
channels playback. The per-channel meter shows **carrier level in dB over the
cross-channel noise reference** (`dB·c`) — idle channels sit ~0, a keyed station
reads positive even when its demod audio is buried — plus a squelch-open dot, the
selected channel's squelch state, and cumulative dropped samples.

**DeepFilterNet enhancement (`D`).** Runs the [DeepFilterNet](https://github.com/Rikorose/DeepFilterNet)
neural speech enhancer on the played stream. It is **on by default** (the host
band-pass/LPF/notch/spectral-denoise instead start off so DFN cleans up alone);
toggle it with `D`. The DFN3 model is embedded in the binary and fed via streaming
resampling from the 20000 sps audio to its native 48 kHz. It runs **after** the
per-channel filter + AGC chain (so it sees a healthy, in-range signal) and **only
while the squelch is open** (the inference is expensive and there is nothing to
enhance in muted silence), adding ~tens of ms of latency. It complements the
spectral `d` denoiser — try either or both. First enable loads the model (a
one-time ~½ s hitch).

DFN's stock config (`--dfn-min-snr` −10, unlimited attenuation) mangles weak
airband speech two ways: it mutes any frame below the local-SNR floor (chopping
quiet/low-SNR speech into silence — the "garbled mumble") and, with unlimited
attenuation, it fully guts frames it judges as noise — which includes noise-like
consonants (CH/S/F), so they get chopped. The tuned defaults fix both:

- `--dfn-min-snr` (default **−20**) — the mute floor; lower keeps fainter speech,
  raise toward −10 to gate weak frames harder.
- `--dfn-atten-lim` (default **15** dB) — caps the maximum attenuation so a frame
  is never fully muted (`enh = (1−m)·enhanced + m·noisy`, `m = 10^(−dB/20)`),
  which preserves consonants. **Lower** (e.g. 12) for less chopping but more
  residual noise; **raise** (toward 100 = unlimited) for deeper suppression.
- `--dfn-pf-beta` (default **0.02**) — DFN's post-filter for trimming residual
  musical noise; `0` disables.

**Brightness boost (`p`).** A high-shelf EQ applied to the cleaned speech *after*
DFN, restoring the upper voice band (~2–3.4 kHz consonants) that the denoiser
rolls off on weak signals (the "muffled" symptom). On by default; tune with
`--presence-db` (default **8**, shelf gain), `--presence-hz` (default **1600**,
corner), and `--presence-q` (default **0.707**). Set `--presence-db 0` to disable.

**Live FFT window (`g`) — debugging.** Pops up a native GUI plotting a **Welch** PSD
(2048-pt Hann, ~7 averaged segments) of the active channel's post-DSP audio, so you
can *see* the effect of the band-pass/LPF/notch/denoise/AGC (toggle them off to
inspect the raw demod spectrum). Hover for a precision crosshair reading frequency
and magnitude; the Y axis is locked (drag/scroll to adjust, double-click to reset).
Press `g` again or close the window to hide it.

### Audio output interface (write your own client)

`airband-reader`/`airband-listen` are thin clients over a single **raw TCP byte
stream**; there is no IIO device and no per-channel socket. To get audio out from
any language, implement this contract (the authoritative spec is
[`SPEC.md`](SPEC.md) §6):

- **Connect** to TCP `192.168.2.1:30000`. The server only writes; it pushes a
  continuous stream of fixed **8-byte little-endian records**, 8-byte aligned from
  the first byte (it sends whole 64 KiB buffers), so just read 8 bytes at a time.
- **Each record is one audio sample for one channel:**

```
bits [23:0]  audio sample    — signed, 24-bit, two's complement
bits [31:24] carrier level   — 8-bit minifloat of the AM carrier (0 = none); for squelch
bits [39:32] channel index   — 0..17
bits [63:40] sequence number — per-channel, +1 per sample, wraps at 2**24
```

- **Demux:** switch on the channel byte and append the sample to that channel's
  stream. Each channel is mono PCM at **20000 sps** (`Fs/160/5`). Records from
  different channels are interleaved; within one channel they are in order.
- **Drop detection:** a per-channel jump in the sequence number (delta > 1) means
  `delta-1` samples were dropped (FPGA FIFO overflow or a slow client). The server
  buffers ~256 chunks per client and *lags* a client that can't keep up — that
  surfaces as a sequence jump, never a stall, so always check the counter.
- **Level:** the 24-bit sample is near unity-gain and airband AM is quiet (often
  tens of LSB), so apply makeup gain before narrowing to 16-bit (the tools default
  to a left-shift of 6 ≈ +36 dB; see `--shift`).

Minimal consumer (decode + demux, no resampling):

```python
import socket, struct
s = socket.create_connection(("192.168.2.1", 30000))
buf = b""
while True:
    buf += s.recv(65536)
    while len(buf) >= 8:
        word = struct.unpack_from("<Q", buf)[0]
        buf = buf[8:]
        sample = word & 0xFFFFFF         # 24-bit audio
        if sample & 0x800000:            # sign-extend the 24-bit field
            sample -= 1 << 24
        carrier = (word >> 24) & 0xFF    # AM carrier minifloat (squelch; 0 = none)
        chan = (word >> 32) & 0xFF       # 0..17
        seq  = (word >> 40) & 0xFFFFFF   # per-channel sequence
        # `sample` is one 20000 sps mono sample for channel `chan`
```

### Web UI (Maia spectrometer) — front-end is read-only

The Pluto still serves the Maia SDR web UI at `http://192.168.2.1:8000`, and its
waterfall is handy for seeing live activity across the **119–134 MHz** airband
window. Because the airband receiver owns the single AD9361 front-end (its
channelizer is built for **126.4 MHz / 16 Msps**), those controls — RX freq,
sampling freq, RF bandwidth, gain, AGC — are **locked read-only** while the
receiver is running. This is deliberate: the web UI used to silently retune the
radio to its 2.4 GHz / 61.44 Msps defaults on page load, which moved the
front-end off-band and made every channel demodulate noise. The lock is enforced
server-side (`/api/ad9361` is a no-op under `--airband`), so the radio stays on
the airband band no matter what the browser does.

## Channel plan

The receiver reads its config from the **SD card** (`/mnt/sdcard/airband.json`) at
startup — copy your plan to the card's root as `airband.json` (the card must be
**FAT32**; the kernel has no exFAT). The committed `firmware/airband.json` is an
**example plan** (Seattle-area channels) to replace with your own; the web config
page reads and writes the same file. If no card is mounted, `maia-httpd` falls
back to a **minimal built-in default — one channel (118.050) at 0 dB** — so
hearing only a single faint channel is the obvious sign the SD plan did not load.
`gain_db` is **adjustable** — edit it to suit your front end (see **Gain** below);
the built-in default (no SD card) is **0 dB**:

```jsonc
{
  "center_hz":   126400000,   // AD9361 RX LO (capture center) — keep within the built window
  "samp_rate":   16000000,    // MUST stay 16 MHz (the rate the channelizer was built for)
  "rf_bandwidth":16000000,
  "gain_db":     30.0,        // fixed manual gain — ADJUSTABLE; built-in default (no SD card) is 0 dB (see Gain)
  "agc":         "manual",    // "manual" | "slow_attack" | "fast_attack" | "hybrid"
  "poll_ms":     20,
  "channels_hz": [ 119200000, 119900000, /* … your channels, up to 18 … */ 126875000, 133650000 ]
}
```

Rules:
- **`samp_rate` must remain `16000000`.** The channelizer's filters/decimation are
  baked into the FPGA bitstream for this rate; changing it requires an HDL rebuild.
- Up to **18** entries in `channels_hz`, each within `center_hz ± samp_rate/2`
  (i.e. 118.4–134.4 MHz). Out-of-window channels are rejected at startup. The
  FPGA always frames **18 positional channels**, so frame channel *index* = position
  in `channels_hz`; with fewer than 18 entries the trailing positions are stale, and
  the host reader must run `--channels = len(channels_hz)` to ignore them. Your
  `feeds.json` channel indices refer to these same positions.
- Changing `center_hz` re-tunes the whole window; keep all desired channels inside it.
- **Gain:** one shared manual RX gain serves all channels (`agc: "manual"`; the
  AD9361 AGC modes settle on *wideband* power and starve weak channels). It is
  **adjustable** — edit `gain_db` on the SD card and restart the receiver (below).
  The firmware's **baked-in default (no SD card) is 0 dB**. The receiver is
  **internal-noise-limited** (the idle audio floor is the same with the antenna or a
  50 Ω load and barely moves with gain → it is ADC quantization + the conducted comb,
  not antenna noise), so `gain_db` only needs to lift voice clear of that floor. Tune
  it to your front end:
  - **External LNA (recommended):** a low `gain_db` (~12 dB) clears quantization
    without clipping; the LNA sets the system noise figure and lets the AD9361's
    internal gain stage (the dominant comb generator) run low. The biggest further
    win is a **SAW airband band-pass + low-NF LNA**.
  - **Bare front end (no LNA):** raise `gain_db` higher — toward the ~48 dB
    ADC-clipping knee (e.g. ~30 dB), accepting a more prominent comb
    ([`firmware/diagnostics/floor_sweep.py`](firmware/diagnostics/floor_sweep.py) finds the knee).

  Gain does **not** remove the fixed-frequency channel "buzz" (the internal clock-tone
  comb; see
  [`firmware/diagnostics/README.md`](firmware/diagnostics/README.md)).

Apply a new plan — edit it on the SD card (the persistent source of truth):

```bash
# Mount the FAT32 SD card on any computer and copy the file to its root as
# airband.json, then reinsert and reboot the Pluto. Or, in place on the device:
cat firmware/airband.json | ssh root@192.168.2.1 'cat > /mnt/sdcard/airband.json'  # password: analog
ssh root@192.168.2.1 /etc/init.d/S60maia-httpd restart
```

(The Pluto's dropbear has no `sftp-server`, so `scp` fails — pipe over `ssh`, or
`scp -O`. Editing `/root/airband.json` no longer takes effect: the receiver now
loads `--airband-config /mnt/sdcard/airband.json`.)

### Web config page (recommended)

Instead of editing JSON by hand, open the browser config page served by the
Pluto:

```
http://192.168.2.1:8000/airband.html     # also linked from the main UI → settings → "Other"
```

It shows a **live spectrum + waterfall** of the captured band (fed by the same
`/waterfall` WebSocket as the main UI) with the channel plan overlaid, so you can
*see* whether a channel carries traffic before committing to it. The waterfall
uses the same dB color scaling as the main UI, so a busy band looks identical
here:

![Airband config page — live spectrum, waterfall, full-band minimap, and channel markers](docs/images/airband-overview.png)

- **Add / remove / reorder / relabel** channels; drag a marker (or edit the MHz
  field) to move a channel; "Add peak" snaps a new channel onto the strongest
  visible signal. A slot counter enforces the **18**-channel limit.
- **Zoom & pan:** type a frequency to auto-zoom to it, scroll-wheel to zoom
  (tuned to stay gentle on trackpads/touchscreens), drag to pan, or use the
  **full-band minimap** to jump around. Known RF spur and DC bands are shaded so
  you can avoid placing channels on them.
- **Per-channel signal meters** (peak vs. noise floor) refresh on every
  waterfall frame, so the bars track activity in near-real time; a suggested
  squelch level is derived from the noise floor.

![Channel list with live per-channel signal meters](docs/images/airband-channels.png)

- **Front-end controls:** RF bandwidth, gain (**0–77 dB**, bounded), AGC mode,
  and poll interval are editable. **Center frequency and sample rate are locked**
  (marked with a lock badge): the sample rate is fixed by the bitstream, and the
  center is held so saved channels stay inside the capture window
  `[center − Fs/2, center + Fs/2)`.
- **Presets + import/export** of the JSON plan.

![Front-end settings with locked center frequency and sample rate, and a bounded gain field](docs/images/airband-frontend.png)

Saving **persists `/mnt/sdcard/airband.json`** (on the SD card, so it survives
power cycles and reflashes) and shows a banner; click **Restart receiver** (or
reboot) to apply — the channelizer NCOs are programmed once at startup, so a
restart is required. The page drives the
[`/api/airband` REST API](SPEC.md#65-web-config-api-and-page); see `SPEC.md` for
the request/response schema.

## Repository layout

| Path | What |
|---|---|
| `README.md` | this hub: overview, get-started, channel plan, doc map |
| `SPEC.md` | the authoritative project spec (design, constraints, rationale) |
| `BUILD.md` | dev/build env, firmware build + flash, troubleshooting |
| `PROGRESS.md` | running engineering log + decisions |
| `hdl/` | Amaranth HDL DSP blocks (channelizer, AM demod, framer) + sims; see `hdl/README.md` |
| `firmware/` | build scripts, devicetree patch, channel-plan template, image notes (`firmware/README.md`) |
| `firmware/diagnostics/` | RF diagnostic toolkit + the buzz root-cause analysis (`firmware/diagnostics/README.md`) |
| `host/` | Rust host workspace (shared libs + two client binaries) |
| `host/airband-dsp/` | shared DSP library: voice band-pass, notch, squelch, noise reduction, AM AGC, presence high-shelf, dBFS |
| `host/airband-dfn/` | shared DeepFilterNet enhancer + presence brightness (used by both binaries) |
| `host/airband-reader/` | host reader: router + per-channel workers, demux, drop detection, DFN/presence DSP, split recording, Icecast/UDP/metrics |
| `host/airband-listen/` | interactive listener: live DSP playback, scanner/mix modes, per-channel meters |
| `host/airband-monitor/` | uv-managed Python listener: streams a channel's pre/post-filtered audio from the reader's `--monitor-port`, channel plan auto-read from the Pluto (`host/airband-monitor/README.md`) |
| `feeds.json` | example all-channel Icecast feeds file (adapt to your own mounts) |
| `deploy/` | systemd feeder unit + secrets template + runbook (`deploy/README.md`) |
| `maia-sdr/` | the Maia SDR fork (gitignored here; the airband HDL + `maia-httpd` integration) |
| `plutosdr-fw/` | Pluto firmware assembler (gitignored; pinned upstream) |

## Building from source

Firmware/bitstream are built on an x86-64 Linux host with Vivado 2023.2 (the Maia
Docker images are amd64-only); HDL authoring and simulation run natively on macOS.
The full recipe — dev env, Vivado volume, `libiio`, firmware build + flash — is in
**[`BUILD.md`](BUILD.md)** (with image specifics in
[`firmware/README.md`](firmware/README.md)).

## Credits

Built on [Maia SDR](https://maia-sdr.org/) by Daniel Estévez and the Maia SDR
project. The hardware is a [Pluto+](https://github.com/plutoplus/plutoplus) — an
open-hardware derivative of the Analog Devices ADALM-Pluto (Zynq-7010 + AD9363,
unlocked to AD9364) adding Gigabit Ethernet, microSD, and a 0.5 ppm VCTCXO.
