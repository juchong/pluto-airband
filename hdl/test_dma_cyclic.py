#!/usr/bin/env python3
"""Verify the new cyclic-ring mode of maia_hdl.dma.DmaStreamWrite.

The recorder uses the one-shot mode (stop at end). The airband audio stream
needs a continuous hardware ring: when the write pointer reaches `end` it must
wrap back to `start` and keep going, never stopping (only `stop` halts it).

This drives a trivial always-accepting AXI3 write subordinate and checks:
  * every issued AWADDR stays in [start, end) (the end address is never written)
  * the pointer wraps start->...->end-burst->start repeatedly (>= 2 full loops)
  * the DMA keeps streaming the whole time (no stall / no `finished`)

Run:  python hdl/test_dma_cyclic.py
"""
from __future__ import annotations

import sys
import os

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..',
                                'maia-sdr', 'maia-hdl'))

from amaranth.sim import Simulator                      # noqa: E402
from maia_hdl.dma import DmaStreamWrite                 # noqa: E402

START = 0x0000_f000
END = 0x0001_1000           # 8 KiB ring = 1024 x 64-bit words = 64 bursts
WORD = 8
BURST_BYTES = 16 * WORD     # 16-beat bursts of 8-byte words = 128 bytes


def main():
    dut = DmaStreamWrite(START, END, width=64, cyclic=True)

    issued = []
    beats = 0
    finished_pulses = 0

    async def bench(ctx):
        nonlocal beats, finished_pulses
        ctx.set(dut.start, 0)
        ctx.set(dut.stop, 0)
        ctx.set(dut.stream_valid, 1)
        ctx.set(dut.stream_data, 0)
        # always-accepting subordinate
        ctx.set(dut.axi.awready, 1)
        ctx.set(dut.axi.wready, 1)
        pending_b = 0
        # kick off
        await ctx.tick()
        ctx.set(dut.start, 1)
        await ctx.tick()
        ctx.set(dut.start, 0)

        for _ in range(6000):
            # subordinate: one write response per completed burst (wlast beat)
            aw_hs = ctx.get(dut.axi.awvalid) and ctx.get(dut.axi.awready)
            w_hs = ctx.get(dut.axi.wvalid) and ctx.get(dut.axi.wready)
            wlast = ctx.get(dut.axi.wlast)
            if aw_hs:
                issued.append(ctx.get(dut.axi.awaddr))
            if w_hs:
                beats += 1
                ctx.set(dut.stream_data, beats)
                if wlast:
                    pending_b += 1
            if ctx.get(dut.finished):
                finished_pulses += 1
            # drive bvalid for outstanding responses; clear as accepted
            if ctx.get(dut.axi.bvalid) and ctx.get(dut.axi.bready):
                pending_b = max(0, pending_b - 1)
            ctx.set(dut.axi.bvalid, 1 if pending_b > 0 else 0)
            await ctx.tick()

    sim = Simulator(dut)
    sim.add_clock(16e-9)
    sim.add_testbench(bench)
    sim.run()

    assert issued, 'no AW addresses were issued'
    lo, hi = min(issued), max(issued)
    assert lo == START, f'min addr {lo:#x} != start {START:#x}'
    assert hi == END - BURST_BYTES, \
        f'max addr {hi:#x} != last burst {END - BURST_BYTES:#x} (end written!)'
    assert all(START <= a < END for a in issued), 'addr out of [start,end)'

    # count wraps: a wrap is an address that decreases back toward start
    wraps = sum(1 for a, b in zip(issued, issued[1:]) if b < a)
    assert wraps >= 2, f'expected >= 2 ring wraps, saw {wraps}'
    assert finished_pulses == 0, \
        f'cyclic DMA must not pulse finished (saw {finished_pulses})'

    print(f'[cyclic DMA] PASS: {len(issued)} bursts issued, {wraps} ring wraps, '
          f'addr range [{lo:#x}, {hi:#x}] within [{START:#x}, {END:#x}), '
          f'{beats} beats streamed, finished never pulsed.')


if __name__ == '__main__':
    main()
