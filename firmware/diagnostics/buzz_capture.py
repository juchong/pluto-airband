#!/usr/bin/env python3
"""Capture ~N s of the airband stream, pin the exact comb fundamental, check
for drift (two-clock beat), and test inter-channel simultaneity.

Reads the decoded airband audio stream at 192.168.2.1:30000."""
from __future__ import annotations
import socket, sys, time
import numpy as np

HOST, PORT, FR, NCH = "192.168.2.1", 30000, 15625, 21


def capture(seconds):
    s = socket.create_connection((HOST, PORT), timeout=5); s.settimeout(5)
    need = int(seconds * NCH * FR) * 8
    buf = bytearray(); t0 = time.time()
    while len(buf) < need and time.time() - t0 < seconds + 10:
        try: d = s.recv(1 << 18)
        except socket.timeout: break
        if not d: break
        buf.extend(d)
    s.close()
    n = len(buf) // 8
    w = np.frombuffer(bytes(buf[:n*8]), dtype="<u8")
    smp = (w & 0xffffffff).astype(np.int64); smp = np.where(smp >= 2**31, smp-2**32, smp)
    ch = ((w >> 32) & 0xff).astype(np.int64)
    return smp, ch


def demux(smp, ch):
    return {c: smp[ch == c].astype(float) for c in range(NCH)}


def exact_fundamental(x):
    x = x - x.mean(); n = len(x)
    P = np.zeros(n//2 + 1); segs = 0; N = 1 << 16; win = np.hanning(N)
    for i in range(0, len(x)-N, N//2):
        P[:N//2+1] += 0  # placeholder
    # single big FFT
    N = 1 << 19
    if len(x) < N: N = 1 << int(np.log2(len(x)))
    xx = x[:N] * np.hanning(N)
    P = np.abs(np.fft.rfft(xx))**2
    f = np.fft.rfftfreq(N, 1/FR); df = f[1]
    def score(f0):
        ks = np.arange(1, int(3000/f0))
        idx = np.round(ks*f0/df).astype(int); idx = idx[idx < len(P)]
        return np.sum(np.log(P[idx]+1e-9))
    grid = np.arange(38.0, 52.0, 0.005)
    sc = [score(v) for v in grid]
    return grid[int(np.argmax(sc))]


def drift(x, f0):
    # track the phase of the fundamental over time via STFT
    import scipy.signal as ss
    f, t, Z = ss.stft(x - x.mean(), fs=FR, nperseg=8192, noverlap=4096)
    bi = np.argmin(np.abs(f - f0))
    band = np.abs(Z[max(0,bi-3):bi+4, :])
    peakbin = np.argmax(band, axis=0) + max(0, bi-3)
    return f[peakbin]


def main():
    secs = float(sys.argv[1]) if len(sys.argv) > 1 else 20.0
    smp, ch = capture(secs)
    X = demux(smp, ch)
    L = min(len(X[c]) for c in range(NCH))
    print(f"captured {L/FR:.1f}s/ch")
    c0 = 20
    x = X[c0][:L]
    f0 = exact_fundamental(x)
    print(f"exact fundamental ch{c0}: {f0:.3f} Hz")
    for rate, lbl in [(FR,'audio'),(109375,'chan'),(14e6,'input'),(NCH*FR,'framer')]:
        p = rate/f0
        print(f"  {lbl}: period={p:.2f} nearest={round(p)} -> {rate/round(p):.4f} Hz")
    fd = drift(x, f0)
    print(f"fundamental over time (Hz): min={fd.min():.2f} max={fd.max():.2f} "
          f"mean={fd.mean():.2f} std={fd.std():.3f}")
    # simultaneity: zero-lag correlation of quiet channels (band 40-3000)
    import scipy.signal as ss
    sos = ss.butter(4,[40,3000],'band',fs=FR,output='sos')
    qs=[20,14,7,19,13]
    B={c:ss.sosfilt(sos,X[c][:L]) for c in qs}
    print("zero-lag normalized corr (quiet pairs):")
    for a in qs:
        row=[]
        for b in qs:
            ca=B[a]-B[a].mean(); cb=B[b]-B[b].mean()
            r=np.dot(ca,cb)/(np.linalg.norm(ca)*np.linalg.norm(cb)+1e-9)
            row.append(f"{r:+.2f}")
        print(f"  ch{a}: "+" ".join(row))


if __name__ == "__main__":
    main()
