# Pluto Airband — Design Spec

> The authoritative design spec: *what* this is, *why* it is built this way, and
> the *as-built* parameters. Section numbers (§) referenced from code comments and
> `PROGRESS.md` point here. For build/flash/ops see `BUILD.md`, for the running
> engineering log see `PROGRESS.md`, for the per-block HDL reference see
> `hdl/README.md`, and start at `README.md` for the hub.

## What this is

An **18-channel VHF airband (AM aircraft voice) receiver** running entirely on a
[Pluto+](https://github.com/plutoplus/plutoplus) (an open-hardware ADALM-Pluto
derivative; §2.4). One wideband AD9361 capture is split into 18 narrow channels
inside the Pluto+ FPGA, each channel is AM-demodulated to audio on-chip, and only
demodulated audio is streamed off the device over the network. The intended end
use is a multi-channel audio feeder for **liveatc.net**.

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
| Pluto firmware exposing many independently-tuned AM channels | **Done — 18 channels, mono, 24-bit, 20000 sps** |
| Demodulated audio reaches a host over the Pluto link, within USB 2.0 | **Done — framed audio over TCP `:30000`, well under USB 2.0** |
| 24/7 stability at an antenna site | **Partially — gap-free + auto-start on boot; long soak pending** |
| Pi-side daemon encodes each channel and pushes to liveatc Icecast | **Done — `airband-reader` has a built-in LAME→Icecast source client (16 kbps mono 22050 Hz); soak pending** |

**Non-goals:** replicating RTLSDR-Airband's CPU FFT-channelizer; transmitting;
non-airband modes (NFM is a possible future per-channel option, not implemented).

## 2. Key facts and constraints

### 2.1 Airband is AM
VHF airband voice (118–136.975 MHz) is **amplitude modulation** (A3E DSB full
carrier) on 25 kHz channel spacing. The demodulator is an **AM envelope detector**
(magnitude of the complex baseband), not an FM arctangent-differentiator. This is
why the per-channel DSP is cheap enough to replicate 18×.

### 2.2 The interface-bandwidth argument (why demod is on the FPGA)
Shipping wideband IQ to the host does not fit the link, so demod *must* happen on
the FPGA:

- Pluto host link is **USB 2.0**: ~480 Mbps ≈ 60 MB/s, realistically ~40–50 MB/s.
- Wideband IQ = `sample_rate × 4 bytes/s`: 16 Msps → 64 MB/s, 61.44 Msps →
  ~246 MB/s. Neither fits comfortably with overhead.
- **Demodulated audio**: 20000 sps mono, packed 8 bytes/sample = 160 KB/s/channel;
  18 channels ≈ 2.9 MB/s raw, trivial over USB. **This is the chosen path.**

Because the FPGA produces audio (not IQ), RTLSDR-Airband — whose pipeline is
strictly IQ-in → channelize → demodulate → audio-out — has no role downstream. We
write a small host reader instead.

### 2.3 Hardware target (known-good, not an open question)
The deployed board is a **Pluto+** (§2.4); the ADALM-Pluto is the alternate build
target. Both share the same SoC and RFIC silicon:

- **SoC:** Xilinx Zynq-7010 (dual ARM Cortex-A9 PS + Artix-7-class PL).
- **RFIC:** a genuine ADI AD9363 (the same part used in the ADALM-Pluto), with the
  "unlock" register written so it operates with **AD9364-class range and wide
  contiguous bandwidth**. The AD9363 and AD9364 are the same silicon die; the practical
  AD9364 prerequisite (a more accurate clock source) is present on this unit.
  - Sensitivity / noise figure at ~120 MHz are valid and equal to the datasheet's
    800 MHz characterization point (the datasheet only characterizes 800 MHz /
    2.4 GHz / 5.5 GHz because those were the lead customer's frequencies; they are
    not performance boundaries). No empirical sensitivity check gated the project.
- **Net effect:** front-end capability is not a project risk. The single
  feasibility gate that mattered was FPGA resource fit (§4), which passed.

### 2.4 Deployed hardware: Pluto+
The deployed board is the **Pluto+** (the open `plutoplus/plutoplus` board; build
with `TARGET=plutoplus`, see `BUILD.md`). It is the **same XC7Z010 die** as the
ADALM-Pluto — a different package (`xc7z010clg400-1`) with a different MIO pinout,
but identical FPGA resources, so the 18-channel design fits unchanged (no resource
re-fit). Relative to the USB-only ADALM-Pluto, the Pluto+ adds three things this
project cares about:

- **Gigabit Ethernet** — a higher-bandwidth alternative to the USB link.
  `maia-httpd` binds `0.0.0.0:30000`, so the framed-audio stream is served on
  the Ethernet `eth0` interface (DHCP) with no firmware change; the host reader
  just connects to the Ethernet IP. (The interface-bandwidth argument of §2.2 is
  unchanged — we still ship only demodulated audio — but Ethernet removes the
  USB 2.0 ceiling for future wider plans.)
- **0.5 ppm VCTCXO** — a disciplined reference vs the ADALM-Pluto's bare,
  uncalibrated XO, so the per-unit `ad936x_ext_refclk_override` calibration of
  §5.2 is generally **unnecessary** (leave nominal 40 MHz; re-measure only if a
  known carrier shows drift worth correcting).
