# Airband RF diagnostics

A toolkit for diagnosing audio artifacts ("buzz") on the PlutoSDR airband
receiver. The full root-cause walkthrough — with plots, the per-tooth spur
taxonomy, the A/B results, and the prioritized fix menu — is in
[`SPUR-INVESTIGATION.md`](../../SPUR-INVESTIGATION.md) (one-paragraph summary in
[`SPEC.md` §7](../../SPEC.md)). **In short:** the wideband comb is a set of fixed
internal clock tones (AD9361 sample-clock 9th harmonic, the Pluto+ 125 MHz Ethernet
clock, the 40 MHz reference 3rd harmonic) coupling into the RX — **not** antenna RF,
**not** the DC-DC switcher, and **not** the airband HDL/DSP. This file documents the
**tools** themselves.

## Prerequisites

- Python venv at the repo root (`../../.venv`) with `numpy`/`scipy`:
  `../../.venv/bin/python <script>.py`
- `iio_attr` (libiio) on `PATH` for the scripts that change device settings.
- The Pluto reachable over the network with maia-httpd on `:8000` (recorder API)
  and the decoded audio stream on `:30000`. Most scripts honor `PLUTO_HOST`
  (default `10.0.16.100`); the legacy ones use `192.168.2.1`.
- Scripts that mutate live RF state (gain/LO/sample-rate) **restore the device on
  exit** and must run outside the agent sandbox. On the Pluto+'s tight 96 MB
  userspace an IQ recording can OOM-kill maia-httpd; the newer tools self-heal it
  over ssh (password `analog`) and re-apply gain.

## Scripts

| Script | What it does | Touches device |
|---|---|---|
| `band_snapshot.py` | Host/IP-parametrized wideband FFT snapshot; auto-detects the comb spacing (peak gaps + cepstrum) and saves an annotated, self-describing plot (`… <label> <gain> <date>.png`). Self-healing. | recorder only |
| `term_tests.py` | Localizes a conducted comb with the input terminated: LO **cross-correlation** (fixed-absolute vs baseband; run `term_tests.py corr`) and an **RX-gain sweep**. Self-healing. | gain + LO |
| `clock_shift_test.py` | Sweeps **Fs** (sample/digital clock) downward and classifies each tooth: alias-solver (fixed absolute aggressor) + digital-solver (∝Fs). Use a **non-commensurate** set via `PLUTO_FS` (e.g. `14,12.3,9.1`) to avoid LCM alias ambiguity. Self-healing. | gain + Fs |
| `lo_track_test.py` | Sweeps the **LO** ±0.5 MHz (Fs fixed) to tell whether a tooth tracks the LO (baseband/ADC) or stays fixed-absolute (board/clock). Self-healing. | gain + LO |
| `wideband_spectrum.py` | Wideband FFT of raw IQ; DC/LO-leak spike; lists discrete spurs and maps each to the channel it lands in; per-channel in-band power. | recorder only |
| `spur_classify.py` | Captures at two LO frequencies and classifies each peak as fixed-offset (LO-relative synth spur) vs fixed-absolute (clock harmonic / EMI / real RF). | gain + LO |
| `samplerate_spur_test.py` | Older Fs sweep (absolute-frequency recurrence). Superseded by `clock_shift_test.py` for the alias/digital classification. | gain + LO + Fs |
| `gain_sweep.py` | Sweeps RX gain; reports ADC RMS/peak/clip% and low-freq wideband AM. | gain |
| `floor_sweep.py` | Sweeps RX gain; per gain reports clip%, wideband PSD noise floor, SFDR, and strong-peak count. Finds the ADC-clipping knee (~48 dB) for choosing the adjustable `gain_db`. | gain |
| `measure_offset.py` | **Coarse clock calibration.** Measures a known AM carrier's frequency error (e.g. a commissioned AWOS) and derives the 40 MHz reference ppm error + the `ad936x_ext_refclk_override` value. ~1 ppm — a bring-up step before `lte_calibrate.py`. | recorder only |
| `lte_calibrate.py` | **Precise clock calibration (~0.01 ppm).** Tunes to an LTE downlink center and measures the carrier frequency offset by cyclic-prefix autocorrelation. `--selftest` validates the estimator with no hardware; omit `--freq` to auto-scan US bands; `--apply` programs the override, reboots, and re-measures. | gain + LO |
| `lo_band_am.py` | At a fixed clean gain, compares wideband level + low-freq AM across LO bands. | gain + LO |
| `iq_envelope.py` | Raw-IQ level/clipping, slow power-envelope AM (buzz signature), impulsive-glitch detection. Live or from a `.sigmf-data` file. | recorder only |
| `buzz_meter.py` | Connects to the decoded audio stream, demuxes channels, reports per-channel buzz metrics. A/B tool. | read-only stream |
| `buzz_capture.py` | Pins the exact comb fundamental, checks drift, tests inter-channel simultaneity from the audio stream. | read-only stream |
| `dsp_chain_sim.py` | Bit-exact reference model of the full airband DSP chain on idle input; demonstrates the DSP is clean (no buzz from the math). | none (offline) |

## Reproducing

The investigation's capture/sweep commands are in
[`SPUR-INVESTIGATION.md`](../../SPUR-INVESTIGATION.md) → *Reproduce*. Offline (no
device), confirm the DSP math itself is clean on idle input:

```bash
cd firmware/diagnostics
../../.venv/bin/python dsp_chain_sim.py
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
