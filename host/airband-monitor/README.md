# airband-monitor

A small Python counterpart to `airband-listen`: an interactive terminal listener
that streams a single channel's **pre-** or **post-filtered** audio live from the
Raspberry Pi's `airband-reader` monitor endpoint. The channel list (frequencies
and labels) is read automatically from the Pluto, so there is nothing to
configure by hand.

- **pre** tap = raw demodulated audio (continuous, pre-DSP, pre-squelch).
- **post** tap = the fully enhanced, squelch-gated audio LiveATC receives.

## How it fits together

```
Pluto  :8000  GET /api/airband          -> channel plan (freq + label)
Pi     :PORT  GET /listen/<ch>.wav?tap=  -> live mono s16le WAV audio
```

The Pi must be running the reader with its monitor server enabled:

```bash
airband-reader <pluto>:30000 --feeds feeds.json --monitor-port 8082
```

(Only one `airband-reader` may talk to the Pluto's single-client `:30000` at a
time — this tool does **not** open a second Pluto connection; it consumes the
reader's monitor endpoint and only reads the plan metadata from the Pluto.)

## Requirements

- [uv](https://docs.astral.sh/uv/) for the Python environment.
- **PortAudio** for playback (bundled in the `sounddevice` wheels on macOS and
  Windows; on Debian/Raspberry Pi OS install `libportaudio2`:
  `sudo apt install libportaudio2`).

## Install / run

```bash
cd host/airband-monitor
uv sync

# interactive TUI; monitor port defaults to 8082, plan auto-read from the Pluto
uv run airband-monitor rfpi.chongflix.tv --pluto plutoplus.chongflix.tv

# a specific monitor port, plan from a specific Pluto, start on the post tap + channel 6
uv run airband-monitor rf-pi.local:8082 --pluto 10.0.0.20:8000 --tap post --channel 6

# fill in blank labels from a feeds.json, and record to WAV files
uv run airband-monitor rfpi.chongflix.tv --feeds ../../feeds.json --record-dir ./caps

# headless (no UI), just play the selected channel/tap
uv run airband-monitor rfpi.chongflix.tv --no-tui --channel 3
```

## Connection indicators

The header's second line shows live health for the two continuous data sources:

- **Pi audio** — the monitor audio stream: `STREAMING` (green, data flowing),
  `IDLE` (yellow, connected but no bytes for 1.5 s — the stream has **stalled**;
  both taps normally stream continuously, the `post` tap ships silence while
  squelched, so IDLE never means "just quiet"), or `DOWN` (red, not connected).
  After 2.5 s without data the monitor drops the stalled connection and
  reconnects (`status: reconnecting: stream stalled ...`).
- **Pluto squelch** — the reader's `/status` squelch feed: `CONNECTED` (green),
  `DOWN` (red, `/status` unreachable), or `disabled` (no `--metrics-port`).

Channels whose squelch is currently open are marked with `*` and shown in green
in the list, so you can see live activity across the plan. In `--no-tui` mode the
same two states are printed on each status line.

## Keys (TUI)

| Key | Action |
|---|---|
| `up`/`down`, `j`/`k` | previous / next channel |
| digits then `Enter` | jump to a channel index |
| `t` | toggle pre / post tap (playback) |
| `s` | toggle **scan** mode (auto-follow active channels) |
| `r` | toggle recording the tap you're listening to |
| `b` | toggle recording **both** taps at once (pre + post simultaneously) |
| `+` / `-` | volume up / down |
| `m` | mute |
| `q` / `Esc` | quit |

## Scan mode (`s`)

Scan auto-switches the played channel to follow live traffic, so you hear as
much as possible without manually hopping. It polls the reader's `/status`
endpoint (its `--metrics-port`, default `9108`) for each channel's squelch
state, stays on a channel while it is keyed, and after it closes (a short hang
to ride over gaps in speech) hops to the open channel with the strongest
carrier. Recording (`r`/`b`) keeps working and follows the hops. Scan needs the
reader started with `--metrics-port`; disable it with `--metrics-port 0`.

Recordings are written as `<label>_<tap>_<UTCtimestamp>.wav` (mono s16le at the
stream's native rate, i.e. the raw un-volume-adjusted samples). Recording is
independent of playback: `b` opens a separate connection for each tap, so you can
capture the raw demod (`pre`) and the enhanced LiveATC audio (`post`) of the same
channel side by side. Changing channel rolls each recording over to a new file
for the new channel.

## Options

| Flag | Meaning |
|---|---|
| `PI_HOST[:PORT]` | **Required host.** The reader's monitor address; port defaults to `8082` (the committed `--monitor-port`). |
| `--pluto HOST[:PORT]` | Pluto `maia-httpd` for the channel plan (default `192.168.2.1:8000`). |
| `--tap pre\|post` | Initial tap (default `pre`). |
| `--channel N` | Initial channel index (default `0`). |
| `--feeds FILE` | `feeds.json` to fill blank channel labels from per-channel `name`. |
| `--record-dir DIR` | Directory for WAV recordings (default `./out`, created on demand). |
| `--record` | Start recording the initial tap immediately. |
| `--metrics-port N` | Reader's `/status` port for scan mode (default `9108`; `0` disables scan). |
| `--scan` | Start in scan mode (auto-follow active channels). |
| `--no-tui` | Headless playback, no curses UI. |

## Tests

```bash
uv sync          # also installs the `dev` dependency group (pytest)
uv run pytest
```

The suite needs no reader, network or sound card: the worker/recorder tests
drive `MonitorApp` against a fake monitor endpoint (channel rollover, stall
reconnect, intentional switches not reported as errors, `rec_error` clearing),
and the stream tests use a loopback socket server (read timeout, cross-thread
`shutdown()` waking a blocked read).

## Notes / limitations

- The monitor endpoint serves one channel per connection, so the level meter
  shows only the channel you are listening to (unlike `airband-listen`, which
  reads all channels from the Pluto directly). Cross-channel activity comes from
  the reader's `/status` (`--metrics-port`) and is used by scan mode.
- The channel plan is fetched once at startup; restart the tool to pick up a
  changed plan (the receiver itself only applies plan changes on restart).
- Stall detection: every monitor stream is opened with a 2.5 s connect/read
  timeout. Because the reader streams both taps continuously, a read that
  yields nothing for that long means the reader or the network is gone, so the
  monitor reconnects (a running recording rolls over to a new file). Channel
  and tap switches shut the live socket down rather than waiting for the
  current read, so the keys respond immediately even when the reader has died.
- `--no-tui` does not need `curses` (the TUI module is imported only when it
  runs), so headless playback works on Windows without `windows-curses`.
