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
| 8. Pi streamer | **done + deployed** — host reader on `rf-pi` feeds all 21 channels to the local Icecast (`feeds.json` + `deploy/airband-feeds.service`, auto-start/restart), end-to-end validated; live LiveATC mount validation + per-feed supervision pending |
| 9. Hardening | host DSP done (squelch/AGC/band-pass/notch in `host/airband-dsp`); 24/7 soak + feed supervision not started |

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

### TX quieted on boot (receive-only build) — DONE on hardware (2026-06-16)
"Pluto is producing EMI that affects other radios" + "suspect it affects its own
RX." This is a pure receiver, but the Pluto boots the AD9361 in **FDD with the TX
LO running** and only ~10 dB TX attenuation — i.e. it actively transmits a carrier.
- **Live state read (iio sysfs, `ad9361-phy` = `iio:device0`):** `ensm_mode=fdd`,
  `out_altvoltage1_TX_LO_powerdown=0`, `TX_LO=2.450 GHz`,
  `out_voltage0_hardwaregain=-10 dB`. The TX DDS tones (`cf-ad9361-dds-core-lpc`)
  were already silent (`scale=0`), so nothing *intentional* was sent — the emitter
  is the **2.45 GHz LO carrier + TX-path noise leaking out the TX port**.
- **No rx-only ENSM.** `ensm_mode_available = sleep wait alert fdd pinctrl
  pinctrl_fdd_indep`; RX needs FDD and `alert` kills RX too. But in FDD the RX/TX
  LOs are independent synths, so the fix is to **power down the TX LO** and **floor
  TX attenuation**: `out_altvoltage1_TX_LO_powerdown=1`,
  `out_voltage0_hardwaregain=-89.75` (range `[-89.75 … 0]`).
- **RX unaffected (verified).** After the change the waterfall + per-channel meters
  are unchanged; `RX_LO` still locked at 123.438 MHz, `ensm=fdd`.
- **Own-RX A/B (raw `/waterfall`, 4096 bins, median of 60 frames):** in-band noise
  floor **72.50 dB (TX on) vs 72.59 dB (TX off)** — no measurable change. 2.45 GHz
  is far out of the 118–138 MHz window, so the TX LO does *not* raise our own floor;
  the win is **external** (no radiated 2.45 GHz carrier to desense nearby radios).
  The known in-band 120 MHz spur is the 40 MHz ref 3rd harmonic, unrelated to TX.
- **Persistence.** rootfs is **ramfs** (`ntype=ramfs`) — a live `iio` write is lost
  on power cycle, and `maia-httpd` never touches the TX. So the quiet step is baked
  into the boot init: `firmware/patch_tx_quiet.py` idempotently injects a
  backgrounded, retry-until-`ad9361-phy`-ready block into the `start)` case of
  `buildroot/board/pluto/S60maia-httpd`, wired into `build_firmware_full.sh` (step
  3c, next to the existing `--airband` patch). Marker-guarded (`airband-tx-quiet`),
  tested for idempotency + `sh -n`. Effective immediately on the running unit via
  the live `iio` writes; power-cycle-persistent after the next firmware flash.

### Noise floor "extremely poor" → it was ADC clipping — DONE on hardware (2026-06-16)
"Massive peaks, noise floor much worse than expected." Dug into the AD9361
front-end (datasheet/UG-570 knobs) and measured rather than guessed.
- **Tracking already optimal.** `in_voltage_{rf,bb}_dc_offset_tracking_en=1`,
  `in_voltage_quadrature_tracking_en=1`, `calib_mode=auto` → DC spike + image
  already suppressed; not the problem. RX FIR was off; analog RX BW = 14 MHz (= Fs).
- **Root cause = clipping, not noise.** `firmware/diagnostics/floor_sweep.py`
  (new) sweeps gain and reports clip% / PSD floor / SFDR / strong-peak count from
  raw recorder IQ. Result at the 71 dB default: **13.4 % of samples clip**, effective
  floor jammed to **−5.8 dBFS**. Hard clipping is a broadband intermod generator →
  that *was* the "poor floor" + scattered peaks.

  | gain | clip% | floor dBFS | SFDR dB |
  |---|---|---|---|
  | 71 | 13.4 | −5.8 | 26.5 |
  | 55 | 0.77 | −13.8 | 22.6 |
  | **48** | **0.25** | **−17.6** | 20.9 |
  | 40 | ~0 | −24.9 | 20.7 |

  Clipping vanishes by ~48 dB; floor drops ~19 dB **with no SNR loss** (signal+noise
  scale together until the far-below quantization-limited region). SFDR stays ~20 dB
  at every gain → the surviving peaks are the **gain-invariant 120 MHz reference
  comb** (40 MHz ref 3rd harmonic), hardware-only, as before.
- **Fix shipped.** Default `gain_db` **71 → 48** in `firmware/airband.json` (+ updated
  `_comment`), applied live and to `/root/airband.json`. Web-page floor metric fell
  ~70 → ~58 dB. Verdict on other levers: TX already quieted (prior entry); analog BW
  narrow 14→11 MHz and an RX FIR are ~1 dB each (deferred); XADC / unused PS
  peripherals not worth the device-tree risk; the comb needs a clean reference /
  shielding. Docs: `SPEC.md` §5, `README.md` (Channel plan + Status),
  `firmware/diagnostics/README.md`.

### Reference-oscillator calibration vs known AWOS — DONE on hardware (2026-06-16)
Zoomed the web UI on ch0 (118.050 MHz AWOS, just commissioned → transmit freq
trustworthy) and the carrier sat slightly off the marker. Measured instead of
eyeballing: `firmware/diagnostics/measure_offset.py` (new) records IQ, finds the
carrier with a 4 M-pt FFT + parabolic interpolation (3.3 Hz bins, 46 dB SNR).
- **Result (repeatable to <0.3 Hz over 4 runs):** carrier **+1597 Hz high** of the
  118.050 marker → reference **−13.53 ppm low**, true XO ≈ **39,999,459 Hz**. (AM
  envelope demod tolerates a 1.6 kHz carrier offset, so audio was unaffected; this
  is an alignment/centering fix.)
