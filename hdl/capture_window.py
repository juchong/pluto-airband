#!/usr/bin/env python3
"""Resolve the capture window (handoff open decision §8.2) from the channel list.

§2.5 requires a *single contiguous* capture in which **every** target channel sits
comfortably inside the window, clear of the band-edge roll-off — the contiguous
coverage the RSPduo could not provide. This script turns the operator's channel
list into a concrete recommendation:

  * capture center (LO) frequency, chosen so the **DC / LO-leakage spur** lands in
    a guard gap between channels (a zero-IF receiver puts an LO self-mix spur at
    baseband 0 Hz = the center RF; per-channel DDC tuning then pushes that spur to
    offset (f_c - f_ch) in each channel, which the channel low-pass rejects as long
    as no channel sits within ~a channel bandwidth of the center),
  * capture width / complex sample rate, sized so the extreme channels stay within
    a usable fraction of the half-band (away from the anti-alias + digital filter
    skirts),
  * the resulting time-multiplexed lane count (feasibility_25ch model), and
  * whether the far-out "nice to have" channel could be included and at what cost.

Run:  python hdl/capture_window.py
"""
from __future__ import annotations

import math
import pathlib

OUT_DIR = pathlib.Path(__file__).parent / "out"

# Target channels (MHz) -- the LIVE operational plan (firmware/airband.json) plus
# 133.65, now admitted by widening the capture to 16 MHz. 18 channels total
# (capped at 18 ch / 6 lanes: 21 ch -> 7 lanes overflows the XC7Z010 LUTs).
# (118.050 S50 AWOS is the firmware no-SD fallback only and is NOT operational.)
CORE_CHANNELS_MHZ = sorted([
    119.2, 119.9, 120.1, 120.4, 120.95, 121.5, 121.6, 121.7, 122.275,
    122.975, 123.9, 124.7, 125.6, 125.9, 126.25, 126.5, 126.875,
    133.65,
])
# 133.65 used to be a deferred outlier; it is now included (see DEPLOY_* below).
OUTLIER_MHZ = []

N_CORE = len(CORE_CHANNELS_MHZ)   # 18 operational channels (incl. 133.65)
F_PL = 62.5e6                 # PL "sync" clock (handoff §4.2)

# A channel must stay within this fraction of the half-band (|offset| < frac*W/2)
# to avoid the anti-alias / decimation-filter roll-off near the capture edges.
# 0.80 is the comfortable design target; the deployment relaxes to ~0.91 for the
# single 133.65 outlier (see DEPLOY_* and the recommendation below).
USABLE_FRACTION = 0.80
# AM voice channel bandwidth (Hz) — the per-channel low-pass; the DC spur must be
# at least this far from any channel after per-channel tuning.
CHANNEL_BW_HZ = 25e3
# AD936x complex sample-rate candidates (MHz) we'd actually request.
FS_CANDIDATES_MHZ = [12.0, 12.288, 14.0, 15.36, 16.0, 20.0]

# Deployed capture window (see plan "Widen capture to 16 MHz"). 16 MHz admits
# 133.65 at ~91% of the half-band (mild AD9361 edge droop, AGC-compensated), keeps
# the channelizer at 6 lanes (cpl=3), and lands the internal sample-clock spur comb
# at the 8th harmonic = 128.000 MHz, clear of every channel.
DEPLOY_CENTER_MHZ = 126.4
DEPLOY_FS_MHZ = 16.0


def gaps(channels_mhz):
    """Consecutive (lower, upper, width) gaps between sorted channels (MHz)."""
    ch = sorted(channels_mhz)
    return [(ch[i], ch[i + 1], ch[i + 1] - ch[i]) for i in range(len(ch) - 1)]


