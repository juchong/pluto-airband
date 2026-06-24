//! Interactive live listener for the Pluto airband multichannel receiver.
//!
//! Connects to the maia-httpd framed-audio TCP stream (default
//! `192.168.2.1:30000`), demultiplexes the 64-bit records, plays ONE selected
//! channel to the default audio output, and shows a live per-channel level
//! meter (dBFS) with a squelch-open indicator.
//!
//! Audio for the selected channel runs the shared `airband-dsp` chain:
//! per-channel **squelch** (gates inter-transmission static), a 300-3400 Hz
//! voice **band-pass**, an optional **notch**, and an **AGC** that normalizes
//! loudness and soft-clips peaks. The squelch runs on *every* channel so the
//! meter can show which frequencies are currently active.
//!
//! Frame layout (little-endian 64-bit word, see `hdl/audio_framer.py`):
//! ```text
//!   bits [31:0]  audio sample (signed, 24-bit content sign-extended to 32)
//!   bits [39:32] channel index (0..N-1)
//!   bits [63:40] per-channel sequence counter (wraps at 2**24)
//! ```
//!
//! Monitor modes (`--monitor`): `single` plays one channel; `follow` is a
//! scanner that auto-switches to whichever channel's squelch is open (with a
//! hang time); `mix` sums every open channel into one stream.
//!
//! Keys:
//!   ↑/↓ or j/k or [/]   previous / next channel
//!   0-9 then Enter      jump to a channel number (Esc cancels)
//!   +/-                 louder / quieter
//!   m                   mute toggle
//!   s                   squelch toggle
//!   a                   AGC toggle
//!   f                   voice band-pass toggle
//!   n                   notch toggle (only if --notch was given)
//!   F                   follow (scanner) toggle
//!   q                   quit

use airband_dsp::{
    level_to_dbfs, Agc, Notch, Squelch, SquelchConfig, SquelchMode, SquelchState, VoiceFilter,
    SAMPLE_SCALE,
};
use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
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
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// Per-channel audio rate: the channelizer is fed IQ at Fs = 14 Msps and
/// decimates by 896 (128 lane CIC * 7 audio) -> 15625 sps. (The pre-fix
/// bitstream silently halved the lane input rate to ~7 Msps -> 7813 sps; the
/// pipelined-lane fix processes every input, restoring 15625 sps and correct
/// NCO tuning. Pass --rate to override for an older bitstream.)
const DEFAULT_RATE: u32 = 15625;

/// Default TCP port of the maia-httpd airband stream, appended when the address
/// argument omits an explicit `:port` (so `10.0.16.183` works like
/// `10.0.16.183:30000`).
const DEFAULT_PORT: u16 = 30000;

/// Default channel plan (MHz), matching `firmware/airband.json`.
const FREQS_MHZ: [f64; 21] = [
    118.050, 119.200, 119.900, 120.100, 120.400, 120.950, 121.500, 121.600, 121.700, 122.275,
    122.950, 122.975, 123.900, 124.700, 125.600, 125.900, 126.250, 126.500, 126.875, 127.100,
    128.500,
];

/// Squelch mode selector for the CLI.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum SquelchArg {
    /// Always pass audio (no gating).
    Off,
    /// Automatic threshold tracking the noise floor (`--squelch-snr` dB above it).
    Auto,
    /// Fixed threshold at `--squelch-level` dBFS.
    Manual,
}