- **Fix = reference calibration, not an LO nudge.** A fixed LO shift only nulls one
  frequency; the proper correction is the per-unit XO value (u-boot env
  `ad936x_ext_refclk_override`; `adi_loadvals` `fdt set`s `/clocks/clock@0
  clock-frequency` = the `ad9364_ext_refclk` the transceiver runs on).
- **Two gotchas found by rebooting and re-measuring (first attempt failed):**
  1. **Brackets mandatory.** `fw_setenv ad936x_ext_refclk_override 39999459` (bare
     decimal) was silently ignored — `fdt set` needs `<N>` for an integer cell, else
     it stores a string. Correct: `fw_setenv ... "<39999338>"`.
  2. **Baseline isn't 40 MHz.** The booted DTB bakes 39,999,891 Hz, and
     displayed = f_true·(believed/true_xo), so the override is `believed × true/
     displayed`, derived from the measured error vs the LIVE clock — not 40e6·(1+ppm).
- **Verified end-to-end:** set `<39999338>`, rebooted → DT clk = 39,999,338, carrier
  error **+1671 → +51 Hz**; web UI shows the AWOS carrier centered on the 118.050
  marker (`docs`/screenshot). Reapplied the ramfs-only session state after reboot
  (gain 48 via `/root/airband.json` + restart; TX-quiet `iio` writes).
- **Warmed-up check (~75 min uptime):** residual settles to **≈ −43 Hz (+0.36 ppm)**
  — the 39,999,338 setting holds within **±0.4 ppm cold→warm**; remaining error is
  XO thermal drift, negligible for 25 kHz AM. No re-tune warranted (a TCXO/GPSDO is
  the only way to do better).
- **Reproducible:** `measure_offset.py` now reads the live believed reference and
  prints the exact bracketed `fw_setenv` (iterate if residual >~1 ppm);
  `pluto_setup_env.py` gained `--refclk-hz` (default 39999338, brackets added
  automatically) to re-apply after a `boot.dfu` flash. Docs: `SPEC.md` §5.2,
  `firmware/diagnostics/README.md`. **Per unit** — re-measure for a different Pluto.

