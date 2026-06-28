# hdl — our HDL experiments on top of maia-hdl

Our own simulation/experiment code. We **use `maia_hdl` as a library** (installed
editable from the pinned clone) and do not modify the upstream tree.

Prereqs (see `../BUILD.md`):

```bash
source ../.venv/bin/activate
pip install -e ../maia-sdr/maia-hdl    # makes `import maia_hdl` work
```

## `ddc_tune_decimate.py`

A standalone Amaranth simulation of maia-hdl's `DDC` (NCO mixer → 3-stage FIR
decimator). Stages 2 and 3 are bypassed so a single decimation stage is
exercised, which keeps the coefficient bookkeeping simple. It:

- designs + fixed-point-scales + packs a stage-1 low-pass (mirroring maia-hdl's
  FIR coefficient memory layout),
- feeds a complex tone, paced one sample per input strobe (the NCO advances only
  on valid strobes, so idle cycles simply let the decimator catch up — the DDC
  exposes no input backpressure),
- **Run A:** tunes the NCO so the tone lands at DC → output is a clean constant
  (baseband),
- **Run B:** detunes so the tone lands in the stopband → output is rejected.

Run:

```bash
python ddc_tune_decimate.py
```

Expected (validated): Run A output settles to a constant complex DC value with
~0% AC ripple; Run B is ~60 dB lower → channel selectivity. A plot is written to
`out/ddc_tune_decimate.png` (git-ignored).

This is the building block for the airband channelizer: per-channel
`NCO tune → decimate → (next) AM envelope detect` (`SPEC.md` §4).

## `am_demod.py`

`EnvelopeMagnitude` — the original AM envelope detector = approximate `|I + jQ|`
via multiplier-free **alpha-max-beta-min** (`max + 3/8·min`). 2-cycle pipeline.

> **Superseded in the datapath.** Its gain ripples ~10% with the I/Q phase angle,
> which amplitude-modulates each (slightly off-tuned) carrier and adds demod
> spurs at 4·df and harmonics (~−30 dBc). The shipping receiver uses a ripple-free
> **CORDIC vectoring magnitude** in `am_backend_tdm.py:TdmAmBackend` instead, so
> the demodulator itself is artifact-free. `EnvelopeMagnitude` is kept only for
> the standalone self-test / resource estimate.
>
> Note: the audible on-air "buzz" was later root-caused to an **RF hardware spur**
> (a comb locked to the Pluto's 40 MHz reference), independent of the demod — see
> `firmware/diagnostics/README.md` and `PROGRESS.md`. CORDIC is the correct
> detector regardless, but it is not the fix for that buzz.

The script verifies the HW matches an exact integer model, bounds the
approximation error (≈ −2.8%/+6.8%), and demonstrates AM-tone recovery; plot at
`out/am_demod.png`.

```bash
python am_demod.py
```

## `am_audio.py`

The AM back-end and the full single-channel chain (`SPEC.md` §4.2):

- `DCBlock` — one-pole high-pass (leaky-integrator DC estimate, then subtract)
  that strips the carrier-amplitude DC term left by the envelope detector.
  Multiplier-free (one arithmetic shift + two adds).
- `CICDecimator` — multiplier-free CIC (cascaded integrator-comb) decimator that
  low-pass filters and drops the magnitude stream to the audio rate (8/16 ksps).
  No DSP48, no coefficient memory → cheap to replicate per channel on the Z-7010.
- `AMChannel` — wires the whole single channel together:
  `DDC (NCO mixer + FIR decimate) → EnvelopeMagnitude → DCBlock → CICDecimator`,
  with the stateful blocks advanced by validated sample strobes derived from the
  DDC output strobe.

```bash
python am_audio.py
```

Verified: `DCBlock` and `CICDecimator` match exact integer reference models; the
DC block removes a large input bias to ≈0; and an end-to-end run feeds a
frequency-offset AM tone through the chain, tunes the NCO, demodulates, and
recovers a clean low-rate audio tone at the expected frequency. Plot at
`out/am_audio.png` (git-ignored).

`audio_decim`/`cic_stages` are parameters; nothing here hard-codes the rate. The
shipping receiver uses **`audio_decim=5` → 20 000 sps** (16 MHz / 160 / 5); see
`SPEC.md` §4.2.

## `synth_estimate.py`

Emits Verilog for maia-hdl's `DDC`, our `AMBackEnd` (`EnvelopeMagnitude` +
`DCBlock` + `CICDecimator`), the `TdmDdcLane` (at 8 and 21 channels), and the
channelizer-chain filters (`FrontEndDecimator`, `CompensationFIR`), then runs
Yosys `synth_xilinx -family xc7` to get hard 7-series resource counts
(LUT/FF/DSP48E1/BRAM). Requires `yosys` on PATH.