- **Better shielding / power** — may reduce the audible modulation of the
  120 MHz reference-harmonic spur (§7), though the spur itself is reference-locked
  RF; confirm on the unit with the diagnostics toolkit rather than assume.

The Pluto+ requires the USB-PHY-reset jumper at **URST↔MIO46** (MIO52 carries
Ethernet MDIO) when running this firmware.

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
   │  Pluto+ (Zynq-7010)                                               │
   │                                                                   │
   │  AD9361  ──12-bit IQ @ 16 Msps──►  PL (FPGA), sync clock 65.3 MHz │
   │  LO 126.4 MHz                      │                               │
   │                ┌───────────────────┴─────────────────────────┐    │
   │                │ ReceiverTop (maia_hdl/airband):              │    │
   │                │   6 time-multiplexed channelizer lanes       │    │
   │                │     per channel: NCO mix → CIC dec-160       │    │
   │                │                 → 63-tap cleanup FIR         │    │
   │                │   → round-robin collector                    │    │
   │                │   → TdmAmBackend: CORDIC |I+jQ| → DC block   │    │
   │                │                 → CIC audio dec-5            │    │
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
   │   airband-dsp / airband-dfn (Rust libs): squelch, band-pass,    │
   │     notch, denoise, AGC, DeepFilterNet, presence                │
   │   airband-reader (Rust): router + per-channel workers, demux,   │
   │     DFN/presence DSP, WAV/raw, Icecast(LAME)/UDP/metrics         │
   │   airband-listen (Rust): live DSP playback, scanner/mix modes,  │
   │     per-channel dBFS meters                                      │
   └───────────────────────────────────────────────────────────────┘
```

### 4.1 The channelizer fits the Z-7010
Dozens of independent DDCs do not fit a Z-7010; **time-multiplexing** does. The PL
runs at 65.278 MHz while each channel's output is far slower, so one physical DDC
datapath iterates across several channels. The PL clock is held **≥ `4·Fs`** so the
shortest input-sample gap (`floor(Fpl/Fs)=4` PL cycles) still covers the
`chans_per_lane+1 = 4` cycles a lane needs to absorb a sample — i.e. it never drops
one. (The original 62.5 MHz gave only `floor(3.906)=3` cycles and silently dropped
the ~9.4% of samples on the short beat → 18.1 ksps instead of 20; see
`hdl/realtime_budget.py`.) The resource/throughput gate (see
`hdl/feasibility_25ch.py`, `hdl/realtime_budget.py`) confirmed **GO** against the
measured Maia base platform, and the integrated design placed-and-routed with timing
met at the shipped 65.278 MHz (WNS +0.153 ns). Per-channel state lives in block RAM
and the lane datapath is pipelined (READ→MIX→INTEG→COMB) to close timing.

### 4.2 Deployed DSP parameters (`maia_hdl/maia_sdr.py`)
These are the exact constants baked into the shipping bitstream.

| Parameter | Value | Notes |
|---|---|---|
| Channels (`n_channels`) | **18** | indices 0..17 (capped at 18: 21 ch → 7 lanes overflows the XC7Z010 LUTs) |
| Channels per lane (`chans_per_lane`) | **3** | → `ceil(18/3)` = **6 lanes** (`[3,3,3,3,3,3]`) |
| Input | **12-bit IQ @ 16 Msps** | AD9361 delivers the working rate directly (no separate front-end decimator) |
| Lane decimation (`decimation`) | **160** | channel rate = 16 MHz / 160 = 100000 sps |
| Cleanup FIR | **63-tap complex, folded** | `design_cic_compensation(160, 3, 63, 0.164, 0.2625)`, `out_shift=17`; inverts CIC droop and doubles as the channel-select filter, set to the full AM voice bandwidth (~±8 kHz at the 100000 sps channel rate: flat through ~6 kHz, −1.1 dB @8 kHz, ~−95 dB at the 25 kHz adjacent channel) |
| AM magnitude | **CORDIC vectoring**, 12 iterations | ripple-free `|I+jQ|`; no angle-dependent gain modulation (multiplier-free) |
| DC block | one-pole high-pass, `dcblock_k=10` | strips the carrier-DC term left by the detector |
| Audio decimation (`audio_decim`) | **5**, CIC order (`cic_stages`) **4** | audio rate = 100000 / 5 = **20000 sps** (order-4 CIC droop −1.6 dB @3.4 kHz, −5.1 dB @6 kHz) |
| Audio sample width | **24-bit** signed | scaled to 16-bit on the host |
| NCO width | 24-bit | per-channel tuning words written via a flat register interface |
| PL clock | **65.278 MHz** (`sync`; 2×/3× = 130.556/195.833) | raised from 62.5 MHz so `floor(Fpl/Fs)=4 ≥ chans_per_lane+1` (no dropped samples); timing-closed, WNS +0.153 ns |

End-to-end the HDL is verified **bit-exact** against Python reference models at
each stage (see `hdl/README.md`).

### 4.3 Per-channel signal flow and sample rates
A single 12-bit IQ stream at **16 Msps** (zero-IF, LO 126.4 MHz) is broadcast to
all 6 lanes. Each channel's path:

```
16 Msps IQ ─► NCO complex mix ─► CIC ÷160 ─► 63-tap complex cleanup FIR ─►
   (tune f-LO to DC)            100.000 ksps   (un-droop + selectivity)
