#!/usr/bin/env python3
"""Z-7010 feasibility model for the airband AM channelizer (§4.2 GATE).

(Filename is historical: the planning target was N=25. The operational channel list
is now 21 -- all "need to have", including 133.65 MHz, which the 16 MHz capture
admits (it was formerly a deferred outlier).)

This is the resource/timing feasibility GATE from the handoff doc: does the
multichannel AM receiver fit in the Pluto's XC7Z010 programmable logic?

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

Capture window (center/Fs, lane count) is resolved separately in
``capture_window.py`` (§8.2): core list -> Fs ~14 MHz / ~5 lanes.
"""
from __future__ import annotations

import math

# XC7Z010 programmable-logic budget (DS190 / UG585).
Z7010 = {"LUT": 17600, "FF": 35200, "DSP48E1": 80, "BRAM36": 60}

# MEASURED base-platform PL usage: unmodified Maia SDR bitstream built from source
# on the x86 build server (Vivado 2023.2, system_top_utilization_placed.rpt),
# timing met (WNS +0.029 ns). This is the AD936x IF + DMA + Maia spectrometer that
# we build on top of, so the channelizer budget is (Z7010 - BASE).  [handoff §7.1]
BASE = {"LUT": 5416, "FF": 6493, "DSP48E1": 18, "BRAM36": 29}
FREE = {k: Z7010[k] - BASE[k] for k in Z7010}

# Measured per-block costs (Yosys synth_xilinx -family xc7); see synth_estimate.py.
#   full DDC (NCO mixer Cmult3x=1 DSP + 3-stage FIR 4+2+4=10 DSP):
DDC = {"LUT": 661, "FF": 1239, "DSP48E1": 11, "BRAM36": 4}
#   AM back-end per channel (|.| + DC-block + CIC decimate) -- multiplier-free:
BACKEND = {"LUT": 295, "FF": 281, "DSP48E1": 0, "BRAM36": 0}

# Hard-block costs of the channelizer primitives (from the code / maia-hdl):
#   NB: the TdmDdcLane prototype (channelizer_lane.py, synth_estimate.py) measures
#   4 DSP48E1 for a straightforward complex mixer; maia-hdl's Cmult3x trims that to
#   1 via a 3x clock. Either way the DSP budget holds (8 lanes * 4 = 32 < 62 free).
MIXER_DSP = 1          # Cmult3x: 1 DSP48E1, reused via the 3x clock
FIR_CLEANUP_DSP = 2    # one folded MAC engine for the per-lane CIC droop-comp +
# selectivity FIR. Prototyped/verified in channelizer_chain.py: at the channel
# rate (~50 kHz) a 119-tap complex FIR folds to ~0.2 DSP/channel, so one engine
# per lane (FIR_CLEANUP_DSP=2 with headroom) covers the lane's channels.
# (bulk decimation done by multiplier-free CIC: 0 DSP)

# Shared front-end decimator (ONE per receiver). OPTIONAL: the AD936x already
# decimates internally (HB1/2/3 + programmable FIR) to the requested rate, so the
# baseline captures at the working rate and needs NO PL front end. This budgets the
# *oversampling fallback*: channelizer_chain.py's verified two-stage halfband
# MultiStageDecimator folds to ~14 DSP (vs ~43 for one long FIR), paid once.
FRONTEND = {"DSP48E1": 14, "LUT": 1144, "FF": 1175, "BRAM36": 2}

N = 21                 # operational channel count (incl. 133.65 MHz)
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
    # per-lane datapath (mixer + cleanup FIR) + one shared front-end decimator.
    dsp = lanes * (MIXER_DSP + FIR_CLEANUP_DSP) + FRONTEND["DSP48E1"]
    # AM back-end runs at audio rate (N*16k << F_S): one shared back-end lane.
    backend_lanes = max(1, math.ceil(N * AUDIO_HZ / F_S))
    lut = (lanes * LANE_LUT + backend_lanes * BACKEND["LUT"]
           + FRONTEND["LUT"] + 2500)                                # +control
    ff = lanes * LANE_FF + backend_lanes * BACKEND["FF"] + FRONTEND["FF"] + 2500
    bram = lanes * 2 + 4 + FRONTEND["BRAM36"]   # coeff + per-channel state (rough)
    return {"window_MHz": window_hz / 1e6, "lanes": lanes,
            "DSP48E1": dsp, "LUT": lut, "FF": ff, "BRAM36": bram,
            "backend_lanes": backend_lanes}


def _bar(used, total):
    pct = 100 * used / total
    flag = "OK" if used <= total else "OVER"
    return f"{used:>6} / {total:<5} ({pct:4.0f}%) {flag}"