```bash
python synth_estimate.py
```

Measured: a full DDC = **11 DSP48E1** (Cmult3x mixer 1 + 3-stage FIR 4+2+4=10),
~661 LUT, ~1239 FF, ~4 BRAM36. The AM back-end is **0 DSP48E1** (multiplier-free),
~295 LUT, ~281 FF per channel. The channelizer-chain FIRs are printed both as the
**unrolled** Yosys upper bound (1 mult/tap) and as the **folded** MAC-engine cost:
the shared front-end as a single long FIR is ~49 DSP at 16 MHz (→ use a multistage
decimator), while the channel selectivity FIR at ~50 kHz folds to ~0.2 DSP/channel
(~4 DSP for all 21 on one engine). DSP/BRAM map 1:1 and are trustworthy; LUT/FF are
Yosys estimates to be re-confirmed with Vivado.

## `feasibility_25ch.py`

The `SPEC.md` §4.1 resource-fit **GATE** for the operational channel set (18,
including 133.65 MHz). Uses the measured per-block costs above plus a time-multiplexing
throughput model to size a shared channelizer and compare against the Z-7010
budget for several capture-window choices (§5). (Filename is historical — the
planning target was 25.)

```bash
python feasibility_25ch.py
```

Result: **GO**, now confirmed against the **measured** Maia base platform (built
from source on the x86 server; LUT 5416/17600, FF 6493/35200, BRAM 29/60, DSP
18/80, timing met). 22 independent DDCs (242 DSP / ~88 BRAM) do not fit, but a
time-multiplexed channelizer fits the FREE budget (Z7010 − base) at every capture
window — even the full ~19 MHz airband (7 lanes / 35 DSP / 78% of free LUT /
65% of free BRAM). The resolved 16 MHz window runs **6 lanes** (`chans_per_lane=3`,
18 channels). The model's `~78% LUT` for 7 lanes proved optimistic — in Vivado the
21-channel/7-lane build hit **18234 LUT > 17600** (over the die), so the plan is
capped at 18 ch / 6 lanes (same lane count as the proven 14 MHz build). Binding
resources: LUT then BRAM36. AM back-end costs zero DSP.

## `capture_window.py`

Resolves the `SPEC.md` §5 capture window from the operator's channel list. Picks the
center (LO) frequency so the zero-IF **DC/LO-leakage spur** lands in a guard gap
between channels, sizes the complex sample rate so every channel stays in the
central ~80% of the band (clear of the filter skirts), and reports the resulting
time-mux lane count. Re-run if the channel list changes.

```bash
python capture_window.py
```

Result (18 channels incl. 133.65, 119.2–133.65 MHz): **center 126.4 MHz, Fs = 16
MHz** (±8 MHz half-band; extreme channels ±7.25 MHz = 90.6%; ≥0.75 MHz edge guard;
DC spur 100 kHz from the nearest channel), costing **6** lanes (`cpl=3`). 133.65
lands at ~91% of the half-band: the strict 80%-usable rule would request 20 MHz, but
16 MHz is chosen to seat the internal sample-clock comb at 128.000 MHz (a guard gap)
and give a round 20 kHz audio rate. The channel count is capped at 18 because
21 ch → 7 lanes overflows the XC7Z010 LUTs in Vivado. Plot at
`out/capture_window.png` (git-ignored).

