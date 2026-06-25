# Airband RF diagnostics

A toolkit for diagnosing audio artifacts ("buzz") on the PlutoSDR airband
receiver, plus the evidence that established the root cause. The full walkthrough
with plots is in [`SPUR-INVESTIGATION.md`](../../SPUR-INVESTIGATION.md); in short,
the wideband comb is a set of **fixed internal clock tones** (AD9361 sample-clock
9th harmonic, the Pluto+ 125 MHz Ethernet clock, the 40 MHz reference 3rd
harmonic) coupling into the RX — **not** antenna RF, **not** the DC-DC switcher,
and **not** the airband HDL/DSP.

## TL;DR root-cause findings

The audible buzz is **not** fixable in the gateware, the DSP, or by antenna-side
filtering/shielding. It is an **internally generated (conducted) spur comb** in
the AD9361 ADC samples, *before* any airband processing. Decisive evidence
(2026-06-24, Pluto+ `10.0.16.100`, `band_snapshot.py` / `term_tests.py`):

- **The comb is present with the antenna disconnected and a 50 Ω load on the RX
  input.** This is the headline test: 15 discrete teeth up to ~40 dB over the
  floor plus a finer comb, across the whole 14 MHz capture, with no antenna. The
  source is **on the board** (power/clock/oscillator harmonics coupling into the
  front end), **not RF arriving through the antenna** — which is why an external
  metal enclosure, an antenna band-pass, and a notch on the strong local carrier
  were all tried on hardware with **no effect**.
- **It is a dense, band-wide comb of on-board harmonics**, not a single line. The
  **120.000 MHz** tooth (the 40 MHz reference 3rd harmonic) is real and present,
  but it is just one tooth. A tooth inside a 25 kHz channel cannot be removed by
  the per-channel NCO/CIC/FIR/AM chain.
- **It is amplified by the RX gain** (gain sweep, terminated input): the strongest
  tooth runs −28 dBFS @71 dB → −43 @50 → −73 @30 → −86 @15, and comb-to-floor
  improves from ~36 dB @71 to ~17 dB @30. So the comb enters at/before the
  front-end gain stage, and **lowering RX gain is a real mitigation** (~19 dB
  comb-to-floor at 30 dB) — on top of removing the 71 dB clipping intermod.
- A large-shift (2.5 MHz) LO cross-correlation leans toward the teeth being at
  **fixed absolute RF frequencies** (consistent with on-board oscillator/clock/
  switcher harmonics radiating into the RF front end), though the comb is only
  partially rigid. **A small (±330 kHz) LO shift is not a reliable test here** —
  the comb is dense enough that tooth-matching aliases (the verdict flipped
  between runs); use the cross-correlation in `term_tests.py corr`.

Supporting facts established earlier:

- The DSP chain is **clean on idle input** (flat noise floor, no comb/spurs) in
  the bit-exact reference model — see `dsp_chain_sim.py`. No HDL/CORDIC/CIC change
  can remove a comb already present in the ADC samples.
- At manual gain **71 dB the ADC clips ~13–15%** of samples — a *separate* problem
  (broadband intermod that raises the floor); lowering gain stops the clipping
  (`gain_sweep.py` / `floor_sweep.py`). The shipped default is now **48 dB** (the
  bare-front-end clipping knee, which also drops the comb-to-floor); raise toward
  **71–73 dB** only behind an external selective filter at a quiet site (see
  `floor_sweep.py` and `SPEC.md` §5).
- The low (~40–47 Hz) modulation that makes the comb audible is most likely the
  switching supply amplitude-modulating the comb (a static carrier would be
  removed by the DC block).

### Spur taxonomy (what each tooth actually is) — measured 2026-06-24, Pluto+

A power-source A/B, an external-reference A/B, an Fs (digital-clock) sweep, and an
LO sweep — all terminated at gain 48 (`band_snapshot.py`, `term_tests.py`,
`clock_shift_test.py`, `lo_track_test.py`) — identified each dominant wideband
tooth. **This corrected an earlier wrong inference that the comb was the on-board
DC-DC switcher** (the power/ref A/Bs held Fs fixed and could not see it):

