"""Live listener core: playback worker, per-tap recorders, /status poller.

Runs one streaming worker thread that connects to the Pi monitor endpoint for
the currently-selected channel + tap, plays it through the default output
device, meters the level, and optionally records the raw stream to a WAV. The
main thread drives the curses UI (``tui.py``) or just idles in ``--no-tui``
mode. Switching channel or tap bumps a generation counter and shuts down the
live stream's socket, which wakes the worker out of a blocked ``read()`` so it
reconnects with the new parameters at once.

Both monitor taps stream bytes continuously, so a read that yields nothing for
``stream.STREAM_TIMEOUT_S`` means the reader (or the network) is gone; the
worker treats that ``TimeoutError`` as a stall and reconnects.
"""

from __future__ import annotations

import array
import os
import socket
import threading
import time
import wave
from datetime import datetime, timezone

from .plan import Channel
from .stream import STREAM_TIMEOUT_S, fetch_status, monitor_url, open_stream, peak_dbfs

# Playback needs PortAudio (via sounddevice). Import it tolerantly so this
# module loads without it (tests, --help); cli.main() refuses to start when it
# is missing and the worker reports it instead of crashing.
AUDIO_BACKEND_ERROR: str | None = None
try:
    import sounddevice as sd
except (ImportError, OSError) as _e:  # e.g. "PortAudio library not found"
    sd = None  # type: ignore[assignment]
    AUDIO_BACKEND_ERROR = str(_e)

VOL_STEP = 0.1
VOL_MAX = 4.0
STATUS_POLL_S = 1.0        # /status poll period with scan off (health indicator only)
SCAN_POLL_S = 0.3          # /status poll period while scanning (bounds hop latency)
SCAN_HANG_S = 1.5          # stay on a channel this long after it closes before hopping
RECONNECT_BACKOFF_S = 1.0  # pause before reconnecting after a real error or stall

# socket.timeout has been an alias of TimeoutError since Python 3.10; both are
# named so the intent is clear whichever one a reader expects.
_STALL_ERRORS = (TimeoutError, socket.timeout)


def choose_scan_target(
    status_channels: list[dict], current: int, idle_secs: float, hang: float
) -> int | None:
    """Scanner decision (pure, so it is unit-testable): given the reader's
    per-channel ``/status`` list, the current channel, how long the current
    channel has been closed, and the hang time, return the channel to hop to, or
    ``None`` to stay put.

    Stay while the current channel is keyed (open) or still within the hang
    window; otherwise hop to the open channel with the strongest carrier."""
    if any(c.get("ch") == current and c.get("open") for c in status_channels):
        return None  # current is active — ride the transmission out
    if idle_secs < hang:
        return None  # brief gap in speech — do not hop yet
    opens = [c for c in status_channels if c.get("open")]
    if not opens:
        return None
    best = max(opens, key=lambda c: c.get("carrier_dbc", float("-inf")))
    ch = best.get("ch")
    return ch if ch != current else None


def _safe_name(label: str, index: int) -> str:
    """Filesystem-safe stem from a channel label (fallback to the index)."""
    stem = "".join(c if c.isalnum() or c in "-_." else "_" for c in label).strip("_")
    return stem or f"ch{index:02d}"


def _abort_stream(stream) -> None:
    """Wake another thread out of a blocked ``stream.read()``.

    ``close()`` from a second thread does not interrupt a ``recv()`` in
    progress (it blocks on the response's buffer lock until the read returns),
    so shut the socket down instead: the reader sees EOF or an error at once
    and closes the stream itself in its ``finally``. Objects without a
    ``shutdown()`` are simply closed."""
    if stream is None:
        return
    fn = getattr(stream, "shutdown", None)
    if fn is None:
        fn = getattr(stream, "close", None)
    if fn is None:
        return
    try:
        fn()
    except Exception:  # noqa: BLE001 (already gone is fine)
        pass


def _describe_error(e: BaseException, connected: bool) -> str:
    """Human-readable cause for the status line / rec_error."""
    if isinstance(e, _STALL_ERRORS):
        if connected:
            return f"stream stalled (no data for {STREAM_TIMEOUT_S:g} s)"
        return f"connect timed out after {STREAM_TIMEOUT_S:g} s"
    return str(e) or e.__class__.__name__


