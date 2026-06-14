#!/usr/bin/env python3
"""Synthesize the channelizer building blocks to hard 7-series resources.

Feasibility-gate helper (handoff §4.2). Emits Verilog for maia-hdl's ``DDC`` and
for our AM back-end (``EnvelopeMagnitude`` + ``DCBlock`` + ``CICDecimator``),
then runs Yosys ``synth_xilinx -family xc7`` and parses the cell counts into
LUT / FF / DSP48E1 / BRAM numbers.

These are *Yosys* estimates, not Vivado. Use them as a relative/architectural
signal (esp. DSP48E1 and BRAM, which are the gating resources on the Z-7010);
absolute LUT/FF differ from Vivado and must be confirmed on the build server.

Run:  python hdl/synth_estimate.py
"""
from __future__ import annotations

import pathlib
import re
import shutil
import subprocess

from amaranth import Elaboratable, Module, Signal
from amaranth.back import verilog
from amaranth.hdl import Fragment

from maia_hdl.ddc import DDC

from am_demod import EnvelopeMagnitude
from am_audio import DCBlock, CICDecimator
from channelizer_lane import TdmDdcLane
from channelizer_chain import (FIRStage, FrontEndDecimator, design_lowpass,
                               design_cic_compensation)

OUT_DIR = pathlib.Path(__file__).parent / "out"


class AMBackEnd(Elaboratable):
    """Per-channel AM back-end after the DDC: |.| -> DC block -> CIC decimate."""

    def __init__(self, *, ddc_out_width=16, audio_decim=6, cic_stages=3,
                 dcblock_k=8):
        self.re_in = Signal(ddc_out_width)
        self.im_in = Signal(ddc_out_width)
        self.strobe_in = Signal()
        self.mag = EnvelopeMagnitude(ddc_out_width)
        self.dcblk = DCBlock(self.mag.ow + 1, k=dcblock_k)
        self.cic = CICDecimator(self.dcblk.w, audio_decim, stages=cic_stages)
        self.audio_out = self.cic.y_out
        self.audio_strobe = self.cic.strobe_out

    def elaborate(self, platform):
        m = Module()
        m.submodules.mag = mag = self.mag
        m.submodules.dcblk = dcblk = self.dcblk
        m.submodules.cic = cic = self.cic
        m.d.comb += [
            mag.clken.eq(1),
            mag.re_in.eq(self.re_in.as_signed()),
            mag.im_in.eq(self.im_in.as_signed()),
            dcblk.clken.eq(self.strobe_in),
            dcblk.x_in.eq(mag.mag_out),
            cic.clken.eq(self.strobe_in),
            cic.x_in.eq(dcblk.y_out),
        ]
        return m


def _ports(dut):
    return [v for v in vars(dut).values() if isinstance(v, Signal)]


def _emit_verilog(dut, name, path):
    frag = Fragment.get(dut, None)
    text = verilog.convert(frag, name=name, ports=_ports(dut))
    path.write_text(text)


def _synth(path, top):
    script = (
        f"read_verilog -sv {path}; "
        f"synth_xilinx -family xc7 -top {top} -flatten; stat"
    )
    res = subprocess.run(["yosys", "-p", script],
                         capture_output=True, text=True)
    if res.returncode != 0:
        return None, res.stderr[-2000:]
    return res.stdout, None


def _parse(stat: str) -> dict:
    # Use only the final, local (non-hierarchical) stat block.
    if "=== DDC ===" in stat or "===" in stat:
        stat = stat[stat.rfind("=== "):]
    counts: dict[str, int] = {}
    for line in stat.splitlines():
        m = re.match(r"\s+(\d+)\s+(\S+)\s*$", line)   # "  <count>   <CELL>"
        if m:
            counts[m.group(2)] = int(m.group(1))

    def total(pred):
        return sum(n for c, n in counts.items() if pred(c))

    luts = total(lambda c: c.startswith("LUT"))
    lutram = total(lambda c: c.startswith("RAM") and not c.startswith("RAMB"))
    ffs = total(lambda c: c.startswith(("FD", "LD")))
    carry = total(lambda c: c.startswith("CARRY"))
    dsp = total(lambda c: c.startswith("DSP48"))
    ramb36 = total(lambda c: c.startswith("RAMB36") or c.startswith("RAMB72"))
    ramb18 = total(lambda c: c.startswith("RAMB18"))
    bram36 = ramb36 + (ramb18 + 1) // 2     # 2x RAMB18 ~ 1x 36Kb block
    return {"LUT": luts, "LUTRAM": lutram, "FF": ffs, "CARRY4": carry,
            "DSP48E1": dsp, "BRAM36": bram36, "RAMB18": ramb18,
            "RAMB36": ramb36}


