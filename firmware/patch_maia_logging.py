#!/usr/bin/env python3
"""Idempotently capture maia-httpd's logs to a bounded on-device ring.

Why this exists
---------------
maia-httpd logs via the ``tracing`` crate to stdout, and the init script launches
it with ``start-stop-daemon -b`` -- so on the Pluto (no journald, and the rootfs is
ramfs) that output is simply discarded. When the receiver misbehaves (an OOM, a
panic, a DMA-stall exit) there is nothing to look at after the fact.

The fix
-------
In the ``start)`` case, just before the daemon launch, pick a log file --
preferring the SD card (``/mnt/sdcard``, persists across reboots and is mounted
earlier in ``start)``) and falling back to ``/tmp`` -- rotate it if it has grown
past ~1 MB (keeping one previous file, so it is a bounded ring), export a sane
``RUST_LOG`` (maia-httpd's ``main`` already honours it via ``EnvFilter``), and
**redirect the launch line's stdout/stderr into that file**. ``start-stop-daemon``
forks for ``-b`` but the daemon inherits the redirected fds, so this captures
maia-httpd's own output.

It is whitespace tolerant and a no-op if already applied (``airband-logcap``
marker). It MUST run BEFORE ``patch_maia_supervisor.py`` so the supervisor's
verbatim relaunch of the launch line inherits the same redirection.

Usage:
    patch_maia_logging.py <path-to-S60maia-httpd> [more ...]
"""
from __future__ import annotations

import re
import sys
import pathlib

MARKER = "airband-logcap"

# The daemon-launch line we anchor on: any
# `start-stop-daemon -S ... /root/maia-httpd ...` invocation.
LAUNCH_LINE_RE = re.compile(
    r"^(?P<indent>[ \t]*)(?P<cmd>start-stop-daemon\s+-S\b.*?/root/maia-httpd.*)$"
)

LOG_PATH = "/mnt/sdcard/maia-httpd.log"
TMP_LOG_PATH = "/tmp/maia-httpd.log"
REDIRECT = '>> "$MAIA_LOG" 2>&1'


def _setup_block(indent: str) -> str:
    return (
        f'{indent}# {MARKER}: capture maia-httpd stdout/stderr to a bounded log so a\n'
        f'{indent}# crash/OOM/DMA-stall is diagnosable (rootfs is ramfs; prefer the SD card,\n'
        f'{indent}# fall back to /tmp). The daemon inherits these fds through start-stop-daemon -b.\n'
        f'{indent}MAIA_LOG={LOG_PATH}\n'
        f"{indent}grep -q ' /mnt/sdcard ' /proc/mounts 2>/dev/null || MAIA_LOG={TMP_LOG_PATH}\n"
        f'{indent}# bounded ring: rotate at ~1 MB, keep one previous file.\n'
        f'{indent}if [ -f "$MAIA_LOG" ] && [ "$(wc -c < "$MAIA_LOG" 2>/dev/null || echo 0)" -gt 1048576 ]; then\n'
        f'{indent}  mv -f "$MAIA_LOG" "$MAIA_LOG.1" 2>/dev/null || true\n'
        f'{indent}fi\n'
        f'{indent}export RUST_LOG="${{RUST_LOG:-info}}"\n'
    )


def patch(path: pathlib.Path) -> bool:
    text = path.read_text()
    if MARKER in text:
        print(f"{path}: already captures maia-httpd logs, skipping")
        return False

    lines = text.splitlines(keepends=True)
    out: list[str] = []
    inserted = False
    for line in lines:
        stripped = line.rstrip("\n")
        m = LAUNCH_LINE_RE.match(stripped)
        if m and not inserted:
            indent = m.group("indent")
            cmd = m.group("cmd")
            out.append(_setup_block(indent))
            # Append the redirect to the launch line itself.
            if REDIRECT not in cmd:
                cmd = f"{cmd} {REDIRECT}"
            out.append(f"{indent}{cmd}\n")
            inserted = True
        else:
            out.append(line if line.endswith("\n") else line + "\n")

    if not inserted:
        raise SystemExit(
            f"{path}: could not find the 'start-stop-daemon -S ... /root/maia-httpd' "
            f"launch line; cannot wire log capture (has the init script changed?)")

    path.write_text("".join(out))
    print(f"{path}: capturing maia-httpd stdout/stderr to {LOG_PATH} (RUST_LOG=info)")
    return True


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        raise SystemExit(f"usage: {argv[0]} <path-to-S60maia-httpd> [more ...]")
    for p in argv[1:]:
        patch(pathlib.Path(p))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
