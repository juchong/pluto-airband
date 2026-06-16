#!/usr/bin/env python3
"""Reproduce the idle-channel buzz in the bit-exact reference models.

Vectorized end-to-end of the production airband chain on an *idle* input
(noise floor + ADC DC offset), to check whether the audible comb / spurs are a
DSP-design artifact (present in the reference model) and, if so, localize the
stage. Result: the DSP chain is clean on idle input (flat noise floor), which
helped prove the buzz originates in the RF front-end, not the HDL.
"""
from __future__ import annotations
import sys, pathlib, math
import numpy as np
import scipy.signal

# ---- inlined bit-exact model math (copied verbatim from the fork) ----
CORDIC_ITERS = 12


def cordic_magnitude(re, im):
    re = np.asarray(re, dtype=np.int64)
    im = np.asarray(im, dtype=np.int64)
    x = np.abs(re).astype(np.int64).copy()
    y = np.abs(im).astype(np.int64).copy()
    for i in range(CORDIC_ITERS):
        dx = x >> i
        dy = y >> i
        pos = y >= 0
        xn = np.where(pos, x + dy, x - dy)
        yn = np.where(pos, y - dx, y + dx)
        x, y = xn, yn
    return (x * 5) >> 3


def dcblock_model(x, k=7):
    x = np.asarray(x, dtype=np.int64)
    y = np.empty_like(x)
    s = np.int64(0)
    for n in range(len(x)):
        dc = s >> k
        y[n] = x[n] - dc
        s = s + y[n]
    return y


def cic_model(x, decimation, stages=3):
    acc = np.asarray(x, dtype=np.int64)
    for _ in range(stages):
        acc = np.cumsum(acc)
    dec = acc[decimation - 1::decimation].copy()
    for _ in range(stages):
        delayed = np.concatenate(([np.int64(0)], dec[:-1]))
        dec = dec - delayed
    return dec


def sine_rom(addr_bits, width):
    n = 1 << addr_bits
    amp = (1 << (width - 1)) - 1
    return [int(round(amp * math.sin(2 * math.pi * k / n))) for k in range(n)]


def cic_response(fo, R, S):
    fo = np.asarray(fo, float)
    with np.errstate(divide="ignore", invalid="ignore"):
        h = (np.sin(np.pi * fo) / (R * np.sin(np.pi * fo / R))) ** S
    h = np.where(fo == 0, 1.0, h)
    return np.abs(h)


def design_cic_compensation(R, S, ntaps, fp, fs, coeff_width=16):
    freq = np.linspace(0.0, 1.0, 1025)
    fo = freq * 0.5
    inv = 1.0 / cic_response(fo, R, S)
    desired = np.zeros_like(freq)
    ft = fp + 0.7 * (fs - fp)
    pb = freq <= fp
    sb = freq >= ft
    tb = (~pb) & (~sb)
    desired[pb] = inv[pb]
    edge = inv[pb][-1] if pb.any() else 1.0
    desired[tb] = edge * (ft - freq[tb]) / (ft - fp)
    h = scipy.signal.firwin2(ntaps, freq, desired, window=("kaiser", 8.6))
    scale = (2 ** (coeff_width - 1) - 1) / np.max(np.abs(h))
    h_int = np.round(h * scale).astype(int)
    out_shift = max(0, int(round(math.log2(abs(int(np.sum(h_int)))))))
    return h_int, out_shift


class TdmAmBackend:
    cordic_magnitude = staticmethod(cordic_magnitude)


class DCBlock:
    model = staticmethod(dcblock_model)


class CICDecimator:
    model = staticmethod(cic_model)

# ---- production params (maia_sdr.py) ----
FS_IN       = 14_000_000      # working rate into the airband receiver (Hz)
LANE_DECIM  = 128            # lane CIC decimation -> channel rate
LANE_STAGES = 3
AUDIO_DECIM = 7
CIC_STAGES  = 4
DCBLOCK_K   = 10
NCO_WIDTH   = 24
LUT_ADDR    = 10
LUT_WIDTH   = 12
FIR_OUT_SHIFT = 17
CHAN_RATE = FS_IN / LANE_DECIM            # 109375 Hz
AUDIO_RATE = CHAN_RATE / AUDIO_DECIM      # 15625 Hz

# cleanup FIR (matches maia_sdr comment: design_cic_compensation(128,3,63,0.22,0.46))
FIR_COEFFS, _shift = design_cic_compensation(128, 3, 63, 0.22, 0.46)
print(f"cleanup FIR taps={len(FIR_COEFFS)} sum={int(np.sum(FIR_COEFFS))} given_shift={_shift} used_shift={FIR_OUT_SHIFT}")
print(f"chan_rate={CHAN_RATE} Hz  audio_rate={AUDIO_RATE} Hz")

