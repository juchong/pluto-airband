#!/usr/bin/env python3
"""Sweep the LO across bands and report the discrete-peak comb at each, to test
whether the ~100 kHz comb is broadband (clock/EMI/amplifier) or airband-specific.
Recorder-only capture; self-heals maia-httpd on OOM (96 MB Pluto+). Restores LO."""
import io, json, os, subprocess, time, urllib.request
import numpy as np

HOST = os.environ.get("PLUTO_HOST", "10.0.16.100")
B, U, PW = f"http://{HOST}:8000", f"ip:{HOST}", os.environ.get("PLUTO_PW", "analog")
ENV = {"PATH": os.environ["PATH"]}
FS = 14e6
LOS = [123_438_000, 250_000_000, 500_000_000, 900_000_000]


def iio(a): subprocess.run(["iio_attr", "-u", U, "-c", "ad9361-phy", *a],
                           capture_output=True, env=ENV)
def set_lo(hz): iio(["altvoltage0", "frequency", str(int(hz))])
def api_up():
    try:
        urllib.request.urlopen(B + "/api", timeout=4).read(); return True
    except Exception:
        return False
def ensure_up():
    if api_up(): return
    subprocess.run(["sshpass", "-p", PW, "ssh", "-o", "StrictHostKeyChecking=accept-new",
                    "-o", "ConnectTimeout=8", f"root@{HOST}",
                    "/etc/init.d/S60maia-httpd restart"], capture_output=True, env=ENV)
    for _ in range(30):
        time.sleep(2)
        if api_up(): time.sleep(2); return
    raise RuntimeError("maia down")


def rec(dur=0.15):
    last = None
    for _ in range(4):
        ensure_up()
        try:
            urllib.request.urlopen(urllib.request.Request(
                B + "/api/recorder", data=json.dumps(
                    {"mode": "IQ16bit", "maximum_duration": dur,
                     "state_change": "Start"}).encode(), method="PATCH",
                headers={"Content-Type": "application/json"}), timeout=10).read()
            time.sleep(dur + 0.4)
            for _ in range(50):
                if json.loads(urllib.request.urlopen(B + "/api/recorder", timeout=10)
                              .read())["state"] == "Stopped": break
                time.sleep(0.1)
            resp = urllib.request.urlopen(B + "/recording", timeout=60); ch = []
            while True:
                b = resp.read(1 << 20)
                if not b: break
                ch.append(b)
            import tarfile
            tf = tarfile.open(fileobj=io.BytesIO(b"".join(ch)))
            d = [tf.extractfile(m).read() for m in tf.getmembers()
                 if m.name.endswith(".sigmf-data")][0]
            a = np.frombuffer(d, dtype="<i2")
            x = a[0::2].astype(np.float64) + 1j * a[1::2].astype(np.float64)
            if len(x) > 100000:
                time.sleep(0.8); return x
        except Exception as e:
            last = e; time.sleep(1)
    raise last or RuntimeError("rec failed")


def analyze(x):
    from numpy.lib.stride_tricks import sliding_window_view
    N = 1 << 14; win = np.hanning(N); P = np.zeros(N); s = 0
    for i in range(0, len(x) - N, N // 2):
        P += np.abs(np.fft.fftshift(np.fft.fft(x[i:i + N] * win))) ** 2; s += 1
    P /= max(s, 1); f = np.fft.fftshift(np.fft.fftfreq(N, 1 / FS))
    floor = np.median(sliding_window_view(np.pad(P, 400, mode="edge"), 801), axis=1)
    ex = 10 * np.log10((P + 1e-9) / (floor + 1e-9))
    cand = np.flatnonzero(ex > 8)
    peaks = []
    if len(cand):
        for g in np.split(cand, np.flatnonzero(np.diff(cand) > 6) + 1):
            peaks.append(f[g[int(np.argmax(P[g]))]])
    peaks = np.array(sorted(p for p in peaks if abs(p) > 20000))
    n = len(peaks)
    gap = np.median(np.diff(peaks)) / 1e3 if n > 3 else float("nan")
    # cepstrum of a peak-presence spectrum -> dominant comb spacing
    sp = np.zeros(len(f)); idx = np.clip(np.searchsorted(f, peaks), 0, len(f) - 1)
    sp[idx] = 1.0
    ceps = np.abs(np.fft.rfft(sp - sp.mean()))
    q = np.fft.rfftfreq(len(sp), d=f[1] - f[0]); q[0] = np.nan
    spacing = 1.0 / q; valid = (spacing > 30e3) & (spacing < 2e6)
    cs = spacing[np.nanargmax(np.where(valid, ceps, np.nan))] / 1e3 if valid.any() else float("nan")
    return n, gap, cs, float(ex.max())


def main():
    ensure_up()
    lo0 = int(json.loads(urllib.request.urlopen(B + "/api", timeout=6).read())
              ["ad9361"]["rx_lo_frequency"])
    print(f"baseline LO {lo0/1e6:.3f} MHz; sweeping {[l//1_000_000 for l in LOS]} MHz")
    print(f"{'LO MHz':>8} {'#peaks':>7} {'medGap kHz':>11} {'cepstral kHz':>13} {'peak dB':>8}")
    try:
        for lo in LOS:
            set_lo(lo); time.sleep(0.6)
            n, gap, cs, mx = analyze(rec())
            print(f"{lo/1e6:8.1f} {n:7d} {gap:11.1f} {cs:13.1f} {mx:8.1f}")
    finally:
        set_lo(lo0)
        print(f"restored LO {lo0/1e6:.3f} MHz")


if __name__ == "__main__":
    main()