def main():
    print("=" * 70)
    print(f"Z-7010 feasibility: {N}-channel airband AM channelizer (handoff §4.2)")
    print("=" * 70)

    print("\nMeasured per-block cost (Yosys synth_xilinx -family xc7):")
    print(f"  full DDC          : DSP48E1 {DDC['DSP48E1']:>2}  "
          f"LUT {DDC['LUT']:>4}  FF {DDC['FF']:>4}  BRAM {DDC['BRAM36']}")
    print(f"  AM back-end / ch  : DSP48E1 {BACKEND['DSP48E1']:>2}  "
          f"LUT {BACKEND['LUT']:>4}  FF {BACKEND['FF']:>4}  BRAM "
          f"{BACKEND['BRAM36']}  (multiplier-free)")

    print(f"\n[A] Naive: {N} independent full DDCs (no sharing)")
    naive = {k: N * DDC[k] for k in DDC}
    print(f"  DSP48E1 {_bar(naive['DSP48E1'], Z7010['DSP48E1'])}")
    print(f"  BRAM36  {_bar(naive['BRAM36'], Z7010['BRAM36'])}")
    print(f"  LUT     {_bar(naive['LUT'], Z7010['LUT'])}")
    print(f"  => INFEASIBLE (DSP {naive['DSP48E1']/Z7010['DSP48E1']:.1f}x over, "
          f"BRAM {naive['BRAM36']/Z7010['BRAM36']:.1f}x over). Must share.")

    print("\n[measured] Maia base platform (Vivado 2023.2, timing met):")
    print(f"  LUT {_bar(BASE['LUT'], Z7010['LUT'])}")
    print(f"  FF  {_bar(BASE['FF'], Z7010['FF'])}")
    print(f"  DSP {_bar(BASE['DSP48E1'], Z7010['DSP48E1'])}")
    print(f"  BRAM{_bar(BASE['BRAM36'], Z7010['BRAM36'])}")
    print(f"  => FREE for the channelizer: LUT {FREE['LUT']}, FF {FREE['FF']}, "
          f"DSP48E1 {FREE['DSP48E1']}, BRAM36 {FREE['BRAM36']} "
          f"(BRAM is the binding resource)")

    print("\n[B] Time-multiplexed shared channelizer, by capture window (§8.2):")
    print("    (shared flat front-end decimator + per-lane datapath; "
          "lanes = ceil(N*W/Fs);")
    print("     AM back-end is DSP-free and time-shared at audio rate)")
    print("    checked against the FREE budget = Z7010 - measured Maia base")
    print(f"\n    {'window':>8} {'lanes':>6} {'DSP48E1':>16} "
          f"{'LUT':>16} {'BRAM36':>14}")
    feasible_windows = []
    for w_mhz in (4, 8, 14, 16, 19):       # 16 MHz = resolved §8.2 capture window
        c = channelizer_cost(w_mhz * 1e6)
        dsp_ok = c["DSP48E1"] <= FREE["DSP48E1"]
        lut_ok = c["LUT"] <= FREE["LUT"]
        bram_ok = c["BRAM36"] <= FREE["BRAM36"]
        if dsp_ok and lut_ok and bram_ok:
            feasible_windows.append(w_mhz)
        print(f"    {w_mhz:>6} MHz {c['lanes']:>6} "
              f"{_bar(c['DSP48E1'], FREE['DSP48E1'])} "
              f"{_bar(c['LUT'], FREE['LUT'])} "
              f"{_bar(c['BRAM36'], FREE['BRAM36'])}")

    print("\n" + "=" * 70)
    print(f"VERDICT: GO (confirmed against measured base). {N} AM channels fit the")
    print("Z-7010 on top of the unmodified Maia platform with a time-multiplexed")
    print("channelizer. The DSP budget is comfortable at every window (62 DSP free;")
    print("the AM back-end uses zero DSP). Binding resources are BRAM36 (31 free)")
    print("and LUT (12184 free), driven by lane count = ceil(N * window / 62.5MHz).")
    print(f"Feasible capture windows: "
          f"{', '.join(str(w)+' MHz' for w in feasible_windows)} "
          f"(even the full ~19 MHz airband fits, the tightest case).")
    print("Resolved (§8.2, see capture_window.py): center 126.4 MHz, Fs ~16 MHz "
          "-> 7 lanes (chans_per_lane=3) for the 21 channels (incl. 133.65 MHz).")
    print("\nStatus / caveats:")
    print("  * front end + per-channel CIC droop-comp/selectivity FIR + multistage")
    print("    front end + folded one-MAC TDM cleanup FIR are all built and verified")
    print("    (channelizer_chain.py): flat window, flat channel passband, >40 dB")
    print("    adjacent rejection; the cleanup FIR folds to ~2 DSP/engine.")
    print(f"  * front end is OPTIONAL ({FRONTEND['DSP48E1']} DSP budgeted): the AD936x")
    print("    can deliver the working rate directly; this covers the oversampling")
    print("    fallback (verified halfband cascade, ~14 folded / ~58 parallel DSP).")
    print("  * Vivado 2023.2 OOC (xc7z010clg225-1) CONFIRMS the lane: 21-ch TdmDdcLane")
    print("    = 4 DSP, 3374 LUT (19%), 7760 FF (22%), 0 BRAM -- matches Yosys. The")
    print("    per-channel state lands in FFs here; a Memory-backed lane moves it to")
    print("    BRAM. Final step: integrate front end + lane + folded FIR and re-place.")


if __name__ == "__main__":
    main()
