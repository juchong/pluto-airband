//! Interactive live listener for the Pluto airband multichannel receiver.
//!
//! Connects to the maia-httpd framed-audio TCP stream (default
//! `192.168.2.1:30000`), demultiplexes the 64-bit records, plays ONE selected
//! channel to the default audio output, and shows a live per-channel level
//! meter. Switch channels on the fly to audition each frequency.
//!
//! Frame layout (little-endian 64-bit word, see `hdl/audio_framer.py`):
//! ```text
//!   bits [31:0]  audio sample (signed, 24-bit content sign-extended to 32)
//!   bits [39:32] channel index (0..N-1)
//!   bits [63:40] per-channel sequence counter (wraps at 2**24)
//! ```
//!
//! Keys:
//!   ↑/↓ or j/k or [/]   previous / next channel
//!   0-9 then Enter      jump to a channel number (Esc cancels)
//!   +/-                 louder / quieter
//!   m                   mute toggle
//!   q                   quit

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use rodio::{OutputStream, Sink, Source};
use std::{
    collections::VecDeque,
    io::{stdout, BufReader, Read, Write},
    net::TcpStream,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// 2**23 — full scale of the signed 24-bit audio sample.
const SAMPLE_SCALE: f32 = 8_388_608.0;
/// Per-channel audio rate: the channelizer is fed IQ at Fs = 14 Msps and
/// decimates by 896 (128 lane CIC * 7 audio) -> 15625 sps. (The pre-fix
/// bitstream silently halved the lane input rate to ~7 Msps -> 7813 sps; the
/// pipelined-lane fix processes every input, restoring 15625 sps and correct
/// NCO tuning. Pass --rate to override for an older bitstream.)
const DEFAULT_RATE: u32 = 15625;

/// Default channel plan (MHz), matching `firmware/airband.json`.
const FREQS_MHZ: [f64; 21] = [
    118.050, 119.200, 119.900, 120.100, 120.400, 120.950, 121.500, 121.600, 121.700, 122.275,
    122.950, 122.975, 123.900, 124.700, 125.600, 125.900, 126.250, 126.500, 126.875, 127.100,
    128.500,
];

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Pluto airband stream address (host:port)
    #[arg(default_value = "192.168.2.1:30000")]
    addr: String,
    /// Number of channels in the stream
    #[arg(long, default_value_t = 21)]
    channels: usize,
    /// Channel to start listening on
    #[arg(long, default_value_t = 0)]
    channel: usize,
    /// Per-channel audio sample rate in Hz (AD9361 Fs / 128 / 7)
    #[arg(long, default_value_t = DEFAULT_RATE)]
    rate: u32,
    /// Initial playback gain (linear; airband audio is quiet, adjust with +/-)
    #[arg(long, default_value_t = 30.0)]
    gain: f32,
}

/// State shared between the network reader thread, the audio source, and the UI.
struct Shared {
    selected: AtomicUsize,
    running: AtomicBool,
    connected: AtomicBool,
    /// Mono f32 samples for the currently selected channel, fed to the audio sink.
    audio: Mutex<VecDeque<f32>>,
    /// Per-channel peak magnitude (f32 bits), reset each UI frame.
    peak: Vec<AtomicU32>,
    /// Per-channel cumulative dropped-sample count (from the seq counter).
    drops: Vec<AtomicU64>,
    /// Total records received (liveness indicator).
    records: AtomicU64,
    /// Max samples to keep queued (bounds playback latency).
    max_queue: usize,
}

impl Shared {
    fn new(channels: usize, max_queue: usize, start: usize) -> Shared {
        Shared {
            selected: AtomicUsize::new(start),
            running: AtomicBool::new(true),
            connected: AtomicBool::new(false),
            audio: Mutex::new(VecDeque::with_capacity(max_queue)),
            peak: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            drops: (0..channels).map(|_| AtomicU64::new(0)).collect(),
            records: AtomicU64::new(0),
            max_queue,
        }
    }
}

