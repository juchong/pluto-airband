"""curses TUI for :class:`airband_monitor.app.MonitorApp`.

Kept apart from ``app`` so ``import curses`` only happens when the TUI is
actually run (``cli.main`` imports this module lazily): ``--no-tui`` and the
tests work on platforms without curses, e.g. Windows without ``windows-curses``.
"""

from __future__ import annotations

import curses

from .app import VOL_STEP, MonitorApp

# curses color pair ids for the health indicators
CLR_OK = 1
CLR_WARN = 2
CLR_BAD = 3


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
    if curses.has_colors():
        curses.start_color()
        curses.use_default_colors()
        curses.init_pair(CLR_OK, curses.COLOR_GREEN, -1)
        curses.init_pair(CLR_WARN, curses.COLOR_YELLOW, -1)
        curses.init_pair(CLR_BAD, curses.COLOR_RED, -1)
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


def _clr(pair: int) -> int:
    return (curses.color_pair(pair) | curses.A_BOLD) if curses.has_colors() else curses.A_BOLD


def _add_segments(stdscr, y: int, segments: list[tuple[str, int]], width: int) -> None:
    """Write ``(text, attr)`` segments left-to-right on one row, clipped to
    ``width - 1`` and swallowing the bottom-right-corner error."""
    x = 0
    for text, attr in segments:
        if x >= width - 1:
            break
        chunk = text[: width - 1 - x]
        try:
            stdscr.addnstr(y, x, chunk, len(chunk), attr)
        except curses.error:
            pass
        x += len(chunk)


def _indicator(label: str, ok: bool, warn: bool = False, ok_text: str = "UP", bad_text: str = "DOWN"):
    """Return the (text, attr) segments for one health indicator."""
    if warn:
        return [(f"  {label}: ", curses.A_NORMAL), ("IDLE", _clr(CLR_WARN))]
    state = ok_text if ok else bad_text
    return [(f"  {label}: ", curses.A_NORMAL), (state, _clr(CLR_OK if ok else CLR_BAD))]


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

    # Connection health for the two continuous sources. IDLE = connected but
    # no bytes for 1.5 s, i.e. the stream is stalling (both taps normally
    # stream continuously); after STREAM_TIMEOUT_S the worker reconnects.
    audio = _indicator(
        "Pi audio", snap["audio_connected"], warn=snap["audio_connected"] and not snap["audio_fresh"],
        ok_text="STREAMING",
    )
    if snap["status_configured"]:
        squelch = _indicator("Pluto squelch", snap["status_ok"], ok_text="CONNECTED")
    else:
        squelch = [("  Pluto squelch: ", curses.A_NORMAL), ("disabled", _clr(CLR_WARN))]
    _add_segments(stdscr, 1, [(" ", curses.A_NORMAL), *audio, *squelch], w)

    _addline(stdscr, 2, f" {_meter_bar(app.meter_dbfs)}", w)
    _addline(stdscr, 3, f" status: {snap['status']}", w)
    row = 4
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
    _addline(stdscr, top, "  # o freq (MHz)  label   (o = squelch open)", w, curses.A_BOLD)
    cur = snap["channel"]
    open_chs = snap["open_channels"]
    for i, c in enumerate(app.channels):
        r = top + 1 + i
        if r >= h - 1:
            break
        marker = ">" if i == cur else " "
        is_open = i in open_chs
        line = f"{marker}{i:>2} {'*' if is_open else ' '} {c.freq_mhz:10.3f}  {c.label}"
        if i == cur:
            attr = curses.A_REVERSE
        elif is_open:
            attr = _clr(CLR_OK)  # squelch open -> green (live squelch data)
        else:
            attr = curses.A_NORMAL
        _addline(stdscr, r, line, w, attr)

    footer = " j/k: channel  #+Enter: jump  t: pre/post  s: scan  r: rec tap  b: rec both  +/-: vol  m: mute  q: quit "
    if pending:
        footer = f" jump-> {pending}   " + footer
    _addline(stdscr, h - 1, footer, w, curses.A_REVERSE)
    stdscr.refresh()
