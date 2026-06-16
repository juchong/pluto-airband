#!/usr/bin/env python3
"""Re-apply the PlutoSDR u-boot environment Maia/airband needs — over serial.

WHY THIS EXISTS
---------------
Flashing the **boot partition** (`boot.dfu` -> `mtd0`, i.e. any HDL/bitstream
change) ships a new u-boot whose env version differs from what is stored in the
env partition (`mtd1`). On the next boot u-boot detects the mismatch and
**resets the environment to the new u-boot's compiled-in defaults**. Those
defaults include the Maia boot args (`uio_pdrv_genirq.of_id`) and `mode=1r1t`,
but they do **NOT** include the device-specific `fw_setenv` customizations:

  * `attr_name` / `attr_val`  -> AD9364 transceiver (else it can mis-probe)
  * `usb_ethernet_mode=ncm`   -> CDC-NCM USB gadget (macOS/iOS need this; the
                                 default is `rndis`, so the Mac never gets a
                                 usb0 IP and the web UI / SSH appear "dead")

So after every `boot.dfu` flash you MUST re-apply these or you waste time
chasing "networking is broken" / "web won't load" / "wrong transceiver". This
is the officially recommended `fw_setenv` method (NOT reflashing the env
partition): https://maia-sdr.org/installation/  (and the browser-based
ADALM-Pluto Setup Utility: https://maia-sdr.org/pluto-setup-tool/).

It runs over the **serial console** on purpose: right after a boot flash the USB
ethernet gadget is in the wrong mode, so SSH/HTTP are unreachable — serial is
the only path in.

USAGE
-----
  # after flashing boot.dfu (device booted to Linux, default usb mode):
  .venv/bin/python firmware/pluto_setup_env.py            # apply + reboot
  .venv/bin/python firmware/pluto_setup_env.py --check    # read-only audit
  .venv/bin/python firmware/pluto_setup_env.py --no-reboot
  .venv/bin/python firmware/pluto_setup_env.py --usb-mode ecm   # Android
  .venv/bin/python firmware/pluto_setup_env.py --ipaddrmulti    # multi-subnet

Requires pyserial (in the repo .venv). Device login defaults to root/analog.
"""
import argparse
import sys
import time

import serial

DEFAULT_PORT = "/dev/cu.usbmodem104"
BAUD = 115200
USER = "root"
PASSWORD = "analog"
SENTINEL = "DONE_SENTINEL_OK"

# Canonical Maia boot-sequence variables (ADALM-Pluto / Pluto+), verbatim from
# https://maia-sdr.org/installation/ and plutosdr-fw build/uboot-env.txt. Each
# embeds `uio_pdrv_genirq.of_id=uio_pdrv_genirq` so the UIO driver binds and
# maia-httpd can start. Only re-applied if missing from the live env.
BOOTSEQ = {
    "ramboot_verbose": (
        "adi_hwref;echo Copying Linux from DFU to RAM... && run dfu_ram;"
        "if run adi_loadvals; then echo Loaded AD936x refclk frequency and "
        "model into devicetree; fi; envversion;setenv bootargs "
        "console=ttyPS0,115200 maxcpus=${maxcpus} rootfstype=ramfs "
        "root=/dev/ram0 rw earlyprintk clk_ignore_unused "
        'uio_pdrv_genirq.of_id=uio_pdrv_genirq uboot="${uboot-version}" && '
        "bootm ${fit_load_address}#${fit_config}"
    ),
    "qspiboot_verbose": (
        "adi_hwref;echo Copying Linux from QSPI flash to RAM... && run read_sf "
        "&& if run adi_loadvals; then echo Loaded AD936x refclk frequency and "
        "model into devicetree; fi; envversion;setenv bootargs "
        "console=ttyPS0,115200 maxcpus=${maxcpus} rootfstype=ramfs "
        "root=/dev/ram0 rw earlyprintk clk_ignore_unused "
        'uio_pdrv_genirq.of_id=uio_pdrv_genirq uboot="${uboot-version}" && '
        "bootm ${fit_load_address}#${fit_config} || echo BOOT failed entering "
        "DFU mode ... && run dfu_sf"
    ),
    "qspiboot": (
        "set stdout nulldev;adi_hwref;test -n $PlutoRevA || gpio input 14 && "
        "set stdout serial@e0001000 && sf probe && sf protect lock 0 100000 && "
        "run dfu_sf;  set stdout serial@e0001000;itest *f8000258 == 480003 && "
        "run clear_reset_cause && run dfu_sf; itest *f8000258 == 480007 && run "
        "clear_reset_cause && run ramboot_verbose; itest *f8000258 == 480006 "
        "&& run clear_reset_cause && run qspiboot_verbose; itest *f8000258 == "
        "480002 && run clear_reset_cause && exit; echo Booting silently && set "
        "stdout nulldev; run read_sf && run adi_loadvals; envversion;setenv "
        "bootargs console=ttyPS0,115200 maxcpus=${maxcpus} rootfstype=ramfs "
        "root=/dev/ram0 rw quiet loglevel=4 clk_ignore_unused "
        'uio_pdrv_genirq.of_id=uio_pdrv_genirq uboot="${uboot-version}" && '
        "bootm ${fit_load_address}#${fit_config} || set stdout "
        "serial@e0001000;echo BOOT failed entering DFU mode ... && sf protect "
        "lock 0 100000 && run dfu_sf"
    ),
}


