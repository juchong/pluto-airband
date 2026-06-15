#!/usr/bin/env python3
"""Diagnostic: does a TdmDdcLaneBRAM keep up at the true 14 MHz cadence?

Drives the lane with a single complex tone, feeding one wideband input every
`gap` PL cycles. Counts decimated outputs; accepted_inputs = outputs * decim.
If the lane drops inputs (busy gating), accepted_inputs < fed_inputs and the
effective rate (hence NCO tuning) is wrong.

Also reports where the tone lands: tuned to the channel's NCO word, the output
should be DC-like (|mean| >> std). If the lane runs at half the fed rate, a tone
placed at offset f (cyc per fed-sample) needs word for 2*f to land at DC.
"""
from __future__ import annotations

import numpy as np
from amaranth.sim import Simulator

from channelizer_lane import TdmDdcLaneBRAM
from ddc_tune_decimate import SYNC_PERIOD


def run(cpl, decimation, gap, n_in, tone_off, word_off):
    dut = TdmDdcLaneBRAM(n_channels=cpl, decimation=decimation, in_width=12,
                         nco_width=24, lut_addr_bits=10, lut_width=12, stages=3)
    nn = np.arange(n_in)
    tone = 1500 * np.exp(1j * 2 * np.pi * tone_off * nn)
    samples = np.stack([np.round(tone.real), np.round(tone.imag)], 1).astype(np.int64)
    word = int(round(word_off * 2 ** dut.nco_width)) & (2 ** dut.nco_width - 1)
    out0 = {"i": [], "q": []}
    fed = [0]

    async def bench(ctx):
        ctx.set(dut.freq_wren, 1)
        ctx.set(dut.freq_waddr, 0)
        ctx.set(dut.freq_wdata, word)
        await ctx.tick()
        for c in range(1, cpl):
            ctx.set(dut.freq_waddr, c)
            ctx.set(dut.freq_wdata, 0)
            await ctx.tick()
        ctx.set(dut.freq_wren, 0)
        await ctx.tick()

        async def step():
            if ctx.get(dut.out_valid) and ctx.get(dut.out_chan) == 0:
                out0["i"].append(ctx.get(dut.out_re))
                out0["q"].append(ctx.get(dut.out_im))
            await ctx.tick()

        for (re, im) in samples:
            ctx.set(dut.re_in, int(re))
            ctx.set(dut.im_in, int(im))
            ctx.set(dut.in_valid, 1)
            await step()
            fed[0] += 1
            ctx.set(dut.in_valid, 0)
            for _ in range(gap - 1):
                await step()
        for _ in range(gap * cpl + 200):
            await step()

    sim = Simulator(dut)
    sim.add_clock(SYNC_PERIOD)
    sim.add_testbench(bench)
    sim.run()

    nout = len(out0["i"])
    accepted = nout * decimation
    z = np.array(out0["i"], float) + 1j * np.array(out0["q"], float)
    z = z[len(z) // 4:] if len(z) > 8 else z
    dc = np.abs(np.mean(z)) if len(z) else 0.0
    ac = np.std(z) if len(z) else 0.0
    return fed[0], nout, accepted, dc, ac


def main():
    cpl, decim = 4, 128
    n_in = decim * 16
    print(f"lane cpl={cpl} decim={decim}, feeding {n_in} inputs\n")
    print(f"{'gap':>4} {'fed':>6} {'outs':>5} {'accepted':>9} {'accept/fed':>11} "
          f"{'|DC|@f':>9} {'|DC|@2f':>9}")
    for gap in (40, 8, 6, 5, 4):
        # tone at offset 0.1 cyc/fed-sample. If lane keeps up, word for 0.1 -> DC.
        f1, o1, a1, dc1, ac1 = run(cpl, decim, gap, n_in, 0.1, 0.1)
        # word for 2*0.1=0.2: lands at DC only if lane runs at HALF the fed rate.
        f2, o2, a2, dc2, ac2 = run(cpl, decim, gap, n_in, 0.1, 0.2)
        ratio = a1 / f1 if f1 else 0
        print(f"{gap:>4} {f1:>6} {o1:>5} {a1:>9} {ratio:>11.3f} "
              f"{dc1:>9.0f} {dc2:>9.0f}")


if __name__ == "__main__":
    main()