─► CORDIC |I+jQ| ─► one-pole DC block ─► CIC ÷5 (order 4) ─► 20.000 ksps audio
   (M=12, ×5/8 gain fix)  (strip carrier DC)                  (24-bit, signed)
```

- **NCO + mix:** a 24-bit NCO tunes each channel's carrier `f` to DC. The tuning
  word is `round(((f − LO) / Fs) · 2^24)`, computed by `maia-httpd` and written per
  channel (see §6). `(f − LO)/Fs` must lie in `[−0.5, 0.5)` or the channel is
  rejected at startup.
- **CIC ÷160 + cleanup FIR:** a multiplier-free CIC drops each channel to 100.000
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
  carrier-amplitude DC term the detector leaves, then an order-4 CIC decimates ÷5
  to **20000 sps** mono audio (`16e6 / 160 / 5`).

The AM back-end is itself folded over all 18 channels (one datapath, per-channel
DC/CIC state in BRAM); the magnitude is shared combinational logic.

## 5. Front-end configuration and channel plan

The receiver reads its config from the **SD card** at startup —
`/mnt/sdcard/airband.json` (the init script mounts the card and passes
`--airband-config /mnt/sdcard/airband.json`; the canonical file is
`firmware/airband.json`). The card must be **FAT32** (the kernel has no exFAT).
The web config UI reads/writes this same SD file, so edits persist. If the card
is missing/unformatted/unmounted, `maia-httpd` falls back to a **deliberately
minimal built-in default — a single channel (118.050 AWOS) at 0 dB gain** — so a
faint, single AWOS channel is the obvious cue that the SD plan did not load. That
fallback is the only place 118.050 AWOS appears: it is deliberately **excluded**
from the operational plan (a continuous AWOS carrier we don't track). With 133.65
MHz added (the 16 MHz capture), the live plan fills **18 channels**
(`N_CHANNELS`; capped at 18 because 21 ch → 7 lanes overflows the XC7Z010 LUTs);
the frame channel index = position in `channels_hz`, so the host reader runs
`--channels 18`.
The SD config is the source of truth (persistence model in §8.1); the operational
values below are what `firmware/airband.json` ships:

- **LO / capture center:** 126.4 MHz.
- **Sample rate:** 16 MHz — **fixed**; the channelizer's decimation/filters are
  baked into the bitstream for this rate. Changing it requires an HDL rebuild.
- **Capture window:** center ± 8 MHz = 118.4–134.4 MHz. The 18 channels span
  119.2–133.65 MHz; every channel sits inside the central ~91% of the band (the
  119.2 and 133.65 extremes at ~90.6%), clear of the filter skirts. (Channel
  selection and the LO choice — placing the zero-IF DC/LO-leakage spur in a guard
  gap — are derived in `hdl/capture_window.py`.)
- **Gain:** one shared RX gain serves all 18 channels (no per-channel *RF* AGC;
  per-channel audio AGC is done on the host, §6.4); fixed **manual gain**, set by
  `gain_db` in the SD-card `airband.json` (the AD9361 AGC modes settle on wideband
  power and starve weak channels). It is **adjustable** — edit `gain_db` and restart
  the receiver; the firmware's **baked-in default (no SD card) is 0 dB**. The
  receiver is **internal-noise-limited, not antenna/thermal-limited**: the idle audio
  floor is the same with the antenna or a 50 Ω termination (Δ0.4 dB) and rises only
  ~1 dB per +6 dB of gain, so it is **ADC quantization + the conducted comb (§7)**
  downstream of the gain — a quieter site cannot lower it, and `gain_db` only needs to
  lift voice clear of that floor. Tune it to the front end:
  - **External LNA (recommended):** a low `gain_db` (~12 dB) clears quantization
    without clipping; the LNA sets the system noise figure (Friis) and lets the
    AD9361's internal gain stage (the dominant comb/intermod generator, §7) run low.
    The biggest further win is a **SAW airband band-pass + low-NF LNA** so strong
    out-of-band signals don't starve the 12-bit ADC.
  - **Bare front end (no LNA):** raise `gain_db` higher — toward the ~48 dB
    ADC-clipping knee (`firmware/diagnostics/floor_sweep.py` finds it; the ~71 dB
    near-ceiling clips ~13–15 % of the wideband ADC), accepting a more prominent comb
    that still falls in the channel-plan guard gaps.

While `maia-httpd` runs with `--airband`, the AD9361 front-end is **locked
read-only** (`/api/ad9361` is a no-op and the web UI disables RF controls) so the
web UI cannot retune the radio off the airband band.

### 5.1 Transmitter quieted on boot (receive-only)

This is a pure receiver, but the Pluto powers up with the AD9361 in **FDD with the
TX LO running** (`out_altvoltage1_TX_LO_powerdown=0`, default `TX_LO=2.45 GHz`) at
only ~10 dB TX attenuation. That radiates a 2.45 GHz carrier (plus TX-path noise)
which is audible EMI to nearby radios. There is no rx-only ENSM mode on the Pluto
AD9361 driver (`ensm_mode_available = sleep wait alert fdd pinctrl
pinctrl_fdd_indep`) and RX requires FDD, but in FDD the RX and TX LOs are
independent synthesizers, so the TX is quieted without touching RX:

- `out_altvoltage1_TX_LO_powerdown = 1` — power down the TX LO synth.
- `out_voltage0_hardwaregain = -89.75` — TX attenuation floor (range `[-89.75 … 0]`).

RX is unaffected (waterfall, per-channel meters, and the 126.4 MHz RX LO all
unchanged). An A/B on the raw `/waterfall` feed (median of 60 frames, 4096 bins)
shows the in-band noise floor is identical with TX on vs off (~72.5 dB) — 2.45 GHz
is far outside the 118–138 MHz window, so the TX never raised our *own* floor; the
benefit is the eliminated external carrier. (The separate in-band 120 MHz spur is
the 40 MHz reference's 3rd harmonic, not the TX — see §7.)

The rootfs is **ramfs**, so this is not a runtime `maia-httpd` setting; it is baked
into the boot init script. `firmware/patch_tx_quiet.py` idempotently injects a
backgrounded, retry-until-`ad9361-phy`-ready block into the `start)` case of
`buildroot/board/pluto/S60maia-httpd`, wired into `firmware/build_firmware_full.sh`
(step 3c). On a running unit the same two `iio` writes apply the change immediately;
it becomes power-cycle-persistent at the next firmware flash.

### 5.2 Reference-oscillator calibration (per unit)

The 40 MHz reference's ppm error shifts every tuned frequency proportionally. The
Pluto+'s 0.5 ppm VCTCXO usually needs **no** correction (re-measure only if a known
carrier shows drift); a bare ADALM-Pluto's uncalibrated XO does. Correct it once per
unit via the u-boot env var **`ad936x_ext_refclk_override`**: the `adi_loadvals`
boot script does
`fdt set /clocks/clock@0 clock-frequency <value>`, so the AD9361 driver computes its
PLL/decimation from the *true* reference and the nominal 126.4 MHz LO / 16 MHz Fs
are hit exactly (all 18 channels corrected together; the channelizer NCO math is
unchanged). `/clocks/clock@0` is the `ad9364_ext_refclk` the transceiver runs on.

Two gotchas, both learned on hardware:
- **Angle brackets are mandatory.** `fdt set ... clock-frequency <N>` needs the
  literal `<>` (an integer cell); a bare decimal is stored as a *string* and
  silently ignored, leaving the baked DTB value. Set it as
  `fw_setenv ad936x_ext_refclk_override "<40000064>"`.
- **The value is the absolute true XO, not 40 MHz×(1+ppm).** The booted DTB bakes a
  non-nominal clock, and the displayed carrier scales as
  `f_true·(believed_ref / true_xo)`. So the override = the *current* believed
  reference × (true/displayed), i.e. derived from the measured error against the
  **live** clock — not from nominal 40 MHz. The diagnostics compute this for you.

**Calibration procedure** (two references, coarse → precise):

1. **Coarse — known AM carrier (gets within the LTE capture range, ±10 ppm).**
   `firmware/diagnostics/measure_offset.py [true_MHz]` measures a commissioned
   **AWOS/ASOS** (default 118.050 MHz) against the live believed reference and
   prints the bracketed `fw_setenv`. Recorder-only. Accuracy is limited by the
   25 kHz AM channel (~1 ppm), so it is a bring-up step, not the final word.
2. **Precise — LTE downlink (GPS-disciplined, ~0.01 ppm).**
   `firmware/diagnostics/lte_calibrate.py --freq <MHz>` tunes to a cell's downlink
   center and measures the carrier frequency offset by **cyclic-prefix (CP)
   autocorrelation**: every LTE OFDM symbol repeats its tail one FFT length (128
   samples @ 1.92 Msps) earlier, so `r[n]·conj(r[n+128])` carries phase
   `−2π·CFO·128/fs` at every CP sample; summed over a 0.15 s capture the CP samples
   add coherently and yield CFO to ~10 Hz, unambiguous over ±7.5 kHz. eNodeBs hold
   ±0.05 ppm by 3GPP TS 36.104, so the cell is absolute truth — and at ~750 MHz the
   high-RF leverage beats a 19 kHz FM pilot or 24 kHz AM audio by orders of
   magnitude. Run `--selftest` (no hardware) to validate the estimator; omit
   `--freq` to auto-scan common US bands. `--apply` programs the override, reboots,
   and re-measures until the residual converges.

**Apply / persist:** the override lives in the env partition and survives reboot,
but a `boot.dfu` flash resets the env — re-apply with
`firmware/pluto_setup_env.py --refclk-hz <hz>` (brackets added automatically;
default carries this board's value). **Per unit** — re-measure for a different Pluto.

## 6. Control plane and data plane

The ARM (`maia-httpd`, `src/airband.rs`) configures the radio and DSP, then does
nothing but copy finished DDR buffers to TCP clients — all per-sample work is in
fabric. (For a consumer-facing summary of the output stream — the contract for
writing your own client — see `README.md` → "Audio output interface".)

### 6.1 Configuration (one-time, at start)
On boot with `--airband`, `maia-httpd` reads `/mnt/sdcard/airband.json` from the
SD card (or the built-in AWOS-only 0 dB fallback if no card is mounted) and:
1. Sets the AD9361 **sampling frequency** (16 MHz), **RX RF bandwidth** (= Fs),
   and **RX LO** (126.4 MHz).
2. Sets the gain: `agc: "manual"` → manual gain mode + `gain_db` (12 dB default,
   tuned for an external LNA — clears ADC quantization, §5); otherwise the named AGC mode.
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
bits [23:0]  audio sample (signed, 24-bit; host sign-extends to 32 bits)
bits [31:24] carrier level (8-bit minifloat [exp(5)|mant(3)] of the DC-block
             carrier estimate; 0 = none / pre-carrier bitstream)
bits [39:32] channel index (0..17)
bits [63:40] per-channel sequence counter (wraps at 2**24; gap = dropped samples)
```

