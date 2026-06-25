#!/usr/bin/env python3
"""Design the widened airband cleanup FIR + verify bandwidth, adjacent rejection,
audio-CIC droop, and the CIC decimation alias rejection (the cross-channel-bleed
mechanism). Run with the repo venv:  .venv/bin/python firmware/diagnostics/design_voice_fir.py
"""
import os
import sys

import numpy as np

# import the deployed fork's coefficient designer so we match it exactly
FORK = os.path.join(os.path.dirname(__file__), "..", "..",
                    "maia-sdr", "maia-hdl", "maia_hdl", "airband")
sys.path.insert(0, FORK)
from channelizer_chain import design_cic_compensation, cic_response  # noqa: E402
import scipy.signal as ss  # noqa: E402

FS = 14_000_000.0
LANE_DECIM = 128
CHAN_RATE = FS / LANE_DECIM          # 109375 Hz
CHAN_NYQ = CHAN_RATE / 2             # 54687.5 Hz
S = 3                                # lane CIC order
NT = 63

print(f"channel rate {CHAN_RATE:.1f} Hz, Nyquist {CHAN_NYQ:.1f} Hz")

# ---- current vs widened cleanup FIR ----
def report_fir(fp, fs, label):
    coeffs, out_shift = design_cic_compensation(LANE_DECIM, S, NT, fp, fs)
    # combined CIC+FIR response on a fine grid (fraction of channel Nyquist)
    freq = np.linspace(0, 1, 4097)
    fo = freq * 0.5
    cic = cic_response(fo, LANE_DECIM, S)
    w, h = ss.freqz(coeffs, worN=freq * np.pi)
    h = np.abs(h)
    h /= h[0]
    comb = cic * h
    comb_db = 20 * np.log10(comb + 1e-12)
    hz = freq * CHAN_NYQ

    def at(fhz):
        i = np.argmin(np.abs(hz - fhz))
        return comb_db[i]
    pb_edge = fp * CHAN_NYQ
    print(f"\n[{label}] design_cic_compensation(128,3,63,{fp},{fs}) out_shift={out_shift}")
    print(f"  passband edge fp -> {pb_edge:.0f} Hz")
    for f in (1000, 3400, 5000, 6000, 8000, 10000, 12500):
        print(f"   {f:6d} Hz: {at(f):7.2f} dB")
    print(f"   adjacent ch (25000 Hz): {at(25000):7.1f} dB")
    return coeffs, out_shift

report_fir(0.11, 0.20, "CURRENT")
coeffs, out_shift = report_fir(0.15, 0.24, "WIDENED ~+-8 kHz")

print("\n_AIRBAND_FIR_OUT_SHIFT =", out_shift)
print("_AIRBAND_FIR_COEFFS = [")
row = ", ".join(str(int(c)) for c in coeffs)
print("    " + row)
print("]")

# ---- audio CIC droop at candidate audio_decim ----
print("\naudio CIC (order 4) droop vs audio_decim (post-detection, real audio):")
for adec in (7, 5, 4):
    rate = CHAN_RATE / adec
    print(f"  audio_decim={adec} -> {rate:.2f} sps (Nyquist {rate/2:.0f} Hz)"
          + ("  [non-integer rate]" if abs(rate - round(rate)) > 1e-6 else "  [integer]"))
    for f in (3400, 6000, 8000):
        # CIC magnitude referred to input freq f at input rate CHAN_RATE
        x = f / CHAN_RATE
        num = np.sin(np.pi * adec * x)
        den = adec * np.sin(np.pi * x)
        mag = (num / den) ** 4
        print(f"     {f:5d} Hz: {20*np.log10(abs(mag)+1e-12):6.2f} dB")

# ---- CIC decimation alias rejection: the cross-channel-bleed mechanism ----
# A strong signal at baseband offset D (Hz, relative to a victim channel center)
# folds through the lane CIC/128 (order S) decimation. Energy near k*CHAN_RATE
# aliases into the victim passband. Report the CIC attenuation at the aliasing
# input frequency for the 118.050 -> 122.975 case and a scan.
print("\nCIC /128 (order 3) alias rejection for a strong off-channel signal:")
def cic_atten_db(offset_hz):
    # input normalized freq (cyc/sample at 14 MHz)
    fin = offset_hz / FS
    # CIC magnitude (normalized) at this input frequency
    x = fin
    num = np.sin(np.pi * LANE_DECIM * x)
    den = LANE_DECIM * np.sin(np.pi * x)
    mag = abs((num / den) ** S)
    # where does it alias to in the output band?
    fold = offset_hz - round(offset_hz / CHAN_RATE) * CHAN_RATE
    return 20 * np.log10(mag + 1e-12), fold

for name, off in [("118.050->122.975", 122.975e6 - 118.050e6),
                  ("118.050->120.100", 120.100e6 - 118.050e6),
                  ("118.050->121.500", 121.500e6 - 118.050e6)]:
    db, fold = cic_atten_db(off)
    print(f"  {name}: offset {off/1e6:+.3f} MHz -> CIC {db:6.1f} dB, "
          f"aliases to {fold/1e3:+.2f} kHz in-band")
