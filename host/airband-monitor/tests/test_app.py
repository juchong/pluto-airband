"""Behavioural tests for ``MonitorApp``'s playback worker and recorder threads.

``airband_monitor.app.open_stream`` is replaced by a fake endpoint that hands
out in-memory streams, and ``app.sd`` by a no-op output device, so no reader,
network or sound card is needed. The fake streams *block* in ``read()`` until
the app shuts them down, exactly like a live socket with no incoming bytes, so
these tests also pin that channel/tap switches and stop really interrupt a
blocked read instead of waiting for it.
"""

from __future__ import annotations

import threading
import time
import types
import wave

import pytest

from airband_monitor import app as app_mod
from airband_monitor.app import MonitorApp
from airband_monitor.plan import Channel
from airband_monitor.stream import monitor_url

PI = "pi.test:8082"
CHANNELS = [Channel(0, 119.2e6, "dep"), Channel(1, 121.5e6, "guard")]
RATE = 20000
BLOCK = b"\x01\x00" * int(RATE * 0.02)  # one ~20 ms chunk, the size the app reads
PARK_LIMIT_S = 5.0  # a blocked fake gives up after this; tests assert far below it


def wait_until(pred, timeout: float = 3.0, step: float = 0.01) -> bool:
    """Poll ``pred`` until truthy or ``timeout``; return its final value."""
    deadline = time.monotonic() + timeout
    while not pred():
        if time.monotonic() >= deadline:
            return bool(pred())
        time.sleep(step)
    return True


class FakeStream:
    """Stands in for ``stream.MonitorStream``.

    Serves ``blocks`` reads of audio, then either parks in ``read()`` until
    ``shutdown()``/``close()`` is called (``then="block"``, a live socket with
    nothing arriving) or raises ``TimeoutError`` (``then="stall"``, what a real
    read does after ``STREAM_TIMEOUT_S``). Once shut down, a read reports it
    the two ways a real socket can: EOF (``after_close="eof"``) or an
    ``OSError`` (``after_close="raise"``)."""

    def __init__(self, url: str, *, blocks: int = 2, then: str = "block", after_close: str = "eof"):
        self.url = url
        self.blocks = blocks
        self.then = then
        self.after_close = after_close
        self.reads = 0
        self.shutdown_calls = 0
        self.closed = threading.Event()
        self.parked = threading.Event()  # set once a read is blocked waiting

    def read(self, n: int) -> bytes:
        if not self.closed.is_set():
            self.reads += 1
            if self.reads <= self.blocks:
                return BLOCK[:n]
            if self.then == "stall":
                raise TimeoutError("timed out")
            self.parked.set()
            self.closed.wait(PARK_LIMIT_S)
        if self.after_close == "raise":
            raise OSError("socket shut down by another thread")
        return b""

    def shutdown(self) -> None:
        self.shutdown_calls += 1
        self.closed.set()

    def close(self) -> None:
        self.closed.set()


class FakeEndpoint:
    """Replacement for ``app.open_stream``.

    ``make(url, n)`` builds the n-th stream (counting every attempt, so it can
    raise to simulate a failed connect). Attempts numbered ``>= gate_from``
    first wait at ``gate``, which lets a test inspect the app while it is
    "reconnecting" and then let it through."""

    def __init__(self, make, *, gate_from: int | None = None):
        self.make = make
        self.gate_from = gate_from
        self.gate = threading.Event()
        self.waiting = threading.Event()  # some attempt is parked at the gate
        self.streams: list[FakeStream] = []
        self.opens = 0

    def __call__(self, url: str, timeout: float | None = None):
        n = self.opens
        self.opens += 1
        if self.gate_from is not None and n >= self.gate_from:
            self.waiting.set()
            self.gate.wait(PARK_LIMIT_S)
        s = self.make(url, n)
        self.streams.append(s)
        return RATE, s


class FakeOutput:
    """No-op stand-in for ``sounddevice.RawOutputStream``."""

    def __init__(self, **kwargs):
        self.kwargs = kwargs
        self.written = 0

    def start(self) -> None:
        pass

    def write(self, data: bytes) -> None:
        self.written += len(data)

    def stop(self) -> None:
        pass

    def close(self) -> None:
        pass


