#!/usr/bin/env python3
"""LO-shift A/B: does a tooth track the LO (fixed baseband offset -> ADC/sample-
clock / baseband-domain, dodgeable by frequency planning) or stay at a fixed
absolute RF frequency (external/board source)?

Fs is held at 14 MHz, gain fixed, RX input terminated. The LO is stepped +/-0.5 MHz
and each tooth is reported in BOTH coordinates. Self-heals maia-httpd over ssh on
OOM (96 MB Pluto+) and re-applies LO+gain after any restart. Restores LO on exit.

Usage:  PLUTO_HOST=10.0.16.100 PLUTO_GAIN=48 python lo_track_test.py
"""
import io
import json
import os
import pathlib
import subprocess
import sys
import tarfile
import time
import urllib.request

import numpy as np

OUT_DIR = pathlib.Path(__file__).parent / "out"
HOST = os.environ.get("PLUTO_HOST") or (sys.argv[1] if len(sys.argv) > 1
                                        else "10.0.16.100")
B = f"http://{HOST}:8000"
U = f"ip:{HOST}"
PW = os.environ.get("PLUTO_PW", "analog")
ENV = {"PATH": os.environ["PATH"]}
GAIN = int(os.environ.get("PLUTO_GAIN", "48"))
LABEL = os.environ.get("PLUTO_LABEL", "intVCTCXO")
FS = 14_000_000.0
LO0 = 123_438_000
LO_STEPS = [LO0, LO0 + 500_000, LO0 - 500_000]


def iio(args):
    subprocess.run(["iio_attr", "-u", U, "-c", "ad9361-phy", *args],
                   capture_output=True, env=ENV)


def set_gain(g):
    iio(["-i", "voltage0", "hardwaregain", str(int(g))])


def set_lo(hz):
    iio(["altvoltage0", "frequency", str(int(hz))])


def _api_up():
    try:
        urllib.request.urlopen(B + "/api", timeout=4).read()
        return True
    except Exception:
        return False


def ensure_up():
    if _api_up():
        return
    print("  maia-httpd down -- restarting over ssh ...")
    subprocess.run(["sshpass", "-p", PW, "ssh", "-o",
                    "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=8",
                    f"root@{HOST}", "/etc/init.d/S60maia-httpd restart"],
                   capture_output=True, env=ENV)
    for _ in range(30):
        time.sleep(2)
        if _api_up():
            time.sleep(2)
            print("  maia-httpd back up.")
            return
    raise RuntimeError("maia-httpd did not come back up")


def _record_once(dur):
    urllib.request.urlopen(urllib.request.Request(
        B + "/api/recorder",
        data=json.dumps({"mode": "IQ16bit", "maximum_duration": dur,
                         "state_change": "Start"}).encode(),
        method="PATCH", headers={"Content-Type": "application/json"}),
        timeout=10).read()
    time.sleep(dur + 0.4)
    for _ in range(60):
        if json.loads(urllib.request.urlopen(B + "/api/recorder",
                                              timeout=10).read())["state"] == "Stopped":
            break
        time.sleep(0.1)
    time.sleep(0.2)
    resp = urllib.request.urlopen(B + "/recording", timeout=60)
    chunks = []
    try:
        while True:
            b = resp.read(1 << 20)
            if not b:
                break
            chunks.append(b)
    except Exception:
        pass
    raw = b"".join(chunks)
    tf = tarfile.open(fileobj=io.BytesIO(raw))
    d = [tf.extractfile(m).read() for m in tf.getmembers()
         if m.name.endswith(".sigmf-data")][0]
    a = np.frombuffer(d, dtype="<i2")
    return a[0::2].astype(np.float64) + 1j * a[1::2].astype(np.float64)


def record(lo, dur=0.12):
    last = None
    for _ in range(5):
        ensure_up()
        set_lo(lo)
        set_gain(GAIN)
        time.sleep(0.6)
        try:
            x = _record_once(dur)
            if len(x) > 120_000:
                time.sleep(0.8)
                return x
        except Exception as e:
            last = e
        time.sleep(1.0)
    if last:
        raise last
    raise RuntimeError("record failed")