The audio sample narrowed from 32 to 24 bits to free byte `[31:24]` for the
per-channel **carrier level** the DC-block stage (§6.2) would otherwise discard:
the FPGA encodes the carrier-DC running mean as an 8-bit log minifloat
(`am_backend_tdm.py`) so the host can run a true **carrier-power squelch** (§6.4).
The audio content was already only 24 bits, so no precision is lost. This is a
**breaking frame change**: the new bitstream and host tools must be deployed
together (old hosts misread the new framing; new hosts read carrier `0` from old
bitstreams and silently fall back to VOX squelch).

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

### 6.4 Host tools and shared DSP
The host side is a Cargo workspace (`host/`): two binaries over shared libraries
`host/airband-dsp` (filters, squelch, AGC, presence high-shelf) and
`host/airband-dfn` (the DeepFilterNet enhancer + presence brightness, shared so
both binaries enhance identically). The DSP is ported from RTLSDR-Airband and
adapted to the fact that the FPGA already AM-demodulates and DC-blocks the audio
(§6.1-6.2), so there is no IQ/de-emphasis stage on the host. The chain per channel
is: **squelch** (EWMA noise-floor tracking → SNR/manual threshold, with an
open-delay/hang state machine; mutes inter-transmission static) → **voice
band-pass** (4th-order Butterworth, 300-3400 Hz) → a standalone **low-pass**
(2nd-order Butterworth, off by default; `--lpf-hz` sets the −3 dB corner, e.g.
2500 — airband AM voice has no usable energy above ~3.4 kHz so a tighter corner
trims hiss) → optional
**notch** (2nd-order band-stop for a tonal spur) → **AM AGC** (loudness
normalization with a bounded soft-clip and click-free fade on close). All units
work in raw 24-bit sample magnitude; level metering is reported in dBFS.

