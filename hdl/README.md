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

Next in the AM chain: DC-block + audio decimation to 8/16 ksps, then wire
`DDC → EnvelopeMagnitude → audio`.
