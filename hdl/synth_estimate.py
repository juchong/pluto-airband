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
        ("lane_ch25", "TdmDdcLane",
         TdmDdcLane(n_channels=25, decimation=16, stages=3)),
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
        n = 25
        print(f"\nNaive {n}x parallel full DDCs would need "
              f"~{n * d['DSP48E1']} DSP48E1 and ~{n * d['BRAM36']} BRAM "
              f"-> exceeds 80 DSP / 60 BRAM by far. Time-multiplexing required.")
    if "am_backend" in results:
        b = results["am_backend"]
        print(f"\nAM back-end per channel: DSP48E1 {b['DSP48E1']} "
              f"(multiplier-free), LUT {b['LUT']}, FF {b['FF']}, "
              f"BRAM {b['BRAM36']}.")
    if "lane_ch8" in results and "lane_ch25" in results:
        l8, l25 = results["lane_ch8"], results["lane_ch25"]
        dlut = (l25["LUT"] - l8["LUT"]) / (25 - 8)
        dff = (l25["FF"] - l8["FF"]) / (25 - 8)
        print("\nTime-multiplexed channelizer lane (NCO ROM + complex mixer + "
              "per-channel CIC):")
        print(f"  @ 8 ch : DSP48E1 {l8['DSP48E1']}  LUT {l8['LUT']}  "
              f"FF {l8['FF']}  BRAM {l8['BRAM36']}")
        print(f"  @25 ch : DSP48E1 {l25['DSP48E1']}  LUT {l25['LUT']}  "
              f"FF {l25['FF']}  BRAM {l25['BRAM36']}")
        print(f"  => mixer DSP is fixed at {l8['DSP48E1']} (4 real mults; the "
              f"real Maia Cmult3x trims this to ~1 via a 3x clock).")
        print(f"  => per-channel state adds ~{dlut:.0f} LUT / ~{dff:.0f} FF per "
              f"channel (NCO phase + CIC I/Q integrators+combs); maps to BRAM in "
              f"the real design rather than the Yosys register-file estimate.")


if __name__ == "__main__":
    main()
