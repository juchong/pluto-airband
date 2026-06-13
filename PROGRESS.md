# Progress Log

Running log of work, decisions, and state for the Pluto FPGA airband receiver.
Authoritative spec: `pluto-airband-fpga.md`. Environment details: `DEV-SETUP.md`.

## Status at a glance

| Handoff §7 task | State |
|---|---|
| 1. x86 build server bring-up (bitstream build of unmodified Maia) | **done** (Vivado 2023.2; from-source bitstream built, timing met; base PL usage measured) |
| 2. Mac dev env (Amaranth, cocotb/Icarus, Rust, libiio+dfu-util) | **done** |
| 3. Flash baseline Maia to Pluto | not started |
| 4. Channelizer feasibility (GATE) | **GO** (confirmed vs measured base; 22 ch = 21 core + 1 deferred, fit) |
| 5. AM demod block | **done** (envelope mag + DC-block + audio decimate, chain verified) |
| 6. Single-channel end-to-end | blocked on hardware (needs a Pluto) |
| 7. Multi-channel | **in progress** (one time-mux channelizer lane prototyped + verified) |
| 8. Pi streamer | not started |
| 9. Hardening | not started |

## Done

### Mac dev environment (handoff §5.2 / §7 step 2)
- Apple Silicon, macOS 26.5. Homebrew tools: `git`, `python@3.12`,
  `icarus-verilog`, `yosys`, `dfu-util`. Rust via rustup (`~/.cargo`).
- Python `.venv` (3.12) with pinned deps — see `requirements-dev.txt` /
  `requirements-dev.lock.txt`.
- Upstream clones pinned by SHA (see `DEV-SETUP.md`): `maia-sdr` and
  `plutosdr-fw`. `XilinxUnisimLibrary` submodule initialized; `adi-hdl` not.
- **Validated:** `python -m unittest` in `maia-hdl/` → 51 tests OK; full
  `maia-hdl/test_cocotb/` suite → all PASS.
- **libiio 0.25** (tag `b6028fd`) built from source → `~/.local` (plain dylib +
  tools, no sudo). `iio_info --version` OK, backends `xml ip usb`. Pinned in
  `DEV-SETUP.md`. (Add `~/.local/bin` to PATH for convenience.)

### x86 build server + baseline bitstream (handoff §7 step 1)
- Server provisioned (Ubuntu 22.04 x86-64, 32 vCPU). **Rootless Docker** — the
  firmware build container must run as `DOCKER_USER=0:0` (host user maps to
  container root); upstream's `$(id -u):$(id -g)` fails to write the bind mount.
  Setup details in `DEV-SETUP.md`.
- **Vivado/Vitis/Vitis_HLS 2023.2** installed to `/opt/Xilinx` (Zynq-7000 only),
  bound to the `vivado2023_2` docker volume.
- **From-source bitstream of unmodified Maia SDR built end-to-end** (kernel,
  u-boot, buildroot rootfs, Vivado synth+impl→`system_top.bit`, firmware images
  `pluto.frm`/`.dfu`/`.itb`). `HAVE_VIVADO=1` real build, not the XSA fallback.
- **Measured base-platform PL usage** (`system_top_utilization_placed.rpt`,
  timing met, WNS +0.029 ns) on the XC7Z010:

  | Resource | Used | Total | % | Free |
  |---|---|---|---|---|
  | Slice LUTs | 5416 | 17600 | 30.8 | 12184 |
  | Slice Registers (FF) | 6493 | 35200 | 18.5 | 28707 |
  | Block RAM Tile (36k) | 29 | 60 | 48.3 | 31 |
  | DSP48E1 | 18 | 80 | 22.5 | 62 |

  Binding resource for the channelizer is **BRAM36 (31 free)**, then LUT
  (12184 free); DSPs are abundant (62 free). Folded into `hdl/feasibility_25ch.py`
  — all 22 channels still fit on top of the base, even at the full ~19 MHz window
  (≈58% of free BRAM, ≈69% of free LUT).

### Repo
- Local git repo pushed to remote `origin`:
  https://github.com/juchong/pluto-airband.git (`master`).

### DDC exploration (toward §4.2 / §7 steps 5–6)
- `maia_hdl` installed editable as a library; built our own sim harness in
  `hdl/` (we don't modify upstream).
- `hdl/ddc_tune_decimate.py`: drives maia-hdl's `DDC` (NCO mixer + 3-stage FIR
  decimator, stages 2/3 bypassed). Designs/scales/packs stage-1 low-pass coeffs.
  **Validated:** tuning the NCO to a complex tone produces a clean DC (baseband)
  output after decimate-by-8 (~0% ripple); detuning into the stopband gives
  ~63 dB rejection. Confirms understanding of the NCO, FIR coeff memory layout,
  bypass paths, strobe pacing, and 1x/3x clocking.