An optional STFT **spectral noise reduction** stage (`airband-dsp::Denoise`,
decision-directed Wiener gain, 256-pt frames at 50% overlap) sits between the
notch and the AGC. It learns the per-bin noise floor only while the channel is
below the speech-present threshold, so broadband hiss under the voice is
attenuated without warbling the speech. With the DFN-centric defaults it is now
**off by default** (`--denoise` to enable, `--denoise-floor-db` to bound the cut).

Two squelch strategies exist.

**VOX (default).** The FPGA DC-blocks the audio (§6.2) before the host sees it, so
the squelch acts on voice energy. To keep that from chattering on continuous
speech (AWOS/ATIS) it uses a **hang time** (`--squelch-hang-ms`, default 1000 ms)
that rides over the pauses between words/phrases, and RTLSDR-Airband's
carrier-loss fast-close (`low_signal_abort`) is **disabled by default** (without a
carrier on the link it would fire on every speech gap).

**Carrier power (`--squelch carrier`).** With a bitstream that ships the
per-channel carrier byte (§6.2), the squelch gates on carrier level instead. Each
channel keeps its **own adaptive carrier-noise floor** (`airband-dsp::CarrierFloor`):
an EWMA of that channel's carrier sampled **only while its squelch is shut**, so
the floor follows the conducted comb's per-channel, diurnal drift up and down but
never learns a transmission into itself. The open threshold is
`floor × 10^(--squelch-snr/20)` — a fixed dB margin above *each channel's own*
noise, so one `--squelch-snr` fits a quiet channel and a comb-hot one alike (the
old single shared threshold could not separate a quiet channel's weak keyings from
a comb-hot channel's noise, since both sat the same distance from one absolute
line). Each floor is **seeded once** from a robust cross-channel noise reference —
the **median** (50th percentile) of the live per-channel carrier levels, recomputed
every 8192 frames (`airband-dsp::carrier_noise_threshold`); that median is also the
`dB·c` meter's reference. A continuous carrier (e.g. ATIS) opens on the seed and
then freezes its floor while open, so it is **not** learned away. The carrier byte
is decoded with `airband-dsp::decode_carrier`; channels start shut until their
floor is seeded. In carrier mode the audio-energy VOX is retained internally only to drive
the speech-present flag that gates AGC gain and denoise noise-learning (the
carrier alone can't distinguish speech from inter-word silence within an open
transmission).