## `channelizer_lane.py`

Prototype of **one time-multiplexed channelizer lane** (`SPEC.md` §4): a
single NCO + complex-mixer + CIC-decimator datapath shared across `n_channels`
channels, with all per-channel state (NCO phase, CIC integrator/comb registers,
decimation counter) in arrays indexed by a channel counter. One wideband IQ sample
is broadcast to every channel and the lane sweeps the channels one-per-cycle — the
concrete realization of the time-multiplexing the feasibility model assumed.

```bash
python channelizer_lane.py
```

Verified: the hardware is **bit-exact** to a Python reference model (same NCO ROM,
fixed-point mixer, and integer CIC) across all channels; a 4-channel / 4-tone demo
shows each channel tuning *its own* tone to baseband through the one shared
datapath (I ≈ DC, Q ≈ 0, ripple < 1%), rejecting the others. Plot at
`out/channelizer_lane.png` (git-ignored).

Resources (`synth_estimate.py`, Yosys xc7): the shared complex mixer is **4
DSP48E1** independent of channel count (maia-hdl's `Cmult3x` would trim this to
~1); even at 4 DSP/lane the budget holds (8 lanes × 4 = 32 < 62 free DSP). The
register-based `TdmDdcLane` holds per-channel CIC/NCO state in flip-flops; the file
also provides **`TdmDdcLaneBRAM`** — same behaviour (bit-exact, same model) but the
per-channel state lives in `amaranth.lib.memory.Memory` (block/distributed RAM) and
the datapath is **pipelined READ→MIX→INTEG→COMB** so it closes the 65.278 MHz sync clock. This is the
lane used by the integrated `channelizer_core.py`. The shared front-end decimator and
per-lane cleanup FIR are built and verified separately in `channelizer_chain.py`.

## `channelizer_chain.py`

The two filtering stages the lane prototype deferred (`SPEC.md` §4),
built on one generic integer block, `FIRStage` (direct-form decimating FIR, verified
**bit-exact** to its model at decimation 1 and 4):

- `FrontEndDecimator` — a **shared** complex FIR low-pass decimator (one per
  receiver, amortized over all channels). The AD936x is run oversampled and this one
  block decimates the whole capture to the working rate (~the `SPEC.md` §5 window) with a
  *flat* passband — a CIC here would droop the band edges and starve the outer
  channels. **Verified:** 0.01 dB passband ripple across the channel region, 57 dB
  rejection beyond the window.
- `CompensationFIR` (a `FIRStage`) — a per-channel FIR that both inverts the CIC
  passband droop *and* provides the sharp channel selectivity the CIC's gentle
  roll-off cannot. **Verified:** CIC droop 2.17 dB → 0.39 dB flat with 88 dB
  adjacent-channel rejection.
- **End-to-end HW** (front-end → NCO mix → per-channel CIC → comp FIR): an
  on-channel tone passes; a tone one channel-spacing away is rejected by 48 dB.

It also contains the two **realistic realizations** (so the front-end + cleanup FIR
are cheap on the device):

- `MultiStageDecimator` — a cascade of halfband decimate-by-2 stages (the cheap
  front end if we oversample). **Bit-exact** to the cascaded model; 0.08 dB
  channel-region ripple, 53 dB rejection; folds to **~14 DSP** vs ~43 for one long
  FIR. Optional in the baseline (the AD936x can deliver the working rate directly).
- `TdmFirEngine` — the **folded** cleanup FIR: one multiply-accumulate iterated over
  taps serves all channels (per-channel delay lines indexed by channel). **Bit-exact**
  to the per-channel parallel FIR; Yosys = 2 DSP. `TdmFirEngineBRAM` is the same engine
  with the per-channel delay lines as a **circular buffer in block RAM** (no FF shift
  register) — bit-exact, and what keeps the integrated core's FF count low.

```bash
python channelizer_chain.py
```

