#!/bin/sh
# Pluto<->Pi capture watchdog + auto-recovery.
#
# WHY THIS EXISTS (the gap it fills): airband-reader keeps petting systemd's
# WatchdogSec even while it is only *reconnecting* to an offline Pluto (see
# host/airband-reader/src/main.rs "Keep the watchdog satisfied across a
# Pluto/network outage ..."). That is deliberate — a healthy-but-waiting reader
# should not be killed — but it means a Pluto that has gone offline (crashed
# maia-httpd, wedged FPGA/DMA, dropped link) is a SILENT outage: systemd never
# restarts anything and no failure hook fires (the feeder is, from systemd's
# point of view, healthy). This daemon watches the reader's own /status heartbeat
# and escalates recovery that systemd cannot: restart the Pi feeder, then
# bounce/reboot the Pluto itself.
#
# It uses the reader's curated /status JSON on the metrics port (the same source
# of truth as the HA dashboard): stream_up (:30000 framed link established),
# data_flowing (a sample seen in the last few seconds), pluto_reachable (the
# Pluto's :8000 web port answered a 2 s probe). No jq dependency — booleans are
# grepped out of the flat JSON.
#
# WHAT COUNTS AS DOWN (see classify below): the capture is down when the reader
# does not answer at all, when stream_up is false, or when data_flowing is false.
# pluto_reachable on its own is NOT a trigger: it is a cosmetic 2 s HTTP probe
# that misses ~20x/day while audio flows perfectly (136 blips in 7 days on rf-pi),
# and keying on system_healthy — which folds that probe in — restarted a healthy
# feeder twice for nothing (once during a Pi-side DNS outage a restart cannot
# fix). A probe-only miss is logged as a warning and never counts toward the
# threshold.
#
# Config comes from the environment: airband-watchdog.service loads
# /etc/airband-feeds.env through EnvironmentFile= (this script deliberately does
# NOT shell-source the file itself — `. file` breaks on values containing '$',
# '#' or spaces, and the unit already provides every variable). Every knob has a
# safe default so an unconfigured install still works. Runs as root (needs
# `systemctl restart` on the feeder and, optionally, `reboot`).
set -u

URL="${AIRBAND_METRICS_URL:-http://127.0.0.1:9108}"          # must match --metrics-port
POLL="${AIRBAND_WATCHDOG_POLL_S:-15}"                        # seconds between probes
THRESH="${AIRBAND_WATCHDOG_FAIL_THRESHOLD:-4}"               # consecutive down probes before acting (4*15s = 60s)
# Minimum seconds between recovery actions, counted from the END of the previous
# action (not its start). Must exceed the feeder's TimeoutStartSec=180: a restart
# blocks for the whole cold DeepFilterNet model load, during which /status is
# down, and the old 180 gave the restarted feeder zero settling time before the
# ladder moved on to bouncing the Pluto.
COOLDOWN="${AIRBAND_WATCHDOG_COOLDOWN_S:-300}"
GRACE="${AIRBAND_WATCHDOG_STARTUP_GRACE_S:-120}"            # quiet period at boot while models load
FEEDS_UNIT="${AIRBAND_WATCHDOG_FEEDS_UNIT:-airband-feeds.service}"
PLUTO_HOST="${AIRBAND_WATCHDOG_PLUTO_HOST:-plutoplus.chongflix.tv}"
PLUTO_PASS="${AIRBAND_WATCHDOG_PLUTO_PASS:-analog}"          # Pluto root password (device default)
REBOOT_PLUTO="${AIRBAND_WATCHDOG_REBOOT_PLUTO:-1}"           # allow maia-httpd bounce + Pluto reboot stages
REBOOT_PI="${AIRBAND_WATCHDOG_REBOOT_PI:-0}"                 # last-resort Pi reboot (off by default)
PLUTO_SSH="${AIRBAND_WATCHDOG_PLUTO_SSH:-}"                  # override the whole ssh command if you use keys
REALERT_MAX=3600  # cap on the exponential "still down" reminder interval (seconds)
SSH_TIMEOUT=60    # wall-clock bound on any one Pluto ssh stage (see pluto_ssh)

