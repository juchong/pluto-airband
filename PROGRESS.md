# Progress Log

Running log of work, decisions, and state for the Pluto FPGA airband receiver.
Authoritative spec: `pluto-airband-fpga.md`. Environment details: `DEV-SETUP.md`.

## Status at a glance

| Handoff §7 task | State |
|---|---|
| 1. x86 build server bring-up (bitstream build of unmodified Maia) | not started |
| 2. Mac dev env (Amaranth, cocotb/Icarus, Rust, libiio+dfu-util) | **done** |
| 3. Flash baseline Maia to Pluto | not started |
| 4. Channelizer feasibility (GATE) | exploring DDC building block |
| 5. AM demod block | **done** (envelope mag + DC-block + audio decimate, chain verified) |
| 6. Single-channel end-to-end | not started |
| 7. Multi-channel | not started |
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
  (handoff §5.1). Not yet provisioned.
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
1. Channel count target N (drives feasibility + framing).
2. Capture window center + width (all channels inside, with edge margin).
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

## Next steps
- Explore time-multiplexing the single DDC datapath across many channels (§4.2)
  to estimate Z-7010 resource fit (the feasibility GATE).
- Provision / get SSH to the x86-64 build server; clean from-source bitstream
  build of unmodified Maia SDR (handoff §7 step 1).
- Resolve the §8 open decisions (esp. channel count + capture window).
