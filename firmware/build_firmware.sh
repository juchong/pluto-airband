#!/usr/bin/env bash
#
# Build a flashable Pluto airband firmware image (pluto.frm / pluto.dfu) on the
# x86-64 build server.
#
# Strategy: feed the *prebuilt* cyclic-DMA bitstream (system_top.xsa produced by
# `make -C maia-sdr/maia-hdl/projects/pluto`) into plutosdr-fw with HAVE_VIVADO=0,
# so the firmware container does not need Vivado or numpy/scipy. The container
# only cross-compiles the kernel (with the airband reserved-memory devicetree),
# maia-kmod, maia-httpd (with the airband module + regenerated maia-pac), and
# packs the rootfs + bitstream into the FIT image.
#
# Run on xilinx-builder:
#   bash firmware/build_firmware.sh
#
set -euo pipefail

# --- paths (override via env) ---------------------------------------------
# Prerequisites (see DEV-SETUP.md "Reproduce the build pipeline from scratch"):
#   - this repo cloned to $AIRBAND_REPO (default ~/pluto-build/airband)
#   - the maia-sdr fork (pluto-airband branch) cloned to $FORK (default
#     $AIRBAND_REPO/maia-sdr)
#   - plutosdr-fw cloned to $FW (default ~/pluto-build/plutosdr-fw)
#   - a prebuilt bitstream XSA available (produced by build_firmware_full.sh and
#     saved as ~/pluto-build/system_top_airband.xsa). This script does NOT build
#     the bitstream; it only repacks the FIT (mtd3) around an existing one.
AIRBAND_REPO="${AIRBAND_REPO:-$HOME/pluto-build/airband}"          # this repo
FORK="${FORK:-$AIRBAND_REPO/maia-sdr}"                              # maia-sdr fork (pluto-airband branch)
FW="${FW:-$HOME/pluto-build/plutosdr-fw}"                           # plutosdr-fw clone
TARGET="${TARGET:-pluto}"

# Resolve the prebuilt XSA: explicit $XSA, then the saved full-build artifact,
# then the last plutosdr-fw build output, then the fork's project output.
if [ -z "${XSA:-}" ]; then
    for cand in \
        "$HOME/pluto-build/system_top_airband.xsa" \
        "$FW/build/system_top.xsa" \
        "$FORK/maia-hdl/projects/pluto/pluto.sdk/system_top.xsa"; do
        [ -f "$cand" ] && { XSA="$cand"; break; }
    done
fi

echo "== plutosdr-fw at $FW =="
[ -d "$FW" ] || { echo "ERROR: plutosdr-fw not found at $FW"; exit 1; }
[ -n "${XSA:-}" ] && [ -f "$XSA" ] || {
    echo "ERROR: no prebuilt XSA found. Run firmware/build_firmware_full.sh first"
    echo "       (it builds the bitstream), or set XSA=/path/to/system_top.xsa."
    exit 1
}
echo "== using prebuilt bitstream XSA: $XSA =="

cd "$FW"

# 1. Make sure the firmware submodules (linux/buildroot/u-boot) are present.
git submodule update --init --recursive linux buildroot u-boot-xlnx

# 2. Splice in our maia-sdr fork (CI pattern) and init its nested submodules.
#    Use rsync with excludes so the multi-GB Vivado run dirs and Rust/web build
#    outputs are not copied (HAVE_VIVADO=0 means maia-hdl is not rebuilt here).
echo "== splicing maia-sdr fork from $FORK =="
rm -rf maia-sdr
mkdir -p maia-sdr
rsync -a \
    --exclude 'maia-hdl/projects/*/*.runs' \
    --exclude 'maia-hdl/projects/*/*.cache' \
    --exclude 'maia-hdl/projects/*/*.gen' \
    --exclude 'maia-hdl/projects/*/*.hw' \
    --exclude 'maia-hdl/projects/*/*.ip_user_files' \
    --exclude 'maia-hdl/projects/*/*.sim' \
    --exclude 'maia-hdl/projects/*/.Xil' \
    --exclude '**/target/' \
    --exclude '**/node_modules/' \
    "$FORK"/ maia-sdr/
# maia-httpd/maia-kmod/maia-wasm have no submodules; the maia-hdl Vivado
# submodules are not needed with HAVE_VIVADO=0, so this is best-effort.
( cd maia-sdr && git submodule update --init --recursive ) || \
    echo "WARN: submodule update skipped (not needed for HAVE_VIVADO=0)"

# 3. Add the airband reserved-memory devicetree node (idempotent).
echo "== patching devicetree =="
python3 "$AIRBAND_REPO/firmware/apply_airband_devicetree.py" \
    linux/arch/arm/boot/dts/zynq-pluto-sdr-maiasdr.dtsi

# 3b. Auto-start airband: append --airband to the maia-httpd init script so the
#     receiver + audio stream come up on boot (idempotent).
S60=buildroot/board/pluto/S60maia-httpd
if [ -f "$S60" ] && ! grep -q -- '--airband' "$S60"; then
    sed -i 's#--ca-cert /mnt/jffs2/maia-sdr-ca.crt#--ca-cert /mnt/jffs2/maia-sdr-ca.crt --airband#' "$S60"
    echo "== patched $S60: maia-httpd now auto-starts with --airband =="
fi

# 4. Stage the prebuilt bitstream for HAVE_VIVADO=0 injection.
cp "$XSA" "$FW/system_top_airband.xsa"

# 5. Write a HAVE_VIVADO=0 entrypoint for the build container.
cat > build-docker-airband.sh <<'EOS'
#!/bin/bash
set -euox pipefail
source /opt/rust/env
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin/:/usr/bin:/sbin:/bin:$PATH
cd /w
make HAVE_VIVADO=0 XSA_FILE=/w/system_top_airband.xsa TARGET="${TARGET:-pluto}" \
     build/${TARGET:-pluto}.frm
make HAVE_VIVADO=0 XSA_FILE=/w/system_top_airband.xsa TARGET="${TARGET:-pluto}" \
     build/${TARGET:-pluto}.dfu || echo "WARN: .dfu not built (dfu-suffix missing?)"
EOS
chmod +x build-docker-airband.sh

# 6. Build inside the maia-sdr-devel container (rootless docker -> DOCKER_USER=0:0).
echo "== building firmware in container =="
DOCKER_USER="${DOCKER_USER:-0:0}" TARGET="$TARGET" \
    docker compose run --rm --entrypoint /w/build-docker-airband.sh build

echo "== artifacts =="
ls -la "$FW"/build/"$TARGET".frm "$FW"/build/"$TARGET".dfu 2>/dev/null || true
echo "Flash with:  dfu-util -a firmware.dfu -D $FW/build/$TARGET.dfu  (or copy .frm to the PlutoSDR mass-storage device)"