class Console:
    def __init__(self, port, verbose=True):
        self.ser = serial.Serial(port, BAUD, timeout=0.2)
        self.verbose = verbose

    def close(self):
        self.ser.close()

    def _emit(self, s):
        if self.verbose:
            sys.stdout.write(s)
            sys.stdout.flush()

    def _drain(self, timeout=2.0):
        end = time.time() + timeout
        buf = b""
        while time.time() < end:
            n = self.ser.in_waiting
            if n:
                buf += self.ser.read(n)
                end = time.time() + 0.4
            else:
                time.sleep(0.05)
        return buf.decode(errors="replace")

    def _wait_for(self, needles, timeout=20.0):
        if isinstance(needles, str):
            needles = [needles]
        end = time.time() + timeout
        buf = ""
        while time.time() < end:
            n = self.ser.in_waiting
            if n:
                buf += self.ser.read(n).decode(errors="replace")
                for nd in needles:
                    if nd in buf:
                        return buf, nd
            else:
                time.sleep(0.05)
        return buf, None

    def login(self):
        self.ser.write(b"\n")
        time.sleep(0.4)
        buf = self._drain(1.5)
        self._emit(buf)
        if buf.rstrip().endswith("#"):
            return
        self.ser.write(b"\n")
        buf2, hit = self._wait_for(["login:", "Password:", "# "], timeout=8)
        self._emit(buf2)
        if hit == "login:":
            self.ser.write((USER + "\n").encode())
            b3, h3 = self._wait_for(["Password:", "# "], timeout=8)
            self._emit(b3)
            if h3 == "Password:":
                self.ser.write((PASSWORD + "\n").encode())
                self._emit(self._wait_for(["# "], timeout=10)[0])
        elif hit == "Password:":
            self.ser.write((PASSWORD + "\n").encode())
            self._emit(self._wait_for(["# "], timeout=10)[0])

    def run(self, cmd, timeout=15):
        """Run a shell command; return its stdout (sentinel stripped)."""
        # Type the sentinel split so the echoed command line never contains the
        # literal token (otherwise the wait matches the echo and desyncs).
        full = cmd + '; echo ' + SENTINEL[:5] + '"' + SENTINEL[5:] + '"'
        self.ser.write((full + "\n").encode())
        buf, _ = self._wait_for([SENTINEL], timeout=timeout)
        self._emit(buf)
        # Strip the echoed command line and the sentinel line.
        lines = buf.splitlines()
        out = [ln for ln in lines if SENTINEL not in ln and not ln.strip().startswith("#")]
        return "\n".join(out)


def getenv(con, name):
    out = con.run("fw_printenv -n %s 2>/dev/null || echo __UNSET__" % name)
    val = out.strip().splitlines()[-1].strip() if out.strip() else "__UNSET__"
    return None if val == "__UNSET__" or val == "" else val


def main():
    ap = argparse.ArgumentParser(description="Apply Maia/airband u-boot env over serial.")
    ap.add_argument("--port", default=DEFAULT_PORT)
    ap.add_argument("--usb-mode", default="ncm", choices=["ncm", "ecm", "rndis"],
                    help="USB ethernet gadget mode (macOS/iOS=ncm, Android=ecm, Windows=rndis). Default ncm.")
    ap.add_argument("--ipaddrmulti", action="store_true",
                    help="Also set ipaddrmulti=1 (bind 192.168.x.1 across subnets).")
    ap.add_argument("--check", action="store_true", help="Read-only audit; change nothing.")
    ap.add_argument("--no-reboot", action="store_true", help="Apply but do not reboot.")
    args = ap.parse_args()

    # Desired state.
    desired = {
        "attr_name": "compatible",
        "attr_val": "ad9364",
        "mode": "1r1t",
        "usb_ethernet_mode": args.usb_mode,
    }
    if args.ipaddrmulti:
        desired["ipaddrmulti"] = "1"

    con = Console(args.port)
    changed = False
    try:
        con.login()

        print("\n=== current vs desired ===")
        for k, want in desired.items():
            cur = getenv(con, k)
            ok = (cur == want)
            print("  %-18s = %-12s  (want %s) %s" % (k, cur, want, "OK" if ok else "-> SET"))
            if not ok and not args.check:
                con.run("fw_setenv %s %s" % (k, want))
                changed = True

        # uio_pdrv_genirq boot args: only rewrite the (long) boot-sequence vars
        # if the of_id arg is missing from the live env.
        ofcount = con.run("fw_printenv 2>/dev/null | grep -c uio_pdrv_genirq.of_id=uio_pdrv_genirq").strip().splitlines()
        ofn = int(ofcount[-1]) if ofcount and ofcount[-1].isdigit() else 0
        print("  uio_pdrv_genirq.of_id present in %d/3 boot vars" % ofn)
        if ofn < 3 and not args.check:
            print("  -> restoring boot-sequence vars (ramboot_verbose/qspiboot_verbose/qspiboot)")
            for name, val in BOOTSEQ.items():
                con.run("fw_setenv %s '%s'" % (name, val), timeout=20)
            changed = True

        print("\n=== verify ===")
        con.run("fw_printenv usb_ethernet_mode attr_name attr_val mode 2>&1")
        con.run("fw_printenv 2>/dev/null | grep -c uio_pdrv_genirq.of_id=uio_pdrv_genirq")

        if args.check:
            print("\n[check] read-only; no changes made.")
        elif changed and not args.no_reboot:
            print("\n[apply] rebooting to apply env changes...")
            con.ser.write(b"reboot\n")
            time.sleep(1.0)
            con._emit(con._drain(3.0))
        elif changed:
            print("\n[apply] changes written; reboot required to take effect.")
        else:
            print("\n[apply] nothing to change; env already correct.")
    finally:
        con.close()


if __name__ == "__main__":
    main()
