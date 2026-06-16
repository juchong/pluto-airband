#!/usr/bin/env python3
"""Analyze raw wideband IQ for a periodic broadband disturbance (the buzz
source): wideband level/clipping, slow power-envelope AM (the buzz signature),
and impulsive-glitch detection.

Usage:
    python3 iq_envelope.py                 # capture live via the maia recorder
    python3 iq_envelope.py path.sigmf-data # analyze an existing ci16 IQ file
"""
import io, json, sys, tarfile, time, urllib.request
import numpy as np

FS = 14_000_000.0
B = "http://192.168.2.1:8000"


def record(dur=0.5):
    urllib.request.urlopen(urllib.request.Request(
        B + "/api/recorder",
        data=json.dumps({"mode": "IQ16bit", "maximum_duration": dur,
                         "state_change": "Start"}).encode(),
        method="PATCH", headers={"Content-Type": "application/json"}), timeout=10).read()
    time.sleep(dur + 0.4)
    for _ in range(60):
        if json.loads(urllib.request.urlopen(B + "/api/recorder", timeout=10).read())["state"] == "Stopped":
            break
        time.sleep(0.1)
    blob = urllib.request.urlopen(B + "/recording", timeout=30).read()
    tf = tarfile.open(fileobj=io.BytesIO(blob))
    d = [tf.extractfile(m).read() for m in tf.getmembers() if m.name.endswith(".sigmf-data")][0]
    return np.frombuffer(d, dtype="<i2")


raw = np.fromfile(sys.argv[1], dtype="<i2") if len(sys.argv) > 1 else record()
I = raw[0::2].astype(np.float64); Q = raw[1::2].astype(np.float64)
x = I + 1j * Q
n = len(x)
print(f"samples={n} dur={n/FS*1000:.1f} ms")
print(f"I: mean={I.mean():.2f} std={I.std():.2f}  Q: mean={Q.mean():.2f} std={Q.std():.2f}")
print(f"|x|: mean={np.abs(x).mean():.1f} max={np.abs(x).max():.1f}")

# Instantaneous power, decimate by block-averaging to expose slow AM
p = (I*I + Q*Q)
DEC = 1400  # 14e6/1400 = 10 kHz envelope rate
m = (len(p)//DEC)*DEC
env = p[:m].reshape(-1, DEC).mean(axis=1)
fr_env = FS/DEC
env = env - env.mean()
N = 1 << int(np.log2(len(env)))
E = np.abs(np.fft.rfft(env[:N]*np.hanning(N)))**2
fe = np.fft.rfftfreq(N, 1/fr_env)
# top spectral lines of the power envelope below 1 kHz
band = (fe > 5) & (fe < 1000)
idx = np.argsort(E[band])[::-1][:12]
fb = fe[band][idx]
print("\n== power-envelope spectral lines (Hz : reldB) ==")
ref = np.median(E[band])
for f in np.sort(fb):
    k = np.argmin(np.abs(fe-f))
    print(f"  {f:8.2f} Hz  {10*np.log10(E[k]/ref):6.1f} dB")

# comb fundamental search 20-120 Hz
df = fe[1]
def score(f0):
    ks = np.arange(1, int(1500/f0)); ii = np.round(ks*f0/df).astype(int)
    ii = ii[ii < len(E)]; return np.sum(np.log(E[ii]+1e-9))
grid = np.arange(20, 120, 0.02)
sc = [score(v) for v in grid]
f0 = grid[int(np.argmax(sc))]
print(f"\nbest comb fundamental in power envelope: {f0:.3f} Hz  (period {FS/f0:.0f} input samples)")

# impulsive glitch detection on raw magnitude
mag = np.abs(x)
med = np.median(mag); mad = np.median(np.abs(mag-med))
thr = med + 8*1.4826*mad
hits = np.flatnonzero(mag > thr)
print(f"\nglitch threshold={thr:.0f} (med={med:.0f}); #samples over thr={len(hits)} "
      f"({100*len(hits)/n:.3f}%)")
if len(hits) > 2:
    d = np.diff(hits)
    d = d[d > 50]  # ignore within-burst
    if len(d):
        print(f"  inter-glitch gaps: median={np.median(d):.0f} samples "
              f"= {FS/np.median(d):.1f} Hz; min={d.min()} max={d.max()}")
