#!/usr/bin/env python3
"""Idempotently enable SD-card detection in the Pluto+ devicetree.

Why this exists
---------------
The Pluto+ has a microSD slot wired to the Zynq SD0 controller
(``&sdhci0`` = ``mmc@e0100000``, ``arasan,sdhci-8.9a``). The stock board .dts
enables the controller (``status = "okay"; disable-wp;``) but declares **no
card-detect**: there is no ``broken-cd``, ``non-removable`` or ``cd-gpios``
property. With no usable CD signal the SDHCI driver assumes the slot is empty
and never probes the card, so ``/dev/mmcblk0`` never appears (confirmed on
hardware: ``mmc0`` registers but no card enumerates).

``broken-cd`` tells the driver there is no working card-detect line and to poll
for a card instead -- the standard fix for a slot whose CD is not wired to the
controller. ``no-1-8-v`` keeps the card on the 3.3 V signalling rail (Zynq SD0
on the Pluto+ has no 1.8 V switch), avoiding a failed UHS voltage switch on
some cards.

This patches the **OS devicetree** (the FIT consumed from ``mtd3``), so the fix
is part of the image the kernel already parses -- no u-boot dependency, and a
``firmware.dfu``-only reflash (no ``boot.dfu``, env preserved).

This script inserts, into the ``&sdhci0 { ... }`` override block of
``zynq-plutoplus-maiasdr.dts``::

    broken-cd;
    no-1-8-v;

It is whitespace tolerant and a no-op if the node already has ``broken-cd``.

Usage:
    apply_sdcard_devicetree.py <path-to-zynq-plutoplus-maiasdr.dts>
"""
from __future__ import annotations

import re
import sys
import pathlib

# The SD controller is overridden by label in the Pluto+ board .dts.
SDHCI_OPEN_RE = r"&sdhci0\s*\{"

# Properties to ensure are present inside the &sdhci0 block.
PROPS = ("broken-cd", "no-1-8-v")


def patch(path: pathlib.Path) -> bool:
    text = path.read_text()
    m = re.search(SDHCI_OPEN_RE, text)
    if not m:
        raise SystemExit(f"{path}: anchor not found: {SDHCI_OPEN_RE!r} "
                         "(is this the Pluto+ board .dts?)")
    # Find the brace-balanced extent of the &sdhci0 block so we only inspect and
    # insert within it.
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

    missing = [p for p in PROPS if not re.search(rf"\b{re.escape(p)}\b", block)]
    if not missing:
        print(f"{path}: &sdhci0 already has {', '.join(PROPS)}, skipping")
        return False

    # Indentation: mirror the first property line inside the block.
    body = text[i + 1:j]
    indent = "\t"
    for line in body.splitlines():
        if line.strip():
            indent = line[:len(line) - len(line.lstrip())]
            break

    addition = "".join(f"\n{indent}{p};" for p in missing)
    # Insert right after the opening brace.
    text = text[:i + 1] + addition + text[i + 1:]
    path.write_text(text)
    print(f"{path}: enabled SD card detect on &sdhci0 ({', '.join(missing)})")
    return True


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        raise SystemExit(__doc__)
    patch(pathlib.Path(argv[1]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
