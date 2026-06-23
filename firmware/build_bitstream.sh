#!/usr/bin/env bash
#
# FAST inner-loop bitstream build (host Vivado, no Docker) for HDL iteration and
# timing inspection. Produces a fresh system_top.xsa / system_top.bit from the
# maia-sdr fork's $TARGET project (default `pluto`; set TARGET=plutoplus for the
# Pluto+ clg400 part).
#
# This does NOT produce flashable firmware. There is no FIT/BOOT.BIN/DFU here.
# To get something you can flash, always use firmware/build_firmware_full.sh
# (the single source of flashable boot.* / pluto.* images). Use this script only
# to check that an HDL change synthesizes and meets timing before committing to a
# ~20 min full build.
#
# Like the full build, it builds from committed git state and bakes the fork's
# short commit hash into the bitstream (USERID + USR_ACCESS via system_project.tcl).
#
# Prereqs on the host:
#   - Vivado 2023.2 at $VIVADO_SETTINGS (see BUILD.md host-Vivado notes).
#   - A Python venv at $VENV with amaranth/numpy/scipy (the Verilog-gen deps).
#   - The maia-sdr fork (pluto-airband branch) cloned at $FORK.
#
# Output: $FORK/maia-hdl/projects/$TARGET/$TARGET.sdk/system_top.xsa
#         $FORK/maia-hdl/projects/$TARGET/$TARGET.runs/impl_1/system_top.bit
#
set -euo pipefail

AIRBAND_REPO="${AIRBAND_REPO:-$HOME/pluto-build/airband}"
FORK="${FORK:-$AIRBAND_REPO/maia-sdr}"
VENV="${VENV:-$AIRBAND_REPO/venv}"
VIVADO_SETTINGS="${VIVADO_SETTINGS:-/opt/Xilinx/Vivado/2023.2/settings64.sh}"
TARGET="${TARGET:-pluto}"

[ -d "$FORK/.git" ] || { echo "ERROR: fork at $FORK is not a git clone"; exit 1; }

# Build from committed state and bake in the commit hash (see system_project.tcl).
echo "== pull fork $FORK =="
git -C "$FORK" pull --ff-only
git -C "$FORK" submodule update --init --recursive
if [ -n "$(git -C "$FORK" status --porcelain)" ]; then
    if [ "${FORCE_DIRTY:-0}" = "1" ]; then
        echo "WARN: fork dirty; FORCE_DIRTY=1 -> sentinel hash 0xDEADBEEF"
        export GIT_HASH="deadbeef"
    else
        echo "ERROR: fork has uncommitted changes; commit them or set FORCE_DIRTY=1."
        git -C "$FORK" status --porcelain
        exit 1
    fi
else
    export GIT_HASH="$(git -C "$FORK" rev-parse --short=8 HEAD)"
fi
echo "== building $(git -C "$FORK" rev-parse --abbrev-ref HEAD) @ $GIT_HASH =="

cd "$FORK/maia-hdl"
# shellcheck disable=SC1090
source "$VIVADO_SETTINGS"
# shellcheck disable=SC1091
. "$VENV/bin/activate"
export AMARANTH_USE_YOSYS=builtin

echo "=== config airband range ==="
grep -n "airband_address_range\|recorder_address_range" maia_hdl/config.py | head

echo "=== make -C ip clean (force IP regen from config.py) ==="
make -C ip clean
echo "=== make -C projects/$TARGET clean ==="
make -C "projects/$TARGET" clean || true

echo "=== START $(date) ==="
make -C "projects/$TARGET"
echo "BITSTREAM_EXIT=$?"
echo "=== END $(date) ==="

BIT="projects/$TARGET/$TARGET.runs/impl_1/system_top.bit"
ls -la "projects/$TARGET/$TARGET.sdk/system_top.xsa" "$BIT" 2>&1
echo "=== embedded provenance (must equal 0x$GIT_HASH) ==="
strings "$BIT" 2>/dev/null | grep -oiE 'UserID=0x[0-9A-Fa-f]+' | head -1 || true
