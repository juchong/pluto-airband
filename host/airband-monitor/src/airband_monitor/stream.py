"""Audio streaming from the Pi ``airband-reader`` monitor endpoint.

    GET http://<pi>:<port>/listen/<ch>.wav?tap=pre|post

serves one channel as an unbounded mono s16le WAV: a 44-byte header (with
RIFF/data sizes set to 0xFFFFFFFF because the length is unknown) followed by raw
little-endian 16-bit samples. We parse the sample rate out of the header, skip
the header, and hand the raw sample bytes to the caller.

``tap=pre`` is the continuous raw demod; ``tap=post`` is the squelch-gated,
fully-enhanced audio that LiveATC receives. Both taps stream bytes continuously
(the post tap ships silence while squelched), so a read that yields nothing
for ``STREAM_TIMEOUT_S`` means the reader or the network is gone, not that the
channel is quiet; callers treat it as a stall and reconnect.
"""

from __future__ import annotations

import array
import http.client
import json
import math
import socket
from urllib.parse import quote, urlsplit
from urllib.request import urlopen

WAV_HEADER_LEN = 44
_SAMPLE_RATE_OFFSET = 24  # bytes into a canonical PCM WAV header
# Connect + per-read timeout. The endpoint never goes byte-silent while alive,
# so hitting this means "stalled" (reader or network gone): reconnect.
STREAM_TIMEOUT_S = 2.5


def monitor_url(pi: str, channel: int, tap: str) -> str:
    """Build the monitor URL. ``pi`` is ``host:port`` (or ``host``)."""
    if tap not in ("pre", "post"):
        raise ValueError(f"tap must be 'pre' or 'post', got {tap!r}")
    return f"http://{pi}/listen/{quote(str(channel))}.wav?tap={tap}"


def _read_exact(resp, n: int) -> bytes:
    """Read exactly ``n`` bytes or raise on early EOF."""
    buf = bytearray()
    while len(buf) < n:
        chunk = resp.read(n - len(buf))
        if not chunk:
            raise ConnectionError("stream closed before WAV header was complete")
        buf.extend(chunk)
    return bytes(buf)


def parse_header_rate(header: bytes) -> int:
    """Extract the sample rate (Hz) from a 44-byte PCM WAV header."""
    if len(header) < WAV_HEADER_LEN or header[:4] != b"RIFF" or header[8:12] != b"WAVE":
        raise ValueError("not a WAV stream")
    return int.from_bytes(header[_SAMPLE_RATE_OFFSET:_SAMPLE_RATE_OFFSET + 4], "little")


class MonitorStream:
    """A live monitor response positioned at the first audio sample.

    Bundles the HTTP response with its socket so that *another* thread can
    interrupt a blocked ``read()``: closing the response from a second thread
    does not wake a ``recv()`` in progress (it just waits for it), whereas
    ``shutdown(SHUT_RDWR)`` makes that ``recv()`` return at once with EOF or
    an error. ``read()`` raises ``TimeoutError`` when no bytes arrive within
    the stream timeout given to :func:`open_stream`.
    """

    def __init__(self, sock: socket.socket, resp: http.client.HTTPResponse) -> None:
        self._sock = sock
        self._resp = resp

    def read(self, n: int) -> bytes:
        """Return up to ``n`` raw s16le bytes; ``b""`` at EOF."""
        return self._resp.read(n)

    def shutdown(self) -> None:
        """Wake a ``read()`` blocked on another thread. Safe to call more than
        once, after ``close()``, or when the peer is already gone."""
        try:
            self._sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

    def close(self) -> None:
        self.shutdown()
        for obj in (self._resp, self._sock):
            try:
                obj.close()
            except OSError:
                pass

    def __enter__(self) -> MonitorStream:
        return self

    def __exit__(self, *exc) -> None:
        self.close()


def open_stream(url: str, timeout: float = STREAM_TIMEOUT_S) -> tuple[int, MonitorStream]:
    """Open the URL, consume the WAV header, return ``(sample_rate, stream)``.

    ``timeout`` bounds the connect and every subsequent ``read()``; the stream
    is positioned at the first audio sample. Read raw s16le bytes from it until
    it returns ``b""`` (closed) or raises (``TimeoutError`` = stalled)."""
    parts = urlsplit(url)
    if parts.scheme != "http" or not parts.hostname:
        raise ValueError(f"monitor URL must look like http://host[:port]/...: {url!r}")
    path = (parts.path or "/") + (f"?{parts.query}" if parts.query else "")
    conn = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=timeout)
    try:
        conn.request("GET", path)
        # getresponse() forgets conn.sock for a ``Connection: close`` reply
        # (which the monitor endpoint always sends), so keep our own handle.
        sock = conn.sock
        resp = conn.getresponse()
    except BaseException:
        conn.close()
        raise
    stream = MonitorStream(sock, resp)
    try:
        if resp.status != 200:
            raise ConnectionError(f"HTTP {resp.status} {resp.reason} from {url}")
        header = _read_exact(stream, WAV_HEADER_LEN)
    except BaseException:
        stream.close()
        raise
    return parse_header_rate(header), stream


def fetch_status(hostport: str, timeout: float = 3.0) -> list[dict]:
    """Fetch the reader's ``/status`` (on its ``--metrics-port``) and return the
    per-channel list: ``[{"ch", "open", "carrier_dbc", ...}, ...]``. Used by the
    scanner to find active channels without opening every audio stream."""
    with urlopen(f"http://{hostport}/status", timeout=timeout) as resp:  # noqa: S310
        return json.load(resp).get("channels", [])


def peak_dbfs(block: bytes) -> float:
    """Peak level of an s16le block in dBFS (``-inf`` for silence/empty)."""
    if len(block) < 2:
        return float("-inf")
    samples = array.array("h")
    samples.frombytes(block[: len(block) & ~1])  # ignore a trailing odd byte
    peak = max((abs(s) for s in samples), default=0)
    if peak == 0:
        return float("-inf")
    return 20.0 * math.log10(peak / 32768.0)
