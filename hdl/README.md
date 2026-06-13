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

Next: time-multiplex one DDC datapath across many channels to estimate Z-7010
resource fit (the §4.2 feasibility gate), and the x86 build-server bring-up.
