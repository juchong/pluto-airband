# Progress Log

Running log of work, decisions, and state for the Pluto FPGA airband receiver.
Authoritative spec: `pluto-airband-fpga.md`. Environment details: `DEV-SETUP.md`.

## Status at a glance

| Handoff §7 task | State |
|---|---|
| 1. x86 build server bring-up (bitstream build of unmodified Maia) | not started |
| 2. Mac dev env (Amaranth, cocotb/Icarus, Rust, libiio+dfu-util) | **done** (libiio pending) |
| 3. Flash baseline Maia to Pluto | not started |
| 4. Channelizer feasibility (GATE) | not started |
| 5. AM demod block | not started |
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

## Decisions made

### Project / dependency strategy
- **Do not fork or modify Maia SDR.** Build on top of it; treat `maia-sdr` and
  `plutosdr-fw` as read-only, SHA-pinned external deps. Use `maia_hdl` as a
  library where practical.
- **This Mac is the development box, not the build server.** Vivado is x86-64
  only; the Maia Docker images are `linux/amd64`-only (verified via GHCR API), so
  synthesis/bitstream/firmware runs on a separate x86-64 Linux host with Docker
  (handoff §5.1). Not yet provisioned.
- **Repo:** workspace root is a local git repo tracking only our artifacts
  (docs, requirements, future HDL/Pi code). No remote yet.

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

## Next steps
- Install `libiio` (not in Homebrew core) for `iio_info` + USB audio reads.
- Provision / get SSH to the x86-64 build server; clean from-source bitstream
  build of unmodified Maia SDR (handoff §7 step 1).
