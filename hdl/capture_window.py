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

# Target channels (MHz). 3 of the final 25 are still pending (2026-06-13); the
# margins below are sized to absorb a few more in-cluster additions.
CORE_CHANNELS_MHZ = sorted([
    118.050, 119.2, 119.9, 120.1, 120.4, 120.95, 121.5, 121.6, 121.7,
    122.275, 122.95, 122.975, 123.9, 124.7, 125.6, 125.9, 126.25, 126.5,
    126.875, 127.1, 128.5,
])
# Far outlier — "nice to have", not "need to have". Excluded from the core window;
# supporting it is a separate, later decision (§8.2 note).
OUTLIER_MHZ = [133.65]

N_TARGET = 25                 # full channel-count target (handoff §8.1, resolved)
F_PL = 62.5e6                 # PL "sync" clock (handoff §4.2)

# A channel must stay within this fraction of the half-band (|offset| < frac*W/2)
# to avoid the anti-alias / decimation-filter roll-off near the capture edges.
USABLE_FRACTION = 0.80
# AM voice channel bandwidth (Hz) — the per-channel low-pass; the DC spur must be
# at least this far from any channel after per-channel tuning.
CHANNEL_BW_HZ = 25e3
# AD936x complex sample-rate candidates (MHz) we'd actually request.
FS_CANDIDATES_MHZ = [12.0, 12.288, 14.0, 15.36, 16.0, 20.0]


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


def lanes_for(width_mhz, n=N_TARGET):
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
    lanes = lanes_for(fs)

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
    print(f"  time-mux lanes (N=25): {lanes}  (of <=8 that the Z-7010 fits)")
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

    center, fs, lanes = _report("CORE list (recommended)", CORE_CHANNELS_MHZ)
    _report("CORE + outlier 133.65 (nice-to-have, for comparison)",
            CORE_CHANNELS_MHZ + OUTLIER_MHZ)

    print("\n" + "=" * 70)
    print("RECOMMENDATION (§8.2):")
    print(f"  center (LO) = {center:.3f} MHz, Fs ~= {fs:.3f} MHz capture.")
    print(f"  All {len(CORE_CHANNELS_MHZ)} core channels fall in the central "
          f"{100*USABLE_FRACTION:.0f}% of the band with healthy edge guard; the DC "
          f"spur sits in a channel gap. Costs {lanes} time-mux lanes (<= the 8 the "
          f"Z-7010 fits).")
    print("  The 133.65 MHz channel is excluded (it would push Fs to ~20 MHz and")
    print("  shift the center up, spending lanes/edge-margin on one nice-to-have).")
    print("  3 of the final 25 channels are still pending; the edge guard above")
    print("  absorbs further in-cluster (118-128.5 MHz) additions without a change.")

    _save_plot(center, fs)


def _save_plot(center, fs):
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

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
