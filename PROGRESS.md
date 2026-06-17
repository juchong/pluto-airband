# Progress Log

Running log of work, decisions, and state for the Pluto FPGA airband receiver.
Authoritative spec: `SPEC.md`. Build/flash/ops: `BUILD.md`. Hub: `README.md`.
Older log entries cite "Handoff §N / §7 step N" — the project's *original* spec/task
numbering. `SPEC.md` has since been rewritten as an as-built spec with a different
structure; for current design sections and remaining work see `SPEC.md` (§9 lists
what is left).

## Status at a glance

| Milestone (original task sequence) | State |
|---|---|
| 1. x86 build server bring-up (bitstream build of unmodified Maia) | **done** (Vivado 2023.2; from-source bitstream built, timing met; base PL usage measured) |
| 2. Mac dev env (Amaranth, cocotb/Icarus, Rust, libiio+dfu-util) | **done** |
| 3. Flash baseline Maia to Pluto | **done** (baseline verified, then airband image flashed) |
| 4. Channelizer feasibility (GATE) | **GO** (confirmed vs measured base; 22 ch = 21 core + 1 deferred, fit) |
| 5. AM demod block | **done** (envelope mag + DC-block + audio decimate, chain verified) |
| 6. Single-channel end-to-end | **done** (verified on hardware as part of the 21-ch stream) |
| 7. Multi-channel | **done** — 21 channels live on hardware, gap-free TCP stream, auto-start |
| 8. Pi streamer | host reader done (`host/airband-reader`); LiveATC feeder integration pending |
| 9. Hardening | not started |

## Done

### Mac dev environment (handoff §5.2 / §7 step 2)
- Apple Silicon, macOS 26.5. Homebrew tools: `git`, `python@3.12`,
  `icarus-verilog`, `yosys`, `dfu-util`. Rust via rustup (`~/.cargo`).
- Python `.venv` (3.12) with pinned deps — see `requirements-dev.txt` /
  `requirements-dev.lock.txt`.
- Upstream clones pinned by SHA (see `BUILD.md`): `maia-sdr` and
  `plutosdr-fw`. `XilinxUnisimLibrary` submodule initialized; `adi-hdl` not.
- **Validated:** `python -m unittest` in `maia-hdl/` → 51 tests OK; full
  `maia-hdl/test_cocotb/` suite → all PASS.
- **libiio 0.25** (tag `b6028fd`) built from source → `~/.local` (plain dylib +
  tools, no sudo). `iio_info --version` OK, backends `xml ip usb`. Pinned in
  `BUILD.md`. (Add `~/.local/bin` to PATH for convenience.)

### x86 build server + baseline bitstream (handoff §7 step 1)
- Server provisioned (Ubuntu 22.04 x86-64, 32 vCPU). **Rootless Docker** — the
  firmware build container must run as `DOCKER_USER=0:0` (host user maps to
  container root); upstream's `$(id -u):$(id -g)` fails to write the bind mount.
  Setup details in `BUILD.md`.
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
  (handoff §5.1). Provisioned — see "x86 build server" above and `BUILD.md`.
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
3. ~~Audio rate: 8 ksps vs 16 ksps~~ — **RESOLVED: 15625 sps** (`audio_decim=7`,
   14 MHz / 128 / 7).
4. Squelch/AGC placement: FPGA vs Pi (default: Pi first) — still open (currently
   neither: fixed manual gain, no squelch).
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
### Channelizer front-end + cleanup FIR (§7 step 7, prototyped + verified)
- `hdl/channelizer_chain.py`: the two filtering stages the lane prototype deferred,
  built on one generic integer block `FIRStage` (direct-form decimating FIR).
  - `FIRStage` — **bit-exact** to its Python model at decimation 1 and 4.
  - `FrontEndDecimator` — **shared** complex FIR low-pass decimator (one per
    receiver). The AD936x is run oversampled and this one block decimates the whole
    capture to the working rate with a *flat* passband (a CIC would droop the band
    edges / outer channels). **Validated:** 0.01 dB ripple across the channel
    region, 57 dB rejection beyond the window.
  - `CompensationFIR` (a `FIRStage`) — per-channel FIR that inverts the per-channel
    CIC passband droop *and* provides the sharp channel selectivity the CIC's gentle
    roll-off cannot. **Validated:** CIC droop 2.17 dB → 0.39 dB flat, 88 dB
    adjacent-channel rejection (plot `hdl/out/channelizer_chain.png`).
  - **End-to-end HW** (front-end → NCO mix → per-channel CIC → comp FIR): an
    on-channel tone passes; one channel-spacing away is rejected by **48 dB**.
- **Resources** (`hdl/synth_estimate.py`): the prototype FIRs are fully unrolled
  (1 mult/tap) so Yosys shows the upper bound (front-end 190 DSP, comp 71 DSP). The
  real blocks **fold** taps onto a MAC engine: the single long front-end FIR is
  ~43 DSP at 14 MHz → must be a **multistage** HBF/CIC+FIR decimator (a handful of
  DSP, shared once); the channel selectivity FIR at ~50 kHz is ~0.2 DSP/channel →
  ~4 DSP covers all 21 on one engine. Folded into `feasibility_25ch.py` (front-end
  optional, per-lane cleanup FIR); still GO.
