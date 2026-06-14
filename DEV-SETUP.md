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
| `libiio` 0.25 / `iio_info` | built from source → `~/.local` | talk to the Pluto over USB |

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
pip install -e maia-sdr/maia-hdl           # use maia_hdl as a library (our hdl/ experiments import it)
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
x86-64-only**, so they cannot be native on Apple Silicon. They run on a separate
x86-64 Linux host (`pluto-airband-fpga.md` §5.1).

- `ghcr.io/maia-sdr/maia-sdr-devel` — Yosys/Icarus/Amaranth/Rust + everything to
  run Vivado 2023.2 (Vivado supplied via a Docker volume at `/opt/Xilinx`). Used
  for: Amaranth→Verilog→Vivado IP, synth+impl→bitstream, firmware assembly.
- `ghcr.io/maia-sdr/cross-armv7-unknown-linux-gnueabihf-maia-sdr` — `cross` image
  to build `maia-httpd` against the Pluto's buildroot toolchain.

> Do **not** run these images on this Mac under x86 emulation for routine work —
> it is slow and unnecessary. The native env above is the inner loop; only ship
> synthesis/bitstream to the x86 box.

### Provisioned server

- Host `xilinx-builder` — Ubuntu 22.04.5, x86-64, 32 vCPU, 31 GiB RAM, ~294 GB
  disk. SSH as `administrator` (key-based auth set up from the Mac).
- **Rootless Docker** (Docker root dir under `~/.local/share/docker`). This is the
  single biggest gotcha: host `administrator` (uid 1000) maps to **container
  root**, so the firmware build container must run as **`DOCKER_USER=0:0`** (not
  `$(id -u):$(id -g)` as upstream docs assume) to own the bind-mounted source.
- Source at `~/pluto-build/plutosdr-fw` — `plutosdr-fw` v0.8.2 (`7d4cfda`) cloned
  recursively (`maia-sdr`, `buildroot`, `linux`, `u-boot-xlnx`, `IQEngine` + nested
  `adi-hdl`). `plutosdr-fw` pins `maia-sdr` at `c96c496` — same SHA as the Mac dev
  pin, so HDL versions are consistent.

### Vivado / Vitis 2023.2

Required for the from-source bitstream (`plutosdr-fw` falls back to downloading a
prebuilt `system_top.xsa` if absent). Installed from the AMD offline Single-File
Download `FPGAs_AdaptiveSoCs_Unified_2023.2_1013_2256.tar.gz` (≈112 GB).

- Installed **Vitis** edition (superset: lays down `Vivado/2023.2`, `Vitis/2023.2`,
  `Vitis_HLS/2023.2` — all three are sourced by `build-docker.sh`), **Zynq-7000
  device family only** (the Pluto is an XC7Z010) to keep the install ~lean.
- Target dir `/opt/Xilinx` → gives the `.settings64-*.sh` files the build sources.
- Batch install: `xsetup --agree XilinxEULA,3rdPartyEULA --batch Install
  --config ~/.Xilinx/install_config.txt` (config from `xsetup -b ConfigGen`, with
  `Destination=/opt/Xilinx` and all `Modules` set to 0 except `Zynq-7000:1`).
- No license needed: the XC7Z010 is in the free WebPACK/Standard device set.

### Vivado Docker volume

`plutosdr-fw/compose.yml` mounts an **external** named volume `vivado2023_2` at
`/opt/Xilinx`. Rather than copy ~70 GB into a volume, bind the named volume to the
host install (compose.yml stays unmodified):

```bash
docker volume rm vivado2023_2 2>/dev/null
docker volume create --driver local \
  --opt type=none --opt device=/opt/Xilinx --opt o=bind vivado2023_2
```

### Run the firmware + bitstream build

```bash
cd ~/pluto-build/plutosdr-fw
DOCKER_USER="0:0" TARGET=pluto docker compose run --rm build
```

This runs `build-docker.sh` in the devel image (sources Vivado/Vitis/Vitis_HLS,
runs `make` under an `Xvfb` display for `xsct`). With Vivado present it does the
full from-source bitstream (`HAVE_VIVADO=1`). Artifacts land in
`plutosdr-fw/build/` (`.frm`/`.dfu`, `boot.frm`, `system_top.xsa`, etc.).

### Out-of-context (OOC) module synthesis — real utilization vs Yosys

For quick real LUT/FF/DSP/BRAM on the **actual Pluto part** without a full build,
run Vivado **directly on the host** (no Docker) against Amaranth-generated Verilog
(`hdl/synth_estimate.py` emits these to `hdl/out/*.v`). Two host quirks to know:

- **Missing `libtinfo.so.5`** — Vivado 2023.2 needs the libtinfo5/ncurses5 ABI but
  Ubuntu 22.04 ships v6. Batch mode fails (`librdi_commontasks.so: libtinfo.so.5`)
  even though `vivado -version` works. Vivado's loader resets `LD_LIBRARY_PATH`, so
  a per-run shim doesn't take; add system compat symlinks once (reversible):

  ```bash
  sudo ln -sf /usr/lib/x86_64-linux-gnu/libtinfo.so.6 /usr/lib/x86_64-linux-gnu/libtinfo.so.5
  sudo ln -sf /usr/lib/x86_64-linux-gnu/libncurses.so.6 /usr/lib/x86_64-linux-gnu/libncurses.so.5
  sudo ldconfig
  ```

