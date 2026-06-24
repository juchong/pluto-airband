#!/usr/bin/env bash
#
# FULL Pluto airband firmware build (HAVE_VIVADO=1) on the x86-64 build server.
# This is the ONLY script that produces flashable images. It rebuilds the
# bitstream + FSBL from our maia-sdr fork and packs BOTH firmware partitions
# (the FIT image name follows $TARGET: pluto.* for the ADALM-Pluto,
# plutoplus.* for the Pluto+):
#   - build/boot.frm     / build/boot.dfu      -> BOOT.BIN (FSBL + bitstream + u-boot) -> mtd0
#   - build/$TARGET.frm  / build/$TARGET.dfu   -> FIT (kernel + DT + rootfs + bitstream) -> mtd3
#
# Build for a Pluto+ with: TARGET=plutoplus bash firmware/build_firmware_full.sh
#
# Provenance by construction (so a stale-gateware flash cannot happen silently):
#   1. Builds ONLY from committed git state. Both repos are `git pull`ed at the
#      start; an uncommitted working tree aborts the build (override: FORCE_DIRTY=1).
#   2. The fork's short commit hash is baked into the bitstream as USERID +
#      USR_ACCESS (see maia-hdl/projects/$TARGET/system_project.tcl). After the
#      build, `strings .../system_top.bit | grep -i UserID` (bare hex, e.g.
#      "UserID=8B601CF0") MUST equal the commit you
#      shipped -- this is the authoritative "is the right gateware in the bit"
#      check (a .bit holds config frames, not net names, so grepping signal names
#      never works; the embedded UserID does).
#
# Run on the build server (Vivado available via the vivado2023_2 docker volume):
#   bash firmware/build_firmware_full.sh
#
set -euo pipefail

# --- paths (override via env) ---------------------------------------------
AIRBAND_REPO="${AIRBAND_REPO:-$HOME/pluto-build/airband}"   # this repo (clone of juchong/pluto-airband)
FORK="${FORK:-$AIRBAND_REPO/maia-sdr}"                       # maia-sdr fork (clone of juchong/maia-sdr, pluto-airband)
FW="${FW:-$HOME/pluto-build/plutosdr-fw}"                    # plutosdr-fw clone
TARGET="${TARGET:-pluto}"

[ -d "$FW" ]   || { echo "ERROR: plutosdr-fw not found at $FW"; exit 1; }
[ -d "$FORK/.git" ] || { echo "ERROR: maia-sdr fork at $FORK is not a git clone (the server must build from git)"; exit 1; }

# --- 0. Build only from committed git state -------------------------------
# Pull this repo first and re-exec once, so script/devicetree changes can't be
# stale. (Guarded against a re-exec loop; skipped if $AIRBAND_REPO isn't a clone.)
if [ -d "$AIRBAND_REPO/.git" ] && [ -z "${_FWBUILD_REEXECED:-}" ]; then
    echo "== pull $AIRBAND_REPO =="
    git -C "$AIRBAND_REPO" pull --ff-only
    export _FWBUILD_REEXECED=1
    exec bash "$AIRBAND_REPO/firmware/build_firmware_full.sh" "$@"
fi
[ -d "$AIRBAND_REPO/.git" ] || echo "WARN: $AIRBAND_REPO is not a git clone; scripts may be stale (see BUILD.md 'Build server')"

echo "== pull fork $FORK =="
git -C "$FORK" pull --ff-only
git -C "$FORK" submodule update --init --recursive

# Refuse to ship gateware whose embedded hash would be a lie.
if [ -n "$(git -C "$FORK" status --porcelain)" ]; then
    if [ "${FORCE_DIRTY:-0}" = "1" ]; then
        echo "WARN: fork has uncommitted changes; FORCE_DIRTY=1 set -> baking sentinel hash 0xDEADBEEF"
        GIT_HASH="deadbeef"
    else
        echo "ERROR: fork at $FORK has uncommitted changes. Commit them so the bitstream's"
        echo "       embedded commit hash is meaningful, or re-run with FORCE_DIRTY=1."
        git -C "$FORK" status --porcelain
        exit 1
    fi
else
    GIT_HASH="$(git -C "$FORK" rev-parse --short=8 HEAD)"
fi
echo "== building fork $(git -C "$FORK" rev-parse --abbrev-ref HEAD) @ $GIT_HASH =="
echo "   $(git -C "$FORK" log -1 --format='%h %s')"

