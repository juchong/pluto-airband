#!/usr/bin/env python3
"""Localize a conducted spur comb with the RX input terminated (50 ohm).

Two tests, both via the maia recorder + direct iio (the /api/ad9361 path is a
no-op while --airband locks the front end, so LO/gain are set over libiio).
Device state (LO, gain) is restored on exit.

  1. LO-SHIFT  -- capture at LO and LO+df. A tooth at a fixed *absolute* freq
     (df tracks the LO) couples onto the RF input pre-mixer; a tooth at a fixed
     *baseband offset* (unchanged) is generated at/after the mixer (clock / ADC
     / digital / DC-region injection).
  2. GAIN SWEEP -- if the comb scales with RX gain it enters before/at the LNA;
     if its absolute ADC level is gain-invariant it is injected after the gain
     stage (clock/ADC reference / on-die digital).

Usage:  PLUTO_HOST=10.0.16.100 python term_tests.py
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
FS = 14_000_000.0
HOST = os.environ.get("PLUTO_HOST") or (sys.argv[1] if len(sys.argv) > 1
                                        else "10.0.16.100")
B = f"http://{HOST}:8000"
U = f"ip:{HOST}"
ENV = {"PATH": os.environ["PATH"]}


PW = os.environ.get("PLUTO_PW", "analog")


def iio(args):
    subprocess.run(["iio_attr", "-u", U, "-c", "ad9361-phy", *args],
                   capture_output=True, env=ENV)


def _api_up():
    try:
        urllib.request.urlopen(B + "/api", timeout=4).read()
        return True
    except Exception:
        return False


def ensure_up():
    """Make sure maia-httpd is listening; restart it over ssh if OOM-killed."""
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


def set_gain(g):
    iio(["-i", "voltage0", "hardwaregain", str(int(g))])


def set_lo(hz):
    iio(["altvoltage0", "frequency", str(int(hz))])


def _record_once(dur):
    urllib.request.urlopen(urllib.request.Request(
        B + "/api/recorder",
        data=json.dumps({"mode": "IQ16bit", "maximum_duration": dur,
                         "state_change": "Start"}).encode(),
        method="PATCH", headers={"Content-Type": "application/json"}),
        timeout=10).read()
    time.sleep(dur + 0.4)
    for _ in range(50):
        if json.loads(urllib.request.urlopen(B + "/api/recorder",
                                              timeout=10).read())["state"] == "Stopped":
            break
        time.sleep(0.1)
    time.sleep(0.2)
    # read the whole body, tolerating a truncated final chunk
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


def record(dur=0.12, on_ready=None):
    """Record IQ. ``on_ready`` is re-applied after every (re)start of maia-httpd,
    so LO/gain set by the caller survive an OOM restart (which reloads the config
    defaults gain=71/LO=123.438)."""
    last = None
    for attempt in range(5):
        ensure_up()
        if on_ready is not None:
            on_ready()
        try:
            x = _record_once(dur)
            if len(x) > 150_000:
                time.sleep(1.0)            # let device free recorder memory
                return x
        except Exception as e:
            last = e
        time.sleep(1.0)
    if last:
        raise last
    raise RuntimeError("record: too few samples after retries")


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


def floor_of(P, W=801):
    from numpy.lib.stride_tricks import sliding_window_view
    return np.median(sliding_window_view(np.pad(P, W // 2, mode="edge"), W), axis=1)


def top_peaks(f, P, k=30, thr=8.0, gap=6):
    fl = floor_of(P)
    ex = 10 * np.log10((P + 1e-9) / (fl + 1e-9))
    cand = np.flatnonzero(ex > thr)
    out = []
    if len(cand):
        for g in np.split(cand, np.flatnonzero(np.diff(cand) > gap) + 1):
            i = g[int(np.argmax(P[g]))]
            out.append((float(f[i]), float(ex[i])))
    out.sort(key=lambda t: -t[1])
    return out[:k]


def lo_shift_test(lo0, df=330_000):
    print(f"\n===== LO-SHIFT TEST (df = +{df/1e3:.0f} kHz) =====")
    f1, P1 = welch(record(on_ready=lambda: (set_lo(lo0), time.sleep(0.6))))
    f2, P2 = welch(record(on_ready=lambda: (set_lo(lo0 + df), time.sleep(0.6))))
    set_lo(lo0)
    pk1 = top_peaks(f1, P1, k=18)
    o2 = np.array([o for o, _ in top_peaks(f2, P2, k=80)])
    print("  tooth offset    abs(MHz)   dB   verdict   dOffset  dAbs(should be ~-df if ABS)")
    noff = nabs = 0
    for o, db in pk1:
        d_off = (o2[np.argmin(np.abs(o2 - o))] - o) if len(o2) else 1e9
        # if absolute-fixed, in the shifted capture the tooth moves to offset o-df
        d_abs = (o2[np.argmin(np.abs(o2 - (o - df)))] - (o - df)) if len(o2) else 1e9
        if abs(d_off) < 15e3:
            tag, noff = "OFFSET(baseband/clk)", noff + 1
        elif abs(d_abs) < 15e3:
            tag, nabs = "ABS(RF-coupled)     ", nabs + 1
        else:
            tag = "?                   "
        print(f"  {o/1e3:+9.1f}kHz {(lo0+o)/1e6:8.3f} {db:5.1f}  {tag}"
              f"  {d_off/1e3:+7.1f} {d_abs/1e3:+7.1f}")
    print(f"\n  fixed-OFFSET (baseband/clock/ADC) = {noff}   "
          f"fixed-ABS (RF-coupled) = {nabs}")
    return (f1, P1, f2, P2)


def gain_test(lo0, gains=(71, 50, 30, 15)):
    print("\n===== GAIN SWEEP (comb vs RX gain, terminated input) =====")
    set_lo(lo0)
    time.sleep(0.4)
    print(f"{'gain':>5} {'floorRMS':>10} {'topTooth_dBoverFloor':>22} "
          f"{'tooth_absdBFS':>14}")
    rows = []
    for g in gains:
        def ready(g=g):
            set_lo(lo0)
            set_gain(g)
            time.sleep(0.5)
        x = record(0.12, on_ready=ready)
        f, P = welch(x)
        fl = floor_of(P)
        ex = 10 * np.log10((P + 1e-9) / (fl + 1e-9))
        # ignore DC bin cluster
        m = np.abs(f) > 20000
        top = float(np.max(ex[m]))
        # absolute level of the strongest tooth, in dBFS (full scale = 32768)
        kpk = np.flatnonzero(m)[np.argmax(ex[m])]
        amp = np.sqrt(P[kpk])
        abs_dbfs = 20 * np.log10(amp / (np.sum(np.hanning(len(P))) * 32768 / 2) + 1e-12)
        floor_rms = float(np.sqrt(np.median(P)))
        rows.append((g, floor_rms, top, abs_dbfs))
        print(f"{g:5.0f} {floor_rms:10.1f} {top:22.1f} {abs_dbfs:14.1f}")
    return rows


def lo_corr_test(lo0, df=2_500_000):
    """Robust fixed-ABS vs fixed-OFFSET test: shift the LO by a large amount (not a
    comb multiple) and CROSS-CORRELATE the whole comb instead of matching teeth.

      baseband/clock-referenced comb -> identical in offset coords -> corr peaks at lag 0
      RF-coupled (fixed absolute)     -> comb shifts by -df in offset -> corr peaks at +df
    """
    print(f"\n===== LO CROSS-CORRELATION TEST (df = +{df/1e6:.2f} MHz) =====")
    f1, P1 = welch(record(on_ready=lambda: (set_lo(lo0), time.sleep(0.6))))
    f2, P2 = welch(record(on_ready=lambda: (set_lo(lo0 + df), time.sleep(0.6))))
    set_lo(lo0)

    def excess(P):
        fl = floor_of(P)
        e = 10 * np.log10((P + 1e-9) / (fl + 1e-9))
        return np.clip(e, 0, None)

    e1, e2 = excess(P1), excess(P2)
    bin_hz = f1[1] - f1[0]
    df_bins = int(round(df / bin_hz))

    def ncorr(a, b):
        a = a - a.mean()
        b = b - b.mean()
        d = np.sqrt((a @ a) * (b @ b)) + 1e-12
        return float((a @ b) / d)

    c0 = ncorr(e1, e2)                              # baseband-fixed hypothesis
    c_abs = ncorr(e1, np.roll(e2, df_bins))          # absolute-fixed hypothesis
    # scan a small range of lags to find the true peak
    lags = range(-df_bins - 200, df_bins + 201, 50)
    best = max(lags, key=lambda L: ncorr(e1, np.roll(e2, L)))
    print(f"  corr(lag=0, BASEBAND-fixed)   = {c0:+.3f}")
    print(f"  corr(lag=+df, ABSOLUTE-fixed) = {c_abs:+.3f}")
    print(f"  best lag over scan            = {best * bin_hz / 1e3:+.0f} kHz "
          f"(0 => baseband/clock; {df/1e3:+.0f} => RF/absolute)")
    verdict = ("BASEBAND/CLOCK-referenced (moves with LO)" if c0 > c_abs
               else "ABSOLUTE/RF-coupled (fixed RF frequency)")
    print(f"  => comb is {verdict}")
    return c0, c_abs, best * bin_hz


def main():
    ensure_up()
    if "corr" in sys.argv:
        cur = json.loads(urllib.request.urlopen(B + "/api", timeout=10).read())["ad9361"]
        g0, lo0 = cur["rx_gain"], int(cur["rx_lo_frequency"])
        print(f"baseline: gain={g0} dB  LO={lo0} Hz   host={HOST}")
        try:
            lo_corr_test(lo0)
        finally:
            set_lo(lo0)
            set_gain(g0)
            print(f"restored gain={g0} LO={lo0}")
        return
    cur = json.loads(urllib.request.urlopen(B + "/api", timeout=10).read())["ad9361"]
    g0 = cur["rx_gain"]
    lo0 = int(cur["rx_lo_frequency"])
    print(f"baseline: gain={g0} dB  LO={lo0} Hz   host={HOST}")
    caps = None
    rows = None
    try:
        caps = lo_shift_test(lo0)
        rows = gain_test(lo0)
    finally:
        set_lo(lo0)
        set_gain(g0)
        print(f"\nrestored gain={g0} LO={lo0}")
    if caps:
        _plot(caps, rows, lo0)


def _plot(caps, rows, lo0):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    OUT_DIR.mkdir(exist_ok=True)
    f1, P1, f2, P2 = caps
    df = 330_000
    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(13, 8))

    a1 = 10 * np.log10(P1 + 1e-9)
    a1 -= np.median(a1)
    # shift capture2 back by df so a fixed-ABS tooth would line up with capture1
    a2 = 10 * np.log10(P2 + 1e-9)
    a2 -= np.median(a2)
    ax1.plot(f1 / 1e6, a1, lw=0.6, label=f"LO={lo0/1e6:.3f}")
    ax1.plot((f2 + df) / 1e6, a2, lw=0.6, alpha=0.7,
             label=f"LO+{df/1e3:.0f}kHz (offset axis shifted back by df)")
    ax1.set_title("LO-shift overlay: teeth that line up here are FIXED-ABSOLUTE "
                  "(RF-coupled); teeth that move are baseband/clock")
    ax1.set_xlabel("baseband offset from LO (MHz)")
    ax1.set_ylabel("dB rel floor")
    ax1.legend(fontsize=8)
    ax1.grid(True, alpha=0.25)

    if rows:
        g = [r[0] for r in rows]
        top = [r[2] for r in rows]
        absd = [r[3] for r in rows]
        ax2.plot(g, top, "o-", label="tooth dB over local floor")
        ax2.plot(g, absd, "s--", label="tooth absolute level (dBFS)")
        ax2.set_xlabel("RX gain (dB)")
        ax2.set_ylabel("dB")
        ax2.set_title("Gain sweep: flat absolute level => injected after gain "
                      "(clock/ADC); rising with gain => pre-LNA")
        ax2.legend(fontsize=8)
        ax2.grid(True, alpha=0.25)
        ax2.invert_xaxis()
    fig.tight_layout()
    path = OUT_DIR / "term_tests.png"
    fig.savefig(path, dpi=130)
    print(f"Saved plot: {path}")


if __name__ == "__main__":
    main()
