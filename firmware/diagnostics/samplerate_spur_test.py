#!/usr/bin/env python3
"""Sample-rate / internal-clock test. For several AD9361 sample rates, capture
raw IQ and locate the in-band spurs in ABSOLUTE frequency. Spurs that stay put
across Fs are physical/EMI (immovable); spurs that move are ADC/BBPLL-clock
aliases (movable by choosing Fs). Restores device state on exit.

Requires iio_attr on PATH and the maia recorder reachable at 192.168.2.1:8000.
Run outside the sandbox (mutates live RF state)."""
import json, io, tarfile, urllib.request, time, subprocess, os
import numpy as np

B = "http://192.168.2.1:8000"; U = "ip:192.168.2.1"
LO = 123_438_000
ENV = {"PATH": os.environ["PATH"]}


def iio_set(args):
    subprocess.run(["iio_attr", "-u", U, "-c", "ad9361-phy", *args], capture_output=True, env=ENV)


def iio_get(args):
    r = subprocess.run(["iio_attr", "-u", U, "-c", "ad9361-phy", *args],
                       capture_output=True, env=ENV, text=True)
    return r.stdout.strip()


def record(dur=0.25):
    urllib.request.urlopen(urllib.request.Request(
        B+"/api/recorder",
        data=json.dumps({"mode":"IQ16bit","maximum_duration":dur,"state_change":"Start"}).encode(),
        method="PATCH", headers={"Content-Type":"application/json"}), timeout=10).read()
    time.sleep(dur+0.4)
    for _ in range(60):
        if json.loads(urllib.request.urlopen(B+"/api/recorder",timeout=10).read())["state"]=="Stopped": break
        time.sleep(0.1)
    raw = urllib.request.urlopen(B+"/recording", timeout=30).read()
    tf = tarfile.open(fileobj=io.BytesIO(raw))
    d = [tf.extractfile(m).read() for m in tf.getmembers() if m.name.endswith(".sigmf-data")][0]
    a = np.frombuffer(d, dtype="<i2")
    return a[0::2].astype(np.float64) + 1j*a[1::2].astype(np.float64)


def top_inband(x, fs, lo, k=12):
    N = 1 << 16; win = np.hanning(N); P = np.zeros(N); s = 0
    for i in range(0, len(x)-N, N):
        P += np.abs(np.fft.fftshift(np.fft.fft(x[i:i+N]*win)))**2; s += 1
    if s == 0: return []
    P /= s
    f = np.fft.fftshift(np.fft.fftfreq(N, 1/fs)) + lo   # absolute Hz
    from numpy.lib.stride_tricks import sliding_window_view
    W = 1501; floor = np.median(sliding_window_view(np.pad(P, W//2, mode="edge"), W), axis=1)
    ex = 10*np.log10((P+1e-9)/(floor+1e-9))
    inb = (f > 118e6) & (f < 128e6)
    cand = np.flatnonzero((ex > 12) & inb)
    out = []
    if len(cand):
        for g in np.split(cand, np.flatnonzero(np.diff(cand) > 5)+1):
            i = g[np.argmax(P[g])]; out.append((f[i], ex[i]))
    out.sort(key=lambda t: -t[1]); return out[:k]


def main():
    cur = json.loads(urllib.request.urlopen(B+"/api",timeout=10).read())["ad9361"]
    g0 = cur["rx_gain"]; lo0 = int(cur["rx_lo_frequency"]); fs0 = int(cur["sampling_frequency"])
    print(f"baseline gain={g0} lo={lo0} fs={fs0}")
    results = {}
    try:
        iio_set(["-i", "voltage0", "hardwaregain", "50"])
        iio_set(["altvoltage0", "frequency", str(LO)])
        for fs in [14_000_000, 15_360_000, 23_040_000, 30_720_000]:
            iio_set(["-i", "voltage0", "rf_bandwidth", str(fs)])
            iio_set(["-i", "voltage0", "sampling_frequency", str(fs)])
            time.sleep(0.8)
            act = int(float(iio_get(["-i", "voltage0", "sampling_frequency"]).split()[0]))
            pk = top_inband(record(), act, LO)
            results[act] = pk
            print(f"\n== Fs={act} ({len(pk)} in-band spurs, abs MHz) ==")
            for fabs, db in pk:
                h = fabs/40e6
                tag = f"  <= 40MHz x{h:.3f}" if abs(h-round(h)) < 0.004 else ""
                print(f"   {fabs/1e6:9.4f} MHz  {db:5.1f} dB{tag}")
    finally:
        iio_set(["-i", "voltage0", "rf_bandwidth", str(fs0)])
        iio_set(["-i", "voltage0", "sampling_frequency", str(fs0)])
        iio_set(["altvoltage0", "frequency", str(lo0)])
        iio_set(["-i", "voltage0", "hardwaregain", str(int(g0))])
        time.sleep(0.5)
        print(f"\nrestored gain={g0} lo={lo0} fs="
              f"{iio_get(['-i','voltage0','sampling_frequency'])}")
    # cross-Fs: which absolute freqs recur (fixed/physical) vs unique (movable)
    allf = [(round(fabs/1e4)*1e4, fs) for fs, pk in results.items() for fabs, _ in pk]
    from collections import defaultdict
    bins = defaultdict(set)
    for fb, fs in allf: bins[fb].add(fs)
    print("\n== absolute spur freq recurrence across Fs (physical if in many) ==")
    for fb in sorted(bins):
        n = len(bins[fb])
        if n >= 2:
            print(f"   {fb/1e6:9.3f} MHz  seen at {n}/{len(results)} sample rates")


if __name__ == "__main__":
    main()
