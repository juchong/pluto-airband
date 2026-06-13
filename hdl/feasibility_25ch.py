#!/usr/bin/env python3
"""Z-7010 feasibility model for a 25-channel airband AM channelizer (§4.2 GATE).

This is the resource/timing feasibility GATE from the handoff doc: does a
25-channel AM receiver fit in the Pluto's XC7Z010 programmable logic?

It combines:
  * the Z-7010 PL budget (DS190 / UG585),
  * **measured** per-block resource costs from ``synth_estimate.py`` (Yosys
    ``synth_xilinx -family xc7`` on the real maia-hdl ``DDC`` and our AM
    back-end), and
  * a time-multiplexing throughput model,

to size a shared-datapath channelizer and compare against the budget for a few
capture-window choices (open decision §8.2).

DSP estimates are credible (DSP48E1 are explicit/inferred and map 1:1); LUT/FF
are Yosys estimates and must be confirmed with Vivado on the build server, on
top of the ADI/Maia base platform (which itself consumes PL — also to be
measured on the build server, step 1).

Run:  python hdl/feasibility_25ch.py
"""
from __future__ import annotations

import math

# XC7Z010 programmable-logic budget (DS190 / UG585).
Z7010 = {"LUT": 17600, "FF": 35200, "DSP48E1": 80, "BRAM36": 60}

# Measured per-block costs (Yosys synth_xilinx -family xc7); see synth_estimate.py.
#   full DDC (NCO mixer Cmult3x=1 DSP + 3-stage FIR 4+2+4=10 DSP):
DDC = {"LUT": 661, "FF": 1239, "DSP48E1": 11, "BRAM36": 4}
#   AM back-end per channel (|.| + DC-block + CIC decimate) -- multiplier-free:
BACKEND = {"LUT": 295, "FF": 281, "DSP48E1": 0, "BRAM36": 0}

# Hard-block costs of the channelizer primitives (from the code / maia-hdl):
MIXER_DSP = 1          # Cmult3x: 1 DSP48E1, reused via the 3x clock
FIR_CLEANUP_DSP = 2    # one FIR2DSP compensation/cleanup filter per lane
# (bulk decimation done by multiplier-free CIC: 0 DSP)

N = 25                 # channel-count target
F_S = 62.5e6           # PL "sync" clock (handoff §4.2)
AUDIO_HZ = 16_000      # 16 ksps (open decision §8.3; 8 ksps only relaxes this)

# Per-lane fabric cost (LUT/FF) for one time-multiplexed channelizer lane:
#   mixer (~200) + per-channel CIC bank (~250) + cleanup FIR (~300) + ctrl
LANE_LUT, LANE_FF = 800, 900


def lanes_needed(window_hz: float) -> int:
    """Datapath lanes to sustain N channels at the intermediate rate.

    After a shared front-end decimates the capture window to ~its own width,
    every channel is processed at R_i ~= window_hz. One lane sustains F_S
    complex samples/s (Cmult3x: one complex product per sync cycle), so we need
    ceil(N * R_i / F_S) lanes.
    """
    return max(1, math.ceil(N * window_hz / F_S))


def channelizer_cost(window_hz: float) -> dict:
    lanes = lanes_needed(window_hz)
    dsp = lanes * (MIXER_DSP + FIR_CLEANUP_DSP)
    # AM back-end runs at audio rate (N*16k << F_S): one shared back-end lane.
    backend_lanes = max(1, math.ceil(N * AUDIO_HZ / F_S))
    lut = lanes * LANE_LUT + backend_lanes * BACKEND["LUT"] + 2500   # +control
    ff = lanes * LANE_FF + backend_lanes * BACKEND["FF"] + 2500
    bram = lanes * 2 + 4        # coeff + per-channel sample state (rough)
    return {"window_MHz": window_hz / 1e6, "lanes": lanes,
            "DSP48E1": dsp, "LUT": lut, "FF": ff, "BRAM36": bram,
            "backend_lanes": backend_lanes}


def _bar(used, total):
    pct = 100 * used / total
    flag = "OK" if used <= total else "OVER"
    return f"{used:>6} / {total:<5} ({pct:4.0f}%) {flag}"


def main():
    print("=" * 70)
    print("Z-7010 feasibility: 25-channel airband AM channelizer (handoff §4.2)")
    print("=" * 70)

    print("\nMeasured per-block cost (Yosys synth_xilinx -family xc7):")
    print(f"  full DDC          : DSP48E1 {DDC['DSP48E1']:>2}  "
          f"LUT {DDC['LUT']:>4}  FF {DDC['FF']:>4}  BRAM {DDC['BRAM36']}")
    print(f"  AM back-end / ch  : DSP48E1 {BACKEND['DSP48E1']:>2}  "
          f"LUT {BACKEND['LUT']:>4}  FF {BACKEND['FF']:>4}  BRAM "
          f"{BACKEND['BRAM36']}  (multiplier-free)")

    print("\n[A] Naive: 25 independent full DDCs (no sharing)")
    naive = {k: N * DDC[k] for k in DDC}
    print(f"  DSP48E1 {_bar(naive['DSP48E1'], Z7010['DSP48E1'])}")
    print(f"  BRAM36  {_bar(naive['BRAM36'], Z7010['BRAM36'])}")
    print(f"  LUT     {_bar(naive['LUT'], Z7010['LUT'])}")
    print("  => INFEASIBLE (DSP ~3.4x over, BRAM ~1.7x over). Must share.")

    print("\n[B] Time-multiplexed shared channelizer, by capture window (§8.2):")
    print("    (shared CIC front-end decimates the window; lanes = ceil(N*W/Fs);")
    print("     AM back-end is DSP-free and time-shared at audio rate)")
    print(f"\n    {'window':>8} {'lanes':>6} {'DSP48E1':>16} "
          f"{'LUT':>16} {'BRAM36':>14}")
    feasible_windows = []
    for w_mhz in (2, 4, 8, 19):
        c = channelizer_cost(w_mhz * 1e6)
        dsp_ok = c["DSP48E1"] <= Z7010["DSP48E1"]
        lut_ok = c["LUT"] <= Z7010["LUT"]
        bram_ok = c["BRAM36"] <= Z7010["BRAM36"]
        if dsp_ok and lut_ok and bram_ok:
            feasible_windows.append(w_mhz)
        print(f"    {w_mhz:>6} MHz {c['lanes']:>6} "
              f"{_bar(c['DSP48E1'], Z7010['DSP48E1'])} "
              f"{_bar(c['LUT'], Z7010['LUT'])} "
              f"{_bar(c['BRAM36'], Z7010['BRAM36'])}")

    print("\n" + "=" * 70)
    print("VERDICT: GO. 25 AM channels fit the Z-7010 with a time-multiplexed")
    print("channelizer. The DSP budget is comfortable at every window (the AM")
    print("back-end uses zero DSP); the binding resources are LUT/FF, driven by")
    print("the lane count = ceil(25 * window / 62.5MHz).")
    print(f"Feasible capture windows in this model: "
          f"{', '.join(str(w)+' MHz' for w in feasible_windows)} "
          f"(even the full ~19 MHz airband fits).")
    print("\nKey caveats to confirm on the x86 build server (step 1):")
    print("  * subtract the ADI/Maia base-platform PL usage (AD936x IF + DMA;")
    print("    mostly LUT/BRAM, few DSP) from the budget above;")
    print("  * resolve §8.2 capture window so the 25 channels share one window")
    print("    with edge margin (narrower window => fewer lanes => more slack);")
    print("  * LUT/FF here are Yosys estimates; re-confirm with Vivado.")


if __name__ == "__main__":
    main()