cd "$FW"

# 1. Firmware submodules (kernel/buildroot/u-boot).
git submodule update --init --recursive linux buildroot u-boot-xlnx

# 2. Provide the fork to plutosdr-fw. We mirror the fork's *committed* checkout
#    (the dirty-tree guard above guarantees there are no uncommitted changes, so
#    this only ever copies tracked, committed sources -- including the fork's .git
#    and its submodule working trees, which the container needs for Vivado). Drop
#    only the heavy regenerated Vivado run dirs and Rust/web build outputs.
echo "== syncing fork into plutosdr-fw/maia-sdr @ $GIT_HASH =="
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
( cd maia-sdr && git submodule update --init --recursive ) 2>/dev/null || \
    echo "WARN: submodule refresh skipped (using committed working-tree sources)"

# 2b. The airband config web page (maia-wasm/assets/airband.html + .js + .css) is
#     a plain static page served by maia-httpd from the rootfs www dir. The
#     maia-sdr buildroot package installs the whole maia-wasm/assets/ tree, so
#     these files ship automatically -- guard against them going missing.
for f in airband.html airband.js airband.css; do
    [ -f "maia-sdr/maia-wasm/assets/$f" ] || \
        echo "WARN: maia-wasm/assets/$f missing from synced fork; /airband.html will not be served"
done

# 3. Devicetree: reset to stock then apply the airband reserved-memory patch
#    (idempotent, relocated to 0x19000000 to avoid the CMA collision). The patch
#    edits the shared maia-sdr overlay (zynq-pluto-sdr-maiasdr.dtsi), which every
#    Pluto-family .dts #includes -- including the Pluto+ (zynq-plutoplus-maiasdr.dts)
#    -- so the airband reserved-memory + rxbuffer nodes carry to all targets.
echo "== patching devicetree =="
DTSI=linux/arch/arm/boot/dts/zynq-pluto-sdr-maiasdr.dtsi
git -C linux checkout -- "arch/arm/boot/dts/zynq-pluto-sdr-maiasdr.dtsi" 2>/dev/null || true
python3 "$AIRBAND_REPO/firmware/apply_airband_devicetree.py" "$DTSI"
# Guard: the airband DT nodes only reach $TARGET if its .dts #includes the shared
# overlay we just patched. Warn loudly if it does not (then the target would boot
# without the maia_sdr_airband reserved region and the receiver could not start).
case "$TARGET" in
    plutoplus) TARGET_DTS="linux/arch/arm/boot/dts/zynq-plutoplus-maiasdr.dts" ;;
    *)         TARGET_DTS="linux/arch/arm/boot/dts/zynq-pluto-sdr-maiasdr.dts" ;;
esac
if [ -f "$TARGET_DTS" ] && ! grep -q 'zynq-pluto-sdr-maiasdr.dtsi' "$TARGET_DTS"; then
    echo "WARNING: $TARGET_DTS does not #include zynq-pluto-sdr-maiasdr.dtsi --"
    echo "         the airband reserved-memory node may be MISSING from this target's DT."
fi

# 3a. Pin a deterministic Ethernet MAC on the Pluto+ (the &gem0 / GEM node).
#     Without a local-mac-address property the Zynq macb driver invents a NEW
#     RANDOM MAC on every boot, so the DHCP lease -- and the device's IP --
#     churns constantly. Baking the MAC into the OS devicetree (this FIT, mtd3)
#     is the durable fix: the kernel reads it natively at probe, with no u-boot
#     env dependency and no in-place FDT growth (which corrupts the FIT and
#     drops the board into DFU). Only the Pluto+ has the Ethernet GEM; the
#     USB-only ADALM-Pluto targets have no gem0 node to patch. Reset to stock
#     first so the value tracks $PLUTO_MAC across rebuilds. Software-only (DT)
#     change -> reflash $TARGET.dfu only; boot.dfu/env untouched.
if [ "$TARGET" = "plutoplus" ] && [ -f "$TARGET_DTS" ]; then
    echo "== pinning Pluto+ Ethernet MAC (${PLUTO_MAC:-02:0a:35:00:01:22}) =="
    git -C linux checkout -- "arch/arm/boot/dts/zynq-plutoplus-maiasdr.dts" 2>/dev/null || true
    python3 "$AIRBAND_REPO/firmware/apply_mac_devicetree.py" \
        "$TARGET_DTS" "${PLUTO_MAC:-02:0a:35:00:01:22}"
