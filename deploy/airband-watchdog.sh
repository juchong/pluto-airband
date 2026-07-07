#!/bin/sh
# Pluto<->Pi capture watchdog + auto-recovery.
#
# WHY THIS EXISTS (the gap it fills): airband-reader keeps petting systemd's
# WatchdogSec even while it is only *reconnecting* to an offline Pluto (see
# host/airband-reader/src/main.rs "Keep the watchdog satisfied across a
# Pluto/network outage ..."). That is deliberate — a healthy-but-waiting reader
# should not be killed — but it means a Pluto that has gone offline (crashed
# maia-httpd, wedged FPGA/DMA, dropped link) is a SILENT outage: systemd never
# restarts anything and OnFailure= never fires. This daemon watches the reader's
# own /status heartbeat and escalates recovery that systemd cannot: restart the
# Pi feeder, then bounce/reboot the Pluto itself.
#
# It uses the reader's curated /status JSON on the metrics port (the same source
# of truth as the HA dashboard): pluto_reachable (Pluto :8000 answered), stream_up
# (:30000 framed link), data_flowing (a sample seen recently), system_healthy (all
# three). No jq dependency — booleans are grepped out of the flat JSON.
#
# Config comes from /etc/airband-feeds.env (shared with airband-feeds.service);
# every knob has a safe default so an unconfigured install still works. Runs as
# root (needs `systemctl restart` on the feeder and, optionally, `reboot`).
set -u

CONF="${AIRBAND_WATCHDOG_ENV:-/etc/airband-feeds.env}"
# shellcheck disable=SC1090
[ -r "$CONF" ] && . "$CONF"

URL="${AIRBAND_METRICS_URL:-http://127.0.0.1:9108}"          # must match --metrics-port
POLL="${AIRBAND_WATCHDOG_POLL_S:-15}"                        # seconds between probes
THRESH="${AIRBAND_WATCHDOG_FAIL_THRESHOLD:-4}"               # consecutive bad probes before acting (4*15s = 60s)
COOLDOWN="${AIRBAND_WATCHDOG_COOLDOWN_S:-180}"               # min seconds between recovery actions (>= reader TimeoutStartSec)
GRACE="${AIRBAND_WATCHDOG_STARTUP_GRACE_S:-120}"            # quiet period at boot while models load
FEEDS_UNIT="${AIRBAND_WATCHDOG_FEEDS_UNIT:-airband-feeds.service}"
PLUTO_HOST="${AIRBAND_WATCHDOG_PLUTO_HOST:-plutoplus.chongflix.tv}"
PLUTO_PASS="${AIRBAND_WATCHDOG_PLUTO_PASS:-analog}"          # Pluto root password (device default)
REBOOT_PLUTO="${AIRBAND_WATCHDOG_REBOOT_PLUTO:-1}"           # allow maia-httpd bounce + Pluto reboot stages
REBOOT_PI="${AIRBAND_WATCHDOG_REBOOT_PI:-0}"                 # last-resort Pi reboot (off by default)
PLUTO_SSH="${AIRBAND_WATCHDOG_PLUTO_SSH:-}"                  # override the whole ssh command if you use keys

# Build the default Pluto ssh command (sshpass with the device password) unless the
# operator supplied their own (e.g. a key-based `ssh pluto`). If neither sshpass nor
# an override is available, Pluto-side recovery is skipped (Pi-side + alerts only).
if [ -z "$PLUTO_SSH" ]; then
    if command -v sshpass >/dev/null 2>&1 && [ -n "$PLUTO_PASS" ]; then
        PLUTO_SSH="sshpass -p $PLUTO_PASS ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 root@$PLUTO_HOST"
    else
        PLUTO_SSH="ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 -o BatchMode=yes root@$PLUTO_HOST"
    fi
fi

log() { echo "airband-watchdog: $*"; }

alert() {
    msg="[$(hostname)] airband-watchdog: $* @ $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    log "$msg"
    [ -n "${AIRBAND_ALERT_URL:-}" ] || return 0
    curl -fsS -m 10 -H "Title: pluto-airband watchdog" -d "$msg" "$AIRBAND_ALERT_URL" >/dev/null 2>&1 \
        || log "alert POST to AIRBAND_ALERT_URL failed"
}

