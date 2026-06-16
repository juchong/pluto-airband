#!/usr/bin/env python3
"""Classify in-band peaks as internal spurs (fixed offset from LO) vs real RF
(fixed absolute freq) by shifting the LO. Non-clipping gain. Restores state.
Restores device state on exit. Requires iio_attr on PATH and the maia recorder
at 192.168.2.1:8000. Run outside the sandbox (mutates live RF state)."""
import json, io, tarfile, urllib.request, time, subprocess, os
import numpy as np

B = "http://192.168.2.1:8000"; U = "ip:192.168.2.1"; FS = 14_000_000.0
ENV = {"PATH": os.environ["PATH"]}


def iio(args):
    subprocess.run(["iio_attr","-u",U,"-c","ad9361-phy",*args], capture_output=True, env=ENV)


def set_gain(g): iio(["-i","voltage0","hardwaregain",str(int(g))])
def set_lo(hz):  iio(["altvoltage0","frequency",str(int(hz))])


def record(dur=0.4):
    urllib.request.urlopen(urllib.request.Request(
        B+"/api/recorder",
        data=json.dumps({"mode":"IQ16bit","maximum_duration":dur,"state_change":"Start"}).encode(),
        method="PATCH", headers={"Content-Type":"application/json"}), timeout=10).read()
    time.sleep(dur+0.4)
    for _ in range(50):
        if json.loads(urllib.request.urlopen(B+"/api/recorder",timeout=10).read())["state"]=="Stopped": break
        time.sleep(0.1)
    raw = urllib.request.urlopen(B+"/recording", timeout=30).read()
    tf = tarfile.open(fileobj=io.BytesIO(raw))
    d = [tf.extractfile(m).read() for m in tf.getmembers() if m.name.endswith(".sigmf-data")][0]
    a = np.frombuffer(d, dtype="<i2")
    return a[0::2].astype(np.float64) + 1j*a[1::2].astype(np.float64)


def spectrum(x):
    N = 1 << 16; win = np.hanning(N); P = np.zeros(N); s = 0
    for i in range(0, len(x)-N, N):
        P += np.abs(np.fft.fftshift(np.fft.fft(x[i:i+N]*win)))**2; s += 1
    P /= s; f = np.fft.fftshift(np.fft.fftfreq(N, 1/FS))
    return f, P


def top_peaks(f, P, k=25):
    from numpy.lib.stride_tricks import sliding_window_view
    W = 2001; floor = np.median(sliding_window_view(np.pad(P, W//2, mode="edge"), W), axis=1)
    ex = 10*np.log10((P+1e-9)/(floor+1e-9))
    cand = np.flatnonzero(ex > 10)
    out = []
    if len(cand):
        for g in np.split(cand, np.flatnonzero(np.diff(cand) > 5)+1):
            i = g[np.argmax(P[g])]; out.append((f[i], ex[i]))
    out.sort(key=lambda t: -t[1]); return out[:k]


def main():
    cur = json.loads(urllib.request.urlopen(B+"/api",timeout=10).read())["ad9361"]
    g0 = cur["rx_gain"]; lo0 = int(cur["rx_lo_frequency"])
    LO1, LO2 = 123_438_000, 123_768_000  # +330 kHz (not a 100 kHz multiple)
    try:
        set_gain(50); time.sleep(0.3)
        set_lo(LO1); time.sleep(0.6); f1, P1 = spectrum(record())
        set_lo(LO2); time.sleep(0.6); f2, P2 = spectrum(record())
    finally:
        set_lo(lo0); set_gain(g0)
        print(f"restored gain={g0} lo={lo0}")
    SHIFT = (LO2 - LO1)
    pk1 = top_peaks(f1, P1, k=30); o2 = np.array([o for o, _ in top_peaks(f2, P2, k=60)])
    print(f"\n== classify top peaks (LO shifted +{SHIFT/1e3:.0f} kHz) ==")
    print("  fixed-OFFSET (dOff~0) => LO-relative synth spur (firmware-fixable)")
    print("  fixed-ABS (dAbs~0)    => clock harmonic / EMI at fixed RF (hardware)")
    noff = nabs = 0
    for o, db in pk1[:18]:
        dOff = o2[np.argmin(np.abs(o2-o))] - o
        dAbs = o2[np.argmin(np.abs(o2-(o-SHIFT)))] - (o-SHIFT)
        if abs(dOff) < 12e3: tag, noff = "OFFSET", noff+1
        elif abs(dAbs) < 12e3: tag, nabs = "ABS   ", nabs+1
        else: tag = "?     "
        print(f"  off={o/1e3:+9.1f}kHz abs={(LO1+o)/1e6:8.3f}MHz {db:5.1f}dB {tag}"
              f"  dOff={dOff/1e3:+6.1f} dAbs={dAbs/1e3:+6.1f}")
    print(f"\nfixed-OFFSET(LO-relative)={noff}  fixed-ABS(clock/EMI)={nabs}")

    # internal spurs: present at the SAME offset in both LO captures.
    internal = np.minimum(P1, P2)
    ipk = top_peaks(f1, internal, k=20)
    CH = [118.05e6,119.2e6,119.9e6,120.1e6,120.4e6,120.95e6,121.5e6,121.6e6,121.7e6,
          122.275e6,122.95e6,122.975e6,123.9e6,124.7e6,125.6e6,125.9e6,126.25e6,
          126.5e6,126.875e6,127.1e6,128.5e6]
    LO = 123_438_000.0
    print("\n== robust INTERNAL spurs (min of both LO captures) ==")
    print("  offset(kHz)  excessdB   abs(MHz)   40MHzharm?   ->channel(in-band kHz)")
    for o, db in ipk:
        absf = LO + o
        h = absf/40e6; harm = f"x{h:.2f}" if abs(h-round(h)) < 0.01 else ""
        nb = min(range(len(CH)), key=lambda i: abs((CH[i]-LO)-o))
        din = o-(CH[nb]-LO)
        inb = "IN-BAND" if abs(din) < 4000 else ""
        print(f"  {o/1e3:+9.1f}  {db:6.1f}   {absf/1e6:8.3f}   {harm:7s}     ch{nb}({din/1e3:+.0f}) {inb}")
    # worst internal spur within each channel's +-4 kHz passband
    print("\n== worst internal spur inside each channel passband (+-4kHz) ==")
    base = np.median(internal)
    for ci, c in enumerate(CH):
        sel = np.abs(f1-(c-LO)) < 4000
        v = 10*np.log10(internal[sel].max()/base)
        flag = " <== reported bad" if ci in (0,5,6,10,11,12) else ""
        print(f"  ch{ci:2d} {c/1e6:7.3f} off={(c-LO)/1e3:+8.1f}kHz  internal spur {v:6.1f} dB{flag}")


if __name__ == "__main__":
    main()
