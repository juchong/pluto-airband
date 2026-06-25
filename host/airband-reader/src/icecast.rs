//! Icecast MP3 source client for LiveATC-style feeds.
//!
//! A background thread receives processed PCM for one channel, linearly
//! resamples it from the airband audio rate to the Icecast output rate, encodes
//! it to constant-bitrate MP3 with LAME, and pushes it to an Icecast mount using
//! the classic `SOURCE` protocol (what LiveATC.net expects). It reconnects on
//! any failure so it can run unattended.
//!
//! Defaults (16 kbps mono, 22050 Hz) match LiveATC's feed requirements.

use mp3lame_encoder::{Bitrate, Builder, MonoPcm, Mode, Quality};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// How the source connection negotiates TLS to the Icecast server. Values mirror
/// RTLSDR-Airband's libshout `tls` option.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TlsMode {
    /// Plain TCP, no TLS (default; matches a plain Icecast source port).
    #[default]
    Disabled,
    /// Implicit TLS: handshake immediately on connect (a TLS-only listener).
    Transport,
    /// Try implicit TLS; fall back to plain TCP if the handshake fails.
    Auto,
    /// Try implicit TLS; never fall back to plain.
    AutoNoPlain,
    /// RFC 2817 in-band upgrade: request `Upgrade: TLS/1.0`, then go TLS.
    Upgrade,
}

impl TlsMode {
    /// Parses RTLSDR-Airband's `tls` value strings.
    pub fn parse(s: &str) -> Result<TlsMode, String> {
        match s {
            "disabled" => Ok(TlsMode::Disabled),
            "transport" => Ok(TlsMode::Transport),
            "auto" => Ok(TlsMode::Auto),
            "auto_no_plain" => Ok(TlsMode::AutoNoPlain),
            "upgrade" => Ok(TlsMode::Upgrade),
            other => Err(format!(
                "invalid tls value {other:?}; must be one of: disabled, transport, auto, auto_no_plain, upgrade"
            )),
        }
    }
}

/// Icecast source configuration.
#[derive(Clone)]
pub struct IcecastConfig {
    pub host: String,
    pub port: u16,
    pub mount: String,
    pub user: String,
    pub password: String,
    pub bitrate: u32,
    /// MP3 output sample rate (must be a LAME-supported rate, e.g. 22050).
    pub out_rate: u32,
    /// Source audio sample rate (the airband channel rate).
    pub in_rate: u32,
    pub name: String,
    /// Optional ICY genre tag.
    pub genre: Option<String>,
    /// Optional ICY description tag.
    pub description: Option<String>,
    /// TLS negotiation mode.
    pub tls: TlsMode,
    /// Testing only: when true, accept invalid/self-signed certs and hostname
    /// mismatches. Disables MITM protection — never use for a public feed.
    pub tls_insecure: bool,
    pub channel: usize,
}

/// A source connection that is either plain TCP or a TLS stream over TCP.
enum Conn {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

/// Linear up/down sampler driven one input sample at a time.
struct Resampler {
    /// Input-sample advance per output sample (`in_rate / out_rate`).
    step: f64,
    /// Next output time, in input-sample units.
    next_out: f64,
    /// Index of the most recent input sample.
    in_idx: f64,
    prev: f32,
    started: bool,
}

impl Resampler {
    fn new(in_rate: u32, out_rate: u32) -> Resampler {
        Resampler {
            step: in_rate as f64 / out_rate as f64,
            next_out: 0.0,
            in_idx: 0.0,
            prev: 0.0,
            started: false,
        }
    }

