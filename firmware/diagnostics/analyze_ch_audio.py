#!/usr/bin/env python3
"""Analyze a continuous airband channel WAV (captured with NO host DSP:
--squelch off --no-agc --no-filter --no-denoise) to characterize voice vs.
background hiss and recommend host-side audio improvements.

Usage: analyze_ch_audio.py caps_listen/ch11.wav [label]
Reads raw PCM directly (tolerates a non-finalized WAV header from a killed
recorder). Saves a spectrogram + voice/idle PSD comparison PNG and prints a
numeric report.
"""
import sys
import pathlib
import numpy as np

FS = 21875.0  # audio sps (AD9361 14 MHz / 128 / 5)
SHIFT_MAKEUP_DB = 20 * np.log10(64)  # capture used --shift -6 (x64); back it out for true dBFS

path = pathlib.Path(sys.argv[1])
label = sys.argv[2] if len(sys.argv) > 2 else path.stem

raw = path.read_bytes()
# skip 44-byte WAV header if present
if raw[:4] == b"RIFF":
    raw = raw[44:]
# even number of bytes
raw = raw[: len(raw) // 2 * 2]
x = np.frombuffer(raw, dtype="<i2").astype(np.float64)
dur = len(x) / FS
print(f"=== {label}: {len(x)} samples, {dur:.1f} s @ {FS:.0f} Hz ===")

full = 32768.0
# back out the x64 makeup so dBFS reflects the true 24-bit demod level
true = x / 64.0
peak_dbfs = 20 * np.log10(max(np.max(np.abs(x)), 1) / full)
clip = np.mean(np.abs(x) >= 32767) * 100
print(f"capture peak (post x64 makeup): {peak_dbfs:.1f} dBFS   clip%={clip:.3f}")

# ---- frame energy envelope (50 ms) ----
fr = int(0.05 * FS)
nfr = len(x) // fr
xf = x[: nfr * fr].reshape(nfr, fr)
# true-level RMS per frame in dBFS (back out makeup)
rms = np.sqrt(np.mean((xf / 64.0) ** 2, axis=1))
rms_db = 20 * np.log10(np.maximum(rms, 1e-6) / full)
t = np.arange(nfr) * 0.05

# noise floor = 10th percentile of frame RMS; transmissions = frames > floor+SNR
floor_db = np.percentile(rms_db, 10)
thr = floor_db + 8.0
active = rms_db > thr
act_frac = np.mean(active) * 100
print(f"frame RMS: floor(10pct)={floor_db:.1f} dBFS, median={np.median(rms_db):.1f}, "
      f"peak={np.max(rms_db):.1f} dBFS")
print(f"active (> floor+8dB): {act_frac:.1f}% of frames; "
      f"SNR(peak-floor)={np.max(rms_db)-floor_db:.1f} dB")

# contiguous active runs -> transmissions
runs = []
i = 0
while i < nfr:
    if active[i]:
        j = i
        while j < nfr and active[j]:
            j += 1
        if (j - i) * 0.05 >= 0.3:  # >=300 ms
            runs.append((i, j))
        i = j
    else:
        i += 1
print(f"transmissions (>=300ms): {len(runs)}; "
      f"total voice {sum((b-a) for a,b in runs)*0.05:.1f} s")
for k, (a, b) in enumerate(runs[:12]):
    seg = rms_db[a:b]
    print(f"  tx{k:2d}: {a*0.05:6.1f}-{b*0.05:6.1f}s  ({(b-a)*0.05:4.1f}s)  "
          f"mean {seg.mean():.1f} dBFS, peak {seg.max():.1f}")

# ---- PSD: voice frames vs idle frames ----
def welch(sig, n=4096):
    win = np.hanning(n)
    acc = np.zeros(n // 2 + 1)
    cnt = 0
    for s in range(0, len(sig) - n, n // 2):
        seg = sig[s:s + n] / 64.0
        X = np.fft.rfft(seg * win)
        acc += np.abs(X) ** 2
        cnt += 1
    if cnt == 0:
        return None, None
    acc /= cnt
    f = np.fft.rfftfreq(n, 1 / FS)
    psd_db = 10 * np.log10(acc / (full ** 2) + 1e-20)
    return f, psd_db

voice_idx = np.flatnonzero(active)
idle_idx = np.flatnonzero(~active)
voice_sig = xf[voice_idx].reshape(-1) if len(voice_idx) else np.array([])
idle_sig = xf[idle_idx].reshape(-1) if len(idle_idx) else np.array([])

fv, pv = welch(voice_sig) if len(voice_sig) > 8192 else (None, None)
fi, pi = welch(idle_sig) if len(idle_sig) > 8192 else (None, None)

def band_frac(f, psd_db, lo, hi):
    p = 10 ** (psd_db / 10)
    m = (f >= lo) & (f < hi)
    return p[m].sum() / p.sum() * 100

if pv is not None:
    print("\n-- VOICE-frame spectrum energy fractions --")
    print(f"  <300 Hz   : {band_frac(fv,pv,0,300):5.1f}%")
    print(f"  300-3400  : {band_frac(fv,pv,300,3400):5.1f}%  (comms voice)")
    print(f"  3400-7000 : {band_frac(fv,pv,3400,7000):5.1f}%")
    print(f"  >7000 Hz  : {band_frac(fv,pv,7000,FS/2):5.1f}%")
if pi is not None:
    print("-- IDLE-frame (hiss) spectrum energy fractions --")
    print(f"  <300 Hz   : {band_frac(fi,pi,0,300):5.1f}%")
    print(f"  300-3400  : {band_frac(fi,pi,300,3400):5.1f}%")
    print(f"  3400-7000 : {band_frac(fi,pi,3400,7000):5.1f}%")
    print(f"  >7000 Hz  : {band_frac(fi,pi,7000,FS/2):5.1f}%")
    # flatness / slope of the noise
    p = pi.copy()
    print(f"  hiss PSD @1k={np.interp(1000,fi,pi):.1f}, @3k={np.interp(3000,fi,pi):.1f}, "
          f"@5k={np.interp(5000,fi,pi):.1f}, @8k={np.interp(8000,fi,pi):.1f} dB/bin")

# ---- plot ----
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt

fig, ax = plt.subplots(3, 1, figsize=(13, 11))
# (1) envelope
ax[0].plot(t, rms_db, lw=0.4, color="tab:blue")
ax[0].axhline(floor_db, color="0.5", ls="--", lw=0.8, label=f"floor {floor_db:.0f} dBFS")
ax[0].axhline(thr, color="tab:red", ls=":", lw=0.8, label=f"active thr {thr:.0f}")
for a, b in runs:
    ax[0].axvspan(a * 0.05, b * 0.05, color="tab:orange", alpha=0.2)
ax[0].set_title(f"{label} — frame RMS envelope (true dBFS), {len(runs)} transmissions")
ax[0].set_xlabel("s"); ax[0].set_ylabel("dBFS"); ax[0].legend(fontsize=8); ax[0].grid(alpha=0.3)
# (2) spectrogram
nfft = 2048
ax[1].specgram(x / 64.0, NFFT=nfft, Fs=FS, noverlap=nfft // 2, cmap="magma")
ax[1].set_title("spectrogram"); ax[1].set_xlabel("s"); ax[1].set_ylabel("Hz")
ax[1].set_ylim(0, FS / 2)
# (3) voice vs idle PSD
if pv is not None:
    ax[2].plot(fv, pv, color="tab:green", lw=0.8, label="voice frames")
if pi is not None:
    ax[2].plot(fi, pi, color="tab:red", lw=0.8, label="idle (hiss)")
for fz in (300, 3400, 7000):
    ax[2].axvline(fz, color="0.6", ls=":", lw=0.8)
ax[2].set_title("PSD: voice vs idle (hiss)"); ax[2].set_xlabel("Hz")
ax[2].set_ylabel("dB rel FS"); ax[2].legend(fontsize=8); ax[2].grid(alpha=0.3)
ax[2].set_xlim(0, FS / 2)
fig.tight_layout()
out = pathlib.Path(__file__).parent / "out" / f"audio_{label}_{__import__('datetime').date.today()}.png"
fig.savefig(out, dpi=110)
print(f"\nSaved {out}")