### Host DSP, recording, and streaming — DONE (2026-06-24)
Brought the audio-quality and feeder pieces of RTLSDR-Airband into the host tools,
restructured as a Cargo workspace (`host/Cargo.toml`).
- **New shared crate `host/airband-dsp`** (lib), ported from RTLSDR-Airband and
  adapted to the FPGA's already-demodulated, DC-blocked audio (no IQ/de-emphasis
  on the host). Units, all in raw 24-bit magnitude with dBFS metering:
  - `Squelch` — EWMA noise-floor tracking (decay 0.97, updated only while shut so a
    held transmission can't pull the floor up), SNR/manual threshold, full
    CLOSED→OPENING→OPEN→CLOSING state machine + fast LOW_SIGNAL_ABORT; delays in ms.
  - `VoiceFilter` (4th-order Butterworth band-pass, moved out of the two binaries)
    and `Notch` (2nd-order band-stop, ported from `NotchFilter`).
  - `Agc` — loudness normalization seeded on squelch-open, trained only on
    above-threshold samples, with a bounded tanh **soft-clip** (can't clip the
    16-bit output) and a click-free fade on close.
  - 11 unit tests (filter response, squelch convergence/open/close, AGC
    normalization/limit/fade, dBFS round-trip).
- **`airband-listen`**: runs the chain on the played channel and the squelch on
  *every* channel for activity meters; per-channel **dBFS meters** + squelch-open
  dot; `s`/`a`/`f`/`n` live toggles; `--monitor single|follow|mix` (scanner via
  `F`, or sum-of-open-channels). Band-pass/squelch/AGC on by default; sink volume
  default 1.5 with AGC (audio is pre-normalized) or 25000 with `--no-agc`.
- **`airband-reader`**: shared DSP chain; **split-on-transmission** recording (one
  timestamped WAV/raw per keyed transmission, `--no-split`/`--min-transmission-ms`);
  dependency-free UTC timestamps; stats now report level/floor dBFS + transmission
  counts. Live outputs (any mode): **Icecast** MP3 (`mp3lame-encoder`/LAME, classic
  `SOURCE` protocol, linear resampler 15625→22050, 16 kbps mono for LiveATC,
  auto-reconnect, back-pressure drop), **UDP** s16 PCM, and a dependency-free
  **Prometheus** `/metrics` endpoint.
- **Verified live** against `10.0.16.183`: 21 ch @ ~15.7k sps, 0 drops; squelch
  opened on 118.050 AWOS (`tx` counted) while quiet channels stayed shut; split
  recording wrote one valid WAV per AWOS transmission (mono 15625 Hz, AGC-leveled
  rms ≈ 11.5k) and nothing for silent channels. 18 tests pass, clippy clean,
  release build (LTO) clean. Docs: `README.md`, `SPEC.md` §6.4.

### Squelch hang — fixed AWOS chatter (2026-06-17)
Live listening on 118.050 AWOS chattered (squelch repeatedly opened/closed within
a single broadcast). **Root cause:** the FPGA DC-blocks the audio (§6.2) *before*
the host sees it, so there is no AM carrier on the link — the host squelch can
only act on voice energy. RTLSDR-Airband can use a ~25 ms close delay because it
squelches on the carrier (which persists through speech pauses); our copy of those
defaults (close 24.6 ms + 11 ms `low_signal_abort`) closed on every word gap.
- **Fix (host-only, no FPGA rebuild):** `airband-dsp::Squelch` now defaults to a
  **1 s hang** (`close_delay`) that rides over intra-transmission gaps, and the
  carrier-loss **`low_signal_abort` is disabled by default** (without a carrier it
  just re-introduces chatter). New `--squelch-hang-ms` (default 1000) on both
  `airband-listen` and `airband-reader`; `SquelchConfig::with_hang_ms`.
- A true carrier squelch would need the FPGA to ship the per-channel carrier-DC it
  discards in the DC-block stage, but the 64-bit audio frame (§6.3) has no spare
  field; the host hang gives equivalent no-chatter behavior without a protocol or
  bitstream change. (Documented as the rationale in `SPEC.md` §6.4.)
- 13 `airband-dsp` tests pass (added `hang_bridges_short_gaps`,
  `abort_disabled_by_default`); clippy + release build clean.

### Hiss reduction — host NR + narrow FIR + carrier squelch (2026-06-17)
Follow-up to the squelch hang: once the squelch is open, AWOS still carries
audible broadband hiss. Implemented the three-pronged plan (host-side fix plus two
FPGA changes that ride along in the next bitstream):
- **Host spectral noise reduction (`airband-dsp::Denoise`):** STFT denoiser using
  the decision-directed Wiener gain (Ephraim-Malah), 256-pt frames at 50% overlap
  with a sqrt-Hann window (COLA, exact reconstruction at unity gain). Per-bin noise
  power is learned only while the channel is below the speech-present threshold, so
  speech never leaks into the noise model; a spectral floor (`--denoise-floor-db`)
  bounds the cut to avoid musical noise. Wired into both `airband-listen` (`d`
  toggle, `--no-denoise`) and `airband-reader` between notch and AGC; on by
  default. `rustfft` is the only new dep. Tests: `improves_snr_on_tone_in_noise`,
  `passes_clean_signal_when_no_noise_learned`. Biggest single hiss win, no reflash.
- **FPGA A — narrowed cleanup FIR:** the channelizer's CIC-compensation FIR was
  re-designed to a narrower passband (`fp=0.11`, `fs=0.20`, 63 taps, out_shift 18)
  in `maia_sdr.py` `_AIRBAND_FIR_COEFFS`. This trims the pre-detection bandwidth to
  ~±5-6 kHz, cutting the broadband noise power that reaches the AM detector by
  ~3-4 dB at no extra FPGA cost (same tap count). `receiver_top` bit-exact and
  `realtime_budget` re-run clean.
- **FPGA B — per-channel carrier level → true carrier squelch:** `am_backend_tdm`
  now encodes the DC-block's carrier running-mean as an 8-bit log **minifloat**
  (`[exp(5)|mant(3)]`) and exports it; `audio_framer` carries it in frame bits
  `[31:24]` (audio narrowed 32→24 bits, which already held all the content), wired
  through `receiver_top`. **Breaking frame change** — bitstream + host deploy
  together (`SPEC.md` §6.2). Host decodes it (`airband-dsp::decode_carrier`) and
  adds `--squelch carrier`, which keys on carrier power: steady through speech
  pauses, so it opens/closes on the transmission with no hang and no chatter. In
  carrier mode an internal audio-energy VOX still drives the speech-present flag
  that gates AGC and denoise noise-learning. HDL sims (`audio_framer`,
  `am_backend_tdm` incl. new `_verify_carrier`, `receiver_top`) and 24 host tests
  pass; clippy clean.
- **Built + flashed + verified on hardware (2026-06-24, Pluto+):**
  `TARGET=plutoplus build_firmware_full.sh` on `xilinx-builder` — Vivado
  `write_bitstream completed successfully`, **timing met** (Post-Route WNS
  +0.518 ns, WHS +0.012 ns, 0 failing), provenance **OK** (embedded
  `UserID=FF6363F1` == fork HEAD `ff6363f`). Flashed `boot.dfu` (mtd0) +
  `plutoplus.dfu` (mtd3) over USB DFU; u-boot env verified already correct
  (`ncm`/`ad9364`/`1r1t`, UIO bootargs 3/3). Live check: 21 ch @ ~15.6 ksps,
  **0 drops**; ch0 (118.050 AWOS) active at −74 dBFS. `--squelch carrier`
  opens on ch0/ch11 carriers (tx>0), confirming the carrier byte is populated
  by the new gateware (a zero carrier would never open) and the new 24-bit
  audio + carrier-byte frame layout demuxes correctly.

### Carrier squelch — fixed "all channels open" (2026-06-24)
First on-hardware test of `--squelch carrier` opened **every** channel. Root cause
was the algorithm, not the gateware: the carrier byte cleanly separates a station
from noise (measured: 20 empty channels at bytes 203-227 → 27-168 Me6 decoded, ch0
AWOS at byte 251 → 1.48e9), but the host reused the per-channel **adaptive-SNR**
floor, which (a) inits far below the carrier scale so all channels latch open, and
(b) fundamentally cannot hold a **continuous** carrier (AWOS) open — once the floor
learns the steady level the channel closes.
- **Fix (host-only, no reflash):** carrier mode now uses a **fixed** threshold
  derived from a robust **cross-channel** noise estimate
  (`airband_dsp::carrier_noise_threshold`): all channels of one receiver see the
  same wideband noise, so the 75th percentile of the per-channel carrier levels is
  the noise reference and a station is a large outlier above it. Threshold =
  `pct75 * 10^(snr_db/20)`, recomputed every 8192 frames and pushed to each
  channel's squelch via `Squelch::set_threshold`. Because the threshold comes from
  the *other* channels' noise, it stays put under a continuous carrier, so AWOS
  stays open while empty channels stay shut. `SquelchMode::Carrier` now starts at a
  huge (shut) threshold until the first cross-channel update.
- Both tools wired (reader `run_session`, listen `reader_loop`). 18 `airband-dsp`
  tests pass (added `carrier_threshold_separates_station_from_noise`,
  `carrier_squelch_opens_on_station_only`); clippy clean.
- **Verified live (Pluto+, 10.0.16.55):** `--squelch carrier` → ch0 (AWOS) tx=1,
  all 20 other channels tx=0, 0 drops.

### Precise clock calibration vs LTE — DONE (2026-06-24)
The AWOS calibration (`measure_offset.py`) is limited to ~1 ppm by the 25 kHz AM
channel. Added a GPS-disciplined calibrator that closes that gap by ~100×.
- **`firmware/diagnostics/lte_calibrate.py`:** tunes to an LTE downlink center and
  measures the carrier frequency offset by **cyclic-prefix (CP) autocorrelation** —
  every OFDM symbol repeats its tail one FFT length earlier (128 samples @ the
  1.92 Msps central-6-RB rate), so `r[n]·conj(r[n+128])` carries phase
  `−2π·CFO·128/fs` at every CP sample; summed over a 0.15 s capture the CP samples
  add coherently → CFO to ~10 Hz, unambiguous over ±7.5 kHz (±10 ppm @ 750 MHz).
  eNodeBs hold ±0.05 ppm (3GPP TS 36.104), so the cell is absolute truth, and the
  high RF (~750 MHz) gives leverage a 19 kHz FM pilot / 24 kHz AM audio can't.
- An initial **PSS** estimator was too noisy/biased (±200 Hz); replaced with CP
  autocorrelation. Added `--selftest` (synthesizes LTE-like OFDM, no hardware),
  band auto-scan (restarts `maia-httpd` per band to survive the Pluto's 96 MB RAM),
  and `find_true_center` to disambiguate the 15 kHz raster (probe candidate centers,
  pick the smallest |CFO|). `--apply` programs the override, reboots, re-measures.
- Investigated and rejected **NWR** (unreachable) and the **broadcast-FM 19 kHz
  pilot** (too low-frequency for sub-ppm) before settling on LTE; their probe
  scripts were removed.
- **Applied on hardware (Pluto+):** override `ad936x_ext_refclk_override=<40000064>`.
  Docs: `SPEC.md` §5.2, `firmware/diagnostics/README.md`. Removed the superseded
  AWOS closed-loop wrapper `calibrate_clock.py` (coarse `measure_offset.py` +
  precise `lte_calibrate.py` cover it).

### Deterministic Ethernet MAC (Pluto+) — DONE (2026-06-24)
The Pluto+ pulled a new IP on every reboot. **Root cause:** the Zynq `macb` driver
invents a **random MAC** when its DT node has no `local-mac-address`, and u-boot's
auto-fixup can't help (the board's `ethernet0` alias resolves to a path the fixup
doesn't match), so every boot → new MAC → new DHCP lease → new IP.
- **First attempt (rejected): u-boot env.** Patching `adi_loadvals` to
  `fdt set /amba/ethernet@e000b000 local-mac-address […]` failed two ways — the
  initial path used `/axi/…` (the live node is `/amba/…`; the broken alias misled
  it), and even corrected, *adding a new property* forces an in-place FDT grow that
  overran the adjacent kernel image in the FIT → `bootm` fail → **DFU**. Recovered
  by reading `uboot-env.dfu` over DFU (`dfu-util -U`), deleting the bad command,
  fixing the env's 4-byte CRC32 header, writing back, and `-a firmware.dfu -e`.
- **Fix (devicetree bake):** `firmware/apply_mac_devicetree.py` (idempotent, wired
  into `build_firmware_full.sh`, plutoplus only) inserts `local-mac-address` into
  the `&gem0` node, so the kernel reads a stable MAC natively at probe — no FDT
  growth, no env dependency. Locally-administered `02:0a:35:00:01:22`, override via
  `PLUTO_MAC`. **Software-only DT change → reflash `plutoplus.dfu` only.** Docs:
  `BUILD.md` ("Deterministic Ethernet MAC").

### Audio buzz — confirmed INTERNAL conducted spur comb (2026-06-24)
Re-tested the buzz from the hardware side after a clean front end, a notch on the
strong 118.050 carrier, and a metal enclosure all failed to help. New diagnostics:
`firmware/diagnostics/band_snapshot.py` (host-parametrized FFT + comb detect) and
`term_tests.py` (LO cross-correlation + gain sweep, with auto-restart of maia-httpd
on OOM). Run on the Pluto+ (`10.0.16.100`) with the **antenna disconnected and a
50 Ω load on the RX input**:
- **Decisive: the comb is present with the input terminated.** 15 discrete teeth up
  to ~40 dB over the floor plus a finer comb, across the whole 14 MHz capture, with
  no antenna → the source is **on the board** (conducted), not antenna-borne RF.
  This is why the enclosure, an antenna band-pass, and the carrier notch did
  nothing.
- **Band-wide comb of on-board harmonics**, not a lone line. The 120.000 MHz tooth
  (40 MHz ref 3rd harmonic) is real but is one tooth of the comb.
- **Gain-dependent** (terminated gain sweep): strongest tooth −28 dBFS @71 dB → −43
  @50 → −73 @30 → −86 @15; comb-to-floor ~36 dB @71 → ~17 dB @30. The comb is
  amplified by the front-end gain, so **lowering RX gain is a real mitigation**
  (~19 dB comb-to-floor at 30 dB), on top of removing the 71 dB clipping intermod.
- **Fixed-absolute vs baseband:** a large-shift (2.5 MHz) LO cross-correlation leans
  toward the teeth being at fixed *absolute* RF frequencies (on-board oscillator/
  clock/switcher harmonics into the front end), though only partially rigid. A small
  ±330 kHz LO shift is unreliable here (dense comb aliases the tooth-matcher — the
  verdict flipped between runs); use `term_tests.py corr`.
- **Correction to the earlier RE-DIAGNOSED write-up:** this *confirms and refines*
  the "fixed-frequency RF spur" finding (it is internal RF) but corrects two points
  — it is a **band-wide comb** (not just 120 MHz) and it is **not gain-invariant**.
- **Research (ADI Pluto hardware notes + Pluto+):** a classic conducted-spur
  problem. ADI fixed bad Pluto RF performance with a dedicated ultra-low-noise LDO
  (ADM7160ACPZN1.8) on the 40 MHz oscillator, a series power choke (DLW5BSM801TQ2L),
  synchronized switchers, and USB common-mode chokes (DLW21HN900SQ2L). The Pluto+
  also exposes an external-clock input (EXCLK→GND, feed a TCXO/GPSDO via IPEX).
  Remedies ranked in `firmware/diagnostics/README.md`.
- **Op note:** this Pluto+ exposes only **96 MB** to Linux (~13 MB free, the rest
  reserved for the maia-sdr DMA regions); repeated IQ recordings OOM-killed
  `maia-httpd`. A **bounded boot-time respawn** already exists
  (`firmware/patch_maia_respawn.py`, commit `7db25ed`) for the first-boot OOM race,
  but steady-state OOM from heavy diagnostic recording is intentionally out of its
  scope, so it did not fire; the diagnostic auto-restarts `maia-httpd` over ssh to
  compensate.
- **Default gain lowered 71 → 48 dB** (built-in `AirbandConfig::default` in the fork
  + the `firmware/airband.json` template) given the gain-amplified comb and the 71 dB
  clipping; 48 dB is the measured clipping knee. Raise toward 71–73 dB only behind an
  external selective filter at a quiet site. Docs aligned (README, SPEC §5/§6.1/§7,
  BUILD, firmware/diagnostics/README).

### Buzz mitigation A/B: power source + external reference (2026-06-24)
Controlled tests on the Pluto+ (`10.0.16.100`), **50 Ω termination** on the RX
input, gain fixed at **48 dB**, captured with `band_snapshot.py` (labeled,
non-overwriting PNGs in `firmware/diagnostics/out/`).
- **Input power ruled out.** Identical comb (same teeth, same amplitudes, peak
  excursion ~37.5 dB) on **USB**, a **battery**, and a **benchtop lab PSU**. The
  conducted comb is generated on the Pluto's own PCB (its DC-DC regulation), not the
  input supply — clean external power does **not** help.
- **External reference partitions the comb.** Fed a **29 MHz** sine (AWG) with the
  internal VCTCXO disabled (EXCLK→GND, `ad936x_ext_refclk_override=<29000000>`,
  reboot; AD9361 re-derived 123.438 MHz / 14 MHz from 29 MHz):
  - The **120.000 MHz** tooth **disappeared** → it was the 40 MHz reference 3rd
    harmonic. A clean external reference removes that one line.
  - **Every other tooth, including the loudest (126.000 MHz, ~38 dB), was
    unchanged** → the dominant comb is the **on-board switching regulator**,
    independent of the reference. A clean reference does **not** fix the buzz.
  - The AWG added its own spurs (119.438, 130.000 MHz) → an AWG is not a clean
    reference; a low-spur OCXO is needed for a real reference.
- **Net ranking:** (1) lower RX gain (removes clipping intermod + lowers comb
  prominence; now 48 dB), (2) on-board power decoupling / LDO rework at the
  regulators + RF rails (ADI ADM7160-on-oscillator + DLW5BSM801TQ2L choke) — the
  only lever that touches the switcher comb, (3) external clean reference (removes
  the 120 MHz line only), (4) channel-plan around the fixed switcher teeth. Input
  power and the reference are each **necessary-but-not-sufficient**.
- Captures: `band_50ohm-term_gain71_2026-06-24.png` (baseline),
  `band_{50ohm-term,battery-50ohmterm,benchtop-50ohmterm,ext-29mhz-50ohmterm}_gain48_2026-06-24.png`,
  `localization_loshift-gainsweep_gain71_2026-06-24.png`.
- Tooling: `band_snapshot.py` + `term_tests.py` now self-heal `maia-httpd` over ssh
  on OOM (96 MB Pluto+) and re-apply gain after a restart. The `ad936x_ext_refclk_override`
  persists in u-boot env — to return to the internal VCTCXO, un-ground EXCLK **and**
  reset the override to `<40000064>` before reboot.

### Buzz spur taxonomy: Fs/LO shifts pin each source (2026-06-24)
Followed the power/reference A/Bs with a digital-clock (Fs) sweep and an LO sweep
(terminated, gain 48, internal VCTCXO): `clock_shift_test.py` (alias + digital
solvers; non-commensurate Fs via `PLUTO_FS` to break the LCM alias ambiguity) and
`lo_track_test.py`. **This corrects the earlier "dominant comb = on-board switcher"
inference** — the power/ref A/Bs held Fs fixed and could not see it. Each dominant
wideband tooth now has a source:
- **126.000 MHz (~36 dB, dominant) = the 9th harmonic of the 14 MHz ADC sample
  clock (9×14).** Fixed ABSOLUTE under LO shift (stays at 126.000 as LO moves
  ±0.5 MHz); moves to other n·Fs under Fs shift (10×12.3=123.0, 14×9.1=127.4); LO-
  and reference-independent → internal to the AD9361 sample clock. NOT a switcher
  tone, NOT an external aggressor, NOT a single ∝Fs fraction (digital- and
  alias-solves both correctly excluded the simple hypotheses).
- **125.004 MHz (~20 dB) = the Gigabit-Ethernet 125 MHz PHY clock (Pluto+ only).**
  Fixed absolute across LO and Fs; 125/14 non-integer → not a sample-clock harmonic.
- **120.000 MHz (~13 dB) = 40 MHz reference 3rd harmonic.** Fixed absolute; removed
  by the external-reference test.
- **122.182 MHz (~12 dB)** unidentified fixed board source; **~LO+1.0 MHz (124.434,
  ~12 dB)** tracks the LO → an ADC/baseband DC-region spur.
- **Mitigation by source:** the sample-clock 9th harmonic, GbE clock, and reference
  harmonic are all at fixed ABSOLUTE frequencies, so **frequency planning** (LO /
  channel placement) dodges them — and the shipped plan already keeps them in guard
  gaps (126.000 between ch15 125.9 and ch16 126.25; 120.000 ~100 kHz off ch3). A
  switcher↔bulk-cap bead is **not indicated** for any of them; the external reference
  helps only the 120 MHz line; the GbE clock is a Pluto+ decouple/triage item (or
  feed audio over USB).
- Figures: `clock_shift_intVCTCXO_14-11-8MHz_*` (commensurate; 616 MHz LCM artifact),
  `clock_shift_intVCTCXO_14.0-12.3-9.1MHz_*` (non-commensurate; clean),
  `lo_track_intVCTCXO_gain48_*`.

### Default gain lowered 48 → 0 dB: internal gain stage is the comb/noise generator (2026-06-25)
Bench A/B on the Pluto+ (`10.0.16.100`, USB) comparing an **external LNA** (ZFL-500LN+
ahead of the Pluto, internal gain ~0) against **internal AD9361 gain** with the
antenna direct. Captured with `band_snapshot.py` (identical N=32768 Welch pipeline);
on the 96 MB unit, single short (≤0.06 s, no-retry) recorder grabs avoid the OOM that
the 0.2 s buffer + retry loop caused.
- **The AD9361 internal gain stage is itself the dominant generator** of the conducted
  spur comb AND broadband noise/intermod. Both grow with internal gain and collapse
  SFDR (comb teeth + broadband hash rise faster than the wanted signal). The external
  LNA path is markedly cleaner — lower floor, fewer broadband peaks, better-behaved
  front end — because a clean low-NF LNA sets the system noise figure (Friis) and lets
  the internal stage run at its floor.
- **No-notch / no-LNA / antenna-direct** captures (gain 0 and 25 dB): no ADC clipping
  (118.050 only −18 to −19 dBFS peak), and +25 dB of internal gain raised wideband RMS
  by only ~1.5 dB — consistent with the dominant energy being internal/conducted, not
  antenna signal. Comb structure (~200 kHz, ~57–59 teeth) unchanged with/without the
  notch and with/without the LNA → reconfirms internal/conducted origin.
- **Default gain changed 48 → 0 dB** (built-in `AirbandConfig::default` in the fork +
  `firmware/airband.json`). **This default assumes an external LNA**; on a bare front
  end 0 dB is very insensitive — raise toward the 48 dB clipping knee for that case.
  Docs aligned (README, SPEC §5/§6.1/§7, SPUR-INVESTIGATION). FIT-only change
  (`maia-httpd` rootfs) → built via `build_firmware_full.sh` (TARGET=plutoplus),
  flashed `plutoplus.dfu` only (u-boot env preserved).
- Captures: `band_antenna-direct-nonotch_gain25_2026-06-25.png`,
  `band_antenna-lna-nonotch_gain{0,25}_2026-06-25.png` (the `lna` labels predate the
  amp removal), `band_antenna-notch-lna_gain0_2026-06-25.png`.

### Gain 0 → 12 dB: receiver is internal-noise/quantization-limited (2026-06-25, supersedes the 48→0 entry)
A controlled investigation on the Pluto+ (`10.0.16.100`, GbE) with the external LNA
(ZFL-500LN+, 16 V → ~29 dB) corrected the earlier "minimum internal gain" stance.
Captures via `airband-reader` (minimal-DSP: `--squelch off --no-agc --no-filter
--no-denoise`) + `firmware/diagnostics/analyze_ch_audio.py` / `ab_audio_chains.py`.
- **The receiver is internal-noise-limited, not antenna/thermal-limited.** ch11 idle
  audio floor: antenna+LNA **−92.9 dBFS** vs **50 Ω load+LNA −93.3 dBFS** (Δ0.4 dB) —
  identical. And +6 dB internal gain raised the floor only **+1.1 dB** (not +6). So the
  audio floor is **ADC quantization + the conducted comb**, downstream of the gain; a
  quieter antenna/site cannot lower it. Plot: `audio_floor_antenna_vs_term_2026-06-25.png`,
  `audio_floor_g0_vs_g6_2026-06-25.png`.
- **At 0 dB the wanted signal is *at* the quantization floor.** Controlled gain sweep
  on the continuous 118.050 AWOS carrier (carrier-over-noise, same signal throughout,
  `awos_snr_vs_gain_2026-06-25.png`): audio SNR **~1 dB @0 dB → ~10 @6 → ~12 @12**, then
  a plateau through 42 dB. The earlier 0 dB default literally buried voice in the noise
  ("none of the dBFS readouts are useful because everything is in the noise"). ADC clip
  at 12 dB = **0 %, ~7 dB headroom** (with the LNA).
- **Default gain 0 → 12 dB** (built-in `AirbandConfig::default` in the fork +
  `firmware/airband.json`), tuned for the external LNA. The LNA still matters (low
  system NF; lets the internal gain — the dominant comb/noise generator — run lower)
  but does **not** replace the ~12 dB internal gain. History: 71 → 48 (clip knee) → 0
  (quantization-starved) → **12**. Bare front end (no LNA): raise toward 48 dB.
- **Biggest remaining audio-quality lever is front-end dynamic range:** a SAW airband
  (118–137 MHz) band-pass + low-NF LNA so strong out-of-band signals don't starve the
  12-bit ADC. Host DSP and a quiet site cannot add SNR here.

### Host audio chain + airband-listen debugging tools (2026-06-25)
- **Carrier metric is the real signal indicator.** The demod audio is ~0 (1–2 LSB,
  ≈ −130 dBFS) until modulation rides up, so audio dBFS meters are useless on weak AM.
  The per-channel carrier byte (frame bits [31:24]) is populated on all 21 ch and
  discriminates (118.050 AWOS ≈ +21 dB over the no-carrier baseline). `airband-listen`
  now **defaults to `--squelch carrier`** and its **meter shows carrier level in dB over
  the cross-channel noise reference** (`dB·c`); idle ≈ 0, a keyed station reads positive.
- **Voice band tightened, hiss reduced (evidence-based).** On-air ch0/ch11 FFTs show
  airband AM voice has no usable energy above ~3.4 kHz. Host changes: band-pass kept at
  **300–3400 Hz**, the earlier **de-droop high-shelf removed** (it boosted HF hiss),
  **denoise on by default** (idle hiss −17 dB), and a **standalone 2.5 kHz low-pass**
  (`LowPass` in `airband-dsp`, `--lpf-hz`, default 2500) toggled independently with the
  **`l`** key. Plots: `audio_ch11-122975_*`, `audio_hiss_chains_ch11_*`, `fft_ch0_*`.
- **Live FFT GUI in `airband-listen` (`g` key).** A native egui/egui_plot window shows
  a **Welch** PSD (2048-pt Hann, ~7 averaged segments) of the active post-DSP audio,
  with a hover crosshair reading frequency + magnitude and a **locked Y axis** (no
  per-frame autoscale; drag/scroll still adjusts, double-click resets). macOS requires
  the GUI event loop on the main thread, so the terminal UI moved to a worker thread.
  Deps added: `eframe 0.34`, `egui_plot 0.35`, `rustfft`.

### DeepFilterNet enhancer + VOX squelch + scanner mode (2026-06-26)
- **DeepFilterNet (DFN3) integrated as a toggleable enhancer (`D` key).** New
  `host/airband-listen/src/dfn.rs` wraps `deep_filter`'s `DfTract` (embedded
  default model) with streaming linear resamplers (21875 ↔ 48000 sps) and a ~2-hop
  priming cushion. It runs on the **played stream only**, **after** the per-channel
  filter + AGC chain, and **only while the squelch is open** (NN inference is
  expensive; `reset()` on close). Deps: `deep_filter` git-pinned (`d375b2d8`, features
  `tract,default-model,transforms`) + `ndarray 0.15`; all `tract-*` pinned to `0.21.4`
  in `Cargo.lock` (newer renames `symbol_table`→`symbols` and breaks `libDF`). The
  `0.21.4` model trips a debug-only codegen dedup bug, so the load test is
  `#[cfg_attr(debug_assertions, ignore)]` — it passes under `cargo test --release`,
  the profile the listener ships in.
- **Default chain is now DFN-centric:** VOX squelch + AGC + DFN on; band-pass, 2.5
  kHz LPF, notch, and spectral denoise start **off** (all still toggleable f/l/n/d).
- **Squelch reverted to audio VOX** (`--squelch auto` default). Carrier-level squelch
  is fragile: the conducted comb gives each channel a different idle carrier baseline,
  so a global threshold either never opens or false-opens. VOX keys on in-band voice
  modulation and is immune to those offsets. The carrier metric is retained purely as
  the **activity meter** readout (`dB·c`), not as the squelch.
- **Follow (scanner) mode + ignore list.** `--monitor follow` (or `F`) auto-jumps to
  any channel that breaks squelch; `--ignore-channel 0,5,11` skips channels that
  false-trigger. Idle channels release immediately when another is active, else after
  `--follow-hang-ms`.
- **UI fixes:** the status bar now shows the **DFN** flag, and the header fields
  (squelch state / volume / status / mode) are fixed-width so the bar no longer jumps
  left/right as the squelch opens and closes. The raw-FFT overlay subtracts its DC
  mean before windowing so the trace is readable.
- **Audio-quality note (DFN placement):** DFN belongs **after** the AGC (it then sees
  a ~−3.5 dBFS, in-range signal → effective suppression). Running it *before* the AGC
  starves it (raw demod ≈ −60 dBFS → no suppression); a slow input leveler to fix that
  instead pumped the inter-word noise floor to full scale (loud hiss). Both were tried
  and reverted; DFN-after-AGC is the shipped arrangement.

### DFN tuning for weak-speech intelligibility (2026-06-26)
- Once DFN was running after the filter+AGC chain, the remaining problems were
  weak-signal artifacts. Tuned `DfTract`'s exposed knobs (set in `dfn.rs`,
  CLI-overridable) by ear against live traffic:
  - **`--dfn-min-snr` −10 → −20 dB.** DFN zero-masks any frame below this local-SNR
    floor; −10 chopped quiet/low-SNR speech frame-by-frame ("garbled mumble").
  - **`--dfn-atten-lim` 100 → 15 dB.** Unlimited attenuation fully guts frames DFN
    judges as noise — which includes noise-like consonants (CH/S/F), chopping them.
    The cap mixes `m = 10^(−dB/20)` of the noisy signal back so a frame is never
    fully muted. 12 dB killed the chopping but was slightly noisy; **15 dB** is the
    sweet spot (voice "sounds great", minimal residual noise).
  - **`--dfn-pf-beta` 0.02** post-filter for residual musical noise.