def choose_center(channels_mhz):
    """Center the capture so the DC spur sits in a guard gap near the cluster mid.

    Start at the span midpoint, then snap to the midpoint of whichever inter-channel
    gap contains it (or the nearest gap), maximizing the DC-to-nearest-channel
    clearance.
    """
    ch = sorted(channels_mhz)
    mid = 0.5 * (ch[0] + ch[-1])
    g = gaps(ch)
    containing = [(lo, hi, w) for (lo, hi, w) in g if lo <= mid <= hi]
    if containing:
        lo, hi, _ = max(containing, key=lambda t: t[2])
    else:                                   # mid coincides with a channel; pick the
        lo, hi, _ = max(g, key=lambda t: t[2])   # widest gap as a fallback
    return 0.5 * (lo + hi)


def required_width_mhz(channels_mhz, center_mhz, usable=USABLE_FRACTION):
    max_off = max(abs(f - center_mhz) for f in channels_mhz)
    return 2.0 * max_off / usable


def lanes_for(width_mhz, n=N_CORE):
    return max(1, math.ceil(n * (width_mhz * 1e6) / F_PL))


def pick_fs(min_width_mhz):
    for fs in FS_CANDIDATES_MHZ:
        if fs >= min_width_mhz:
            return fs
    return math.ceil(min_width_mhz)


def _report(title, channels):
    ch = sorted(channels)
    center = choose_center(ch)
    min_w = required_width_mhz(ch, center)
    fs = pick_fs(min_w)
    half = fs / 2.0
    max_off = max(abs(f - center) for f in ch)
    edge_margin = half - max_off
    dc_clear_khz = min(abs(f - center) for f in ch) * 1e3
    lanes = lanes_for(fs, len(ch))

    print(f"\n=== {title} ===")
    print(f"  channels            : {len(ch)}  ({ch[0]:.3f} .. {ch[-1]:.3f} MHz, "
          f"span {ch[-1]-ch[0]:.3f} MHz)")
    print(f"  recommended center  : {center:.4f} MHz  (DC spur in a guard gap)")
    print(f"  DC-to-nearest chan  : {dc_clear_khz:.0f} kHz "
          f"(need > {CHANNEL_BW_HZ/1e3:.0f} kHz: "
          f"{'OK' if dc_clear_khz > CHANNEL_BW_HZ/1e3 else 'TOO CLOSE'})")
    print(f"  min width @ {USABLE_FRACTION:.0%} usable: {min_w:.3f} MHz")
    print(f"  -> request Fs       : {fs:.3f} MHz  (half-band +/-{half:.3f} MHz)")
    print(f"  extreme channel off : +/-{max_off:.3f} MHz "
          f"({100*max_off/half:.0f}% of half-band)")
    print(f"  edge guard          : {edge_margin:.3f} MHz to each band edge")
    print(f"  time-mux lanes (N={len(ch)}): {lanes}  (of <=8 that the Z-7010 fits)")
    return center, fs, lanes


