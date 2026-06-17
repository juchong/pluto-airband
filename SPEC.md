# Pluto Airband — Design Spec

> The authoritative design spec: *what* this is, *why* it is built this way, and
> the *as-built* parameters. Section numbers (§) referenced from code comments and
> `PROGRESS.md` point here. For build/flash/ops see `BUILD.md`, for the running
> engineering log see `PROGRESS.md`, for the per-block HDL reference see
> `hdl/README.md`, and start at `README.md` for the hub.

## What this is

A **21-channel VHF airband (AM aircraft voice) receiver** running entirely on an
Analog Devices ADALM-Pluto. One wideband AD9361 capture is split into 21 narrow
channels inside the Pluto FPGA, each channel is AM-demodulated to audio on-chip,
and only demodulated audio is streamed off the device over the network. The
intended end use is a multi-channel audio feeder for **liveatc.net**.

**Why an FPGA channelizer (not a host channelizer).** The goal is *contiguous*
coverage of the whole airband from a single tuner. The prior feeder used an
SDRplay RSPduo, whose dual-tuner architecture swaps unreliably, leaves a dead
zone between tuners, and whose ~10 MHz window cannot span the required channel
spread. The Pluto's AD936x captures the entire band in one contiguous,
single-tuner slice with no seam and no swap logic. The motivation is **coverage,
not front-end quality** — better demodulation alone was never the win.

## 1. Goal and outcome

**Goal:** replace the RSPduo feeder with a single Pluto that channelizes and
AM-demodulates many airband voice channels in fabric and ships only audio
off-device.

**Status against the original success criteria:**

| Criterion | Status |
|---|---|
| Pluto firmware exposing many independently-tuned AM channels | **Done — 21 channels, mono, 24-bit, 15625 sps** |
| Demodulated audio reaches a host over the Pluto link, within USB 2.0 | **Done — framed audio over TCP `:30000`, well under USB 2.0** |
| 24/7 stability at an antenna site | **Partially — gap-free + auto-start on boot; long soak pending** |
| Pi-side daemon encodes each channel and pushes to liveatc Icecast | **Not yet built** (see §9) |

**Non-goals:** replicating RTLSDR-Airband's CPU FFT-channelizer; transmitting;
non-airband modes (NFM is a possible future per-channel option, not implemented).

## 2. Key facts and constraints

### 2.1 Airband is AM
VHF airband voice (118–136.975 MHz) is **amplitude modulation** (A3E DSB full
carrier) on 25 kHz channel spacing. The demodulator is an **AM envelope detector**
(magnitude of the complex baseband), not an FM arctangent-differentiator. This is
why the per-channel DSP is cheap enough to replicate 21×.

### 2.2 The interface-bandwidth argument (why demod is on the FPGA)
Shipping wideband IQ to the host does not fit the link, so demod *must* happen on
the FPGA:

- Pluto host link is **USB 2.0**: ~480 Mbps ≈ 60 MB/s, realistically ~40–50 MB/s.
- Wideband IQ = `sample_rate × 4 bytes/s`: 14 Msps → 56 MB/s, 61.44 Msps →
  ~246 MB/s. Neither fits comfortably with overhead.
- **Demodulated audio**: 15625 sps mono, packed 8 bytes/sample = 125 KB/s/channel;
  21 channels ≈ 2.6 MB/s raw, trivial over USB. **This is the chosen path.**

Because the FPGA produces audio (not IQ), RTLSDR-Airband — whose pipeline is
strictly IQ-in → channelize → demodulate → audio-out — has no role downstream. We
write a small host reader instead.

### 2.3 Hardware target (known-good, not an open question)
- **SoC:** Xilinx Zynq-7010 (dual ARM Cortex-A9 PS + Artix-7-class PL).
- **RFIC:** a genuine ADI AD9363 from a genuine ADALM-Pluto, with the "unlock"
  register written so it operates with **AD9364-class range and wide contiguous
  bandwidth**. The AD9363 and AD9364 are the same silicon die; the practical
  AD9364 prerequisite (a more accurate clock source) is present on this unit.
  - Sensitivity / noise figure at ~120 MHz are valid and equal to the datasheet's
    800 MHz characterization point (the datasheet only characterizes 800 MHz /
    2.4 GHz / 5.5 GHz because those were the lead customer's frequencies; they are
    not performance boundaries). No empirical sensitivity check gated the project.
- **Net effect:** front-end capability is not a project risk. The single
  feasibility gate that mattered was FPGA resource fit (§4), which passed.

