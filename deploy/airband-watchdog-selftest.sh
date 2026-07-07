#!/bin/sh
# ponytail check for airband-watchdog.sh: exercises the /status JSON parsing
# (jbool) against real snapshot shapes. If this parse breaks, the watchdog either
# never recovers a dead Pluto or falsely escalates to rebooting a healthy one, so
# it is the one thing worth a runnable check. No framework: source the daemon as a
# lib (AIRBAND_WATCHDOG_LIB=1 returns before the poll loop) and assert.
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

# Real snapshot shapes from GET /status (see host/airband-reader/src/metrics.rs).
healthy='{"pluto_reachable":true,"maia_httpd_up":true,"stream_up":true,"data_flowing":true,"dma_advancing":true,"fpga_overflow":false,"system_healthy":true,"seconds_since_last_sample":0.0}'
down='{"pluto_reachable":false,"maia_httpd_up":false,"stream_up":false,"data_flowing":false,"system_healthy":false}'

check "healthy system_healthy" true "$(jbool "$healthy" system_healthy)"
check "healthy pluto_reachable" true "$(jbool "$healthy" pluto_reachable)"
# The failure mode this guards: a short key matching inside a longer one.
check "stream_up not confused by maia_httpd_up" true "$(jbool "$healthy" stream_up)"
check "down pluto_reachable" false "$(jbool "$down" pluto_reachable)"
check "down data_flowing" false "$(jbool "$down" data_flowing)"
check "down system_healthy" false "$(jbool "$down" system_healthy)"
# Missing field -> empty (never equals "true"/"false", so treated as unhealthy).
check "missing field -> empty" "" "$(jbool "$healthy" no_such_field)"

[ "$fail" -eq 0 ] && echo "airband-watchdog selftest PASSED" || echo "airband-watchdog selftest FAILED"
exit "$fail"