# Pluto ssh transport. Every stage is bounded by `timeout` (a wedged Pluto can
# accept the TCP connect and then hang the session; ConnectTimeout alone does not
# cover that) and by keepalives (ServerAlive*: 3 missed 5 s probes = ~15 s to
# notice a dead peer mid-session). The Pluto's host key churns across firmware
# flashes and /root/.ssh has no known_hosts, so host keys are neither checked nor
# recorded (UserKnownHostsFile=/dev/null; this is a LAN device reached with its
# default password — MITM is not the threat model). LogLevel=ERROR drops the
# resulting "Permanently added ..." noise but keeps real errors, which ARE
# captured into the journal (see pluto_run). The password reaches sshpass through
# the environment (SSHPASS + -e), never on the command line, where
# /proc/<pid>/cmdline would expose it to every local user.
SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=5 -o ServerAliveInterval=5 -o ServerAliveCountMax=3"
if [ -n "$PLUTO_SSH" ]; then
    SSH_AUTH=override                 # operator-supplied command, e.g. a key-based `ssh pluto`
elif command -v sshpass >/dev/null 2>&1 && [ -n "$PLUTO_PASS" ]; then
    SSH_AUTH=password                 # sshpass with the device password
else
    SSH_AUTH=keys                     # no sshpass -> key-based BatchMode ssh (needs /root/.ssh set up)
fi

# pluto_ssh CMD...: run CMD on the Pluto as root under the transport above.
pluto_ssh() {
    case "$SSH_AUTH" in
        override)
            # shellcheck disable=SC2086
            timeout "$SSH_TIMEOUT" $PLUTO_SSH "$@" ;;
        password)
            # shellcheck disable=SC2086
            SSHPASS="$PLUTO_PASS" timeout "$SSH_TIMEOUT" sshpass -e ssh $SSH_OPTS "root@$PLUTO_HOST" "$@" ;;
        *)
            # shellcheck disable=SC2086
            timeout "$SSH_TIMEOUT" ssh $SSH_OPTS -o BatchMode=yes "root@$PLUTO_HOST" "$@" ;;
    esac
}

log() { echo "airband-watchdog: $*"; }

# pluto_run DESC CMD...: pluto_ssh with the outcome logged — exit status plus
# whatever ssh or the Pluto printed (stderr included), folded onto one journal
# line — so a stage that fails is diagnosable instead of vanishing into
# ">/dev/null 2>&1". Returns the ssh status (124 = `timeout` fired, 255 = ssh
# could not connect / session dropped).
pluto_run() {
    desc=$1; shift
    out=$(pluto_ssh "$@" 2>&1); rc=$?
    out=$(printf '%s' "$out" | tr '\n' ' ')
    if [ "$rc" -eq 0 ]; then
        log "$desc: ok${out:+ — $out}"
    else
        log "$desc: FAILED rc=$rc${out:+ — $out}"
    fi
    return "$rc"
}

alert() {
    msg="[$(hostname)] airband-watchdog: $* @ $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    log "$msg"
    [ -n "${AIRBAND_ALERT_URL:-}" ] || return 0
    err=$(curl -fsS -m 10 -H "Title: pluto-airband watchdog" -d "$msg" "$AIRBAND_ALERT_URL" 2>&1 >/dev/null) \
        || log "alert POST to AIRBAND_ALERT_URL failed: $err"
}

# Extract a boolean field from the flat /status JSON ("field":true|false). The
# leading quote + trailing colon anchor the exact key, so a short name never
# matches a longer one (e.g. stream_up vs maia_httpd_up); missing field -> empty.
jbool() { printf '%s' "$1" | grep -o "\"$2\":[a-z]*" | head -1 | cut -d: -f2; }