/// Monitor mode selector for the CLI.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Monitor {
    /// Play only the selected channel.
    Single,
    /// Scanner: auto-switch to whichever channel is currently active.
    Follow,
    /// Sum every open channel into a single stream.
    Mix,
}

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Pluto airband stream address (host[:port]; port defaults to 30000)
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
    /// Playback volume (linear sink gain). Defaults to 1.5 with AGC on (audio is
    /// already normalized) or 25000 with --no-agc (raw airband audio is quiet).
    #[arg(long)]
    gain: Option<f32>,
    /// Squelch mode: off, auto (noise-floor tracking), or manual (fixed dBFS).
    #[arg(long, value_enum, default_value_t = SquelchArg::Auto)]
    squelch: SquelchArg,
    /// Automatic-squelch open threshold, in dB of SNR above the noise floor.
    #[arg(long, default_value_t = 9.0)]
    squelch_snr: f32,
    /// Manual-squelch threshold in dBFS (used when --squelch manual).
    #[arg(long, default_value_t = -45.0, allow_hyphen_values = true)]
    squelch_level: f32,
    /// Squelch hang time in ms: stay open through gaps this short (bridges the
    /// pauses in continuous speech like AWOS/ATIS so it does not chatter).
    #[arg(long, default_value_t = 1000.0)]
    squelch_hang_ms: f32,
    /// Disable the AGC (play raw, band-passed audio scaled by the sink volume).
    #[arg(long)]
    no_agc: bool,
    /// Disable the voice band-pass (on by default; 300-3400 Hz, toggle with 'f').
    #[arg(long)]
    no_filter: bool,
    /// Voice band-pass low corner in Hz (high-pass)
    #[arg(long, default_value_t = 300.0)]
    filter_low: f64,
    /// Voice band-pass high corner in Hz (low-pass)
    #[arg(long, default_value_t = 3400.0)]
    filter_high: f64,
    /// Notch (band-stop) center frequency in Hz to kill a tonal spur (off if unset).
    #[arg(long)]
    notch: Option<f64>,
    /// Notch quality factor (higher = narrower).
    #[arg(long, default_value_t = 10.0)]
    notch_q: f64,
    /// Monitor mode: single channel, follow (scanner), or mix (sum open channels).
    #[arg(long, value_enum, default_value_t = Monitor::Single)]
    monitor: Monitor,
    /// Follow-mode hang time in ms: stay on a channel this long after it goes idle.
    #[arg(long, default_value_t = 2000)]
    follow_hang_ms: u64,
}

/// Immutable DSP parameters handed to the reader thread.
#[derive(Clone)]
struct DspCfg {
    rate: u32,
    low: f64,
    high: f64,
    squelch_mode: SquelchMode,
    squelch_hang_ms: f32,
    notch_freq: Option<f64>,
    notch_q: f64,
}

/// State shared between the network reader thread, the audio source, and the UI.
struct Shared {
    selected: AtomicUsize,
    running: AtomicBool,
    connected: AtomicBool,
    /// Mono f32 samples for the currently selected channel, fed to the audio sink.
    audio: Mutex<VecDeque<f32>>,
    /// Per-channel peak magnitude in raw 24-bit units (f32 bits), reset each frame.
    peak: Vec<AtomicU32>,
    /// Per-channel cumulative dropped-sample count (from the seq counter).
    drops: Vec<AtomicU64>,
    /// Per-channel squelch-open flag (for the activity indicator).
    sq_open: Vec<AtomicBool>,
    /// Squelch state of the selected channel (see `state_code`), for the header.
    sel_state: AtomicU8,
    /// Total records received (liveness indicator).
    records: AtomicU64,
    /// Max samples to keep queued (bounds playback latency).
    max_queue: usize,
    filter_on: AtomicBool,
    squelch_on: AtomicBool,
    agc_on: AtomicBool,
    notch_on: AtomicBool,
}

impl Shared {
    fn new(channels: usize, max_queue: usize, start: usize, toggles: Toggles) -> Shared {
        Shared {
            selected: AtomicUsize::new(start),
            running: AtomicBool::new(true),
            connected: AtomicBool::new(false),
            audio: Mutex::new(VecDeque::with_capacity(max_queue)),
            peak: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            drops: (0..channels).map(|_| AtomicU64::new(0)).collect(),
            sq_open: (0..channels).map(|_| AtomicBool::new(false)).collect(),
            sel_state: AtomicU8::new(0),
            records: AtomicU64::new(0),
            max_queue,
            filter_on: AtomicBool::new(toggles.filter),
            squelch_on: AtomicBool::new(toggles.squelch),
            agc_on: AtomicBool::new(toggles.agc),
            notch_on: AtomicBool::new(toggles.notch),
        }
    }
}

#[derive(Clone, Copy)]
struct Toggles {
    filter: bool,
    squelch: bool,
    agc: bool,
    notch: bool,
}

/// Per-sample DSP toggle snapshot handed to [`ChannelDsp::process`].
#[derive(Clone, Copy)]
struct ChainOpts {
    filter: bool,
    notch: bool,
    agc: bool,
}

