#!/usr/bin/env python3
"""Idempotently install a permanent, restart-safe maia-httpd supervisor.

Why this exists
---------------
The original ``patch_maia_respawn.py`` injected a **bounded ~90 s** boot-time
retry: it self-heals the first-boot OOM race, but once that window closes nothing
restarts maia-httpd, so a later crash (OOM, panic, or the new DMA-stall exit)
leaves web :8000 / audio :30000 dead until a human intervenes.

This replaces it with a **permanent supervisor** that relaunches maia-httpd
whenever it dies, indefinitely. The reason a permanent watchdog was *avoided*
before is that maia-httpd is restarted at runtime by the web UI and
``diagnostics/lte_calibrate.py`` (both ``S60maia-httpd restart``), and a naive
respawner would relaunch the daemon out from under that deliberate ``stop``. The
fix is an **intentional-stop flag**: the ``stop)`` case touches it (so the
supervisor stands down for a deliberate stop/restart), and the ``start)`` case
clears it (and kills any prior supervisor, so only one ever runs).

What it does
------------
1. Strips a previously-applied bounded respawn block (``airband-respawn``) if
   present, so switching an existing checkout over is clean.
2. ``start)``: after the daemon launch, clears the flag, kills a stale supervisor,
   and starts a permanent background loop that relaunches maia-httpd (using the
   launch line captured verbatim, including any log redirection) unless the flag
   is set.
3. ``stop)``: before the ``start-stop-daemon -K ... /root/maia-httpd`` kill, sets
   the flag so the supervisor does not fight the deliberate stop/restart.

It is whitespace tolerant and a no-op if already applied (``airband-supervisor``
marker). Run it AFTER ``patch_maia_logging.py`` so the relaunch inherits the log
redirection.

Usage:
    patch_maia_supervisor.py <path-to-S60maia-httpd> [more ...]
"""
from __future__ import annotations

import re
import sys
import pathlib

MARKER = "airband-supervisor"
RESPAWN_MARKER = "airband-respawn"

FLAG = "/tmp/maia-httpd.intentional-stop"
SUP_PID = "/tmp/maia-httpd.supervisor.pid"

LAUNCH_LINE_RE = re.compile(
    r"^(?P<indent>[ \t]*)(?P<cmd>start-stop-daemon\s+-S\b.*?/root/maia-httpd.*)$"
)
# The deliberate-stop kill line in the stop) case.
STOP_LINE_RE = re.compile(
    r"^(?P<indent>[ \t]*)start-stop-daemon\s+-K\b.*?/root/maia-httpd.*$"
)


def _strip_respawn(lines: list[str]) -> list[str]:
    """Removes a previously-injected bounded-respawn block (comment header
    through its closing ``) &``), if present."""
    start = next((i for i, l in enumerate(lines) if RESPAWN_MARKER in l), None)
    if start is None:
        return lines
    end = next((j for j in range(start, len(lines)) if lines[j].strip() == ") &"), None)
    if end is None:
        return lines
    print("  (removing the old bounded airband-respawn block)")
    return lines[:start] + lines[end + 1:]


def _supervisor_block(indent: str, launch: str) -> str:
    return (
        f"{indent}# {MARKER}: permanent, restart-safe maia-httpd supervisor. Relaunches\n"
        f"{indent}# maia-httpd whenever it dies (OOM/panic/DMA-stall exit) so web :8000 /\n"
        f"{indent}# audio :30000 self-heal indefinitely -- not just during the boot window.\n"
        f"{indent}# An intentional-stop flag (set by stop)/restart) makes it stand down so it\n"
        f"{indent}# never fights a deliberate `S60maia-httpd restart`.\n"
        f"{indent}rm -f {FLAG}\n"
        f'{indent}[ -f {SUP_PID} ] && kill "$(cat {SUP_PID})" 2>/dev/null\n'
        f"{indent}(\n"
        f"{indent}  echo $$ > {SUP_PID}\n"
        f"{indent}  while true; do\n"
        f"{indent}    sleep 10\n"
        f"{indent}    [ -f {FLAG} ] && continue\n"
        f"{indent}    start-stop-daemon -K -t -q -x /root/maia-httpd && continue\n"
        f"{indent}    {launch}\n"
        f"{indent}  done\n"
        f"{indent}) &\n"
    )


def _stop_block(indent: str) -> str:
    return (
        f"{indent}# {MARKER}: stand the supervisor down for a deliberate stop/restart.\n"
        f"{indent}touch {FLAG}\n"
    )


def patch(path: pathlib.Path) -> bool:
    text = path.read_text()
    if MARKER in text:
        print(f"{path}: already supervises maia-httpd, skipping")
        return False

    lines = _strip_respawn(text.splitlines(keepends=True))

    out: list[str] = []
    launch_done = False
    stop_done = False
    for line in lines:
        stripped = line.rstrip("\n")
        # Insert the stand-down flag BEFORE the deliberate-stop kill line.
        if not stop_done:
            ms = STOP_LINE_RE.match(stripped)
            if ms:
                out.append(_stop_block(ms.group("indent")))
                stop_done = True
        out.append(line if line.endswith("\n") else line + "\n")
        # Insert the supervisor AFTER the daemon launch line.
        if not launch_done:
            ml = LAUNCH_LINE_RE.match(stripped)
            if ml:
                out.append(_supervisor_block(ml.group("indent"), ml.group("cmd").strip()))
                launch_done = True

    if not launch_done:
        raise SystemExit(
            f"{path}: could not find the 'start-stop-daemon -S ... /root/maia-httpd' "
            f"launch line; cannot install the supervisor (has the init script changed?)")
    if not stop_done:
        raise SystemExit(
            f"{path}: could not find the 'start-stop-daemon -K ... /root/maia-httpd' "
            f"stop line; refusing to install a supervisor that would fight a deliberate "
            f"restart (has the init script changed?)")

    path.write_text("".join(out))
    print(f"{path}: installed permanent restart-safe maia-httpd supervisor")
    return True


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        raise SystemExit(f"usage: {argv[0]} <path-to-S60maia-httpd> [more ...]")
    for p in argv[1:]:
        patch(pathlib.Path(p))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
