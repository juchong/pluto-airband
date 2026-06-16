#!/usr/bin/env python3
"""Internal-vs-external test: at a fixed clean gain, record raw IQ at several
LO frequencies and compare wideband level + low-freq AM. Restores state.
Restores device state on exit. Requires iio_attr on PATH and the maia recorder
at 192.168.2.1:8000. Run outside the sandbox (mutates live RF state)."""
import json, time, io, tarfile, urllib.request, subprocess, os
import numpy as np

BASE = "http://192.168.2.1:8000"
U = "ip:192.168.2.1"
FS = 14_000_000.0
ENV = {"PATH": os.environ["PATH"]}


def iio_set(chan, attr, val, out=False):
    args = ["iio_attr", "-u", U, "-c", "ad9361-phy", chan, attr, str(int(val))]
    subprocess.run(args, capture_output=True, env=ENV)


def set_gain(g):
    subprocess.run(["iio_attr", "-u", U, "-i", "-c", "ad9361-phy", "voltage0",
                    "hardwaregain", str(int(g))], capture_output=True, env=ENV)


def set_lo(hz):
    subprocess.run(["iio_attr", "-u", U, "-c", "ad9361-phy", "altvoltage0",
                    "frequency", str(int(hz))], capture_output=True, env=ENV)


def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as r:
        return r.read()


def record_iq(dur=0.3):
    api("PATCH", "/api/recorder",
        {"mode": "IQ16bit", "maximum_duration": dur, "state_change": "Start"})
    time.sleep(dur + 0.3)
    for _ in range(50):
        if json.loads(api("GET", "/api/recorder"))["state"] == "Stopped":
            break
        time.sleep(0.1)
    raw = urllib.request.urlopen(BASE + "/recording", timeout=30).read()
    tf = tarfile.open(fileobj=io.BytesIO(raw))
    for m in tf.getmembers():
        if m.name.endswith(".sigmf-data"):
            return np.frombuffer(tf.extractfile(m).read(), dtype="<i2")
    raise RuntimeError("no data")


def measure(raw):
    I = raw[0::2].astype(np.float64); Q = raw[1::2].astype(np.float64)
    rms = np.sqrt((I*I + Q*Q).mean())
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
    return rms, clip, 10*np.log10(am/base)


def main():
    cur = json.loads(api("GET", "/api"))["ad9361"]
    g0 = cur["rx_gain"]; lo0 = int(cur["rx_lo_frequency"])
    print(f"baseline gain={g0} lo={lo0}")
    set_gain(50); time.sleep(0.3)
    print(f"{'LO (Hz)':>12} {'rms':>8} {'clip%':>7} {'AM dB':>7}")
    try:
        for lo in [123438000, 75000000, 200000000, 433000000, 868000000]:
            set_lo(lo); time.sleep(0.6)
            rms, clip, am = measure(record_iq(0.3))
            print(f"{lo:12d} {rms:8.0f} {clip:7.3f} {am:7.1f}")
    finally:
        set_lo(lo0); set_gain(g0)
        print(f"restored gain={g0} lo={lo0}")


if __name__ == "__main__":
    main()
