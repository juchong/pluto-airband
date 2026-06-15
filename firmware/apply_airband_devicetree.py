#!/usr/bin/env python3
"""Idempotently add the airband reserved-memory region to the Pluto devicetree.

The airband framed-audio DMA writes a continuous ring to the top 16 MiB of the
Pluto's 512 MiB DDR (phys 0x1f000000 .. 0x20000000), matching
``MaiaSDRConfig.airband_address_range``. The maia-sdr kernel module exposes that
region to userspace as an ``rxbuffer`` character device (``/dev/maia-sdr-airband``)
when a matching reserved-memory node + platform node exist in the devicetree.

This inserts both nodes into ``zynq-pluto-sdr-maiasdr.dtsi`` (the shared maia-sdr
overlay #included by every Pluto rev's .dts), right after the existing
``maia_sdr_recording`` / ``maia-sdr-spectrometer`` nodes. It is whitespace
tolerant and a no-op if the airband nodes are already present.

Usage:
    apply_airband_devicetree.py <path-to-zynq-pluto-sdr-maiasdr.dtsi> [more.dtsi ...]

Defaults to the standard plutosdr-fw location if no path is given.
"""
from __future__ import annotations

import re
import sys
import pathlib

# Must match MaiaSDRConfig.airband_address_range (start, end).
AIRBAND_BASE = 0x1F00_0000
AIRBAND_SIZE = 0x0100_0000          # 16 MiB
# Ring buffer slot size for the rxbuffer device (region / buffer-size = slots).
AIRBAND_BUFFER_SIZE = 0x1_0000      # 64 KiB -> 256 slots

DEFAULT_DTSI = (
    "linux/arch/arm/boot/dts/zynq-pluto-sdr-maiasdr.dtsi"
)


def _indent_of(line: str) -> str:
    return line[:len(line) - len(line.lstrip())]


def _insert_after_block(text: str, open_re: str, new_node: str) -> str:
    """Insert new_node right after the brace-balanced block whose opening line
    matches open_re. Reproduces the matched block's indentation."""
    m = re.search(open_re, text)
    if not m:
        raise SystemExit(f'anchor not found: {open_re!r}')
    # find the matching closing brace for the block opened at m
    i = text.index('{', m.start())
    depth = 0
    j = i
    while j < len(text):
        if text[j] == '{':
            depth += 1
        elif text[j] == '}':
            depth -= 1
            if depth == 0:
                break
        j += 1
    # j points at the closing '}'; include trailing ';'
    end = j + 1
    if text[end:end + 1] == ';':
        end += 1
    line_start = text.rfind('\n', 0, m.start()) + 1
    outer = _indent_of(text[line_start:m.start() + 1])
    inner = outer + ('\t' if '\t' in outer or not outer else '    ')
    node = (new_node
            .replace('@OUTER@', outer)
            .replace('@INNER@', inner))
    return text[:end] + '\n' + node + text[end:]


RESERVED_NODE = (
    '@OUTER@maia_sdr_airband: maia_sdr_airband@1f000000 {\n'
    '@INNER@no-map;\n'
    f'@INNER@reg = <{AIRBAND_BASE:#x} {AIRBAND_SIZE:#x}>;\n'
    '@INNER@label = "maia_sdr_airband";\n'
    '@OUTER@};\n'
)

PLATFORM_NODE = (
    '@OUTER@maia-sdr-airband {\n'
    '@INNER@compatible = "maia-sdr,rxbuffer";\n'
    '@INNER@memory-region = <&maia_sdr_airband>;\n'
    f'@INNER@buffer-size = <{AIRBAND_BUFFER_SIZE:#x}>;\n'
    '@OUTER@};\n'
)


def patch(path: pathlib.Path) -> bool:
    text = path.read_text()
    if 'maia_sdr_airband' in text:
        print(f'{path}: already has airband nodes, skipping')
        return False
    text = _insert_after_block(
        text,
        r'maia_sdr_recording:\s*maia_sdr_recording@[0-9a-fA-Fx]+\s*\{',
        RESERVED_NODE)
    text = _insert_after_block(
        text,
        r'maia-sdr-spectrometer\s*\{',
        PLATFORM_NODE)
    path.write_text(text)
    print(f'{path}: inserted airband reserved-memory + rxbuffer nodes '
          f'({AIRBAND_BASE:#x}, {AIRBAND_SIZE:#x}, slot {AIRBAND_BUFFER_SIZE:#x})')
    return True


def main(argv: list[str]) -> int:
    paths = argv[1:] or [DEFAULT_DTSI]
    for p in paths:
        patch(pathlib.Path(p))
    return 0


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