Plot at `out/channelizer_chain.png` (git-ignored). Realization note: the prototype
direct FIRs are fully parallel (1 mult/tap) — Yosys/Vivado show that upper bound;
`MultiStageDecimator` + `TdmFirEngine` are the folded/cheap forms.

**Vivado 2023.2 OOC cross-check** (build server, `xc7z010clg225-1`; see
`../BUILD.md` for the recipe): the 21-channel `TdmDdcLane` synthesizes to **4
DSP, 3374 LUT (19%), 7760 FF (22%), 0 BRAM**, closely matching the Yosys estimate and
confirming the lane fit; per-channel state lands in FFs here (the Memory-backed lane
in `channelizer_core.py` moves it to BRAM).

## `channelizer_core.py`

Integrated channelizer (`SPEC.md` §4): **`ChannelizerCore`** wires the verified
blocks into one top — `TdmDdcLaneBRAM` → burst-absorbing `SyncFIFO` → folded complex
cleanup FIR (`TdmFirEngineBRAM` ×2, I and Q in lockstep). All channels share one
decimation cadence, so each CIC boundary emits a burst of N outputs; the FIFO buffers
it and the cleanup FIR drains at the low channel rate.

```bash
python channelizer_core.py        # bit-exact + end-to-end selectivity
python emit_core_verilog.py       # -> out/channelizer_core.v (for Vivado)
```

Verified **bit-exact**: HW == (lane model → per-channel cleanup-FIR model); the
end-to-end demo tunes each channel to a clean baseband (<0.1 % ripple) and rejects
neighbours.

**Vivado 2023.2 synth + place + route** (`ooc_place.tcl`, `xc7z010clg225-1`; this
out-of-context cross-check was run at the then-current 62.5 MHz — the shipped full
`plutoplus` build now closes at the 65.278 MHz sync clock, WNS +0.153 ns)
of one deployment lane (5 ch, dec-64 CIC, complex 119-tap cleanup):

| Resource | Used | % | 
|---|---|---|
| Slice LUTs | 1309 | 7.4 % |
| Slice Registers | 1577 | 4.5 % |
| Block RAM Tile | 3 | 5.0 % |
| DSP48E1 | 8 | 10 % |

**Timing MET: WNS +3.07 ns, 0 failing endpoints**, route clean. (Pre-BRAM/pre-pipeline
this overflowed FFs at 38 980 and missed timing at −3.28 ns; BRAM-backed state +
the 4-stage lane pipeline fixed both.) 6 lanes cover the 18 core channels; the
full 7-lane/21-channel build overflowed the die in Vivado (18234 LUT > 17600),
which is why the plan caps at 18 ch / 6 lanes.

## `am_backend_tdm.py`

**`TdmAmBackend`** — the AM demodulator, folded over channels (`SPEC.md` §4.2).
It iterates one datapath (`|I+jQ|` → one-pole DC block → CIC audio decimator) over
all channels, holding each channel's DC-block and CIC state in
`amaranth.lib.memory` (BRAM/distributed RAM). The magnitude is a multiplier-free
**CORDIC vectoring** detector (`CORDIC_ITERS=12`): unlike the old alpha-max-beta-min
estimator it has **no angle-dependent gain ripple**, so it does not modulate
off-tuned carriers into demod spurs. A short sequential FSM
processes one channel (CORDIC iterations + DC + CIC stages); `busy` gates the
caller; the AM duty has ample headroom (~0.85 at the deployment rate). **Bit-exact**
to the per-channel `cordic_magnitude → DCBlock → CICDecimator` models, and recovers
a real AM tone. DSP-free (adds/shifts only).

```bash
python am_backend_tdm.py
```

## `audio_framer.py`

**`AudioFramer`** — packs each per-channel audio strobe into a fixed 8-byte
record and exposes an AXI4-Stream (`stream_data`/`valid`/`ready`) that plugs
straight into `maia_hdl.dma.DmaStreamWrite` (`width=64`). Record layout (`SPEC.md` §6):
`bits[31:0]` signed sample, `bits[39:32]` channel index, `bits[63:40]` per-channel
sequence counter (so userspace can demux and detect drops). Verified: sample +
channel + monotonic per-channel sequence preserved through the staging FIFO.

