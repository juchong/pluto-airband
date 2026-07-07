"""Assert-based self-checks (no framework). Run: `uv run python tests/test_core.py`.

Covers the pure logic that would silently corrupt audio or channel mapping if
it broke: WAV-header parsing, s16le dBFS math, /api/airband + airband.json plan
parsing (incl. the SD-card fallback and feeds.json enrichment), and the monitor
URL builder.
"""

import json
import math
import os
import struct
import sys
import tempfile

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "src"))

from airband_monitor.cli import with_default_port  # noqa: E402
from airband_monitor.plan import Channel, enrich_from_feeds, parse_plan  # noqa: E402
from airband_monitor.stream import (  # noqa: E402
    WAV_HEADER_LEN,
    monitor_url,
    parse_header_rate,
    peak_dbfs,
)


def _wav_header(rate: int) -> bytes:
    return (
        b"RIFF" + struct.pack("<I", 0xFFFFFFFF) + b"WAVE"
        + b"fmt " + struct.pack("<IHHIIHH", 16, 1, 1, rate, rate * 2, 2, 16)
        + b"data" + struct.pack("<I", 0xFFFFFFFF)
    )


def test_header_rate():
    h = _wav_header(20000)
    assert len(h) == WAV_HEADER_LEN, len(h)
    assert parse_header_rate(h) == 20000
    try:
        parse_header_rate(b"not a wav header........................xxxx")
        assert False, "expected ValueError"
    except ValueError:
        pass


def test_peak_dbfs():
    assert peak_dbfs(b"") == float("-inf")
    assert peak_dbfs(struct.pack("<hhhh", 0, 0, 0, 0)) == float("-inf")
    # full-scale negative sample -> ~0 dBFS
    assert abs(peak_dbfs(struct.pack("<h", -32768)) - 0.0) < 1e-6
    # half scale -> ~ -6 dBFS
    assert abs(peak_dbfs(struct.pack("<h", 16384)) - (-6.02)) < 0.05
    # a trailing odd byte must be ignored, not crash
    assert peak_dbfs(struct.pack("<h", 16384) + b"\x01") == peak_dbfs(struct.pack("<h", 16384))


def test_parse_plan_api():
    doc = {
        "center_hz": 126400000,
        "channels": [
            {"freq_hz": 119200000.0, "label": "KSEA Dep East"},
            {"freq_hz": 121500000.0},
        ],
    }
    chans = parse_plan(doc)
    assert [c.index for c in chans] == [0, 1]
    assert chans[0].label == "KSEA Dep East"
    assert abs(chans[0].freq_mhz - 119.2) < 1e-9
    assert chans[1].label == ""


def test_parse_plan_sdcard_fallback():
    doc = {"channels_hz": [119200000, 121500000], "channel_labels": ["A", ""]}
    chans = parse_plan(doc)
    assert len(chans) == 2
    assert chans[0].label == "A"
    assert chans[1].label == ""
    assert abs(chans[1].freq_mhz - 121.5) < 1e-9


def test_enrich_from_feeds():
    chans = [Channel(0, 119200000.0, ""), Channel(1, 121500000.0, "keep")]
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "feeds.json")
        with open(p, "w") as f:
            json.dump(
                {"feeds": [
                    {"channel": 0, "name": "Dep East"},
                    {"channel": 0, "name": "second wins? no"},
                    {"channel": 1, "name": "should not override"},
                ]},
                f,
            )
        out = enrich_from_feeds(chans, p)
    assert out[0].label == "Dep East"  # filled from first feed for ch0
    assert out[1].label == "keep"  # existing Pluto label preserved


def test_with_default_port():
    assert with_default_port("rfpi.chongflix.tv", 8082) == "rfpi.chongflix.tv:8082"
    assert with_default_port("rfpi.chongflix.tv:9000", 8082) == "rfpi.chongflix.tv:9000"
    assert with_default_port("10.0.0.5", 8082) == "10.0.0.5:8082"
    for bad in ("host:abc", "host:0", "host:99999"):
        try:
            with_default_port(bad, 8082)
            assert False, f"expected ValueError for {bad}"
        except ValueError:
            pass


def test_choose_scan_target():
    from airband_monitor.app import choose_scan_target

    chans = [
        {"ch": 0, "open": False, "carrier_dbc": 0.0},
        {"ch": 1, "open": True, "carrier_dbc": 5.0},
        {"ch": 2, "open": True, "carrier_dbc": 12.0},
    ]
    # current is open -> stay
    assert choose_scan_target(chans, current=2, idle_secs=99, hang=1.5) is None
    # current closed but within hang -> stay
    assert choose_scan_target(chans, current=0, idle_secs=0.5, hang=1.5) is None
    # current closed past hang -> hop to strongest open (ch2 @ 12 dBc)
    assert choose_scan_target(chans, current=0, idle_secs=2.0, hang=1.5) == 2
    # nothing open -> stay
    idle = [{"ch": i, "open": False, "carrier_dbc": 0.0} for i in range(3)]
    assert choose_scan_target(idle, current=0, idle_secs=9, hang=1.5) is None
    # only current open would be a hop to itself -> None
    one = [{"ch": 0, "open": True, "carrier_dbc": 3.0}]
    assert choose_scan_target(one, current=0, idle_secs=9, hang=1.5) is None


def test_monitor_url():
    assert monitor_url("pi.local:8081", 3, "pre") == "http://pi.local:8081/listen/3.wav?tap=pre"
    assert monitor_url("10.0.0.5:9000", 0, "post") == "http://10.0.0.5:9000/listen/0.wav?tap=post"
    try:
        monitor_url("pi:1", 0, "bogus")
        assert False, "expected ValueError"
    except ValueError:
        pass


def main():
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    for t in tests:
        t()
        print(f"ok  {t.__name__}")
    print(f"\n{len(tests)} checks passed")


if __name__ == "__main__":
    main()
