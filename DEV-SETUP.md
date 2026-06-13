# Pluto Airband — Development Environment Setup

This documents the **macOS (Apple Silicon) native development environment** for the
Pluto FPGA multichannel airband receiver (see `pluto-airband-fpga.md` §5.2). It is
the fast inner-loop environment for authoring Amaranth HDL and running
cocotb/Icarus simulations.

> **Scope:** this machine is the *development* box, **not** the build server.
> Vivado synthesis / bitstream / firmware assembly runs on a separate **x86-64
> Linux** box using Docker (see [Build server](#build-server-x86-64--docker)).

## What's installed (this Mac)

| Component | Source | Purpose |
|---|---|---|
| `git` | Homebrew | clones |
| `python@3.12` | Homebrew | interpreter for the venv |
| `icarus-verilog` (`iverilog`/`vvp`) | Homebrew | Verilog simulator for cocotb |
| `yosys` | Homebrew | present, but **not used** by the sim flow (see note below) |
| `dfu-util` | Homebrew | flash the Pluto over USB (later) |
| Rust toolchain | rustup (`~/.cargo`) | edit / `cargo check` maia-httpd |
| Python deps | `.venv` (see `requirements-dev.*`) | Amaranth, cocotb, numpy, scipy |
| `libiio` / `iio_info` | **not yet installed** | talk to the Pluto over USB (needed before HW bring-up) |

### Upstream dependencies (read-only, pinned by SHA)

We **build on top of** Maia SDR and do not fork or modify it. The clones below are
external dependencies — they are git-ignored by this project and pinned to the
commits we validated against. `maia_hdl` can also be used as a library
(`pip install maia-hdl`) in our own Amaranth designs.

| Repo | Pinned commit | Notes |
|---|---|---|
| `maia-sdr` | `c96c496a63d720912fcd56e530b905c8bc2c6c84` (2026-04-27, `main`) | primary foundation (DDC, FFT, DMA) |
| └ submodule `XilinxUnisimLibrary` | `1c8e05fd1e9a79ceb8b996a0996674122eed086f` | **initialized** — cocotb tests need `DSP48E1.v`, `FIFO18E1.v`, `glbl.v` |
| └ submodule `adi-hdl` | `065c8f186ef87ff049d279ed5859ee8d97d91808` | **NOT initialized here** — only used by the Vivado bitstream build on the x86 server; large |
| `plutosdr-fw` | `7d4cfda89bef67f9e3c2fb8bd196bd4f49698799` (v0.8.2, 2025-11-09) | Maia fork; submodules (linux/hdl/buildroot/u-boot) only needed on build server |

Recreate the clones:

```bash
cd /Users/juanjchong/Documents/GitHub/pluto-airband
git clone https://github.com/maia-sdr/maia-sdr.git
git -C maia-sdr checkout c96c496a63d720912fcd56e530b905c8bc2c6c84
git -C maia-sdr submodule update --init --depth 1 maia-hdl/XilinxUnisimLibrary
git clone https://github.com/maia-sdr/plutosdr-fw.git
git -C plutosdr-fw checkout 7d4cfda89bef67f9e3c2fb8bd196bd4f49698799
```

## Recreate the Python environment

```bash
cd /Users/juanjchong/Documents/GitHub/pluto-airband
/opt/homebrew/opt/python@3.12/bin/python3.12 -m venv .venv
source .venv/bin/activate                 # also sets AMARANTH_USE_YOSYS=builtin
pip install --upgrade pip wheel
pip install -r requirements-dev.lock.txt   # exact lock; or requirements-dev.txt for top-level pins
```

### Critical: `AMARANTH_USE_YOSYS=builtin`

Amaranth needs Yosys to emit Verilog. The Homebrew `yosys` (0.66) is **too new**
and breaks Amaranth's backend. The venv's `activate` exports
`AMARANTH_USE_YOSYS=builtin` so Amaranth uses the bundled `amaranth-yosys` wheel
instead. If you run Python outside the venv, set this manually.

## Run the test suites (validates the env)

```bash
source .venv/bin/activate

# Pure Amaranth tests (51 tests)
cd maia-sdr/maia-hdl && python -m unittest

# Mixed Amaranth/Verilog tests (cocotb + Icarus); all subdirs should PASS
cd maia-sdr/maia-hdl/test_cocotb && make
grep --include=results.xml -r -e failure .   # should print nothing
```

Both suites pass with the pinned versions below.

## Version alignment (why these pins)

The pins match the upstream **`maia-sdr-devel` container** (the source of truth
that upstream CI uses), tag `20260304`
([Dockerfile](https://github.com/maia-sdr/maia-sdr-docker/blob/main/maia-sdr-devel/Dockerfile)):

- **Amaranth `0.5.8`** — the handoff doc's `0.5.2` is stale; current `main`'s
  cocotb tests use the cocotb 2.0 API and `0.5.2` emits the obsolete `read_ilang`
  yosys command. `0.5.8` emits `read_rtlil` and works with modern yosys.
- **cocotb `2.0.1`** (+ `cocotb-bus`) — the tests use the cocotb 2.0 `unit=`
  kwarg; cocotb 1.x fails.
- **numpy `1.26.4`** (`<2`) — Ubuntu 24.04 apt ships numpy 1.26; numpy 2's NEP-50
  integer rules break `test_packer`.
- **`amaranth-yosys 0.40.0.0.post103`** — bundled yosys for the Mac (the container
  uses OSS CAD Suite instead).

## Build server (x86-64 + Docker)

The Maia Docker images are **`linux/amd64`-only** (verified via the GHCR registry
API — no `arm64` / multi-arch variant exists). They wrap **Vivado 2023.2, which is
x86-64-only**, so they cannot be native on Apple Silicon. Run them on a separate
x86-64 Linux host (`pluto-airband-fpga.md` §5.1):

- `ghcr.io/maia-sdr/maia-sdr-devel` — Yosys/Icarus/Amaranth/Rust + everything to
  run Vivado 2023.2 (Vivado installed manually into a Docker volume at
  `/opt/Xilinx`). Used for: Amaranth→Verilog→Vivado IP, synth+impl→bitstream,
  firmware assembly.
- `ghcr.io/maia-sdr/cross-armv7-unknown-linux-gnueabihf-maia-sdr` — `cross` image
  to build `maia-httpd` against the Pluto's buildroot toolchain.

> Do **not** run these images on this Mac under x86 emulation for routine work —
> it is slow and unnecessary. The native env above is the inner loop; only ship
> synthesis/bitstream to the x86 box.

## Still TODO before hardware bring-up

- Install `libiio` (not in Homebrew core): build from
  `analogdevicesinc/libiio`, or install ADI's macOS `.pkg`. Needed for
  `iio_info` / reading audio from the Pluto over USB.
- Stand up / get SSH access to the x86-64 build server and do a clean
  from-source bitstream build of unmodified Maia SDR (handoff doc §7 step 1).