- **Multistage front end + folded cleanup FIR built and verified** (extends
  `channelizer_chain.py`):
  - `MultiStageDecimator` — two halfband decimate-by-2 stages (11+31 taps, 7+17
    nonzero). **Bit-exact** to the cascaded model; 0.08 dB channel-region ripple,
    53 dB out-of-window rejection. Folds to **~14 DSP** (vs ~43 for one long FIR).
    NB it is **optional**: the AD936x decimates internally (HB1/2/3 + prog. FIR) to
    the requested rate, so the baseline captures at the working rate with no PL
    front end; this block is the oversampling-fallback realization.
  - `TdmFirEngine` — folded one-MAC cleanup FIR: a single multiply-accumulate
    iterated over taps serves all channels (per-channel delay lines indexed by
    channel). **Bit-exact** to the per-channel parallel FIR. Yosys: **2 DSP**.
- **Vivado 2023.2 OOC cross-check** (build server, `xc7z010clg225-1`, the real
  Pluto part — see `BUILD.md`):
  - `TdmDdcLane` (21 ch): **4 DSP, 3374 LUT (19%), 7760 FF (22%), 0 BRAM** — closely
    matches Yosys (4 DSP / 3583 LUT / 7708 FF), de-risking the LUT/FF estimates. The
    per-channel state lands in FFs (register file); a Memory-backed lane moves it to
    BRAM.
  - `MultiStageDecimator` (parallel build): 58 DSP / 212 LUT / 1055 FF — the
    parallel structural cost (1 mult/nonzero-tap); the folded MAC version is ~14 DSP.

### Integrated channelizer core + Vivado place (§7 step 8)
- `hdl/channelizer_core.py`: `ChannelizerCore` unifies the verified blocks into one
  top — **BRAM-backed TDM DDC lane → burst-absorbing FIFO → folded complex cleanup
  FIR** (I+Q `TdmFirEngineBRAM` in lockstep). All channels share one decimation
  cadence, so each CIC boundary emits a burst of N outputs; the `SyncFIFO` buffers it
  and the cleanup FIR drains at its own (low) rate. **Bit-exact**: HW == lane model →
  per-channel FIR model. End-to-end demo: each tuned tone → clean baseband (<0.1 %
  ripple), neighbours rejected.
- **Memory-backed per-channel state** (the §7-step-8 goal):
  - `TdmDdcLaneBRAM` (`channelizer_lane.py`): per-channel NCO/CIC state moved from a
    fan-out register file into `amaranth.lib.memory.Memory`, and the datapath
    **pipelined READ→MIX→INTEG→COMB** (one channel/clock) so it closes 62.5 MHz.
  - `TdmFirEngineBRAM` (`channelizer_chain.py`): cleanup-FIR delay lines as a
    per-channel **circular buffer in block RAM** (no FF shift register). Both are
    bit-exact to their register-based parents.
- **Vivado 2023.2 synth + place + route** of `ChannelizerCore` (one deployment lane:
  5 ch, dec-64 CIC, complex 119-tap cleanup), `xc7z010clg225-1`, 62.5 MHz (16 ns):

  | Resource | Used | % of XC7Z010 | vs free (Z7010−base) |
  |---|---|---|---|
  | Slice LUTs | 1309 | 7.4 % | 11 % of 12184 free |
  | Slice Registers (FF) | 1577 | 4.5 % | 5.5 % of 28707 free |
  | Block RAM Tile | 3 | 5.0 % | ~10 % of 31 free |
  | DSP48E1 | 8 | 10 % | 13 % of 62 free |

  **Timing MET: WNS +3.07 ns, 0 failing endpoints** at 62.5 MHz (route done, 0
  errors). The first place attempt (single-cycle compute) failed FF over-utilization
  (38 980 FF — FIR delay lines mapped to fabric) and timing (WNS −3.28 ns, the 6-deep
  CIC adder cascade); moving the delay lines to BRAM dropped FF to ~0.9 k and
  pipelining the lane closed timing (+3.07 ns) for +650 FF. ~5 such lanes cover the
  21 core channels and still leave large margin. Flow + reports: `hdl/ooc_place.tcl`,
  `BUILD.md`.

### Channelizer feasibility GATE (§4.2) — GO for N=22
- `hdl/synth_estimate.py`: emits Verilog for the real maia-hdl `DDC` and our AM
  back-end, runs Yosys `synth_xilinx -family xc7`. **Measured:** full DDC = 11
  DSP48E1 (Cmult3x mixer 1 + 3-stage FIR 4+2+4=10), ~661 LUT, ~1239 FF, ~4 BRAM36;
  AM back-end = **0 DSP** (multiplier-free), ~295 LUT, ~281 FF/ch.