# Z-7010 (XC7Z010) programmable-logic budget.
Z7010 = {"LUT": 17600, "FF": 35200, "DSP48E1": 80, "BRAM36": 60}

# Channelizer-chain filter realizations (must match channelizer_chain.py).
FE_DECIM = 4
FE_COEFFS, FE_SHIFT = design_lowpass(95, 0.95 / FE_DECIM, coeff_width=16)
COMP_NTAPS = 119
COMP_COEFFS, COMP_SHIFT = design_cic_compensation(8, 5, COMP_NTAPS, 0.35, 0.60,
                                                  coeff_width=16)


def main():
    OUT_DIR.mkdir(exist_ok=True)
    if shutil.which("yosys") is None:
        raise SystemExit("yosys not found on PATH")

    duts = [
        ("ddc", "DDC", DDC("clk3x", in_width=12, nco_width=28)),
        ("am_backend", "AMBackEnd", AMBackEnd()),
        # Time-multiplexed channelizer lane (NCO+mixer+CIC shared over N ch).
        # Measured at two channel depths to separate the (≈constant) arithmetic
        # datapath from the per-channel state that scales with the channel count.
        ("lane_ch8", "TdmDdcLane",
         TdmDdcLane(n_channels=8, decimation=16, stages=3)),
        ("lane_ch21", "TdmDdcLane",
         TdmDdcLane(n_channels=21, decimation=16, stages=3)),
        # Shared front-end decimator (one per receiver) and per-channel CIC droop-
        # compensation FIR. Written fully-parallel here (1 multiplier/tap), so these
        # are an UNROLLED upper bound; the real blocks fold the taps onto a small MAC
        # engine -- see the folded MAC-rate note below.
        ("frontend", "FrontEndDecimator",
         FrontEndDecimator(FE_COEFFS, FE_DECIM, 12, FE_SHIFT)),
        ("compfir", "FIRStage",
         FIRStage(COMP_COEFFS, 1, 24, COMP_SHIFT)),
    ]

    print(f"{'block':<14}{'LUT':>8}{'FF':>8}{'CARRY4':>8}"
          f"{'DSP48E1':>9}{'BRAM36':>8}")
    print("-" * 55)
    results = {}
    for fname, top, dut in duts:
        vpath = OUT_DIR / f"{fname}.v"
        _emit_verilog(dut, top, vpath)
        stat, err = _synth(vpath, top)
        if err:
            print(f"{fname:<14} synth failed:\n{err}")
            continue
        r = _parse(stat)
        results[fname] = r
        print(f"{fname:<14}{r['LUT']:>8}{r['FF']:>8}{r['CARRY4']:>8}"
              f"{r['DSP48E1']:>9}{r['BRAM36']:>8}")

    print("-" * 55)
    print(f"{'Z-7010 budget':<14}{Z7010['LUT']:>8}{Z7010['FF']:>8}{'':>8}"
          f"{Z7010['DSP48E1']:>9}{Z7010['BRAM36']:>8}")

    if "ddc" in results:
        d = results["ddc"]
        print("\nPer full DDC (NCO mixer + 3-stage FIR decimator):")
        print(f"  DSP48E1 {d['DSP48E1']}  (Cmult3x=1 + FIR4DSP/2DSP/4DSP=10)")
        print(f"  BRAM36  {d['BRAM36']}   LUT {d['LUT']}   FF {d['FF']}")
        n = 22
        print(f"\nNaive {n}x parallel full DDCs would need "
              f"~{n * d['DSP48E1']} DSP48E1 and ~{n * d['BRAM36']} BRAM "
              f"-> exceeds 80 DSP / 60 BRAM by far. Time-multiplexing required.")
    if "am_backend" in results:
        b = results["am_backend"]
        print(f"\nAM back-end per channel: DSP48E1 {b['DSP48E1']} "
              f"(multiplier-free), LUT {b['LUT']}, FF {b['FF']}, "
              f"BRAM {b['BRAM36']}.")
    if "lane_ch8" in results and "lane_ch21" in results:
        l8, l21 = results["lane_ch8"], results["lane_ch21"]
        dlut = (l21["LUT"] - l8["LUT"]) / (21 - 8)
        dff = (l21["FF"] - l8["FF"]) / (21 - 8)
        print("\nTime-multiplexed channelizer lane (NCO ROM + complex mixer + "
              "per-channel CIC):")
        print(f"  @ 8 ch : DSP48E1 {l8['DSP48E1']}  LUT {l8['LUT']}  "
              f"FF {l8['FF']}  BRAM {l8['BRAM36']}")
        print(f"  @21 ch : DSP48E1 {l21['DSP48E1']}  LUT {l21['LUT']}  "
              f"FF {l21['FF']}  BRAM {l21['BRAM36']}")
        print(f"  => mixer DSP is fixed at {l8['DSP48E1']} (4 real mults; the "
              f"real Maia Cmult3x trims this to ~1 via a 3x clock).")
        print(f"  => per-channel state adds ~{dlut:.0f} LUT / ~{dff:.0f} FF per "
              f"channel (NCO phase + CIC I/Q integrators+combs); maps to BRAM in "
              f"the real design rather than the Yosys register-file estimate.")

    if "frontend" in results and "compfir" in results:
        fe, cf = results["frontend"], results["compfir"]
        f_clk = 62.5e6                  # PL sync clock (handoff §4.2)
        fs_work = 14e6                  # working rate after front-end (§8.2 window)
        n_ch = 21
        # Folded MAC rate = taps * output_rate; each DSP does f_clk MACs/s.
        fe_out = fs_work
        fe_dsp = 2 * len(FE_COEFFS) * fe_out / f_clk             # x2 = complex
        # The channel-defining (selectivity + droop) FIR runs near the channel rate
        # (~2x the 25 kHz channel spacing), NOT the demo's R_ch=8 CIC rate which was
        # kept high only for fast simulation. At the real rate it is nearly free.
        chan_rate = 50e3
        comp_dsp_per_ch = 2 * COMP_NTAPS * chan_rate / f_clk     # x2 = complex
        print("\nChannelizer filtering blocks (UNROLLED Yosys upper bound; the real "
              "blocks fold taps onto a MAC engine):")
        print(f"  front-end decimator (shared, {len(FE_COEFFS)}-tap complex): "
              f"DSP48E1 {fe['DSP48E1']}  LUT {fe['LUT']}  FF {fe['FF']}")
        print(f"  comp FIR ({COMP_NTAPS}-tap, fully unrolled): "
              f"DSP48E1 {cf['DSP48E1']}  LUT {cf['LUT']}  FF {cf['FF']}")
        print("  --- folded (time-multiplexed MAC) cost @ 62.5 MHz PL clock ---")
        print(f"  front-end (single long FIR @ {fe_out/1e6:.0f} MHz): ~{fe_dsp:.0f} "
              f"DSP48E1 -- too costly; use a multistage HBF/CIC+FIR decimator "
              f"(brings it to a handful). Cost is paid ONCE (shared).")
        print(f"  channel FIR @ ~{chan_rate/1e3:.0f} kHz: ~{comp_dsp_per_ch:.2f} "
              f"DSP48E1/channel -> ~{comp_dsp_per_ch*n_ch:.1f} DSP for all {n_ch} "
              f"channels on one folded MAC engine.")


if __name__ == "__main__":
    main()
