# Running the Icecast feeder as a service

This directory holds the assets for running the all-channel Icecast feeder
(`airband-reader --feeds`) unattended on a host — typically a Raspberry Pi at a
tower site — under **systemd**, so it auto-starts on boot and restarts on crash.

| File | What |
|---|---|
| [`airband-feeds.service`](airband-feeds.service) | systemd unit that runs `airband-reader --feeds …`. **Edit it** for your host: the device address, checkout path, user, `--channels`, and squelch flags. |
| [`airband-feeds.env.example`](airband-feeds.env.example) | template for the root-only env file that supplies the Icecast/LiveATC source passwords referenced as `${…}` in `feeds.json` (plus optional MQTT/alert settings). |
| [`airband-alert@.service`](airband-alert@.service) | one-shot unit the feeder's `OnFailure=` triggers on crash/watchdog timeout; runs `airband-alert.sh`. |
| [`airband-alert.sh`](airband-alert.sh) | notification hook: POSTs a one-line message to `$AIRBAND_ALERT_URL` (webhook/ntfy), or logs only if that is unset. |

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

Install and start the service (edit `airband-feeds.service` first if your paths,
user, device address, or channel count differ):

```bash
# Install the feeder unit and the OnFailure alert template together.
sudo cp deploy/airband-feeds.service deploy/airband-alert@.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now airband-feeds.service
systemctl status airband-feeds            # state
journalctl -u airband-feeds -f            # live logs + per-channel stats
sudo systemctl restart airband-feeds      # apply a feeds.json edit
sudo systemctl stop airband-feeds         # graceful stop (SIGINT closes feeds)
```

The unit sets `Restart=always` with `StartLimitIntervalSec=0` (never give up) and
`KillSignal=SIGINT` so `stop` triggers the reader's graceful shutdown. Because the
reader reconnects to both the Pluto and Icecast on its own, a restart only fires
on an actual crash or a watchdog timeout.

It also runs as `Type=notify` with `WatchdogSec=30`: the reader signals readiness
once its DeepFilterNet models are loaded and pings the watchdog from its live read
loop, so a process that is *alive but no longer moving data* (a hang) is restarted,
not just one that exits. `MemoryMax=1500M`/`OOMPolicy=kill` cap a runaway leak (well
above normal RSS), and `OnFailure=` fires the alert hook below.

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
link is a **silent outage** — nothing restarts and `OnFailure=` never fires.

`airband-watchdog.service` closes that gap. It polls the reader's `/status`
heartbeat and, when `system_healthy` stays false past a threshold, escalates
recovery on a cooldown (each stage gets time to work before the next):

1. **restart `airband-feeds`** — clears a wedged reader / forces a clean reconnect;
2. **bounce `maia-httpd` on the Pluto** (over SSH) — recovers a crashed daemon
   without a full reboot;
3. **reboot the Pluto** (over SSH) — recovers a wedged FPGA/DMA or kernel.

Every action is announced to `AIRBAND_ALERT_URL` (same hook as the crash alert),
and a "recovered" note fires when `/status` goes healthy again. A last-resort Pi
reboot is available but **off by default** (`AIRBAND_WATCHDOG_REBOOT_PI=0`) — a Pi
reboot can't fix a dead Pluto and takes the whole feeder down. All knobs live in
`/etc/airband-feeds.env` (`AIRBAND_WATCHDOG_*`, see `airband-feeds.env.example`)
and default to safe values; the only prerequisite for Pluto-side recovery is
`sshpass` on the Pi (`sudo apt-get install -y sshpass`) or a key-based
`AIRBAND_WATCHDOG_PLUTO_SSH` override.

```bash
sudo apt-get install -y sshpass                # for the Pluto-bounce/reboot stages
sudo cp deploy/airband-watchdog.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now airband-watchdog.service
journalctl -u airband-watchdog -f              # watch probes + recovery actions
```

### MQTT → Home Assistant

`ExecStart` already passes `--mqtt-broker/--mqtt-user/--mqtt-pass` from the
`AIRBAND_MQTT_*` vars in `/etc/airband-feeds.env`; just fill them in:

```
AIRBAND_MQTT_BROKER=10.0.16.9
AIRBAND_MQTT_USER=plutoplus
AIRBAND_MQTT_PASS=…
```

Leave `AIRBAND_MQTT_BROKER` empty to disable MQTT — the reader treats an empty
broker as "off", so the same unit works with or without a broker.

The reader publishes a retained JSON state topic (`pluto-airband/state`) and Home
Assistant **MQTT discovery** configs once per connect, so the entities — including
the two headline tiles **Capture healthy** (`system_healthy`) and **LiveATC
healthy** (`liveatc_healthy`), plus `pluto_reachable`, `maia_httpd_up`,
`data_flowing`, and the Pluto FPGA flags `dma_advancing` / `fpga_overflow` — 
auto-appear in HA with no manual YAML. A Last Will
flips `pluto-airband/availability` to `offline` the instant the feeder dies, so the
whole dashboard greys out on a crash or Pi outage. Add `--mqtt-per-channel` for the
(noisier) per-channel open/carrier entities.

### Failure alerting

`airband-feeds.service` declares `OnFailure=airband-alert@%n.service`. Install the
template (done above) and set `AIRBAND_ALERT_URL` in `/etc/airband-feeds.env` to a
webhook or ntfy topic; the hook POSTs a one-line message on any crash or watchdog
timeout. Left unset, it logs to the alert unit's journal only. Test it with
`systemctl start airband-alert@test.service`.

## Updating a deployment

Commit and push from your workstation, then on the host pull and rebuild. Stop the
service first so the build gets all cores (a running 18-channel reader otherwise
starves the compiler), then restart:

```bash
cd /home/pi/pluto-airband
git pull --ff-only
# If a unit FILE changed (e.g. --channels/--rate, or the alert template), reinstall
# it first — git pull only updates the repo copy, not the one under /etc/systemd/system:
#   sudo cp deploy/airband-feeds.service deploy/airband-alert@.service /etc/systemd/system/ && sudo systemctl daemon-reload
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