- Key learnings: DDC exposes no input backpressure (NCO advances only on valid
  strobe_in); coeff memory is per-stage (stage1 @0, stage2 @256, stage3 @512);
  output width 16; defaults in_width=12, nco_width=28, coeff_width=18.

## Decisions made

### Project / dependency strategy
- **Do not fork or modify Maia SDR.** Build on top of it; treat `maia-sdr` and
  `plutosdr-fw` as read-only, SHA-pinned external deps. Use `maia_hdl` as a
  library where practical.
- **This Mac is the development box, not the build server.** Vivado is x86-64
  only; the Maia Docker images are `linux/amd64`-only (verified via GHCR API), so
  synthesis/bitstream/firmware runs on a separate x86-64 Linux host with Docker
  (handoff §5.1). Provisioned — see "x86 build server" above and `DEV-SETUP.md`.
- **Repo:** workspace root is a git repo tracking only our artifacts
  (docs, requirements, future HDL/Pi code), pushed to
  `github.com/juchong/pluto-airband`.

### Toolchain version alignment (why our pins differ from the handoff doc)
Pins follow the upstream `maia-sdr-devel` container (tag `20260304`) rather than
the doc's older numbers, because current `maia-sdr` `main` requires them:
- **Amaranth `0.5.8`** (doc said 0.5.2) — 0.5.2 emits the obsolete `read_ilang`
  yosys command and fails with modern yosys.
- **cocotb `2.0.1`** + `cocotb-bus` — tests use the cocotb 2.0 `unit=` API.
- **numpy `1.26.4`** (`<2`) — numpy 2's NEP-50 integer rules break `test_packer`.
- **`AMARANTH_USE_YOSYS=builtin`** — use the bundled `amaranth-yosys`, not the
  newer/incompatible Homebrew `yosys`. Set automatically by the venv `activate`.

## Open decisions (from handoff §8 — still to resolve, do not guess)
1. ~~Channel count target N~~ — **RESOLVED: N = 22** (final list: 21 need-to-have
   core channels + 1 deferred nice-to-have outlier at 133.65 MHz).
2. ~~Capture window center + width~~ — **RESOLVED (core list): center (LO)
   123.438 MHz, Fs ≈ 14 MHz** (see `hdl/capture_window.py`). The 21 core channels
   span 118.05–128.5 MHz; centering in the 122.975–123.9 guard gap puts the DC/LO
   spur 463 kHz from the nearest channel and keeps every channel inside the central
   ~80% (extreme ±5.39 MHz = 77% of the ±7 MHz half-band, ≥1.6 MHz edge guard).
   Costs ~5 time-mux lanes (≤8 the Z-7010 fits). 133.65 MHz is a **nice-to-have**,
   deferred (would force Fs≈20 MHz / 8 lanes / center 125.75 MHz); it can be added
   later as a separate decision without disturbing the core.
3. Audio rate: 8 ksps vs 16 ksps.
4. Squelch/AGC placement: FPGA vs Pi (default: Pi first).
5. Front-end filtering: airband BPF + broadcast-FM notch (hygiene).
6. liveatc specifics: server, mountpoint convention, codec/bitrate.

> Resolved by the handoff doc (§2.4): Pluto RF capability — no hardware-capability
> gating. Only feasibility gate is FPGA resource fit (§4.2).

### AM demod block (§7 step 5, done)
- `hdl/am_demod.py`: `EnvelopeMagnitude` — multiplier-free alpha-max-beta-min
  `|z| ~= max(|I|,|Q|) + 3/8*min(|I|,|Q|)` (no DSP48). 2-cycle pipeline.
  **Validated:** exact match to integer model; approx error vs true magnitude in
  [-2.77%, +6.80%]; numpy demo recovers a 1 kHz AM tone after DC block.