- `hdl/feasibility_25ch.py`: time-multiplexing resource model vs the XC7Z010
  budget (17.6k LUT / 35.2k FF / 80 DSP48E1 / 60 BRAM36).
  - Naive 22× parallel DDCs = 242 DSP / ~88 BRAM → INFEASIBLE.
  - Time-multiplexed shared channelizer (shared flat front-end decimator + lanes =
    ceil(N*W/62.5MHz), DSP-free back-end shared at audio rate): **fits at every
    window**.
- **Base-platform usage now measured** (build server step 1) and folded in: the
  model checks the channelizer against the FREE budget (Z7010 − base). Still GO —
  full ~19 MHz airband ≈ 7 lanes / 21 DSP / 69% of free LUT / 58% of free BRAM;
  the resolved 14 MHz core window ≈ 5 lanes (56% LUT / 45% BRAM). Binding
  resources: BRAM36 then LUT. The 21-ch lane LUT/FF are now **Vivado-confirmed** (OOC,
  see above); the full integrated channelizer is the remaining Vivado place step.

### Full receiver datapath assembled (§7 step 5 + 7, verified)
- `hdl/am_backend_tdm.py`: **`TdmAmBackend`** — the AM demod (`|I+jQ|` →
  one-pole DC block → CIC audio decimate) **folded over channels**, per-channel
  DC-block/CIC state in `amaranth.lib.memory`. **Bit-exact** to the per-channel
  `EnvelopeMagnitude → DCBlock → CICDecimator` models; DSP-free; recovers a real
  AM tone. (§8.3 audio rate stays a parameter — `audio_decim`/`cic_stages`.)
- `hdl/audio_framer.py`: **`AudioFramer`** — §4.3 framing **resolved**: each audio
  sample → fixed 8-byte record `{seq[24] | chan[8] | sample[32]}`, drained over an
  AXI4-Stream (`stream_data/valid/ready`) that matches `maia_hdl.dma.DmaStreamWrite`
  (`width=64`). Per-channel sequence counter for demux + drop detection. Verified.
- `hdl/receiver_top.py`: **`ReceiverTop`** — wideband IQ → N `ChannelizerCore`
  lanes (balanced, e.g. 21 → `[5,4,4,4,4]`) → round-robin collector → `TdmAmBackend`
  → `AudioFramer` → DMA stream, with a flat per-channel NCO register interface.
  **Bit-exact** end-to-end (framed audio == lane→FIR→AM models, per-channel seq
  monotonic); 6 ch/3 lanes simulated, 21 ch/5 lanes elaborates.

### Real-time throughput budget (§4.2, cycle level) — closed
- `hdl/realtime_budget.py`: the area GATE didn't check whether the *folded*
  datapaths keep up with the sample cadence. At Fpl=62.5/Fs=14 MHz there are only
  ~4.46 cycles/input, so: **chans_per_lane ≤ 4**; the cleanup FIR needs enough lane
  decimation for its tap count (`duty_fir = cpl·(Fs/decim)·ntaps/Fpl < 1`).
- **Finding:** the OOC config (dec-64, 119-tap) does **not** close real-time timing
  (`duty_fir ≈ 1.75`) — it was a *resource* measurement. **Deployment config:
  chans_per_lane=4, lane_decim=128, ntaps=63 → 6 lanes, audio_decim=7 → 15.6 ksps.**
- **Validated** with a cycle-accurate stress test at the true cadence: the fitting
  config is overflow-free + bit-exact; an over-budget config correctly overflows
  (detector now covers the lane→FIR FIFO, collector FIFOs, and framer FIFO).

### maia-hdl splice — DONE (HDL level, elaborates + SVD + ports verified)
Work is on the `pluto-airband` branch of the **fork** (`maia-sdr` origin →
`github.com/juchong/maia-sdr`, upstream renamed `upstream`). Edits are a thin,
documented integration shim:
- **Vendored DSP**: `hdl/*.py` copied verbatim into `maia-hdl/maia_hdl/airband/`
  (9 modules); `maia_sdr.py` adds that dir to `sys.path` and imports
  `ReceiverTop` by flat name, so the verified sources drop in unchanged. (The
  Verilog-gen step therefore needs **numpy + scipy** in the build env — see
  build step.)
- **`MaiaSDR.__init__`**: instantiates `ReceiverTop` at the deployment config
  (21 ch, chans_per_lane=4, lane_decim=128, 63-tap cleanup FIR → 6 lanes,
  audio_decim=7, 24-bit samples). The cleanup-FIR coeffs are **precomputed**
  (`design_cic_compensation(128,3,63,0.22,0.46)`, out_shift=17) and embedded as
  a constant so the build needs no scipy at construction time. Also instantiates
  `m_axi_airband` `DmaStreamWrite(width=64)` and a new `airband` register bank.
- **`airband` register bank** (sync domain, own `RegisterCDC`, mapped at byte
  `0x40`; `axi4_awidth` 4→5, decode on `address[4]` — fully backward-compatible,
  existing control/recorder/sdr offsets and decode unchanged):
  `airband_control` {dma_start[Wpulse], dma_stop[Wpulse], enable[RW],
  overflow[R sticky]}, `airband_freq_addr` {freq_waddr}, `airband_freq`
  {freq_wren[Wpulse], freq_wdata[24]}, `airband_dma_next_address` {next_address}.
