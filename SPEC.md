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
| Pi-side daemon encodes each channel and pushes to liveatc Icecast | **Done — `airband-reader` has a built-in LAME→Icecast source client (16 kbps mono 22050 Hz); soak pending** |

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

### 2.4 Hardware variant: Pluto+
The receiver also targets the **Pluto+** (the open `plutoplus/plutoplus` board).
It is the **same XC7Z010 die** as the ADALM-Pluto — a different package
(`xc7z010clg400-1`) with a different MIO pinout, but identical FPGA resources, so
the 21-channel design fits unchanged (no resource re-fit; build with
`TARGET=plutoplus`, see `BUILD.md`). The Pluto+ adds three things this project
cares about:

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
- **Gain:** one shared RX gain serves all 21 channels (no per-channel *RF* AGC;
  per-channel audio AGC is done on the host, §6.4); fixed **manual gain** (the
  AD9361 AGC modes settle on wideband power and starve weak channels). The shipped
  default is **12 dB**, tuned for an external LNA, and the choice is set by two
  measured facts:
  - **The receiver is internal-noise-limited, not antenna/thermal-limited.** The
    channel-11 idle audio floor is identical with the antenna or a **50 Ω termination**
    (−92.9 vs −93.3 dBFS, Δ0.4 dB) and rises only ~1 dB per +6 dB of gain. So the
    audio floor is **ADC quantization + the conducted comb (§7)**, downstream of the
    gain — a quieter antenna environment cannot lower it.
  - **At 0 dB the wanted signal sits at that floor.** A controlled gain sweep on the
    continuous 118.050 AWOS carrier (carrier-over-noise, same signal throughout)
    measured audio SNR **~1 dB at 0 dB → ~10 dB at 6 dB → ~12 dB at 12 dB**, plateauing
    through 42 dB. ~12 dB is therefore the minimum internal gain that lifts voice clear
    of quantization; with the external LNA the wideband ADC does not clip (0 %, ~7 dB
    headroom).

  The external LNA still matters — a clean low-NF stage ahead of the Pluto sets the
  system noise figure (Friis) and lets the AD9361's internal gain stage (the dominant
  comb/intermod generator, §7) run lower — but it does **not** substitute for the
  ~12 dB internal gain needed to clear quantization. Because the limit is internal
  dynamic range, the largest further gains come from a **front-end SAW airband
  band-pass + low-NF LNA** (so strong out-of-band signals don't starve the 12-bit
  ADC), not a quieter site. On a *bare* front end (no LNA) raise toward the **48 dB**
  clipping knee (a sweep, `firmware/diagnostics/floor_sweep.py`, shows the **71 dB**
  near-ceiling clips ~13–15 % of the wideband ADC). Higher RX gain does raise the
  conducted comb (§7), but it stays in the channel-plan guard gaps.

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

RX is unaffected (waterfall, per-channel meters, and the 123.438 MHz RX LO all
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

The Pluto's 40 MHz reference is an uncalibrated oscillator (the ADALM-Pluto's bare
XO; the Pluto+ has a 0.5 ppm VCTCXO that drifts far less). Its ppm error shifts
every tuned frequency proportionally, so it is corrected once per unit via the
u-boot env var **`ad936x_ext_refclk_override`**: the `adi_loadvals` boot script does
`fdt set /clocks/clock@0 clock-frequency <value>`, so the AD9361 driver computes its
PLL/decimation from the *true* reference and the nominal 123.438 MHz LO / 14 MHz Fs
are hit exactly (all 21 channels corrected together; the channelizer NCO math is
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
On boot with `--airband`, `maia-httpd` reads `/root/airband.json` (or built-in
defaults) and:
1. Sets the AD9361 **sampling frequency** (14 MHz), **RX RF bandwidth** (= Fs),
   and **RX LO** (123.438 MHz).
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
bits [39:32] channel index (0..20)
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
(2nd-order Butterworth, default −3 dB at 2.5 kHz, `--lpf-hz`; airband AM voice has
no usable energy above ~3.4 kHz so a tighter corner trims hiss) → optional
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
per-channel carrier byte (§6.2), the squelch gates on carrier level instead. A
per-channel adaptive-SNR floor is *not* usable here: it can't tell a continuous
AWOS carrier from a high steady noise floor, so it learns the carrier away and
closes. Instead the host derives a **shared, fixed threshold from a cross-channel
noise estimate** — all channels of one receiver see the same wideband noise, so
the **median** (50th percentile) of the live per-channel carrier levels is the
noise reference and a station is a large outlier above it (the median matches
`airband-listen`: at useful gain the conducted comb elevates several channels,
which would inflate a high percentile and push the threshold above real traffic).
The threshold is `median × 10^(--squelch-snr/20)`, recomputed every 8192 frames
(`airband-dsp::carrier_noise_threshold`) and applied to every channel via
`Squelch::set_threshold`. Because it tracks the *other* channels' noise rather
than a channel's own level, it holds a continuous carrier open (no hang, no
chatter) while empty channels stay shut. The frame's carrier byte is decoded with
`airband-dsp::decode_carrier`; channels start shut until the first cross-channel
update. In carrier mode the audio-energy VOX is retained internally only to drive
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
21 channels on the Raspberry Pi 5 (4× Cortex-A76) it uses a **router + per-channel
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
native 48 kHz via streaming resampling from 21875 sps, **after** the filter+AGC chain
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