- **Post-DFN brightness high-shelf (`p` key)**, new `HighShelf` in `airband-dsp`:
  the denoiser rolls off the upper voice band on weak speech ("muffled"); a
  high-shelf (default +8 dB above 1600 Hz, Q 0.707) applied to the *cleaned* speech
  restores ~2–3.4 kHz consonants, followed by a soft clip so the boost can't clip.
  (An earlier 2 kHz peaking bell was too narrow; replaced by the shelf.)
- All knobs are live/CLI-tunable: `--dfn-min-snr`, `--dfn-atten-lim`, `--dfn-pf-beta`,
  `--presence-db/-hz/-q`; `p` toggles brightness, `D` toggles DFN.

### Multi-channel DFN/presence in airband-reader for all 21 Icecast feeds (2026-06-26)
- **Goal:** bring the listener's proven chain (VOX squelch → filters → AGC → DFN →
  presence) to `airband-reader` so every streamed channel sounds the same, within
  the Raspberry Pi 5's 4× A76 / 8 GB budget. Per the user: **always filter, never
  drop**; **eager model init at startup** so no transmission is missed.
- **Shared `host/airband-dfn` crate.** Moved `LinResampler`/`DfnEnhancer`/`DfnParams`
  out of the listener and added `Presence` (the `HighShelf` brightness + `soft_clip`),
  so both binaries enhance audio from one source. `airband-listen` now depends on it
  (its `src/dfn.rs` deleted); the `tract-*` 0.21.4 pins and the debug-gated model-load
  test moved with it (passes under `cargo test --release`).