- **Root disk is full** (`/dev/vda2` at 100%; the from-source build artifacts fill
  it). Run OOC work on the SMB share `/mnt/vivado-share` and point `HOME`/`TMPDIR`
  there so Vivado's `.Xilinx`/logs/scratch don't hit the full root fs:

  ```bash
  cd /mnt/vivado-share/ooc && export HOME=$PWD/home TMPDIR=$PWD/home
  source /opt/Xilinx/Vivado/2023.2/settings64.sh
  vivado -mode batch -nojournal -source ooc_synth.tcl -tclargs <module>.v <TopName>
  # report -> <TopName>_ooc_util.rpt ; ooc_synth.tcl is in hdl/
  ```

  Launch detached (`setsid ... </dev/null &`) so the run survives the SSH session.

  For a full **synth + place + route + timing** run (the integrated core), use
  `ooc_place.tcl` instead, which also creates a clock and reports timing:

  ```bash
  vivado -mode batch -nojournal -source ooc_place.tcl \
         -tclargs channelizer_core.v channelizer_core 16.0   # 16 ns = 62.5 MHz
  # reports -> channelizer_core_routed_util.rpt, channelizer_core_timing.rpt
  ```

  Generate the Verilog on the Mac first with `python hdl/emit_core_verilog.py` and
  `scp out/channelizer_core.v` to `/mnt/vivado-share/ooc/`.

**Confirmed (2026-06-14):**
- 21-ch `TdmDdcLane` (synth OOC) = 4 DSP48E1, 3374 LUT, 7760 FF, 0 BRAM (matches
  Yosys); parallel `MultiStageDecimator` = 58 DSP / 212 LUT / 1055 FF.
- Integrated **`ChannelizerCore`** (5 ch, BRAM-backed lane + folded complex 119-tap
  cleanup), **place + route, 62.5 MHz**: 1309 LUT (7.4%), 1577 FF (4.5%), 3 BRAM
  tiles, 8 DSP48E1 — **timing MET, WNS +3.07 ns, 0 failing endpoints**.

## libiio (host tools to talk to the Pluto)

`libiio` is **not in Homebrew core**, and its macOS CMake build defaults to a
`.framework` installed under `/Library/Frameworks` (needs root). We instead build
a plain dylib + tools into `~/.local` (no sudo). Pinned to tag `v0.25`
(`b6028fd`) — the last 0.x release, best-tested against the Pluto's `iiod`.

Build deps (Homebrew): `libusb`, `libxml2`, `cmake`, `pkg-config`.

```bash
cd /Users/juanjchong/Documents/GitHub/pluto-airband
git clone --depth 1 --branch v0.25 https://github.com/analogdevicesinc/libiio.git
export PKG_CONFIG_PATH="/opt/homebrew/opt/libxml2/lib/pkgconfig:/opt/homebrew/lib/pkgconfig"
# NOTE: Homebrew cmake is shadowed by MacPorts here; call it explicitly.
/opt/homebrew/bin/cmake -S libiio -B libiio/build \
  -DCMAKE_INSTALL_PREFIX="$HOME/.local" \
  -DOSX_FRAMEWORK=OFF \
  -DWITH_USB_BACKEND=ON -DWITH_NETWORK_BACKEND=ON \
  -DWITH_SERIAL_BACKEND=OFF -DWITH_LOCAL_BACKEND=OFF -DHAVE_DNS_SD=OFF \
  -DCMAKE_MACOSX_RPATH=OFF -DCMAKE_INSTALL_NAME_DIR="$HOME/.local/lib"
/opt/homebrew/bin/cmake --build libiio/build -j4
/opt/homebrew/bin/cmake --install libiio/build
```

`-DCMAKE_MACOSX_RPATH=OFF -DCMAKE_INSTALL_NAME_DIR=...` is required: libiio bakes
an rpath (`@executable_path/../..`) suited to its framework layout, which is wrong
for a `bin`+`lib` prefix. Building with an absolute install name avoids it.

Installs to `~/.local`: `bin/iio_{info,readdev,writedev,attr,reg,genxml}`,
`lib/libiio.0.25.dylib`, `include/iio.h`, `lib/pkgconfig/libiio.pc`.

Put it on PATH (add to `~/.zshrc`):

```bash
export PATH="$HOME/.local/bin:$PATH"
export PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig:$PKG_CONFIG_PATH"
```

Verify: `iio_info --version` → `0.25 ... backends: xml ip usb`. With no Pluto
attached, `iio_info -s` prints "No IIO context found" (expected).

## Still TODO before hardware bring-up

- Add `~/.local/bin` to your shell PATH (see above) for convenient access.
- ~~Stand up the x86-64 build server and do a clean from-source bitstream build
  of unmodified Maia SDR (handoff §7 step 1).~~ **Done** — server provisioned,
  Vivado 2023.2 installed, bitstream built (timing met), base PL usage measured
  (LUT 5416/17600, FF 6493/35200, BRAM 29/60, DSP 18/80). See `PROGRESS.md`.
- Flash the baseline Maia image (`build/pluto.dfu`) to a Pluto (§7 step 3).