@pytest.fixture
def fast_backoff(monkeypatch):
    monkeypatch.setattr(app_mod, "RECONNECT_BACKOFF_S", 0.05)


@pytest.fixture
def fake_audio(monkeypatch, fast_backoff):
    monkeypatch.setattr(app_mod, "sd", types.SimpleNamespace(RawOutputStream=FakeOutput))


@pytest.fixture
def install_endpoint(monkeypatch):
    endpoints: list[FakeEndpoint] = []

    def _install(make, **kw) -> FakeEndpoint:
        ep = FakeEndpoint(make, **kw)
        monkeypatch.setattr(app_mod, "open_stream", ep)
        endpoints.append(ep)
        return ep

    yield _install
    for ep in endpoints:
        ep.gate.set()  # never leave a worker parked at a gate


@pytest.fixture
def make_app(tmp_path, monkeypatch):
    # ``monkeypatch`` is requested so this fixture tears down (stopping the
    # threads) before the patches are undone.
    apps: list[MonitorApp] = []

    def _make(**kw) -> MonitorApp:
        kw.setdefault("record_dir", str(tmp_path))
        app = MonitorApp(PI, CHANNELS, **kw)
        apps.append(app)
        return app

    yield _make
    for app in apps:
        app.stop()


def test_recorder_rolls_over_on_channel_change(make_app, install_endpoint, tmp_path):
    ep = install_endpoint(lambda url, n: FakeStream(url, blocks=2))
    app = make_app()
    app.toggle_record()  # record the current tap ("pre") on channel 0
    assert wait_until(lambda: ep.streams and ep.streams[0].parked.is_set())
    assert wait_until(lambda: "dep_pre_" in (app.snapshot()["record_paths"].get("pre") or ""))
    first = ep.streams[0]

    t0 = time.monotonic()
    app.set_channel(1)
    assert wait_until(lambda: len(ep.streams) == 2 and ep.streams[1].parked.is_set())
    assert time.monotonic() - t0 < 2.0, "channel change did not interrupt the blocked read"
    assert first.shutdown_calls >= 1  # the socket was shut down, not merely closed
    assert [s.url for s in ep.streams] == [monitor_url(PI, 0, "pre"), monitor_url(PI, 1, "pre")]
    assert wait_until(lambda: "guard_pre_" in (app.snapshot()["record_paths"].get("pre") or ""))
    assert app.snapshot()["rec_error"] is None

    app.stop()
    wavs = sorted(p.name for p in tmp_path.glob("*.wav"))
    assert len(wavs) == 2
    assert wavs[0].startswith("dep_pre_") and wavs[1].startswith("guard_pre_")
    with wave.open(str(tmp_path / wavs[0])) as w:  # the file closed by the rollover
        assert w.getframerate() == RATE
        assert w.getnframes() == 2 * len(BLOCK) // 2  # the two blocks served before the switch


def test_stop_recorder_interrupts_blocked_read(make_app, install_endpoint):
    ep = install_endpoint(lambda url, n: FakeStream(url, blocks=1))
    app = make_app()
    app.toggle_record()
    assert wait_until(lambda: ep.streams and ep.streams[0].parked.is_set())
    thread = app._recorders["pre"]["thread"]

    t0 = time.monotonic()
    app.toggle_record()  # off
    assert time.monotonic() - t0 < 1.5, "stopping the recorder waited for the blocked read"
    assert not thread.is_alive()
    assert ep.streams[0].shutdown_calls >= 1
    snap = app.snapshot()
    assert snap["record_taps"] == [] and snap["rec_error"] is None


