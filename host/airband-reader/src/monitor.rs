//! Low-latency direct-PCM debug monitor.
//!
//! A tiny local HTTP server that streams one selectable channel as raw s16 WAV
//! (no Icecast, no MP3 — those are the source of the 30–60 s feed latency), so
//! an operator can listen live **in the same process as the feeder**, without
//! opening a second client to the Pluto's single-client `:30000`.
//!
//! ```text
//! GET /listen/<ch>.wav?tap=pre|post
//!   pre  = raw demod audio (pre-DSP, pre-squelch), continuous, lowest latency
//!   post = fully-enhanced post-DFN audio (what LiveATC gets), squelch-gated
//! ```
//!
//! End-to-end latency is then the client's buffer, e.g.:
//! ```text
//! ffplay -fflags nobuffer -flags low_delay -probesize 32 -analyzeduration 0 \
//!   http://<pi>:<port>/listen/3.wav?tap=pre
//! ```

use crate::{Msg, Tap};
use std::io::{BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Sender};
use std::thread;

/// Streaming WAV header (mono, 16-bit, `rate` Hz). The RIFF/data sizes are set
/// to `0xFFFFFFFF` because the length is unbounded; `ffplay`/VLC/`sox` stream it
/// until the connection closes.
fn wav_header(rate: u32) -> Vec<u8> {
    let byte_rate = rate * 2;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    h.extend_from_slice(b"WAVE");
    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&1u16.to_le_bytes()); // mono
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&byte_rate.to_le_bytes());
    h.extend_from_slice(&2u16.to_le_bytes()); // block align
    h.extend_from_slice(&16u16.to_le_bytes()); // bits/sample
    h.extend_from_slice(b"data");
    h.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    h
}

/// Parses `GET /listen/<ch>.wav?tap=pre|post` → `(channel, tap)`.
fn parse_request(req: &str) -> Option<(usize, Tap)> {
    let path = req.split_whitespace().nth(1)?;
    let (route, query) = match path.split_once('?') {
        Some((r, q)) => (r, q),
        None => (path, ""),
    };
    let rest = route.strip_prefix("/listen/")?;
    let num = rest.strip_suffix(".wav").unwrap_or(rest);
    let ch: usize = num.parse().ok()?;
    // Default to the continuous, always-on pre tap (best for matching the
    // waterfall); ?tap=post selects the squelch-gated enhanced audio.
    let tap = if query.contains("tap=post") { Tap::Post } else { Tap::Pre };
    Some((ch, tap))
}

/// Handles one monitor client: registers a sink in the target channel's worker,
/// streams WAV until the client disconnects, then returns (dropping the receiver
/// so the worker prunes the dead sink on its next send).
fn handle(mut stream: TcpStream, senders: &[Sender<Msg>], rate: u32) {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);

    let Some((ch, tap)) = parse_request(&req) else {
        let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
        return;
    };
    if ch >= senders.len() {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
        return;
    }

    // ~1 s of slack; drop under back-pressure rather than stalling the worker.
    let (tx, rx) = mpsc::sync_channel::<i16>(rate as usize);
    if senders[ch].send(Msg::AddMonitor { tap, tx }).is_err() {
        return;
    }
    eprintln!("monitor: ch{ch:02} {tap:?} client connected");

    let header = "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    if stream.write_all(header.as_bytes()).is_err() || stream.write_all(&wav_header(rate)).is_err() {
        return;
    }

    // Flush roughly every 20 ms so the player buffer fills promptly.
    let chunk = (rate / 50).max(1) as usize;
    let mut w = BufWriter::new(stream);
    let mut since_flush = 0usize;
    while let Ok(s) = rx.recv() {
        if w.write_all(&s.to_le_bytes()).is_err() {
            break;
        }
        since_flush += 1;
        if since_flush >= chunk {
            if w.flush().is_err() {
                break;
            }
            since_flush = 0;
        }
    }
    eprintln!("monitor: ch{ch:02} client disconnected");
}

/// Spawns the monitor HTTP server on `port`. `senders` routes a registration to
/// the per-channel worker; `rate` is the channel audio rate (WAV header).
pub fn serve(port: u16, senders: Vec<Sender<Msg>>, rate: u32) {
    thread::spawn(move || {
        let listener = match TcpListener::bind(("0.0.0.0", port)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("monitor: failed to bind port {port}: {e}");
                return;
            }
        };
        eprintln!("monitor: serving /listen/<ch>.wav?tap=pre|post on :{port}");
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let senders = senders.clone();
            thread::spawn(move || handle(stream, &senders, rate));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channel_and_tap() {
        assert_eq!(parse_request("GET /listen/3.wav?tap=pre HTTP/1.1").unwrap().0, 3);
        assert!(matches!(parse_request("GET /listen/3.wav?tap=post HTTP/1.1").unwrap().1, Tap::Post));
        // default tap is pre
        assert!(matches!(parse_request("GET /listen/0.wav HTTP/1.1").unwrap().1, Tap::Pre));
        assert!(parse_request("GET /favicon.ico HTTP/1.1").is_none());
    }
}
