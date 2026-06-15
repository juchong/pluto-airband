#!/usr/bin/env bash
#
# Build ONLY the airband bitstream/XSA, using Vivado + a Python venv directly on
# the build-server host (no Docker). This is a faster inner loop than the full
# container build (firmware/build_firmware_full.sh) when you are iterating on the
# HDL and only need a fresh system_top.xsa / system_top.bit.
#
# It forces an IP regen (so config.py changes such as the airband address map
# take effect), then builds the maia-hdl `pluto` project.
#
# Prereqs on the host:
#   - Vivado 2023.2 at $VIVADO_SETTINGS (see DEV-SETUP.md host-Vivado notes).
#   - A Python venv at $VENV with amaranth/numpy/scipy (the Verilog-gen deps).
#   - The maia-sdr fork (pluto-airband branch) at $FORK.
#
# Output XSA: $FORK/maia-hdl/projects/pluto/pluto.sdk/system_top.xsa
# (feed it to firmware/build_firmware.sh via XSA= for a FIT-only firmware build).
#
set -euo pipefail

AIRBAND_REPO="${AIRBAND_REPO:-$HOME/pluto-build/airband}"
FORK="${FORK:-$AIRBAND_REPO/maia-sdr}"
VENV="${VENV:-$AIRBAND_REPO/venv}"
VIVADO_SETTINGS="${VIVADO_SETTINGS:-/opt/Xilinx/Vivado/2023.2/settings64.sh}"

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
echo "=== make -C projects/pluto clean ==="
make -C projects/pluto clean || true

echo "=== START $(date) ==="
make -C projects/pluto
echo "BITSTREAM_EXIT=$?"
echo "=== END $(date) ==="

ls -la projects/pluto/pluto.sdk/system_top.xsa \
       projects/pluto/pluto.runs/impl_1/system_top.bit 2>&1
