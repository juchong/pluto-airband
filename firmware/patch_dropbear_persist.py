#!/usr/bin/env python3
"""Idempotently persist the SSH host key + authorized_keys across power cycles.

Why this exists
---------------
The Pluto rootfs is ramfs (volatile). The dropbear init script (S50dropbear)
launches with ``-R`` ("create hostkeys as required") into ``/etc/dropbear`` --
which, being on ramfs, is empty on every boot, so dropbear generates a BRAND NEW
host key each power cycle. Result: the SSH host-key fingerprint churns and
clients hit "REMOTE HOST IDENTIFICATION HAS CHANGED". Likewise ``/root/.ssh`` is
wiped each boot, so key-based auth can never persist.

dropbear has public-key auth compiled in (``svr_auth_pubkey``); it only lacks a
persistent ``authorized_keys``. The fix for both is the writable jffs2 NVM
(``/mnt/jffs2``, ``mtd2`` -- survives power cycles AND ``firmware.dfu``
reflashes, which only rewrite ``mtd3``).

This injects a block at the TOP of ``start()`` in the dropbear init script
(``buildroot/package/dropbear/S50dropbear``), before the ``-R`` line, that:

  * generates any missing host key once into ``/mnt/jffs2/dropbear`` and copies
    it into ``/etc/dropbear`` (so the existing ``-R`` reuses it, never
    regenerates), and
  * restores ``/root/.ssh/authorized_keys`` from ``/mnt/jffs2/dropbear`` with
    the strict perms dropbear requires.

Operator (one-time, persists): drop your public key on the device with
``echo "ssh-ed25519 AAAA... you@host" >> /mnt/jffs2/dropbear/authorized_keys``.
No private keys are committed to the repo.

It is whitespace tolerant and a no-op if the block is already present (guarded
by the ``airband-ssh-persist`` marker). Software-only init-script change ->
``firmware.dfu``-only reflash. Password auth is left enabled; disabling it later
is a separate one-line change (add ``-s``/``-g`` to ``DROPBEAR_ARGS``).

Usage:
    patch_dropbear_persist.py <path-to-S50dropbear> [more ...]
"""
from __future__ import annotations

import sys
import pathlib

MARKER = "airband-ssh-persist"

# Anchor: the opening of the start() function. The block goes right after it,
# before `DROPBEAR_ARGS="$DROPBEAR_ARGS -R"`.
ANCHOR = "start() {"

# Tab-indented to match the S50dropbear start() body.
BLOCK = (
    "\t# airband-ssh-persist: rootfs is ramfs, so /etc/dropbear (host keys) and\n"
    "\t# /root/.ssh (authorized_keys) are wiped every boot -> dropbear's -R would\n"
    "\t# regenerate a NEW host key each power cycle (known_hosts churn) and pubkey\n"
    "\t# auth could never persist. Persist both on jffs2 (/mnt/jffs2, mtd2; survives\n"
    "\t# power cycles AND firmware.dfu reflashes) and restore before dropbear starts.\n"
    "\tPERSIST=/mnt/jffs2/dropbear\n"
    "\tif [ -d /mnt/jffs2 ]; then\n"
    "\t\tmkdir -p \"$PERSIST\" /etc/dropbear\n"
    "\t\tfor t in ed25519 rsa ecdsa; do\n"
    "\t\t\tf=\"$PERSIST/dropbear_${t}_host_key\"\n"
    "\t\t\t[ -f \"$f\" ] || dropbearkey -t \"$t\" -f \"$f\" >/dev/null 2>&1\n"
    "\t\t\t[ -f \"$f\" ] && cp -a \"$f\" /etc/dropbear/\n"
    "\t\tdone\n"
    "\t\tif [ -f \"$PERSIST/authorized_keys\" ]; then\n"
    "\t\t\tmkdir -p /root/.ssh && chmod 700 /root/.ssh\n"
    "\t\t\tcp \"$PERSIST/authorized_keys\" /root/.ssh/authorized_keys\n"
    "\t\t\tchmod 600 /root/.ssh/authorized_keys\n"
    "\t\tfi\n"
    "\tfi\n"
)


def patch(path: pathlib.Path) -> bool:
    text = path.read_text()
    if MARKER in text:
        print(f"{path}: already persists SSH keys on boot, skipping")
        return False

    lines = text.splitlines(keepends=True)
    out = []
    inserted = False
    for line in lines:
        out.append(line)
        if not inserted and line.strip() == ANCHOR:
            if not line.endswith("\n"):
                out[-1] = line + "\n"
            out.append(BLOCK)
            inserted = True

    if not inserted:
        raise SystemExit(
            f"{path}: anchor {ANCHOR!r} not found; cannot place SSH-persist block "
            f"(has the dropbear init script changed?)")

    path.write_text("".join(out))
    print(f"{path}: injected jffs2 SSH host-key + authorized_keys persistence")
    return True


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        raise SystemExit(f"usage: {argv[0]} <path-to-S50dropbear> [more ...]")
    for p in argv[1:]:
        patch(pathlib.Path(p))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