class MonitorApp:
    def __init__(
        self,
        pi: str,
        channels: list[Channel],
        *,
        channel: int = 0,
        tap: str = "pre",
        record_dir: str | None = None,
        status_hostport: str | None = None,
    ) -> None:
        self.pi = pi
        self.channels = channels
        self.status_hostport = status_hostport  # reader /status (--metrics-port)
        self._lock = threading.Lock()
        self._stop = threading.Event()
        # Two generations: playback restarts on channel OR tap change; recorders
        # (fixed tap) restart only on channel change, so a play-tap toggle does
        # not needlessly split their WAV files.
        self._play_gen = 0
        self._chan_gen = 0
        self.channel = max(0, min(channel, len(channels) - 1))
        self.tap = tap
        self.record_dir = record_dir
        self.volume = 1.0
        self.mute = False
        self.rate = 0
        self.meter_dbfs = float("-inf")
        self.status = "starting"
        self.rec_error: str | None = None
        self._play_resp = None  # live playback stream, shut down on change
        # Recording is decoupled from playback: one recorder thread + its own
        # connection per tap being recorded. ponytail: recording the tap you are
        # also listening to opens a second identical stream (the monitor endpoint
        # is one-tap-per-connection); ~40 KB/s, negligible, and keeps record and
        # playback fully independent.
        self.record_taps: set[str] = set()
        self._recorders: dict[str, dict] = {}  # tap -> {thread, stop, resp, path}
        self.scan = False
        self.scan_status = ""
        # Health/liveness for the two continuous data sources.
        self.audio_connected = False       # playback HTTP stream is established
        self._last_audio_ts = 0.0          # monotonic time of the last audio block
        self.status_ok = False             # last /status poll succeeded
        self._last_status_ts = 0.0
        self.status_channels: list[dict] = []  # latest per-channel squelch/carrier
        self._thread = threading.Thread(target=self._worker, daemon=True)
        self._scan_thread = threading.Thread(target=self._scanner, daemon=True)

    # ---- control (called from the UI thread) -----------------------------

    def start(self) -> None:
        self._thread.start()
        self._scan_thread.start()

    def stop(self) -> None:
        self._stop.set()
        for tap in list(self._recorders):
            self._stop_recorder(tap)
        self._bump_and_close(chan=True)
        for t in (self._thread, self._scan_thread):
            if t.is_alive():  # start() may never have been called
                t.join(timeout=2.0)

    def toggle_scan(self) -> None:
        """`s`: auto-scan — follow whichever channels are active."""
        if not self.status_hostport:
            with self._lock:
                self.scan_status = "scan needs the reader's --metrics-port (pass --metrics-port)"
            return
        with self._lock:
            self.scan = not self.scan
            if not self.scan:
                self.scan_status = ""

    def _bump_and_close(self, chan: bool) -> None:
        """Bump the relevant generation(s) and shut down the live stream(s) so
        a blocked ``read()`` returns at once. ``chan=True`` restarts everyone
        (channel change); ``chan=False`` restarts only playback (tap change)."""
        with self._lock:
            self._play_gen += 1
            resps = [self._play_resp]
            if chan:
                self._chan_gen += 1
                resps += [r.get("resp") for r in self._recorders.values()]
        for r in resps:
            _abort_stream(r)

    def set_channel(self, index: int) -> None:
        with self._lock:
            index = max(0, min(index, len(self.channels) - 1))
            if index == self.channel:
                return
            self.channel = index
        self._bump_and_close(chan=True)

    def step_channel(self, delta: int) -> None:
        with self._lock:
            cur = self.channel
        self.set_channel(cur + delta)

    def toggle_tap(self) -> None:
        with self._lock:
            self.tap = "post" if self.tap == "pre" else "pre"
        self._bump_and_close(chan=False)

    def toggle_record(self) -> None:
        """`r`: record the tap currently being listened to (toggle)."""
        with self._lock:
            want = set(self.record_taps)
            tap = self.tap
        want.discard(tap) if tap in want else want.add(tap)
        self._set_recording(want)

    def toggle_record_both(self) -> None:
        """`b`: record both taps at once (toggle)."""
        both = {"pre", "post"}
        with self._lock:
            cur = set(self.record_taps)
        self._set_recording(set() if both <= cur else both)

    def toggle_mute(self) -> None:
        with self._lock:
            self.mute = not self.mute

    def change_volume(self, delta: float) -> None:
        with self._lock:
            self.volume = max(0.0, min(VOL_MAX, round(self.volume + delta, 3)))

    # ---- worker ----------------------------------------------------------

    def _process(self, block: bytes) -> bytes:
        with self._lock:
            vol = 0.0 if self.mute else self.volume
        if vol == 1.0:
            return block
        samples = array.array("h")
        samples.frombytes(block[: len(block) & ~1])
        for i, s in enumerate(samples):
            v = int(s * vol)
            samples[i] = 32767 if v > 32767 else (-32768 if v < -32768 else v)
        return samples.tobytes()

    def _new_wav(self, channel: int, tap: str, rate: int) -> tuple[wave.Wave_write, str]:
        os.makedirs(self.record_dir, exist_ok=True)  # type: ignore[arg-type]
        ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
        stem = _safe_name(self.channels[channel].label, channel)
        path = os.path.join(self.record_dir, f"{stem}_{tap}_{ts}.wav")  # type: ignore[arg-type]
        w = wave.open(path, "wb")
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        return w, path

    # ---- recording (one thread + connection per tap) --------------------

    def _set_recording(self, taps: set[str]) -> None:
        """Reconcile the running recorder threads with the desired ``taps``."""
        if self.record_dir is None:
            with self._lock:
                self.rec_error = "no --record-dir set; cannot record"
            return
        with self._lock:
            self.record_taps = set(taps)
            running = set(self._recorders)
        for tap in running - taps:
            self._stop_recorder(tap)
        for tap in taps - running:
            self._start_recorder(tap)

    def _start_recorder(self, tap: str) -> None:
        rec: dict = {"stop": threading.Event(), "resp": None, "path": None}
        rec["thread"] = threading.Thread(target=self._recorder, args=(tap, rec), daemon=True)
        with self._lock:
            self._recorders[tap] = rec
        rec["thread"].start()

    def _stop_recorder(self, tap: str) -> None:
        with self._lock:
            rec = self._recorders.pop(tap, None)
            resp = rec.get("resp") if rec is not None else None
        if rec is None:
            return
        rec["stop"].set()
        _abort_stream(resp)  # wake a read() blocked on the live socket
        rec["thread"].join(timeout=2.0)

    def _recorder(self, tap: str, rec: dict) -> None:
        """Stream ``tap`` for the current channel to a WAV, reconnecting (and
        rolling to a new file) whenever the channel changes, until stopped."""
        stop = rec["stop"]
        while not self._stop.is_set() and not stop.is_set():
            with self._lock:
                ch, cgen = self.channel, self._chan_gen
            resp = wfile = None
            try:
                rate, resp = open_stream(monitor_url(self.pi, ch, tap))
                with self._lock:
                    if cgen != self._chan_gen or stop.is_set():
                        resp.close()
                        continue
                    rec["resp"] = resp
                wfile, path = self._new_wav(ch, tap, rate)
                with self._lock:
                    rec["path"] = path
                    self.rec_error = None  # a fresh file supersedes any earlier failure
                chunk = max(2, int(rate * 0.02) * 2)
                while not self._stop.is_set() and not stop.is_set():
                    block = resp.read(chunk)
                    if not block:
                        break
                    wfile.writeframes(block)
            except Exception as e:  # noqa: BLE001
                with self._lock:
                    # A channel change or stop shuts our socket down and can
                    # surface here as EOF/OSError: intentional, not an error.
                    if cgen == self._chan_gen and not stop.is_set() and not self._stop.is_set():
                        self.rec_error = f"record {tap}: {_describe_error(e, resp is not None)}"
            finally:
                if resp is not None:
                    try:
                        resp.close()
                    except Exception:
                        pass
                if wfile is not None:
                    try:
                        wfile.close()
                    except Exception:
                        pass
                with self._lock:
                    rec["resp"] = None
                    rec["path"] = None
            with self._lock:
                changed = cgen != self._chan_gen
            if not self._stop.is_set() and not stop.is_set() and not changed:
                stop.wait(RECONNECT_BACKOFF_S)  # backoff after a real error / stall

    # ---- playback worker -------------------------------------------------

    def _worker(self) -> None:
        while not self._stop.is_set():
            with self._lock:
                ch, tap, pgen = self.channel, self.tap, self._play_gen
            url = monitor_url(self.pi, ch, tap)
            out = resp = None
            try:
                if sd is None:
                    raise RuntimeError(f"audio playback unavailable: {AUDIO_BACKEND_ERROR}")
                rate, resp = open_stream(url)
                with self._lock:
                    if pgen != self._play_gen:  # changed while connecting
                        resp.close()
                        continue
                    self._play_resp = resp
                    self.rate = rate
                    self.status = f"connected  ch{ch:02d} {tap} @ {rate} Hz"
                out = sd.RawOutputStream(samplerate=rate, channels=1, dtype="int16")
                out.start()
                chunk = max(2, int(rate * 0.02) * 2)  # ~20 ms of s16 mono
                # Stamp before flagging connected so no snapshot pairs
                # "connected" with a stale timestamp (a spurious IDLE).
                self._last_audio_ts = time.monotonic()
                self.audio_connected = True
                while not self._stop.is_set():
                    block = resp.read(chunk)
                    if not block:
                        break
                    self._last_audio_ts = time.monotonic()
                    self.meter_dbfs = peak_dbfs(block)
                    out.write(self._process(block))
            except Exception as e:  # noqa: BLE001 (surface any error, then retry)
                with self._lock:
                    # A channel/tap switch or stop shuts our socket down and can
                    # surface here as EOF/OSError: intentional, not an error.
                    if pgen == self._play_gen and not self._stop.is_set():
                        self.status = f"reconnecting: {_describe_error(e, resp is not None)}"
                self.meter_dbfs = float("-inf")
            finally:
                self.audio_connected = False
                if out is not None:
                    out.stop()
                    out.close()
                if resp is not None:
                    try:
                        resp.close()
                    except Exception:
                        pass
                with self._lock:
                    self._play_resp = None
            with self._lock:
                changed = pgen != self._play_gen
            if not self._stop.is_set() and not changed:
                self._stop.wait(RECONNECT_BACKOFF_S)  # backoff after a real error / stall

    # ---- /status poller (squelch health + scanner) ----------------------

    def _scanner(self) -> None:
        """Continuously poll the reader's /status: keeps the squelch-data health
        indicator and per-channel open flags live, and (while scanning) hops
        playback to the strongest active channel, riding out transmissions.
        Polls every ``SCAN_POLL_S`` while scanning (hop latency matters) and
        only every ``STATUS_POLL_S`` otherwise (the indicator does not)."""
        last_open = time.monotonic()
        while not self._stop.is_set():
            if not self.status_hostport:
                self._stop.wait(1.0)
                continue
            try:
                chans = fetch_status(self.status_hostport)
            except Exception as e:  # noqa: BLE001
                with self._lock:
                    self.status_ok = False
                    if self.scan:
                        self.scan_status = f"scan: /status unreachable ({e})"
                self._stop.wait(1.0)
                continue
            now = time.monotonic()
            with self._lock:
                self.status_ok = True
                self._last_status_ts = now
                self.status_channels = chans
                cur = self.channel
                scanning = self.scan
            cur_open = any(c.get("ch") == cur and c.get("open") for c in chans)
            if cur_open:
                last_open = now
            idle = now - last_open
            if scanning:
                n_active = sum(1 for c in chans if c.get("open"))
                with self._lock:
                    self.scan_status = (
                        f"scan: {n_active} active"
                        + ("" if cur_open else f", idle {idle:.1f}s")
                    )
                target = choose_scan_target(chans, cur, idle, SCAN_HANG_S)
                if target is not None:
                    self.set_channel(target)
                    last_open = now
            self._stop.wait(SCAN_POLL_S if scanning else STATUS_POLL_S)

    # ---- snapshot for the UI --------------------------------------------

    def snapshot(self) -> dict:
        now = time.monotonic()
        with self._lock:
            audio_fresh = self.audio_connected and (now - self._last_audio_ts) < 1.5
            status_fresh = self.status_ok and (now - self._last_status_ts) < 3.0
            open_chs = {c.get("ch") for c in self.status_channels if c.get("open")}
            return {
                "channel": self.channel,
                "tap": self.tap,
                "record_taps": sorted(self.record_taps),
                "record_paths": {t: r.get("path") for t, r in self._recorders.items()},
                "rec_error": self.rec_error,
                "scan": self.scan,
                "scan_status": self.scan_status,
                "volume": self.volume,
                "mute": self.mute,
                "rate": self.rate,
                "status": self.status,
                "audio_connected": self.audio_connected,
                "audio_fresh": audio_fresh,
                "status_configured": self.status_hostport is not None,
                "status_ok": status_fresh,
                "open_channels": open_chs,
            }


# ---- headless mode -------------------------------------------------------

def run_headless(app: MonitorApp) -> None:
    app.start()
    last = None
    print(
        f"streaming ch{app.channel:02d} ({app.channels[app.channel].freq_mhz:.3f} MHz) "
        f"tap={app.tap} from {app.pi}; Ctrl-C to stop",
        flush=True,
    )
    try:
        while True:
            snap = app.snapshot()
            audio = "streaming" if snap["audio_fresh"] else ("connected" if snap["audio_connected"] else "DOWN")
            sq = "n/a" if not snap["status_configured"] else ("connected" if snap["status_ok"] else "DOWN")
            line = f"[Pi audio: {audio}] [Pluto squelch: {sq}] {snap['status']}"
            if line != last:
                print(line, flush=True)
                last = line
            time.sleep(0.5)
    except KeyboardInterrupt:
        pass
    finally:
        app.stop()