## `receiver_top.py`

**`ReceiverTop`** — the complete receiver datapath (`SPEC.md` §4):
wideband IQ → N `ChannelizerCore` lanes (same stream broadcast) → round-robin
collector → `TdmAmBackend` → `AudioFramer` → DMA stream. Channels are balanced
across `ceil(N/chans_per_lane)` lanes; the shipping config is `chans_per_lane=3`,
so 18 → **6 lanes** `[3,3,3,3,3,3]` (21 ch → 7 lanes overflows the XC7Z010 LUTs).
One lane sweeps all its channels per input
sample and needs `chans_per_lane+1` PL cycles to do so, while the input's *shortest*
gap is `floor(Fpl/Fs)` cycles — so `chans_per_lane ≤ floor(Fpl/Fs) − 1` (= 3 here:
`floor(65.278/16) = 4`). Per-channel
NCO tuning words are written through a flat register interface
(`freq_wren`/`freq_waddr`=global channel/`freq_wdata`), routed to the owning lane.

```bash
python receiver_top.py            # end-to-end bit-exact (6 ch / 3 lanes)
```

Verified **bit-exact** end-to-end: framed per-channel audio == (lane model →
cleanup-FIR model → AM model), per-channel sequence monotonic. Remaining: splice
into the Maia base platform (DMA HP port + control registers) and a full-design
place.

## `realtime_budget.py`

The `SPEC.md` §4.1 feasibility GATE checked **area**; this checks **throughput**. The shared
(folded) datapaths must keep up with the sample cadence: at `Fpl=65.278 MHz`,
`Fs=16 MHz` there are `~4.08` PL cycles per input sample. Two things must hold:

- **Integer cadence (no dropped samples).** The input CDC delivers a sample every
  `floor(Fpl/Fs)` cycles on its *shortest* beat, and a lane needs `chans_per_lane+1`
  cycles, so we require `floor(Fpl/Fs) ≥ chans_per_lane+1`. At `chans_per_lane=3`
  that needs `floor(Fpl/Fs) ≥ 4` ⟹ **`Fpl ≥ 4·Fs = 64 MHz`**. The original
  62.5 MHz gave `floor(3.906) = 3 < 4` and silently dropped the ~9.4 % of samples
  landing on the short beat (the 18.1 ksps bug); **65.278 MHz** gives
  `floor(4.08) = 4`. `realtime_budget.py` asserts this gap.
- **Duty cycle < 1.** Each binding folded stage must finish within the cadence:
  `duty_lane = chans_per_lane / (Fpl/Fs)`,
  `duty_fir  = chans_per_lane · (Fs/lane_decim) · (ntaps+ovh) / Fpl`,
  `duty_am   = N · (Fs/lane_decim) · am_cycles / Fpl`.

**Important:** the OOC config (`dec-64`, `119-tap`) was for *resource* measurement
and does **not** close real-time timing (`duty_fir > 1`). The recommended
deployment config is **`chans_per_lane=3`, `lane_decim=160`, cleanup `ntaps=63`
→ 6 lanes** (18 channels), with `audio_decim=5` giving **20.0 ksps** audio (duties
lane≈0.74 / fir≈0.32 / am≈0.63, min gap 4 ≥ 4).

```bash
python realtime_budget.py     # prints the duty table + cycle-accurate stress test
```

A cycle-accurate stress test drives `ReceiverTop` at the **true** cadence (one
input every `Fpl/Fs` cycles on average, with the realistic gap=4/gap=5 jitter for a
non-integer ratio): a budget-fitting config stays **overflow-free and bit-exact**,
and an over-budget config (`dec-32`/`119-tap`, `duty_fir>1`)
**correctly trips** the overflow detector (which now covers the lane→FIR FIFO, the
collector FIFOs, and the framer FIFO).
