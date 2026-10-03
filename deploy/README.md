# Running the Icecast feeder as a service

This directory holds the assets for running the all-channel Icecast feeder
(`airband-reader --feeds`) unattended on a host — typically a Raspberry Pi at a
tower site — under **systemd**, so it auto-starts on boot and restarts on crash.

| File | What |
|---|---|
| [`airband-feeds.service`](airband-feeds.service) | systemd unit that runs `airband-reader --feeds …`. **Edit it** for your host: the device address, checkout path, user, `--channels`, and squelch flags. |
| [`airband-feeds.env.example`](airband-feeds.env.example) | template for the root-only env file that supplies the Icecast/LiveATC source passwords referenced as `${…}` in `feeds.json` (plus optional MQTT/alert settings). |
| [`airband-alert.sh`](airband-alert.sh) | failure hook run by the feeder's `ExecStopPost=` after every stop: silent on a clean stop/restart (`$SERVICE_RESULT=success`), otherwise POSTs a one-line message to `$AIRBAND_ALERT_URL` (webhook/ntfy), or logs only if that is unset. |
| [`airband-alert@.service`](airband-alert@.service) | one-shot template around `airband-alert.sh`; the manual test harness (`systemctl start airband-alert@test`). No longer wired to the feeder — its old `OnFailure=` could never fire, see *Failure alerting*. |
| [`airband-watchdog.service`](airband-watchdog.service) | root daemon that polls the reader's `/status` and escalates recovery (restart feeder → bounce `maia-httpd` → reboot Pluto) when the stream is really down; see *Auto-recovery watchdog*. |
| [`airband-watchdog.sh`](airband-watchdog.sh) | the watchdog poll loop itself (POSIX sh; its knobs come from the env file through the unit's `EnvironmentFile=`). |
| [`airband-watchdog-selftest.sh`](airband-watchdog-selftest.sh) | offline check of the watchdog's JSON parse and down/not-down decision against real `/status` shapes: `sh deploy/airband-watchdog-selftest.sh`. |
| [`install.sh`](install.sh) | `sudo sh deploy/install.sh`: installs root-owned copies of the two scripts into `/usr/local/sbin` and the three units into `/etc/systemd/system`, `daemon-reload`s, and prints what changed and what to restart. Idempotent; never restarts anything itself. |

The shipped `airband-feeds.service` is a **working example**, not a fixed
recipe — its `User=`, `WorkingDirectory=`, `ExecStart=` device address, and
`--channels`/`--squelch` flags reflect one deployment. Adjust them to match your
own host, channel count, and squelch preference.

## Prerequisites

- A built `airband-reader` on the host (`cargo build --release --manifest-path
  host/Cargo.toml -p airband-reader` — the Cargo workspace is `host/`; there is no
  root manifest, so a bare `cargo build` from the checkout root fails with "could
  not find Cargo.toml". `airband-listen` needs ALSA and is not built headless).
- A reachable Pluto+ streaming on `:30000` (see the main [`README.md`](../README.md)).
- A `feeds.json` describing which channel feeds which Icecast mount (schema in the
  main README → *Stream to Icecast / LiveATC*). Keep passwords out of it — write
  them as `${AIRBAND_…}` and supply them from the env file below.

## Initial setup (one time)

Deploy as a plain **git checkout** built **on the host** — never `rsync` source
onto it, so `git pull` always works and the tree stays in a known state. The
example unit assumes the `pi` user and `/home/pi/pluto-airband`; change both if
your host differs.

```bash
git clone https://github.com/juchong/pluto-airband.git /home/pi/pluto-airband
cd /home/pi/pluto-airband

# Build only the streamer. The Cargo workspace is host/ (no root manifest). A clean
# build pulls deep_filter + deps — tens of minutes on a Pi — so run it detached and
# logged, then poll the log, rather than holding the session (a dropped SSH or a
# command-timeout would otherwise strand it). cargo is NOT on a non-interactive SSH
# PATH, so source its env first.
nohup sh -c '. "$HOME/.cargo/env"; cargo build --release --manifest-path host/Cargo.toml -p airband-reader; echo "BUILD_EXIT=$?"' > /tmp/reader_build.log 2>&1 &
until grep -q '^BUILD_EXIT=' /tmp/reader_build.log; do sleep 10; done   # wait for completion
tail -3 /tmp/reader_build.log    # expect: "Finished `release`" then BUILD_EXIT=0

# Supply the secrets that feeds.json references as ${AIRBAND_*}. The committed
# feeds.json carries no passwords, so it is safe to commit and pull; the real
# passwords live only in a root-only env file (never committed; *.env is gitignored).
sudo cp deploy/airband-feeds.env.example /etc/airband-feeds.env
sudo $EDITOR /etc/airband-feeds.env        # fill in the AIRBAND_* passwords
sudo chown root:root /etc/airband-feeds.env && sudo chmod 600 /etc/airband-feeds.env
```

Install and start the services (edit `airband-feeds.service` first if your paths,
user, device address, or channel count differ):

```bash
# install.sh puts root-owned copies of airband-watchdog.sh + airband-alert.sh in
# /usr/local/sbin (the units execute THOSE, never the pi-writable checkout), copies
# the three units into /etc/systemd/system, daemon-reloads, and prints what
# changed. It never restarts anything itself.
sudo sh deploy/install.sh
sudo systemctl enable --now airband-feeds.service airband-watchdog.service
systemctl status airband-feeds            # state
journalctl -u airband-feeds -f            # live logs (one-line summary per minute; add --stats-table for the 5 s table)
sudo systemctl restart airband-feeds      # apply a feeds.json edit (35-50 s gap on every feed: 18 DFN models reload)
sudo systemctl stop airband-feeds         # stop (plain SIGTERM; see below)
```

The unit sets `Restart=always` with `StartLimitIntervalSec=0` (never give up).
`stop` uses systemd's default `SIGTERM`: the reader installs **no** signal handler,
so there is no graceful shutdown to trigger — it simply exits and both Icecast
servers drop the mounts within seconds (an earlier `KillSignal=SIGINT` "graceful
stop" claim here was wrong). Because the reader reconnects to both the Pluto and
Icecast on its own, a restart only fires on an actual crash or a watchdog timeout.

It also runs as `Type=notify` with `WatchdogSec=30`: the reader signals readiness
once its DeepFilterNet models are loaded and pets the watchdog from its heartbeat
keeper for as long as the router is making progress (samples arriving, or
reconnect attempts cycling), so a process that is *alive but no longer moving
data* (a hang) is restarted, not just one that exits. Every stop runs the
`ExecStopPost=` alert hook (see *Failure alerting*). The unit also carries
`Nice=-10`/`CPUWeight=1000` (the real-time DSP wins CPU over the ADS-B
containers), `OOMScoreAdjust=-500`, and a conservative systemd sandbox
(`ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateDevices`, … — the reader
writes nothing in `--feeds` mode; add `ReadWritePaths=` for the `--out-dir` if you
ever add `--mode wav`). `MemoryMax=1500M`/`MemorySwapMax=0`/`OOMPolicy=kill` guard
against a runaway leak, **but are inert on a kernel booted with
`cgroup_disable=memory`** — as `rf-pi` is today (`cat
/sys/fs/cgroup/cgroup.controllers` lacks `memory`); they take effect once the
memory controller is enabled in `cmdline.txt` and the Pi rebooted.

## Monitoring, health & debugging

The reader exposes everything needed to answer "is reliable audio reaching
LiveATC?" without extra daemons:

- **Prometheus + health probes** (`--metrics-port 9108`, in the example `ExecStart`):
  - `http://<pi>:9108/metrics` — per-channel and per-feed counters/gauges, plus the
    pipeline-health gauges `airband_pluto_reachable`, `airband_link_up`,
    `airband_data_flowing`, `airband_seconds_since_last_sample`,
    `airband_system_healthy`, `airband_liveatc_healthy`, and the Pluto-side FPGA
    flags `airband_dma_advancing` / `airband_fpga_overflow` (from `GET /api/health`).
  - `http://<pi>:9108/healthz` — returns **200** only when the Pluto is reachable,
    the stream is up, samples are flowing, and every feed is connected; **503**
    otherwise. Use it for an uptime check.
  - `http://<pi>:9108/status` — the curated JSON snapshot (same data the MQTT
    publisher sends).
- **The three Pluto questions** are derived purely Pi-side (no Pluto agent):
  *connected?* = a periodic `GET /api/health` probe of the Pluto web port
  (`--pluto-web-port`, default 8000); *application running?* = the `:30000` stream
  is established; *data flowing?* = a sample arrived in the last few seconds. A dead
  stream while the web port still answers pinpoints a died airband task vs. a down
  board. That same probe folds the Pluto's FPGA flags `dma_advancing` /
  `fpga_overflow` into `/status` + MQTT (older firmware without the endpoint keeps
  the benign defaults: advancing=true, overflow=false).
- **Low-latency debug listen** (`--monitor-port 8082`): listen to one channel live,
  in the same process as the feeder (no second Pluto connection, no Icecast lag):

  ```bash
  # pre  = raw demod (continuous, matches the waterfall); lowest latency
  ffplay -fflags nobuffer -flags low_delay -probesize 32 -analyzeduration 0 \
    http://<pi>:8082/listen/3.wav?tap=pre
  # post = the enhanced, squelch-gated audio byte-identical to what LiveATC gets
  ffplay -nodisp http://<pi>:8082/listen/3.wav?tap=post
  ```

  Change the channel by changing the URL — no restart. End-to-end latency is just
  the client's buffer (~100–200 ms), versus 30–60 s through Icecast.

### Auto-recovery watchdog (`airband-watchdog.service`)

The feeder's own `WatchdogSec=30` restarts a *hung* reader, but it does **not**
cover a Pluto that goes offline: the reader keeps petting the systemd watchdog
while it reconnects (by design — a healthy reader waiting for the board back
shouldn't be killed), so a crashed `maia-httpd`, a wedged FPGA/DMA, or a dropped
link is a **silent outage** — nothing restarts and, since the feeder never stops,
the `ExecStopPost=` alert hook never runs either.

`airband-watchdog.service` closes that gap. It polls the reader's `/status`
heartbeat every 15 s and classifies each snapshot: **down** when `/status` does not
answer (reader crashed or reloading), when `stream_up` is false (the `:30000` link
is gone — Pluto offline or `maia-httpd` dead), or when `data_flowing` is false
(link up, no samples — wedged DMA/FPGA). After 4 consecutive down probes (60 s) it
escalates recovery on a 300 s cooldown counted from the *end* of the previous
action, so a restarted feeder gets settling time past its 180 s start timeout
before the next stage:

1. **restart `airband-feeds`** — clears a wedged reader / forces a clean reconnect;
2. **bounce `maia-httpd` on the Pluto** (over SSH) — recovers a crashed daemon
   without a full reboot;
3. **reboot the Pluto** (over SSH) — recovers a wedged FPGA/DMA or kernel.

What does **not** trigger it: `pluto_reachable=false` on its own. That is the
reader's 2 s HTTP probe of the Pluto's `:8000` web port, and it misses ~20×/day
while audio flows perfectly (136 blips in 7 days on `rf-pi`); the previous gate on
`system_healthy` — which folds that probe in — restarted a healthy feeder twice
for nothing (once during a Pi-side DNS outage a restart cannot fix). A probe-only
miss is logged as a warning and never counted. Once the ladder is exhausted the
"still down" reminder backs off exponentially (300 s, 600 s, … capped at 1 h) so a
long outage stays visible without paging every five minutes.

Every action is announced to `AIRBAND_ALERT_URL` (same hook as the crash alert),
and a "recovered" note fires when the stream is up and flowing again. A last-resort
Pi reboot is available but **off by default** (`AIRBAND_WATCHDOG_REBOOT_PI=0`) — a
Pi reboot can't fix a dead Pluto and takes the whole feeder down. All knobs live in
`/etc/airband-feeds.env` (`AIRBAND_WATCHDOG_*`, see `airband-feeds.env.example`),
reach the script through the unit's `EnvironmentFile=` (the script does not source
the file itself), and default to safe values. The Pluto-side stages (2 and 3) need
`sshpass` on the Pi (`sudo apt-get install -y sshpass`) or a key-based
`AIRBAND_WATCHDOG_PLUTO_SSH` override; every ssh stage is bounded by `timeout 60`
plus keepalives, ignores the Pluto's churning host key, passes the password only
through the environment, and logs ssh's stderr to the journal. After its startup
grace period the watchdog runs one `ssh … true` against the Pluto and logs
`Pluto ssh check …: ok` or the exact failure — keep
`AIRBAND_WATCHDOG_REBOOT_PLUTO=0` (restart-feeder + alert only) until that line
reads ok.

```bash
sudo apt-get install -y sshpass                # for the Pluto-bounce/reboot stages
sudo sh deploy/install.sh                      # script -> /usr/local/sbin, unit -> /etc/systemd/system (if not done above)
sudo systemctl enable --now airband-watchdog.service
journalctl -u airband-watchdog -f              # 'starting: …', the ssh check, then probes + recovery actions
sh deploy/airband-watchdog-selftest.sh         # offline check of the parse + down/not-down decision
```

### MQTT → Home Assistant

`ExecStart` already passes `--mqtt-broker/--mqtt-user` from the `AIRBAND_MQTT_*`
vars in `/etc/airband-feeds.env`, and the reader reads the password straight from
the `AIRBAND_MQTT_PASS` environment variable (never put it on the command line:
`/proc/<pid>/cmdline` is world-readable, so a `--mqtt-pass` flag would leak it to
every local user and to `systemctl status`). Just fill them in:

```
AIRBAND_MQTT_BROKER=10.0.16.9
AIRBAND_MQTT_USER=plutoplus
AIRBAND_MQTT_PASS=…
```

Leave `AIRBAND_MQTT_BROKER` empty to disable MQTT — the reader treats an empty
broker as "off", so the same unit works with or without a broker.

The reader publishes a retained JSON state topic (`pluto-airband/state`) and Home
Assistant **MQTT discovery** configs once per connect, so the entities — including
the consolidated **Outage** tile (`outage`, `device_class: problem`), the two
headline tiles **Capture healthy** (`system_healthy`) and **LiveATC healthy**
(`liveatc_healthy`), plus `pluto_reachable`, `maia_httpd_up`, `data_flowing`, and
the Pluto FPGA flags `dma_advancing` / `fpga_overflow` — auto-appear in HA with no
manual YAML. On a Raspberry Pi the reader also publishes the host's cooling state:
**Host thermal problem** (`thermal_problem`, `device_class: problem` — on when no fan
is detected, the SoC is at the firmware's throttling temperature (80 °C), or a
present fan reads 0 rpm above 70 °C), **Fan detected** (`fan_detected`; the Pi 5
firmware only creates the fan device when one is plugged into the header),
**CPU temperature** (`cpu_temp_c`) and **Fan speed** (`fan_rpm`). A **Last alert**
text sensor (`last_alert`) carries the most recent line from the alert hooks (see
*Failure alerting*). A Last Will flips `pluto-airband/availability` to `offline` the instant
the feeder dies, so the whole dashboard greys out on a crash or Pi outage. Add
`--mqtt-per-channel` for the (noisier) per-channel open/carrier entities.

**Outage notifications.** `outage` is the single signal to alert on: it is `on`
whenever the **Pluto→Pi capture** is unhealthy (Pluto unreachable, stream down, or
no samples flowing) **or** any **output feed** is down — including the previously
silent case where a feed socket stays connected but ships dead air because the
Pluto is off (now `liveatc_healthy` also requires data to be flowing). It is
**debounced by 30 s** — the underlying condition must persist continuously that
long before `outage` flips `on` — so a routine feed reconnect or a brief
data-flow gap never pulses the flag (which would otherwise fire a spurious
"recovered" a minute later). The granular tiles (`system_healthy`,
`liveatc_healthy`, per-feed `connected`) and `/healthz` stay instantaneous for
diagnostics. A total
**reader/Pi outage** is caught out-of-band: the MQTT Last-Will marks every entity
`unavailable`. One automation covers both:

```yaml
automation:
  - alias: Pluto Airband outage alert
    trigger:
      - trigger: state
        entity_id: binary_sensor.pluto_airband_outage
        to: "on"                 # capture/feed outage (Pluto or feeds down)
      - trigger: state
        entity_id: binary_sensor.pluto_airband_outage
        to: "unavailable"        # reader/Pi down (Last-Will offline)
        for: "00:02:00"          # ride out a brief reader restart
    action:
      - action: notify.notify
        data:
          title: "Pluto Airband outage"
          message: >-
            {{ 'Reader/Pi offline' if trigger.to_state.state == 'unavailable'
               else 'Capture or feed outage (check Pluto reachable / data flowing)' }}
```

### Failure alerting

`airband-feeds.service` runs `ExecStopPost=/usr/local/sbin/airband-alert.sh %n`
after **every** stop. systemd hands the script `$SERVICE_RESULT` / `$EXIT_CODE` /
`$EXIT_STATUS`: on `success` (a clean `systemctl stop`/`restart`, including the
watchdog's own recovery restart) it exits silently; on anything else — crash
(`exit-code`), `signal`, `watchdog` timeout, `oom-kill`, start `timeout` — it logs
to the feeder's journal and delivers one line two ways:

1. **MQTT → Home Assistant (default when the broker is configured).** With
   `AIRBAND_MQTT_BROKER` set in `/etc/airband-feeds.env` the script (and the
   watchdog's recovery actions) publish the line retained to
   `pluto-airband/last_alert`. The reader announces a discovery entity for it,
   `sensor.pluto_airband_last_alert`, deliberately **without** an availability
   binding, so HA shows the text even while the reader itself is dead. Nothing to
   configure beyond the MQTT credentials you already have. Trigger a notification
   on it (and on the host thermal flag) with:

   ```yaml
   automation:
     - alias: Pluto Airband alert
       trigger:
         - trigger: state
           entity_id: sensor.pluto_airband_last_alert
           not_to: ["unknown", "unavailable"]
         - trigger: state
           entity_id: binary_sensor.pluto_airband_host_thermal_problem
           to: "on"
           for: "00:05:00"          # fan unplugged / stalled, or SoC at the throttle point
       action:
         - action: notify.notify
           data:
             title: "Pluto Airband"
             message: >-
               {{ trigger.to_state.state if trigger.entity_id.startswith('sensor.')
                  else 'Host thermal problem: no fan detected, fan stalled, or CPU at the throttling temperature' }}
   ```

2. **Webhook / ntfy (optional).** If `AIRBAND_ALERT_URL` is also set, the same
   line is POSTed there (`curl -m 10`). Useful for a phone push via
   [ntfy](https://ntfy.sh) without going through HA.

With neither configured everything is log-only and a crash loop or a dead Pluto
goes unnoticed.

Why not `OnFailure=`: the unit used to declare `OnFailure=airband-alert@%n.service`,
but with `Restart=always` + `StartLimitIntervalSec=0` a service never enters the
`failed` state (systemd only marks a restarting unit failed when its start-rate
limit trips, and that limit is disabled), so the hook never fired — not even for
the 2026-09-07 watchdog kill. `airband-alert@.service` is kept as the manual test
harness: `sudo systemctl start airband-alert@test.service` sends a `test FAILED`
line through the same script and URL.

## Updating a deployment

Commit and push from your workstation, then on the host pull and rebuild. Stop the
service first so the build gets all cores (a running 18-channel reader otherwise
starves the compiler), then restart:

```bash
cd /home/pi/pluto-airband
git pull --ff-only
# If anything under deploy/ changed, reinstall it — git pull only updates the
# checkout, not the unit copies under /etc/systemd/system nor the root-owned script
# copies under /usr/local/sbin that the units actually execute:
sudo sh deploy/install.sh              # idempotent; prints what changed + what to restart (restarts nothing)
#   -> "airband-watchdog changed":      sudo systemctl restart airband-watchdog   (no feed impact)
#   -> "airband-feeds.service changed": the stop/start below applies it
sudo systemctl stop airband-feeds      # free all cores for the build (no-op if already stopped)

# Rebuild detached + logged, then poll the log (survives SSH drops / command-timeouts).
# Workspace is host/ (no root manifest); cargo isn't on a non-interactive SSH PATH:
nohup sh -c '. "$HOME/.cargo/env"; cargo build --release --manifest-path host/Cargo.toml -p airband-reader; echo "BUILD_EXIT=$?"' > /tmp/reader_build.log 2>&1 &
until grep -q '^BUILD_EXIT=' /tmp/reader_build.log; do sleep 10; done
tail -3 /tmp/reader_build.log          # expect: "Finished `release`" then BUILD_EXIT=0

sudo systemctl start airband-feeds
journalctl -u airband-feeds -n 30 --no-pager   # confirm "ready" + all mounts connect
```

`feeds.json` carries no secrets, so `git pull` updates it cleanly; only the
`cargo` artifacts under `host/target/` are reused for an incremental build. To
change a password, edit `/etc/airband-feeds.env` and `systemctl restart
airband-feeds` — no pull or rebuild needed.

## One reader per Pluto

The Pluto serves a **single client** on `:30000`; running several
`airband-reader` instances against it at once makes them fight over the socket and
can wedge `maia-httpd` (recover with `ssh root@<pluto>
/etc/init.d/S60maia-httpd restart`). The systemd unit enforces a single instance —
don't also run a manual reader against the same device. When cleaning up stray
readers, match by process **name** (`pkill -x airband-reader`), not `pkill -f`
(which self-matches the shell running it).
