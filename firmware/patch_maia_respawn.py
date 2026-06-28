#!/usr/bin/env python3
"""Idempotently make maia-httpd survive a tight-memory boot race.

SUPERSEDED: the firmware build now installs a *permanent* restart-safe supervisor
via ``patch_maia_supervisor.py`` (which also strips this script's bounded block on
migration). This bounded respawn is retained only for reference / historical
provenance; it is no longer wired into ``build_firmware_full.sh``.

Why this exists
---------------
On the Pluto the kernel sees 512 MB but reserves ~416 MB for the maia-sdr DMA
regions (recording + spectrometer + airband), leaving userspace a tight **~96 MB**
pool. maia-httpd's startup footprint sits close to that edge, so a transient spike
on a single boot -- observed on the **first boot after a flash** -- can let the OOM
killer take it. The init script launches it with ``start-stop-daemon -b`` and then
**never respawns**, so one unlucky boot leaves every service dead (web :8000,
audio stream :30000) until a human runs ``S60maia-httpd restart``. A manual restart
always works, because by then the transient pressure is gone -- i.e. the failure is
a boot-time race, not a steady-state leak (uptime is stable once it is up).

The fix
-------
Inject a **backgrounded, bounded** retry into the ``start)`` case, right after the
daemon launch: re-check every 10 s for ~90 s and relaunch maia-httpd if it died,
then exit. This self-heals the boot race without manual intervention.

It is deliberately **bounded, not a permanent supervisor**: maia-httpd is itself
restarted at runtime by the web UI and by ``diagnostics/lte_calibrate.py`` (both
call ``S60maia-httpd restart``), and a permanent watchdog would race that ``stop``
and relaunch the daemon out from under it. A bounded loop finishes within ~90 s of
boot and is gone before any such restart, so it cannot interfere. Steady-state OOM
protection is intentionally *not* attempted here (making the one big process immune
just redirects the killer at dropbear/the watchdog -- worse).

The relaunch command is captured **verbatim** from the script's own
``start-stop-daemon -S ... /root/maia-httpd ...`` line, so it can never drift from
the real launch (including the ``--airband`` flag appended earlier in the build).

It is whitespace tolerant and a no-op if the block is already present (guarded by
the ``airband-respawn`` marker).

Usage:
    patch_maia_respawn.py <path-to-S60maia-httpd> [more ...]
"""
from __future__ import annotations

import re
import sys
import pathlib

MARKER = "airband-respawn"

# The daemon-launch line we anchor on (and reuse verbatim for the relaunch):
# any `start-stop-daemon -S ... /root/maia-httpd ...` invocation.
LAUNCH_LINE_RE = re.compile(
    r"^(?P<indent>[ \t]*)(?P<cmd>start-stop-daemon\s+-S\b.*?/root/maia-httpd.*)$"
)


def _block(indent: str, launch: str) -> str:
    return (
        f"{indent}# {MARKER}: maia-httpd starts in a tight ~96 MB pool (the rest is\n"
        f"{indent}# reserved for the maia-sdr DMA regions), so a transient spike on the\n"
        f"{indent}# FIRST boot after a flash can let the OOM killer take it. start-stop-daemon\n"
        f"{indent}# -b does not respawn, which leaves web :8000 / audio :30000 dead until a\n"
        f"{indent}# manual restart. Re-check for ~90 s and relaunch if it died, so boot\n"
        f"{indent}# self-heals. Bounded (NOT a permanent supervisor) so it never fights the\n"
        f"{indent}# web-UI / calibration `S60maia-httpd restart`.\n"
        f"{indent}(\n"
        f"{indent}  n=0\n"
        f"{indent}  while [ $n -lt 9 ]; do\n"
        f"{indent}    sleep 10\n"
        f"{indent}    start-stop-daemon -K -t -q -x /root/maia-httpd && break\n"
        f"{indent}    {launch}\n"
        f"{indent}    n=$((n+1))\n"
        f"{indent}  done\n"
        f"{indent}) &\n"
    )


def patch(path: pathlib.Path) -> bool:
    text = path.read_text()
    if MARKER in text:
        print(f"{path}: already self-heals maia-httpd on boot, skipping")
        return False

    lines = text.splitlines(keepends=True)
    out: list[str] = []
    inserted = False
    for line in lines:
        out.append(line)
        if not inserted:
            m = LAUNCH_LINE_RE.match(line.rstrip("\n"))
            if m:
                if not line.endswith("\n"):
                    out[-1] = line + "\n"
                out.append(_block(m.group("indent"), m.group("cmd").strip()))
                inserted = True

    if not inserted:
        raise SystemExit(
            f"{path}: could not find the 'start-stop-daemon -S ... /root/maia-httpd' "
            f"launch line; cannot place the respawn block (has the init script changed?)")

    path.write_text("".join(out))
    print(f"{path}: injected bounded boot-time maia-httpd respawn")
    return True


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        raise SystemExit(f"usage: {argv[0]} <path-to-S60maia-httpd> [more ...]")
    for p in argv[1:]:
        patch(pathlib.Path(p))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
