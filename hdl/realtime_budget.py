#!/usr/bin/env python3
"""Real-time throughput budget for the time-multiplexed receiver (§4.2, cycle level).

The §4.2 feasibility GATE checked *area* (LUT/FF/DSP/BRAM). This checks the other
half: can each shared (folded) datapath keep up with the sample cadence? The OOC
numbers (dec-64 CIC, 119-tap cleanup) were chosen to *measure resources*, not to
close real-time timing, and the functional sims used a relaxed inter-sample gap.
Here we make the cycle budget explicit and pick deployment parameters that fit,
then stress-test ``ReceiverTop`` at the true cadence (one input every
``Fpl/Fs`` cycles) and confirm **nothing is dropped** (overflow stays 0).

Binding duty cycles (must each stay < 1, with margin):

  lane sweep : the lane visits all its channels one-per-cycle every input sample,
               so   duty_lane = chans_per_lane / (Fpl / Fs).
  cleanup FIR: folded MAC, ~ntaps cycles per channel-output, at the channel rate
               (Fs/lane_decim) for each of its channels:
               duty_fir = chans_per_lane * (Fs/lane_decim) * (ntaps+ovh) / Fpl.
  AM back-end: ~4 cycles per channel-output, shared across all N channels:
               duty_am  = N * (Fs/lane_decim) * 4 / Fpl.

duty_lane forces chans_per_lane <= floor(Fpl/Fs); duty_fir forces enough lane
decimation for the chosen tap count. Both are satisfied by the recommended config.

Run:  python hdl/realtime_budget.py
"""
from __future__ import annotations

import math

import numpy as np
from amaranth.sim import Simulator

from channelizer_chain import design_cic_compensation
from ddc_tune_decimate import SYNC_PERIOD
from receiver_top import ReceiverTop
from audio_framer import AudioFramer

F_PL = 62.5e6          # PL sync clock (handoff §4.2)
F_S = 16.0e6           # resolved capture rate (§8.2: 16 MHz to admit 133.65 MHz)
N = 18                 # core channels (18 ch / 6 lanes; 21 ch -> 7 lanes overflows LUTs)


def duties(Fs, Fpl, n, cpl, lane_decim, ntaps, am_cycles=23, fir_ovh=6):
    # am_cycles: TdmAmBackend is pipelined one arithmetic stage per clock to
    # close 62.5 MHz timing -> worst case (decimation boundary) is
    # ~cordic(12)+scale+DC+integ(S)+comb(S)+accept = 23 for S=4 (the CORDIC
    # vectoring magnitude replaced the 1-cycle alpha-max-beta-min estimator to
    # remove the AM buzz; see am_backend_tdm.py). The AM datapath is dedicated,
    # so this only needs duty_am < 1 with margin -- it stays ~0.85 at decim=128.
    n_lanes = math.ceil(n / cpl)
    base, rem = divmod(n, n_lanes)
    max_size = base + (1 if rem else 0)
    cyc_in = Fpl / Fs
    ch_rate = Fs / lane_decim
    return {
        "n_lanes": n_lanes, "max_size": max_size, "cyc_in": cyc_in,
        "ch_rate": ch_rate,
        "duty_lane": max_size / cyc_in,
        "duty_fir": max_size * ch_rate * (ntaps + fir_ovh) / Fpl,
        "duty_am": n * ch_rate * am_cycles / Fpl,
    }


def _print_table():
    print(f"Fpl={F_PL/1e6:.1f} MHz  Fs={F_S/1e6:.1f} MHz  "
          f"cycles/input={F_PL/F_S:.2f}  -> chans_per_lane <= {int(F_PL//F_S)}\n")
    print(f"{'cpl':>3} {'decim':>6} {'ntaps':>6} {'lanes':>6} {'ch_rate':>9} "
          f"{'duty_lane':>10} {'duty_fir':>9} {'duty_am':>8}  fit")
    best = None
    for cpl in (3, 4):
        for lane_decim in (64, 128, 160, 175, 256):
            for ntaps in (63, 119):
                d = duties(F_S, F_PL, N, cpl, lane_decim, ntaps)
                fit = (d["duty_lane"] < 0.95 and d["duty_fir"] < 0.9
                       and d["duty_am"] < 0.9)
                print(f"{cpl:>3} {lane_decim:>6} {ntaps:>6} {d['n_lanes']:>6} "
                      f"{d['ch_rate']/1e3:>7.1f}k {d['duty_lane']:>10.2f} "
                      f"{d['duty_fir']:>9.2f} {d['duty_am']:>8.3f}  "
                      f"{'OK' if fit else 'no'}")
                if fit and best is None and cpl == 3:
                    best = (cpl, lane_decim, ntaps)
    return best


