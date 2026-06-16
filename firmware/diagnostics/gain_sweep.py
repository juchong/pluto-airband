#!/usr/bin/env python3
"""Sweep rx_gain (and optionally LO), record raw IQ via maia recorder, and
measure wideband level + low-freq AM (buzz) strength. Restores state at end.
Restores device state on exit. Requires iio_attr on PATH and the maia recorder
at 192.168.2.1:8000. Run outside the sandbox (mutates live RF state)."""
import json, time, io, tarfile, urllib.request
import numpy as np

BASE = "http://192.168.2.1:8000"
FS = 14_000_000.0


def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as r:
        return r.read()


import subprocess
def set_gain(g):
    subprocess.run(["iio_attr", "-u", "ip:192.168.2.1", "-i", "-c", "ad9361-phy",
                    "voltage0", "hardwaregain", str(int(g))],
                   capture_output=True, env={"PATH": __import__("os").environ["PATH"]})


def set_lo(hz):
    api("PATCH", "/api/ad9361", {"rx_lo_frequency": int(hz)})


def record_iq(dur=0.3):
    api("PATCH", "/api/recorder",
        {"mode": "IQ16bit", "maximum_duration": dur, "state_change": "Start"})
    time.sleep(dur + 0.3)
    for _ in range(50):
        st = json.loads(api("GET", "/api/recorder"))["state"]
        if st == "Stopped":
            break
        time.sleep(0.1)
    raw = urllib.request.urlopen(BASE + "/recording", timeout=30).read()
    tf = tarfile.open(fileobj=io.BytesIO(raw))
    for m in tf.getmembers():
        if m.name.endswith(".sigmf-data"):
            d = tf.extractfile(m).read()
            return np.frombuffer(d, dtype="<i2")
    raise RuntimeError("no data")


def measure(raw):
    I = raw[0::2].astype(np.float64); Q = raw[1::2].astype(np.float64)
    rms = np.sqrt((I*I + Q*Q).mean())
    pk = np.sqrt((I*I + Q*Q).max())
    clip = np.mean((np.abs(I) >= 32000) | (np.abs(Q) >= 32000)) * 100
    p = I*I + Q*Q
    DEC = 1400; m = (len(p)//DEC)*DEC
    env = p[:m].reshape(-1, DEC).mean(axis=1); fr = FS/DEC
    env = env - env.mean()
    N = 1 << int(np.log2(len(env)))
    E = np.abs(np.fft.rfft(env[:N]*np.hanning(N)))**2
    f = np.fft.rfftfreq(N, 1/fr)
    am = E[(f > 10) & (f < 200)].sum()
    base = E[(f > 800) & (f < 2000)].sum() + 1e-9
    return rms, pk, clip, 10*np.log10(am/base)


def main():
    cur = json.loads(api("GET", "/api"))["ad9361"]
    g0 = cur["rx_gain"]; lo0 = int(cur["rx_lo_frequency"])
    print(f"baseline gain={g0} lo={lo0}")
    print(f"{'gain':>6} {'rms':>8} {'peak':>8} {'clip%':>7} {'AM/base dB':>11}")
    try:
        for g in [71, 60, 50, 40, 30, 20]:
            set_gain(g); time.sleep(0.4)
            rms, pk, clip, am = measure(record_iq(0.3))
            print(f"{g:6.0f} {rms:8.0f} {pk:8.0f} {clip:7.3f} {am:11.1f}")
    finally:
        set_gain(g0)
        print(f"restored gain={g0} lo={lo0}")


if __name__ == "__main__":
    main()
