#!/bin/sh
# Install (or update) the deploy layer on the host. Run with sudo from anywhere:
#
#   sudo sh /home/pi/pluto-airband/deploy/install.sh
#
# What it does — idempotent, prints what changed, and never restarts anything:
#   1. installs root-owned copies of airband-watchdog.sh and airband-alert.sh into
#      /usr/local/sbin (the units execute THOSE, not the pi-writable checkout, so
#      a compromise of the pi account cannot run code as root through them);
#   2. copies the three units (airband-feeds, airband-watchdog, airband-alert@)
#      into /etc/systemd/system;
#   3. `systemctl daemon-reload` so systemd sees the new unit files;
#   4. prints the restart/enable steps that are left to you — the feeder restart
#      costs a 35-50 s gap on every feed (18 DeepFilterNet models reload), so it
#      is deliberately a human decision, not a side effect of an install.
#
# `git pull` only updates the checkout. Re-run this after every pull that touches
# deploy/, or the running watchdog/alert scripts and units silently stay stale.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
SBIN=/usr/local/sbin
UNITDIR=/etc/systemd/system
ENVFILE=/etc/airband-feeds.env
SCRIPTS="airband-watchdog.sh airband-alert.sh"
UNITS="airband-feeds.service airband-watchdog.service airband-alert@.service"

if [ "$(id -u)" -ne 0 ]; then
    echo "install.sh: must run as root (sudo sh $0)" >&2
    exit 1
fi

# Refuse to install a script that does not even parse.
for s in $SCRIPTS; do
    sh -n "$here/$s" || { echo "install.sh: $s has a syntax error; nothing installed" >&2; exit 1; }
done

changed=""      # files actually replaced this run
feeds_changed=0
watchdog_changed=0

# put SRC DST MODE: install SRC as DST (root:root, MODE) only if it differs.
put() {
    if cmp -s "$1" "$2"; then
        echo "  unchanged  $2"
    else
        if [ -e "$2" ]; then echo "  updated    $2"; else echo "  installed  $2"; fi
        install -o root -g root -m "$3" "$1" "$2"
        changed="$changed $2"
    fi
}

echo "install.sh: scripts -> $SBIN"
for s in $SCRIPTS; do
    put "$here/$s" "$SBIN/$s" 755
done

echo "install.sh: units -> $UNITDIR"
for u in $UNITS; do
    put "$here/$u" "$UNITDIR/$u" 644
done

case " $changed " in *" $UNITDIR/airband-feeds.service "*) feeds_changed=1 ;; esac
case " $changed " in
    *" $UNITDIR/airband-watchdog.service "*|*" $SBIN/airband-watchdog.sh "*) watchdog_changed=1 ;;
esac

echo "install.sh: systemctl daemon-reload"
systemctl daemon-reload

# Sanity: every unit must still load (a typo in a directive is only a warning to
# systemd, but a broken [Unit]/[Service] section is not).
for u in $UNITS; do
    case "$u" in *@.service) continue ;; esac   # templates cannot be queried uninstantiated
    state=$(systemctl show -p LoadState --value "$u" 2>/dev/null || echo unknown)
    [ "$state" = "loaded" ] || echo "install.sh: WARNING: $u LoadState=$state (see: systemctl status $u)"
done

if [ ! -f "$ENVFILE" ]; then
    echo "install.sh: WARNING: $ENVFILE is missing — the feeder will not start and the"
    echo "  watchdog/alert hooks run with defaults and no AIRBAND_ALERT_URL."
    echo "  Create it from $here/airband-feeds.env.example (root:root, chmod 600)."
elif ! grep -q '^AIRBAND_ALERT_URL=' "$ENVFILE"; then
    echo "install.sh: NOTE: no AIRBAND_ALERT_URL in $ENVFILE — every crash/watchdog/recovery"
    echo "  notification is log-only until you set one."
fi

echo
if [ -z "$changed" ]; then
    echo "install.sh: nothing changed; the installed copies already match deploy/."
else
    echo "install.sh: changed:$changed"
fi
echo
echo "Next steps (nothing has been restarted):"
if [ "$feeds_changed" -eq 1 ]; then
    echo "  * airband-feeds.service changed. Apply with"
    echo "        sudo systemctl restart airband-feeds"
    echo "    (35-50 s gap on all feeds while 18 DeepFilterNet models reload; the"
    echo "    ExecStopPost hook sees SERVICE_RESULT=success for this restart, so no alert)."
    echo "    Then check:  systemctl status airband-feeds ; journalctl -u airband-feeds -n 30"
    echo "                 systemd-analyze security airband-feeds"
fi
if [ "$watchdog_changed" -eq 1 ]; then
    echo "  * airband-watchdog changed (unit and/or script). Apply with"
    echo "        sudo systemctl restart airband-watchdog"
    echo "    (no feed impact; the script is only re-read at restart). Then watch"
    echo "        journalctl -u airband-watchdog -f    # 'starting: ...' then the Pluto ssh check after the grace period"
fi
for u in airband-feeds.service airband-watchdog.service; do
    en=$(systemctl is-enabled "$u" 2>/dev/null || true)
    [ "$en" = "enabled" ] || echo "  * $u is '$en', not enabled at boot:  sudo systemctl enable $u"
done
echo "  * Test the alert path end to end:  sudo systemctl start airband-alert@test"
echo "    then journalctl -u airband-alert@test -n 3 (and your ntfy/webhook)."
echo "  * Verify the watchdog decision logic offline:  sh $here/airband-watchdog-selftest.sh"
