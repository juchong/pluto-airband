#!/usr/bin/env python3
"""GPS-disciplined clock calibration against an LTE downlink carrier.

LTE base stations (eNodeBs) must hold their carrier within +/-0.05 ppm of a
GPS/network reference (3GPP TS 36.104) -- effectively absolute truth, and
(unlike 2G GSM, which kalibrate used and which is now decommissioned) still
everywhere in 2026. We tune to a cell's downlink center and measure the carrier
frequency offset (CFO) of the Pluto vs the cell, then derive the XO error and
the persistent ``ad936x_ext_refclk_override`` that nulls it.

Method -- cyclic-prefix (CP) autocorrelation. An LTE OFDM signal has no carrier
line, but every OFDM symbol prepends a copy of its own tail (the CP). So samples
spaced one FFT length apart (128 samples at the 1.92 Msps central-6-RB rate) are
identical except for the phase the CFO adds over that span:
    r[n]*conj(r[n+128]) has phase -2*pi*CFO*128/fs at every CP sample.
Summing over the whole capture, the CP samples (identical) add coherently while
the payload samples (different symbols) average out, so angle(sum) yields the
CFO, unambiguous over +/-fs/256 = +/-7.5 kHz (~+/-10 ppm at 750 MHz) and accurate
to ~10 Hz from the thousands of symbols in a 0.15 s capture. At ~750 MHz, 10 Hz
is ~0.013 ppm -- the high-RF leverage a 19 kHz FM pilot or 24 kHz AM audio can
never provide.

  self-test (no hardware):   lte_calibrate.py --selftest
  measure:  PLUTO_HOST=<ip>  lte_calibrate.py --freq 751.0
  apply:    add --apply  (programs override, then reboots; IP may change)

Tip: find a strong cell's downlink-center MHz with any "LTE Discovery"/cell-scan
phone app, or sweep common US bands (B71 617-652, B12/B13 729-746, B5 869-894,
B2 1930-1990, B66 2110-2200 MHz). The device should already be within ~10 ppm
(the +/-7.5 kHz capture range); the AWOS calibration satisfies that. Mutates live
RF; restores LO/gain over SSH (no reboot). Run OUTSIDE the sandbox.
"""
import json, time, io, tarfile, urllib.request, subprocess, sys, os
import numpy as np
from scipy.signal import resample_poly

HOST = os.environ.get("PLUTO_HOST", "192.168.2.1")
BASE = f"http://{HOST}:8000"
FS = 14_000_000.0                 # Pluto IQ rate (nominal)
LTE_FS = 1_920_000.0              # central 6 RB rate (128-pt FFT, 15 kHz SCS)
NFFT = 128
AIRBAND_LO = 123_438_000
AIRBAND_GAIN = 71


# ---------- device I/O ----------
def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method,
                                 headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=15) as r:
        return r.read()


def ssh(cmd):
    return subprocess.run(
        ["sshpass", "-p", "analog", "ssh", "-o", "ConnectTimeout=8",
         "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
         f"root@{HOST}", cmd], capture_output=True, text=True, timeout=25)


def set_lo(hz):
    ssh(f"iio_attr -c ad9361-phy altvoltage0 frequency {int(hz)}")


def set_gain(db):
    ssh(f"iio_attr -i -c ad9361-phy voltage0 hardwaregain {int(db)}")


def cur_lo():
    return int(json.loads(api("GET", "/api"))["ad9361"]["rx_lo_frequency"])


def believed_refclk():
    try:
        out = ssh("grep -m1 ext_refclk /sys/kernel/debug/clk/clk_summary")
        return float(out.stdout.split()[4])
    except Exception:
        return 40_000_000.0


def _read_full(url, timeout=30):
    import http.client
    with urllib.request.urlopen(url, timeout=timeout) as r:
        try:
            return r.read()
        except http.client.IncompleteRead as e:
            return e.partial


