#!/usr/bin/env python3
"""Capture a wideband IQ snapshot via the maia recorder and characterize the band.

Unlike ``wideband_spectrum.py`` this is host/IP-parametrized and adds explicit
**comb detection** (peak spacing + cepstrum) so a uniform spur comb (e.g. an
on-board switching-regulator/clock artifact) is identified and measured. Saves a
PNG (spectrum + channel markers + detected comb lines) for inspection.

Usage:
    PLUTO_HOST=10.0.16.100 python band_snapshot.py [dur_s]
    python band_snapshot.py 10.0.16.100 [dur_s]

Recorder-only (does not change RF state).
"""
import io
import json
import os
import pathlib
import sys
import tarfile
import time
import urllib.request

import numpy as np

OUT_DIR = pathlib.Path(__file__).parent / "out"

FS = 14_000_000.0
LO = 123_438_000.0
CH = [118.05e6, 119.2e6, 119.9e6, 120.1e6, 120.4e6, 120.95e6, 121.5e6, 121.6e6,
      121.7e6, 122.275e6, 122.95e6, 122.975e6, 123.9e6, 124.7e6, 125.6e6,
      125.9e6, 126.25e6, 126.5e6, 126.875e6, 127.1e6, 128.5e6]


import subprocess

PW = os.environ.get("PLUTO_PW", "analog")


def _host_of(base):
    return base.split("//", 1)[1].split(":", 1)[0]


def _api_up(base):
    try:
        urllib.request.urlopen(base + "/api", timeout=4).read()
        return True
    except Exception:
        return False


def ensure_up(base):
    """Restart maia-httpd over ssh if it OOM-died (96 MB Pluto+); re-apply gain."""
    host = _host_of(base)
    if not _api_up(base):
        print("  maia-httpd down -- restarting over ssh ...")
        subprocess.run(["sshpass", "-p", PW, "ssh", "-o",
                        "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=8",
                        f"root@{host}", "/etc/init.d/S60maia-httpd restart"],
                       capture_output=True, env={"PATH": os.environ["PATH"]})
        for _ in range(30):
            time.sleep(2)
            if _api_up(base):
                time.sleep(2)
                print("  maia-httpd back up.")
                break
        else:
            raise RuntimeError("maia-httpd did not come back up")
    # a restart reloads built-in config (gain 71); re-apply requested gain
    g = os.environ.get("PLUTO_GAIN")
    if g:
        subprocess.run(["iio_attr", "-u", f"ip:{host}", "-c", "ad9361-phy",
                        "-i", "voltage0", "hardwaregain", str(int(float(g)))],
                       capture_output=True, env={"PATH": os.environ["PATH"]})
        time.sleep(0.4)


def _record_once(base, dur):
    urllib.request.urlopen(urllib.request.Request(
        base + "/api/recorder",
        data=json.dumps({"mode": "IQ16bit", "maximum_duration": dur,
                         "state_change": "Start"}).encode(),
        method="PATCH", headers={"Content-Type": "application/json"}),
        timeout=10).read()
    time.sleep(dur + 0.4)
    for _ in range(50):
        st = json.loads(urllib.request.urlopen(base + "/api/recorder",
                                                timeout=10).read())["state"]
        if st == "Stopped":
            break
        time.sleep(0.1)
    time.sleep(0.2)
    resp = urllib.request.urlopen(base + "/recording", timeout=60)
    chunks = []
    try:
        while True:
            b = resp.read(1 << 20)
            if not b:
                break
            chunks.append(b)
    except Exception:
        pass                                   # tolerate a truncated final chunk
    raw = b"".join(chunks)
    tf = tarfile.open(fileobj=io.BytesIO(raw))
    d = [tf.extractfile(m).read() for m in tf.getmembers()
         if m.name.endswith(".sigmf-data")][0]
    a = np.frombuffer(d, dtype="<i2")
    return a[0::2].astype(np.float64) + 1j * a[1::2].astype(np.float64)


def record(base, dur=0.5):
    last = None
    for _ in range(5):
        ensure_up(base)
        try:
            x = _record_once(base, dur)
            if len(x) > 150_000:
                time.sleep(0.8)
                return x
        except Exception as e:
            last = e
        time.sleep(0.8)
    if last:
        raise last
    raise RuntimeError("record: too few samples after retries")