- **Router + per-channel worker model.** `run_session` is now a thin **router**: it
  unpacks frames, detects drops, maintains the carrier-mode noise reference, and
  routes each sample to its channel's **worker thread** over an **unbounded**
  `mpsc` queue — so the socket read never blocks and **no sample is ever dropped**.
  One worker per channel owns its squelch + VOX `sq_filter` (voice-band level, like
  the listener) + band-pass/LPF/notch/denoise/AGC + DFN + presence + recorder/UDP/
  Icecast fan-out, so the Pi's cores run channels in parallel.
- **Eager DFN init + barrier.** Each worker builds its `DfTract` at thread start
  (in-thread, since `DfTract` is `!Send`) and the router waits on a `Barrier` until
  all are ready before connecting — no transmission lost to a cold model. Fixed
  ~10–15 MB/channel RSS, deliberate per the dedicated-Pi spec.
- **DFN concurrency cap = wait, never bypass — per inference hop.** A global
  counting semaphore (`--dfn-max-active`, default 3) bounds simultaneous NN
  inference; a hop that can't get a permit **blocks**, so its channel's (unbounded)
  queue buffers and the audio is delayed but still fully DFN-filtered — latency is
  the only overload valve, honoring *always filter / never drop*. The permit is
  acquired/released **per hop** (around each `DfTract::process` call), not for the
  whole open period, so permits rotate among all open channels and a continuously-
  keyed channel (AWOS/ATIS) cannot starve the rest. DFN3 is sub-real-time on one
  A76, so the cap rarely engages for realistic airband concurrency.
