#!/usr/bin/env python3
"""Capture wideband IQ via maia recorder and analyze the spectrum: DC/LO-leakage
spike, discrete spurs, and per-channel band power.

Requires the maia recorder at 192.168.2.1:8000."""
import json, io, tarfile, urllib.request, time
import numpy as np

B = "http://192.168.2.1:8000"
FS = 14_000_000.0
LO = 123_438_000.0
CH = [118.05e6,119.2e6,119.9e6,120.1e6,120.4e6,120.95e6,121.5e6,121.6e6,121.7e6,
      122.275e6,122.95e6,122.975e6,123.9e6,124.7e6,125.6e6,125.9e6,126.25e6,
      126.5e6,126.875e6,127.1e6,128.5e6]


def record(dur=0.5):
    urllib.request.urlopen(urllib.request.Request(
        B+"/api/recorder",
        data=json.dumps({"mode":"IQ16bit","maximum_duration":dur,"state_change":"Start"}).encode(),
        method="PATCH", headers={"Content-Type":"application/json"}), timeout=10).read()
    time.sleep(dur+0.4)
    for _ in range(50):
        if json.loads(urllib.request.urlopen(B+"/api/recorder",timeout=10).read())["state"]=="Stopped":
            break
        time.sleep(0.1)
    raw = urllib.request.urlopen(B+"/recording", timeout=30).read()
    tf = tarfile.open(fileobj=io.BytesIO(raw))
    d = [tf.extractfile(m).read() for m in tf.getmembers() if m.name.endswith(".sigmf-data")][0]
    a = np.frombuffer(d, dtype="<i2")
    return a[0::2].astype(np.float64) + 1j*a[1::2].astype(np.float64)


def main():
    x = record(0.5)
    n = len(x); print(f"samples={n} dur={n/FS*1000:.0f}ms")
    N = 1 << 16; win = np.hanning(N); P = np.zeros(N); segs = 0
    for i in range(0, n-N, N):
        X = np.fft.fftshift(np.fft.fft(x[i:i+N]*win)); P += np.abs(X)**2; segs += 1
    P /= segs
    f = np.fft.fftshift(np.fft.fftfreq(N, 1/FS))  # baseband offset from LO
    PdB = 10*np.log10(P+1e-9); PdB -= PdB.max()
    # DC spike: power in +-3 kHz vs median
    med = np.median(P)
    dc = P[np.abs(f) < 3000].max()
    print(f"DC/LO-leak spike: {10*np.log10(dc/med):.1f} dB above median floor")
    # discrete spurs: local peaks > 12 dB above a moving-median floor
    from numpy.lib.stride_tricks import sliding_window_view
    W = 2001
    pad = np.pad(P, W//2, mode="edge")
    floor = np.median(sliding_window_view(pad, W), axis=1)
    excess = 10*np.log10((P+1e-9)/(floor+1e-9))
    cand = np.flatnonzero(excess > 12)
    # cluster adjacent bins, keep local maxima
    spurs = []
    if len(cand):
        groups = np.split(cand, np.flatnonzero(np.diff(cand) > 5)+1)
        for g in groups:
            k = g[np.argmax(P[g])]
            spurs.append((f[k], excess[k]))
    spurs.sort(key=lambda t: -t[1])
    print(f"\n{len(spurs)} discrete spurs (offset from LO):")
    for off, db in spurs[:25]:
        nearest = min(CH, key=lambda c: abs((c-LO)-off))
        ci = CH.index(nearest); doff = off-(nearest-LO)
        print(f"  {off/1e3:+9.1f} kHz  {db:5.1f} dB   -> ch{ci} ({nearest/1e6:.3f}MHz, {doff/1e3:+.1f}kHz in-band)")
    # per-channel band power (within +-4 kHz of each channel offset)
    print("\nper-channel band power (rel dB, +-4kHz):")
    base = med
    for ci, c in enumerate(CH):
        off = c-LO; sel = np.abs(f-off) < 4000
        bp = 10*np.log10(P[sel].mean()/base)
        flag = " <== reported bad" if ci in (0,5,6,10,11,12) else ""
        print(f"  ch{ci:2d} {c/1e6:7.3f} off={off/1e3:+8.1f}kHz  {bp:6.1f} dB{flag}")


if __name__ == "__main__":
    main()
