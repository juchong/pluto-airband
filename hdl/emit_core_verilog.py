#!/usr/bin/env python3
"""Emit synthesizable Verilog for the integrated ChannelizerCore.

Used for the combined Vivado place-and-route run on the build server (real
LUT/FF/DSP/BRAM + timing for the whole per-lane datapath, not just the isolated
blocks). Config mirrors one deployment lane: ~5 channels share the datapath, a
3-stage CIC decimates, and the verified 119-tap CIC droop-comp / selectivity FIR
runs folded over the channels (complex -> I and Q engines).

Run:  python hdl/emit_core_verilog.py   ->   out/channelizer_core.v
"""
from __future__ import annotations

import pathlib

from amaranth.back import verilog
from amaranth.hdl import Fragment

from channelizer_chain import design_cic_compensation
from channelizer_core import ChannelizerCore

OUT_DIR = pathlib.Path(__file__).parent / "out"

N_CHANNELS = 5
DECIMATION = 64
COMP_NTAPS = 119


def build():
    coeffs, out_shift = design_cic_compensation(8, 5, COMP_NTAPS, 0.35, 0.60)
    return ChannelizerCore(n_channels=N_CHANNELS, decimation=DECIMATION,
                           coeffs=coeffs, out_shift=out_shift, in_width=12,
                           nco_width=24, lut_addr_bits=10, lut_width=12, stages=3)


def main():
    OUT_DIR.mkdir(exist_ok=True)
    dut = build()
    ports = [dut.freq_wren, dut.freq_waddr, dut.freq_wdata,
             dut.in_valid, dut.re_in, dut.im_in,
             dut.out_valid, dut.out_chan, dut.out_re, dut.out_im]
    frag = Fragment.get(dut, None)
    text = verilog.convert(frag, name="channelizer_core", ports=ports)
    path = OUT_DIR / "channelizer_core.v"
    path.write_text(text)
    print(f"wrote {path}  ({len(text.splitlines())} lines)  "
          f"N={N_CHANNELS} decim={DECIMATION} comp_ntaps={COMP_NTAPS}")


if __name__ == "__main__":
    main()