def welch(x, N=1 << 15):
    win = np.hanning(N)
    P = np.zeros(N)
    segs = 0
    for i in range(0, len(x) - N, N // 2):
        X = np.fft.fftshift(np.fft.fft(x[i:i + N] * win))
        P += np.abs(X) ** 2
        segs += 1
    P /= max(segs, 1)
    f = np.fft.fftshift(np.fft.fftfreq(N, 1 / FS))
    return f, P, segs


def find_peaks(P, floor, min_excess_db=8.0, min_gap=8):
    excess = 10 * np.log10((P + 1e-9) / (floor + 1e-9))
    cand = np.flatnonzero(excess > min_excess_db)
    peaks = []
    if len(cand):
        for g in np.split(cand, np.flatnonzero(np.diff(cand) > min_gap) + 1):
            k = g[int(np.argmax(P[g]))]
            peaks.append(k)
    return np.array(peaks, dtype=int), excess


def main():
    args = [a for a in sys.argv[1:]]
    host = os.environ.get("PLUTO_HOST")
    dur = 0.5
    label = "snapshot"
    out_override = None
    for a in args:
        if a.startswith("--out="):
            out_override = a.split("=", 1)[1]
        elif a.count(".") == 3 and a.replace(".", "").isdigit():
            host = a
        else:
            try:
                dur = float(a)
            except ValueError:
                label = a                      # free-form condition tag for the filename
    if not host:
        host = "10.0.16.100"
    base = f"http://{host}:8000"
    print(f"recording {dur}s from {base} ...")
    x = record(base, dur)
    n = len(x)
    print(f"samples={n}  dur={n / FS * 1000:.0f} ms")
    # read RX gain AFTER capture (service is up by now) for title + filename
    try:
        gain = json.loads(urllib.request.urlopen(base + "/api", timeout=6)
                          .read())["ad9361"]["rx_gain"]
    except Exception:
        gain = None

    f, P, segs = welch(x)
    print(f"averaged {segs} FFT segments (N={len(P)})")

    # moving-median noise floor
    from numpy.lib.stride_tricks import sliding_window_view
    W = 1001
    pad = np.pad(P, W // 2, mode="edge")
    floor = np.median(sliding_window_view(pad, W), axis=1)

    # DC / LO leakage
    med = np.median(P)
    dc = P[np.abs(f) < 5000].max()
    print(f"\nDC/LO-leak spike: {10 * np.log10(dc / med):.1f} dB above median floor")

    # discrete peaks
    peaks, excess = find_peaks(P, floor, min_excess_db=8.0)
    pk_off = f[peaks]
    # ignore the DC bin cluster for comb spacing
    mask = np.abs(pk_off) > 20000
    comb_off = np.sort(pk_off[mask])
    print(f"\n{len(comb_off)} discrete peaks (>8 dB over local floor, |off|>20kHz)")

    spacing_khz = float("nan")
    if len(comb_off) >= 4:
        diffs = np.diff(comb_off)
        diffs = diffs[diffs > 20000]            # drop tightly-split clusters
        if len(diffs):
            spacing_khz = float(np.median(diffs)) / 1e3
        # cepstrum: periodicity of the (peak-only) spectrum
        spec = np.zeros(len(f))
        spec[peaks[mask]] = excess[peaks[mask]]
        ceps = np.abs(np.fft.rfft(spec - spec.mean()))
        q = np.fft.rfftfreq(len(spec), d=(f[1] - f[0]))  # 1/Hz -> "quefrency" in s
        # convert quefrency to a frequency spacing: spacing = 1/quefrency
        q[0] = np.nan
        sp_from_ceps = 1.0 / q
        valid = (sp_from_ceps > 50e3) & (sp_from_ceps < 3e6)
        if valid.any():
            kbest = np.nanargmax(np.where(valid, ceps, np.nan))
            ceps_spacing_khz = sp_from_ceps[kbest] / 1e3
        else:
            ceps_spacing_khz = float("nan")
        print(f"comb spacing: median peak gap = {spacing_khz:.1f} kHz, "
              f"cepstral estimate = {ceps_spacing_khz:.1f} kHz")
        print("\nfirst 20 comb lines (offset from LO, abs MHz):")
        for off in comb_off[:20]:
            print(f"  {off / 1e3:+9.1f} kHz   {(LO + off) / 1e6:8.3f} MHz")

    # band statistics
    PdB = 10 * np.log10(P + 1e-9)
    PdB -= np.median(PdB)
    print(f"\nfloor(median)=0 dB ref; peak excursion = {PdB.max():.1f} dB; "
          f"95th pct = {np.percentile(PdB, 95):.1f} dB")

    _plot(f, P, floor, peaks, mask, host, spacing_khz, gain, label, out_override)


def _plot(f, P, floor, peaks, mask, host, spacing_khz, gain=None,
          label="snapshot", out_override=None):
    import datetime
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    OUT_DIR.mkdir(exist_ok=True)
    fabs = (LO + f) / 1e6
    PdB = 10 * np.log10(P + 1e-9)
    ref = np.median(PdB)
    PdB -= ref
    floordB = 10 * np.log10(floor + 1e-9) - ref

    fig, ax = plt.subplots(figsize=(13, 4.5))
    ax.plot(fabs, PdB, lw=0.6, color="tab:blue", label="PSD")
    ax.plot(fabs, floordB, lw=0.8, color="0.5", ls="--", label="local floor")
    cl = peaks[mask]
    ax.scatter(fabs[cl], PdB[cl], s=14, color="tab:red", zorder=5,
               label=f"comb lines ({len(cl)})")
    for c in CH:
        ax.axvline(c / 1e6, color="tab:green", lw=0.8, alpha=0.45)
    ax.axvline(LO / 1e6, color="k", ls=":", lw=1, label="LO / DC")
    gtag = f"gain {gain:.0f} dB" if gain is not None else "gain ?"
    title = f"Band snapshot {host} [{label}, {gtag}] - Fs=14M, LO=123.438M"
    if not np.isnan(spacing_khz):
        title += f"  | comb ~{spacing_khz:.0f} kHz"
    ax.set_title(title)
    ax.set_xlabel("frequency (MHz)")
    ax.set_ylabel("dB (rel median floor)")
    ax.set_xlim(fabs[0], fabs[-1])
    ax.legend(loc="upper right", fontsize=8, ncol=2)
    ax.grid(True, alpha=0.25)
    fig.tight_layout()
    if out_override:
        path = pathlib.Path(out_override)
    else:
        date = datetime.date.today().isoformat()
        gnum = f"gain{int(round(gain))}" if gain is not None else "gainNA"
        path = OUT_DIR / f"band_{label}_{gnum}_{date}.png"
    fig.savefig(path, dpi=130)
    print(f"\nSaved plot: {path}")


if __name__ == "__main__":
    main()