# classify BODY: print the capture state derived from one /status snapshot.
#   healthy      stream_up and data_flowing both true (whatever the :8000 probe says)
#   reader-down  BODY empty: the reader did not answer (crashed, reloading, wedged)
#   stream-down  stream_up=false: the :30000 link is gone (Pluto offline / maia-httpd dead)
#   stalled      stream_up=true but data_flowing=false (wedged DMA/FPGA, dead airband task)
#   probe-blip   audio flows but pluto_reachable=false: the :8000 probe missed — warn only
#   unknown      BODY carries none of the expected fields (wrong AIRBAND_METRICS_URL?) — warn only
# Only reader-down / stream-down / stalled count toward recovery (see is_down).
# Precedence matters: a real outage (stream/data) wins over the probe, and the
# probe never wins on its own.
classify() {
    [ -n "$1" ] || { echo reader-down; return; }
    stream=$(jbool "$1" stream_up)
    flowing=$(jbool "$1" data_flowing)
    pluto=$(jbool "$1" pluto_reachable)
    if [ "$stream" = "false" ]; then
        echo stream-down
    elif [ "$flowing" = "false" ]; then
        echo stalled
    elif [ "$stream" = "true" ] && [ "$flowing" = "true" ]; then
        if [ "$pluto" = "false" ]; then echo probe-blip; else echo healthy; fi
    else
        echo unknown
    fi
}

# is_down STATE: true for the classes that count toward the recovery threshold.
is_down() {
    case "$1" in
        reader-down|stream-down|stalled) return 0 ;;
        *) return 1 ;;
    esac
}

# reason STATE: the operator-facing description used in log and alert lines.
reason() {
    case "$1" in
        reader-down) echo "reader /status endpoint down (reader crashed, reloading or wedged)" ;;
        stream-down) echo "stream down (:30000 link lost — Pluto offline or maia-httpd dead)" ;;
        stalled)     echo "stream stalled (link up but no samples flowing — wedged DMA/FPGA?)" ;;
        *)           echo "capture unhealthy ($1)" ;;
    esac
}

# deploy/airband-watchdog-selftest.sh sources this file with AIRBAND_WATCHDOG_LIB=1
# to exercise jbool + classify/is_down (the parse and the decision whose failure
# would silently misjudge health, or falsely reboot the Pluto) without entering
# the poll loop.
[ -n "${AIRBAND_WATCHDOG_LIB:-}" ] && return 0

log "starting: url=$URL poll=${POLL}s threshold=$THRESH cooldown=${COOLDOWN}s grace=${GRACE}s reboot_pluto=$REBOOT_PLUTO reboot_pi=$REBOOT_PI ssh_auth=$SSH_AUTH"
# Let the just-started feeder load its DeepFilterNet models and connect before we
# judge it (cold model load can take up to the unit's TimeoutStartSec=180s).
sleep "$GRACE"

if [ "$REBOOT_PLUTO" = "1" ]; then
    # One-time reachability check so a broken ssh path (wrong password, dropbear
    # down, sshpass missing) shows up in the journal now, not weeks later when
    # stage 2 is first needed. Informational only — done after the grace period so
    # a Pluto that was itself still booting has had time to come up; the ladder
    # retries for real when it needs to.
    pluto_run "Pluto ssh check (root@$PLUTO_HOST, auth=$SSH_AUTH)" true \
        || log "Pluto-side recovery (stages 2/3) will not work until ssh does; Pi-side restart + alerts still run"
else
    log "Pluto-side recovery disabled (AIRBAND_WATCHDOG_REBOOT_PLUTO=0): ladder is restart-feeder -> alert only"
fi

fails=0              # consecutive down probes
stage=0              # recovery escalation stage (reset to 0 when healthy)
last_action=0        # epoch when the last recovery action FINISHED (cooldown gate)
realert=$COOLDOWN    # current "still down" reminder interval (doubles each time, capped)

