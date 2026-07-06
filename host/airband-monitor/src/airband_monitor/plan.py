"""Channel plan discovery.

The authoritative channel list (frequency + optional label) comes from the
Pluto's ``GET /api/airband`` REST API (served by ``maia-httpd`` on port 8000):

    {
      "center_hz": 126400000,
      "samp_rate": 16000000,
      "channels": [{"freq_hz": 119200000.0, "label": "KSEA Dep East"}, ...],
      "max_channels": 18,
      ...
    }

For robustness we also accept the SD-card file schema (``channels_hz`` +
optional parallel ``channel_labels``) so the same parser works if pointed at a
raw ``airband.json``. Labels/descriptions can be enriched from a local
``feeds.json`` (its per-channel ``name``/``description``).
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from urllib.request import urlopen


@dataclass(frozen=True)
class Channel:
    index: int
    freq_hz: float
    label: str

    @property
    def freq_mhz(self) -> float:
        return self.freq_hz / 1e6


def parse_plan(doc: dict) -> list[Channel]:
    """Parse an ``/api/airband`` response (or an ``airband.json`` file) into
    an ordered channel list. The list index is the frame/monitor channel index."""
    chans = doc.get("channels")
    if isinstance(chans, list):
        out: list[Channel] = []
        for i, c in enumerate(chans):
            freq = float(c["freq_hz"])
            label = (c.get("label") or "").strip()
            out.append(Channel(i, freq, label))
        return out

    # SD-card file fallback: parallel channels_hz / channel_labels arrays.
    freqs = doc.get("channels_hz")
    if isinstance(freqs, list):
        labels = doc.get("channel_labels") or []
        return [
            Channel(i, float(f), (labels[i].strip() if i < len(labels) and labels[i] else ""))
            for i, f in enumerate(freqs)
        ]

    raise ValueError("no 'channels' or 'channels_hz' in plan document")


def enrich_from_feeds(channels: list[Channel], feeds_path: str) -> list[Channel]:
    """Fill in blank labels from a ``feeds.json`` file's per-channel names.

    A feed's ``name`` (or ``description``) is keyed by channel index; the first
    entry for a channel wins. Only fills labels that are currently empty so the
    Pluto's own labels take precedence."""
    with open(feeds_path, encoding="utf-8") as f:
        feeds = json.load(f).get("feeds", [])
    names: dict[int, str] = {}
    for feed in feeds:
        ch = feed.get("channel")
        if not isinstance(ch, int) or ch in names:
            continue
        name = (feed.get("name") or feed.get("description") or "").strip()
        if name:
            names[ch] = name
    return [
        c if c.label else Channel(c.index, c.freq_hz, names.get(c.index, ""))
        for c in channels
    ]


def fetch_plan(pluto: str, timeout: float = 5.0) -> list[Channel]:
    """Fetch and parse the channel plan from ``http://<pluto>/api/airband``.

    ``pluto`` is ``host`` or ``host:port`` (default port 8000)."""
    host = pluto if ":" in pluto else f"{pluto}:8000"
    url = f"http://{host}/api/airband"
    with urlopen(url, timeout=timeout) as resp:  # noqa: S310 (trusted LAN device)
        doc = json.load(resp)
    return parse_plan(doc)
