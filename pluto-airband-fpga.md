# Pluto FPGA Multichannel Airband Receiver — Project Handoff

## Purpose of this document

This is a design-and-environment handoff for a Claude Code agent (and the human
operating it) to build an FPGA-based multichannel airband (VHF aviation voice)
receiver on the Analog Devices ADALM-Pluto SDR. The end goal is a high-quality
audio feeder for **liveatc.net**: monitor *dozens* of airband channels
simultaneously and stream clean per-channel audio to liveatc's Icecast servers.

The reader (the agent) should treat this as the authoritative statement of
*what* we are building and *why*, plus the toolchain it will operate in. It is
deliberately explicit about reasoning so the agent does not "optimize" the
architecture back into a dead end we have already ruled out.

**Baseline being replaced:** The operator's *current* feeder uses an SDRplay
**RSPduo**, not a bank of RTL-SDR dongles. The RSPduo is a good 14-bit receiver,
so this project is **not** about escaping a noisy front end. The RSPduo fails for
**coverage** reasons: its dual-tuner architecture swaps between receivers
unreliably (hangs over time, no clean workaround) and leaves a **dead zone
between the two tuners**, and its ~10 MHz contiguous window **cannot span the
operator's required channel spread** — several important channels sit at the very
edge of, or outside, that window. The quality problem is therefore
**coverage-driven, not DSP-driven.** The agent should not expect "better
demodulation" alone to be the win; the win is **one contiguous wideband capture
of the entire voice band from a single tuner, with no seam and no swap logic.**

---

## 1. Goal and success criteria

**Primary goal:** Replace the operator's current SDRplay RSPduo feeder with a
single Pluto whose FPGA channelizes and demodulates many airband voice channels,
shipping only demodulated audio off-device.

