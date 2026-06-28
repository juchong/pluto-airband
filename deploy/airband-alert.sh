#!/bin/sh
# Notification hook fired by systemd OnFailure= when airband-feeds crashes or its
# watchdog trips. POSTs a one-line message to $AIRBAND_ALERT_URL (a webhook or an
# ntfy topic URL such as https://ntfy.sh/your-topic). With no URL set it just logs
# to the alert unit's journal, so the OnFailure hook is safe to leave installed
# even before alerting is configured.
set -eu

unit="${1:-airband-feeds.service}"
host="$(hostname)"
ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
msg="[$host] $unit FAILED at $ts"

echo "$msg"

[ -n "${AIRBAND_ALERT_URL:-}" ] || exit 0
if ! curl -fsS -m 10 -H "Title: pluto-airband alert" -d "$msg" "$AIRBAND_ALERT_URL" >/dev/null 2>&1; then
    echo "airband-alert: POST to AIRBAND_ALERT_URL failed"
fi
