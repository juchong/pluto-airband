#!/bin/sh
# ponytail check for airband-watchdog.sh: exercises the /status JSON parsing
# (jbool) and the recovery decision (classify / is_down) against real snapshot
# shapes. If either breaks, the watchdog either never recovers a dead Pluto or
# falsely escalates to restarting a healthy feeder / rebooting a healthy Pluto —
# the second is exactly the bug the gating replaced (two needless feeder restarts
# on cosmetic :8000 probe misses), so it is worth a runnable check. No framework:
# source the daemon as a lib (AIRBAND_WATCHDOG_LIB=1 returns before the poll loop)
# and assert. Run:  sh deploy/airband-watchdog-selftest.sh
set -eu
here=$(dirname "$0")
AIRBAND_WATCHDOG_LIB=1 . "$here/airband-watchdog.sh"

fail=0
check() { # desc expected actual
    if [ "$2" = "$3" ]; then
        echo "ok:   $1"
    else
        echo "FAIL: $1 (expected '$2', got '$3')"
        fail=1
    fi
}
# down_p STATE -> "down" / "up" (wrapped so set -e cannot abort on is_down's 1).
down_p() { if is_down "$1"; then echo down; else echo up; fi; }

# Real snapshot shapes from GET /status (see host/airband-reader/src/metrics.rs).
healthy='{"pluto_reachable":true,"maia_httpd_up":true,"stream_up":true,"data_flowing":true,"dma_advancing":true,"fpga_overflow":false,"system_healthy":true,"liveatc_healthy":true,"outage":false,"seconds_since_last_sample":0.0,"uptime_secs":2873,"active_channels":18,"total_drops":0}'
down='{"pluto_reachable":false,"maia_httpd_up":false,"stream_up":false,"data_flowing":false,"system_healthy":false}'
# The 2026-09-10 shape: audio flowing, only the 2 s :8000 probe missed (Pi-side
# DNS outage). system_healthy is false, yet nothing is wrong with the capture.
probe_blip='{"pluto_reachable":false,"maia_httpd_up":true,"stream_up":true,"data_flowing":true,"dma_advancing":true,"fpga_overflow":false,"system_healthy":false,"liveatc_healthy":true,"outage":false}'
# Link up but no samples: wedged DMA/FPGA or a dead airband task behind a live socket.
stalled='{"pluto_reachable":true,"maia_httpd_up":true,"stream_up":true,"data_flowing":false,"system_healthy":false}'
# maia-httpd's web port answers but the :30000 stream is gone (airband task died).
stream_down_web_up='{"pluto_reachable":true,"maia_httpd_up":false,"stream_up":false,"data_flowing":false,"system_healthy":false}'
# A 200 from something that is not the reader (wrong AIRBAND_METRICS_URL).
foreign='{"status":"ok"}'

echo "# jbool"
check "healthy system_healthy" true "$(jbool "$healthy" system_healthy)"
check "healthy pluto_reachable" true "$(jbool "$healthy" pluto_reachable)"
# The failure mode this guards: a short key matching inside a longer one.
check "stream_up not confused by maia_httpd_up" true "$(jbool "$healthy" stream_up)"
check "down pluto_reachable" false "$(jbool "$down" pluto_reachable)"
check "down data_flowing" false "$(jbool "$down" data_flowing)"
check "down system_healthy" false "$(jbool "$down" system_healthy)"
# Missing field -> empty (never equals "true"/"false", so never a trigger by itself).
check "missing field -> empty" "" "$(jbool "$healthy" no_such_field)"

echo "# classify (health classification)"
check "healthy snapshot -> healthy" healthy "$(classify "$healthy")"
check "empty body (reader not answering) -> reader-down" reader-down "$(classify "")"
check "everything false -> stream-down" stream-down "$(classify "$down")"
check "web port up, stream gone -> stream-down" stream-down "$(classify "$stream_down_web_up")"
check "link up, no samples -> stalled" stalled "$(classify "$stalled")"
check ":8000 miss while audio flows -> probe-blip" probe-blip "$(classify "$probe_blip")"
check "foreign JSON -> unknown" unknown "$(classify "$foreign")"

echo "# is_down (what counts toward the recovery threshold)"
check "reader-down counts" down "$(down_p reader-down)"
check "stream-down counts" down "$(down_p stream-down)"
check "stalled counts" down "$(down_p stalled)"
check "healthy does not count" up "$(down_p healthy)"
check "probe-blip does NOT count (warn only)" up "$(down_p probe-blip)"
check "unknown does NOT count (warn only)" up "$(down_p unknown)"
# End to end: the exact snapshot that caused the Sep 10 false restart must not count.
check "Sep-10 false-restart snapshot is not down" up "$(down_p "$(classify "$probe_blip")")"

[ "$fail" -eq 0 ] && echo "airband-watchdog selftest PASSED" || echo "airband-watchdog selftest FAILED"
exit "$fail"