def _audio_decim_for(Fs, lane_decim, target_audio=20e3):
    ch_rate = Fs / lane_decim
    return max(2, round(ch_rate / target_audio))


def _stress(cpl=3, lane_decim=64, ntaps=63, audio_decim=4, n_channels=6,
            n_in=2600, seed=7):
    """Drive ReceiverTop at the true cadence (one input every Fpl/Fs sync cycles
    on average, with the realistic gap=3/gap=4 jitter for a non-integer ratio) and
    report whether any stage overflows and whether the framed audio is bit-exact.

    ``audio_decim`` is kept small here (independent of the 16 ksps deployment
    value) so a few audio samples appear within a short sim; the real-time
    stress comes from the inter-sample ``gap`` and the lane/FIR burst cadence,
    which depend on ``lane_decim``/``ntaps``/``cpl``, not on ``audio_decim``."""
    coeffs, out_shift = design_cic_compensation(8, 5, ntaps, 0.35, 0.60)
    dut = ReceiverTop(n_channels=n_channels, chans_per_lane=cpl,
                      decimation=lane_decim, coeffs=coeffs, out_shift=out_shift,
                      audio_decim=audio_decim, cic_stages=3, dcblock_k=10,
                      in_width=12, nco_width=24, stages=3, audio_sample_w=24)
    # True inter-sample spacing is Fpl/Fs sync cycles. For a non-integer ratio
    # (e.g. 62.5/16 = 3.906) the CDC delivers a sample every 3 or 4 cycles so the
    # AVERAGE is 3.906; a fixed integer floor (3) would feed ~30% too fast and
    # falsely over-stress the lanes. Drive the realistic fractional cadence with a
    # phase accumulator below; `gap` here is just the nominal average for display.
    cyc_per_in = F_PL / F_S
    gap = cyc_per_in
    rng = np.random.default_rng(seed)
    lim = 2 ** 11 - 1
    samples = rng.integers(-lim, lim, size=(n_in, 2)).astype(np.int64)
    freqs = [int(round(f * 2 ** dut.nco_width))
             for f in np.linspace(-0.2, 0.2, n_channels)]
    words = []
    overflowed = [False]

    async def bench(ctx):
        ctx.set(dut.stream_ready, 1)
        ctx.set(dut.freq_wren, 1)
        for c, f in enumerate(freqs):
            ctx.set(dut.freq_waddr, c)
            ctx.set(dut.freq_wdata, int(f) & (2 ** dut.nco_width - 1))
            await ctx.tick()
        ctx.set(dut.freq_wren, 0)
        await ctx.tick()

        async def step():
            if ctx.get(dut.overflow):
                overflowed[0] = True
            if ctx.get(dut.stream_valid):
                words.append(ctx.get(dut.stream_data))
            await ctx.tick()

        acc = 0.0
        for (re, im) in samples:
            ctx.set(dut.re_in, int(re))
            ctx.set(dut.im_in, int(im))
            ctx.set(dut.in_valid, 1)
            await step()
            ctx.set(dut.in_valid, 0)
            acc += cyc_per_in - 1.0       # cycles owed before the next sample
            idle = int(acc)
            acc -= idle
            for _ in range(idle):
                await step()
        for _ in range(4000):
            await step()

    sim = Simulator(dut)
    sim.add_clock(SYNC_PERIOD)
    sim.add_testbench(bench)
    sim.run()

    # bit-exact check (per channel) against the model, plus no overflow.
    # IMPORTANT: also require the HW to emit ~all the model's audio samples. A
    # lane that silently drops inputs (the half-rate / 2x-detune bug) produces
    # HALF as many audio samples; comparing only a min-length prefix would mask
    # that (and the prefix would itself mismatch once tuning differs). We assert
    # both full-prefix bit-exactness AND that no more than the final in-flight
    # frame is missing per channel.
    per = {c: [] for c in range(n_channels)}
    for w in words:
        _, chan, sample, _carrier = AudioFramer.unpack(w)
        per[chan].append(sample)
    ref = dut.model(samples, freqs)
    bit_ok = True
    complete = True
    for c in range(n_channels):
        g = np.array(per[c], np.int64)
        e = ref[c]
        kk = min(len(g), len(e))
        if kk == 0 or not np.array_equal(g[:kk], e[:kk]):
            bit_ok = False
        if len(g) < len(e) - 1:          # dropped inputs -> too few audio samples
            complete = False
    return dut, gap, audio_decim, (not overflowed[0]), bit_ok and complete, len(words)


