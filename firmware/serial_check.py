#!/usr/bin/env python3
"""Log into the Pluto USB serial console and run diagnostic commands."""
import sys
import time
import serial

PORT = sys.argv[1] if len(sys.argv) > 1 else "/dev/cu.usbmodem104"
ser = serial.Serial(PORT, 115200, timeout=1)


def drain(t=1.5):
    end = time.time() + t
    out = b""
    while time.time() < end:
        n = ser.in_waiting
        if n:
            out += ser.read(n)
            end = time.time() + 0.6
        else:
            time.sleep(0.05)
    return out.decode("utf-8", "replace")


def send(line, wait=1.5):
    ser.write((line + "\n").encode())
    ser.flush()
    return drain(wait)


# Wake the console.
ser.write(b"\n\n")
ser.flush()
banner = drain(2.0)
sys.stdout.write(banner)

low = banner.lower()
# Log in if a login prompt is present.
if "login:" in low or "pluto login" in low:
    sys.stdout.write(send("root", 1.5))
    sys.stdout.write(send("analog", 2.0))
elif "password:" in low:
    sys.stdout.write(send("analog", 2.0))

# Run diagnostics with a marker so we can see clean output.
cmds = [
    "echo MARKER_START",
    "cat /sys/firmware/devicetree/base/model; echo",
    "uname -a",
    "head -1 /proc/meminfo",
    "dmesg | grep -i 'cma:'",
    "grep -iE 'reserved|System RAM' /proc/iomem",
    "ls -d /dev/maia-sdr-* 2>/dev/null; echo ls_rc=$?",
    "pgrep -af maia-httpd | head -1",
    "dmesg | grep -i airband | head",
    "echo MARKER_END",
]
for c in cmds:
    sys.stdout.write(send(c, 2.0))

ser.close()