/// Lock-free `peak = max(peak, v)` for the meters.
fn atomic_max_f32(a: &AtomicU32, v: f32) {
    let mut cur = a.load(Ordering::Relaxed);
    loop {
        if v <= f32::from_bits(cur) {
            return;
        }
        match a.compare_exchange_weak(cur, v.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(e) => cur = e,
        }
    }
}

/// An endless rodio source that drains the shared selected-channel queue,
/// emitting silence on underrun so the sink never stops.
struct ChannelSource {
    shared: Arc<Shared>,
    rate: u32,
}

impl Iterator for ChannelSource {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let mut q = self.shared.audio.lock().unwrap();
        Some(q.pop_front().unwrap_or(0.0))
    }
}

impl Source for ChannelSource {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        1
    }
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Connects (and reconnects) to the stream, demuxing records into the shared
/// state. Runs until `shared.running` is cleared.
fn reader_loop(shared: Arc<Shared>, addr: String, n: usize) {
    let mut last_seq = vec![u32::MAX; n];
    while shared.running.load(Ordering::Relaxed) {
        match TcpStream::connect(&addr) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                shared.connected.store(true, Ordering::Relaxed);
                last_seq.iter_mut().for_each(|s| *s = u32::MAX);
                let mut r = BufReader::new(stream);
                let mut buf = [0u8; 8];
                while shared.running.load(Ordering::Relaxed) {
                    if r.read_exact(&mut buf).is_err() {
                        break;
                    }
                    let w = u64::from_le_bytes(buf);
                    let sample = (w & 0xffff_ffff) as u32 as i32;
                    let chan = ((w >> 32) & 0xff) as usize;
                    let seq = ((w >> 40) & 0xff_ffff) as u32;
                    if chan >= n {
                        continue;
                    }
                    shared.records.fetch_add(1, Ordering::Relaxed);

                    let prev = last_seq[chan];
                    if prev != u32::MAX {
                        let d = seq.wrapping_sub(prev) & 0xff_ffff;
                        if d > 1 {
                            shared.drops[chan].fetch_add((d - 1) as u64, Ordering::Relaxed);
                        }
                    }
                    last_seq[chan] = seq;

                    let mag = (sample.unsigned_abs() as f32) / SAMPLE_SCALE;
                    atomic_max_f32(&shared.peak[chan], mag);

                    if chan == shared.selected.load(Ordering::Relaxed) {
                        let f = (sample as f32) / SAMPLE_SCALE;
                        let mut q = shared.audio.lock().unwrap();
                        q.push_back(f);
                        while q.len() > shared.max_queue {
                            q.pop_front();
                        }
                    }
                }
                shared.connected.store(false, Ordering::Relaxed);
            }
            Err(_) => thread::sleep(Duration::from_millis(800)),
        }
        if shared.running.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(400));
        }
    }
}

fn freq_label(ch: usize) -> String {
    FREQS_MHZ
        .get(ch)
        .map(|f| format!("{f:>8.3} MHz"))
        .unwrap_or_else(|| "    —     ".to_string())
}

/// Switches the listened channel: update selection and flush stale audio.
fn select_channel(shared: &Shared, ch: usize) {
    shared.selected.store(ch, Ordering::Relaxed);
    shared.audio.lock().unwrap().clear();
}