def test_worker_reconnects_after_stalled_read(make_app, install_endpoint, fake_audio):
    # First stream: two blocks, then the read times out (reader gone). The
    # reconnect attempt parks at the gate so the "stalled" state is observable.
    ep = install_endpoint(
        lambda url, n: FakeStream(url, blocks=2, then="stall" if n == 0 else "block"),
        gate_from=1,
    )
    app = make_app(record_dir=None)
    app.start()
    assert wait_until(ep.waiting.is_set), "worker did not try to reconnect after the stall"
    snap = app.snapshot()
    assert "stalled" in snap["status"], snap["status"]
    assert snap["audio_connected"] is False and snap["audio_fresh"] is False

    ep.gate.set()
    assert wait_until(lambda: app.snapshot()["audio_connected"])
    assert len(ep.streams) == 2 and ep.streams[1].url == monitor_url(PI, 0, "pre")
    assert app.snapshot()["status"].startswith("connected  ch00 pre")


def test_connect_stamps_audio_before_flagging_connected(make_app, install_endpoint, fake_audio):
    # A stream that never delivers a byte: right after connecting the indicator
    # must still read fresh (STREAMING), not IDLE from a stale timestamp.
    ep = install_endpoint(lambda url, n: FakeStream(url, blocks=0))
    app = make_app(record_dir=None)
    app.start()
    assert wait_until(lambda: app.snapshot()["audio_connected"])
    assert app.snapshot()["audio_fresh"] is True
    assert ep.streams[0].parked.wait(1.0)


def test_intentional_switch_is_not_an_error(make_app, install_endpoint, fake_audio):
    # After shutdown the fakes *raise* from read(): the noisiest way a real
    # socket reports a cross-thread shutdown. Attempts 2+ (the reconnects for
    # ch1) park at the gate so the state in between is observable.
    ep = install_endpoint(lambda url, n: FakeStream(url, blocks=1, after_close="raise"), gate_from=2)
    app = make_app()
    app.start()
    app.toggle_record()  # playback + recorder, both ch0 "pre"
    assert wait_until(lambda: len(ep.streams) == 2 and all(s.parked.is_set() for s in ep.streams))
    assert wait_until(lambda: app.snapshot()["status"].startswith("connected  ch00"))

    app.set_channel(1)
    assert wait_until(lambda: ep.opens == 4), "both threads should be reconnecting for ch1"
    snap = app.snapshot()
    assert snap["rec_error"] is None
    assert not snap["status"].startswith("reconnecting"), snap["status"]

    ep.gate.set()
    assert wait_until(lambda: len(ep.streams) == 4 and all(s.parked.is_set() for s in ep.streams[2:]))
    assert {s.url for s in ep.streams[2:]} == {monitor_url(PI, 1, "pre")}
    assert wait_until(lambda: app.snapshot()["status"].startswith("connected  ch01 pre"))
    assert app.snapshot()["rec_error"] is None

    # A tap toggle restarts only playback: one of the two live streams is shut
    # down, the recorder's keeps running, and still nothing is an error.
    app.toggle_tap()
    assert wait_until(lambda: len(ep.streams) == 5 and ep.streams[4].parked.is_set())
    assert ep.streams[4].url == monitor_url(PI, 1, "post")
    assert sum(1 for s in ep.streams[2:4] if s.shutdown_calls) == 1
    assert wait_until(lambda: app.snapshot()["status"].startswith("connected  ch01 post"))
    assert app.snapshot()["rec_error"] is None

    # Stopping shuts every stream down; that is not an error either.
    app.stop()
    snap = app.snapshot()
    assert snap["rec_error"] is None
    assert not snap["status"].startswith("reconnecting"), snap["status"]


def test_rec_error_cleared_when_new_file_opens(make_app, install_endpoint, fast_backoff):
    def make(url: str, n: int) -> FakeStream:
        if n == 0:
            raise ConnectionRefusedError("reader down")
        return FakeStream(url, blocks=1)

    install_endpoint(make)
    app = make_app()
    app.toggle_record()
    assert wait_until(lambda: app.snapshot()["rec_error"] is not None)
    assert "reader down" in app.snapshot()["rec_error"]
    # The retry succeeds and opens a fresh WAV: the stale error must go away.
    assert wait_until(lambda: app.snapshot()["record_paths"].get("pre") is not None)
    assert app.snapshot()["rec_error"] is None
