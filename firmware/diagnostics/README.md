# Airband RF diagnostics

A toolkit for diagnosing audio artifacts ("buzz") on the PlutoSDR airband
receiver, plus the evidence that established the root cause. These scripts were
written to chase a persistent buzz on several channels and ended up proving the
buzz is an **RF front-end / hardware spur problem, not the airband HDL/DSP**.

## TL;DR root-cause findings

The audible buzz is **not** fixable in the gateware, the DSP, or the gain
setting. It is caused by **physical, fixed-frequency RF spurs** present in the
AD9361 ADC samples *before* any airband processing:

- A **~485 kHz spur comb phase-locked to 120.000 MHz** sits across the airband.
  120.000 MHz is exactly the **3rd harmonic of the Pluto's 40 MHz reference**
  oscillator, and it falls squarely inside the receiver's capture window
  (118.05–128.5 MHz), so it cannot be tuned around.
- These spurs land inside the passbands of the channels reported as bad
  (ch0/2/3/5/6/8/10/11/12 at ~18–26 dB) and miss the clean ones
  (ch7/9/14/16/18). A spur inside a 25 kHz channel cannot be removed by the
  per-channel NCO/CIC/FIR/AM chain.
- The spurs are **invariant** to sample rate (BBPLL/ADC clock), LO frequency,
  and RX gain — confirming they are reference-locked physical RF, not digital
  aliases, LO-synthesizer spurs, or clipping intermod.

Supporting facts established along the way:

- The DSP chain is **clean on idle input** (flat noise floor, no comb/spurs) in
  the bit-exact reference model — see `dsp_chain_sim.py`.
- The disturbance is present in the **raw wideband IQ** captured via maia's own
  recorder, i.e. upstream of all airband logic — see `iq_envelope.py`,
  `wideband_spectrum.py`.
- At the original manual gain of **71 dB the ADC clips ~13–15%** of samples;
  lowering gain stops the clipping and the wideband AM but does **not** remove
  the in-band spurs — see `gain_sweep.py` / `floor_sweep.py`. (This clipping was
  the real cause of the "poor noise floor"; the default is now **48 dB**, just
  below the clipping knee — see `floor_sweep.py` and `SPEC.md` §5.)
- The buzz is coherent/common-mode across all channels (one source); the low
  (~40–47 Hz) modulation is most likely the switching supply amplitude-
  modulating the spur comb (a static carrier would be removed by the DC block).

### Practical remedies (hardware, not firmware)

1. **Clean power** (linear/LDO supply or filtered battery, USB common-mode
   chokes/ferrites) to reduce the modulation that makes the spurs audible.
2. **Shielding** against the 120 MHz reference-harmonic coupling.
3. **External clean reference** (TCXO/GPSDO) into the Pluto.
4. **Channel triage** — prefer the spur-free channels.

## Prerequisites

- Python venv at the repo root (`../../.venv`) with `numpy`/`scipy`:
  `../../.venv/bin/python <script>.py`
- `iio_attr` (libiio) on `PATH` for the scripts that change device settings.
- The Pluto reachable at `192.168.2.1` with maia-httpd on `:8000` (recorder API)
  and the decoded audio stream on `:30000`.
- Scripts that mutate live RF state (gain/LO/sample-rate) **restore the device
  on exit** and must run outside the agent sandbox.

## Scripts

| Script | What it does | Touches device |
|---|---|---|
| `wideband_spectrum.py` | Wideband FFT of raw IQ; DC/LO-leak spike; lists discrete spurs and maps each to the channel it lands in; per-channel in-band power. | recorder only |
| `spur_classify.py` | Captures at two LO frequencies and classifies each peak as fixed-offset (LO-relative synth spur) vs fixed-absolute (clock harmonic / EMI / real RF); maps internal spurs to channels. | gain + LO |
| `samplerate_spur_test.py` | Sweeps the AD9361 sample rate and checks whether in-band spurs move (clock alias) or stay (physical). | gain + LO + Fs |
| `gain_sweep.py` | Sweeps RX gain; reports ADC RMS/peak/clip% and low-freq wideband AM. | gain |
| `floor_sweep.py` | Sweeps RX gain; per gain reports clip%, wideband PSD noise floor (median dBFS), SFDR (worst in-band spur above floor), and strong-peak count. Used to set the manual gain just below ADC clipping (the 71→48 dB change). | gain |
| `measure_offset.py` | **Coarse clock calibration.** Measures a known AM carrier's frequency error (e.g. a commissioned AWOS) and derives the 40 MHz reference ppm error + the `ad936x_ext_refclk_override` value. ~1 ppm (limited by the 25 kHz channel) — a bring-up step before `lte_calibrate.py`. | recorder only |
| `lte_calibrate.py` | **Precise clock calibration (~0.01 ppm).** Tunes to an LTE downlink center and measures the carrier frequency offset by cyclic-prefix autocorrelation (eNodeBs are GPS-disciplined to ±0.05 ppm). `--selftest` validates the estimator with no hardware; omit `--freq` to auto-scan US bands; `--apply` programs the override, reboots, and re-measures to convergence. | gain + LO |
| `lo_band_am.py` | At a fixed clean gain, compares wideband level + low-freq AM across LO bands (internal vs external test). | gain + LO |
| `iq_envelope.py` | Raw-IQ level/clipping, slow power-envelope AM (buzz signature), impulsive-glitch detection. Captures live or reads a `.sigmf-data` file. | recorder only |
| `buzz_meter.py` | Connects to the decoded audio stream, demuxes channels, reports per-channel buzz metrics (comb%, 7625 Hz tone, etc.). A/B tool. | read-only stream |
| `buzz_capture.py` | Pins the exact comb fundamental, checks drift (two-clock beat), and tests inter-channel simultaneity from the audio stream. | read-only stream |
| `dsp_chain_sim.py` | Bit-exact reference model of the full airband DSP chain on idle input; demonstrates the DSP is clean (no buzz from the math). | none (offline) |

## Reproducing the headline result

```bash
cd firmware/diagnostics
PY=../../.venv/bin/python

# 1. The DSP math is clean on idle input (no device needed):
$PY dsp_chain_sim.py

# 2. The spurs are real and in the raw input, mapped to channels:
$PY wideband_spectrum.py

# 3. The spurs are physical/fixed (don't move with the internal clock):
$PY samplerate_spur_test.py
```

## Clock calibration

Correct the reference-oscillator ppm error (see `SPEC.md` §5.2). Coarse first
(brings the unit within the LTE capture range), then precise:

```bash
cd firmware/diagnostics
PY=../../.venv/bin/python
export PLUTO_HOST=<device-ip>

$PY lte_calibrate.py --selftest          # validate the estimator (no hardware)
$PY measure_offset.py                     # coarse, against AWOS 118.050 MHz
$PY lte_calibrate.py --freq <cell_MHz>    # precise (omit --freq to auto-scan)
$PY lte_calibrate.py --freq <cell_MHz> --apply   # program override + reboot + verify
```