ROM = np.array(sine_rom(LUT_ADDR, LUT_WIDTH), dtype=np.int64)
AMASK = (1 << LUT_ADDR) - 1
NMASK = (1 << NCO_WIDTH) - 1
QUARTER = 1 << (LUT_ADDR - 2)
SHIFT_PHASE = NCO_WIDTH - LUT_ADDR
MIX_SHIFT = LUT_WIDTH - 1


def nco_mix(re, im, fword):
    n = np.arange(len(re), dtype=np.int64)
    phase = (fword * n) & NMASK
    a = (phase >> SHIFT_PHASE) & AMASK
    s = ROM[a]
    co = ROM[(a + QUARTER) & AMASK]
    oi = (re * co + im * s) >> MIX_SHIFT
    oq = (im * co - re * s) >> MIX_SHIFT
    return oi, oq


def cic_decim(x, decim, stages):
    acc = np.asarray(x, dtype=np.int64)
    for _ in range(stages):
        acc = np.cumsum(acc)
    dec = acc[decim - 1::decim].copy()
    for _ in range(stages):
        dec = dec - np.concatenate(([np.int64(0)], dec[:-1]))
    return dec


def fir(x, coeffs, out_shift):
    h = np.asarray([int(c) for c in coeffs], dtype=np.int64)
    full = np.convolve(x.astype(np.int64), h)[:len(x)]
    return full >> out_shift   # arithmetic (numpy floor for negatives via //? use shift)


def fir_floor(x, coeffs, out_shift):
    h = np.asarray([int(c) for c in coeffs], dtype=np.int64)
    full = np.convolve(x.astype(np.int64), h)[:len(x)]
    # arithmetic right shift = floor division by 2**out_shift
    return np.floor_divide(full, (1 << out_shift))


def channel_chain(re, im, fword, *, return_stage=None):
    oi, oq = nco_mix(re, im, fword)
    ci = cic_decim(oi, LANE_DECIM, LANE_STAGES)
    cq = cic_decim(oq, LANE_DECIM, LANE_STAGES)
    yi = fir_floor(ci, FIR_COEFFS, FIR_OUT_SHIFT)
    yq = fir_floor(cq, FIR_COEFFS, FIR_OUT_SHIFT)
    if return_stage == "baseband":
        return yi, yq
    mag = cordic_magnitude(yi, yq)
    if return_stage == "mag":
        return mag
    y = dcblock_model(mag, k=DCBLOCK_K)
    if return_stage == "dcblock":
        return y
    audio = cic_model(y, AUDIO_DECIM, stages=CIC_STAGES)
    return audio


def spectrum(audio, fs=AUDIO_RATE, drop=0.2):
    a = audio.astype(float)
    a = a[int(len(a) * drop):]
    a = a - np.mean(a)
    w = np.hanning(len(a))
    sp = np.abs(np.fft.rfft(a * w))
    f = np.fft.rfftfreq(len(a), 1 / fs)
    sp_db = 20 * np.log10(sp / (np.max(sp) + 1e-12) + 1e-12)
    return f, sp_db


def top_peaks(f, sp_db, n=12, fmin=20):
    idx = np.where(f >= fmin)[0]
    order = idx[np.argsort(sp_db[idx])[::-1]]
    out = []
    used = []
    for i in order:
        if all(abs(f[i] - u) > 30 for u in used):
            out.append((f[i], sp_db[i]))
            used.append(f[i])
        if len(out) >= n:
            break
    return out


def main():
    rng = np.random.default_rng(1234)
    n_audio_target = 6000
    n_chan = n_audio_target * AUDIO_DECIM + 4000
    n_in = n_chan * LANE_DECIM
    print(f"n_in={n_in} samples ({n_in/FS_IN*1000:.1f} ms)")

    # idle input: noise floor (12-bit, small) + ADC DC offset
    noise_std = 6.0
    dc_re, dc_im = 9, -5
    re = np.round(rng.normal(0, noise_std, n_in) + dc_re).astype(np.int64)
    im = np.round(rng.normal(0, noise_std, n_in) + dc_im).astype(np.int64)
    re = np.clip(re, -2047, 2047)
    im = np.clip(im, -2047, 2047)

    # 21 channels across +/-250 kHz; look at a couple "idle" ones
    offsets = (np.arange(21) - 10) * 25_000.0
    fwords = [int(round(off / FS_IN * 2 ** NCO_WIDTH)) & NMASK for off in offsets]

    for ch in (5, 6, 10):
        fword = fwords[ch]
        audio = channel_chain(re, im, fword)
        f, sp = spectrum(audio)
        pk = top_peaks(f, sp)
        print(f"\n=== ch{ch} off={offsets[ch]/1e3:+.0f}kHz fword={fword} "
              f"({len(audio)} audio samp) ===")
        for fr, db in pk:
            print(f"   {fr:8.1f} Hz   {db:6.1f} dB")


if __name__ == "__main__":
    main()