while :; do
    sleep "$POLL"

    body=$(curl -fsS -m 5 "$URL/status" 2>/dev/null)
    state=$(classify "$body")

    if ! is_down "$state"; then
        case "$state" in
            probe-blip) log "warning: Pluto :8000 probe missed while audio flows (not counted toward recovery)" ;;
            unknown)    log "warning: /status answered without stream_up/data_flowing (not counted) — is AIRBAND_METRICS_URL the reader?" ;;
        esac
        if [ "$stage" -ne 0 ]; then
            alert "capture recovered — stream up and data flowing again"
        fi
        fails=0
        stage=0
        realert=$COOLDOWN
        continue
    fi

    fails=$((fails + 1))
    why=$(reason "$state")
    log "down $fails/$THRESH: $why"

    [ "$fails" -ge "$THRESH" ] || continue
    # Cooldown gate, measured from the END of the previous action so a feeder
    # restart (which blocks for the whole model load) gets COOLDOWN seconds of
    # settling before the ladder moves on. Once the ladder is exhausted the gate
    # is the growing reminder interval instead, so a long outage stays visible
    # without paging every five minutes.
    if [ "$stage" -ge 4 ]; then wait=$realert; else wait=$COOLDOWN; fi
    [ $(( $(date +%s) - last_action )) -ge "$wait" ] || continue

    case "$stage" in
        0)
            # Cheapest fix: bounce the Pi feeder. Clears a wedged reader and forces
            # a clean reconnect after a transient Pluto/link blip. With Pluto-side
            # recovery disabled the ladder skips straight to the exhausted stage.
            alert "$why — restarting $FEEDS_UNIT (recovery 1/3)"
            systemctl restart "$FEEDS_UNIT" || log "systemctl restart $FEEDS_UNIT failed (rc=$?)"
            if [ "$REBOOT_PLUTO" = "1" ]; then stage=1; else stage=3; fi
            ;;
        1)
            # Feeder restart did not help. If the Pluto is the culprit, bounce its
            # maia-httpd (recovers a crashed daemon without a full reboot).
            alert "$why — restarting maia-httpd on $PLUTO_HOST (recovery 2/3)"
            pluto_run "Pluto maia-httpd restart" '/etc/init.d/S60maia-httpd restart'
            stage=2
            ;;
        2)
            # maia-httpd bounce did not help — reboot the Pluto (recovers a wedged
            # FPGA/DMA or kernel). SSH may still work even when :8000 is dead. The
            # session normally drops as the board goes down, so a non-zero status
            # here is expected; the log line still shows how it ended.
            alert "$why — rebooting Pluto $PLUTO_HOST (recovery 3/3)"
            pluto_run "Pluto reboot" 'reboot'
            stage=3
            ;;
        3)
            # Everything Pluto-side has been tried (or is disabled). Optionally
            # reboot the Pi as a last resort (default off — a Pi reboot cannot fix a
            # dead Pluto and takes the whole feeder offline; enable only if the Pi
            # itself wedges).
            if [ "$REBOOT_PI" = "1" ]; then
                alert "$why — recovery ladder exhausted; rebooting the Pi (last resort)"
                systemctl reboot
            else
                alert "$why — recovery ladder exhausted; still down (Pi reboot disabled), monitoring"
            fi
            stage=4
            ;;
        *)
            # Still down after the full ladder: remind with exponential back-off
            # (COOLDOWN, 2x, 4x, ... capped at REALERT_MAX) so the outage stays
            # visible without hammering the alert channel for its whole duration.
            realert=$((realert * 2))
            [ "$realert" -gt "$REALERT_MAX" ] && realert=$REALERT_MAX
            alert "$why — still down after full recovery ladder (next reminder in ${realert}s)"
            ;;
    esac
    # Stamped AFTER the action completes (a feeder restart blocks for the whole
    # model load), so the cooldown is settling time rather than the action's own
    # duration.
    last_action=$(date +%s)
    fails=0
done
