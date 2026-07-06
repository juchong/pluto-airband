"""Audio streaming from the Pi ``airband-reader`` monitor endpoint.

    GET http://<pi>:<port>/listen/<ch>.wav?tap=pre|post

serves one channel as an unbounded mono s16le WAV: a 44-byte header (with
RIFF/data sizes set to 0xFFFFFFFF because the length is unknown) followed by raw
little-endian 16-bit samples. We parse the sample rate out of the header, skip
the header, and hand the raw sample bytes to the caller.

``tap=pre`` is the continuous raw demod; ``tap=post`` is the squelch-gated,
fully-enhanced audio that LiveATC receives.
"""

from __future__ import annotations

import array
import math
from urllib.parse import quote
from urllib.request import urlopen

WAV_HEADER_LEN = 44
_SAMPLE_RATE_OFFSET = 24  # bytes into a canonical PCM WAV header


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


def open_stream(url: str, timeout: float = 10.0):
    """Open the URL, consume the WAV header, return ``(sample_rate, response)``.

    The returned response is positioned at the first audio sample; read raw
    s16le bytes from it until it closes."""
    resp = urlopen(url, timeout=timeout)  # noqa: S310 (trusted LAN device)
    header = _read_exact(resp, WAV_HEADER_LEN)
    return parse_header_rate(header), resp


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
