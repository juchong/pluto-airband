"""Interactive live listener (curses TUI) and headless player.

Runs one streaming worker thread that connects to the Pi monitor endpoint for
the currently-selected channel + tap, plays it through the default output
device, meters the level, and optionally records the raw stream to a WAV. The
main thread drives the curses UI (or just idles in ``--no-tui`` mode); switching
channel or tap closes the live HTTP response to unblock the worker, which then
reconnects with the new parameters.
"""

from __future__ import annotations

import array
import curses
import os
import threading
import time
import wave
from datetime import datetime, timezone

import sounddevice as sd

from .plan import Channel
from .stream import fetch_status, monitor_url, open_stream, peak_dbfs

VOL_STEP = 0.1
VOL_MAX = 4.0
SCAN_POLL_S = 0.3   # how often to poll /status while scanning
SCAN_HANG_S = 1.5   # stay on a channel this long after it closes before hopping


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
        self._play_resp = None  # live playback response, closed on change
        # Recording is decoupled from playback: one recorder thread + its own
        # connection per tap being recorded. ponytail: recording the tap you are
        # also listening to opens a second identical stream (the monitor endpoint
        # is one-tap-per-connection); ~40 KB/s, negligible, and keeps record and
        # playback fully independent.
        self.record_taps: set[str] = set()
        self._recorders: dict[str, dict] = {}  # tap -> {thread, stop, resp, path}
        self.scan = False
        self.scan_status = ""
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
        self._thread.join(timeout=2.0)
        self._scan_thread.join(timeout=2.0)

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
        """Bump the relevant generation(s) and close the live response(s) so a
        blocked ``read()`` returns at once. ``chan=True`` restarts everyone
        (channel change); ``chan=False`` restarts only playback (tap change)."""
        with self._lock:
            self._play_gen += 1
            resps = [self._play_resp]
            if chan:
                self._chan_gen += 1
                resps += [r.get("resp") for r in self._recorders.values()]
        for r in resps:
            if r is not None:
                try:
                    r.close()
                except Exception:
                    pass

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
        if rec is None:
            return
        rec["stop"].set()
        r = rec.get("resp")
        if r is not None:
            try:
                r.close()
            except Exception:
                pass
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
                chunk = max(2, int(rate * 0.02) * 2)
                while not self._stop.is_set() and not stop.is_set():
                    block = resp.read(chunk)
                    if not block:
                        break
                    wfile.writeframes(block)
            except Exception as e:  # noqa: BLE001
                with self._lock:
                    self.rec_error = f"record {tap}: {e}"
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
                stop.wait(1.0)  # backoff after a real error

    # ---- playback worker -------------------------------------------------

    def _worker(self) -> None:
        while not self._stop.is_set():
            with self._lock:
                ch, tap, pgen = self.channel, self.tap, self._play_gen
            url = monitor_url(self.pi, ch, tap)
            out = resp = None
            try:
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
                while not self._stop.is_set():
                    block = resp.read(chunk)
                    if not block:
                        break
                    self.meter_dbfs = peak_dbfs(block)
                    out.write(self._process(block))
            except Exception as e:  # noqa: BLE001 (surface any error, then retry)
                with self._lock:
                    self.status = f"reconnecting: {e}"
                self.meter_dbfs = float("-inf")
            finally:
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
                self._stop.wait(1.0)  # backoff after a real error

    # ---- scanner ---------------------------------------------------------

    def _scanner(self) -> None:
        """While scanning, poll the reader's /status and hop playback to the
        strongest active channel, riding out each transmission."""
        last_open = time.monotonic()
        while not self._stop.is_set():
            if not self.scan:
                last_open = time.monotonic()
                self._stop.wait(0.2)
                continue
            try:
                chans = fetch_status(self.status_hostport)  # type: ignore[arg-type]
            except Exception as e:  # noqa: BLE001
                with self._lock:
                    self.scan_status = f"scan: /status unreachable ({e})"
                self._stop.wait(1.0)
                continue
            with self._lock:
                cur = self.channel
            now = time.monotonic()
            cur_open = any(c.get("ch") == cur and c.get("open") for c in chans)
            if cur_open:
                last_open = now
            idle = now - last_open
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
            self._stop.wait(SCAN_POLL_S)

    # ---- snapshot for the UI --------------------------------------------

    def snapshot(self) -> dict:
        with self._lock:
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
            }


# ---- meter rendering -----------------------------------------------------

def _meter_bar(dbfs: float, width: int = 30, floor: float = -60.0) -> str:
    if dbfs == float("-inf") or dbfs < floor:
        filled = 0
    else:
        filled = int(round((dbfs - floor) / (0.0 - floor) * width))
        filled = max(0, min(width, filled))
    label = "  -inf" if dbfs == float("-inf") else f"{dbfs:6.1f}"
    return f"[{'#' * filled}{'-' * (width - filled)}] {label} dBFS"