def main():
    best = _print_table()
    print(f"\nRecommended deployment config (3 ch/lane fits at Fs=16 MHz): "
          f"chans_per_lane={best[0]}, lane_decim={best[1]}, cleanup ntaps={best[2]} "
          f"-> {math.ceil(N/best[0])} lanes; "
          f"audio_decim={_audio_decim_for(F_S, best[1])} gives "
          f"{F_S/best[1]/_audio_decim_for(F_S, best[1])/1e3:.1f} ksps audio.")

    print("\nStress test: drive ReceiverTop at the true cadence "
          f"(one input every ~{F_PL/F_S:.2f} cycles on average, Fpl/Fs)...")

    # The stress configs use the DEPLOYMENT decimation (lane_decim=160), not a
    # speed-shrunk one: at Fs=16 the fractional cadence has occasional gap=3 bursts
    # (~1 in 10 samples), and a near-saturated FIR (e.g. lane_decim=64 -> duty_fir
    # 0.83) cannot absorb them. The shipped lane_decim=160 (duty_fir 0.33) has ample
    # margin and stays bit-exact.

    # (1) a budget-fitting config (extra lane margin): overflow-free AND bit-exact.
    dut, gap, ad, no_ovf, bit_ok, nwords = _stress(
        cpl=2, lane_decim=160, ntaps=63, audio_decim=5, n_channels=4, n_in=4000)
    print(f"  [fits]   {dut.n_channels} ch / {dut.n_lanes} lanes, cpl=2, decim=160, "
          f"63-tap, gap~{gap:.2f} -> {nwords} records, overflow-free={no_ovf}, "
          f"bit-exact+complete={bit_ok}")
    assert no_ovf, "FIFO overflow at real-time cadence: parameters do not fit"
    assert bit_ok, "framed audio not bit-exact/complete under real-time cadence"

    # (1b) the DEPLOYMENT geometry (chans_per_lane=3, lane_decim=160) at the true
    #      fractional cadence. This is the config that ships at Fs=16 MHz; the
    #      pipelined lanes must accept EVERY input (no half-rate drop) and stay
    #      bit-exact across the gap=3/gap=4 jitter.
    dut, gap, ad, no_ovf, bit_ok, nwords = _stress(
        cpl=3, lane_decim=160, ntaps=63, audio_decim=5, n_channels=8, n_in=4000)
    print(f"  [deploy] {dut.n_channels} ch / {dut.n_lanes} lanes, cpl=3, decim=160, "
          f"63-tap, gap~{gap:.2f} -> {nwords} records, overflow-free={no_ovf}, "
          f"bit-exact+complete={bit_ok}")
    assert no_ovf, "FIFO overflow at real-time cadence (cpl=3): parameters do not fit"
    assert bit_ok, "cpl=3 lane dropped inputs at the true cadence (half-rate bug)"

    # (2) negative control: an over-budget config (FIR too slow, duty>1) MUST
    #     trip the overflow detector -- proves the detector actually fires.
    _, _, _, no_ovf2, _, _ = _stress(
        cpl=3, lane_decim=32, ntaps=119, audio_decim=4, n_in=1500)
    print(f"  [over]   decim=32, 119-tap (duty_fir>1): overflow-free={no_ovf2} "
          f"(expected False)")
    assert not no_ovf2, "over-budget config did not overflow (detector broken?)"

    print("\nPASS: real-time budget closes for the fitting config (no dropped "
          "samples, still bit-exact); the over-budget config correctly overflows.")


if __name__ == "__main__":
    main()