- **`elaborate`**: wires post-CDC RX IQ (`rxiq_cdc.re_out/im_out`, gated by
  `enable` on `strobe_out`) into the receiver; routes NCO writes; streams framed
  audio into the airband DMA; latches a sticky overflow; exposes `next_address`.
- **`system_bd.tcl`**: `m_axi_airband` → **HP0** (free; HP1=spectrometer,
  HP2=recorder), HP0 ACLK = `clk_out1` (sync, 62.5 MHz), with a full-DDR address
  segment. `config.py` adds `airband_address_range = (0x1f00_0000, 0x2000_0000)`
  (top-of-DDR ring; **must be reconciled with the reserved-memory devicetree +
  maia-kmod before hardware use**).
- **Verified**: `python -m maia_hdl.maia_sdr --config default` elaborates clean
  (96.9k-line Verilog), `m_axi_airband_*` AXI3 ports emitted, SVD exposes the 4
  airband registers, `test/test_register.py` passes. `ReceiverTop` itself stays
  bit-exact (verified separately).

### Full-design bitstream build #1 (server, `projects/pluto`, default config)
Built on `xilinx-builder` from the fork's `pluto-airband` branch (fresh clone +
adi-hdl `065c8f1`/XilinxUnisim submodules, venv amaranth 0.5.8 / numpy 1.26.4 /
scipy 1.15.3, Vivado 2023.2). IP packaging (both configs) + `axi_ad9361` + the
pluto project all ran; **synth + place + route completed** (design fits the
Z-7010), but **timing failed**:
- **Utilization (whole design):** LUT **16279/17600 = 92.5%** (logic 72.4%,
  mem 59%), FF 17422/35200 = 49.5%, **BRAM 48/60 = 80%**, **DSP 66/80 = 82.5%**.
  Fits, but LUT is tight.
- **Timing: WNS = −6.308 ns** (TNS −788.96) on `clk_out1` (62.5 MHz). Worst path:
  `receiver/am/im_l_reg` → `receiver/am/mem5` write, **41 logic levels / 31
  CARRY4 (~22 ns)** — `TdmAmBackend` computes `|I+jQ|` (alpha-max-beta-min) +
  one-pole DC block + all CIC integrator stages **combinationally in one cycle**
  before the per-channel state write.
- **Fix:** pipeline `TdmAmBackend` across its FSM cycles (envelope → DC block →
  CIC in separate clocked stages). The AM duty is only ~0.15 (21 ch ·
  Fs/lane_decim · 4 / Fpl), so there is ample cycle budget to add stages and stay
  real-time + bit-exact; mostly adds FFs (49% used), not LUTs. Then rebuild.

### Full-design bitstream build #2 — TIMING MET, bitstream produced
After pipelining `TdmAmBackend` (one arithmetic stage per cycle; bit-exact +
overflow-free re-verified), the full `projects/pluto` build **passes**:
- **Timing MET: WNS = +0.426 ns**, TNS 0.000, WHS +0.006 ns, 0 failing endpoints
  (62.5 MHz `clk_out1`). (Was WNS −6.308 ns pre-pipeline.)
- **Utilization:** LUT **16199/17600 = 92.0%**, FF 17464/35200 = 49.6%,
  **BRAM 48/60 = 80%**, **DSP 66/80 = 82.5%**. LUT is the tight resource.
- **Artifacts:** `pluto.runs/impl_1/system_top.bit` + `pluto.sdk/system_top.xsa`
  on `xilinx-builder`. The 21-channel airband receiver fits the Z-7010 and meets
  timing alongside the full Maia base (spectrometer + recorder + DDC).

### Cyclic audio DMA + bitstream build #3 — TIMING MET
The one-shot `DmaStreamWrite` (fills start→end then stops, like the recorder)
was made into a **hardware ring** for the airband audio: `DmaStreamWrite` gains
an opt-in `cyclic` mode (on reaching `end` it wraps the write pointer back to
`start` and keeps running; only `stop` halts it; `finished` never pulses). The
recorder path is unchanged (`cyclic=False`). `airband_dma` now uses `cyclic=True`.
- **Verified** (`hdl/test_dma_cyclic.py`): pointer wraps within `[start, end)`,
  the `end` address is never written, streams continuously, `finished` never
  pulses. Full design elaborates clean.
- **Server build #3 (`projects/pluto`, default):** **timing MET, WNS +0.305 ns**,
  TNS 0.000, WHS +0.019 ns. LUT 16223/17600 = 92.2%, BRAM 48/60 = 80%,
  DSP 66/80 = 82.5%. `system_top.bit` + `system_top.xsa` produced.

### Host/PS software for the framed-audio ring — DONE (compiles + reader tested)
Decision: keep the maia DMA, serve framed audio over the **network from
maia-httpd** (no IIO device), reader in **Rust**.
- **maia-pac regenerated** (`svd2rust 0.37.1`) from the new SVD → `airband_*`
  register accessors (`airband_control/freq_addr/freq/dma_next_address`).
