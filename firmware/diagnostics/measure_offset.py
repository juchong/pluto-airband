#!/usr/bin/env python3
"""Measure a known carrier's frequency error vs its true frequency and derive
the Pluto 40 MHz reference (XO) ppm error + the u-boot ``ad936x_ext_refclk_override``
value that calibrates it out. Point it at a station whose transmit frequency you
trust (a freshly commissioned AWOS/ASOS carrier is ideal). Read-only w.r.t. RF
state (uses the maia recorder only). Run OUTSIDE the sandbox.

  usage: measure_offset.py [TRUE_FREQ_MHZ]   (default 118.05 = ch0 AWOS)
  host:  set PLUTO_HOST=<ip> for the device (default 192.168.2.1; e.g. an
         Ethernet-attached Pluto+ at its DHCP address)

Apply the printed value persistently (survives reboot; reset by a boot.dfu flash,
then re-applied by firmware/pluto_setup_env.py):
  ssh root@<host> 'fw_setenv ad936x_ext_refclk_override <value>' && reboot
"""
import json, time, io, tarfile, urllib.request, sys, os
import numpy as np

HOST = os.environ.get("PLUTO_HOST", "192.168.2.1")
BASE = f"http://{HOST}:8000"
# IQ capture seconds. At 14 Msps a recording is ~56 MB/s of IQ16, which maia-httpd
# buffers in RAM to serve -- 0.5 s OOM-killed the daemon, so keep this small. 0.1 s
# (~5.6 MB) still gives ~27 Hz FFT bins (sub-Hz after parabolic interpolation).
IQ_DUR = float(os.environ.get("PLUTO_IQ_DUR", "0.1"))
FS_NOM = 14_000_000.0
XO_NOM = 40_000_000.0
def _argv_freq():
    """Optional TRUE_FREQ_MHZ from argv[1], but only when it parses as a float
    (so this module is safe to `import` from tools that have their own argv)."""
    if len(sys.argv) > 1:
        try:
            return float(sys.argv[1]) * 1e6
        except ValueError:
            pass
    return 118.05e6


CH0 = _argv_freq()  # known-accurate carrier (default ch0 AWOS)


def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=15) as r:
        return r.read()


def _read_full(url, timeout=30):
    """urlopen().read() but tolerant of a short/partial transfer: keep the bytes
    that did arrive instead of raising, then validate length at the call site."""
    import http.client
    with urllib.request.urlopen(url, timeout=timeout) as r:
        try:
            return r.read()
        except http.client.IncompleteRead as e:
            return e.partial


def record_iq(dur=IQ_DUR, tries=4):
    """Trigger an IQ recording and return the int16 I/Q samples. Retries the
    whole record+download a few times -- the maia HTTP transfer occasionally
    truncates (IncompleteRead) on the Pluto."""
    last = None
    for _ in range(tries):
        try:
            api("PATCH", "/api/recorder",
                {"mode": "IQ16bit", "maximum_duration": dur, "state_change": "Start"})
            time.sleep(dur + 0.3)
            for _ in range(60):
                if json.loads(api("GET", "/api/recorder"))["state"] == "Stopped":
                    break
                time.sleep(0.1)
            raw = _read_full(BASE + "/recording")
            tf = tarfile.open(fileobj=io.BytesIO(raw))
            for m in tf.getmembers():
                if m.name.endswith(".sigmf-data"):
                    return np.frombuffer(tf.extractfile(m).read(), dtype="<i2")
            raise RuntimeError("no .sigmf-data in recording archive")
        except Exception as e:       # truncated download, tar error, transient 5xx
            last = e
            time.sleep(0.5)
    raise RuntimeError(f"record_iq failed after {tries} tries: {last}")


def believed_refclk_hz():
    """Read the reference clock the AD9361 currently believes (the DTB/override
    value), via SSH. Falls back to nominal 40 MHz if unreachable -- but the
    suggested override is only one-shot-correct when this is the real value;
    otherwise just iterate (apply, reboot, re-run)."""
    import subprocess
    try:
        out = subprocess.run(
            ["sshpass", "-p", "analog", "ssh", "-o", "ConnectTimeout=5",
             "-o", "StrictHostKeyChecking=no", f"root@{HOST}",
             "grep -m1 ext_refclk /sys/kernel/debug/clk/clk_summary"],
            capture_output=True, text=True, timeout=10)
        return float(out.stdout.split()[4])    # rate column
    except Exception:
        return XO_NOM


def measure_once(true_freq=CH0):
    """One capture -> dict with the carrier error, reference ppm, and the
    suggested ``ad936x_ext_refclk_override``."""
    lo = int(json.loads(api("GET", "/api"))["ad9361"]["rx_lo_frequency"])
    expected = true_freq - lo                 # nominal baseband offset (Hz)
    raw = record_iq()
    I = raw[0::2].astype(np.float64); Q = raw[1::2].astype(np.float64)
    n = min(len(I), len(Q)); x = (I[:n] + 1j * Q[:n])
    N = 1 << int(np.log2(n))                   # power-of-two FFT
    x = x[:N] * np.hanning(N)
    X = np.fft.fftshift(np.fft.fft(x))
    f = np.fft.fftshift(np.fft.fftfreq(N, 1 / FS_NOM))   # labeled (nominal) Hz
    mag = np.abs(X)
    win = (f > expected - 40_000) & (f < expected + 40_000)
    idx = np.where(win)[0]
    k = idx[np.argmax(mag[win])]
    if 0 < k < N - 1:
        a, b, c = mag[k - 1], mag[k], mag[k + 1]
        denom = (a - 2 * b + c)
        delta = 0.5 * (a - c) / denom if denom != 0 else 0.0
    else:
        delta = 0.0
    bin_hz = FS_NOM / N
    measured = f[k] + delta * bin_hz
    err = measured - expected                  # Hz the carrier is off (labeled)
    ppm = (-err / true_freq) * 1e6
    floor = np.median(mag[win])
    snr = 20 * np.log10(mag[k] / (floor + 1e-9))
    believed = believed_refclk_hz()
    override = round(believed * true_freq / (true_freq + err))
    return {
        "lo": lo, "expected": expected, "measured": measured, "err": err,
        "ppm": ppm, "snr": snr, "bin_hz": bin_hz, "believed": believed,
        "override": override, "true_freq": true_freq,
    }


def main():
    lo = int(json.loads(api("GET", "/api"))["ad9361"]["rx_lo_frequency"])
    expected = CH0 - lo                       # nominal baseband offset (Hz)
    raw = record_iq()
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
    print(f"  ssh root@{HOST} 'fw_setenv ad936x_ext_refclk_override "
          f'"<{new_override}>"\' && ssh root@{HOST} reboot')
    print(f"  then re-run this to confirm the carrier error is ~0 (iterate if not).")


if __name__ == "__main__":
    main()
