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
AIRBAND_REPO="${AIRBAND_REPO:-$HOME/pluto-build/airband}"          # this repo
FORK="${FORK:-$AIRBAND_REPO/maia-sdr}"                              # maia-sdr fork (pluto-airband branch)
FW="${FW:-$HOME/pluto-build/plutosdr-fw}"                           # plutosdr-fw clone
XSA="${XSA:-$FORK/maia-hdl/projects/pluto/pluto.sdk/system_top.xsa}"  # prebuilt cyclic-DMA xsa
TARGET="${TARGET:-pluto}"

echo "== plutosdr-fw at $FW =="
[ -d "$FW" ] || { echo "ERROR: plutosdr-fw not found at $FW"; exit 1; }
[ -f "$XSA" ] || { echo "ERROR: prebuilt xsa not found at $XSA (build the bitstream first)"; exit 1; }

cd "$FW"

# 1. Make sure the firmware submodules (linux/buildroot/u-boot) are present.
git submodule update --init --recursive linux buildroot u-boot-xlnx

# 2. Splice in our maia-sdr fork (CI pattern) and init its nested submodules.
echo "== splicing maia-sdr fork from $FORK =="
rm -rf maia-sdr
cp -a "$FORK" maia-sdr
( cd maia-sdr && git submodule update --init --recursive )

# 3. Add the airband reserved-memory devicetree node (idempotent).
echo "== patching devicetree =="
python3 "$AIRBAND_REPO/firmware/apply_airband_devicetree.py" \
    linux/arch/arm/boot/dts/zynq-pluto-sdr-maiasdr.dtsi

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
