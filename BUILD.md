# Pluto Airband — Build, Flash & Operations

The complete build/deploy/operate reference: the macOS dev environment, the
x86-64 build server, firmware build + DFU flashing, the u-boot/devicetree
invariants, and field troubleshooting. Start at `README.md` for the project hub;
see `SPEC.md` for design rationale (this guide expands on §8 of the spec).

The macOS (Apple Silicon) box is the fast inner loop for authoring Amaranth HDL
and running cocotb/Icarus sims; Vivado synthesis/bitstream/firmware runs on the
x86-64 Linux build server.

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

### Upstream dependencies

We **build on top of** Maia SDR. The airband DSP/DMA/control changes live in our
**fork** of `maia-sdr` (`maia-hdl` HDL + `maia-httpd` control daemon); the
firmware assembler `plutosdr-fw` stays at the Maia pin and we splice our fork in
at build time (see [airband firmware build](#airband-firmware-build-boot--pluto)).

| Repo | Source / pin | Notes |
|---|---|---|
| `maia-sdr` (our fork) | `github.com/juchong/maia-sdr`, branch **`pluto-airband`** | airband DDC/decimation/AM, cyclic airband DMA @ `0x1900_0000`, `maia-httpd` airband control + framed-audio TCP stream |
| └ submodule `XilinxUnisimLibrary` | `1c8e05fd1e9a79ceb8b996a0996674122eed086f` | **initialized** — cocotb tests need `DSP48E1.v`, `FIFO18E1.v`, `glbl.v` |
| └ submodule `adi-hdl` | `065c8f186ef87ff049d279ed5859ee8d97d91808` | needed by the Vivado bitstream build on the x86 server; large |
| `plutosdr-fw` | `7d4cfda89bef67f9e3c2fb8bd196bd4f49698799` (v0.8.2, 2025-11-09) | Maia fork; submodules (linux/hdl/buildroot/u-boot) only needed on build server. Our fork is spliced into `plutosdr-fw/maia-sdr` at build time. |

Recreate the clones (Mac dev box — the fork is the working tree for `maia-sdr`):

```bash
cd /Users/juanjchong/Documents/GitHub/pluto-airband
git clone -b pluto-airband https://github.com/juchong/maia-sdr.git
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

- **Amaranth `0.5.8`** — current `main`'s cocotb tests use the cocotb 2.0 API, and
  older `0.5.x` (e.g. `0.5.2`) emits the obsolete `read_ilang` yosys command.
  `0.5.8` emits `read_rtlil` and works with modern yosys.
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
x86-64 Linux host (`SPEC.md` §8).

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
- Source layout under `~/pluto-build/` — **everything the build touches is a git
  clone** (the build refuses loose/uncommitted sources, so the gateware's baked-in
  commit hash is always meaningful):
  - `airband/` — clone of **this** repo (build scripts, devicetree patch,
    channel-plan template). The `maia-sdr` fork (`pluto-airband` branch) is a
    separate clone at `airband/maia-sdr` (this repo git-ignores `maia-sdr/`).
  - `plutosdr-fw/` — `plutosdr-fw` v0.8.2 (`7d4cfda`) cloned recursively
    (`buildroot`, `linux`, `u-boot-xlnx`). Its bundled `maia-sdr` submodule is
    **replaced at build time by a local git clone** of `airband/maia-sdr` checked
    out at the exact committed HEAD, so the bitstream + `maia-httpd` are built from
    our fork's tracked sources, not the upstream pin or a dirty working tree.

### Bootstrap the server from scratch

On a fresh x86-64 Ubuntu host with Docker (rootless) installed:

```bash
mkdir -p ~/pluto-build && cd ~/pluto-build

# 1. This repo (build scripts, devicetree patch, channel-plan template).
git clone https://github.com/juchong/pluto-airband.git airband

# 2. The maia-sdr fork (airband HDL + maia-httpd), INTO airband/maia-sdr.
#    (This repo git-ignores maia-sdr/, so the fork is a separate clone living
#    there.) Init its HDL submodules — the bitstream build needs adi-hdl +
#    XilinxUnisimLibrary working-tree sources.
git clone -b pluto-airband https://github.com/juchong/maia-sdr.git airband/maia-sdr
git -C airband/maia-sdr submodule update --init --recursive maia-hdl

# 3. plutosdr-fw (firmware assembler), pinned, recursive.
git clone https://github.com/maia-sdr/plutosdr-fw.git
git -C plutosdr-fw checkout 7d4cfda89bef67f9e3c2fb8bd196bd4f49698799
git -C plutosdr-fw submodule update --init --recursive linux buildroot u-boot-xlnx
```

Then install Vivado/Vitis 2023.2 and create the `vivado2023_2` volume (below).
The build scripts handle the fork splice, devicetree patch, init-script
auto-start patch, and submodule init automatically — you do not edit
`plutosdr-fw` by hand.

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

### Airband firmware build (`boot.*` + `pluto.*`)

`firmware/build_firmware_full.sh` (in this repo) is the **single entrypoint that
produces flashable images**. It builds only from **committed git state** and bakes
the fork's commit hash into the bitstream, so a stale-gateware flash can't happen
silently. It (1) `git pull`s this repo *and* the `maia-sdr` fork — aborting if the
fork has uncommitted changes (override `FORCE_DIRTY=1`), (2) clones the fork into
`plutosdr-fw/maia-sdr` at the exact committed HEAD, (3) applies the airband
reserved-memory devicetree patch (`firmware/apply_airband_devicetree.py`), then
(4) runs the full `HAVE_VIVADO=1` build with `GIT_HASH` exported so Vivado stamps
the commit hash into the bitstream (USERID + USR_ACCESS):

```bash
# on the build server — no manual pull needed; the script pulls both repos
bash ~/pluto-build/airband/firmware/build_firmware_full.sh   # ~20 min (see breakdown)
```

> **Push to `origin` first** — the server pulls from GitHub, so unpushed work
> won't be built. (HDL lives in the fork; `hdl/*.py` in *this* repo is the sim
> mirror only.) `firmware/build_bitstream.sh` is a faster host-Vivado inner loop
> for checking synthesis/timing, but it does **not** produce flashable images.

**Per-phase wall-clock** (measured on `xilinx-builder`, 32 vCPU; full clean
`HAVE_VIVADO=1` build). Use these to size waits so you don't poll more than
necessary — the bitstream is the only long pole:

| Phase (in order) | ~Time | Log marker to watch for |
|---|---|---|
| kernel `zImage` + U-Boot SPL | ~2 min | `Kernel: arch/arm/boot/zImage is ready` |
| `maia-httpd` Rust release build | ~4 min | `Compiling maia-httpd` … `Finished \`release\`` |
| `maia-wasm` web UI (wasm-pack) | ~3 min | `Compiling maia-wasm` / `Optimizing wasm` |
| buildroot rootfs + Vivado IP packaging (both configs) | ~4 min | `Building pluto project [...pluto_vivado.log] ...` |
| **Vivado pluto synth → place → route → bitstream** | **~7 min** | `write_bitstream completed successfully` |
| FSBL/`BOOT.BIN` + FIT packaging → artifacts | ~2 min | `== artifacts ==` then `Flash BOTH partitions` |
| **total** | **~20 min** | (first poll ≥ 15 min in) |

Produces in `plutosdr-fw/build/`:

| Artifact | Contents | Flash target / MTD |
|---|---|---|
| `boot.frm` / `boot.dfu` | `BOOT.BIN` = **FSBL + bitstream + U-Boot** | `mtd0` (DFU alt `boot.dfu`) |
| `pluto.frm` / `pluto.dfu` | FIT = kernel + **devicetree** + rootfs (+ bitstream copy) | `mtd3` (DFU alt `firmware.dfu`) |

> **CRITICAL — an HDL change MUST go through `build_firmware_full.sh`, and you
> flash BOTH partitions.** The PL bitstream + FSBL live in `BOOT.BIN` (`boot.*`,
> `mtd0`); the FIT (`pluto.*`, `mtd3`) carries a copy too, so flashing only one
> leaves stale gateware behind. **`git HEAD` showing your commit is not proof the
> bitstream contains it**, and the date inside `pluto.dfu` is only the `mkimage`
> repack time (`strings pluto.dfu | grep 20../../..`). The one trustworthy signal
> is the **commit hash baked into the bitstream** (see below). (The build now
> pulls + clones from committed git state and bakes that hash precisely because we
> once flashed a FIT repacked around a stale prebuilt XSA — clean-looking flash,
> bug unchanged. Other symptom of old PL + new FIT: airband registers alias to the
> control block, AXI-HP DMA hangs → watchdog hard-reset; old FSBL leaves
> `S_AXI_HP0` disabled.)

**Verify bitstream provenance via the baked-in commit hash.** Vivado stamps the
fork's short commit into the bitstream as **USERID + USR_ACCESS**
(`maia-hdl/projects/pluto/system_project.tcl`), and `build_firmware_full.sh`
prints an `OK`/`WARNING` line at the end comparing the embedded UserID to the
committed fork HEAD. To check by hand on the server:

```bash
FW=~/pluto-build/plutosdr-fw
BIT=$FW/maia-sdr/maia-hdl/projects/pluto/pluto.runs/impl_1/system_top.bit
git -C ~/pluto-build/airband/maia-sdr rev-parse --short=8 HEAD   # the commit you shipped
strings "$BIT" | grep -oiE 'UserID=[0-9a-f]+'                    # bare hex, must equal <that hash>
grep "write_bitstream completed successfully" \
  $FW/maia-sdr/maia-hdl/projects/pluto/pluto_vivado.log | tail -1
```

If the embedded UserID does **not** match HEAD, **do not flash** — the build ran
on stale source (commit not pushed/pulled) or the `GIT_HASH` env didn't reach
Vivado. A `.bit` holds config frames, not your netlist, so grepping signal names
(`grep cordic`) never works — the embedded UserID is the identity. USR_ACCESS
carries the same hash and is readable at runtime for on-device confirmation.

### Pluto+ variant (`TARGET=plutoplus`)

The **Pluto+** (the open `plutoplus/plutoplus` board: Gigabit Ethernet, microSD,
0.5 ppm VCTCXO) is the **same XC7Z010 die** as the ADALM-Pluto, just a different
package (`xc7z010clg400-1` vs `clg225-1`) with a different MIO pinout. Maia SDR
ships a `plutoplus` build target, and the airband receiver fits **identically**
(same LUT/FF/BRAM/DSP) — there is no new FPGA project and no resource re-fit.

Build and flash exactly like the ADALM-Pluto, but set `TARGET=plutoplus`:

```bash
# on the build server (pushes pulled, ~20 min)
TARGET=plutoplus bash ~/pluto-build/airband/firmware/build_firmware_full.sh
# artifacts: build/{boot,plutoplus}.{frm,dfu}
scp <server>:~/pluto-build/plutosdr-fw/build/{boot,plutoplus}.dfu firmware/build/
cd firmware/build
dfu-util -a boot.dfu     -D boot.dfu          # mtd0: bitstream + FSBL + u-boot
dfu-util -a firmware.dfu -D plutoplus.dfu     # mtd3: kernel + DT + rootfs
dfu-util -a firmware.dfu -e                   # leave DFU
```

What carries over automatically (no edit needed):

- **Bitstream:** `projects/plutoplus/system_project.tcl` builds the `clg400` part
  and (after this work) bakes the same USERID/USR_ACCESS commit hash; its
  `system_bd.tcl` just `source`s `../pluto/system_bd.tcl` (so the airband HP0
  `m_axi_airband` DMA is wired) under a `plutoplus` flag that adds Ethernet
  (MIO16-27 + MDIO52-53), microSD, and the USB-PHY reset on **MIO46**.
- **Devicetree:** `zynq-plutoplus-maiasdr.dts` `#include`s the shared
  `zynq-pluto-sdr-maiasdr.dtsi` that `apply_airband_devicetree.py` patches, so
  the `maia_sdr_airband@19000000` reserved region + rxbuffer node are present.
- **Init script:** `buildroot/board/plutoplus/S60maia-httpd` gets the same
  `--airband` auto-start and TX-quiet patches (the build script now uses
  `board/$TARGET`).

Pluto+-specific operational notes:

- **Jumper:** the Maia/airband firmware requires the USB-PHY-reset jumper at
  **URST↔MIO46** (MIO52 is taken by Ethernet MDIO). With the jumper at MIO52 the
  board only runs stock ADALM-Pluto firmware (no Ethernet).
- **Transport (Gigabit Ethernet, DHCP):** `maia-httpd` binds `0.0.0.0:30000`
  (and `:8000`), so the stream is reachable on the DHCP-assigned `eth0` address
  with no code change — `airband-reader <eth0-ip>:30000`. Configure the Ethernet
  IP via the `[USB_ETHERNET]` section of `config.txt` on the device (despite its
  name it controls the physical Ethernet port); leave it blank for a DHCP lease,
  which you can discover via mDNS/router/serial. Leave `ipaddrmulti` **disabled**
  (it conflicts with the Pluto+ Ethernet IP).
- **Reference (VCTCXO):** the Pluto+ has a 0.5 ppm VCTCXO, so it needs **no**
  per-unit bare-XO calibration. Re-apply the u-boot env after a `boot.dfu` flash
  with `firmware/pluto_setup_env.py --device plutoplus` (defaults the refclk
  override to 0 = nominal 40 MHz). Only set `--refclk-hz` if
  `diagnostics/measure_offset.py` shows it is worthwhile.
- **Buzz spur:** the 120 MHz reference-harmonic spur (`SPEC.md` §7) is a
  hardware/PCB property; the Pluto+'s shielding/cleaner supply may reduce the
  audible modulation, but re-run `diagnostics/wideband_spectrum.py` on the unit
  to confirm rather than assume.

### Running a build from an agent (launch detached + wait-and-check)

The build outlives the SSH session and far outlives a single tool-call timeout,
so **never run it in the foreground** and never sit in a tight poll loop (the
harness aborts long-lived local pollers, and SSH control sessions can drop —
neither affects the build because it runs under `nohup`). Pattern that works:

1. **Push first.** The script `git pull`s both repos itself at the start, so it
   builds your latest *pushed* commit — but it can't build what you haven't pushed,
   and it aborts on an uncommitted fork working tree. Push your HDL/script commits
   to `origin` before launching. (HDL lives in the fork; `hdl/*.py` in *this* repo
   is the sim mirror only.)
2. **Launch detached**, logging to a file, and immediately return:
   ```bash
   ssh administrator@10.0.16.36 'cd ~/pluto-build/airband && \
     nohup bash firmware/build_firmware_full.sh > ~/pluto-build/fwbuild.log 2>&1 & echo PID=$!'
   ```
3. **Do a one-shot smoke check** ~5 s later (`tail` the log) to confirm it
   started, then **stop polling**.
4. **Wait in coarse blocks** sized to the table above — first check at **~15 min**,
   then every few minutes. Each check is a *fresh, short* SSH (don't hold one
   open): `pgrep -f build_firmware_full.sh` for liveness +
   `tail ~/pluto-build/fwbuild.log`. Stable pings with no progress for >10 min
   past the expected phase ⇒ inspect `.../projects/pluto/pluto_vivado.log`.
5. **Done when** the build process is gone AND all four artifacts exist
   (`build/{boot,pluto}.{frm,dfu}`). Before flashing, confirm **(a) timing** —
   `grep "write_bitstream completed successfully" .../pluto_vivado.log` + the
   `Post Routing Timing Summary | WNS=` line ≥ 0 — **and (b) provenance**: the
   embedded UserID equals the committed fork HEAD (the build prints an `OK`/
   `WARNING` line; see "Verify bitstream provenance"). Both must pass.

### Flashing + first-boot expectations (for unattended reflash)

Enter DFU mode (`device_reboot sf` over ssh/serial, or power-cycle holding the
DFU button) until `dfu-util -l` shows `alt=0 boot.dfu` … `alt=1 firmware.dfu`,
then flash **both** partitions and detach:

```bash
scp administrator@10.0.16.36:'~/pluto-build/plutosdr-fw/build/{boot,pluto}.dfu' firmware/build/
dfu-util -a boot.dfu     -D firmware/build/boot.dfu     # mtd0 (~2 s)
dfu-util -a firmware.dfu -D firmware/build/pluto.dfu    # mtd3 (~100 s, 19 MB)
dfu-util -a firmware.dfu -e || true                     # detach -> boot (plain `-e` errors with >1 alt)
```

> **MANDATORY after any `boot.dfu` flash:** it re-defaults the u-boot env, wiping
> `usb_ethernet_mode=ncm` (→ macOS loses `usb0`, "web is dead") and the AD9364
> attrs. Re-apply over serial — `.venv/bin/python firmware/pluto_setup_env.py`
> (`--check` to audit). See [u-boot environment](#critical-the-u-boot-environment-reset-by-every-bootdfu-flash).
> `pluto.dfu`-only reflashes don't touch the env.

First boot after a flash is **slow**: the device answers ping within ~30 s
(kernel + USB-ethernet gadget up) but `sshd`/`maia-httpd` (`:22` / `:30000`)
can take **several minutes** more. Stable pings with **0 % loss** mean no
watchdog reset loop (the old airband failure mode dropped pings every ~10–30 s).
Verify over SSH (`sshpass -p analog ssh root@192.168.2.1`) — or over serial
(`firmware/pluto_setup_env.py` logs in the same way): clean `dmesg`
(no `watchdog`/`panic`), `maia_sdr_airband@19000000` reserved node present,
`maia-httpd` running, then on the host run `airband-reader 192.168.2.1:30000`
and confirm **~15625 sps/channel** (= 14 MHz / 128 / 7) — half that (7813) means
an old/half-rate bitstream is still loaded.

### CRITICAL: the AD9361 front-end is locked read-only under `--airband`

**Symptom (the "all channels on noise" saga):** the stream runs at the correct
rate and 0 drops, but every channel is near-silent noise — *even though the band
is active*. The Maia waterfall (`http://<pluto>:8000`) shows lots of signals, but
they're at **2.4 GHz / 61.44 Msps**, not the airband window.

**Cause:** the AD9361 is a **single shared front-end**. The airband task programs
it to **123.438 MHz / 14 Msps** at startup, but the Maia web UI re-applies its own
settings: on every page load `maia-wasm`'s `preferences.apply()` re-`PATCH`es each
stored AD9361 field, and the stored *defaults are 2.4 GHz LO / 61.44 Msps*. That
retunes the radio off-band, so the channelizer NCOs (baked for 14 Msps) all land
on noise.

**Fix (shipped, fork commit `aa9364e`):** when `maia-httpd` runs with `--airband`
the front-end is **locked read-only** — `/api/ad9361` `PATCH`/`PUT` is a no-op
(returns current values), `/api` exposes `"airband": true`, and the web UI
disables the RX freq / Fs / bandwidth / gain / AGC controls. The airband task
still configures the AD9361 directly on the iio device, so it is unaffected. This
is **software-only** (FIT-only build, reflash `pluto.dfu`; no bitstream change).

**Diagnose** (if you see noise on a build *without* the lock, or after manual iio
pokes): check the live front-end and re-assert it if it drifted off-band.

```sh
# on the device
P=/sys/bus/iio/devices/iio:device0
cat $P/out_altvoltage0_RX_LO_frequency   # must be 123438000, not 2399999998
cat $P/in_voltage_sampling_frequency     # must be 14000000, not 61440000
# re-assert (Fs and bandwidth before LO):
echo 14000000  > $P/in_voltage_sampling_frequency
echo 14000000  > $P/in_voltage_rf_bandwidth
echo 123438000 > $P/out_altvoltage0_RX_LO_frequency
```

Or from the host: `curl -s http://<pluto>:8000/api | grep -o '"airband":[a-z]*'`
should print `"airband":true`, and the `ad9361` block should read
`rx_lo_frequency 123438000 / sampling_frequency 14000000`.

### "Receiver works but no audio" — it's a level problem, not the DSP

**Symptom:** the stream runs (correct rate, 0 drops) and the channel carrying a
known always-on signal (e.g. AWOS on 118.050) is the loudest in `airband-reader`
stats, but it's still near-silent when you play/record it.

The DSP chain (channelizer → `|I+jQ|` → DC block → audio CIC) is unity-ish gain
and bit-exact in sim — it does not amplify. So the 24-bit audio sample only ever
gets as large as the demodulated signal, which for weak airband AM is **tens of
LSB** (i.e. ~ −90 to −100 dBFS at 24-bit). Two things were eating it:

1. **AGC starves weak channels.** `slow_attack`/`fast_attack`/`hybrid` set RF
   gain from the *wideband* (14 MHz) power and settle to ~48–57 dB, which leaves
   the weak narrowband carrier tiny. Measured ch0 raw 24-bit peak:

   | gain setting | RF gain | ch0 peak |
   |---|---|---|
   | slow_attack | 48 dB | ~40 |
   | fast_attack | 55 dB | ~54 |
   | hybrid | 57 dB | ~71 |
   | manual 64 dB | 64 dB | ~154 |
   | **manual 71 dB** | **71 dB** | **~280** |

   Fixed **manual gain near max (71 dB)** wins by ~5× for weak-signal recovery
   and is the default. Caveat (found later, see `firmware/diagnostics/`): at
   strong-signal sites 71 dB clips the *wideband* ADC (~15% of samples); lower
   `gain_db` if you hear distortion. Note this does not remove the fixed RF-spur
   "buzz". This is the default in `firmware/airband.json` and the `maia-httpd` built-in
   (`AirbandConfig::default`). To apply without reflashing, drop the config on the
   device and restart:

   ```sh
   cat firmware/airband.json | ssh root@192.168.2.1 'cat > /root/airband.json'
   ssh root@192.168.2.1 /etc/init.d/S60maia-httpd restart
   ```

   (The Pluto's dropbear has no `sftp-server`, so `scp` fails — pipe over `ssh`,
   or `scp -O`.)

2. **The host reader needs makeup gain.** `airband-reader --shift` scales the
   24-bit sample into 16-bit; it is **signed** — positive right-shifts
   (attenuate), negative left-shifts (gain). The default is `-6` (≈ +36 dB). With
   manual 71 dB + `--shift -6`, ch0 lands at ~ −19 dBFS, no clipping, ~60 % of
   energy in the 300–3400 Hz voice band, ~22 dB above the idle-channel floor.
   `airband-listen` has the same need: its `--gain` default is 3000 (adjust live
   with `+`/`-`).

To confirm audio is *real voice* (not a carrier spike), record a few seconds and
check the voice-band energy fraction, e.g. with numpy on `chNN.wav`: a live AWOS
channel sits ~20+ dB over an idle channel with most energy in 300–3400 Hz.

### CRITICAL: the u-boot environment (reset by every `boot.dfu` flash)

The Pluto keeps several device-specific settings in the **u-boot environment**
(`mtd1`). Maia/airband needs four of them; if any is wrong the device looks
"broken" in a confusing, non-obvious way:

| Env var(s) | Required value | If wrong |
|---|---|---|
| `usb_ethernet_mode` | `ncm` (macOS/iOS; `ecm` Android, `rndis` Windows) | host gets no `usb0` IP → **web/SSH look dead** (the "networking is broken" trap) |
| `attr_name` / `attr_val` / `mode` | `compatible` / `ad9364` / `1r1t` | transceiver mis-probes (not AD9364) |
| `ramboot_verbose` / `qspiboot_verbose` / `qspiboot` | each contains `uio_pdrv_genirq.of_id=uio_pdrv_genirq` | no `/dev/uio0` → **`maia-httpd` won't start**, `:8000`/`:30000` never open |

> **Flashing `boot.dfu` (mtd0) RESETS this environment.** The new u-boot's env
> version differs from `mtd1`, so on first boot u-boot overwrites `mtd1` with its
> **compiled-in defaults**. Those defaults keep the `uio_pdrv_genirq.of_id` boot
> args and `mode=1r1t`, but **drop** `usb_ethernet_mode=ncm` (reverts to `rndis`)
> and the AD9364 `attr_name`/`attr_val`. So **every HDL/bitstream reflash silently
> breaks macOS networking and the transceiver mode** until you re-apply them. This
> is *not* true of `pluto.dfu`-only (FIT) reflashes — those leave `mtd1` alone.

**Fix — one command, over serial** (the USB-ethernet path is down right after a
boot flash). [`firmware/pluto_setup_env.py`](firmware/pluto_setup_env.py) logs in
over serial and applies/verifies all of the above via `fw_setenv` (the
[officially recommended](https://maia-sdr.org/installation/) way — it never
reflashes the bootloader/env partition). It's idempotent and reboots only if it
changed something:

```bash
.venv/bin/python firmware/pluto_setup_env.py            # apply (usb=ncm) + reboot
.venv/bin/python firmware/pluto_setup_env.py --check    # read-only audit
.venv/bin/python firmware/pluto_setup_env.py --usb-mode ecm   # Android instead
```

Manual / browser alternative: the official
[ADALM-Pluto Setup Utility](https://maia-sdr.org/pluto-setup-tool/) (Web Serial).
Per-variable failure signatures and manual `fw_setenv` strings (in
`plutosdr-fw/build/uboot-env.txt`):

- **`usb_ethernet_mode` wrong** → host gets no `usb0` IP; device is fine on serial
  (`maia-httpd` up, `:8000`/`:30000` listening) but `192.168.2.x` is unreachable
  from the Mac (pings to `.2.1` leak onto the LAN — looks like an IP collision,
  isn't). Built from `/etc/init.d/S23udc`; default `rndis`. macOS/iOS=`ncm`,
  Android=`ecm`, Windows=`rndis`.
- **`uio_pdrv_genirq.of_id` missing** → no `/dev/uio0`, `maia-httpd` aborts with
  `failed to open maia-sdr UIO` and `:30000` never opens. Lives in the
  `ramboot_verbose`/`qspiboot_verbose`/`qspiboot` bootargs. Check:
  `grep -o 'uio_pdrv_genirq.of_id=[^ ]*' /proc/cmdline` (empty == bug).
- **AD9364 attrs wrong** → transceiver mis-probes. Verify after boot:
  `dmesg | grep -i ad936` shows `ad9361_probe : enter (ad9364)` /
  `probed ADC AD9364 as MASTER`. (The IIO node is always `ad9361-phy` and the FDT
  board string still says `AD9363` — both cosmetic.)

> **Do NOT reflash `uboot-env.dfu` (`mtd1`, `alt=3`) to fix the env**: the factory
> u-boot rejects that image format and falls back to its built-in default (no
> `of_id`). Use `fw_setenv` / the script, which write the canonical `mtd1` layout.

> **Not the cause — don't chase:** `dmesg: maia_sdr: no symbol version for
> module_layout` is the **normal** ADI out-of-tree taint, not a kernel mismatch
> (device `uname -r` == `6.1.0-271887-g9670e17f01f1-dirty`).

**Reboot shortcuts** (`/usr/sbin/device_reboot`): `sf` → Serial-Flash DFU mode
(no power-cycle needed to reach `dfu-util`); `break` → halt at u-boot prompt;
`ram` / `verbose` / `reset`.

### Memory map (why airband is at `0x1900_0000`)

The airband DMA ring must live in a devicetree `no-map` reserved region that does
**not** collide with the kernel CMA pool (the original `0x1f00_0000` choice
bricked the device). It is now carved out of the recorder region:

- recorder: `0x0100_0000 – 0x1900_0000`
- airband ring: `0x1900_0000 – 0x1a00_0000` (16 MiB)

These are set in `maia-sdr/maia-hdl/maia_hdl/config.py` (`airband_address_range`,
`recorder_address_range`) and mirrored by `apply_airband_devicetree.py`. The DMA
start/end addresses are **baked into the bitstream** (`DmaStreamWrite`), so
changing them requires a bitstream rebuild (`boot.dfu`), not just a DT edit.

### Flash both partitions over DFU

Put the Pluto in DFU mode (`device_reboot sf`), then flash **both** images
(order: boot first) and re-apply the u-boot env:

```bash
dfu-util -a boot.dfu     -D plutosdr-fw/build/boot.dfu
dfu-util -a firmware.dfu -D plutosdr-fw/build/pluto.dfu
dfu-util -a firmware.dfu -e          # leave DFU (plain `dfu-util -e` errors: >1 alt)
# boot.dfu reset the u-boot env -> re-apply over serial (see u-boot env section):
.venv/bin/python firmware/pluto_setup_env.py
```

## Reproduce the build pipeline from scratch (end-to-end)

The whole flow, assuming the server is [bootstrapped](#bootstrap-the-server-from-scratch)
and Vivado + the `vivado2023_2` volume are in place:

```bash
# --- on the build server ---------------------------------------------------
# Push your commits to origin first. The build pulls both repos itself, builds
# only from committed state, and bakes the fork commit hash into the bitstream.
# Produces plutosdr-fw/build/{boot,pluto}.{frm,dfu}; ~20 min (see phase table).
bash ~/pluto-build/airband/firmware/build_firmware_full.sh

# --- copy artifacts to the machine with the Pluto -------------------------
scp <server>:~/pluto-build/plutosdr-fw/build/{boot,pluto}.dfu firmware/build/

# --- flash (Pluto in DFU mode: `device_reboot sf`) ------------------------
cd firmware/build
dfu-util -a boot.dfu     -D boot.dfu      # mtd0: bitstream + FSBL + u-boot
dfu-util -a firmware.dfu -D pluto.dfu     # mtd3: kernel + DT + rootfs
dfu-util -a firmware.dfu -e               # leave DFU (plain `-e` errors: >1 alt)

# --- if boot.dfu was flashed, re-apply the wiped u-boot env (over serial) -
cd ../.. && .venv/bin/python firmware/pluto_setup_env.py
```

`build_firmware_full.sh` is the only builder (there is no fast FIT-only script —
that path caused stale-gateware flashes and was removed). What to flash:

| Change | Flash | Then |
|---|---|---|
| Any HDL / block-design / address-map change | `boot.dfu` **and** `pluto.dfu` | **`pluto_setup_env.py`** (boot flash wiped the env) |
| Software only (`maia-httpd` / devicetree / channel plan / init script) | `pluto.dfu` only (bitstream logic unchanged) | nothing (env untouched) |

### Verify on hardware

After boot (default `192.168.2.1`, ssh `root` / `analog`):

```bash
# clean boot + airband reserved-memory node at the relocated address
dmesg | grep -iE 'cma|maia_sdr_airband|panic|watchdog'
ls /proc/device-tree/reserved-memory/ | grep airband      # maia_sdr_airband@19000000

# UIO bootarg present + UIO device created (else maia-httpd can't start; see the
# "uio_pdrv_genirq.of_id" section above for the fw_setenv fix)
grep -o 'uio_pdrv_genirq.of_id=[^ ]*' /proc/cmdline         # must be non-empty
ls /sys/class/uio/                                          # must list uio0

# receiver auto-started with --airband, TCP stream listening
ps w | grep '[m]aia-httpd'                                  # ... --airband
netstat -ltn | grep :30000

# airband register page decodes (not aliased): version magic, then airband regs
busybox devmem 0x7C400000                                   # 0x6169616D ("maia")
```

Then from the host, confirm a live, gap-free 21-channel stream:

```bash
host/airband-reader/target/release/airband-reader 192.168.2.1:30000
# expect: 21 channels, ~15625 sps each, 0 dropped samples
```

A healthy result is: clean boot (no panic/watchdog), `--airband` in the running
`maia-httpd`, `:30000` listening, and the reader showing all 21 channels with no
drops. (Audio is near-silent without a real airband signal on the antenna.)

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

## Status / bring-up

- ~~Add `~/.local/bin` to your shell PATH (see above) for convenient access.~~
- ~~Stand up the x86-64 build server and do a clean from-source bitstream build
  of unmodified Maia SDR.~~ **Done** — server provisioned, Vivado 2023.2
  installed, bitstream built (timing met), base PL usage measured
  (LUT 5416/17600, FF 6493/35200, BRAM 29/60, DSP 18/80). See `PROGRESS.md`.
- ~~Flash the baseline Maia image to a Pluto.~~ **Done** — baseline Maia verified
  on hardware.
- ~~Build the airband bitstream (channelizer + cyclic airband DMA) and integrate
  into `maia_sdr`.~~ **Done** — `pluto-airband` fork, timing met.
- ~~First airband firmware flash bricked the device (CMA collision at
  `0x1f00_0000`).~~ **Fixed** — airband ring relocated to `0x1900_0000`; cause
  confirmed with a minimal build.
- ~~Airband enable caused watchdog resets / no DMA after flashing only
  `pluto.dfu`.~~ **Root-caused + fixed** — `BOOT.BIN`/`mtd0` (old bitstream + FSBL
  with HP0 disabled) was never reflashed; the full `HAVE_VIVADO=1` build
  (`boot.dfu` + `pluto.dfu`) was flashed and the receiver now runs.
- ~~Verify the airband register page + cyclic DMA on hardware.~~ **Done
  (2026-06-15)** — register page decodes correctly (no aliasing), cyclic DMA
  advances, all 21 channels stream gap-free over TCP `:30000` (per-channel seq
  delta = 1), no watchdog reset.
- ~~Enable airband auto-start.~~ **Done** — `S60maia-httpd` patched to launch
  `maia-httpd --airband`; verified the receiver comes up automatically on boot.

**The end-to-end receiver is live on hardware.** Remaining work is signal-quality
tuning (antenna, gain/AGC, `--shift`) and the LiveATC feeder integration.