## 7. Known limitation: the channel "buzz" is an internal (conducted) spur comb

A persistent audible buzz on several channels was root-caused to an **internally
generated spur comb in the ADC samples** — *not* a DSP/HDL problem and *not* RF
arriving through the antenna. The decisive test (Pluto+, **50 Ω termination on the
RX input, antenna disconnected**): the comb is **still present** — a dense,
band-wide comb of discrete teeth (up to ~40 dB over the floor) across the whole
14 MHz capture. It is therefore generated on the board (power/clock/oscillator
harmonics coupling into the front end). The **120.000 MHz** tooth is the 40 MHz
reference's 3rd harmonic, but it is just one line of the comb; a tooth inside a
25 kHz channel cannot be removed by the per-channel NCO/CIC/FIR/AM chain.

Because the comb is conducted, **antenna-side filtering, an external enclosure, and
a notch on the strong local carrier do not remove it** (all three were tried on
hardware with no effect). The comb **is** amplified by the RX gain (a terminated-
input gain sweep: strongest tooth −28 dBFS @71 dB → −86 dBFS @15 dB; comb-to-floor
~36 dB @71 → ~17 dB @30), A 2026-06-24 test series (power, reference, Fs and LO
shifts; see `firmware/diagnostics/README.md`) pins each wideband tooth: the
**dominant (126.000 MHz) is the 9th harmonic of the 14 MHz ADC sample clock** (fixed
absolute under LO shift; moves to other n·Fs under Fs shift), plus a **125 MHz
Gigabit-Ethernet PHY clock** (Pluto+) and the **120 MHz reference 3rd harmonic** —
all at fixed *absolute* frequencies, none on the switcher rail. Input power
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

## 9. Remaining work

- **Long-duration soak** for 24/7 antenna-site stability (USB/TCP streaming,
  thermal/power, watchdog/restart, monitoring). The Icecast source client is built
  (§6.4) and **deployed on the tower Pi** (`rf-pi`): the all-channel `feeds.json`
  feeds the local Icecast under the `deploy/airband-feeds.service` systemd unit
  (auto-start + auto-restart), validated end-to-end (21/21 mounts, 0 drops; a TLS
  feed proven). `feeds.json` now carries the live KSEA/KRNT/S50 plan and fans the
  16 LiveATC channels out to `audio-in.liveatc.net:8010` alongside the local
  Icecast; **passwords are `${ENV_VAR}` references** expanded at load, so the file
  holds no secrets — they live in the systemd `EnvironmentFile`
  (`/etc/airband-feeds.env`). The Pi runs a plain **git checkout built on-device**
  (never `rsync`); update with `git pull` + `cargo build --release -p
  airband-reader` + service restart (see README → *Run the feeder as a service*).
  What remains is unattended hardening (per-feed supervision/alerting)
  and field validation against a live LiveATC mount. NB: the Pluto serves a
  **single client** on `:30000` — run exactly one reader per device.
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
