#!/usr/bin/env python3
"""Connect to the Pluto airband stream, capture N seconds, demux channels, and
report a per-channel buzz metric (comb-band rms + 7625 Hz tone + harmonic comb).
Used to A/B test front-end changes (e.g. tracking-cal on/off, gain).

Reads the decoded airband audio stream at 192.168.2.1:30000."""
from __future__ import annotations
import socket, struct, sys, time
import numpy as np

HOST, PORT = "192.168.2.1", 30000
FR = 15625


def capture(seconds, n_ch=21):
    s = socket.create_connection((HOST, PORT), timeout=5)
    s.settimeout(5)
    buf = bytearray()
    need = int(seconds * n_ch * FR) * 8
    t0 = time.time()
    while len(buf) < need and time.time() - t0 < seconds + 8:
        try:
            d = s.recv(262144)
        except socket.timeout:
            break
        if not d:
            break
        buf.extend(d)
    s.close()
    nrec = len(buf) // 8
    words = np.frombuffer(bytes(buf[:nrec*8]), dtype="<u8")
    sample = (words & 0xffffffff).astype(np.int64)
    sample = np.where(sample >= 2**31, sample - 2**32, sample)
    chan = ((words >> 32) & 0xff).astype(np.int64)
    seq = ((words >> 40) & 0xffffff).astype(np.int64)
    return sample, chan, seq


def per_channel(sample, chan, n_ch=21):
    out = {}
    for c in range(n_ch):
        out[c] = sample[chan == c].astype(float)
    return out


def metric(x):
    if len(x) < 4096:
        return None
    x = x - x.mean()
    n = len(x)
    w = np.hanning(n)
    sp = np.abs(np.fft.rfft(x * w))
    f = np.fft.rfftfreq(n, 1 / FR)
    P = sp**2
    def band(lo, hi):
        m = (f >= lo) & (f < hi)
        return np.sqrt(np.sum(P[m]))
    total = np.sqrt(np.sum(P) + 1e-9)
    # buzz bands
    comb = band(40, 250)
    mid = band(600, 2400)
    nyq = band(7500, 7700)
    rms = x.std()
    # 7625 tone level vs local floor
    m = (f > 7000) & (f < 7800)
    return dict(rms=rms, comb=comb/total, mid=mid/total, nyq=nyq/total,
                tone7625_db=10*np.log10((P[(f>7600)&(f<7650)].max()+1e-9)/(np.median(P[f>1000])+1e-9)))


def main():
    secs = float(sys.argv[1]) if len(sys.argv) > 1 else 6.0
    sample, chan, seq = capture(secs)
    print(f"captured {len(sample)} records ({len(sample)/21/FR:.1f}s/ch)")
    X = per_channel(sample, chan)
    quiet = [20, 14, 7, 19, 13, 8]
    print(f"{'ch':>3} {'rms':>8} {'comb%':>7} {'mid%':>7} {'nyq%':>7} {'7625dB':>7}")
    for c in quiet + [5, 6, 10, 0]:
        m = metric(X[c])
        if m is None:
            print(f"{c:>3}  (insufficient)"); continue
        print(f"{c:>3} {m['rms']:8.1f} {100*m['comb']:7.2f} {100*m['mid']:7.2f} "
              f"{100*m['nyq']:7.2f} {m['tone7625_db']:7.1f}")


if __name__ == "__main__":
    main()
