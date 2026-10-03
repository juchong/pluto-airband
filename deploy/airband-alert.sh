#!/bin/sh
# Failure notification hook for airband-feeds.service. Two callers:
#
#  * ExecStopPost= in airband-feeds.service (the real alerting path). systemd runs
#    it after EVERY stop of the feeder with $SERVICE_RESULT / $EXIT_CODE /
#    $EXIT_STATUS set (systemd.exec(5)). A clean stop or restart
#    (SERVICE_RESULT=success — `systemctl stop/restart`, the watchdog's own
#    recovery restart) exits 0 silently; anything else — crash (exit-code),
#    signal, WatchdogSec timeout (watchdog), start timeout (timeout), OOM kill
#    (oom-kill), core dump — logs and POSTs. This replaces the old
#    OnFailure=airband-alert@%n hook, which could never fire: with Restart=always
#    + StartLimitIntervalSec=0 the unit never enters the "failed" state (systemd
#    only marks a restarting service failed when its start-rate limit trips, and
#    that limit is disabled), so OnFailure= never activated — not even for the
#    2026-09-07 watchdog kill.
#  * airband-alert@<name>.service (manual test: `systemctl start airband-alert@test`,
#    or another unit's OnFailure=). ExecStart= does not get $SERVICE_RESULT, so
#    this path always logs/POSTs, with the instance name as the subject.
#
# POSTs one line to $AIRBAND_ALERT_URL (a webhook or an ntfy topic URL such as
# https://ntfy.sh/your-topic). With no URL set it only logs to the journal, so the
# hook is safe to leave installed before alerting is configured — but then nobody
# is told about a crash; set the URL.
set -eu

unit="${1:-airband-feeds.service}"
result="${SERVICE_RESULT:-}"

# ExecStopPost after a clean stop/restart: nothing to report.
[ "$result" = "success" ] && exit 0

host="$(hostname)"
ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
if [ -n "$result" ]; then
    # e.g. "result=watchdog exit=killed/ABRT", "result=signal exit=killed/KILL",
    # "result=exit-code exit=exited/1", "result=oom-kill exit=killed/KILL".
    msg="[$host] $unit FAILED at $ts (result=$result exit=${EXIT_CODE:-?}/${EXIT_STATUS:-?})"
else
    msg="[$host] $unit FAILED at $ts"
fi

echo "$msg"

[ -n "${AIRBAND_ALERT_URL:-}" ] || exit 0
if ! err=$(curl -fsS -m 10 -H "Title: pluto-airband alert" -d "$msg" "$AIRBAND_ALERT_URL" 2>&1 >/dev/null); then
    echo "airband-alert: POST to AIRBAND_ALERT_URL failed: $err"
fi