    /// Feeds one input sample, emitting zero or more output samples into `out`.
    fn push(&mut self, x: f32, out: &mut Vec<i16>) {
        if !self.started {
            self.started = true;
            self.prev = x;
            self.in_idx = 0.0;
            return;
        }
        let cur_idx = self.in_idx + 1.0;
        while self.next_out < cur_idx {
            let frac = (self.next_out - self.in_idx) as f32;
            let y = self.prev + (x - self.prev) * frac;
            out.push((y.clamp(-1.0, 1.0) * 32767.0) as i16);
            self.next_out += self.step;
        }
        self.in_idx = cur_idx;
        self.prev = x;
    }
}

fn bitrate_enum(kbps: u32) -> Bitrate {
    match kbps {
        8 => Bitrate::Kbps8,
        16 => Bitrate::Kbps16,
        24 => Bitrate::Kbps24,
        32 => Bitrate::Kbps32,
        40 => Bitrate::Kbps40,
        48 => Bitrate::Kbps48,
        64 => Bitrate::Kbps64,
        96 => Bitrate::Kbps96,
        128 => Bitrate::Kbps128,
        _ => Bitrate::Kbps16,
    }
}

/// RFC 4648 base64 (standard alphabet) for the HTTP Basic auth header.
fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Validates the first line of an Icecast SOURCE response. Returns `Ok` on a
/// `2xx` status, `Err` (with the status text) otherwise. Pulled out of `connect`
/// so it can be unit-tested without a socket.
fn parse_status(resp: &[u8]) -> std::io::Result<()> {
    let text = String::from_utf8_lossy(resp);
    let line = text.lines().next().unwrap_or("").trim();
    // Expected: "HTTP/1.0 200 OK". The status code is the second token.
    let code = line.split_whitespace().nth(1).and_then(|c| c.parse::<u16>().ok());
    match code {
        Some(c) if (200..300).contains(&c) => Ok(()),
        Some(_) => Err(std::io::Error::other(format!("server rejected SOURCE: {line}"))),
        None => Err(std::io::Error::other(format!(
            "unrecognized SOURCE response: {:?}",
            line
        ))),
    }
}

/// Builds the TLS connector, honoring the insecure (testing) flag.
fn tls_connector(cfg: &IcecastConfig) -> std::io::Result<native_tls::TlsConnector> {
    let mut b = native_tls::TlsConnector::builder();
    if cfg.tls_insecure {
        b.danger_accept_invalid_certs(true);
        b.danger_accept_invalid_hostnames(true);
    }
    b.build().map_err(std::io::Error::other)
}

/// Wraps an established TCP stream in TLS (implicit handshake).
fn tls_wrap(cfg: &IcecastConfig, tcp: TcpStream) -> std::io::Result<Conn> {
    let connector = tls_connector(cfg)?;
    let tls = connector
        .connect(cfg.host.as_str(), tcp)
        .map_err(std::io::Error::other)?;
    Ok(Conn::Tls(Box::new(tls)))
}

/// Opens the transport per the configured TLS mode (no SOURCE request yet).
fn open_conn(cfg: &IcecastConfig) -> std::io::Result<Conn> {
    match cfg.tls {
        TlsMode::Disabled => {
            if cfg.tls_insecure {
                eprintln!("icecast: tls_insecure ignored (tls is disabled)");
            }
            Ok(Conn::Plain(TcpStream::connect((cfg.host.as_str(), cfg.port))?))
        }
        TlsMode::Transport | TlsMode::AutoNoPlain => {
            let tcp = TcpStream::connect((cfg.host.as_str(), cfg.port))?;
            tls_wrap(cfg, tcp)
        }
        TlsMode::Auto => {
            let tcp = TcpStream::connect((cfg.host.as_str(), cfg.port))?;
            match tls_wrap(cfg, tcp) {
                Ok(c) => Ok(c),
                Err(e) => {
                    eprintln!("icecast: TLS handshake failed ({e}); falling back to plain");
                    Ok(Conn::Plain(TcpStream::connect((cfg.host.as_str(), cfg.port))?))
                }
            }
        }
        TlsMode::Upgrade => {
            // RFC 2817 in-band upgrade: ask the plain connection to switch to TLS,
            // expect "101 Switching Protocols", then handshake over the same socket.
            let mut tcp = TcpStream::connect((cfg.host.as_str(), cfg.port))?;
            let req = format!(
                "GET / HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: TLS/1.0\r\n\r\n",
                host = cfg.host,
            );
            tcp.write_all(req.as_bytes())?;
            tcp.flush()?;
            let mut buf = [0u8; 256];
            tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
            let n = tcp.read(&mut buf)?;
            let line = String::from_utf8_lossy(&buf[..n]);
            let first = line.lines().next().unwrap_or("");
            if !first.contains("101") {
                return Err(std::io::Error::other(format!(
                    "TLS upgrade refused: {}",
                    first.trim()
                )));
            }
            tcp.set_read_timeout(None)?;
            tls_wrap(cfg, tcp)
        }
    }
}

/// Opens the Icecast mount, sends the SOURCE request + headers, and verifies the
/// server accepted it (reads the HTTP status line).
fn connect(cfg: &IcecastConfig) -> std::io::Result<Conn> {
    let mut conn = open_conn(cfg)?;
    let auth = base64(format!("{}:{}", cfg.user, cfg.password).as_bytes());
    let mut req = format!(
        "SOURCE {mount} HTTP/1.0\r\n\
         Authorization: Basic {auth}\r\n\
         User-Agent: pluto-airband\r\n\
         Content-Type: audio/mpeg\r\n\
         ice-name: {name}\r\n\
         ice-public: 0\r\n\
         ice-audio-info: bitrate={br};channels=1;samplerate={sr}\r\n",
        mount = cfg.mount,
        auth = auth,
        name = cfg.name,
        br = cfg.bitrate,
        sr = cfg.out_rate,
    );
    if let Some(g) = cfg.genre.as_ref() {
        req.push_str(&format!("ice-genre: {g}\r\n"));
    }
    if let Some(d) = cfg.description.as_ref() {
        req.push_str(&format!("ice-description: {d}\r\n"));
    }
    req.push_str("\r\n");
    conn.write_all(req.as_bytes())?;
    conn.flush()?;

    // Read and validate the handshake response. Icecast replies with an HTTP
    // status line on accept (200) or reject (401/403/...). Some servers send
    // nothing until data flows, so a read timeout is treated as provisional-OK
    // rather than a hard failure.
    if let Conn::Plain(s) = &conn {
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
    }
    let mut buf = [0u8; 512];
    match conn.read(&mut buf) {
        Ok(0) => return Err(std::io::Error::other("server closed connection during SOURCE handshake")),
        Ok(n) => parse_status(&buf[..n])?,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {
            eprintln!("icecast: no handshake response yet (continuing)");
        }
        Err(e) => return Err(e),
    }
    if let Conn::Plain(s) = &conn {
        s.set_read_timeout(None)?;
    }
    Ok(conn)
}

/// Output samples to buffer before each MP3 encode call.
const ENCODE_CHUNK: usize = 4096;

/// Runs the encode/stream loop against a single connection until it errors.
fn stream_once(cfg: &IcecastConfig, rx: &Receiver<i16>) -> std::io::Result<()> {
    let mut sock = connect(cfg)?;
    eprintln!(
        "icecast: connected to {}:{}{} (ch {}, {} kbps, {} Hz)",
        cfg.host, cfg.port, cfg.mount, cfg.channel, cfg.bitrate, cfg.out_rate
    );

    let mut encoder = Builder::new()
        .and_then(|b| {
            b.with_num_channels(1)
                .and_then(|b| b.with_sample_rate(cfg.out_rate))
                .and_then(|b| b.with_output_sample_rate(std::num::NonZeroU32::new(cfg.out_rate)))
                .and_then(|b| b.with_brate(bitrate_enum(cfg.bitrate)))
                .and_then(|b| b.with_mode(Mode::Mono))
                .and_then(|b| b.with_quality(Quality::Good))
                .and_then(|b| b.build())
                .ok()
        })
        .ok_or_else(|| std::io::Error::other("failed to build LAME encoder"))?;

    let mut resampler = Resampler::new(cfg.in_rate, cfg.out_rate);
    let mut pcm: Vec<i16> = Vec::with_capacity(ENCODE_CHUNK * 2);
    let mut mp3: Vec<u8> = Vec::new();

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(s) => resampler.push(s as f32 / 32768.0, &mut pcm),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if pcm.len() >= ENCODE_CHUNK {
            mp3.clear();
            mp3.reserve(mp3lame_encoder::max_required_buffer_size(pcm.len()));
            if encoder.encode_to_vec(MonoPcm(&pcm), &mut mp3).is_err() {
                return Err(std::io::Error::other("mp3 encode failed"));
            }
            pcm.clear();
            if !mp3.is_empty() {
                sock.write_all(&mp3)?;
            }
        }
    }
}