def welch(x, N=1 << 14):
    win = np.hanning(N)
    P = np.zeros(N)
    s = 0
    for i in range(0, len(x) - N, N // 2):
        P += np.abs(np.fft.fftshift(np.fft.fft(x[i:i + N] * win))) ** 2
        s += 1
    P /= max(s, 1)
    f = np.fft.fftshift(np.fft.fftfreq(N, 1 / FS))
    return f, P


def teeth(f, P, thr=8.0):
    from numpy.lib.stride_tricks import sliding_window_view
    W = 801
    floor = np.median(sliding_window_view(np.pad(P, W // 2, mode="edge"), W), axis=1)
    ex = 10 * np.log10((P + 1e-9) / (floor + 1e-9))
    cand = np.flatnonzero((ex > thr) & (np.abs(f) > 20000))
    out = []
    if len(cand):
        for g in np.split(cand, np.flatnonzero(np.diff(cand) > 6) + 1):
            i = g[int(np.argmax(P[g]))]
            out.append((float(f[i]), float(ex[i])))
    out.sort(key=lambda t: -t[1])
    return out


def main():
    ensure_up()
    caps = {}
    try:
        for lo in LO_STEPS:
            x = record(lo)
            f, P = welch(x)
            tl = teeth(f, P)
            caps[lo] = (f, P, tl)
            print(f"\n== LO={lo/1e6:.3f} MHz: {len(tl)} teeth (offset kHz | abs MHz | dB) ==")
            for off, db in tl[:8]:
                print(f"   {off/1e3:+8.1f} kHz  | {(lo+off)/1e6:8.3f} MHz | {db:4.1f}")
    finally:
        ensure_up()
        set_lo(LO0)
        print(f"\nrestored LO={LO0}")

    _classify(caps)
    _plot(caps)


def _classify(caps):
    los = list(caps)
    ref = LO0
    if ref not in caps:
        ref = los[0]
    _, _, t_ref = caps[ref]
    tol = 30e3
    print("\n===== TRACKING (does each tooth follow the LO?) =====")
    print("  tooth@refLO   dB   verdict             detail")
    for off, db in t_ref[:8]:
        votes_off = votes_abs = 0
        det = []
        for lo in los:
            if lo == ref:
                continue
            offs = np.array([o for o, _ in caps[lo][2]])
            if not len(offs):
                continue
            d_off = np.min(np.abs(offs - off))                    # fixed baseband offset
            d_abs = np.min(np.abs((lo + offs) - (ref + off)))     # fixed absolute freq
            if d_off < d_abs and d_off < tol:
                votes_off += 1
            elif d_abs < d_off and d_abs < tol:
                votes_abs += 1
            det.append(f"{(lo-ref)/1e3:+.0f}k:off{d_off/1e3:+.0f}/abs{d_abs/1e3:+.0f}")
        if votes_off > votes_abs:
            v = "TRACKS-LO (ADC/baseband)"
        elif votes_abs > votes_off:
            v = "FIXED-RF (board/ext)"
        else:
            v = "ambiguous"
        print(f"   {(ref+off)/1e6:8.3f}  {db:4.1f}  {v:22s} {' '.join(det)}")


def _plot(caps):
    import datetime
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    OUT_DIR.mkdir(exist_ok=True)
    los = list(caps)
    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(13, 8))
    for lo in los:
        f, P, _ = caps[lo]
        PdB = 10 * np.log10(P + 1e-9)
        PdB -= np.median(PdB)
        ax1.plot((lo + f) / 1e6, PdB, lw=0.6, label=f"LO={lo/1e6:.3f}")
        ax2.plot(f / 1e6, PdB, lw=0.6, label=f"LO={lo/1e6:.3f}")
    ax1.set_title("Absolute frequency -- teeth that ALIGN are FIXED-RF "
                  "(board/external: e.g. GbE 125 MHz, ref 120 MHz)")
    ax1.set_xlabel("frequency (MHz)")
    ax1.set_ylabel("dB rel floor")
    ax1.legend(fontsize=8)
    ax1.grid(True, alpha=0.25)
    ax2.set_title("Baseband offset from LO -- teeth that ALIGN TRACK the LO "
                  "(ADC/sample-clock / baseband; dodgeable by frequency planning)")
    ax2.set_xlabel("offset from LO (MHz)")
    ax2.set_ylabel("dB rel floor")
    ax2.legend(fontsize=8)
    ax2.grid(True, alpha=0.25)
    fig.tight_layout()
    date = datetime.date.today().isoformat()
    path = OUT_DIR / f"lo_track_{LABEL}_gain{GAIN}_{date}.png"
    fig.savefig(path, dpi=130)
    print(f"Saved plot: {path}")


if __name__ == "__main__":
    main()
