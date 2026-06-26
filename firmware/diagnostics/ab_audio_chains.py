#!/usr/bin/env python3
"""A/B host-DSP chains on a captured channel WAV to demonstrate hiss reduction.

Renders normalized, listenable WAVs of the busy window under:
  (0) raw demod (no filter)
  (1) band-pass 300-7000 Hz   (current default high corner)
  (2) band-pass 300-3400 Hz   (proposed: airband voice ends ~3.4 kHz)
  (3) band-pass 300-3400 + spectral denoise (noise learned from idle frames)
and saves an idle-hiss residual-PSD comparison PNG.

Usage: ab_audio_chains.py caps_listen/ch11.wav [label]
"""
import sys
import pathlib
import numpy as np
from scipy.signal import butter, sosfilt

FS = 21875.0
path = pathlib.Path(sys.argv[1])
label = sys.argv[2] if len(sys.argv) > 2 else path.stem
out = pathlib.Path(__file__).parent / "out"

raw = path.read_bytes()
if raw[:4] == b"RIFF":
    raw = raw[44:]
raw = raw[: len(raw) // 2 * 2]
x = np.frombuffer(raw, dtype="<i2").astype(np.float64) / 64.0  # back out x64 makeup -> true 24b

# busy window (from the envelope analysis): 390-475 s and 540-590 s
w1 = x[int(390 * FS):int(475 * FS)]
w2 = x[int(540 * FS):int(590 * FS)]
busy = np.concatenate([w1, w2])

def bp(sig, lo, hi):
    sos = butter(4, [lo / (FS / 2), hi / (FS / 2)], btype="band", output="sos")
    return sosfilt(sos, sig)

# noise template from a quiet stretch (100-200 s is idle in the envelope)
noise = x[int(100 * FS):int(200 * FS)]

def spectral_denoise(sig, noise_ref, n=512, floor_db=-18.0, over=2.0):
    hop = n // 2
    win = np.hanning(n)
    # noise magnitude estimate
    NA = []
    for s in range(0, len(noise_ref) - n, hop):
        NA.append(np.abs(np.fft.rfft(noise_ref[s:s + n] * win)))
    Nmag = np.median(np.array(NA), axis=0)
    floor = 10 ** (floor_db / 20)
    outsig = np.zeros(len(sig))
    wsum = np.zeros(len(sig))
    for s in range(0, len(sig) - n, hop):
        seg = sig[s:s + n] * win
        X = np.fft.rfft(seg)
        mag = np.abs(X)
        ph = np.angle(X)
        red = mag - over * Nmag
        gain = np.maximum(red, floor * mag) / np.maximum(mag, 1e-9)
        Y = gain * mag * np.exp(1j * ph)
        outsig[s:s + n] += np.fft.irfft(Y, n) * win
        wsum[s:s + n] += win ** 2
    wsum[wsum < 1e-6] = 1e-6
    return outsig / wsum

chains = {
    "0raw": busy,
    "1bp300-7000": bp(busy, 300, 7000),
    "2bp300-3400": bp(busy, 300, 3400),
    "3bp300-3400_denoise": spectral_denoise(bp(busy, 300, 3400), bp(noise, 300, 3400)),
}

import wave
def write_wav(name, sig):
    # normalize to -3 dBFS peak for fair listening (stand-in for AGC)
    pk = np.max(np.abs(sig))
    g = (10 ** (-3 / 20) * 32767) / max(pk, 1e-9)
    s16 = np.clip(sig * g, -32768, 32767).astype("<i2")
    p = out / f"ch_{label}_{name}.wav"
    w = wave.open(str(p), "wb")
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(int(FS))
    w.writeframes(s16.tobytes()); w.close()
    return p

print(f"busy window {len(busy)/FS:.1f}s; rendered WAVs (normalized -3 dBFS peak):")
for k, v in chains.items():
    p = write_wav(k, v)
    print(f"  {p.name}")

# idle-hiss residual comparison: apply each chain to the noise stretch, measure PSD
def welch(sig, n=4096):
    win = np.hanning(n); acc = np.zeros(n // 2 + 1); c = 0
    for s in range(0, len(sig) - n, n // 2):
        acc += np.abs(np.fft.rfft(sig[s:s + n] * win)) ** 2; c += 1
    f = np.fft.rfftfreq(n, 1 / FS)
    return f, 10 * np.log10(acc / max(c, 1) / (32768.0 ** 2) + 1e-20)

noise_chains = {
    "raw": noise,
    "bp300-7000": bp(noise, 300, 7000),
    "bp300-3400": bp(noise, 300, 3400),
    "bp300-3400+denoise": spectral_denoise(bp(noise, 300, 3400), bp(noise, 300, 3400)),
}
# total idle RMS (dBFS) per chain = perceived hiss loudness during gaps
print("\nidle-hiss RMS (dBFS, lower = quieter background):")
base = None
for k, v in noise_chains.items():
    rms = 20 * np.log10(np.sqrt(np.mean(v ** 2)) / 32768.0 + 1e-12)
    if base is None:
        base = rms
    print(f"  {k:22s}: {rms:7.1f} dBFS   ({rms-base:+.1f} dB vs raw)")

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
plt.figure(figsize=(12, 5))
for k, v in noise_chains.items():
    f, p = welch(v)
    plt.plot(f, p, lw=0.8, label=k)
for fz in (300, 3400, 7000):
    plt.axvline(fz, color="0.6", ls=":", lw=0.8)
plt.title(f"{label}: idle-hiss residual PSD by host chain (lower = less hiss)")
plt.xlabel("Hz"); plt.ylabel("dB rel FS"); plt.legend(fontsize=8); plt.grid(alpha=0.3)
plt.xlim(0, FS / 2)
pp = out / f"audio_hiss_chains_{label}_{__import__('datetime').date.today()}.png"
plt.tight_layout(); plt.savefig(pp, dpi=110)
print(f"\nSaved {pp}")