## 3. Built on Maia SDR

This receiver is built **on top of** [Maia SDR](https://maia-sdr.org/) by Daniel
Estévez. The airband DSP, DMA, and control live in our fork
(`github.com/juchong/maia-sdr`, `pluto-airband` branch); the Maia base
(spectrometer / recorder / DDC) is preserved and still runs.

- **maia-hdl** — the FPGA design in **Amaranth** (Python HDL), synthesized to
  Verilog and packaged as a Vivado IP core. Our airband receiver is vendored under
  `maia_hdl/airband/` and instantiated from `maia_hdl/maia_sdr.py` (IP core
  `_version = 0.6.2`).
- **maia-httpd** — async **Rust** app on the ARM (PS): configures the AD9361 and
  per-channel NCOs, starts the cyclic DMA, drains the DDR ring, and serves the
  framed audio over TCP. Register map is auto-generated (Amaranth → SVD →
  `svd2rust`), keeping HW/SW definitions consistent.
- **maia-wasm** — the Rust/WASM web UI (still served; read-only over RF while the
  airband receiver owns the front-end).
- **maia-kmod** — kernel module for FPGA↔CPU DMA buffer cache coherency.

Maia's **DDC** (NCO+mixer → decimating low-pass) is the per-channel building block
the airband channelizer generalizes by time-multiplexing.

## 4. As-built architecture

```
        Antenna (airband BPF / FM-notch recommended as front-end hygiene)
                     │
   ┌─────────────────┴───────────────────────────────────────────────┐
   │  ADALM-Pluto (Zynq-7010)                                          │
   │                                                                   │
   │  AD9361  ──12-bit IQ @ 14 Msps──►  PL (FPGA), sync clock 62.5 MHz │
   │  LO 123.438 MHz                    │                               │
   │                ┌───────────────────┴─────────────────────────┐    │
   │                │ ReceiverTop (maia_hdl/airband):              │    │
   │                │   6 time-multiplexed channelizer lanes       │    │
   │                │     per channel: NCO mix → CIC dec-128       │    │
   │                │                 → 63-tap cleanup FIR         │    │
   │                │   → round-robin collector                    │    │
   │                │   → TdmAmBackend: CORDIC |I+jQ| → DC block   │    │
   │                │                 → CIC audio dec-7            │    │
   │                │   → AudioFramer (64-bit records)             │    │
   │                │   → cyclic DmaStreamWrite → DDR ring         │    │
   │                └───────────────────┬─────────────────────────┘    │
   │  maia-httpd (ARM): AD9361 + NCO config, DMA, drains ring,         │
   │  serves framed audio over TCP :30000.  Web UI on :8000.          │
   └────────────────────────────────────┬──────────────────────────────┘
                                         │ USB 2.0 / TCP (audio only)
                                         ▼
   ┌───────────────────────────────────────────────────────────────┐
   │  Host (PC now; Raspberry Pi at the tower in production)         │
   │   airband-reader (Rust): demux by channel, drop detection,      │
   │     24→16-bit scale, WAV / raw / live link stats                │
   │   airband-listen (Rust): play one channel live, switch on the   │
   │     fly                                                          │
   │   [future] encode (LAME) → Icecast source client → liveatc      │
   └───────────────────────────────────────────────────────────────┘
```

### 4.1 The channelizer fits the Z-7010
Dozens of independent DDCs do not fit a Z-7010; **time-multiplexing** does. The PL
runs at 62.5 MHz while each channel's output is far slower, so one physical DDC
datapath iterates across several channels. The resource/throughput gate (see
`hdl/feasibility_25ch.py`, `hdl/realtime_budget.py`) confirmed **GO** against the
measured Maia base platform, and the integrated lane placed-and-routed with timing
met at 62.5 MHz. Per-channel state lives in block RAM and the lane datapath is
pipelined (READ→MIX→INTEG→COMB) to close timing.

### 4.2 Deployed DSP parameters (`maia_hdl/maia_sdr.py`)
These are the exact constants baked into the shipping bitstream.

| Parameter | Value | Notes |
|---|---|---|
| Channels (`n_channels`) | **21** | indices 0..20 |
| Channels per lane (`chans_per_lane`) | **4** | → `ceil(21/4)` = **6 lanes** (`[4,4,4,3,3,3]`) |
| Input | **12-bit IQ @ 14 Msps** | AD9361 delivers the working rate directly (no separate front-end decimator) |
| Lane decimation (`decimation`) | **128** | channel rate = 14 MHz / 128 = 109375 sps |
| Cleanup FIR | **63-tap complex, folded** | `design_cic_compensation(128, 3, 63, 0.22, 0.46)`, `out_shift=17`; inverts CIC droop + channel selectivity |
| AM magnitude | **CORDIC vectoring**, 12 iterations | ripple-free `|I+jQ|`; no angle-dependent gain modulation (multiplier-free) |
| DC block | one-pole high-pass, `dcblock_k=10` | strips the carrier-DC term left by the detector |
| Audio decimation (`audio_decim`) | **7**, CIC order (`cic_stages`) **4** | audio rate = 109375 / 7 = **15625 sps** |
| Audio sample width | **24-bit** signed | scaled to 16-bit on the host |
| NCO width | 24-bit | per-channel tuning words written via a flat register interface |
| PL clock | **62.5 MHz** (`sync`) | design is timing-closed at this rate |

End-to-end the HDL is verified **bit-exact** against Python reference models at
each stage (see `hdl/README.md`).

### 4.3 Per-channel signal flow and sample rates
A single 12-bit IQ stream at **14 Msps** (zero-IF, LO 123.438 MHz) is broadcast to
all 6 lanes. Each channel's path:

```
14 Msps IQ ─► NCO complex mix ─► CIC ÷128 ─► 63-tap complex cleanup FIR ─►
   (tune f-LO to DC)            109.375 ksps   (un-droop + selectivity)
─► CORDIC |I+jQ| ─► one-pole DC block ─► CIC ÷7 (order 4) ─► 15.625 ksps audio
   (M=12, ×5/8 gain fix)  (strip carrier DC)                  (24-bit, signed)
```

- **NCO + mix:** a 24-bit NCO tunes each channel's carrier `f` to DC. The tuning
  word is `round(((f − LO) / Fs) · 2^24)`, computed by `maia-httpd` and written per
  channel (see §6). `(f − LO)/Fs` must lie in `[−0.5, 0.5)` or the channel is
  rejected at startup.
- **CIC ÷128 + cleanup FIR:** a multiplier-free CIC drops each channel to 109.375
  ksps; the 63-tap complex FIR (`design_cic_compensation`) flattens the CIC
  passband droop and supplies the sharp adjacent-channel selectivity the CIC's
  gentle roll-off cannot. Both are **folded** (one engine time-shared across a
  lane's channels), with per-channel state in BRAM.
- **AM detect:** a **CORDIC vectoring** magnitude (12 iterations) computes `|I+jQ|`
  with no angle-dependent gain ripple (residual < −140 dBc); its fixed gain
  K≈1.6468 is corrected by a 5/8 shift-add so audio levels are unchanged. This
  replaced an alpha-max-beta-min estimator whose ~10% angle ripple modulated each
  off-tuned carrier into ~−30 dBc demod spurs.
- **DC block + audio CIC:** a one-pole leaky-integrator high-pass removes the
  carrier-amplitude DC term the detector leaves, then an order-4 CIC decimates ÷7
  to **15625 sps** mono audio (`14e6 / 128 / 7`).

The AM back-end is itself folded over all 21 channels (one datapath, per-channel
DC/CIC state in BRAM); the magnitude is shared combinational logic.

## 5. Front-end configuration and channel plan

The receiver reads `/root/airband.json` at startup (template:
`firmware/airband.json`), falling back to built-in defaults:

- **LO / capture center:** 123.438 MHz.
- **Sample rate:** 14 MHz — **fixed**; the channelizer's decimation/filters are
  baked into the bitstream for this rate. Changing it requires an HDL rebuild.
- **Capture window:** center ± 7 MHz = 116.438–130.438 MHz. The 21 channels span
  118.05–128.5 MHz; every channel sits inside the central ~80% of the band, clear
  of the filter skirts. (Channel selection and the LO choice — placing the zero-IF
  DC/LO-leakage spur in a guard gap — are derived in `hdl/capture_window.py`.)
- **Gain:** one shared RX gain serves all 21 channels (no per-channel AGC). The
  default is fixed **manual 71 dB**, near max, to favour weak/intermittent airband
  signals; the AD9361 AGC modes settle on wideband power and starve weak channels.
  At strong-signal sites 71 dB can clip the wideband ADC (~15% observed) — lower
  `gain_db` if you hear distortion, trading sensitivity.

While `maia-httpd` runs with `--airband`, the AD9361 front-end is **locked
read-only** (`/api/ad9361` is a no-op and the web UI disables RF controls) so the
web UI cannot retune the radio off the airband band.

## 6. Control plane and data plane

The ARM (`maia-httpd`, `src/airband.rs`) configures the radio and DSP, then does
nothing but copy finished DDR buffers to TCP clients — all per-sample work is in
fabric. (For a consumer-facing summary of the output stream — the contract for
writing your own client — see `README.md` → "Audio output interface".)

### 6.1 Configuration (one-time, at start)
On boot with `--airband`, `maia-httpd` reads `/root/airband.json` (or built-in
defaults) and:
1. Sets the AD9361 **sampling frequency** (14 MHz), **RX RF bandwidth** (= Fs),
   and **RX LO** (123.438 MHz).
2. Sets the gain: `agc: "manual"` → manual gain mode + `gain_db` (71 dB); otherwise
   the named AGC mode.
3. Computes each channel's 24-bit NCO word `round(((f − LO)/Fs)·2^24)` and writes
   it to the FPGA, rejecting any channel outside `±Fs/2`.
4. Enables the receiver and starts the cyclic DMA.

These go through the **airband register bank** (AXI-Lite, base `0x40`), crossed
into the PL `sync` domain by a `RegisterCDC`:

| Register | Fields |
|---|---|
| `airband_control` | `enable`, `dma_start`, `dma_stop` |
| `airband_freq_addr` | global channel index for the next NCO write |
| `airband_freq` | `freq_wren`, `freq_wdata` (the 24-bit NCO word) |
| `airband_dma_next_address` | (read) DDR address the DMA is currently writing |

### 6.2 Framing and the DDR ring
`AudioFramer` packs each per-channel audio sample into one fixed **64-bit
little-endian record**:

```
bits [31:0]  audio sample (signed, sign-extended to 32 bits; 24-bit content)
bits [39:32] channel index (0..20)
bits [63:40] per-channel sequence counter (wraps at 2**24; gap = dropped samples)
```

A **cyclic** `DmaStreamWrite` (`width=64`) streams these over an AXI-HP port into a
**16 MiB DDR ring** at `0x19000000`, organized as **256 × 64 KiB** slots
(`buffer-size 0x10000`). This addressing is an invariant shared by three sources of
truth (FPGA `airband_address_range`, devicetree `reg`, kmod `buffer-size`) — see
`firmware/README.md`. A small staging FIFO in the framer absorbs the per-CIC-boundary
burst; if it ever backs up, the dropped sample surfaces as a sequence-counter jump
(an `overflow` bit), never as corrupted data.

### 6.3 Draining and serving
`maia-httpd` polls `airband_dma_next_address` every `poll_ms` (20 ms), and for every
slot that is **≥2 buffers behind** the one being written (so the DMA has certainly
committed it to DDR), it cache-invalidates the slot and broadcasts the whole 64 KiB
buffer to all connected TCP clients on `0.0.0.0:30000`. Clients that fall more than
256 buffers behind are lagged (and see a sequence jump). The stream is therefore
**raw 64-bit records**, not per-channel sockets.

### 6.4 Host reader
`host/airband-reader` (Rust) connects to `:30000`, demuxes records by the channel
byte, and uses the per-channel sequence counter to count dropped samples. It scales
the 24-bit sample to 16-bit via `--shift` (positive = right-shift/attenuate,
negative = left-shift/makeup gain; default **−6** ≈ +36 dB, since airband AM audio
is quiet). Modes: `stats` (live link health, default), `wav` (one WAV/channel), and
`raw` (`chNN.s16` per channel for piping into an encoder). `host/airband-listen`
plays one channel live and switches on the fly. Neither applies the voice band-pass
by default; it only masks artifacts (see §7).

### 6.5 Web config API and page
The channel plan and front-end settings can be edited from a browser instead of
hand-editing JSON. `maia-httpd` exposes a small REST API consumed by the static
page `maia-wasm/assets/airband.html` (served at `/airband.html`, also linked from
the main UI). The page renders a live spectrum + waterfall from the existing
`/waterfall` WebSocket — using the same dB color scaling as the main UI — overlays
the channel plan, and offers zoom/pan (wheel sensitivity tuned for
trackpads/touchscreens), a full-band minimap, and add/remove/reorder/relabel
editing. Per-channel signal meters (peak vs. measured noise floor) refresh on
every waterfall frame so the bars track activity in near-real time. **Center
frequency and sample rate are presented read-only** (with a lock badge): the
sample rate is fixed by the bitstream, and locking the center keeps saved
channels inside the capture window. The gain field is bounded to `[0, 77] dB`. To
keep the displays responsive the page requests a faster spectrometer output rate
(≈ 20 Hz) on load when the device is slower; this rate is shared with the main
`:8000` waterfall. Annotated screenshots are in `README.md` → *Web config page*.

| Endpoint | Purpose |
|---|---|
| `GET /api/airband` | Returns the persisted (pending) config: `center_hz`, `samp_rate`, `rf_bandwidth`, `gain_db`, `agc`, `channels` (`[{freq_hz, label?}]`), `poll_ms`, plus capability/read-only fields `max_channels` (21), `samp_rate_locked` (`true`), `enabled` (receiver active), and `needs_restart` (persisted ≠ running). |
| `PATCH /api/airband` | Merges the provided fields, **validates** (≤ `max_channels` channels, each within `center_hz ± samp_rate/2`, `gain_db ∈ [0, 77]`, `poll_ms ≥ 1`; `samp_rate` cannot be changed), and persists the merged config to `/root/airband.json`. Returns the updated config. |
| `POST /api/system/restart` | Restarts the `maia-httpd` service (detached, ~1 s delay) so a freshly saved config is applied. If unavailable, reboot manually. |

The config handlers only read/write the JSON file — they never touch the radio
or FPGA. Because the channelizer NCO words are programmed once at startup
(§6.1), saved changes take effect only after the receiver is restarted (hence
`needs_restart` and the page's "Restart receiver" button). `AppState` keeps the
config loaded at boot (the *running* plan) so the handler can report whether a
restart is pending. Optional per-channel labels are stored in a
`channel_labels` array parallel to `channels_hz`; they are cosmetic (used only
by the web UI) and ignored by the receiver.

## 7. Known limitation: the channel "buzz" is an RF hardware spur

A persistent audible buzz on several channels was root-caused to a **physical,
fixed-frequency RF spur**, *not* a DSP/HDL/gain problem. A ~485 kHz spur comb
phase-locked to **120.000 MHz** — exactly the **3rd harmonic of the Pluto's 40 MHz
reference** — falls inside some channels' passbands. The spurs are invariant to
sample rate, LO, and gain (confirming reference-locked physical RF, not digital
aliases or clipping intermod), and a spur inside a 25 kHz channel cannot be removed
by the per-channel NCO/CIC/FIR/AM chain. Remedies are **hardware** (clean power,
shielding, external reference, channel triage). Full analysis and the diagnostic
toolkit are in `firmware/diagnostics/README.md`.

(The CORDIC magnitude detector replaced an earlier alpha-max-beta-min estimator
that added its own ~−30 dBc demod spurs from angle-dependent gain ripple. CORDIC
is the correct, artifact-free detector — but it is not the fix for the RF spur.)

## 8. Development and build stack

Develop on **macOS (Apple Silicon)**, build on **x86-64 Linux** — Vivado is
x86-64 only. Amaranth authoring and cocotb/Icarus simulation run natively on the
Mac; only synthesis + bitstream + firmware assembly run on the server (Dockerized
Vivado **2023.2**, Amaranth **0.5.8**). Flashing is independent of building:
`dfu-util` over USB from the Mac. Full environment, build, and flash procedures
are in **`BUILD.md`**; image contents and addressing invariants in
**`firmware/README.md`**.

## 9. Remaining work

- **Pi-side liveatc streamer:** the host reader produces per-channel PCM/WAV; the
  daemon that encodes (LAME) and pushes each channel to liveatc's Icecast as a
  source client is not yet built. See `PROGRESS.md` → Next steps.
- **Long-duration soak** for 24/7 antenna-site stability (USB/TCP streaming,
  thermal/power, watchdog/restart, monitoring).
- **Squelch/AGC:** none currently. A coarse power-squelch could move into the FPGA
  later; finer AGC/level is better iterated on the host first.
- **Optional NFM mode** (CORDIC arctan-differentiator) if ever needed.

## 10. Licensing / legal

- maia-hdl: MIT. maia-httpd / maia-wasm: Apache-2.0 or MIT. Contributions upstream
  are welcome and aligned with the Maia SDR roadmap.
- Airband monitoring legality varies by country (legal in US/UK/NL/CH and others;
  illegal without a license in e.g. Germany). The operator is an authorized
  liveatc feeder; confirm local rules for the deployment site.
