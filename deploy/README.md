# Running the Icecast feeder as a service

This directory holds the assets for running the all-channel Icecast feeder
(`airband-reader --feeds`) unattended on a host — typically a Raspberry Pi at a
tower site — under **systemd**, so it auto-starts on boot and restarts on crash.

| File | What |
|---|---|
| [`airband-feeds.service`](airband-feeds.service) | systemd unit that runs `airband-reader --feeds …`. **Edit it** for your host: the device address, checkout path, user, `--channels`, and squelch flags. |
| [`airband-feeds.env.example`](airband-feeds.env.example) | template for the root-only env file that supplies the Icecast/LiveATC source passwords referenced as `${…}` in `feeds.json`. |

The shipped `airband-feeds.service` is a **working example**, not a fixed
recipe — its `User=`, `WorkingDirectory=`, `ExecStart=` device address, and
`--channels`/`--squelch` flags reflect one deployment. Adjust them to match your
own host, channel count, and squelch preference.

## Prerequisites

- A built `airband-reader` on the host (`cargo build --release -p airband-reader`;
  `airband-listen` needs ALSA and is not built headless).
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

# Build only the streamer.
cargo build --release -p airband-reader

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
sudo cp deploy/airband-feeds.service /etc/systemd/system/
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
on an actual crash.

## Updating a deployment

Commit and push from your workstation, then on the host pull and rebuild. Stop the
service first so the build gets all cores (a running 21-channel reader otherwise
starves the compiler), then restart:

```bash
cd /home/pi/pluto-airband
git pull --ff-only
sudo systemctl stop airband-feeds
cargo build --release -p airband-reader
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