| tooth | source | behaviour |
|---|---|---|
| **126.000 MHz (~36 dB, dominant)** | **9th harmonic of the 14 MHz ADC sample clock** (9×14) | fixed ABSOLUTE under LO shift; moves to other n·Fs under Fs shift (10×12.3=123.0, 14×9.1=127.4); LO- and reference-independent → internal to the AD9361 |
| 125.004 MHz (~20 dB) | **Gigabit-Ethernet 125 MHz PHY clock** (Pluto+ only) | fixed absolute across LO and Fs; 125/14 non-integer (not a sample-clock harmonic) |
| 120.000 MHz (~13 dB) | **40 MHz reference 3rd harmonic** | fixed absolute; removed by an external reference |
| 122.182 MHz (~12 dB) | unidentified fixed board source | fixed absolute |
| ~LO+1.0 MHz (124.434, ~12 dB) | ADC/baseband DC-region spur | tracks the LO (fixed baseband offset) |

A/B facts behind the table:
- **Input power ruled out:** USB, a battery, and a benchtop PSU give an identical comb.
- **External reference** removes only the 120 MHz (40×3) line; the AWG used for the
  test adds its own spurs (119.438, 130.000) — use a low-spur OCXO, not an AWG.
- **Gain 71→48 dB** removes ADC-clipping intermod (teeth 15→8, cleaner floor); the
  primary teeth above persist.
- **Fs sweep gotcha:** a commensurate integer-MHz set has a small LCM (14/11/8 →
  616 MHz), so the alias-solver reports spurious 616-spaced frequencies; use a
  non-commensurate set (e.g. 14.0/12.3/9.1) — `PLUTO_FS` in `clock_shift_test.py`.

### Practical remedies (hardware/config, not gateware)

None of the dominant teeth ride the switcher↔bulk-cap rail, so that bead is **not
indicated**. By source:

1. **Lower RX gain** (≈40–48 dB; default is now 48) — removes clipping intermod and
   lowers comb-to-floor. Config change, no reflash.
2. **Frequency planning** — the sample-clock 9th harmonic (126.000), GbE clock
   (125.000), and reference 3rd harmonic (120.000) are all at fixed ABSOLUTE
   frequencies, so the LO/channel plan can keep them in guard gaps (the shipped plan
   already does: 126.000 between ch15 125.9 and ch16 126.25; 120.000 ~100 kHz off
   ch3). This is the main lever for the dominant (sample-clock) tooth, which is
   internal to the AD9361 and not otherwise removable.
3. **External clean reference** (Pluto+ EXCLK→GND + a low-spur OCXO) — removes the
   120 MHz reference 3rd harmonic.
4. **GbE 125 MHz (Pluto+):** decouple/shield the Ethernet PHY, feed audio over USB,
   or triage channels off 125.0.
5. **NOT levers (measured):** input supply (USB/battery/benchtop identical), external
   enclosure, antenna band-pass, and a switcher↔bulk-cap bead.
6. **DSP palliative (last resort):** a static per-channel notch can mask a tooth that
   lands in a channel passband, but cannot restore SNR there.

Captures in `out/`: `band_{50ohm-term_gain71,50ohm-term,battery-50ohmterm,benchtop-50ohmterm,ext-29mhz-50ohmterm}_gain48_*`,
`clock_shift_intVCTCXO_{14-11-8,14.0-12.3-9.1}MHz_*`,
`lo_track_intVCTCXO_gain48_*`, `localization_loshift-gainsweep_gain71_*`.

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
| `band_snapshot.py` | Host/IP-parametrized wideband FFT snapshot; auto-detects the comb spacing (peak gaps + cepstrum) and saves an annotated plot. Used for the 50 Ω-termination capture (`PLUTO_HOST=… python band_snapshot.py`). | recorder only |
| `term_tests.py` | Localizes a conducted comb with the input terminated: LO **cross-correlation** (fixed-absolute RF vs baseband/clock; run `term_tests.py corr`) and an **RX-gain sweep** of the comb. Auto-restarts maia-httpd over ssh if an IQ recording OOMs (96 MB Pluto+). | gain + LO |
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
