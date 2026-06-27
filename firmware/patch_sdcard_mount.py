#!/usr/bin/env python3
"""Idempotently mount the SD-card config before maia-httpd starts.

Why this exists
---------------
The airband channel plan + front-end gain now live on the Pluto's SD card
(``/mnt/sdcard/airband.json``), loaded by maia-httpd via
``--airband-config /mnt/sdcard/airband.json`` and read/written by the web config
UI. The rootfs is ramfs (volatile), so the mount must be re-established on every
boot, *before* maia-httpd launches and reads the config.

This injects a mount block into the ``start)`` case of the maia-httpd init
script (``buildroot/board/$TARGET/S60maia-httpd``), right after the ``cd /root``
that precedes the daemon launch. It:

  * skips if ``/mnt/sdcard`` is already mounted (so a web-triggered
    ``S60maia-httpd restart`` is a no-op),
  * waits briefly for ``/dev/mmcblk0`` to enumerate (card detect can lag),
  * tries FAT32 then ext4, partitioned (``mmcblk0p1``) then whole-device
    (``mmcblk0``).

If nothing mounts (no card / unformatted / exFAT), maia-httpd falls back to its
built-in default (a single 118.050 AWOS channel at 0 dB) -- the obvious "the SD
plan did not load" signal.

It is whitespace tolerant and a no-op if the block is already present (guarded
by the ``airband-sdcard`` marker). Software-only init-script change ->
``firmware.dfu``-only reflash.

Usage:
    patch_sdcard_mount.py <path-to-S60maia-httpd> [more ...]
"""
from __future__ import annotations

import sys
import pathlib

MARKER = "airband-sdcard"

# Anchor: the `cd /root` immediately before the maia-httpd daemon launch in the
# start) case. The mount runs after it (cwd is irrelevant to mount).
ANCHOR = "cd /root"

# 8-space indentation matches the existing body of the start) case.
IND = " " * 8

MOUNT_BLOCK = f"""{IND}# {MARKER}: mount the SD card so maia-httpd reads its channel plan + gain
{IND}# from /mnt/sdcard/airband.json (--airband-config). Card must be FAT32 (the
{IND}# kernel has no exFAT); ext4 is also accepted. rootfs is ramfs, so this runs
{IND}# every boot, before the daemon launches. If nothing mounts, maia-httpd uses
{IND}# its built-in AWOS-only 0 dB default. Idempotent: skip if already mounted.
{IND}if ! grep -q " /mnt/sdcard " /proc/mounts 2>/dev/null; then
{IND}  mkdir -p /mnt/sdcard
{IND}  n=0
{IND}  while [ $n -lt 5 ]; do
{IND}    if [ -b /dev/mmcblk0p1 ] || [ -b /dev/mmcblk0 ]; then break; fi
{IND}    n=$((n+1)); sleep 1
{IND}  done
{IND}  mount -t vfat -o rw,umask=0000 /dev/mmcblk0p1 /mnt/sdcard 2>/dev/null \\
{IND}    || mount -t vfat -o rw,umask=0000 /dev/mmcblk0 /mnt/sdcard 2>/dev/null \\
{IND}    || mount -t ext4 /dev/mmcblk0p1 /mnt/sdcard 2>/dev/null \\
{IND}    || mount -t ext4 /dev/mmcblk0 /mnt/sdcard 2>/dev/null \\
{IND}    || echo "airband: no SD config mounted; using built-in defaults"
{IND}fi
"""


def patch(path: pathlib.Path) -> bool:
    text = path.read_text()
    if MARKER in text:
        print(f"{path}: already mounts the SD config on boot, skipping")
        return False

    lines = text.splitlines(keepends=True)
    out = []
    inserted = False
    for line in lines:
        out.append(line)
        if not inserted and line.strip() == ANCHOR:
            if not line.endswith("\n"):
                out[-1] = line + "\n"
            out.append(MOUNT_BLOCK)
            inserted = True

    if not inserted:
        raise SystemExit(
            f"{path}: anchor {ANCHOR!r} not found; cannot place SD-mount block "
            f"(has the init script changed?)")

    path.write_text("".join(out))
    print(f"{path}: injected SD-card mount on boot (/mnt/sdcard)")
    return True


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        raise SystemExit(f"usage: {argv[0]} <path-to-S60maia-httpd> [more ...]")
    for p in argv[1:]:
        patch(pathlib.Path(p))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
