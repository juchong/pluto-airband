"""Command-line entry point."""

from __future__ import annotations

import argparse
import sys

from .plan import enrich_from_feeds, fetch_plan


def _build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="airband-monitor",
        description=(
            "Interactive listener for the Pluto airband receiver. Streams a "
            "channel's pre- or post-filtered audio from the Pi's airband-reader "
            "monitor endpoint; the channel plan is read from the Pluto."
        ),
    )
    p.add_argument(
        "pi",
        metavar="PI_HOST[:PORT]",
        help="airband-reader monitor endpoint; port defaults to 8082 (--monitor-port).",
    )
    p.add_argument(
        "--pluto",
        default="192.168.2.1:8000",
        help="Pluto maia-httpd address for the channel plan (default: %(default)s).",
    )
    p.add_argument(
        "--tap",
        choices=("pre", "post"),
        default="pre",
        help="Initial tap: pre = raw demod, post = enhanced/gated (default: %(default)s).",
    )
    p.add_argument(
        "--channel",
        type=int,
        default=0,
        help="Initial channel index (default: %(default)s).",
    )
    p.add_argument(
        "--feeds",
        metavar="FEEDS_JSON",
        help="Optional feeds.json to fill in blank channel labels.",
    )
    p.add_argument(
        "--record-dir",
        metavar="DIR",
        default="./out",
        help="Directory for WAV recordings (default: %(default)s, created on demand).",
    )
    p.add_argument(
        "--record",
        action="store_true",
        help="Start recording the initial tap immediately.",
    )
    p.add_argument(
        "--no-tui",
        action="store_true",
        help="Headless: play the selected channel/tap without the curses UI.",
    )
    return p


DEFAULT_MONITOR_PORT = 8082  # matches deploy/airband-feeds.service (--monitor-port)


def with_default_port(s: str, default: int) -> str:
    """Return ``host:port``, applying ``default`` when no port was given.

    A bare host gets the default port; an explicit valid port is kept; a
    malformed ``host:port`` (non-numeric / out-of-range) raises. ponytail:
    IPv4/hostnames only, not bracketed IPv6 (the LAN devices here use names)."""
    host, sep, port = s.rpartition(":")
    if sep != ":":
        return f"{s}:{default}"
    if port.isdigit() and 0 < int(port) < 65536:
        return s
    raise ValueError(f"invalid port in {s!r}")


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)

    # The reader's monitor port defaults to 8082 (the committed deploy value);
    # accept a bare host and fill it in, but reject a clearly-malformed port.
    try:
        args.pi = with_default_port(args.pi, DEFAULT_MONITOR_PORT)
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2

    try:
        channels = fetch_plan(args.pluto)
    except Exception as e:  # noqa: BLE001
        print(f"error: could not read channel plan from {args.pluto}: {e}", file=sys.stderr)
        return 1
    if not channels:
        print(f"error: channel plan from {args.pluto} is empty", file=sys.stderr)
        return 1
    if args.feeds:
        try:
            channels = enrich_from_feeds(channels, args.feeds)
        except Exception as e:  # noqa: BLE001
            print(f"warning: could not read {args.feeds}: {e}", file=sys.stderr)

    if not 0 <= args.channel < len(channels):
        print(
            f"error: --channel {args.channel} out of range 0..{len(channels) - 1}",
            file=sys.stderr,
        )
        return 2

    # Pre-flight the monitor endpoint so a wrong host/port or a reader without
    # --monitor-port fails now with a clear message, instead of opening a UI
    # that just spins on "reconnecting".
    from .stream import monitor_url, open_stream

    url = monitor_url(args.pi, args.channel, args.tap)
    try:
        _rate, resp = open_stream(url, timeout=8.0)
        resp.close()
    except Exception as e:  # noqa: BLE001
        print(
            f"error: could not open monitor stream {url}: {e}\n"
            f"       is airband-reader running on {args.pi} with --monitor-port?",
            file=sys.stderr,
        )
        return 1

    from .app import MonitorApp, run_headless, run_tui

    app = MonitorApp(
        args.pi,
        channels,
        channel=args.channel,
        tap=args.tap,
        record_dir=args.record_dir,
    )
    if args.record:
        app.toggle_record()  # start recording the initial tap

    if args.no_tui:
        run_headless(app)
    else:
        run_tui(app)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
