"""Socket-level tests for ``stream.open_stream`` / ``MonitorStream`` against a
tiny loopback server that answers once and then goes silent: the read timeout
surfaces as ``TimeoutError`` (the app's "stalled" signal), a cross-thread
``shutdown()`` wakes a blocked ``read()`` at once, and bad replies are errors.
"""

from __future__ import annotations

import socket
import struct
import threading
import time

import pytest

from airband_monitor.stream import WAV_HEADER_LEN, MonitorStream, open_stream

RATE = 20000
BLOCK = b"\x01\x00" * 400
OK_REPLY = b"HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n"


def _wav_header(rate: int) -> bytes:
    return (
        b"RIFF" + struct.pack("<I", 0xFFFFFFFF) + b"WAVE"
        + b"fmt " + struct.pack("<IHHIIHH", 16, 1, 1, rate, rate * 2, 2, 16)
        + b"data" + struct.pack("<I", 0xFFFFFFFF)
    )


class StallingServer:
    """Accepts one connection, sends ``reply``, then keeps the socket open
    without sending anything more until released."""

    def __init__(self, reply: bytes):
        self.reply = reply
        self.release = threading.Event()
        self.sock = socket.socket()
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(1)
        self.sock.settimeout(5.0)
        self.url = f"http://127.0.0.1:{self.sock.getsockname()[1]}/listen/0.wav?tap=pre"
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()

    def _serve(self) -> None:
        try:
            conn, _ = self.sock.accept()
        except OSError:
            return
        with conn:
            conn.settimeout(5.0)
            try:
                conn.recv(4096)  # the request
                conn.sendall(self.reply)
            except OSError:
                return
            self.release.wait(5.0)

    def close(self) -> None:
        self.release.set()
        self.sock.close()
        self.thread.join(5.0)


@pytest.fixture
def server():
    made: list[StallingServer] = []

    def _make(reply: bytes) -> StallingServer:
        srv = StallingServer(reply)
        made.append(srv)
        return srv

    yield _make
    for srv in made:
        srv.close()


def test_open_stream_parses_header_then_times_out_on_stall(server):
    srv = server(OK_REPLY + _wav_header(RATE) + BLOCK)
    rate, stream = open_stream(srv.url, timeout=0.3)
    try:
        assert rate == RATE
        assert isinstance(stream, MonitorStream)
        assert stream.read(len(BLOCK)) == BLOCK
        t0 = time.monotonic()
        with pytest.raises(TimeoutError):  # == socket.timeout on 3.10+
            stream.read(10)
        assert 0.2 < time.monotonic() - t0 < 2.0
    finally:
        stream.close()


def test_shutdown_from_another_thread_unblocks_read(server):
    srv = server(OK_REPLY + _wav_header(RATE) + BLOCK)
    rate, stream = open_stream(srv.url, timeout=10.0)
    assert stream.read(len(BLOCK)) == BLOCK
    result: list = []

    def reader() -> None:
        try:
            result.append(stream.read(10))
        except Exception as e:  # noqa: BLE001
            result.append(e)

    t = threading.Thread(target=reader, daemon=True)
    t.start()
    time.sleep(0.1)  # let it park in recv()
    assert t.is_alive()
    t0 = time.monotonic()
    stream.shutdown()
    t.join(2.0)
    assert not t.is_alive(), "shutdown() did not wake the blocked read"
    assert time.monotonic() - t0 < 1.0
    assert result and (result[0] == b"" or isinstance(result[0], OSError))
    stream.shutdown()  # idempotent
    stream.close()


def test_open_stream_rejects_non_200(server):
    srv = server(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    with pytest.raises(ConnectionError, match="404"):
        open_stream(srv.url, timeout=1.0)


def test_open_stream_rejects_non_wav_body(server):
    srv = server(OK_REPLY + b"x" * WAV_HEADER_LEN)
    with pytest.raises(ValueError):
        open_stream(srv.url, timeout=1.0)


def test_open_stream_rejects_truncated_header(server):
    srv = server(OK_REPLY + _wav_header(RATE)[:10])
    with pytest.raises((ConnectionError, TimeoutError)):
        open_stream(srv.url, timeout=0.3)

