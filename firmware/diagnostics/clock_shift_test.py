#!/usr/bin/env python3
"""Digital-clock (Fs) shift test: is each dominant spur tooth FIXED or Fs-LOCKED?

This is the test that must precede any switcher-side ferrite/decoupling work. The
power-source and reference A/Bs both held Fs constant, so neither could separate
the on-board DC-DC switcher from PL-fabric / ADC-clock switching noise. Here we
move only the AD9361 sample-rate / data clock (Fs, LO and gain fixed) with the RX
input terminated, and watch each tooth:

  * a tooth whose baseband offset is constant in **Hz** across Fs is FIXED
    (on-board switcher / physical / a fixed fabric clock) -- a series bead between
    the switcher and the bulk caps can attenuate it.
  * a tooth whose baseband offset **scales with Fs** is ADC/data-clock (digital)
    -- the bead will NOT help (and may hurt Zynq core transient response), because
    those currents are drawn from the cap side of the bead.

Fs is swept DOWNWARD only (>= timing margin; the design is closed at the nominal
clock). Self-heals maia-httpd over ssh on OOM (96 MB Pluto+) and re-applies Fs +
gain after any restart. Restores Fs on exit.

Usage:  PLUTO_HOST=10.0.16.100 python clock_shift_test.py
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
# Fs sweep: downward only (timing-safe). Default is NON-commensurate so the
# alias-solver isn't ambiguous (a commensurate integer-MHz set has a small LCM, so
# alias solutions repeat every LCM MHz). Override via PLUTO_FS="14.0,12.3,9.1".
if os.environ.get("PLUTO_FS"):
    FS_LIST = [int(round(float(x) * 1e6)) for x in os.environ["PLUTO_FS"].split(",")]
else:
    FS_LIST = [14_000_000, 12_300_000, 9_100_000]
LABEL = os.environ.get("PLUTO_LABEL", "intVCTCXO")


def iio(args):
    subprocess.run(["iio_attr", "-u", U, "-c", "ad9361-phy", *args],
                   capture_output=True, env=ENV)


def set_gain(g):
    iio(["-i", "voltage0", "hardwaregain", str(int(g))])


def set_fs(fs):
    iio(["-i", "voltage0", "rf_bandwidth", str(int(fs))])
    iio(["-i", "voltage0", "sampling_frequency", str(int(fs))])


def api():
    return json.loads(urllib.request.urlopen(B + "/api", timeout=6).read())["ad9361"]


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


def record(fs, dur=0.12):
    """Set Fs+gain (re-applied after any OOM restart), capture, return (x, actual_fs)."""
    last = None
    for _ in range(5):
        ensure_up()
        set_fs(fs)
        set_gain(GAIN)
        time.sleep(0.6)
        try:
            x = _record_once(dur)
            if len(x) > 120_000:
                act = int(float(api()["sampling_frequency"]))
                time.sleep(0.8)
                return x, act
        except Exception as e:
            last = e
        time.sleep(1.0)
    if last:
        raise last
    raise RuntimeError("record failed")


def welch(x, fs, N=1 << 14):
    win = np.hanning(N)
    P = np.zeros(N)
    s = 0
    for i in range(0, len(x) - N, N // 2):
        P += np.abs(np.fft.fftshift(np.fft.fft(x[i:i + N] * win))) ** 2
        s += 1
    P /= max(s, 1)
    f = np.fft.fftshift(np.fft.fftfreq(N, 1 / fs))       # baseband offset (Hz)
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
    cur = api()
    lo0 = int(cur["rx_lo_frequency"])
    fs0 = int(float(cur["sampling_frequency"]))
    g0 = cur["rx_gain"]
    print(f"baseline: LO={lo0} Fs={fs0} gain={g0}  (sweeping Fs down, gain->{GAIN})")

    caps = {}
    try:
        for fs in FS_LIST:
            x, act = record(fs)
            f, P = welch(x, act)
            tl = teeth(f, P)
            caps[act] = (f, P, tl)
            print(f"\n== Fs={act/1e6:.3f} MHz: {len(tl)} teeth (offset kHz, abs MHz, dB) ==")
            for off, db in tl[:10]:
                print(f"   {off/1e3:+8.1f} kHz  {(lo0+off)/1e6:8.3f} MHz  {db:4.1f}")
    finally:
        ensure_up()
        set_fs(14_000_000)
        set_gain(int(g0) if g0 else 48)
        print(f"\nrestored Fs=14M gain={g0}")

    _classify(caps, lo0)
    _alias_solve(caps, lo0)
    _digital_solve(caps)
    _plot(caps, lo0)


def _ex_arr(P, W=801):
    from numpy.lib.stride_tricks import sliding_window_view
    floor = np.median(sliding_window_view(np.pad(P, W // 2, mode="edge"), W), axis=1)
    return 10 * np.log10((P + 1e-9) / (floor + 1e-9))


def _alias_solve(caps, lo0, thr=8.0):
    """Find fixed ABSOLUTE frequencies F that alias to a strong in-band line at
    EVERY Fs (complex-baseband: offset = ((F-LO+Fs/2) mod Fs) - Fs/2). A consistent
    out-of-band F => a fixed aggressor (switcher harmonic / clock / EMI) folding in;
    decouple/identify at source. Uses full spectra, robust to the peak picker."""
    rates = sorted(caps)
    Fg = np.arange(20e6, 1200e6, 2e3)
    mins = np.full(Fg.shape, 1e9)
    for fs in rates:
        f, P, _ = caps[fs]
        ex = _ex_arr(P)
        df = f[1] - f[0]
        pred = ((Fg - lo0 + fs / 2) % fs) - fs / 2
        idx = np.clip(np.round((pred - f[0]) / df).astype(int), 0, len(f) - 1)
        mins = np.minimum(mins, ex[idx])
    cand = np.flatnonzero(mins > thr)
    groups = []
    if len(cand):
        for g in np.split(cand, np.flatnonzero(np.diff(cand) > 50) + 1):
            k = g[int(np.argmax(mins[g]))]
            groups.append((Fg[k], mins[k]))
    groups.sort(key=lambda t: -t[1])
    print("\n===== ALIAS SOLVE: fixed absolute F that aliases in at ALL Fs =====")
    if not groups:
        print("  none — no single fixed out-of-band frequency explains the movers")
    for F, e in groups[:12]:
        h40 = F / 40e6
        tag = f"  ~ 40 MHz x{h40:.2f}" if abs(h40 - round(h40)) < 0.02 else ""
        print(f"  F = {F/1e6:8.3f} MHz   min in-band excess {e:4.1f} dB{tag}")


def _digital_solve(caps, thr=8.0):
    """Find Fs-proportional spurs: a fraction r so r*Fs is strong at every Fs
    (=> ADC/data-clock digital spur, moves with the clock)."""
    rates = sorted(caps)
    rg = np.arange(-0.30, 0.30, 1e-4)
    mins = np.full(rg.shape, 1e9)
    for fs in rates:
        f, P, _ = caps[fs]
        ex = _ex_arr(P)
        df = f[1] - f[0]
        idx = np.clip(np.round((rg * fs - f[0]) / df).astype(int), 0, len(f) - 1)
        mins = np.minimum(mins, ex[idx])
    cand = np.flatnonzero((mins > thr) & (np.abs(rg) > 0.01))
    groups = []
    if len(cand):
        for g in np.split(cand, np.flatnonzero(np.diff(cand) > 20) + 1):
            k = g[int(np.argmax(mins[g]))]
            groups.append((rg[k], mins[k]))
    groups.sort(key=lambda t: -t[1])
    print("\n===== DIGITAL SOLVE: Fs-proportional fraction strong at ALL Fs =====")
    if not groups:
        print("  none — no constant offset/Fs fraction explains the movers")
    for r, e in groups[:12]:
        print(f"  offset/Fs = {r:+.4f}   min in-band excess {e:4.1f} dB")


def _classify(caps, lo0):
    rates = sorted(caps)
    if len(rates) < 2:
        print("need >=2 rates to classify")
        return
    ref = max(rates)                                 # 14 MHz reference set
    f_ref, P_ref, t_ref = caps[ref]
    tol = 20e3
    print(f"\n===== CLASSIFICATION (reference Fs={ref/1e6:.3f} MHz) =====")
    print("  tooth(abs MHz)  dB   verdict        detail")
    nfix = nlock = 0
    for off, db in t_ref[:10]:
        if abs(off) > 3.6e6:                         # must stay in-band at the lowest Fs
            continue
        votes_fixed = votes_scaled = 0
        details = []
        for fs in rates:
            if fs == ref:
                continue
            offs = np.array([o for o, _ in caps[fs][2]])
            if not len(offs):
                continue
            pred_fixed = off
            pred_scaled = off * (fs / ref)
            d_fixed = np.min(np.abs(offs - pred_fixed))
            d_scaled = np.min(np.abs(offs - pred_scaled))
            if d_fixed < d_scaled and d_fixed < tol:
                votes_fixed += 1
            elif d_scaled < d_fixed and d_scaled < tol:
                votes_scaled += 1
            details.append(f"{fs/1e6:.1f}:fix{d_fixed/1e3:+.0f}/scl{d_scaled/1e3:+.0f}")
        if votes_fixed > votes_scaled:
            verdict = "FIXED (switcher)"
            nfix += 1
        elif votes_scaled > votes_fixed:
            verdict = "Fs-LOCKED (digital)"
            nlock += 1
        else:
            verdict = "ambiguous"
        print(f"   {(lo0+off)/1e6:8.3f}     {db:4.1f}  {verdict:18s} {' '.join(details)}")
    print(f"\n  dominant teeth: FIXED/switcher={nfix}  Fs-LOCKED/digital={nlock}")
    if nlock > nfix:
        print("  => teeth move with the digital clock: a switcher-side bead will NOT")
        print("     help the comb (currents come from the cap side). Address PL-fabric")
        print("     power integrity / digital decoupling instead.")
    elif nfix > nlock:
        print("  => teeth are fixed vs the digital clock: consistent with the on-board")
        print("     switcher (or a fixed source). A series bead/decoupling at the")
        print("     switcher can directly attenuate them -- worth trying.")


def _plot(caps, lo0):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    OUT_DIR.mkdir(exist_ok=True)
    rates = sorted(caps)
    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(13, 8))
    for fs in rates:
        f, P, tl = caps[fs]
        PdB = 10 * np.log10(P + 1e-9)
        PdB -= np.median(PdB)
        ax1.plot(f / 1e6, PdB, lw=0.6, label=f"Fs={fs/1e6:.2f} MHz")
        ax2.plot(f / fs, PdB, lw=0.6, label=f"Fs={fs/1e6:.2f} MHz")
    ax1.set_title("Baseband offset in Hz -- teeth that ALIGN here are FIXED "
                  "(switcher/physical; bead may help)")
    ax1.set_xlabel("offset from LO (MHz)")
    ax1.set_ylabel("dB rel floor")
    ax1.set_xlim(-3.6, 3.6)
    ax1.legend(fontsize=8)
    ax1.grid(True, alpha=0.25)
    ax2.set_title("Offset as a FRACTION of Fs -- teeth that ALIGN here are "
                  "Fs-LOCKED (digital/ADC clock; bead won't help)")
    ax2.set_xlabel("offset / Fs")
    ax2.set_ylabel("dB rel floor")
    ax2.set_xlim(-0.3, 0.3)
    ax2.legend(fontsize=8)
    ax2.grid(True, alpha=0.25)
    fig.tight_layout()
    import datetime
    rates_tag = "-".join(f"{fs/1e6:.1f}" for fs in sorted(caps, reverse=True))
    date = datetime.date.today().isoformat()
    path = OUT_DIR / f"clock_shift_{LABEL}_{rates_tag}MHz_{date}.png"
    fig.savefig(path, dpi=130)
    print(f"Saved plot: {path}")


if __name__ == "__main__":
    main()