- **maia-httpd `airband` module** (`maia-httpd/src/airband.rs` + `fpga.rs`
  accessors + `args.rs`/`app.rs` wiring): configures the AD9361 (LO/Fs/BW/gain),
  programs per-channel NCO words (`round((f−LO)/Fs·2²⁴)`), enables the receiver,
  starts the cyclic DMA, drains `/dev/maia-sdr-airband` (reused
  `maia-sdr,rxbuffer` device, **no kmod change**) keeping a ≥2-buffer safety lag,
  and streams the raw 64-bit records over **TCP `0.0.0.0:30000`**. Built-in
  21-channel default plan; optional `/root/airband.json`. `cargo check` clean.
- **Devicetree** (`firmware/apply_airband_devicetree.py`): idempotent inserter
  adds `maia_sdr_airband@1f000000` (16 MiB) reserved-memory + `maia-sdr,rxbuffer`
  node (`buffer-size 0x10000` → 256 slots). Tested against the pinned dtsi.
- **Host reader** (`host/airband-reader/`, Rust): connects to the TCP stream,
  demuxes by channel, detects drops via the per-channel seq counter, scales
  24→16-bit, outputs stats / per-channel WAV / raw s16; auto-reconnects.
  Smoke-tested end-to-end (3 channels, injected drop detected exactly).
- **Firmware build** (`firmware/build_firmware_full.sh` + `README.md`): the single
  flashable-image builder. Pulls this repo + the fork from git, clones the fork
  into `plutosdr-fw` at the committed HEAD (refuses a dirty tree), patches the DT,
  runs the full `HAVE_VIVADO=1` build, and bakes the fork commit hash into the
  bitstream (USERID + USR_ACCESS) so the running gateware is verifiable. The old
  `HAVE_VIVADO=0` FIT-only shortcut (`build_firmware.sh` + a frozen prebuilt XSA)
  was **removed** — it silently flashed stale gateware. `build_bitstream.sh`
  remains as a fast, non-flashable host-Vivado synthesis/timing check.
- Addressing invariant reconciled: HDL `airband_address_range` == DT
  `maia_sdr_airband` `reg`, slot `0x10000` (see hardware bring-up below for the
  final relocated address).

### Hardware bring-up — DONE (2026-06-15, receiver live on a real Pluto)
First flash bricked the Pluto: the airband reserved-memory node at `0x1f000000`
**collided with the kernel CMA pool** (`cma: Reserved 16 MiB at 0x1f000000`),
so the kernel never came up.
- **Memory-map fix:** relocated the airband DDR ring out of the CMA region by
  carving it from the recorder area — recorder `0x01000000–0x19000000`, **airband
  ring `0x19000000–0x1a000000` (16 MiB)**. Updated `config.py`
  (`airband_address_range`/`recorder_address_range`, `DmaStreamWrite` bakes the
  addresses into the bitstream) and `apply_airband_devicetree.py` (DT `reg` +
  recorder shrink). Confirmed the diagnosis with a minimal no-Vivado build, then
  did the full fix.
- **Second bug — resets/aliasing on `--airband`:** after relocating, enabling
  airband still caused watchdog resets and the DMA never advanced. **Root cause:**
  the `HAVE_VIVADO=0` build only updates `pluto.frm` (FIT/`mtd3`), **never
  `boot.frm` (`BOOT.BIN`/`mtd0`, which holds the bitstream + FSBL)**. The device
  ran the new kernel/DT on the *old* PL: airband register page aliased to the
  control block, and `S_AXI_HP0` was disabled in the old FSBL → AXI hang →
  watchdog. **Fix:** full `HAVE_VIVADO=1` build (`firmware/build_firmware_full.sh`)
  producing a matched `boot.dfu` + `pluto.dfu`; flash **both** partitions.
- **Verified on hardware after flashing both partitions:** clean boot (no
  panic/watchdog), `maia_sdr_airband@19000000` reserved node present, airband
  register page decodes correctly (no aliasing, FPGA magic `"maia"`), enabling
  `--airband` does **not** reset, cyclic DMA advances, **all 21 channels stream
  gap-free over TCP `:30000`** (per-channel seq delta = 1, 0 drops, ~15 625 sps
  matching `14 MHz/128/7`).
- **Auto-start:** patched `buildroot/board/pluto/S60maia-httpd` (baked into both
  build scripts) so `maia-httpd` launches with `--airband` on boot; rebuilt
  `pluto.dfu` (FIT-only) and verified the receiver comes up automatically after a
  power cycle. Full DFU/MTD details: `BUILD.md`, `firmware/README.md`.