`host/airband-reader` connects to `:30000`, demuxes records by the channel byte,
and uses the per-channel sequence counter to count dropped samples. Defaults are
**DFN-centric** (matching `airband-listen`): squelch `auto` (`--squelch-snr 9` dB),
AGC + DeepFilterNet + presence on, with band-pass/LPF/notch/spectral-denoise off
(enable with `--filter`/`--lpf-hz`/`--notch`/`--denoise`). Modes: `stats` (live
link health + audio peak dBFS + carrier dB·c + transmission counts, default — the
carrier `dB·c` meter mirrors `airband-listen`: each channel's FPGA carrier level
over the cross-channel median, so signal presence shows even when the demod audio
is buried), `wav`, and `raw`.
Recording defaults to **split-on-transmission** (one timestamped file per keyed
transmission, gated by the squelch — no dead air); `--no-split` writes one
continuous file. With `--no-agc`, fixed-gain output is scaled to 16-bit via
`--shift` (positive = attenuate, negative = makeup gain; airband AM is quiet).
Independently of the recording mode it can also stream live: **Icecast** MP3 (LAME
source client, classic `SOURCE` protocol, 16 kbps mono 22050 Hz for LiveATC),
**UDP** s16 PCM, and a **Prometheus** `/metrics` endpoint. The Icecast feeder
takes a JSON **feeds file** (`--feeds`, strict JSON — no comments) that maps any
number of channels to one or more servers (a channel may fan out to several
servers), with per-feed
`name`/`genre`/`description`, `bitrate`/`samplerate`, and `tls`
(`disabled`/`transport`/`upgrade`/`auto`/`auto_no_plain`, plus a testing-only
`tls_insecure`); the handshake response is checked so a rejected source
reconnects instead of going silently quiet. A single `--icecast-*` flag set is
the one-channel shortcut.

`airband-reader` runs the **same full enhancement chain as `airband-listen`** —
VOX squelch (voice-band) → band-pass/LPF/notch/denoise → AGC → **DeepFilterNet**
→ **presence brightness** → soft clip — on **every** streamed channel, so all
Icecast/UDP/WAV outputs carry identical enhanced audio. To stay real-time across
18 channels on the Raspberry Pi 5 (4× Cortex-A76) it uses a **router + per-channel
worker** model: a single router thread reads the socket, detects drops, maintains
the carrier-mode noise reference, and routes each sample to its channel's worker
over an **unbounded queue** (never blocking the socket, never dropping a sample);
one worker thread per channel runs that channel's DSP + DFN + presence and fans
out to its sinks. DeepFilterNet models load **eagerly at startup** (the router
waits on a barrier for all workers before reading the stream) so no transmission
is missed to a cold model (~10–15 MB RAM per streamed channel, fixed). A global
**DFN concurrency cap** (`--dfn-max-active`, default 3) bounds simultaneous NN
inference, acquired/released **per inference hop** (not per transmission) so a
continuously-keyed channel cannot starve the others — permits free between hops
and rotate among all open channels. Over the cap a hop **waits** (its channel's
queue buffers, adding latency) rather than dropping or bypassing — the constraint
is *always filter, never drop*, with latency as the only overload valve. Built so
per the user's dedicated-Pi requirement; DFN3 runs faster than real time on one
A76, so for realistic airband concurrency the cap rarely engages. `host/airband-listen`
runs the same chain on the played channel (and the squelch on every channel for
activity meters), with `single`/`follow` (scanner)/`mix` monitor modes and live
toggles. It defaults to **audio VOX squelch** (a carrier-level squelch proved fragile
against the conducted comb's per-channel offsets, so detection keys on in-band voice
modulation) while its meter still shows **carrier level in dB over the cross-channel
noise reference** (the demod audio is ~0 until modulation rides up, so audio dBFS is
not a useful weak-signal indicator). A **DeepFilterNet** neural enhancer (`D` key) runs
on the played stream **by default** — the host band-pass/LPF/notch/spectral-denoise
instead start off so DFN cleans up alone. The DFN3 model is embedded and run at its
native 48 kHz via streaming resampling from 20000 sps, **after** the filter+AGC chain
and **only while the squelch is open** (NN inference is expensive); it adds ~tens of
ms of latency and complements the spectral denoiser. DFN is tuned for weak airband
speech via `--dfn-min-snr` (−20 dB mute floor, keeps faint speech), `--dfn-atten-lim`
(15 dB attenuation cap so noise-like consonants are never fully chopped), and
`--dfn-pf-beta`; a post-DFN high-shelf **brightness** boost (`p` key) restores the
upper voice band the denoiser rolls off (the "muffled" symptom). A **live FFT window** (`g`
key) plots a Welch PSD of the active post-DSP audio with a hover crosshair
(frequency/magnitude) and a locked Y axis — a debugging aid for the host filters. The
voice band-pass is on by default but does not remove the RF spur "buzz" (see §7).

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
| `GET /api/airband` | Returns the persisted (pending) config: `center_hz`, `samp_rate`, `rf_bandwidth`, `gain_db`, `agc`, `channels` (`[{freq_hz, label?}]`), `poll_ms`, plus capability/read-only fields `max_channels` (18, = `N_CHANNELS`), `samp_rate_locked` (`true`), `enabled` (receiver active), and `needs_restart` (persisted ≠ running). |
| `PATCH /api/airband` | Merges the provided fields, **validates** (≤ `max_channels` channels, each within `center_hz ± samp_rate/2`, `gain_db ∈ [0, 77]`, `poll_ms ≥ 1`; `samp_rate` cannot be changed), and persists the merged config to the SD-card file (`/mnt/sdcard/airband.json`). Returns the updated config. |
| `POST /api/system/restart` | Restarts the `maia-httpd` service (detached, ~1 s delay) so a freshly saved config is applied. If unavailable, reboot manually. |