- **Carrier-noise percentile aligned to the listener** (75th → median 0.5): at useful
  gain the conducted comb elevates several channels, so a high percentile inflates the
  "noise" reference. New CLI flags mirror the listener: `--no-dfn`, `--dfn-min-snr`,
  `--dfn-atten-lim`, `--dfn-pf-beta`, `--presence-db/-hz/-q`, `--dfn-max-active`.
- **All outputs enhanced** (Icecast/UDP/WAV get the identical DFN+presence audio).
  Stats/metrics now flow through the shared `Metrics` atomics (router writes
  samples/drops/peak; workers write the squelch gauges).
- **Reader defaults flipped to DFN-centric** for listener parity: band-pass, LPF,
  notch, and spectral denoise now **off by default** (AGC + DFN + presence on).
  The enable flags changed accordingly — `--no-filter`/`--no-denoise` became
  `--filter`/`--denoise` (off→on), and `--lpf-hz` now defaults to `0` (pass e.g.
  2500 to enable). AGC stays on (`--no-agc` to disable).
- Workspace builds + tests green (debug + `--release`); **live Pi 5 validation
  pending credentials.**

### Fix: DFN permit starvation — acquire/release per inference hop (2026-06-26)
- **Symptom:** on the device, with all 21 channels enhanced, only ~3 channels ever
  produced audio; the rest were silent. (A continuously-keyed channel like ch0
  AWOS made it worse.)