# ---- curses UI -----------------------------------------------------------

def run_tui(app: MonitorApp) -> None:
    curses.wrapper(_tui_loop, app)


def _tui_loop(stdscr, app: MonitorApp) -> None:
    curses.curs_set(0)
    stdscr.nodelay(True)
    stdscr.timeout(100)
    app.start()
    pending = ""  # digits typed for a channel jump
    try:
        while True:
            _draw(stdscr, app, pending)
            try:
                ch = stdscr.getch()
            except KeyboardInterrupt:
                break
            if ch == -1:
                continue
            if ch in (ord("q"), 27):  # q / ESC
                break
            elif ch in (curses.KEY_UP, ord("k")):
                app.step_channel(-1)
            elif ch in (curses.KEY_DOWN, ord("j")):
                app.step_channel(1)
            elif ch in (ord("t"), ord("T")):
                app.toggle_tap()
            elif ch in (ord("r"), ord("R")):
                app.toggle_record()
            elif ch in (ord("b"), ord("B")):
                app.toggle_record_both()
            elif ch in (ord("s"), ord("S")):
                app.toggle_scan()
            elif ch in (ord("+"), ord("=")):
                app.change_volume(VOL_STEP)
            elif ch in (ord("-"), ord("_")):
                app.change_volume(-VOL_STEP)
            elif ch in (ord("m"), ord("M")):
                app.toggle_mute()
            elif ord("0") <= ch <= ord("9"):
                pending += chr(ch)
            elif ch in (curses.KEY_ENTER, 10, 13):
                if pending:
                    app.set_channel(int(pending))
                pending = ""
            elif ch in (curses.KEY_BACKSPACE, 127, 8):
                pending = pending[:-1]
            else:
                pending = ""
    finally:
        app.stop()


def _addline(stdscr, y: int, text: str, width: int, attr: int = 0) -> None:
    """Write one padded row, clipped to ``width - 1``. Curses returns ERR when
    the write reaches the bottom-right cell (the cursor can't advance past it),
    so we cap at width-1 and ignore the harmless error."""
    try:
        stdscr.addnstr(y, 0, text.ljust(width)[: width - 1], width - 1, attr)
    except curses.error:
        pass


def _draw(stdscr, app: MonitorApp, pending: str) -> None:
    snap = app.snapshot()
    stdscr.erase()
    h, w = stdscr.getmaxyx()

    rec = "+".join(snap["record_taps"]) if snap["record_taps"] else "off"
    vol = "MUTE" if snap["mute"] else f"{snap['volume']:.1f}x"
    header = (
        f" pluto airband monitor  pi={app.pi}  tap={snap['tap']}  "
        f"vol={vol}  rec={rec}  scan={'ON' if snap['scan'] else 'off'} "
    )
    _addline(stdscr, 0, header, w, curses.A_REVERSE)

    _addline(stdscr, 1, f" {_meter_bar(app.meter_dbfs)}", w)
    _addline(stdscr, 2, f" status: {snap['status']}", w)
    row = 3
    if snap["scan_status"]:
        _addline(stdscr, row, f" {snap['scan_status']}", w)
        row += 1
    for tap in snap["record_taps"]:
        path = snap["record_paths"].get(tap)
        _addline(stdscr, row, f" rec {tap}: {path or '(connecting...)'}", w)
        row += 1
    if snap["rec_error"] and not snap["record_taps"]:
        _addline(stdscr, row, f" record error: {snap['rec_error']}", w)
        row += 1

    top = row + 1
    _addline(stdscr, top, "  #   freq (MHz)  label", w, curses.A_BOLD)
    cur = snap["channel"]
    for i, c in enumerate(app.channels):
        r = top + 1 + i
        if r >= h - 1:
            break
        marker = ">" if i == cur else " "
        line = f"{marker} {i:>2}  {c.freq_mhz:10.3f}  {c.label}"
        _addline(stdscr, r, line, w, curses.A_REVERSE if i == cur else curses.A_NORMAL)

    footer = " j/k: channel  #+Enter: jump  t: pre/post  s: scan  r: rec tap  b: rec both  +/-: vol  m: mute  q: quit "
    if pending:
        footer = f" jump-> {pending}   " + footer
    _addline(stdscr, h - 1, footer, w, curses.A_REVERSE)
    stdscr.refresh()


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
            if snap["status"] != last:
                print(snap["status"], flush=True)
                last = snap["status"]
            time.sleep(0.5)
    except KeyboardInterrupt:
        pass
    finally:
        app.stop()
