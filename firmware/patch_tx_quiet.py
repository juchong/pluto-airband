#!/usr/bin/env python3
"""Idempotently quiet the AD9361 transmitter on boot for the receive-only build.

This is an *airband receiver*; nothing ever transmits. But the Pluto boots the
AD9361 in FDD with the TX LO running (powerdown=0) and only ~10 dB of TX
attenuation. That leaves a CW carrier at the TX LO (Pluto default 2.45 GHz, on
top of WiFi/BT) plus broadband TX-path noise leaking out the TX port -- audible
EMI to nearby radios, and it folds back into our own RX as a raised noise
floor / spurs.

There is no "rx-only" ENSM mode exposed by the Pluto AD9361 driver
(`ensm_mode_available` = "sleep wait alert fdd pinctrl pinctrl_fdd_indep"), and
RX needs FDD, so we cannot simply switch the state machine off. Instead, in FDD
the RX and TX LOs are independent synthesizers, so we:

  1. power down the TX LO synth  (out_altvoltage1_TX_LO_powerdown = 1), and
  2. set TX attenuation to the floor (out_voltage0_hardwaregain = -89.75 dB),

which kills the TX carrier and ~80 dB of any residual leakage while leaving RX
byte-for-byte unchanged (verified on hardware: waterfall + per-channel meters
unaffected). The rootfs is ramfs, so a live `iio` write is lost on power cycle;
baking it into the init script is the only power-cycle-persistent path.

This patcher injects a backgrounded, retry-until-ready block into the `start)`
case of the maia-httpd init script (buildroot/board/pluto/S60maia-httpd), right
after the daemon is launched. It is whitespace tolerant and a no-op if the block
is already present (guarded by the `airband-tx-quiet` marker).

Usage:
    patch_tx_quiet.py <path-to-S60maia-httpd> [more ...]
"""
from __future__ import annotations

import sys
import pathlib

MARKER = "airband-tx-quiet"

# Anchor: the last line of the daemon-launch sequence in the start) case.
ANCHOR = "cd - > /dev/null"

# 8-space indentation matches the existing body of the start) case.
IND = " " * 8

TX_QUIET_BLOCK = f"""{IND}# {MARKER}: receive-only build -> silence the AD9361 TX so its LO carrier
{IND}# (Pluto default 2.45 GHz) and TX-path noise don't radiate as EMI to nearby
{IND}# radios or fold back into our own RX. FDD keeps RX/TX LOs independent, so we
{IND}# power down the TX LO and floor the TX attenuation; RX is unaffected.
{IND}# Backgrounded (boot isn't delayed) and retried until ad9361-phy appears.
{IND}(
{IND}  i=0
{IND}  while [ $i -lt 15 ]; do
{IND}    for d in /sys/bus/iio/devices/iio:device*; do
{IND}      [ "$(cat "$d/name" 2>/dev/null)" = "ad9361-phy" ] || continue
{IND}      echo 1 > "$d/out_altvoltage1_TX_LO_powerdown" 2>/dev/null
{IND}      echo -89.75 > "$d/out_voltage0_hardwaregain" 2>/dev/null
{IND}      exit 0
{IND}    done
{IND}    i=$((i+1))
{IND}    sleep 1
{IND}  done
{IND}) &
"""


def patch(path: pathlib.Path) -> bool:
    text = path.read_text()
    if MARKER in text:
        print(f"{path}: already quiets TX on boot, skipping")
        return False

    lines = text.splitlines(keepends=True)
    out = []
    inserted = False
    for line in lines:
        out.append(line)
        if not inserted and ANCHOR in line:
            # Preserve a trailing newline on the anchor line before inserting.
            if not line.endswith("\n"):
                out[-1] = line + "\n"
            out.append(TX_QUIET_BLOCK)
            inserted = True

    if not inserted:
        raise SystemExit(
            f"{path}: anchor {ANCHOR!r} not found; cannot place TX-quiet block "
            f"(has the init script changed?)")

    path.write_text("".join(out))
    print(f"{path}: injected TX LO powerdown + max-attenuation on boot")
    return True


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        raise SystemExit(f"usage: {argv[0]} <path-to-S60maia-httpd> [more ...]")
    for p in argv[1:]:
        patch(pathlib.Path(p))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