The config handlers only read/write the JSON file — they never touch the radio
or FPGA. Because the channelizer NCO words are programmed once at startup
(§6.1), saved changes take effect only after the receiver is restarted (hence
`needs_restart` and the page's "Restart receiver" button). `AppState` keeps the
config loaded at boot (the *running* plan) so the handler can report whether a
restart is pending. Optional per-channel labels are stored in a
`channel_labels` array parallel to `channels_hz`; they are cosmetic (used only
by the web UI) and ignored by the receiver.

## 7. Known limitation: the channel "buzz" is an internal (conducted) spur comb

A persistent audible buzz on several channels was root-caused to an **internally
generated spur comb in the ADC samples** — *not* a DSP/HDL problem and *not* RF
arriving through the antenna. The decisive test (Pluto+, **50 Ω termination on the
RX input, antenna disconnected**): the comb is **still present** — a dense,
band-wide comb of discrete teeth (up to ~40 dB over the floor) across the whole
16 MHz capture. It is therefore generated on the board (power/clock/oscillator
harmonics coupling into the front end). The **120.000 MHz** tooth is the 40 MHz
reference's 3rd harmonic, but it is just one line of the comb; a tooth inside a
25 kHz channel cannot be removed by the per-channel NCO/CIC/FIR/AM chain.

Because the comb is conducted, **antenna-side filtering, an external enclosure, and
a notch on the strong local carrier do not remove it** (all three were tried on
hardware with no effect). The comb **is** amplified by the RX gain (a terminated-
input gain sweep: strongest tooth −28 dBFS @71 dB → −86 dBFS @15 dB; comb-to-floor
~36 dB @71 → ~17 dB @30), A 2026-06-24 test series (power, reference, Fs and LO
shifts; see `firmware/diagnostics/README.md`) pins each wideband tooth: the
**dominant tooth is the ADC sample-clock harmonic** that lands in-band — measured at
**126.000 MHz = 9 × 14 MHz** on the original 14 MHz build (fixed absolute under LO
shift; moves to other n·Fs under Fs shift), and so **relocated to 128.000 MHz =
8 × 16 MHz** on the deployed 16 MHz build, where it deliberately sits in a guard gap
clear of every channel — plus a **125 MHz Gigabit-Ethernet PHY clock** (Pluto+) and
the **120 MHz reference 3rd harmonic**, both fixed *absolute* (Fs-independent), none
on the switcher rail. Input power
(USB/battery/benchtop identical), enclosure shielding, an antenna band-pass, and a
switcher↔bulk-cap bead do **not** help; the effective levers are a clean **external
LNA + modest internal gain** (~12 dB default — the internal gain stage is the dominant
comb/intermod generator, so keep it low, but it must still clear ADC quantization, see
§5), **frequency planning** (keep channels off the fixed lines — the shipped plan
already places them in guard gaps), and an **external clean reference** (removes the
120 MHz line). The
full investigation with plots is in `SPUR-INVESTIGATION.md`; the diagnostic toolkit
is in `firmware/diagnostics/README.md`.

(The CORDIC magnitude detector replaced an earlier alpha-max-beta-min estimator
that added its own ~−30 dBc demod spurs from angle-dependent gain ripple. CORDIC
is the correct, artifact-free detector — but it is not the fix for the spur comb.)

## 8. Development and build stack

Develop on **macOS (Apple Silicon)**, build on **x86-64 Linux** — Vivado is
x86-64 only. Amaranth authoring and cocotb/Icarus simulation run natively on the
Mac; only synthesis + bitstream + firmware assembly run on the server (Dockerized
Vivado **2023.2**, Amaranth **0.5.8**). Flashing is independent of building:
`dfu-util` over USB from the Mac. Full environment, build, and flash procedures
are in **`BUILD.md`**; image contents and addressing invariants in
**`firmware/README.md`**.

### 8.1 Persistent state (ramfs vs jffs2 vs SD)

The rootfs is **ramfs** (`root=/dev/ram0`), reconstituted from the FIT image on
every boot, so anything written to `/etc` or `/root` is **lost on power-off**.
Three partitions on the QSPI flash plus the SD card hold the durable state:

| Store | Device | Survives power cycle | Survives `firmware.dfu` reflash | Holds |
|---|---|---|---|---|
| rootfs | ramfs (from `mtd3` FIT) | no | rewritten | OS, init scripts, built-in defaults |
| u-boot env | `mtd1` | yes | yes | clock cal, USB mode, AD9364 attrs |
| jffs2 NVM | `mtd2` (`/mnt/jffs2`) | yes | **yes** (only `mtd3` is rewritten) | TLS certs, **SSH host key + `authorized_keys`** |
| SD card | `/dev/mmcblk0` (`/mnt/sdcard`, FAT32) | yes | yes | **airband channel plan + gain** (`airband.json`) |

Consequences, both addressed by this design:

- **Airband config** lives on the **SD card** (`/mnt/sdcard/airband.json`), not in
  ramfs. The init script mounts the card (requires `broken-cd` in the `&sdhci0`
  devicetree node — the stock DT enables the controller but declares no
  card-detect, so the kernel never enumerates a card) and points `maia-httpd` at
  it. The built-in fallback is intentionally AWOS-only at 0 dB.
- **SSH host key** would otherwise be regenerated every boot: the dropbear init
  launches with `-R` (create-if-missing) into ramfs `/etc/dropbear`, so the
  fingerprint churned on each power-cycle and key-based auth could not persist.
  The init now restores/generates the host key on jffs2 and seeds
  `/root/.ssh/authorized_keys` from jffs2 before dropbear starts (pubkey auth is
  compiled in; password auth is left enabled and can be disabled later with
  dropbear `-s`/`-g`).

## 9. Remaining work

- **Long-duration soak** for 24/7 antenna-site stability (USB/TCP streaming,
  thermal/power, watchdog/restart, monitoring). The Icecast source client is built
  (§6.4) and **deployed on the tower Pi** (`rf-pi`): the all-channel `feeds.json`
  feeds the local Icecast under the `deploy/airband-feeds.service` systemd unit
  (auto-start + auto-restart), validated end-to-end (18/18 mounts, 0 drops; a TLS
  feed proven). `feeds.json` now carries the live KSEA/KRNT/S50 plan and fans the
  16 LiveATC channels out to `audio-in.liveatc.net:8010` alongside the local
  Icecast; **passwords are `${ENV_VAR}` references** expanded at load, so the file
  holds no secrets — they live in the systemd `EnvironmentFile`
  (`/etc/airband-feeds.env`). The Pi runs a plain **git checkout built on-device**
  (never `rsync`); update with `git pull` + `cargo build --release -p
  airband-reader` + service restart (see README → *Run the feeder as a service*).
  NB: the Pluto serves a **single client** on `:30000` — run exactly one reader
  per device.
- **Reliability, observability & recovery (implemented; soak pending).** The
  pipeline now defends "reliable audio reaches LiveATC" on its own:
  - *Pi (airband-reader):* extended Prometheus `/metrics` plus `/healthz`
    (200 only when the Pluto is reachable, the stream is up, samples flow, and
    every feed is connected) and `/status` JSON. Three derived gauges answer the
    operator's questions — `pluto_reachable` (periodic TCP probe of the Pluto web
    port), `maia_httpd_up`/`airband_link_up` (the `:30000` stream is established),
    `data_flowing` (recent sample) — rolling up into `system_healthy` and
    `liveatc_healthy`. Curated metrics + those two headline tiles publish over
    **MQTT** with Home Assistant auto-discovery and a Last-Will availability topic.
    A **low-latency raw-PCM debug monitor** (`--monitor-port`,
    `GET /listen/<ch>.wav?tap=pre|post`) streams one selectable channel in-process
    (no Icecast/MP3, ~100–200 ms). The systemd unit is `Type=notify` with
    `WatchdogSec` + `sd_notify`, `MemoryMax`/`OOMPolicy`, and an `OnFailure=` alert
    hook (webhook/ntfy).
  - *Pluto (maia-httpd):* the airband `reader_loop` has a **DMA-stall watchdog**
    (fails if the FPGA write pointer freezes) and escalates sustained overflow; a
    failed airband task now **restarts in-process with backoff** (app.rs) instead
    of pending forever, keeping the web UI up; an optional `GET /api/health`
    exposes internal DMA progress/overflow. The init script captures maia-httpd
    logs to a bounded on-device ring (`patch_maia_logging.py`) and a **permanent
    restart-safe supervisor** (`patch_maia_supervisor.py`, intentional-stop flag)
    replaces the old bounded boot respawn.
  What remains is the **fault-injection soak** (kill/stall/network/OOM) on
  hardware to confirm each component auto-recovers with the failure visible in
  logs/metrics and an alert fired, plus field validation against a live LiveATC
  mount.
- **Pluto memory headroom (built; flash + validate pending).** The unused IQ-
  recorder DDR reserve is shrunk from ~384 MiB to 16 MiB
  (`maia-hdl/config.py` + `apply_airband_devicetree.py`, kept in lockstep),
  returning ~368 MiB to Linux (~96 MiB → ~464 MiB usable) to remove the OOM race
  at its source while keeping a small functional recorder, the airband ring, and
  the spectrometer. Requires a coordinated bitstream + DT rebuild flashed as a set
  (BUILD.md); free-RAM + 0-drop soak validation is hardware work.
- **Squelch/AGC:** implemented on the host (`airband-dsp`, §6.4). A coarse
  power-squelch could still move into the FPGA later to cut link bandwidth, but the
  host chain already gates audio and normalizes level.
- **Optional NFM mode** (CORDIC arctan-differentiator) if ever needed.

## 10. Licensing / legal

- maia-hdl: MIT. maia-httpd / maia-wasm: Apache-2.0 or MIT. Contributions upstream
  are welcome and aligned with the Maia SDR roadmap.
- Airband monitoring legality varies by country (legal in US/UK/NL/CH and others;
  illegal without a license in e.g. Germany). The operator is an authorized
  liveatc feeder; confirm local rules for the deployment site.