def record_iq(dur, tries=4):
    last = None
    for _ in range(tries):
        try:
            api("PATCH", "/api/recorder",
                {"mode": "IQ16bit", "maximum_duration": dur, "state_change": "Start"})
            time.sleep(dur + 0.3)
            for _ in range(80):
                if json.loads(api("GET", "/api/recorder"))["state"] == "Stopped":
                    break
                time.sleep(0.1)
            raw = _read_full(BASE + "/recording")
            tf = tarfile.open(fileobj=io.BytesIO(raw))
            for m in tf.getmembers():
                if m.name.endswith(".sigmf-data"):
                    return np.frombuffer(tf.extractfile(m).read(), dtype="<i2")
            raise RuntimeError("no .sigmf-data in archive")
        except Exception as e:
            last = e
            time.sleep(0.5)
    raise RuntimeError(f"record_iq failed after {tries} tries: {last}")


def _raw_complex(raw):
    I = raw[0::2].astype(np.float64); Q = raw[1::2].astype(np.float64)
    n = min(len(I), len(Q))
    return I[:n] + 1j * Q[:n]


def capture_192(dur):
    """Capture Pluto IQ and resample to the 1.92 Msps LTE grid (centered on LO)."""
    return resample_poly(_raw_complex(record_iq(dur)), 24, 175)  # 14e6*24/175=1.92e6


# Common US LTE downlink ranges (MHz), ordered by typical indoor strength
# (700/850 first, then 600, then PCS/AWS): 600/700/850/PCS/AWS.
SCAN_RANGES = [(728, 768), (869, 894), (617, 652), (1930, 1995), (2110, 2155)]


def restart_maia():
    """Restart maia-httpd and wait for it to listen again. The 96 MB board leaks
    across repeated IQ recordings and OOM-kills the daemon; a fresh start before
    each band keeps the scan reliable."""
    ssh("/etc/init.d/S60maia-httpd restart >/dev/null 2>&1")
    for _ in range(20):
        time.sleep(1)
        try:
            api("GET", "/api"); return True
        except Exception:
            pass
    return False


def scan_bands(gain, ranges=SCAN_RANGES, keep_mhz=12.0, dur=0.03, stop_db=12.0):
    """Wideband PSD sweep -> list of detected carriers (center_mhz, bw_mhz, pwr_db),
    strongest first. Centers are snapped to the LTE 100 kHz channel raster so the
    later CFO read reflects only clock error, not a centering mistake. Scans one
    band at a time (restarting maia between bands to dodge the OOM), and returns
    early once a band yields a carrier stronger than stop_db over the floor."""
    half = keep_mhz / 2
    found = []
    for a, b in ranges:
        if not restart_maia():
            continue
        set_gain(gain)
        fa, pa = [], []
        lo = a + half
        while lo - half < b:
            set_lo(int(lo * 1e6)); time.sleep(0.25)
            try:
                x = _raw_complex(record_iq(dur))
            except Exception:
                lo += keep_mhz; continue
            N = 4096
            nseg = len(x) // N
            if nseg < 1:
                lo += keep_mhz; continue
            P = np.zeros(N)
            w = np.hanning(N)
            for i in range(nseg):
                P += np.abs(np.fft.fftshift(np.fft.fft(x[i*N:(i+1)*N] * w))) ** 2
            P /= nseg
            f = np.fft.fftshift(np.fft.fftfreq(N, 1 / FS)) + lo * 1e6
            m = np.abs(f - lo * 1e6) < half * 1e6
            fa.append(f[m]); pa.append(10 * np.log10(P[m] + 1e-9))
            lo += keep_mhz
        band = _carriers_from_psd(fa, pa)
        if band:
            print(f"  band {a}-{b} MHz: " + ", ".join(
                f"{c:.1f}MHz({pw:.0f}dB)" for c, bw, pw in band[:4]))
            found.extend(band)
            if band[0][2] >= stop_db:
                break
        else:
            print(f"  band {a}-{b} MHz: nothing")
    found.sort(key=lambda c: -c[2])
    return found


