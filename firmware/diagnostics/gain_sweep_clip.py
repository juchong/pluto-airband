#!/usr/bin/env python3
"""Sweep RX gain (from 0) and measure wideband ADC clip% / RMS / peak (dBFS) at
the current antenna+LO. Validates the gain-stage behavior on this unit. Recorder-
only; self-heals maia-httpd on OOM (96 MB Pluto+) and re-applies gain. Restores gain."""
import io, json, os, subprocess, tarfile, time, urllib.request
import numpy as np

HOST = os.environ.get("PLUTO_HOST", "10.0.16.100")
B, U, PW = f"http://{HOST}:8000", f"ip:{HOST}", os.environ.get("PLUTO_PW", "analog")
ENV = {"PATH": os.environ["PATH"]}
GAINS = [0, 10, 20, 30, 40, 48, 55, 64, 71]
FSCALE = 32768.0


def iio(a): subprocess.run(["iio_attr", "-u", U, "-c", "ad9361-phy", *a],
                           capture_output=True, env=ENV)
def set_gain(g): iio(["-i", "voltage0", "hardwaregain", str(int(g))])
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
    raise RuntimeError("maia did not come back up")


def rec(g, dur=0.12):
    last = None
    for _ in range(5):
        ensure_up(); set_gain(g); time.sleep(0.5)
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
            tf = tarfile.open(fileobj=io.BytesIO(b"".join(ch)))
            d = [tf.extractfile(m).read() for m in tf.getmembers()
                 if m.name.endswith(".sigmf-data")][0]
            a = np.frombuffer(d, dtype="<i2")
            if a.size > 200000:
                time.sleep(0.6)
                return a[0::2].astype(np.float64), a[1::2].astype(np.float64)
        except Exception as e:
            last = e; time.sleep(1)
    raise last or RuntimeError("rec failed")


def main():
    ensure_up()
    g0 = int(json.loads(urllib.request.urlopen(B + "/api", timeout=6).read())
             ["ad9361"]["rx_gain"])
    print(f"baseline gain {g0} dB; sweeping {GAINS} (antenna, LO 123.438)")
    print(f"{'gain':>5} {'clip%':>8} {'RMS dBFS':>10} {'peak dBFS':>10}")
    try:
        for g in GAINS:
            I, Q = rec(g)
            clip = np.mean((np.abs(I) >= 32000) | (np.abs(Q) >= 32000)) * 100
            rms = np.sqrt(np.mean(I * I + Q * Q))
            pk = np.sqrt(np.max(I * I + Q * Q))
            print(f"{g:5d} {clip:8.3f} {20*np.log10(rms/FSCALE+1e-12):10.1f} "
                  f"{20*np.log10(pk/FSCALE+1e-12):10.1f}")
    finally:
        ensure_up(); set_gain(g0)
        print(f"restored gain {g0} dB")


if __name__ == "__main__":
    main()