/// Spawns the Icecast worker, returning a bounded sender for processed PCM.
///
/// The processing loop pushes one `i16` per channel sample with `try_send`;
/// when the worker is disconnected or behind, samples are dropped rather than
/// blocking the demux loop.
pub fn spawn(cfg: IcecastConfig) -> SyncSender<i16> {
    // ~1 s of audio of slack before back-pressure drops samples.
    let (tx, rx) = mpsc::sync_channel::<i16>(cfg.in_rate as usize);
    thread::spawn(move || loop {
        if let Err(e) = stream_once(&cfg, &rx) {
            eprintln!("icecast: stream error ({e}); reconnecting in 2s");
        }
        // Drain stale samples queued during the outage so we resume near-live.
        while rx.try_recv().is_ok() {}
        thread::sleep(Duration::from_secs(2));
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_mode_parse() {
        assert_eq!(TlsMode::parse("disabled").unwrap(), TlsMode::Disabled);
        assert_eq!(TlsMode::parse("transport").unwrap(), TlsMode::Transport);
        assert_eq!(TlsMode::parse("auto").unwrap(), TlsMode::Auto);
        assert_eq!(TlsMode::parse("auto_no_plain").unwrap(), TlsMode::AutoNoPlain);
        assert_eq!(TlsMode::parse("upgrade").unwrap(), TlsMode::Upgrade);
        assert!(TlsMode::parse("https").is_err());
    }

    #[test]
    fn parse_status_accepts_2xx_rejects_others() {
        assert!(parse_status(b"HTTP/1.0 200 OK\r\n\r\n").is_ok());
        assert!(parse_status(b"HTTP/1.1 200 OK\r\n").is_ok());
        let e = parse_status(b"HTTP/1.0 401 Authentication Required\r\n\r\n").unwrap_err();
        assert!(e.to_string().contains("401"));
        assert!(parse_status(b"HTTP/1.0 403 Forbidden\r\n").is_err());
        assert!(parse_status(b"garbage").is_err());
    }

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"source:hackme"), "c291cmNlOmhhY2ttZQ==");
    }

    #[test]
    fn resampler_upsamples_roughly_by_ratio() {
        let mut r = Resampler::new(15625, 22050);
        let mut out = Vec::new();
        let n = 15625;
        for i in 0..n {
            let x = ((i as f32) * 0.01).sin() * 0.5;
            r.push(x, &mut out);
        }
        // ~22050 outputs for ~15625 inputs (within a small margin).
        let expected = (n as f64 * 22050.0 / 15625.0) as usize;
        let diff = (out.len() as i64 - expected as i64).abs();
        assert!(diff < 50, "got {} outputs, expected ~{}", out.len(), expected);
    }
}