### AD9361 front-end lock — DONE (2026-06-16, "all channels on noise" root-caused)
After the cadence fix the stream ran at the correct rate but every channel still
sat on noise on hardware. **Root cause: the Maia web UI was retuning the radio
off the airband band.** The AD9361 is a single shared front-end; on every page
load `maia-wasm`'s `preferences.apply()` re-`PATCH`es each stored AD9361 setting,
and the stored defaults are **2.4 GHz LO / 61.44 Msps**. That overwrote the
123.438 MHz / 14 Msps front-end the airband task programs at startup, so the
channelizer NCOs (baked for 14 Msps) all sat off-band → noise. (The "waterfall
full of signals" was 2.4 GHz Wi-Fi.) Confirmed on the device: `RX_LO=2399999998,
Fs=61440000`; re-asserting `123.438 MHz / 14 Msps` over iio immediately dropped
RSSI 99→77 dB and the waterfall showed airband-band carriers.
- **Fix (fork commit `aa9364e`):** when `--airband` is set the receiver owns the
  AD9361 and the front-end is **locked read-only**:
  - `maia-httpd` `app.rs`: `AppState` carries `airband_locked` (from `--airband`).
  - `httpd/ad9361.rs`: `ad9361_update` is a **no-op while locked** (returns the
    current values), so no `PATCH`/`PUT` on `/api/ad9361` can retune the radio.
    The airband task configures the AD9361 directly on the iio device, so it is
    unaffected.
  - `maia-json` `Api` gains an `airband: bool` flag; `httpd/api.rs` populates it.
  - `maia-wasm` `ui.rs`: `update_airband_lock()` disables the RX freq / Fs /
    bandwidth / gain / AGC controls when the flag is set (UX; the server enforces
    it independently).
- **Software-only** (no HDL/bitstream change): FIT-only build, reflash `pluto.dfu`
  only. **Verified on hardware after reflash:** `maia-wasm v0.12.0-14-gaa9364e`,
  `/api` returns `airband:true`, front-end boots on-band (123.438 MHz / 14 Msps,
  RSSI ~80 dB); loading the web UI with stale 2.4 GHz/61.44 Msps prefs **no longer
  clobbers** the front-end (device stays 123.438/14M, all five controls
  `disabled:true`); stream still 21 ch / ~15.6 ksps / 0 drops.

### Audio level — DONE (2026-06-16, "works but no audio" root-caused)
With the front-end locked on-band, the receiver demodulated correctly but every
channel was near-silent. **Root cause: signal level, not the DSP.** The chain
(channelizer → `|I+jQ|` → DC block → audio CIC) is ~unity gain (bit-exact in
sim), so weak airband AM only reaches tens of LSB at 24-bit (~ −95 dBFS). Two
amplifiers were missing:
- **RF gain:** the AD9361 AGC modes set gain from the *wideband* 14 MHz power and
  settle low, starving weak narrowband channels. Measured ch0 (118.050 AWOS, raw
  24-bit peak): slow_attack@48 dB → ~40, fast_attack@55 → ~54, hybrid@57 → ~71,
  manual 64 → ~154, **manual 71 → ~280** (per-channel peak scales ~linearly with
  gain). → Default changed to **fixed `agc:"manual"`, `gain_db:71.0`**
  (`firmware/airband.json` + `AirbandConfig::default`).
  > Correction (later finding): while each *narrowband* channel peak scales with
  > gain, the *wideband* ADC composite **does clip ~15% at 71 dB** at strong-signal
  > sites — see the RE-DIAGNOSED section below. Lower `gain_db` if you hear
  > distortion.
- **Host makeup gain:** `airband-reader --shift` was unsigned (right-shift only;
  default 8 → divided the quiet sample to silence). Made it **signed** (negative =
  left-shift / makeup gain), default **`-6`** (≈ +36 dB). `airband-listen` default
  `--gain` raised 30 → 3000.
- **Verified on hardware:** manual 71 dB + `--shift -6` → ch0 at **−19 dBFS, peak
  ~18900, 0% clip, ~60% voice-band energy, ~22 dB over the idle-channel floor**;
  21 ch / 0 drops. Config pushed to `/root/airband.json` (persists across reboot);
  built-in default + host tools updated for fresh builds.

> Gotcha logged: `CARGO_TARGET_DIR` is exported to a sandbox cache dir in agent
> shells, so `cargo build` lands the binary there, not `host/*/target/`. `unset
> CARGO_TARGET_DIR` before building host tools to get the repo-local binary.

### Audio buzz — CORDIC magnitude (2026-06-16) — SUPERSEDED, see next section
> **Note:** this section's conclusion (buzz fixed by the CORDIC demod) was wrong.
> The buzz persisted after flashing; it was re-diagnosed as an RF hardware spur.
> See "RE-DIAGNOSED as an RF hardware spur problem" below. CORDIC is retained as
> the correct demodulator. The analysis below is kept as history.
With levels up, a tonal **buzz** was audible on all channels (worst on ch0/5/6/
10/11/12). Diagnosis (host recordings + numpy modelling, not RF):
- FFT showed a low **~90/330/660 Hz** comb on every channel plus a channel-
  specific **~7.6 kHz** whine. Moving the LO to a quiet 240 MHz collapsed all of
  it — i.e. the tones need an in-band **carrier**, ruling out digital/clock/PSU
  noise and pointing at the demod of a real signal.
- **Root cause: the AM envelope detector.** It used a multiplier-free
  `alpha-max-beta-min` magnitude (`max + 3/8·min`) whose gain **ripples ~10% with
  the I/Q phase angle**. Every carrier sits slightly off the un-disciplined Pluto
  LO, so its baseband phasor rotates at the residual offset `df`; the ripple then
  amplitude-modulates it, emitting spurs at **4·df and harmonics at ~−30 dBc**.
  Modelled df≈83 Hz → 333/666 Hz; df≈1906 Hz → 7624 Hz (8th harmonic alias) —
  matching the recordings exactly. Software receivers use exact √(I²+Q²), hence
  clean.
- Single-cycle multiplier-free approximations only reached ~−39 dBc (still
  audible), so the magnitude was replaced with a **CORDIC vectoring** detector in
  `TdmAmBackend` (`am_backend_tdm.py`): multiplier-free (adds/shifts), relative
  accuracy set by iteration count not the 43-bit datapath width. **M=12 → spur
  floor < −140 dBc** (ripple ~0). Constant CORDIC gain K≈1.6468 corrected by a
  5/8 shift-add so audio levels are unchanged.
- **Verified in sim:** HW bit-exact to the new CORDIC model; full `ReceiverTop`
  end-to-end bit-exact; real-time budget closes at the deployment scale (N=21,
  decim=128, cic=4) — `duty_am` 0.40 → 0.85, overflow-free, complete. Pending:
  bitstream build + on-hardware listen.
- **Host band-pass** (`airband-reader`/`airband-listen` `--filter`, 300–3400 Hz)
  was added first as a stop-gap but **degrades voice and only masks** the buzz, so
  it is now **opt-in/off by default**; the CORDIC fix is the real solution.

### Audio buzz — RE-DIAGNOSED as an RF hardware spur problem (2026-06-16, PM)
**Supersedes the CORDIC conclusion above.** After flashing the CORDIC bitstream
the buzz persisted (only its spectrum shifted). A from-the-RF investigation
(raw-IQ capture via maia's recorder + on-device sweeps; toolkit in
`firmware/diagnostics/`) established the real root cause:
- The buzz is in the **raw wideband ADC samples, upstream of all airband DSP**.
  The bit-exact DSP chain is **clean on idle input** (`dsp_chain_sim.py`), so no
  HDL/CORDIC/DC-block/CIC change can fix it.
- It is a **~485 kHz spur comb phase-locked to 120.000 MHz = 3rd harmonic of the
  40 MHz reference**. The comb lines land inside the passbands of exactly the bad
  channels (ch0/2/3/5/6/8/10/11/12, ~18–26 dB) and miss the clean ones.
- The spurs are **invariant to sample rate (BBPLL/ADC clock), LO, and gain** —
  i.e. physical, reference-locked RF, not digital aliases, synth spurs, or
  clipping intermod. The internal-clock change test (`samplerate_spur_test.py`)
  showed **zero** spur movement → **no firmware/HDL fix is possible**.
- Side finding: production manual gain **71 dB clips the ADC ~15%**; lowering
  gain stops clipping but neither removes the in-band spurs nor is worth the lost
  weak-signal sensitivity (`gain_sweep.py`).
- The low (~40–47 Hz) modulation that makes the spurs *audible* is most likely
  the switching supply AM-ing the comb (a static carrier would be DC-blocked).
- **Remedies are hardware**: clean/linear power + USB ferrites (reduce the AM),
  shielding against 120 MHz coupling, an external clean reference, and/or channel
  triage (prefer the spur-free channels ch7/9/14/16/18). See
  `firmware/diagnostics/README.md`.

### Web channel-config page — DONE on hardware (2026-06-16)
A browser config page (`/airband.html`) now edits the channel plan + front-end
settings against a new `maia-httpd` REST API, with a live spectrum/waterfall from
the `/waterfall` WebSocket. Shipped and verified end-to-end on the device.
- **Backend (fork):** `GET`/`PATCH /api/airband` (read/persist the plan to
  `/root/airband.json`; validates ≤21 channels, in-window, gain 0–77 dB, poll
  ≥1 ms; `samp_rate` locked) and `POST /api/system/restart` (applies a saved
  config by restarting the service). `AppState` holds the boot-time (running)
  config + path so the handler reports `needs_restart`. New `maia-json` types
  `Airband`/`PatchAirband`/`AirbandChannel`/`AirbandAgcMode` (snake_case serde);
  `AirbandConfig` gained `Serialize`/`PartialEq` + optional `channel_labels`.
- **Frontend (fork):** static `airband.{html,js,css}` (Canvas2D spectrum +
  scrolling waterfall, channel markers, per-channel signal meters, spur/DC bands,
  zoom/pan + full-band minimap, add/remove/retune/label editing, presets +
  import/export). Linked from the main UI's settings dialog.
- **Build-flow fix (fork `8b601cf`):** the bitstream commit-hash baking used
  `write_bitstream -g USERID:.. -g USR_ACCESS:..`, but Vivado 2023.2's
  `write_bitstream` has **no `-g`** (legacy `bitgen` syntax) → impl_1 died with
  "Unknown option '-g'". This had never built since it was added. Fixed by
  stamping `BITSTREAM.CONFIG.USERID`/`USR_ACCESS` from a pre-`write_bitstream`
  TCL hook (`system_project.tcl`). Note Vivado embeds **bare hex** in the .bit
  header (`UserID=8B601CF0`), not `0x`-prefixed — the provenance grep in
  `build_firmware_full.sh` + `BUILD.md` was updated to match.
- **Build + flash:** full `HAVE_VIVADO=1` build, timing met (WNS +0.296 ns,
  WHS +0.009 ns), `write_bitstream` OK, embedded `UserID=8B601CF0` == fork HEAD.
  Software-only change → reflashed **`pluto.dfu` only** (u-boot env untouched, no
  serial re-apply); the airband gateware logic is unchanged. (Runtime
  `USR_ACCESS` still reads the old mtd0 bitstream until a both-partition flash —
  expected for a FIT-only reflash.)
- **Verified on hardware:** clean boot, no panic/watchdog, `maia-httpd --airband`
  up, `:30000` listening, stream **21 ch / ~15.6 ksps / 0 drops**. `GET
  /api/airband` returns the 21-ch plan; `PATCH` valid persists + flips
  `needs_restart`, out-of-range gain → 400, off-window channel → 400; `POST
  /api/system/restart` brings the service back in ~6 s and applies the config
  (AD9361 re-tuned). **Browser-tested** `/airband.html`: waterfall + 21 channel
  markers + signal meters render, spur/DC bands + minimap draw, label edit →
  Save → `/root/airband.json` written + "saved" + restart banner, Add-channel
  capped at 21/21. Test config removed afterward (device left pristine).

### Web config UI refinements — DONE on hardware (2026-06-17)
Follow-up fixes after first on-device use. All client-side (`airband.{html,js,css}`);
live-validated by pushing assets to `/root` and reloading. Screenshots in
`docs/images/`; embedded in `README.md` → *Web config page*.
- **Waterfall showed only a black plot — two compounding bugs.** (1) CSS
  specificity: the overlay `<canvas>` matched `.canvas_stack canvas { background:
  #05070a }` (0,1,1), which out-ranked a plain `.overlay { background:transparent }`
  (0,1,0), so the opaque overlay hid the spectrum + waterfall (only the markers
  drawn *on* it showed). Fixed with `.canvas_stack canvas.overlay` (0,2,1).
  (2) dB scaling: `percentile()` used a hardcoded `[-120, 40] dB` histogram range
  while the live feed is ~65–98 dB, so the noise-floor estimate and color window
  collapsed. Now the percentile range is derived from the frame and the color
  window tracks the measured floor (≈ the main UI's 35/85 dB profile). Verified
  the canvas paints the orange/red colormap identically to the main waterfall.
- **Per-channel signal bars decayed too slowly.** maia's `Average` mode is a
  per-window mean (no cross-frame decay), so the lag was structural: a ~5 Hz feed
  re-read by a 250 ms timer → up to ~450 ms hold. Now meters update inside
  `onSpectrum` (every frame), and the page raises the spectrometer output rate to
  ≈ 20 Hz on load (`number_integrations` 684→171; shared with the main waterfall,
  only ever raised). Effective latency ~450 ms → ~50 ms.
- **Gain bounded with a clear limit.** Field stays `0–77` (matches backend) and
  `readFrontEndForm` clamps + reflects the value (999→77, −5→0); a `range 0–77 dB`
  hint is shown.
- **Locked fields are now obvious.** Center frequency and sample rate render a
  lock badge + lock icon + dashed/muted read-only styling. Center was made
  read-only (was editable) so it can't move channels outside `[center ± Fs/2]`
  (which the backend rejects); `plan.centerHz` round-trips the device value.
- **Zoom less sensitive (touch/trackpad).** Wheel zoom is `exp(deltaY·0.0015)`
  with line/page normalization and ±50 clamp (~7%/notch vs. the old fixed 20%).

## Next steps
- **Buzz is hardware-bound** (see RE-DIAGNOSED section): pursue power-supply
  cleanup / shielding / external reference; no further HDL work will help. Keep
  the CORDIC magnitude (it is the correct demod regardless and improves accuracy).
- **Signal-quality tuning on real RF:** front-end locked on-band, manual 71 dB,
  host `--shift -6` → live AWOS (ch0) recovers as clean voice at ~ −19 dBFS. Still
  to do: a better antenna for weaker fields, and per-site tuning of `gain_db` /
  `--shift` (lower gain if a strong local signal ever overloads the front-end).
- **LiveATC feeder integration** (`SPEC.md` §9): wire `host/airband-reader`
  per-channel audio into the LiveATC mountpoint convention (server, codec/bitrate
  still open).
- **Hardening** (`SPEC.md` §9): squelch/AGC placement, front-end BPF + FM notch
  hygiene, reconnection/feed supervision.
- **Optional:** add the deferred 133.65 MHz outlier (needs Fs≈20 MHz / 8 lanes /
  recentered LO — a separate window decision, `SPEC.md` §5).