# Extract a boolean field from the flat /status JSON ("field":true|false).
jbool() { printf '%s' "$1" | grep -o "\"$2\":[a-z]*" | head -1 | cut -d: -f2; }

log "starting: url=$URL poll=${POLL}s threshold=$THRESH cooldown=${COOLDOWN}s grace=${GRACE}s reboot_pluto=$REBOOT_PLUTO reboot_pi=$REBOOT_PI"
# Let the just-started feeder load its DeepFilterNet models and connect before we
# judge it (cold model load can take up to the unit's TimeoutStartSec=180s).
sleep "$GRACE"

fails=0        # consecutive unhealthy probes
stage=0        # recovery escalation stage (reset to 0 when healthy)
last_action=0  # epoch of the last recovery action (cooldown gate)

while :; do
    sleep "$POLL"
    now=$(date +%s)

    body=$(curl -fsS -m 5 "$URL/status" 2>/dev/null)
    if [ -n "$body" ]; then
        healthy=$(jbool "$body" system_healthy)
        pluto=$(jbool "$body" pluto_reachable)
        flowing=$(jbool "$body" data_flowing)
    else
        healthy=""; pluto=""; flowing=""
    fi

    if [ "$healthy" = "true" ]; then
        if [ "$stage" -ne 0 ]; then
            alert "capture recovered — healthy again"
        fi
        fails=0
        stage=0
        continue
    fi

    fails=$((fails + 1))
    if [ -z "$body" ]; then
        reason="reader /status endpoint down (reader crashed or reloading)"
    elif [ "$pluto" = "false" ]; then
        reason="Pluto unreachable (:8000 did not answer)"
    elif [ "$flowing" = "false" ]; then
        reason="stream stalled (Pluto up but no data flowing)"
    else
        reason="capture unhealthy"
    fi
    log "unhealthy $fails/$THRESH: $reason"

    [ "$fails" -ge "$THRESH" ] || continue
    [ $((now - last_action)) -ge "$COOLDOWN" ] || continue

    case "$stage" in
        0)
            # Cheapest fix: bounce the Pi feeder. Clears a wedged reader and forces
            # a clean reconnect after a transient Pluto/link blip.
            alert "$reason — restarting $FEEDS_UNIT (recovery 1/3)"
            systemctl restart "$FEEDS_UNIT" || log "systemctl restart $FEEDS_UNIT failed"
            stage=1
            ;;
        1)
            # Feeder restart did not help. If the Pluto is the culprit, bounce its
            # maia-httpd (recovers a crashed daemon without a full reboot).
            if [ "$REBOOT_PLUTO" = "1" ]; then
                alert "$reason — restarting maia-httpd on $PLUTO_HOST (recovery 2/3)"
                # shellcheck disable=SC2086
                $PLUTO_SSH '/etc/init.d/S60maia-httpd restart' >/dev/null 2>&1 \
                    || log "Pluto maia-httpd restart failed/unreachable"
                stage=2
            else
                stage=3
            fi
            ;;
        2)
            # maia-httpd bounce did not help — reboot the Pluto (recovers a wedged
            # FPGA/DMA or kernel). SSH may still work even when :8000 is dead.
            if [ "$REBOOT_PLUTO" = "1" ]; then
                alert "$reason — rebooting Pluto $PLUTO_HOST (recovery 3/3)"
                # shellcheck disable=SC2086
                $PLUTO_SSH 'reboot' >/dev/null 2>&1 \
                    || log "Pluto reboot failed/unreachable"
                stage=3
            else
                stage=3
            fi
            ;;
        3)
            # Everything Pluto-side has been tried. Optionally reboot the Pi as a
            # last resort (default off — a Pi reboot cannot fix a dead Pluto and
            # takes the whole feeder offline; enable only if the Pi itself wedges).
            if [ "$REBOOT_PI" = "1" ]; then
                alert "$reason — recovery ladder exhausted; rebooting the Pi (last resort)"
                systemctl reboot
            else
                alert "$reason — recovery ladder exhausted; Pluto still down (Pi reboot disabled), monitoring"
            fi
            stage=4
            ;;
        *)
            # Still down after the full ladder: re-alert once per cooldown so the
            # outage stays visible without hammering anything.
            alert "$reason — still down after full recovery ladder"
            ;;
    esac
    last_action=$now
    fails=0
done
