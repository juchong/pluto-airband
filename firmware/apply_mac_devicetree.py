#!/usr/bin/env python3
"""Idempotently pin a fixed Ethernet MAC in the Pluto+ devicetree.

Why this exists
---------------
The Zynq ``macb`` (Cadence GEM) driver assigns a **random** MAC at every boot
when its devicetree node has no ``local-mac-address`` property. On the Pluto+
the node (`&gem0` = ``ethernet@e000b000``) ships without one, and u-boot's
automatic ``fdt_fixup_ethernet`` (which would copy ``${ethaddr}`` into the node)
does not help because the board's ``ethernet0`` alias resolves to a path the
fixup can't match. The result: a new random MAC -> a new DHCP lease -> a new IP
on every reboot.

Patching the **OS devicetree** (the FIT consumed from ``mtd3``) is the durable
fix: the kernel reads ``local-mac-address`` directly from the ``&gem0`` node at
probe time, so the MAC is deterministic from first boot with no u-boot env
dependency and no fragile in-place FDT growth (which corrupts the FIT and drops
the board into DFU).

This script inserts::

    local-mac-address = [02 0a 35 00 01 22];

into the ``&gem0 { ... }`` override block of ``zynq-plutoplus-maiasdr.dts``.
The address is locally administered (the ``0x02`` bit-1 of the first octet),
so it cannot collide with a globally assigned vendor MAC.

It is whitespace tolerant and a no-op if the node already has a
``local-mac-address``.

Usage:
    apply_mac_devicetree.py <path-to-zynq-plutoplus-maiasdr.dts> [MAC]

MAC defaults to 02:0a:35:00:01:22 and may be given as ``02:0a:35:00:01:22`` or
``02 0a 35 00 01 22``. Override per unit if you flash this image to more than
one board (a baked-in MAC is shared by every device flashed with the image).
"""
from __future__ import annotations

import re
import sys
import pathlib

DEFAULT_MAC = "02:0a:35:00:01:22"

# The ethernet node is overridden by label in the Pluto+ board .dts.
GEM_OPEN_RE = r"&gem0\s*\{"


def _mac_bytes(mac: str) -> str:
    """Normalise an input MAC to the dts byte-string body '02 0a 35 00 01 22'."""
    parts = re.split(r"[:\s]+", mac.strip())
    if len(parts) != 6 or any(not re.fullmatch(r"[0-9a-fA-F]{2}", p) for p in parts):
        raise SystemExit(f"invalid MAC: {mac!r} (want 6 hex octets)")
    return " ".join(p.lower() for p in parts)


def patch(path: pathlib.Path, mac: str) -> bool:
    text = path.read_text()
    m = re.search(GEM_OPEN_RE, text)
    if not m:
        raise SystemExit(f"{path}: anchor not found: {GEM_OPEN_RE!r} "
                         "(is this the Pluto+ board .dts?)")
    # Find the brace-balanced extent of the &gem0 block so we only inspect/insert
    # within it (the file has other nodes, e.g. pinctrl, mentioning ethernet).
    i = text.index("{", m.start())
    depth, j = 0, i
    while j < len(text):
        if text[j] == "{":
            depth += 1
        elif text[j] == "}":
            depth -= 1
            if depth == 0:
                break
        j += 1
    block = text[i:j]
    if "local-mac-address" in block:
        print(f"{path}: &gem0 already has local-mac-address, skipping")
        return False

    # Indentation: mirror the first property line inside the block.
    body = text[i + 1:j]
    indent = "\t"
    for line in body.splitlines():
        if line.strip():
            indent = line[:len(line) - len(line.lstrip())]
            break

    prop = f"\n{indent}local-mac-address = [{_mac_bytes(mac)}];"
    # Insert right after the opening brace.
    text = text[:i + 1] + prop + text[i + 1:]
    path.write_text(text)
    print(f"{path}: pinned &gem0 local-mac-address = [{_mac_bytes(mac)}]")
    return True


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        raise SystemExit(__doc__)
    path = pathlib.Path(argv[1])
    mac = argv[2] if len(argv) > 2 else DEFAULT_MAC
    patch(path, mac)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