**Why:** The operator is already an authorized liveatc feeder. The RSPduo cannot
provide **contiguous** coverage of all required channels: its dual-tuner swapping
is unreliable and leaves a dead zone between tuners, and its ~10 MHz window is too
narrow for the operator's channel spread (several channels at the edges). The
motivation is **coverage**, not front-end quality — the Pluto's AD936x covers the
entire ~19 MHz voice band in one contiguous, single-tuner capture. (See "Baseline
being replaced" above.)

**Concrete success criteria:**

1. Pluto firmware that exposes N (target: 24–48) independently-tuned airband
   channels, each AM-demodulated to mono PCM audio (8 ksps or 16 ksps, 16-bit).
2. Demodulated audio reaches a host (a Raspberry Pi at the antenna tower) over
   the Pluto's USB link, well within USB 2.0 bandwidth.
3. A small Pi-side daemon encodes each channel and pushes it to liveatc's
   Icecast as a source client.
4. Stable for continuous (24/7) operation at an antenna site.

**Non-goals:** Replicating RTLSDR-Airband's CPU FFT-channelizer approach;
transmitting; supporting non-airband modes beyond optionally NFM. We are NOT
porting the CPU algorithm — we are doing the equivalent work in fabric.

---

## 2. Key facts and constraints (do not re-derive or override these)

### 2.1 Airband is AM
VHF airband voice (118–136.975 MHz) is **amplitude modulation** (A3E DSB full
carrier), on 25 kHz (or 8.33 kHz) channel spacing. The demodulator is therefore
an **AM envelope detector** (magnitude of the complex baseband), *not* an FM
arctangent-differentiator. This makes the per-channel DSP simpler than most FPGA
FM-demod literature. NFM is an optional secondary mode (RTLSDR-Airband supports
it as a per-channel option); if added later it uses the CORDIC
arctan-differentiator structure.

### 2.2 The interface-bandwidth argument is load-bearing
This is the reason demod MUST happen on the FPGA. Do not propose shipping
wideband IQ to the host.

- Pluto host link is **USB 2.0**: ~480 Mbps theoretical ≈ 60 MB/s, realistically
  ~40–50 MB/s after overhead and the IIO daemon's buffering behavior.
- Wideband IQ to cover the airband: complex 16-bit = `sample_rate × 4 bytes/s`.
  - 20 Msps → 80 MB/s (does NOT fit USB 2.0).
  - 61.44 Msps (Pluto max) → ~246 MB/s (does NOT fit).
- Per-channel narrowband IQ (e.g. 16 ksps complex) ≈ 64 KB/s/channel; dozens of
  channels ≈ 1.5–3 MB/s (fits USB) — but this keeps demod on the host CPU, which
  is the problem we are escaping, and fights RTLSDR-Airband's data model.
- **Demodulated audio**: 8 ksps mono 16-bit = 16 KB/s/channel; 48 channels ≈
  <1 MB/s before compression. Trivial over USB. **This is the chosen path.**

Conclusion: **channelize + demodulate in the FPGA, ship only audio.**

### 2.3 RTLSDR-Airband cannot be reused downstream of FPGA demod
RTLSDR-Airband's pipeline is strictly **IQ-in → (FFT channelize) → demodulate →
audio-out**. Every input it supports (native RTLSDR, MiriSDR, SoapySDR) delivers
*IQ*; the SoapySDR driver negotiates IQ sample formats (CS16/CS8). There is **no
input path that ingests pre-demodulated PCM audio.** Demodulation is the program's
core function.

Therefore: once the FPGA produces audio, RTLSDR-Airband has no role. We write a
small Pi-side streamer instead. (RTLSDR-Airband would only be reusable if we let
*it* demodulate — i.e. ship it IQ — which §2.2 forbids at this channel count.)

### 2.4 Hardware target (confirmed — not open questions)
- Pluto SoC: **Xilinx Zynq-7010** (Z-7010): dual ARM Cortex-A9 (PS) + Artix-7
  class programmable logic (PL).
- RFIC: **genuine ADI AD9363 from a genuine ADALM-Pluto.** The following are
  established facts for *this specific unit*, not assumptions to verify:
  - The AD9363 and AD9364 are the **same silicon die.** The operator has written
    the "unlock" register, so the part operates with **AD9364-class range and
    wide contiguous bandwidth** (the practical difference the AD9364 requires is
    a more accurate clock source).
  - The unit came from the ADI business unit that originally developed the chip
    (codename **Catalina**) and **already has the more precise clock source**
    installed. So the clock-accuracy prerequisite for AD9364-class operation is
    met.
  - **Sensitivity / noise figure at 120 MHz are valid and equal to the
    datasheet's 800 MHz characterization point.** The datasheet only characterizes
    sensitivity at 800 MHz / 2.4 GHz / 5.5 GHz because those were the lead
    customer's operating frequencies — they are *not* performance boundaries.
    Per the operator (who has authoritative knowledge of the part), the 800 MHz
    sensitivity and noise numbers hold at the airband. **No empirical sensitivity
    check is required as a project gate.**
  - RAM: standard Pluto. (If RAM buffer sizing becomes relevant, confirm, but it
    is not a tuning-range or bandwidth concern.)
- **Net effect:** the front end is known-good. Wide contiguous capture of the
  full ~19 MHz voice band (118–136.975 MHz) with margin is available, with
  characterized sensitivity. There are **no open hardware-capability questions.**

### 2.5 Capture window must cover all channels with edge margin
Because the entire purpose is contiguous coverage (§ baseline above), **every
target channel must fit comfortably *inside* the capture window with margin from
the band edges.** Channels near the front-end filter skirts or the digital filter
roll-off degrade — this edge degradation, plus the inter-tuner dead zone, is
exactly what the RSPduo could not avoid. The Pluto capture must be wide enough,
and centered such, that no channel of interest sits in a roll-off region.

---

## 3. Prior art — build on this, don't reinvent

### 3.1 Maia SDR (PRIMARY foundation)
Open-source FPGA-based SDR project focused on the Pluto, by Daniel Estévez.
Repo: https://github.com/maia-sdr/maia-sdr • Site: https://maia-sdr.org

Components:
- **maia-hdl** — the FPGA design, written in **Amaranth** (Python HDL).
  Synthesizes to Verilog, packaged as a Vivado IP core via TCL (same build
  pattern ADI uses for the stock Pluto bitstream). Includes a low-resource
  pipelined FFT, high-throughput DMAs, and a **DDC (digital downconverter)**.
- **maia-httpd** — async **Rust** app on the Zynq ARM (PS). REST API + WebSocket.
  Reaches the FPGA IP core via a **UIO device** (mmap registers + interrupts).
  Register map auto-generated: Amaranth emits an **SVD** file → `svd2rust` →
  safe Rust API, keeping HW/SW register definitions consistent.
- **maia-wasm** — Rust/WASM web UI (not needed for our headless feeder, but
  present).
- **maia-kmod** — kernel module managing cache coherency for FPGA↔CPU RAM
  buffers (DMA).

**The DDC is the key building block.** It uses an NCO+mixer to shift a slice of
spectrum to baseband, then a decimating low-pass filter — exactly the per-channel
front end we need. It already reaches audio-adjacent rates (demonstrated at
48 ksps). Estévez's published roadmap explicitly lists: FPGA receivers for
analog voice (SSB/**AM**/FM), network streaming of DDC output, and **multiple
DDCs fitting in the Pluto FPGA** for independent simultaneous tuning. We are
extending the grain of this design, not fighting it.

Version pins (current as of this writing): maia-hdl v0.6.x targets **Amaranth
0.5.2**; the adi-hdl submodule is on the **Vivado 2023.2** branch. Verify against
the repo's CHANGELOG before building.

### 3.2 Supporting references
- ADI Pluto HDL project + "Build an HDL project" user guide (wiki.analog.com,
  analogdevicesinc.github.io/hdl).
- `analogdevicesinc/plutosdr-fw` — official firmware build (submodules: linux,
  hdl, buildroot, u-boot). Maia maintains a fork with prebuilt images.
- `mmaroti/plutosdr-dev` — edit/compile the FPGA project in the Vivado GUI.
- `timcardenuto/testPlutoSDR` — hands-on walkthrough of inserting a custom DSP
  module into the AD9361 RX chain (conceptually our insertion point).
- `ekigwana/adi_dmac_iio_client` — custom IIO driver + example streaming app for
  a custom HDL `axi_dmac` module (template for surfacing our audio to userspace).
- FM-demod (only if NFM added later): CORDIC arctan-differentiator is the
  consensus best feed-forward structure; Xilinx CORDIC IP provides both
  vectoring (magnitude, for AM) and arctan modes.

---

## 4. System architecture

```
        Antenna + airband BPF / FM-notch filter
                     │
                     ▼
   ┌──────────────────────────────────────────────┐
   │  ADALM-Pluto                                   │
   │                                                │
   │  AD936x RFIC ──IQ──► PL (FPGA, Zynq-7010)      │
   │                       │                         │
   │   ┌───────────────────┴────────────────────┐   │
   │   │  Channelizer: N channel chains           │  │
   │   │  (time-multiplexed / polyphase DDC)      │  │
   │   │   per channel: NCO+mix → decim LPF       │  │
   │   │      → AM envelope detect (|I+jQ|)       │  │
   │   │      → audio decimate/filter (8/16 ksps) │  │
   │   │      → [optional power squelch]          │  │
   │   └───────────────────┬──────────────────────┘  │
   │                        │ framed multi-channel    │
   │                        ▼ audio                    │
   │   axi_dmac ──► RAM buffer ──► IIO/UIO            │
   │                        │                         │
   │  ARM (PS) Linux: maia-httpd (control) +          │
   │           audio mux/exposure                     │
   └────────────────────────┬─────────────────────────┘
                             │ USB 2.0 (audio only, <1 MB/s)
                             ▼
   ┌──────────────────────────────────────────────┐
   │  Raspberry Pi (at tower)                       │
   │   libiio reader → per-channel PCM              │
   │   → [squelch/AGC if not done in FPGA]          │
   │   → MP3 encode (LAME)                           │
   │   → Icecast source client (libshout)            │
   └────────────────────────┬─────────────────────────┘
                             │ Internet
                             ▼
                    liveatc.net Icecast servers
```

### 4.1 Where the demod boundary sits
**On the FPGA.** This is forced by §2.2 and is the whole point. The Pi receives
already-demodulated audio and does only encode + stream (+ optional
squelch/AGC/level if we choose to keep those off-FPGA).

### 4.2 Channelizer scaling — the load-bearing feasibility question
We need *dozens* of channels in a small (Z-7010) FPGA. Do NOT instantiate dozens
of fully independent DDCs naively; check resource budget first. Two scaling
strategies, likely combined:

- **Time-multiplexing:** the FPGA clock (62.5–100 MHz) is enormously faster than
  the per-channel audio rate. At 8 ksps out and 62.5 MHz clock there are ~7,800
  cycles per output sample, so one physical DDC datapath can iterate across many
  channels. This is almost certainly how dozens of channels fit.
- **Polyphase / shared CIC front-end:** a shared decimation front-end feeding
  per-channel fine-tuning, akin to a polyphase channelizer, amortizes filtering
  cost across channels.

**First real engineering task:** determine, via resource estimation and
maia-hdl's existing DDC, whether a time-multiplexed/polyphase channelizer for
24–48 AM channels fits the Z-7010 (LUTs, DSP48E1s, BRAM). This gates the project.

### 4.3 Audio transport / framing
Dozens of low-rate audio streams must be multiplexed into the DMA buffer with
**per-channel framing** so ARM + Pi can demux. Decide the framing format early
(e.g. interleaved fixed-size frames with a channel index / sample counter). This
affects both HDL and the maia-httpd-side exposure. Surface to userspace via
IIO/UIO following the `ekigwana/adi_dmac_iio_client` pattern.

### 4.4 Squelch / AGC placement (decision needed)
- **In FPGA:** cheap per-channel power threshold; avoids DMAing/streaming dead
  air. More HDL logic.
- **On Pi/ARM:** easier to tune/iterate in software; more data moved.
For a feeder, a hybrid is reasonable: coarse power-squelch gate in FPGA, finer
AGC/level on the Pi. Default recommendation: start with squelch+AGC on the Pi
(faster iteration), move to FPGA only if bandwidth or quality demands it.

---

## 5. Development stack

The architecture is **develop on macOS (Apple Silicon), build on x86 Linux**,
because **Vivado is x86-64 only** (no native macOS/ARM build, and it is required
for synthesis + bitstream). Everything else runs natively on ARM.

### 5.1 Build machine (x86-64 Linux) — REQUIRED for Vivado
- **Host OS:** Ubuntu Server 22.04 LTS (Vivado-validated, clean Docker host,
  headless). 24.04 is fine purely as a Docker host. May be a VM on a beefy x86
  server — allocate **≥8 vCPU, ≥32 GB RAM** (16 GB swaps painfully even for the
  small 7010), **~100 GB disk**. No USB passthrough needed (never touches Pluto).
- **Docker Engine** — the only hard host requirement.
- **`ghcr.io/maia-sdr/maia-sdr-devel`** — dev container with OSS CAD Suite
  (Yosys), Icarus Verilog, and all prereqs to run Vivado 2023.2.
- **Vivado 2023.2 ML Standard** (FREE edition covers Z-7010) installed into a
  Docker **volume** mounted at `/opt/Xilinx` (install once via the Xilinx
  installer run inside the container with X11 forwarded; the volume persists it).
  Requires a free AMD account to download.
- **`ghcr.io/maia-sdr/cross-armv7-unknown-linux-gnueabihf-maia-sdr`** — `cross`
  image to build the ARM-side Rust app with the same Buildroot Linaro toolchain
  as the Pluto firmware.

The Maia Docker images exist precisely to pin all dependencies — prefer them over
hand-assembling the toolchain (see `plops/build_pluto_firmware` for how painful
the manual path is).

### 5.2 Development machine (macOS, Apple Silicon) — native, no emulation
- **Git** + clones of `maia-sdr` (recursive — adi-hdl submodule on Vivado 2023.2
  branch) and the Maia fork of `plutosdr-fw`.
- **Python + Amaranth** (HDL authoring; match maia-hdl's Amaranth version, 0.5.2).
  Use a venv/conda.
- **Icarus Verilog + cocotb** (Homebrew `icarus-verilog`; cocotb via pip) — the
  fast local iteration loop. maia-hdl uses cocotb+Icarus for testbenches; verify
  demod logic on the Mac in seconds without invoking Vivado.
- **Rust toolchain** (rustup) — for editing maia-httpd, `cargo check`, unit
  tests. Final ARM binary is produced by the server's `cross` image.
- **`libiio` / `iio-utils` + `dfu-util`** (Homebrew) — talk to and flash the
  Pluto over USB directly from the Mac.
- Editor with Python/Rust/Verilog support.

### 5.3 Why this split works
- Amaranth, cocotb/Icarus simulation, and Rust editing are all native aarch64 —
  the bulk of day-to-day work needs no x86.
- Only **synthesis + bitstream + final firmware assembly** need x86 (slow, batch;
  tens of minutes). That is the one step shipped to the server.
- **Flashing is separate from building:** copy the finished firmware artifact
  (`.frm`/`.dfu`) to the Mac (or the Pi), put Pluto in DFU mode over USB, flash
  with `dfu-util`. So the build VM never needs USB to the Pluto — the
  "VMs can't pass USB" issue never bites.

> Note: **Parallels/VMware on Apple Silicon only run ARM guests** (virtualization,
> not emulation), so an "Ubuntu VM on the Mac" does NOT provide an x86 Vivado
> environment. The only on-Mac route to Vivado is Rosetta-emulated x86 Linux
> (`ichi4096/vivado-on-silicon-mac`) — slow and not recommended when an x86 build
> box is available. Use the x86 server.

### 5.4 Production host (Raspberry Pi at tower) — separate from build/dev
- Pi OS (64-bit recommended).
- **`libiio`** to read demodulated audio from the Pluto over USB.
- **LAME** (MP3 encode) + **libshout** / `python-shout` (Icecast source client).
- A small custom daemon (Python or C) tying read → [squelch/AGC] → encode →
  Icecast. One Icecast mountpoint per channel (or per liveatc requirement).
- Optionally a local Icecast server for staging/monitoring; for production point
  the source client at liveatc's server with operator-issued credentials.

---

## 6. Build & deploy workflow

1. **Author + simulate (Mac):** write Amaranth modules; run cocotb/Icarus
   testbenches locally; iterate until DSP is correct. No server needed.
2. **Sync to server:** git push (or rsync / SSHFS mount). Maia container mounts
   the repo dir into `/hdl`.
3. **Build (server, SSH):** run the containerized flow — maia-hdl elaborates
   Amaranth→Verilog→Vivado IP core (TCL); Vivado synth+impl→bitstream; `cross`
   builds maia-httpd; plutosdr-fw assembles a flashable firmware image.
4. **Flash (Mac or Pi):** copy firmware back; DFU over USB; `dfu-util`.
5. **Test on hardware:** configure DDCs and pull audio via libiio over USB/Eth.

For tight inner-loop dev, an **SSHFS mount** of the server build dir (or the Mac
repo into the VM) avoids constant git round-trips; keep git for anything durable.

---

## 7. Concrete task sequence for the agent

Order matters — each step de-risks the next.

1. **Toolchain bring-up:** stand up the x86 build server (Docker + Vivado 2023.2
   in volume + Maia devel/cross images). Milestone: a clean **from-source
   bitstream build of unmodified Maia SDR**. This proves the whole toolchain
   end-to-end and is the real first milestone.
2. **Mac dev env:** Python+Amaranth, Icarus+cocotb, Rust, libiio+dfu-util.
   Milestone: run maia-hdl's existing test suite locally.
3. **Flash baseline:** put unmodified Maia SDR on the Pluto; exercise the DDC
   live to understand its behavior and limits.
4. **Channelizer feasibility (GATE):** estimate Z-7010 resource budget for a
   time-multiplexed/polyphase DDC channelizer at 24–48 AM channels. If it does
   not fit, redesign (fewer channels per bitstream, multiple captures, etc.)
   before writing more. (Note: the *RF* side is not a gate — see §2.4. The only
   feasibility gate is FPGA resource fit.)
5. **AM demod block:** implement an AM envelope detector (CORDIC vectoring
   magnitude or a magnitude approximation) + DC-block/high-pass + audio
   decimation to 8/16 ksps. Verify in cocotb against reference Python.
6. **Single-channel end-to-end:** one DDC → AM demod → DMA → IIO → libiio read on
   host → listen. Prove audio quality on one real airband channel.
7. **Multi-channel:** parameterize into the time-multiplexed channelizer; define
   per-channel framing in the DMA buffer; extend maia-httpd to configure N
   channels and expose framed audio.
8. **Pi streamer:** libiio reader → demux → [squelch/AGC] → LAME → libshout →
   Icecast. Test against a local Icecast first, then liveatc.
9. **Hardening:** 24/7 stability (USB streaming robustness — the stock IIO daemon
   favors large infrequent buffers; watch for buffer/over-underflow issues),
   thermal/power at the tower, watchdog/restart, monitoring.

### Engage upstream early
FPGA voice demod + DDC IQ/network streaming are on Maia SDR's published roadmap.
Open a GitHub discussion on `maia-sdr/maia-sdr` before building the channelizer —
there may be in-progress work or design guidance to build on rather than
duplicate. Contributing the AM channelizer back is a natural outcome.

---

## 8. Open decisions to resolve (flag to the human, don't guess)

1. **Channel count target** — exact N (drives §4.2 feasibility and framing).
2. **Capture window center + width** — chosen so all target channels fit inside
   with edge margin (§2.5). The full voice band is ~19 MHz; size the window and
   center frequency to keep every channel of interest clear of the roll-off
   regions.
3. **Audio rate** — 8 ksps vs 16 ksps (liveatc quality vs bandwidth/resources).
4. **Squelch/AGC placement** — FPGA vs Pi (§4.4). Default: Pi first.
5. **Front-end filtering** — an airband BPF + broadcast-FM notch ahead of the
   Pluto is recommended as good hygiene for a wide capture sitting near strong
   FM broadcasters (88–108 MHz, just below the band). This is *protective*, not a
   fix for a suspected problem — the front end is known-good (§2.4) and the prior
   RSPduo had a clean front end too. Specify the filter, but treat it as standard
   practice rather than a presumed cause of quality loss.
6. **liveatc specifics** — server address, mountpoint convention, per-channel vs
   mixed streams, codec/bitrate expectations (operator has feeder credentials).

> Resolved (no longer open): the Pluto unit and its RF capability — see §2.4.
> Genuine AD9363 die unlocked to AD9364-class bandwidth, precise clock present,
> sensitivity at 120 MHz equal to the 800 MHz datasheet point. No hardware
> capability verification is gating.

---

## 9. Risks / watch-items

- **Resource fit on Z-7010** is the top technical risk (§4.2). Validate before
  deep implementation.
- **USB streaming stability** for continuous operation — the IIO daemon's
  buffering model and documented buffer-handling flakiness; budget time for
  soak testing. Consider USB-Ethernet (RNDIS) vs raw bulk; quality cabling/power.
- **Live PL reprogramming from Linux** can crash IIO streaming (documented
  unbind/rebind dance is flaky); prefer flashing full firmware images over
  `cat system_top.bit > /dev/xdevcfg`.

> Not a risk for this project: front-end sensitivity or dynamic range. The RFIC
> is known-good at airband (§2.4), and the baseline being replaced (RSPduo) also
> had a clean front end — the problem was coverage, not the front end. The
> airband BPF / FM-notch (§8.5) is hygiene, not a fix.

---

## 10. Licensing / legal

- maia-hdl: MIT. maia-httpd / maia-wasm: Apache-2.0 or MIT. Contributions back
  are welcome and aligned with the project roadmap.
- Airband monitoring legality varies by country (legal in US/UK/NL/CH and others;
  illegal without a license in e.g. Germany). Operator is an authorized liveatc
  feeder; confirm local rules for the deployment site.