- **Root cause:** `Worker::apply_dfn` acquired a `--dfn-max-active` permit when a
  channel's squelch *opened* and held it for the **entire transmission** (released
  only on close). With the default cap of 3, the first 3 channels to open kept their
  permits indefinitely; every later channel blocked forever in `Semaphore::acquire`,
  so its samples were never processed → silence. A continuous carrier never released
  its slot at all. This contradicted the plan's intended *block per hop* semantics.
- **Fix:** scope the permit to the **inference**, not the open period. Added an
  `InferencePermit` trait in `host/airband-dfn` (no-op `()` impl for unbounded use)
  and a `DfnEnhancer::process_sample_gated` that wraps only the `DfTract::process`
  forward pass in `enter()`/`leave()`. `airband-reader` now `impl`s the trait on its
  `Semaphore` and calls `process_sample_gated` each sample; the `holding_permit`
  field and open/close acquire/release are gone. Permits now free between hops and
  rotate among all open channels — a slow/overrun hop still **waits** (buffers, never
  drops), but no channel can be starved. `airband-listen` untouched (uses the no-op
  permit via `process_sample`).
- **Verified on device** (`…:30000`): forcing all 21 channels open with
  `--dfn-max-active 3` now yields audio on **all 21** (RMS −14…−18 dBFS, 0 drops),
  vs 3/21 before. Default VOX+DFN run: ch0 AWOS loud (−18.4 dBFS RMS, −1.5 peak),
  0 drops. `airband-dfn` (3) + `airband-reader` (13) tests pass.