fn state_code(s: SquelchState) -> u8 {
    match s {
        SquelchState::Closed => 0,
        SquelchState::Opening => 1,
        SquelchState::Open => 2,
        SquelchState::Closing => 3,
        SquelchState::LowSignalAbort => 4,
    }
}

fn state_label(code: u8) -> &'static str {
    match code {
        1 => "OPENING",
        2 => "OPEN",
        3 => "CLOSING",
        4 => "ABORT",
        _ => "CLOSED",
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

/// Pushes one mono sample to the playback queue, trimming to the latency bound.
fn push_audio(shared: &Shared, sample: f32) {
    let mut q = shared.audio.lock().unwrap();
    q.push_back(sample);
    while q.len() > shared.max_queue {
        q.pop_front();
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

/// Per-channel audio DSP (band-pass + optional notch + AGC). One instance per
/// channel so the listener can mix or scan without losing filter/AGC state.
struct ChannelDsp {
    vf: VoiceFilter,
    notch: Option<Notch>,
    agc: Agc,
}

impl ChannelDsp {
    fn new(cfg: &DspCfg) -> ChannelDsp {
        ChannelDsp {
            vf: VoiceFilter::new(cfg.rate as f64, cfg.low, cfg.high),
            notch: cfg
                .notch_freq
                .map(|f| Notch::new(f, cfg.rate as f64, cfg.notch_q)),
            agc: Agc::new(),
        }
    }

    /// Runs one sample through the gated chain, returning normalized audio.
    fn process(&mut self, sample: i32, open: bool, just_opened: bool, above: bool, opt: ChainOpts) -> f32 {
        if !open {
            return if opt.agc {
                self.agc.process(0.0, false, false, false)
            } else {
                0.0
            };
        }
        let mut v = sample as f64;
        if opt.filter {
            v = self.vf.process(v);
        }
        if opt.notch {
            if let Some(nf) = self.notch.as_mut() {
                v = nf.process(v);
            }
        }
        let vn = (v / SAMPLE_SCALE as f64) as f32;
        if opt.agc {
            self.agc.process(vn, true, just_opened, above)
        } else {
            vn
        }
    }
}

/// Connects (and reconnects) to the stream, demuxing records into the shared
/// state and running the DSP chain (selected channel, or all channels in mix
/// mode).
fn reader_loop(shared: Arc<Shared>, addr: String, n: usize, cfg: DspCfg, mix: bool) {
    let mut last_seq = vec![u32::MAX; n];
    let mut squelches: Vec<Squelch> = (0..n)
        .map(|_| {
            Squelch::new(
                SquelchConfig::new(cfg.squelch_mode, cfg.rate).with_hang_ms(cfg.squelch_hang_ms),
            )
        })
        .collect();
    let mut dsps: Vec<ChannelDsp> = (0..n).map(|_| ChannelDsp::new(&cfg)).collect();

    // Mix-mode accumulator: one contribution per channel, summed and emitted
    // once per audio tick (detected when the channel index wraps to a lower one).
    let mut mix_buf = vec![0f32; n];
    let mut prev_chan: Option<usize> = None;

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

                    let mag = sample.unsigned_abs() as f32;
                    atomic_max_f32(&shared.peak[chan], mag);

                    // Squelch runs on every channel so the meter shows activity.
                    let sq = &mut squelches[chan];
                    let st = sq.process(mag);
                    shared.sq_open[chan].store(sq.is_open(), Ordering::Relaxed);

                    let filter_on = shared.filter_on.load(Ordering::Relaxed);
                    let notch_on = shared.notch_on.load(Ordering::Relaxed);
                    let squelch_on = shared.squelch_on.load(Ordering::Relaxed);
                    let agc_on = shared.agc_on.load(Ordering::Relaxed);
                    let open = !squelch_on || sq.is_open();
                    let just_opened = squelch_on && sq.just_opened();
                    let above = mag >= sq.threshold();
                    let opt = ChainOpts {
                        filter: filter_on,
                        notch: notch_on,
                        agc: agc_on,
                    };

                    if mix {
                        // Emit the previous tick's sum when the index wraps.
                        if let Some(p) = prev_chan {
                            if chan <= p {
                                let s: f32 = mix_buf.iter().sum();
                                push_audio(&shared, s.clamp(-1.0, 1.0));
                                mix_buf.iter_mut().for_each(|x| *x = 0.0);
                            }
                        }
                        mix_buf[chan] = dsps[chan].process(sample, open, just_opened, above, opt);
                        prev_chan = Some(chan);
                        continue;
                    }

                    let sel = shared.selected.load(Ordering::Relaxed);
                    if chan != sel {
                        continue;
                    }
                    shared.sel_state.store(state_code(st), Ordering::Relaxed);
                    let out = dsps[chan].process(sample, open, just_opened, above, opt);
                    push_audio(&shared, out);
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

/// Normalizes the user-supplied address: if it has no `:port`, appends the
/// default airband port. This makes `airband-listen 10.0.16.183` behave like
/// `airband-listen 10.0.16.183:30000` instead of failing to resolve (a bare
/// host is not a valid `host:port` for `TcpStream::connect`, so the connect
/// loop would otherwise spin forever showing "connecting…").
fn normalize_addr(addr: &str) -> String {
    if addr.contains(':') {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_PORT}")
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

fn render(shared: &Shared, n: usize, gain: f32, muted: bool, monitor: Monitor, entry: &str) -> Result<()> {
    let sel = shared.selected.load(Ordering::Relaxed);
    let connected = shared.connected.load(Ordering::Relaxed);
    let records = shared.records.load(Ordering::Relaxed);
    let mut out = stdout();
    queue!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;

    let status = if connected { "connected" } else { "connecting…" };
    let mode = match monitor {
        Monitor::Single => "single",
        Monitor::Follow => "FOLLOW",
        Monitor::Mix => "MIX",
    };
    let vol = if muted {
        "MUTED".to_string()
    } else {
        format!("vol x{gain:.1}")
    };
    let sq = if shared.squelch_on.load(Ordering::Relaxed) {
        state_label(shared.sel_state.load(Ordering::Relaxed))
    } else {
        "SQ off"
    };
    let flags = format!(
        "{} {} {}",
        if shared.agc_on.load(Ordering::Relaxed) { "AGC" } else { "agc" },
        if shared.filter_on.load(Ordering::Relaxed) { "BPF" } else { "bpf" },
        if shared.notch_on.load(Ordering::Relaxed) { "NOTCH" } else { "notch" },
    );
    queue!(
        out,
        SetForegroundColor(Color::Cyan),
        Print(format!(
            "Pluto airband live listener — {status} — {vol} — sq:{sq} — {flags} — {mode} — {records} recs\r\n"
        )),
        ResetColor,
        Print("↑/↓ select  0-9+Enter jump  +/- vol  m mute  s squelch  a agc  f bpf  n notch  F follow  q quit\r\n\r\n"),
    )?;

    for ch in 0..n {
        let peak = f32::from_bits(shared.peak[ch].swap(0, Ordering::Relaxed));
        let drops = shared.drops[ch].load(Ordering::Relaxed);
        let db = level_to_dbfs(peak);
        // map -60..0 dBFS onto the 24-cell meter
        let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let bars = (frac * 24.0).round() as usize;
        let meter: String = "#".repeat(bars) + &"·".repeat(24 - bars);
        let marker = if ch == sel { "▶" } else { " " };
        let active = if shared.sq_open[ch].load(Ordering::Relaxed) {
            "●"
        } else {
            " "
        };
        let color = if ch == sel { Color::Green } else { Color::Grey };
        queue!(
            out,
            SetForegroundColor(color),
            Print(format!(
                "{marker}{active} ch {ch:>2}  {}  [{meter}] {db:>6.1} dBFS  drops {drops}\r\n",
                freq_label(ch)
            )),
            ResetColor,
        )?;
    }

    if !entry.is_empty() {
        queue!(out, Print(format!("\r\njump to channel: {entry}_\r\n")))?;
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

    let squelch_mode = match args.squelch {
        SquelchArg::Off => SquelchMode::Off,
        SquelchArg::Auto => SquelchMode::AutoSnr {
            snr_db: args.squelch_snr,
        },
        SquelchArg::Manual => SquelchMode::ManualDbfs(args.squelch_level),
    };
    let agc_on = !args.no_agc;
    let toggles = Toggles {
        filter: !args.no_filter,
        squelch: !matches!(args.squelch, SquelchArg::Off),
        agc: agc_on,
        notch: args.notch.is_some(),
    };
    let cfg = DspCfg {
        rate: args.rate,
        low: args.filter_low,
        high: args.filter_high,
        squelch_mode,
        squelch_hang_ms: args.squelch_hang_ms,
        notch_freq: args.notch,
        notch_q: args.notch_q,
    };

    // ~0.4 s of queued audio caps playback latency on channel switches.
    let max_queue = (args.rate as usize * 2) / 5;
    let shared = Arc::new(Shared::new(n, max_queue, start, toggles));

    // Audio output: keep _stream alive for the lifetime of the sink.
    let (_stream, handle) = OutputStream::try_default()
        .context("no default audio output device (is one available?)")?;
    let sink = Sink::try_new(&handle).context("failed to create audio sink")?;
    // With AGC the audio is already normalized to ~unity, so a small sink gain
    // suffices; without AGC the raw airband audio is very quiet and needs a big
    // makeup factor (the historical default).
    let mut gain = args.gain.unwrap_or(if agc_on { 1.5 } else { 25000.0 }).max(0.0);
    let mut muted = false;
    sink.set_volume(gain);
    sink.append(ChannelSource {
        shared: Arc::clone(&shared),
        rate: args.rate,
    });

    let mut monitor = args.monitor;
    let mix = monitor == Monitor::Mix;
    let reader = {
        let shared = Arc::clone(&shared);
        let addr = normalize_addr(&args.addr);
        let cfg = cfg.clone();
        thread::spawn(move || reader_loop(shared, addr, n, cfg, mix))
    };
    let hang = Duration::from_millis(args.follow_hang_ms);
    let mut follow_idle = Instant::now();

    terminal::enable_raw_mode().context("failed to enter raw terminal mode")?;
    execute!(stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
    let _guard = TermGuard;

    let mut entry = String::new();
    loop {
        // Scanner: stay on an active channel; after it goes idle for `hang`,
        // hop (round-robin) to the next channel whose squelch is open.
        if monitor == Monitor::Follow {
            let sel = shared.selected.load(Ordering::Relaxed);
            if shared.sq_open[sel].load(Ordering::Relaxed) {
                follow_idle = Instant::now();
            } else if follow_idle.elapsed() >= hang {
                for off in 1..=n {
                    let c = (sel + off) % n;
                    if shared.sq_open[c].load(Ordering::Relaxed) {
                        select_channel(&shared, c);
                        follow_idle = Instant::now();
                        break;
                    }
                }
            }
        }

        render(&shared, n, gain, muted, monitor, &entry)?;

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
                    KeyCode::Char('s') => {
                        let now = !shared.squelch_on.load(Ordering::Relaxed);
                        shared.squelch_on.store(now, Ordering::Relaxed);
                    }
                    KeyCode::Char('a') => {
                        let now = !shared.agc_on.load(Ordering::Relaxed);
                        shared.agc_on.store(now, Ordering::Relaxed);
                    }
                    KeyCode::Char('f') => {
                        let now = !shared.filter_on.load(Ordering::Relaxed);
                        shared.filter_on.store(now, Ordering::Relaxed);
                    }
                    KeyCode::Char('n') => {
                        let now = !shared.notch_on.load(Ordering::Relaxed);
                        shared.notch_on.store(now, Ordering::Relaxed);
                    }
                    KeyCode::Char('F') => {
                        // Toggle scanner; mix is fixed at startup (reader-side).
                        monitor = match monitor {
                            Monitor::Follow => Monitor::Single,
                            Monitor::Single => Monitor::Follow,
                            Monitor::Mix => Monitor::Mix,
                        };
                        follow_idle = Instant::now();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_addr_appends_default_port() {
        assert_eq!(normalize_addr("10.0.16.183"), "10.0.16.183:30000");
        assert_eq!(normalize_addr("pluto.local"), "pluto.local:30000");
    }

    #[test]
    fn normalize_addr_keeps_explicit_port() {
        assert_eq!(normalize_addr("10.0.16.183:30000"), "10.0.16.183:30000");
        assert_eq!(normalize_addr("192.168.2.1:40000"), "192.168.2.1:40000");
    }
}