def _carriers_from_psd(fa, pa):
    if not fa:
        return []
    f = np.concatenate(fa); p = np.concatenate(pa)
    o = np.argsort(f); f, p = f[o], p[o]
    # resample onto a uniform 50 kHz grid (max-hold) for edge detection
    grid = np.arange(f[0], f[-1], 50e3)
    prof = np.full(len(grid), -120.0)
    gi = np.clip(((f - grid[0]) / 50e3).astype(int), 0, len(grid) - 1)
    np.maximum.at(prof, gi, p)
    floor = np.median(prof)
    occ = prof > floor + 8.0
    carriers = []
    i = 0
    while i < len(occ):
        if occ[i]:
            j = i
            gap = 0
            while j + 1 < len(occ) and (occ[j + 1] or gap < 8):
                j += 1
                gap = 0 if occ[j] else gap + 1
            bw = (grid[j] - grid[i]) / 1e6
            center = (grid[i] + grid[j]) / 2
            if 1.0 <= bw <= 22.0:
                csnap = round(center / 1e5) * 1e5        # 100 kHz raster
                pw = float(np.median(prof[i:j + 1]) - floor)
                carriers.append((csnap / 1e6, bw, pw))
            i = j + 1
        else:
            i += 1
    carriers.sort(key=lambda c: -c[2])
    return carriers


def find_true_center(rough_mhz, gain):
    """The CP estimator reads offset only mod 15 kHz, so the assumed center must be
    the exact LTE raster center or a 100 kHz snap error injects a large aliased
    offset. Probe the rough center +/-100/200 kHz (raster steps) and pick the one
    with the smallest |CFO| -- that is the true center (only clock error remains).
    Assumes the clock is within ~3 ppm so the true center's |CFO| is the minimum."""
    cands = [round(rough_mhz + d, 1) for d in (-0.2, -0.1, 0.0, 0.1, 0.2)]
    best_c, best_abs = None, 1e18
    for c in cands:
        set_gain(gain); set_lo(int(c * 1e6)); time.sleep(0.4)
        vals, qs = [], []
        for _ in range(2):
            try:
                cfo, q = measure_cfo(capture_192(0.08))
                if q > 0.3:
                    vals.append(cfo); qs.append(q)
            except Exception:
                pass
        if vals:
            m = float(np.median(vals))
            print(f"    center probe {c:7.1f} MHz: CFO {m:+8.1f} Hz  q={max(qs):.2f}")
            if abs(m) < best_abs:
                best_abs = abs(m); best_c = c
    return best_c


def measure_freq(fc, gain, passes, dur):
    """Tune to fc (Hz), run `passes` CFO measurements; return (cfos, quals)."""
    set_gain(gain); set_lo(int(fc)); time.sleep(0.4)
    cfos, quals = [], []
    for i in range(passes):
        try:
            cfo, q = measure_cfo(capture_192(dur))
            ppm = (-cfo / fc) * 1e6
            cfos.append(cfo); quals.append(q)
            print(f"  pass {i+1}/{passes}: cfo {cfo:+9.1f} Hz  {ppm:+7.3f} ppm  "
                  f"lock q={q:.3f}")
        except Exception as e:
            print(f"  pass {i+1}/{passes}: failed ({e})")
    return cfos, quals


# LTE normal-CP slot at 1.92 Msps: 960 samples = 7 OFDM symbols, CP lengths
# 10,9,9,9,9,9,9 (the first symbol's CP is one sample longer).
SLOT = 960
CP_LENS = [10, 9, 9, 9, 9, 9, 9]


def _cp_offsets():
    """Sample indices, within one 960-sample slot, that belong to a CP."""
    starts, p = [], 0
    for cp in CP_LENS:
        starts.append(p); p += cp + NFFT
    idx = []
    for s, cp in zip(starts, CP_LENS):
        idx.extend(range(s, s + cp))
    return np.array(idx)