- `hdl/am_audio.py`: AM back-end + full single-channel chain.
  - `DCBlock` — one-pole high-pass (leaky-integrator DC estimate, subtract);
    multiplier-free (1 shift + 2 adds). Strips the carrier-amplitude DC before
    decimation so it doesn't inflate CIC word growth.
  - `CICDecimator` — multiplier-free N-stage CIC decimator (no DSP48, no coeff
    memory) to the audio rate; integrator/comb cascades evaluated combinationally
    so HW matches the cumsum/diff model bit-exactly.
  - `AMChannel` — wires `DDC -> EnvelopeMagnitude -> DCBlock -> CICDecimator`;
    stateful blocks advance on validated strobes derived from `ddc.strobe_out`
    (note: a wrapper exposing the DDC's multi-rate `common_edge` is required or
    the decimator emits only one sample).
  - **Validated:** DCBlock/CIC match exact integer models; DC block drives a
    large input bias to ~0; end-to-end run tunes a frequency-offset AM tone to
    baseband, demodulates, DC-blocks, and decimates to a clean audio tone at the
    expected frequency (plot `hdl/out/am_audio.png`).
- Audio rate (8 vs 16 ksps) left open (§8.3): `audio_decim`/`cic_stages` are
  parameters; nothing hard-codes the choice.

### Time-multiplexed channelizer lane (§7 step 7, in progress)
- `hdl/channelizer_lane.py`: `TdmDdcLane` — one physical NCO + complex-mixer + CIC
  datapath **shared across N channels**, with per-channel state (NCO phase, CIC
  integrator/comb regs, decim counter) in channel-indexed arrays. One wideband IQ
  sample is broadcast to all channels; the lane sweeps channels one-per-cycle.
  This is the concrete realization of the time-multiplexing the feasibility GATE
  assumed.
- **Validated:** HW is **bit-exact** to a Python reference (shared sine ROM,
  fixed-point complex mixer, integer CIC) across all channels; a 4-channel/4-tone
  demo shows each channel tuning its own tone to baseband through the single
  datapath (I≈DC, Q≈0, ripple <1%), rejecting the others. Plot
  `hdl/out/channelizer_lane.png`.
- **Resources** (`hdl/synth_estimate.py`, Yosys xc7): shared complex mixer = 4
  DSP48E1 regardless of channel count (one shared multiplier; maia-hdl `Cmult3x`
  trims to ~1). Even at 4 DSP/lane the budget holds (8 lanes×4 = 32 < 62 free DSP).
  Per-channel CIC/NCO state shows up as FF/LUT in the prototype (register arrays);
  in the real design it maps to **BRAM** — matching the model's BRAM-for-state.
- Not yet built: shared front-end decimator (window → intermediate rate) and the
  per-lane cleanup/compensation FIR (CIC passband-droop correction).

### Channelizer feasibility GATE (§4.2) — GO for N=22
- `hdl/synth_estimate.py`: emits Verilog for the real maia-hdl `DDC` and our AM
  back-end, runs Yosys `synth_xilinx -family xc7`. **Measured:** full DDC = 11
  DSP48E1 (Cmult3x mixer 1 + 3-stage FIR 4+2+4=10), ~661 LUT, ~1239 FF, ~4 BRAM36;
  AM back-end = **0 DSP** (multiplier-free), ~295 LUT, ~281 FF/ch.
- `hdl/feasibility_25ch.py`: time-multiplexing resource model vs the XC7Z010
  budget (17.6k LUT / 35.2k FF / 80 DSP48E1 / 60 BRAM36).
  - Naive 22× parallel DDCs = 242 DSP / ~88 BRAM → INFEASIBLE.
  - Time-multiplexed shared channelizer (shared CIC front-end + lanes =
    ceil(N*W/62.5MHz), DSP-free back-end shared at audio rate): **fits at every
    window**.
- **Base-platform usage now measured** (build server step 1) and folded in: the
  model checks the channelizer against the FREE budget (Z7010 − base). Still GO —
  full ~19 MHz airband ≈ 7 lanes / 21 DSP / 69% of free LUT / 58% of free BRAM;
  the resolved 14 MHz core window ≈ 5 lanes (56% LUT / 45% BRAM). Binding
  resources: BRAM36 then LUT. LUT/FF lane costs remain Yosys-scale estimates to
  re-confirm in Vivado once a full lane is built.

## Next steps
- §8.2 capture window **resolved** (final list): center 123.438 MHz, Fs ≈ 14 MHz,
  ~5 lanes for the 21 core channels (`hdl/capture_window.py`); 133.65 MHz deferred.
- Extend the channelizer lane: add the shared front-end decimator and a per-lane
  cleanup/compensation FIR; then re-measure the lane in Vivado on the build server
  (real LUT/FF/DSP/BRAM, with per-channel state in BRAM).
- Define §4.3 per-channel DMA framing (channel index + sample counter) once the
  multi-channel datapath shape is fixed.
- (Blocked on hardware) §7 step 3/6: flash baseline Maia (`build/pluto.dfu`) and
  bring up one real channel end-to-end on a Pluto.