### Live Pi 5 deployment + all-channel Icecast feeds (2026-06-26)
- **End-to-end validated on the tower Pi (`rf-pi`, Pi 5 / 8 GB)** against the live
  Pluto+ (`plutoplus.chongflix.tv:30000`). Built `airband-reader` only (the Pi is
  headless; `airband-listen` needs ALSA, which isn't installed). Stats: 21 ch @
  ~21875 sps, **0 drops**; VOX squelch keys the active channels (ch0 AWOS etc.).
- **TLS source proven:** single `--icecast-*` feed to `icecast.chongflix.tv:9344`
  (`tls: transport`) connected, stayed up, and a listener pull of the mount returned
  valid MP3 frames (`ff f3 …`) — full path Pluto RF → DFN → MP3 → Icecast → listener.
- **All-channel feeds file** `feeds.json` (repo root): one entry per built-in
  channel → the local Icecast plaintext source port (`:9343`), 32 kbps / 22050 Hz,
  mounts keyed by frequency (`/pluto-118p050.mp3` …). **Strict JSON** — the parser
  is `serde_json`, so the README's illustrative `//` comments must be removed in a
  real file (README updated to say so). A clean single-reader run connected **all
  21 mounts**, 0 drops, no errors.
- **systemd unit** `deploy/airband-feeds.service` (installed on `rf-pi`,
  enabled): runs the `--feeds` reader as `pi`, `Restart=always` +
  `StartLimitIntervalSec=0`, `KillSignal=SIGINT` for graceful stop. Single instance
  is inherent to the service.
- **Gotcha — Pluto is single-client.** Several concurrent readers (leftover
  backgrounded test processes that outlived their SSH sessions because the binary
  traps SIGINT) fought over the `:30000` socket and **wedged `maia-httpd`** (both
  `:8000` and `:30000` refused; device still pinged). Recovered with
  `/etc/init.d/S60maia-httpd restart` on the Pluto. Lessons baked into the README:
  run exactly one reader per device, and clean up with `pkill -x airband-reader`
  (process-name match), never `pkill -f` (self-matches the controlling shell).

## Next steps
- **Buzz spurs are characterized** (see "Buzz spur taxonomy", 2026-06-24): the
  dominant 126.000 MHz tooth is the AD9361 sample-clock 9th harmonic (9×14 MHz), plus
  a 125 MHz GbE clock (Pluto+) and the 120 MHz reference 3rd harmonic — all fixed
  absolute. Levers: lower RX gain (done, 48 dB), frequency planning (keep channels off
  those lines; the plan already does), and an external OCXO reference (for 120 MHz).
  Input power, enclosure shielding, antenna filtering, and a switcher↔bulk-cap bead do
  not help; no HDL work will. Keep the CORDIC magnitude (the correct demod regardless).
- **Signal-quality tuning on real RF:** front-end locked on-band, manual 71 dB,
  host `--shift -6` → live AWOS (ch0) recovers as clean voice at ~ −19 dBFS. Still
  to do: a better antenna for weaker fields, and per-site tuning of `gain_db` /
  `--shift` (lower gain if a strong local signal ever overloads the front-end).
- **LiveATC feeder validation** (`SPEC.md` §9): the Icecast source client is built
  and now **validated end-to-end** on `rf-pi` — all 21 channels feed the local
  Icecast (`feeds.json` + `deploy/airband-feeds.service`), plus a single TLS feed
  proven. Remaining is validating against a real **LiveATC** mount and per-feed
  supervision/alerting.
- **Hardening** (`SPEC.md` §9): host squelch/AGC/band-pass/notch are done
  (`airband-dsp`) and the feeder runs under systemd (auto-restart); remaining is
  24/7 soak and per-feed supervision.
- **Optional:** add the deferred 133.65 MHz outlier (needs Fs≈20 MHz / 8 lanes /
  recentered LO — a separate window decision, `SPEC.md` §5).