# ---------- CFO estimation (CP autocorrelation) ----------
def measure_cfo(r, fs=LTE_FS):
    """Estimate CFO (Hz) from cyclic-prefix autocorrelation.

    a[n]=r[n]*conj(r[n+128]) has phase -2*pi*CFO*128/fs at every CP sample,
    regardless of where the symbol sits, so all CP samples add coherently. We
    first lock the slot timing (scan the 960-sample grid for the offset whose CP
    mask maximizes |sum a|), then sum a ONLY over CP samples -- excluding the
    random payload products that would otherwise bias the phase. Returns
    (cfo_hz, quality) with quality in [0,1] the CP-sum coherence (~0 => no OFDM)."""
    a = r[:-NFFT] * np.conj(r[NFFT:])
    La = len(a)
    nslots = La // SLOT
    if nslots < 2:
        P = np.sum(a)
        return float(-np.angle(P) * fs / (2 * np.pi * NFFT)), \
            float(np.abs(P) / (np.sum(np.abs(a)) + 1e-12))
    base = (_cp_offsets()[None, :] + SLOT * np.arange(nslots)[:, None]).ravel()
    best_P, best_abs = 0.0 + 0.0j, -1.0
    for d0 in range(SLOT):
        idx = base + d0
        idx = idx[idx < La]
        S = np.sum(a[idx])
        if abs(S) > best_abs:
            best_abs = abs(S); best_P = S; best_d0 = d0
    idx = (base + best_d0); idx = idx[idx < La]
    cfo = -np.angle(best_P) * fs / (2 * np.pi * NFFT)
    quality = float(np.abs(best_P) / (np.sum(np.abs(a[idx])) + 1e-12))
    return float(cfo), quality


# ---------- self test ----------
def _make_ofdm(nslots, cfo, fs, snr_db, rng=None):
    """Synthesize an LTE-like OFDM stream (real 960-sample slot structure,
    CP lengths 10,9,9,9,9,9,9) with a known CFO."""
    rng = rng or np.random.default_rng(0)
    used = np.r_[np.arange(1, 37), np.arange(NFFT - 36, NFFT)]   # ~72 subcarriers
    out = []
    for _ in range(nslots):
        for cp in CP_LENS:
            F = np.zeros(NFFT, dtype=complex)
            bits = rng.integers(0, 4, len(used))
            F[used] = np.exp(1j * (np.pi / 2 * bits + np.pi / 4))    # QPSK
            s = np.fft.ifft(F) * NFFT
            out.append(np.r_[s[-cp:], s])                            # prepend CP
    x = np.concatenate(out)
    x = x / np.sqrt(np.mean(np.abs(x) ** 2))
    npow = 10 ** (-snr_db / 10)
    x = x + np.sqrt(npow / 2) * (rng.standard_normal(len(x)) +
                                 1j * rng.standard_normal(len(x)))
    n = np.arange(len(x))
    return x * np.exp(1j * 2 * np.pi * cfo / fs * n)


def _selftest():
    rng = np.random.default_rng(1)
    fs = LTE_FS
    ok = True
    # 300 slots ~ 0.15 s capture; sweep CFO and SNR (cells are usually strong)
    qsig = []
    for cfo in (+537.0, -823.0, +61.0, +3120.0, -6450.0):
        for snr in (20, 6, 0):
            x = _make_ofdm(300, cfo, fs, snr, rng=rng)
            est, q = measure_cfo(x, fs)
            err = est - cfo
            # one 0.15 s capture; <25 Hz is ~0.033 ppm at 750 MHz, and live runs
            # average several passes with MAD rejection for ~0.01 ppm.
            good = abs(err) < 25.0
            ok = ok and good
            qsig.append(q)
            print(f"  cfo={cfo:+8.1f} snr={snr:3d}dB -> est={est:+8.1f}"
                  f"  err={err:+6.2f} Hz  q={q:.3f}  {'PASS' if good else 'FAIL'}")
    # noise-only must read low quality (no false lock)
    nse = (rng.standard_normal(300 * SLOT) + 1j * rng.standard_normal(300 * SLOT))
    _, qn = measure_cfo(nse, fs)
    sep = qn < min(qsig)
    print(f"  noise-only quality q={qn:.3f}  (min signal q={min(qsig):.3f})")
    print("\nself-test:", "PASS" if ok and sep else "FAIL")
    return 0 if (ok and sep) else 1


