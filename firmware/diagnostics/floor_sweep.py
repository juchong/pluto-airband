#!/usr/bin/env python3
"""Sweep RX gain; for each, capture raw IQ via the maia recorder and report
clip%, wideband PSD noise floor (median dBFS), SFDR (worst spur above floor),
and the count of strong peaks. Restores gain on exit. Run OUTSIDE the sandbox."""
import json, time, io, tarfile, urllib.request, subprocess, os
import numpy as np

BASE = "http://192.168.2.1:8000"
FS = 14_000_000.0
FULL = 32768.0


def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=15) as r:
        return r.read()


def set_gain(g):
    subprocess.run(["iio_attr", "-u", "ip:192.168.2.1", "-i", "-c", "ad9361-phy",
                    "voltage0", "hardwaregain", str(int(g))],
                   capture_output=True, env={"PATH": os.environ["PATH"]})


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
    n = min(len(I), len(Q)); I, Q = I[:n], Q[:n]
    clip = np.mean((np.abs(I) >= 32000) | (np.abs(Q) >= 32000)) * 100
    x = (I + 1j * Q) / FULL
    # averaged periodogram (Welch) -> smooth wideband PSD in dBFS
    nps = 8192
    nseg = len(x) // nps
    w = np.hanning(nps); wn = (w ** 2).sum()
    acc = np.zeros(nps)
    for k in range(nseg):
        seg = x[k * nps:(k + 1) * nps] * w
        acc += np.abs(np.fft.fftshift(np.fft.fft(seg))) ** 2
    psd = acc / (nseg * wn)
    psd_db = 10 * np.log10(psd + 1e-20)
    f = np.fft.fftshift(np.fft.fftfreq(nps, 1 / FS)) / 1e6  # MHz offset from LO
    # in-band = central +/-6 MHz (avoid analog filter skirts at +/-7)
    band = np.abs(f) <= 6.0
    pdb = psd_db[band]
    floor = np.median(pdb)
    sfdr = pdb.max() - floor
    npk = int(np.sum(pdb > floor + 20))         # strong discrete peaks
    return clip, floor, sfdr, npk


def main():
    cur = json.loads(api("GET", "/api"))["ad9361"]
    g0 = cur["rx_gain"]
    print(f"baseline gain={g0} lo={int(cur['rx_lo_frequency'])} fir_en=0")
    print(f"{'gain':>5} {'clip%':>7} {'floor dBFS':>11} {'SFDR dB':>8} {'#pk>+20':>8}")
    try:
        for g in [71, 62, 55, 48, 40, 32, 24]:
            set_gain(g); time.sleep(0.5)
            clip, floor, sfdr, npk = measure(record_iq(0.3))
            print(f"{g:5.0f} {clip:7.3f} {floor:11.1f} {sfdr:8.1f} {npk:8d}")
    finally:
        set_gain(g0)
        print(f"restored gain={g0}")


if __name__ == "__main__":
    main()
