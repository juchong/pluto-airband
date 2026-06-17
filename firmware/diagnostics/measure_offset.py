#!/usr/bin/env python3
"""Measure a known carrier's frequency error vs its true frequency and derive
the Pluto 40 MHz reference (XO) ppm error + the u-boot ``ad936x_ext_refclk_override``
value that calibrates it out. Point it at a station whose transmit frequency you
trust (a freshly commissioned AWOS/ASOS carrier is ideal). Read-only w.r.t. RF
state (uses the maia recorder only). Run OUTSIDE the sandbox.

  usage: measure_offset.py [TRUE_FREQ_MHZ]   (default 118.05 = ch0 AWOS)

Apply the printed value persistently (survives reboot; reset by a boot.dfu flash,
then re-applied by firmware/pluto_setup_env.py):
  ssh root@192.168.2.1 'fw_setenv ad936x_ext_refclk_override <value>' && reboot
"""
import json, time, io, tarfile, urllib.request, sys
import numpy as np

BASE = "http://192.168.2.1:8000"
FS_NOM = 14_000_000.0
XO_NOM = 40_000_000.0
CH0 = (float(sys.argv[1]) * 1e6 if len(sys.argv) > 1 else 118.05e6)  # known-accurate carrier


def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=15) as r:
        return r.read()


def record_iq(dur=0.5):
    api("PATCH", "/api/recorder",
        {"mode": "IQ16bit", "maximum_duration": dur, "state_change": "Start"})
    time.sleep(dur + 0.3)
    for _ in range(60):
        if json.loads(api("GET", "/api/recorder"))["state"] == "Stopped":
            break
        time.sleep(0.1)
    raw = urllib.request.urlopen(BASE + "/recording", timeout=30).read()
    tf = tarfile.open(fileobj=io.BytesIO(raw))
    for m in tf.getmembers():
        if m.name.endswith(".sigmf-data"):
            return np.frombuffer(tf.extractfile(m).read(), dtype="<i2")
    raise RuntimeError("no data")


def believed_refclk_hz():
    """Read the reference clock the AD9361 currently believes (the DTB/override
    value), via SSH. Falls back to nominal 40 MHz if unreachable -- but the
    suggested override is only one-shot-correct when this is the real value;
    otherwise just iterate (apply, reboot, re-run)."""
    import subprocess
    try:
        out = subprocess.run(
            ["sshpass", "-p", "analog", "ssh", "-o", "ConnectTimeout=5",
             "-o", "StrictHostKeyChecking=no", "root@192.168.2.1",
             "grep -m1 ext_refclk /sys/kernel/debug/clk/clk_summary"],
            capture_output=True, text=True, timeout=10)
        return float(out.stdout.split()[4])    # rate column
    except Exception:
        return XO_NOM


def main():
    lo = int(json.loads(api("GET", "/api"))["ad9361"]["rx_lo_frequency"])
    expected = CH0 - lo                       # nominal baseband offset (Hz)
    raw = record_iq(0.5)
    I = raw[0::2].astype(np.float64); Q = raw[1::2].astype(np.float64)
    n = min(len(I), len(Q)); x = (I[:n] + 1j * Q[:n])
    N = 1 << int(np.log2(n))                   # power-of-two FFT
    x = x[:N] * np.hanning(N)
    X = np.fft.fftshift(np.fft.fft(x))
    f = np.fft.fftshift(np.fft.fftfreq(N, 1 / FS_NOM))   # labeled (nominal) Hz
    mag = np.abs(X)
    # search +/-40 kHz around the expected offset for the carrier peak
    win = (f > expected - 40_000) & (f < expected + 40_000)
    idx = np.where(win)[0]
    k = idx[np.argmax(mag[win])]
    # parabolic interpolation for sub-bin precision
    if 0 < k < N - 1:
        a, b, c = mag[k - 1], mag[k], mag[k + 1]
        denom = (a - 2 * b + c)
        delta = 0.5 * (a - c) / denom if denom != 0 else 0.0
    else:
        delta = 0.0
    bin_hz = FS_NOM / N
    measured = f[k] + delta * bin_hz
    err = measured - expected                  # Hz the carrier is off (labeled)
    # apparent_offset - expected = -e * f_true  ->  e = -err / f_true
    e = -err / CH0
    ppm = e * 1e6
    floor = np.median(mag[win])
    snr = 20 * np.log10(mag[k] / (floor + 1e-9))
    print(f"rx_lo            = {lo} Hz")
    print(f"expected offset  = {expected:+.1f} Hz  (ch0 {CH0/1e6:.4f} MHz - LO)")
    print(f"measured offset  = {measured:+.1f} Hz  (carrier peak, SNR {snr:.0f} dB)")
    print(f"carrier error    = {err:+.1f} Hz  -> signal sits "
          f"{'LOW' if err < 0 else 'HIGH'} of the marker")
    print(f"reference error  = {ppm:+.2f} ppm  (XO actual is "
          f"{'HIGH' if ppm > 0 else 'LOW'})")
    print(f"FFT bin          = {bin_hz:.2f} Hz  (N={N})")
    # displayed_carrier = f_true * (believed_ref / true_xo), so the true XO (the
    # value to program) scales the CURRENTLY believed reference by true/displayed.
    # NB: it is NOT 40 MHz * (1+e) unless the DTB's baked clock is exactly 40 MHz.
    believed = believed_refclk_hz()
    new_override = round(believed * CH0 / (CH0 + err))
    print(f"believed refclk  = {believed:.0f} Hz  (current DT/clk value)")
    print(f"suggested ad936x_ext_refclk_override = {new_override}  "
          f"(= believed x true/displayed)")
    print(f"  apply (note the REQUIRED angle brackets):")
    print(f"  ssh root@192.168.2.1 'fw_setenv ad936x_ext_refclk_override "
          f'"<{new_override}>"\' && ssh root@192.168.2.1 reboot')
    print(f"  then re-run this to confirm the carrier error is ~0 (iterate if not).")


if __name__ == "__main__":
    main()