# ---------- main ----------
def main():
    args = sys.argv[1:]
    if "--selftest" in args:
        sys.exit(_selftest())

    def opt(name, d):
        return args[args.index(name) + 1] if name in args else d
    gain = float(opt("--gain", "50"))
    passes = int(opt("--passes", "6"))
    dur = float(opt("--dur", "0.15"))
    do_apply = "--apply" in args

    base = json.loads(api("GET", "/api"))["ad9361"]
    print(f"baseline rx_lo={base['rx_lo_frequency']} gain={base['rx_gain']}\n")

    if "--freq" in args:
        fc = float(opt("--freq", "751.0")) * 1e6
    else:
        print("no --freq: scanning for LTE downlink carriers "
              f"(gain {gain} dB) ...")
        cands = scan_bands(gain)
        if not cands:
            print("\nno LTE carriers found in the common US bands.")
            print("restoring airband LO/gain ...")
            set_gain(AIRBAND_GAIN); set_lo(AIRBAND_LO)
            print("If you know a cell's DL-center MHz, pass it with --freq.")
            return
        print("\ndetected carriers (center MHz, ~BW, power over floor):")
        for c, bw, pw in cands[:8]:
            print(f"  {c:8.1f} MHz   ~{bw:4.1f} MHz   {pw:5.1f} dB")
        rough = cands[0][0]
        print(f"\nstrongest: {rough:.1f} MHz; resolving exact raster center ...")
        restart_maia()
        true_c = find_true_center(rough, gain)
        if true_c is None:
            print("could not lock a center; restoring and exiting.")
            set_gain(AIRBAND_GAIN); set_lo(AIRBAND_LO)
            return
        fc = true_c * 1e6
        print(f"  -> true center {fc/1e6:.1f} MHz")

    print(f"\ntuning LO -> {fc/1e6:.3f} MHz, gain -> {gain} dB ...")
    restart_maia()      # fresh memory for the measurement passes (96 MB board OOMs)
    cfos, quals = measure_freq(fc, gain, passes, dur)

    print("\nrestoring airband LO/gain (no reboot) ...")
    set_gain(AIRBAND_GAIN); set_lo(AIRBAND_LO); time.sleep(0.4)
    print(f"  rx_lo now {cur_lo()} Hz")
    if not cfos:
        print("no measurements")
        return
    cfos = np.array(cfos); quals = np.array(quals)
    if quals.max() < 0.1:
        print(f"\nWARNING: low lock quality (max q={quals.max():.3f}). Likely no LTE"
              " downlink at this frequency -- pick a stronger cell/center.")
    med = np.median(cfos)
    mad = np.median(np.abs(cfos - med)) * 1.4826 + 1e-9
    keep = (np.abs(cfos - med) <= max(3 * mad, 20.0)) & (quals > 0.1)
    if not keep.any():
        keep = np.ones(len(cfos), bool)
    cfo_mean = cfos[keep].mean(); cfo_std = cfos[keep].std()
    ppm = (-cfo_mean / fc) * 1e6
    believed = believed_refclk()
    override = round(believed * fc / (fc + cfo_mean))
    print(f"\n=== LTE-CP calibration ({keep.sum()}/{len(cfos)} kept,"
          f" mean q={quals[keep].mean():.3f}) ===")
    print(f"  cell DL center : {fc/1e6:.3f} MHz")
    print(f"  carrier CFO    : {cfo_mean:+.1f} Hz  (+/- {cfo_std:.1f})")
    print(f"  XO error       : {ppm:+.3f} ppm  [{'HIGH' if ppm>0 else 'LOW'}]")
    print(f"  believed refclk: {believed:.0f} Hz")
    print(f"  -> ad936x_ext_refclk_override = {override}")
    if do_apply:
        print("\napplying (note REQUIRED angle brackets) ...")
        ssh(f"fw_setenv ad936x_ext_refclk_override '<{override}>'")
        print(" ", ssh("fw_printenv ad936x_ext_refclk_override").stdout.strip())
        print("rebooting; the MAC/IP may change. Re-run with the new --host/PLUTO_HOST")
        print("and the same --freq to confirm CFO ~0 (iterate if needed).")
        ssh("reboot")
    else:
        print(f"\n  apply:  ssh root@{HOST} 'fw_setenv ad936x_ext_refclk_override "
              f'"<{override}>"\' && ssh root@{HOST} reboot')
        print("  then re-run with --freq to confirm CFO ~0 (iterate if needed).")


if __name__ == "__main__":
    main()