fi

# 3b. Auto-start airband: append --airband to the maia-httpd init script so the
#     receiver + audio stream come up on boot (idempotent).
S60=buildroot/board/$TARGET/S60maia-httpd
if [ -f "$S60" ] && ! grep -q -- '--airband' "$S60"; then
    sed -i 's#--ca-cert /mnt/jffs2/maia-sdr-ca.crt#--ca-cert /mnt/jffs2/maia-sdr-ca.crt --airband#' "$S60"
    echo "== patched $S60: maia-httpd now auto-starts with --airband =="
fi

# 3c. Quiet the AD9361 TX on boot (this is a receive-only build). The Pluto comes
#     up in FDD with the TX LO running (2.45 GHz default) at only ~10 dB
#     attenuation, which leaks a carrier + TX-path noise -> EMI to nearby radios
#     and a raised RX noise floor. There is no rx-only ENSM, but RX/TX LOs are
#     independent in FDD, so we power down the TX LO and floor TX attenuation.
#     rootfs is ramfs, so this must live in the init script to survive a power
#     cycle. Idempotent (guarded by the airband-tx-quiet marker).
if [ -f "$S60" ]; then
    python3 "$AIRBAND_REPO/firmware/patch_tx_quiet.py" "$S60"
fi

# 3d. Self-heal maia-httpd on a tight-memory boot. The kernel reserves ~416 MB of
#     the 512 MB for the maia-sdr DMA regions, leaving userspace ~96 MB; maia-httpd's
#     startup footprint sits near that edge, so a transient spike on the FIRST boot
#     after a flash can let the OOM killer take it -- and `start-stop-daemon -b` never
#     respawns, leaving web :8000 / audio :30000 dead until a manual restart. This
#     injects a BOUNDED (~90 s) boot-time retry that relaunches it if it died, then
#     exits (so it never fights the web-UI / lte_calibrate `S60maia-httpd restart`).
#     Idempotent (airband-respawn marker). Software-only init-script change.
if [ -f "$S60" ]; then
    python3 "$AIRBAND_REPO/firmware/patch_maia_respawn.py" "$S60"
fi

# 4. Full build inside the maia-sdr-devel container (Vivado from /opt/Xilinx
#    volume -> HAVE_VIVADO=1). GIT_HASH is exported into the container so the
#    Vivado flow bakes it into the bitstream (USERID + USR_ACCESS).
echo "== full firmware+bitstream build in container (this takes a while) =="
DOCKER_USER="${DOCKER_USER:-0:0}" TARGET="$TARGET" \
    docker compose run --rm -e GIT_HASH="$GIT_HASH" build

# 5. Provenance check: the freshly built bitstream must carry our commit hash.
BIT="$FW/maia-sdr/maia-hdl/projects/$TARGET/$TARGET.runs/impl_1/system_top.bit"
echo "== bitstream provenance =="
if [ -f "$BIT" ]; then
    # Vivado's BITSTREAM.CONFIG.USERID embeds bare uppercase hex in the .bit
    # header (e.g. "UserID=8B601CF0"), not a 0x-prefixed value -- tolerate both.
    EMB="$(strings "$BIT" | grep -oiE 'UserID=(0x)?[0-9A-Fa-f]+' | head -1)"
    EMB_HEX="$(printf '%s' "$EMB" | grep -oiE '[0-9A-Fa-f]+$' | tr 'A-F' 'a-f')"
    echo "   expected UserID=$GIT_HASH ; embedded: ${EMB:-<none>}"
    printf '%s' "$EMB_HEX" | grep -qiE "^0*${GIT_HASH}\$" \
        && echo "   OK: bitstream matches committed fork HEAD" \
        || echo "   WARNING: embedded UserID does not match $GIT_HASH -- DO NOT FLASH until resolved"
else
    echo "   WARNING: $BIT not found; cannot verify provenance"
fi

echo "== artifacts =="
ls -la "$FW"/build/boot.frm "$FW"/build/boot.dfu \
       "$FW"/build/"$TARGET".frm "$FW"/build/"$TARGET".dfu 2>/dev/null || true
echo
echo "Flash BOTH partitions over DFU:"
echo "  dfu-util -a boot.dfu     -D $FW/build/boot.dfu"
echo "  dfu-util -a firmware.dfu -D $FW/build/$TARGET.dfu"
