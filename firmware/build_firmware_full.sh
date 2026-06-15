#!/usr/bin/env bash
#
# FULL Pluto airband firmware build (HAVE_VIVADO=1) on the x86-64 build server.
#
# Unlike build_firmware.sh (HAVE_VIVADO=0, FIT/mtd3 only), this rebuilds the
# bitstream AND the FSBL from our maia-sdr fork, then packs BOTH firmware
# partitions:
#   - build/boot.frm  / build/boot.dfu   -> BOOT.BIN (FSBL + bitstream + u-boot) -> mtd0
#   - build/pluto.frm / build/pluto.dfu  -> FIT (kernel + DT + rootfs + bitstream) -> mtd3
#
# The FSBL is regenerated from the new XSA via xsct, so it matches the new
# block design (HP0 enabled for the airband DMA). This is the build that fixes
# the "old bitstream / HP0 disabled" mismatch that crashed airband.
#
# Run on the build server (Vivado available via the vivado2023_2 docker volume):
#   bash firmware/build_firmware_full.sh
#
set -euo pipefail

# --- paths (override via env) ---------------------------------------------
AIRBAND_REPO="${AIRBAND_REPO:-$HOME/pluto-build/airband}"   # this repo
FORK="${FORK:-$AIRBAND_REPO/maia-sdr}"                       # maia-sdr fork (pluto-airband branch)
FW="${FW:-$HOME/pluto-build/plutosdr-fw}"                    # plutosdr-fw clone
TARGET="${TARGET:-pluto}"

echo "== plutosdr-fw at $FW =="
[ -d "$FW" ] || { echo "ERROR: plutosdr-fw not found at $FW"; exit 1; }
[ -d "$FORK" ] || { echo "ERROR: maia-sdr fork not found at $FORK"; exit 1; }

cd "$FW"

# 1. Firmware submodules (kernel/buildroot/u-boot).
git submodule update --init --recursive linux buildroot u-boot-xlnx

# 2. Splice in our maia-sdr fork. Keep the HDL sources (adi-hdl etc.) so the
#    container can rebuild the bitstream; only drop the heavy Vivado run dirs
#    and Rust/web build outputs (those regenerate).
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
    --exclude 'maia-hdl/projects/*/*.sdk' \
    --exclude '**/target/' \
    --exclude '**/node_modules/' \
    "$FORK"/ maia-sdr/
# Best-effort: refresh nested submodules. If the gitlink is broken by the
# rsync, the working-tree files copied above are still sufficient for Vivado.
( cd maia-sdr && git submodule update --init --recursive ) 2>/dev/null || \
    echo "WARN: submodule refresh skipped (using rsync'd working-tree sources)"

# 3. Devicetree: reset to stock then apply the airband reserved-memory patch
#    (idempotent, relocated to 0x19000000 to avoid the CMA collision).
echo "== patching devicetree =="
DTSI=linux/arch/arm/boot/dts/zynq-pluto-sdr-maiasdr.dtsi
git -C linux checkout -- "arch/arm/boot/dts/zynq-pluto-sdr-maiasdr.dtsi" 2>/dev/null || true
python3 "$AIRBAND_REPO/firmware/apply_airband_devicetree.py" "$DTSI"

# 3b. Auto-start airband: append --airband to the maia-httpd init script so the
#     receiver + audio stream come up on boot (idempotent). The init script is a
#     static buildroot overlay file installed by board/pluto/post-build.sh.
S60=buildroot/board/pluto/S60maia-httpd
if [ -f "$S60" ] && ! grep -q -- '--airband' "$S60"; then
    sed -i 's#--ca-cert /mnt/jffs2/maia-sdr-ca.crt#--ca-cert /mnt/jffs2/maia-sdr-ca.crt --airband#' "$S60"
    echo "== patched $S60: maia-httpd now auto-starts with --airband =="
fi

# 4. Full build inside the maia-sdr-devel container (Vivado from /opt/Xilinx
#    volume -> HAVE_VIVADO=1). Default entrypoint = build-docker.sh = `make -C /w`
#    = clean-build + boot.{frm,dfu} + pluto.{frm,dfu} + jtag-bootstrap.
echo "== full firmware+bitstream build in container (this takes a while) =="
DOCKER_USER="${DOCKER_USER:-0:0}" TARGET="$TARGET" \
    docker compose run --rm build

echo "== artifacts =="
ls -la "$FW"/build/boot.frm "$FW"/build/boot.dfu \
       "$FW"/build/"$TARGET".frm "$FW"/build/"$TARGET".dfu 2>/dev/null || true
echo
echo "Flash BOTH partitions over DFU:"
echo "  dfu-util -a boot.dfu     -D $FW/build/boot.dfu"
echo "  dfu-util -a firmware.dfu -D $FW/build/$TARGET.dfu"