fn render(shared: &Shared, n: usize, gain: f32, muted: bool, entry: &str) -> Result<()> {
    let sel = shared.selected.load(Ordering::Relaxed);
    let connected = shared.connected.load(Ordering::Relaxed);
    let records = shared.records.load(Ordering::Relaxed);
    let mut out = stdout();
    queue!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;

    let status = if connected { "connected" } else { "connecting…" };
    let vol = if muted {
        "MUTED".to_string()
    } else {
        format!("gain x{gain:.0}")
    };
    queue!(
        out,
        SetForegroundColor(Color::Cyan),
        Print(format!(
            "Pluto airband live listener — {status} — {vol} — {records} recs\r\n"
        )),
        ResetColor,
        Print("↑/↓ select   0-9+Enter jump   +/- gain   m mute   q quit\r\n\r\n"),
    )?;

    for ch in 0..n {
        let peak = f32::from_bits(shared.peak[ch].swap(0, Ordering::Relaxed));
        let drops = shared.drops[ch].load(Ordering::Relaxed);
        let level = (peak * gain).clamp(0.0, 1.0);
        let bars = (level * 24.0).round() as usize;
        let meter: String = "#".repeat(bars) + &"·".repeat(24 - bars);
        let marker = if ch == sel { "▶" } else { " " };
        let color = if ch == sel {
            Color::Green
        } else {
            Color::Grey
        };
        queue!(
            out,
            SetForegroundColor(color),
            Print(format!(
                "{marker} ch {ch:>2}  {}  [{meter}]  drops {drops}\r\n",
                freq_label(ch)
            )),
            ResetColor,
        )?;
    }

    if !entry.is_empty() {
        queue!(
            out,
            Print(format!("\r\njump to channel: {entry}_\r\n")),
        )?;
    }
    out.flush()?;
    Ok(())
}

/// Restores the terminal on drop (raw mode off, leave alternate screen).
struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(stdout(), cursor::Show, terminal::LeaveAlternateScreen);
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let n = args.channels;
    anyhow::ensure!(n > 0, "channels must be > 0");
    let start = args.channel.min(n - 1);

    // ~0.4 s of queued audio caps playback latency on channel switches.
    let max_queue = (args.rate as usize * 2) / 5;
    let shared = Arc::new(Shared::new(n, max_queue, start));

    // Audio output: keep _stream alive for the lifetime of the sink.
    let (_stream, handle) = OutputStream::try_default()
        .context("no default audio output device (is one available?)")?;
    let sink = Sink::try_new(&handle).context("failed to create audio sink")?;
    let mut gain = args.gain.max(0.0);
    let mut muted = false;
    sink.set_volume(gain);
    sink.append(ChannelSource {
        shared: Arc::clone(&shared),
        rate: args.rate,
    });

    let reader = {
        let shared = Arc::clone(&shared);
        let addr = args.addr.clone();
        thread::spawn(move || reader_loop(shared, addr, n))
    };

    terminal::enable_raw_mode().context("failed to enter raw terminal mode")?;
    execute!(stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
    let _guard = TermGuard;

    let mut entry = String::new();
    loop {
        render(&shared, n, gain, muted, &entry)?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Release {
                    continue;
                }
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc if entry.is_empty() => break,
                    KeyCode::Esc => entry.clear(),
                    KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('[') => {
                        select_channel(&shared, (shared.selected.load(Ordering::Relaxed) + n - 1) % n);
                    }
                    KeyCode::Down | KeyCode::Char('j') | KeyCode::Char(']') => {
                        select_channel(&shared, (shared.selected.load(Ordering::Relaxed) + 1) % n);
                    }
                    KeyCode::Char(c @ '0'..='9') => {
                        entry.push(c);
                        if let Ok(v) = entry.parse::<usize>() {
                            // Auto-commit when no larger valid channel can be formed.
                            if v >= n || v * 10 >= n {
                                if v < n {
                                    select_channel(&shared, v);
                                }
                                entry.clear();
                            }
                        }
                    }
                    KeyCode::Enter => {
                        if let Ok(v) = entry.parse::<usize>() {
                            if v < n {
                                select_channel(&shared, v);
                            }
                        }
                        entry.clear();
                    }
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        gain = (gain * 1.5).min(100_000.0);
                        if !muted {
                            sink.set_volume(gain);
                        }
                    }
                    KeyCode::Char('-') | KeyCode::Char('_') => {
                        gain = (gain / 1.5).max(0.1);
                        if !muted {
                            sink.set_volume(gain);
                        }
                    }
                    KeyCode::Char('m') => {
                        muted = !muted;
                        sink.set_volume(if muted { 0.0 } else { gain });
                    }
                    _ => {}
                }
            }
        }
    }

    shared.running.store(false, Ordering::Relaxed);
    let _ = reader.join();
    Ok(())
}
