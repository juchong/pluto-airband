# hdl — our HDL experiments on top of maia-hdl

Our own simulation/experiment code. We **use `maia_hdl` as a library** (installed
editable from the pinned clone) and do not modify the upstream tree.

Prereqs (see `../DEV-SETUP.md`):

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
`NCO tune → decimate → (next) AM envelope detect` (handoff doc §4, §7 steps 5–6).

## `am_demod.py`

`EnvelopeMagnitude` — AM envelope detector = approximate `|I + jQ|` via
multiplier-free **alpha-max-beta-min** (`max + 3/8·min`; no DSP48, cheap per
channel). 2-cycle pipeline. The script verifies the HW matches an exact integer
model, bounds the approximation error (≈ −2.8%/+6.8%), and demonstrates AM-tone
recovery (magnitude → DC block → audio); plot at `out/am_demod.png`.

```bash
python am_demod.py
```

## `am_audio.py`

The AM back-end and the full single-channel chain (handoff §7 step 5, completed):

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

Audio rate (8 vs 16 ksps) is open decision §8.3 — `audio_decim`/`cic_stages` are
parameters; nothing here hard-codes the choice.

## `synth_estimate.py`

Emits Verilog for maia-hdl's `DDC`, our `AMBackEnd` (`EnvelopeMagnitude` +
`DCBlock` + `CICDecimator`), and the `TdmDdcLane` (at 8 and 25 channels) and runs
Yosys `synth_xilinx -family xc7` to get hard 7-series resource counts
(LUT/FF/DSP48E1/BRAM). Requires `yosys` on PATH.

```bash
python synth_estimate.py
```

Measured: a full DDC = **11 DSP48E1** (Cmult3x mixer 1 + 3-stage FIR 4+2+4=10),
~661 LUT, ~1239 FF, ~4 BRAM36. The AM back-end is **0 DSP48E1** (multiplier-free),
~295 LUT, ~281 FF per channel. DSP/BRAM map 1:1 and are trustworthy; LUT/FF are
Yosys estimates to be re-confirmed with Vivado.

## `feasibility_25ch.py`

The §4.2 resource-fit **GATE** for the 25-channel target. Uses the measured
per-block costs above plus a time-multiplexing throughput model to size a shared
channelizer and compare against the Z-7010 budget for several capture-window
choices (§8.2).

```bash
python feasibility_25ch.py
```

Result: **GO**, now confirmed against the **measured** Maia base platform (built
from source on the x86 server; LUT 5416/17600, FF 6493/35200, BRAM 29/60, DSP
18/80, timing met). 25 independent DDCs (275 DSP / ~100 BRAM) do not fit, but a
time-multiplexed channelizer fits the FREE budget (Z7010 − base) at every capture
window — even the full ~19 MHz airband (~8 lanes / 24 DSP / 75% of free LUT /
65% of free BRAM). Binding resources: BRAM36 then LUT. AM back-end costs zero DSP.

## `capture_window.py`

Resolves the §8.2 capture window from the operator's channel list. Picks the
center (LO) frequency so the zero-IF **DC/LO-leakage spur** lands in a guard gap
between channels, sizes the complex sample rate so every channel stays in the
central ~80% of the band (clear of the filter skirts), and reports the resulting
time-mux lane count. Re-run when the final channels arrive.

```bash
python capture_window.py
```

Result (21 core channels, 118.05–128.5 MHz): **center 123.438 MHz, Fs ≈ 14 MHz**
(±7 MHz half-band; extreme channel ±5.39 MHz = 77%; ≥1.6 MHz edge guard; DC spur
463 kHz from the nearest channel), costing ~6 of the ≤8 lanes the Z-7010 fits. The
far-out 133.65 MHz is deferred (a nice-to-have that would force Fs≈20 MHz / 8
lanes). Plot at `out/capture_window.png` (git-ignored).

## `channelizer_lane.py`

Prototype of **one time-multiplexed channelizer lane** (handoff §7 step 7): a
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
~1); even at 4 DSP/lane the budget holds (8 lanes × 4 = 32 < 62 free DSP). FF/LUT
grow with channel count because the prototype holds per-channel CIC/NCO state in
registers — in the real design that state maps to **BRAM** (which the feasibility
model already budgets). Not yet included: the shared front-end decimator and the
per-lane cleanup/compensation FIR (CIC droop correction) — the next increment.

Next: resolve the §8.2 capture window (needs the operator's 25-channel frequency
list — it sets the lane count), then add the shared front-end + cleanup FIR and
re-measure on the build server in Vivado.