def main():
    print("=" * 70)
    print("Capture window resolver (handoff §8.2)")
    print("=" * 70)

    g = gaps(CORE_CHANNELS_MHZ)
    biggest = sorted(g, key=lambda t: t[2], reverse=True)[:3]
    print("\nWidest inter-channel guard gaps (candidate DC-spur homes):")
    for lo, hi, w in biggest:
        print(f"  {lo:.3f} .. {hi:.3f} MHz   (gap {w*1e3:.0f} kHz, "
              f"mid {0.5*(lo+hi):.4f} MHz)")

    # The strict 80%-usable rule (for reference) on the full 18-channel list:
    center, fs, lanes = _report(
        f"Strict {USABLE_FRACTION:.0%}-usable auto-pick (reference)",
        CORE_CHANNELS_MHZ)

    # The DEPLOYED decision: a fixed 16 MHz at center 126.4, relaxing the usable
    # fraction for the single 133.65 outlier.
    ch = sorted(CORE_CHANNELS_MHZ)
    half = DEPLOY_FS_MHZ / 2.0
    max_off = max(abs(f - DEPLOY_CENTER_MHZ) for f in ch)
    dc_clear_khz = min(abs(f - DEPLOY_CENTER_MHZ) for f in ch) * 1e3
    dep_lanes = lanes_for(DEPLOY_FS_MHZ, len(ch))
    print(f"\n=== DEPLOYED window (16 MHz, center 126.4) ===")
    print(f"  channels            : {len(ch)}  ({ch[0]:.3f} .. {ch[-1]:.3f} MHz)")
    print(f"  center (LO)         : {DEPLOY_CENTER_MHZ:.3f} MHz")
    print(f"  Fs                  : {DEPLOY_FS_MHZ:.3f} MHz  (half-band +/-{half:.3f} MHz)")
    print(f"  extreme channel off : +/-{max_off:.3f} MHz "
          f"({100*max_off/half:.1f}% of half-band)")
    print(f"  edge guard          : {half-max_off:.3f} MHz to each band edge")
    print(f"  DC-to-nearest chan  : {dc_clear_khz:.0f} kHz "
          f"(> {CHANNEL_BW_HZ/1e3:.0f} kHz channel BW: "
          f"{'OK' if dc_clear_khz > CHANNEL_BW_HZ/1e3 else 'TOO CLOSE'})")
    print(f"  time-mux lanes      : {dep_lanes}  (cpl=3 -> 6 lanes; 7 lanes (21 ch) "
          f"overflow the Z-7010 LUTs)")

    print("\n" + "=" * 70)
    print("RECOMMENDATION (deployed):")
    print(f"  center (LO) = {DEPLOY_CENTER_MHZ:.3f} MHz, Fs = {DEPLOY_FS_MHZ:.3f} MHz.")
    print(f"  All {len(ch)} channels (incl. 133.65) fit; both extremes sit at "
          f"~{100*max_off/half:.0f}% of the half-band. The strict 80%-usable rule "
          f"would request {fs:.0f} MHz, but 16 MHz keeps the channelizer at 6 lanes,")
    print("  lands the sample-clock spur comb at 128.000 MHz (clear of channels), and")
    print("  gives a round 20 kHz audio rate; 133.65 sees mild edge droop (AGC-comp).")

    _save_plot(DEPLOY_CENTER_MHZ, DEPLOY_FS_MHZ)


def _save_plot(center, fs):
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        print("\n(matplotlib not installed; skipping capture_window.png)")
        return

    OUT_DIR.mkdir(exist_ok=True)
    half = fs / 2.0
    fig, ax = plt.subplots(figsize=(11, 3.2))
    # capture band + usable region
    ax.axvspan(center - half, center + half, color="tab:blue", alpha=0.10,
               label=f"capture {fs:.2f} MHz")
    ax.axvspan(center - USABLE_FRACTION * half, center + USABLE_FRACTION * half,
               color="tab:green", alpha=0.10,
               label=f"usable {USABLE_FRACTION:.0%}")
    ax.axvline(center, color="k", ls="--", lw=1, label=f"center/DC {center:.2f}")
    for f in CORE_CHANNELS_MHZ:
        ax.axvline(f, color="tab:blue", lw=1.4)
    for f in OUTLIER_MHZ:
        ax.axvline(f, color="tab:red", lw=1.4, ls=":")
        ax.text(f, 0.9, " 133.65\n (deferred)", color="tab:red", fontsize=8,
                va="top")
    ax.set_xlim(117, 135)
    ax.set_yticks([])
    ax.set_xlabel("frequency (MHz)")
    ax.set_title("Airband capture window vs target channels (handoff §8.2)")
    ax.legend(loc="upper right", fontsize=8, ncol=2)
    fig.tight_layout()
    path = OUT_DIR / "capture_window.png"
    fig.savefig(path, dpi=120)
    print(f"\nSaved plot: {path}")


if __name__ == "__main__":
    main()
