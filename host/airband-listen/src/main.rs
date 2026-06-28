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
//! hang time); `mix` sums every open channel into one stream. In `follow`,
//! `--ignore-channel` skips channels that false-trigger squelch.
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
//!   d                   spectral noise reducer toggle
//!   F                   follow (scanner) toggle
//!   q                   quit

use airband_dsp::{
    carrier_noise_threshold, decode_carrier, Agc, Denoise, LowPass, Notch, Squelch,
    SquelchConfig, SquelchMode, SquelchState, VoiceFilter, SAMPLE_SCALE,
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
use eframe::egui;
use egui_plot::{CoordinatesFormatter, Corner, Line, Plot, PlotPoints};
use rodio::{OutputStream, Sink, Source};
use rustfft::{num_complex::Complex32, Fft, FftPlanner};
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

use airband_dfn::{DfnEnhancer, DfnParams, Presence};

/// Per-channel audio rate: the channelizer is fed IQ at Fs = 16 Msps and
/// decimates by 800 (160 lane CIC * 5 audio) -> 20000 sps. (Older 14 MHz bitstreams
/// used lane_decim 128 -> 21875 sps; pass --rate 21875 for those.)
const DEFAULT_RATE: u32 = 20000;

/// Default TCP port of the maia-httpd airband stream, appended when the address
/// argument omits an explicit `:port` (so `10.0.16.183` works like
/// `10.0.16.183:30000`).
const DEFAULT_PORT: u16 = 30000;

/// Default channel plan (MHz), matching `firmware/airband.json` `channels_hz`
/// (positional: index 0 is the first streamed channel). 118.050 is NOT here -- it
/// is only the firmware no-SD fallback indicator, not an operational channel.
const FREQS_MHZ: [f64; 18] = [
    119.200, 119.900, 120.100, 120.400, 120.950, 121.500, 121.600, 121.700, 122.275,
    122.975, 123.900, 124.700, 125.600, 125.900, 126.250, 126.500, 126.875,
    133.650,
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
    /// Carrier-power squelch (needs a bitstream that ships the carrier byte).
    Carrier,
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
    /// Per-channel audio sample rate in Hz (AD9361 Fs / 160 / 5 = 20000)
    #[arg(long, default_value_t = DEFAULT_RATE)]
    rate: u32,
    /// Playback volume (linear sink gain). Defaults to 1.5 with AGC on (audio is
    /// already normalized) or 25000 with --no-agc (raw airband audio is quiet).
    #[arg(long)]
    gain: Option<f32>,
    /// Squelch mode: auto (audio-energy VOX, default), off, manual (fixed dBFS),
    /// or carrier (cross-channel carrier power). Auto/VOX keys on the demodulated
    /// *modulation* (the audio) via a per-channel adaptive noise floor, so it is
    /// immune to the per-channel carrier-level offsets the conducted comb causes
    /// (the comb is a steady carrier bump with no in-band voice modulation).
    #[arg(long, value_enum, default_value_t = SquelchArg::Auto)]
    squelch: SquelchArg,
    /// Squelch open threshold, in dB of SNR above the (per-channel) noise floor.
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
    /// Disable the spectral noise reducer (on by default; toggle with 'd').
    #[arg(long)]
    no_denoise: bool,
    /// Noise-reducer spectral floor in dB (more negative = more aggressive cut).
    #[arg(long, default_value_t = -18.0, allow_hyphen_values = true)]
    denoise_floor_db: f32,
    /// Disable the voice band-pass (on by default; 300-3400 Hz, toggle with 'f').
    #[arg(long)]
    no_filter: bool,
    /// Voice band-pass low corner in Hz (high-pass)
    #[arg(long, default_value_t = 300.0)]
    filter_low: f64,
    /// Voice band-pass high corner in Hz (low-pass)
    #[arg(long, default_value_t = 3400.0)]
    filter_high: f64,
    /// Standalone low-pass -3 dB cutoff in Hz, applied after the band-pass
    /// (a distinct LPF stage; 0 = off)
    #[arg(long, default_value_t = 2500.0)]
    lpf_hz: f64,
    /// Enable DeepFilterNet neural enhancement on the played stream at startup
    /// (toggle live with `D`). Heavy inference; runs only while squelch is open.
    #[arg(long)]
    dfn: bool,
    /// DeepFilterNet local-SNR floor in dB: frames below this are muted. DFN's
    /// stock value (-10) chops faint speech frame-by-frame ("garble"); -20 keeps
    /// even very quiet mumbles. Raise toward -10 to gate weak/noisy frames harder.
    #[arg(long, default_value_t = -20.0, allow_hyphen_values = true)]
    dfn_min_snr: f32,
    /// DeepFilterNet max noise attenuation in dB (>=100 = unlimited). The cap mixes
    /// some of the noisy signal back so a frame is never fully muted — this keeps
    /// noise-like consonants (CH/S/F) from being chopped. Lower = less chopping but
    /// more residual noise; raise toward 100 for deeper suppression.
    #[arg(long, default_value_t = 15.0)]
    dfn_atten_lim: f32,
    /// DeepFilterNet post-filter beta (0 = off; ~0.02 trims residual musical noise).
    #[arg(long, default_value_t = 0.02)]
    dfn_pf_beta: f32,
    /// Post-DFN brightness: high-shelf gain (dB) above `--presence-hz` to restore
    /// the upper voice band the denoiser rolls off (de-muffle). 0 = off; `p` toggles.
    #[arg(long, default_value_t = 8.0, allow_hyphen_values = true)]
    presence_db: f64,
    /// Corner frequency (Hz) of the post-DFN brightness high-shelf.
    #[arg(long, default_value_t = 1600.0)]
    presence_hz: f64,
    /// Transition Q of the post-DFN brightness high-shelf.
    #[arg(long, default_value_t = 0.707)]
    presence_q: f64,
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
    /// Channel index(es) to skip in follow (scanner) mode. Repeat the flag or
    /// pass a comma-separated list (e.g. `--ignore-channel 0,5,11`).
    #[arg(long = "ignore-channel", value_delimiter = ',')]
    ignore_channels: Vec<usize>,
}

/// Immutable DSP parameters handed to the reader thread.
#[derive(Clone)]
struct DspCfg {
    rate: u32,
    low: f64,
    high: f64,
    lpf_hz: f64,
    squelch_mode: SquelchMode,
    squelch_hang_ms: f32,
    notch_freq: Option<f64>,
    notch_q: f64,
    denoise_floor_db: f32,
    dfn: DfnParams,
    presence_hz: f64,
    presence_q: f64,
    presence_db: f64,
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
    /// Per-channel latest decoded carrier level (f32 bits) — the meter metric.
    carrier: Vec<AtomicU32>,
    /// Cross-channel carrier noise reference (75th pct, f32 bits); meter shows
    /// each channel's carrier in dB over this, so idle ~ 0 dB and a keyed station
    /// reads positive regardless of how quiet the demod audio is.
    carrier_noise: AtomicU32,
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
    denoise_on: AtomicBool,
    lpf_on: AtomicBool,
    /// Toggle: DeepFilterNet enhancement on the played stream.
    dfn_on: AtomicBool,
    /// Toggle: post-DFN presence (consonant) boost on the played stream.
    presence_on: AtomicBool,
    /// Toggle: live FFT GUI window visible.
    fft_open: AtomicBool,
    /// Ring of the most recent played (post-DSP) samples of the active stream,
    /// consumed by the live FFT window.
    fft_samples: Mutex<VecDeque<f32>>,
    /// Ring of the raw, pre-DSP demodulated samples (normalized to [-1,1]) of the
    /// active stream, for the raw-vs-filtered overlay in the FFT window.
    fft_raw: Mutex<VecDeque<f32>>,
}

impl Shared {
    fn new(channels: usize, max_queue: usize, start: usize, toggles: Toggles) -> Shared {
        Shared {
            selected: AtomicUsize::new(start),
            running: AtomicBool::new(true),
            connected: AtomicBool::new(false),
            audio: Mutex::new(VecDeque::with_capacity(max_queue)),
            peak: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            carrier: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            carrier_noise: AtomicU32::new(0f32.to_bits()),
            drops: (0..channels).map(|_| AtomicU64::new(0)).collect(),
            sq_open: (0..channels).map(|_| AtomicBool::new(false)).collect(),
            sel_state: AtomicU8::new(0),
            records: AtomicU64::new(0),
            max_queue,
            filter_on: AtomicBool::new(toggles.filter),
            squelch_on: AtomicBool::new(toggles.squelch),
            agc_on: AtomicBool::new(toggles.agc),
            notch_on: AtomicBool::new(toggles.notch),
            denoise_on: AtomicBool::new(toggles.denoise),
            lpf_on: AtomicBool::new(toggles.lpf),
            dfn_on: AtomicBool::new(toggles.dfn),
            presence_on: AtomicBool::new(toggles.presence),
            fft_open: AtomicBool::new(false),
            fft_samples: Mutex::new(VecDeque::with_capacity(FFT_RING)),
            fft_raw: Mutex::new(VecDeque::with_capacity(FFT_RING)),
        }
    }
}

#[derive(Clone, Copy)]
struct Toggles {
    filter: bool,
    squelch: bool,
    agc: bool,
    notch: bool,
    denoise: bool,
    lpf: bool,
    dfn: bool,
    presence: bool,
}

/// Per-sample DSP toggle snapshot handed to [`ChannelDsp::process`].
#[derive(Clone, Copy)]
struct ChainOpts {
    filter: bool,
    notch: bool,
    agc: bool,
    denoise: bool,
    lpf: bool,
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

/// Welch FFT segment length, and the ring of recent samples we average over
/// (8192 / 2048 with 50% overlap -> ~7 averaged periodograms).
const WELCH_NSEG: usize = 2048;
const FFT_RING: usize = 8192;

/// Taps one played (post-DSP) sample into the live-FFT ring.
fn push_fft(shared: &Shared, sample: f32) {
    let mut q = shared.fft_samples.lock().unwrap();
    q.push_back(sample);
    while q.len() > FFT_RING {
        q.pop_front();
    }
}

/// Taps one raw (pre-DSP) demodulated sample into the raw-FFT ring.
fn push_fft_raw(shared: &Shared, sample: f32) {
    let mut q = shared.fft_raw.lock().unwrap();
    q.push_back(sample);
    while q.len() > FFT_RING {
        q.pop_front();
    }
}

/// Post-DFN presence (consonant) boost on the played sample, then a soft clip.
/// Toggled by `presence_on`; the IIR is reset while the stream is idle so it
/// starts each transmission clean.
fn apply_presence(shared: &Shared, presence: &mut Presence, sample: f32, active: bool) -> f32 {
    if !active {
        presence.reset();
        return sample;
    }
    if !shared.presence_on.load(Ordering::Relaxed) {
        return sample;
    }
    presence.process(sample)
}

/// Applies DeepFilterNet to one played sample when enabled, lazily building the
/// enhancer (the model load stalls the audio thread once) and resetting it while
/// the stream is inactive — DFN only runs while the squelch is open. Runs on the
/// fully-processed (filtered + AGC-leveled) audio so the model sees a healthy,
/// in-range signal.
fn apply_dfn(
    shared: &Shared,
    dfn: &mut Option<DfnEnhancer>,
    params: DfnParams,
    rate: u32,
    sample: f32,
    active: bool,
) -> f32 {
    if !shared.dfn_on.load(Ordering::Relaxed) {
        return sample;
    }
    if dfn.is_none() {
        match DfnEnhancer::new(rate as f64, params) {
            Ok(e) => *dfn = Some(e),
            Err(err) => {
                eprintln!("DeepFilterNet init failed: {err:#}");
                shared.dfn_on.store(false, Ordering::Relaxed);
                return sample;
            }
        }
    }
    let e = dfn.as_mut().unwrap();
    if active {
        e.process_sample(sample)
    } else {
        e.reset();
        sample
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
    lpf: Option<LowPass>,
    notch: Option<Notch>,
    denoise: Denoise,
    agc: Agc,
}

/// STFT frame size for the spectral denoiser (~13 ms at 20000 sps).
const DENOISE_FRAME: usize = 256;

impl ChannelDsp {
    fn new(cfg: &DspCfg) -> ChannelDsp {
        ChannelDsp {
            vf: VoiceFilter::new(cfg.rate as f64, cfg.low, cfg.high),
            lpf: (cfg.lpf_hz > 0.0).then(|| LowPass::new(cfg.rate as f64, cfg.lpf_hz)),
            notch: cfg
                .notch_freq
                .map(|f| Notch::new(f, cfg.rate as f64, cfg.notch_q)),
            denoise: Denoise::new(DENOISE_FRAME, cfg.denoise_floor_db),
            agc: Agc::new(),
        }
    }

    /// Runs one sample through the full gated chain (filters -> AGC), returning
    /// normalized audio. DeepFilterNet (when enabled) runs after this on the
    /// played stream, so it sees the filtered, AGC-leveled signal.
    fn process(&mut self, sample: i32, open: bool, just_opened: bool, above: bool, opt: ChainOpts) -> f32 {
        if !open {
            // Keep the AGC tail decaying so it re-seeds cleanly on the next open.
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
        // Distinct standalone LPF stage, toggled independently of the band-pass.
        if opt.lpf {
            if let Some(lp) = self.lpf.as_mut() {
                v = lp.process(v);
            }
        }
        if opt.notch {
            if let Some(nf) = self.notch.as_mut() {
                v = nf.process(v);
            }
        }
        let mut vn = (v / SAMPLE_SCALE as f64) as f32;
        if opt.denoise {
            // Learn the noise model from below-threshold (non-speech) samples.
            vn = self.denoise.process(vn, !above);
        }
        if !opt.agc {
            return vn;
        }
        self.agc.process(vn, true, just_opened, above)
    }
}

/// Connects (and reconnects) to the stream, demuxing records into the shared
/// state and running the DSP chain (selected channel, or all channels in mix
/// mode).
fn reader_loop(shared: Arc<Shared>, addr: String, n: usize, cfg: DspCfg, mix: bool) {
    // Carrier-squelch: cross-channel noise percentile + recompute cadence. The
    // median (0.5) is used rather than a high percentile because at useful gain
    // several channels are elevated by the conducted comb, which would inflate a
    // 75th-percentile "noise" reference and push the threshold above real traffic.
    const CARRIER_NOISE_PCT: f32 = 0.5;
    const CARRIER_UPDATE_FRAMES: u64 = 8192;
    let carrier_mode = matches!(cfg.squelch_mode, SquelchMode::Carrier { .. });
    let carrier_snr_ratio = match cfg.squelch_mode {
        SquelchMode::Carrier { snr_db } => 10f32.powf(snr_db / 20.0),
        _ => 1.0,
    };
    let mut last_carrier = vec![0f32; n];
    let mut since_carrier_update: u64 = 0;

    let mut last_seq = vec![u32::MAX; n];
    let mut squelches: Vec<Squelch> = (0..n)
        .map(|_| {
            Squelch::new(
                SquelchConfig::new(cfg.squelch_mode, cfg.rate).with_hang_ms(cfg.squelch_hang_ms),
            )
        })
        .collect();
    // In carrier-squelch mode the main squelch keys on the carrier; this
    // audio-energy VOX drives the `above` (speech-present) flag for AGC/denoise.
    let mut audio_gates: Vec<Option<Squelch>> = (0..n)
        .map(|_| match cfg.squelch_mode {
            SquelchMode::Carrier { snr_db } => Some(Squelch::new(SquelchConfig::new(
                SquelchMode::AutoSnr { snr_db },
                cfg.rate,
            ))),
            _ => None,
        })
        .collect();
    let mut dsps: Vec<ChannelDsp> = (0..n).map(|_| ChannelDsp::new(&cfg)).collect();
    // Dedicated 300-3400 Hz voice band-pass per channel, used only to derive the
    // VOX squelch level from voice-band modulation energy. This keeps the squelch
    // decision independent of the playback filter toggles and excludes the
    // out-of-voice-band conducted comb and low-frequency rumble.
    let mut sq_filters: Vec<VoiceFilter> =
        (0..n).map(|_| VoiceFilter::new(cfg.rate as f64, 300.0, 3400.0)).collect();

    // Mix-mode accumulator: one contribution per channel, summed and emitted
    // once per audio tick (detected when the channel index wraps to a lower one).
    let mut mix_buf = vec![0f32; n];
    let mut raw_mix_buf = vec![0f32; n];
    let mut prev_chan: Option<usize> = None;
    // DeepFilterNet enhancer (lazy: built on first enable) plus its input leveler.
    // One instance on the played stream, not per channel.
    let mut dfn: Option<DfnEnhancer> = None;
    // Post-DFN brightness (high-shelf) lift on the played stream; gated by presence_on.
    let mut presence = Presence::new(
        cfg.rate as f64,
        cfg.presence_hz,
        cfg.presence_q,
        cfg.presence_db,
    );

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
                    // layout: seq[63:40] | chan[39:32] | carrier[31:24] | audio[23:0]
                    let raw = (w & 0xff_ffff) as u32;
                    let sample = ((raw << 8) as i32) >> 8; // sign-extend 24->32
                    let carrier = ((w >> 24) & 0xff) as u8;
                    let chan = ((w >> 32) & 0xff) as usize;
                    let seq = ((w >> 40) & 0xff_ffff) as u32;
                    if chan >= n {
                        continue;
                    }
                    // Always track the per-channel carrier (drives the meter) and
                    // periodically recompute the cross-channel noise reference.
                    let car = decode_carrier(carrier);
                    last_carrier[chan] = car;
                    shared.carrier[chan].store(car.to_bits(), Ordering::Relaxed);
                    since_carrier_update += 1;
                    if since_carrier_update >= CARRIER_UPDATE_FRAMES {
                        let noise = carrier_noise_threshold(&last_carrier, CARRIER_NOISE_PCT, 1.0);
                        shared.carrier_noise.store(noise.to_bits(), Ordering::Relaxed);
                        if carrier_mode {
                            let thr = noise * carrier_snr_ratio;
                            for sq in squelches.iter_mut() {
                                sq.set_threshold(thr);
                            }
                        }
                        since_carrier_update = 0;
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
                    // Voice-band magnitude for VOX squelch (excludes comb/rumble).
                    let sq_mag = sq_filters[chan].process(sample as f64).abs() as f32;

                    // Carrier mode keys the squelch on the FPGA carrier level; all
                    // other modes key on voice-band modulation energy (VOX).
                    let sq_level = match cfg.squelch_mode {
                        SquelchMode::Carrier { .. } => decode_carrier(carrier),
                        _ => sq_mag,
                    };
                    // Squelch runs on every channel so the meter shows activity.
                    let sq = &mut squelches[chan];
                    let st = sq.process(sq_level);
                    shared.sq_open[chan].store(sq.is_open(), Ordering::Relaxed);

                    let filter_on = shared.filter_on.load(Ordering::Relaxed);
                    let notch_on = shared.notch_on.load(Ordering::Relaxed);
                    let squelch_on = shared.squelch_on.load(Ordering::Relaxed);
                    let agc_on = shared.agc_on.load(Ordering::Relaxed);
                    let denoise_on = shared.denoise_on.load(Ordering::Relaxed);
                    let lpf_on = shared.lpf_on.load(Ordering::Relaxed);
                    let open = !squelch_on || sq.is_open();
                    let just_opened = squelch_on && sq.just_opened();
                    // Speech-present flag is always voice-band modulation based.
                    let above = match audio_gates[chan].as_mut() {
                        Some(g) => {
                            g.process(sq_mag);
                            sq_mag >= g.threshold()
                        }
                        None => sq_mag >= sq.threshold(),
                    };
                    let opt = ChainOpts {
                        filter: filter_on,
                        notch: notch_on,
                        agc: agc_on,
                        denoise: denoise_on,
                        lpf: lpf_on,
                    };

                    if mix {
                        // Emit the previous tick's sum when the index wraps.
                        if let Some(p) = prev_chan {
                            if chan <= p {
                                let s: f32 = mix_buf.iter().sum::<f32>().clamp(-1.0, 1.0);
                                let raw_s: f32 = raw_mix_buf.iter().sum::<f32>().clamp(-1.0, 1.0);
                                let any_open =
                                    (0..n).any(|c| shared.sq_open[c].load(Ordering::Relaxed));
                                let s = apply_dfn(&shared, &mut dfn, cfg.dfn, cfg.rate, s, any_open);
                                let s = apply_presence(&shared, &mut presence, s, any_open);
                                push_audio(&shared, s);
                                push_fft(&shared, s);
                                push_fft_raw(&shared, raw_s);
                                mix_buf.iter_mut().for_each(|x| *x = 0.0);
                                raw_mix_buf.iter_mut().for_each(|x| *x = 0.0);
                            }
                        }
                        mix_buf[chan] = dsps[chan].process(sample, open, just_opened, above, opt);
                        raw_mix_buf[chan] = sample as f32 / SAMPLE_SCALE as f32;
                        prev_chan = Some(chan);
                        continue;
                    }

                    let sel = shared.selected.load(Ordering::Relaxed);
                    if chan != sel {
                        continue;
                    }
                    shared.sel_state.store(state_code(st), Ordering::Relaxed);
                    // Chain: filters -> AGC -> DFN -> presence boost. DFN sees the
                    // filtered, AGC-leveled (in-range) signal; the presence EQ then
                    // lifts the ~2 kHz consonant band of the cleaned speech.
                    let out = dsps[chan].process(sample, open, just_opened, above, opt);
                    let out = apply_dfn(&shared, &mut dfn, cfg.dfn, cfg.rate, out, open);
                    let out = apply_presence(&shared, &mut presence, out, open);
                    push_audio(&shared, out);
                    push_fft(&shared, out);
                    push_fft_raw(&shared, sample as f32 / SAMPLE_SCALE as f32);
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

/// Builds a per-channel ignore mask from CLI indices (out-of-range entries are dropped).
fn ignore_mask(n: usize, indices: &[usize]) -> Vec<bool> {
    let mut mask = vec![false; n];
    for &ch in indices {
        if ch < n {
            mask[ch] = true;
        }
    }
    mask
}

/// First non-ignored channel whose squelch is open, scanning round-robin from
/// `start` (inclusive).
fn first_open_channel(start: usize, n: usize, shared: &Shared, ignore: &[bool]) -> Option<usize> {
    for off in 0..n {
        let c = (start + off) % n;
        if ignore.get(c).copied().unwrap_or(false) {
            continue;
        }
        if shared.sq_open[c].load(Ordering::Relaxed) {
            return Some(c);
        }
    }
    None
}

fn render(
    shared: &Shared,
    n: usize,
    gain: f32,
    muted: bool,
    monitor: Monitor,
    ignore: &[bool],
    entry: &str,
) -> Result<()> {
    let sel = shared.selected.load(Ordering::Relaxed);
    let connected = shared.connected.load(Ordering::Relaxed);
    let records = shared.records.load(Ordering::Relaxed);
    let mut out = stdout();
    queue!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;

    // Fixed-width fields so the header doesn't shift as squelch/volume/mode change.
    let status = if connected { "connected" } else { "connecting…" };
    let status = format!("{status:<11}");
    let mode = match monitor {
        Monitor::Single => "single",
        Monitor::Follow => "FOLLOW",
        Monitor::Mix => "MIX",
    };
    let mode = format!("{mode:<6}");
    let vol = if muted {
        "MUTED".to_string()
    } else {
        format!("vol x{gain:.1}")
    };
    let vol = format!("{vol:<12}");
    let sq = if shared.squelch_on.load(Ordering::Relaxed) {
        state_label(shared.sel_state.load(Ordering::Relaxed))
    } else {
        "SQ off"
    };
    let sq = format!("{sq:<7}");
    let flags = format!(
        "{} {} {} {} {} {} {} {}",
        if shared.agc_on.load(Ordering::Relaxed) { "AGC" } else { "agc" },
        if shared.filter_on.load(Ordering::Relaxed) { "BPF" } else { "bpf" },
        if shared.notch_on.load(Ordering::Relaxed) { "NOTCH" } else { "notch" },
        if shared.denoise_on.load(Ordering::Relaxed) { "NR" } else { "nr" },
        if shared.lpf_on.load(Ordering::Relaxed) { "LPF" } else { "lpf" },
        if shared.dfn_on.load(Ordering::Relaxed) { "DFN" } else { "dfn" },
        if shared.presence_on.load(Ordering::Relaxed) { "PRES" } else { "pres" },
        if shared.fft_open.load(Ordering::Relaxed) { "FFT" } else { "fft" },
    );
    queue!(
        out,
        SetForegroundColor(Color::Cyan),
        Print(format!(
            "Pluto airband live listener — {status} — {vol} — sq:{sq} — {flags} — {mode} — {records} recs\r\n"
        )),
        ResetColor,
        Print("↑/↓ select  0-9+Enter jump  +/- vol  m mute  s squelch  a agc  f bpf  n notch  d nr  l lpf  D dfn  p pres  g fft  F follow  q quit\r\n"),
    )?;
    if monitor == Monitor::Follow {
        let skipped: Vec<String> = (0..n)
            .filter(|&c| ignore.get(c).copied().unwrap_or(false))
            .map(|c| c.to_string())
            .collect();
        if !skipped.is_empty() {
            queue!(
                out,
                Print(format!("follow: ignoring ch {}\r\n", skipped.join(", "))),
            )?;
        }
    }
    queue!(out, Print("\r\n"))?;

    // Carrier metering: each channel's FPGA carrier level in dB over the
    // cross-channel noise reference. Idle channels sit ~0 dB; a keyed station
    // reads positive even when its demod audio is buried in the noise.
    let noise_ref = f32::from_bits(shared.carrier_noise.load(Ordering::Relaxed)).max(1.0);
    for ch in 0..n {
        let carrier = f32::from_bits(shared.carrier[ch].load(Ordering::Relaxed)).max(1.0);
        let drops = shared.drops[ch].load(Ordering::Relaxed);
        let cdb = 20.0 * (carrier / noise_ref).log10();
        // map 0..30 dB-over-noise onto the 24-cell meter
        let frac = (cdb / 30.0).clamp(0.0, 1.0);
        let bars = (frac * 24.0).round() as usize;
        let meter: String = "#".repeat(bars) + &"·".repeat(24 - bars);
        let marker = if ch == sel { "▶" } else { " " };
        let active = if ignore.get(ch).copied().unwrap_or(false) {
            "−"
        } else if shared.sq_open[ch].load(Ordering::Relaxed) {
            "●"
        } else {
            " "
        };
        let color = if ch == sel { Color::Green } else { Color::Grey };
        queue!(
            out,
            SetForegroundColor(color),
            Print(format!(
                "{marker}{active} ch {ch:>2}  {}  [{meter}] {cdb:>5.1} dB·c  drops {drops}\r\n",
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
        SquelchArg::Carrier => SquelchMode::Carrier {
            snr_db: args.squelch_snr,
        },
    };
    let agc_on = !args.no_agc;
    // DFN-centric default chain: every host filter (band-pass, LPF, notch,
    // spectral denoise) starts OFF and DeepFilterNet starts ON, so DFN does the
    // cleanup in isolation. Each is still runtime-toggleable (f / l / n / d / D);
    // AGC is leveling (gain), not a filter, so it follows --no-agc as before.
    let toggles = Toggles {
        filter: false,
        squelch: !matches!(args.squelch, SquelchArg::Off),
        agc: agc_on,
        notch: false,
        denoise: false,
        lpf: false,
        dfn: true,
        presence: args.presence_db != 0.0,
    };
    let cfg = DspCfg {
        rate: args.rate,
        low: args.filter_low,
        high: args.filter_high,
        lpf_hz: args.lpf_hz,
        squelch_mode,
        squelch_hang_ms: args.squelch_hang_ms,
        notch_freq: args.notch,
        notch_q: args.notch_q,
        denoise_floor_db: args.denoise_floor_db,
        dfn: DfnParams {
            min_snr_db: args.dfn_min_snr,
            atten_lim_db: args.dfn_atten_lim,
            pf_beta: args.dfn_pf_beta,
        },
        presence_hz: args.presence_hz,
        presence_q: args.presence_q,
        presence_db: args.presence_db,
    };

    // ~0.4 s of queued audio caps playback latency on channel switches.
    let max_queue = (args.rate as usize * 2) / 5;
    let shared = Arc::new(Shared::new(n, max_queue, start, toggles));

    // Audio output: keep _stream alive on the main thread (it is !Send and the
    // GUI event loop also lives here). The Sink is shared with the terminal UI
    // thread for volume control.
    let (_stream, handle) = OutputStream::try_default()
        .context("no default audio output device (is one available?)")?;
    let sink = Arc::new(Sink::try_new(&handle).context("failed to create audio sink")?);
    // With AGC the audio is already normalized to ~unity, so a small sink gain
    // suffices; without AGC the raw airband audio is very quiet and needs a big
    // makeup factor (the historical default).
    let gain = args.gain.unwrap_or(if agc_on { 1.5 } else { 25000.0 }).max(0.0);
    sink.set_volume(gain);
    sink.append(ChannelSource {
        shared: Arc::clone(&shared),
        rate: args.rate,
    });

    let mix = args.monitor == Monitor::Mix;
    let reader = {
        let shared = Arc::clone(&shared);
        let addr = normalize_addr(&args.addr);
        let cfg = cfg.clone();
        thread::spawn(move || reader_loop(shared, addr, n, cfg, mix))
    };

    // Terminal control UI on a background thread; the FFT GUI event loop must
    // own the main thread (macOS requires native windows on the main thread).
    let ignore = ignore_mask(n, &args.ignore_channels);
    let term = {
        let shared = Arc::clone(&shared);
        let sink = Arc::clone(&sink);
        let monitor = args.monitor;
        let follow_hang_ms = args.follow_hang_ms;
        let ignore = ignore.clone();
        thread::spawn(move || {
            let s = Arc::clone(&shared);
            if let Err(e) = run_terminal(shared, sink, n, monitor, follow_hang_ms, ignore, gain) {
                eprintln!("terminal UI error: {e:#}");
            }
            s.running.store(false, Ordering::Relaxed);
        })
    };

    // Blocks until the app quits (running -> false), then we join the threads.
    run_fft_gui(Arc::clone(&shared), args.rate as f32);
    shared.running.store(false, Ordering::Relaxed);
    let _ = term.join();
    let _ = reader.join();
    Ok(())
}

/// Terminal control UI loop (background thread): owns the raw-mode terminal and
/// all keyboard handling. Exits when `running` clears (`q` here, or the GUI
/// window quitting).
fn run_terminal(
    shared: Arc<Shared>,
    sink: Arc<Sink>,
    n: usize,
    mut monitor: Monitor,
    follow_hang_ms: u64,
    ignore: Vec<bool>,
    mut gain: f32,
) -> Result<()> {
    let mut muted = false;
    let hang = Duration::from_millis(follow_hang_ms);
    let mut follow_idle = Instant::now();

    terminal::enable_raw_mode().context("failed to enter raw terminal mode")?;
    execute!(stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
    let _guard = TermGuard;

    let mut entry = String::new();
    while shared.running.load(Ordering::Relaxed) {
        // Scanner: stay on the current (non-ignored) channel while it's active;
        // jump immediately to any other channel that breaks squelch; once the
        // current channel goes idle for `hang`, rescan (bridges speech gaps).
        if monitor == Monitor::Follow {
            let sel = shared.selected.load(Ordering::Relaxed);
            let sel_ignored = ignore.get(sel).copied().unwrap_or(false);
            let cur_open = !sel_ignored && shared.sq_open[sel].load(Ordering::Relaxed);

            if cur_open {
                follow_idle = Instant::now();
            } else if let Some(c) = first_open_channel((sel + 1) % n, n, &shared, &ignore) {
                // Another channel is active — switch right away (don't wait for hang).
                select_channel(&shared, c);
                follow_idle = Instant::now();
            } else if follow_idle.elapsed() >= hang {
                // Hang expired on an idle channel; rescan from the current position.
                if let Some(c) = first_open_channel(sel, n, &shared, &ignore) {
                    select_channel(&shared, c);
                }
                follow_idle = Instant::now();
            }
        }

        render(&shared, n, gain, muted, monitor, &ignore, &entry)?;

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
                    KeyCode::Char('d') => {
                        let now = !shared.denoise_on.load(Ordering::Relaxed);
                        shared.denoise_on.store(now, Ordering::Relaxed);
                    }
                    KeyCode::Char('l') => {
                        let now = !shared.lpf_on.load(Ordering::Relaxed);
                        shared.lpf_on.store(now, Ordering::Relaxed);
                    }
                    KeyCode::Char('g') => {
                        let now = !shared.fft_open.load(Ordering::Relaxed);
                        shared.fft_open.store(now, Ordering::Relaxed);
                    }
                    KeyCode::Char('D') => {
                        let now = !shared.dfn_on.load(Ordering::Relaxed);
                        shared.dfn_on.store(now, Ordering::Relaxed);
                    }
                    KeyCode::Char('p') => {
                        let now = !shared.presence_on.load(Ordering::Relaxed);
                        shared.presence_on.store(now, Ordering::Relaxed);
                    }
                    KeyCode::Char('F') => {
                        // Toggle scanner; mix is fixed at startup (reader-side).
                        monitor = match monitor {
                            Monitor::Follow => Monitor::Single,
                            Monitor::Single => Monitor::Follow,
                            Monitor::Mix => Monitor::Mix,
                        };
                        follow_idle = Instant::now();
                        if monitor == Monitor::Follow {
                            if let Some(c) = first_open_channel(0, n, &shared, &ignore) {
                                select_channel(&shared, c);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    shared.running.store(false, Ordering::Relaxed);
    Ok(())
}

/// Live FFT GUI (egui/eframe) — runs on the main thread. The window starts
/// hidden and is shown/hidden by the `fft_open` toggle (`g` in the terminal).
/// egui_plot's hover crosshair reads out frequency (x) and magnitude (y) for
/// precision debugging; scroll/drag to zoom/pan.
fn run_fft_gui(shared: Arc<Shared>, rate: f32) {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("airband-listen — live FFT")
            .with_inner_size([960.0, 440.0])
            .with_visible(false),
        ..Default::default()
    };
    let app_shared = Arc::clone(&shared);
    if let Err(e) = eframe::run_native(
        "airband-listen-fft",
        options,
        Box::new(move |_cc| Ok(Box::new(FftApp::new(app_shared, rate)))),
    ) {
        eprintln!("FFT GUI error: {e}");
    }
    // GUI exited -> bring the whole app down.
    shared.running.store(false, Ordering::Relaxed);
}

/// egui app drawing a live single-sided FFT (dB vs Hz) of the active stream.
struct FftApp {
    shared: Arc<Shared>,
    rate: f32,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
}

impl FftApp {
    fn new(shared: Arc<Shared>, rate: f32) -> FftApp {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(WELCH_NSEG);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        // Hann window (coherent gain ~0.5, corrected in the magnitude scale).
        let window: Vec<f32> = (0..WELCH_NSEG)
            .map(|i| {
                let w = 2.0 * std::f32::consts::PI * i as f32 / (WELCH_NSEG as f32 - 1.0);
                0.5 - 0.5 * w.cos()
            })
            .collect();
        FftApp {
            shared,
            rate,
            fft,
            window,
            buf: vec![Complex32::new(0.0, 0.0); WELCH_NSEG],
            scratch,
        }
    }

    /// Snapshots a ring buffer into a Vec (empty if it has < one FFT segment).
    fn snapshot(ring: &Mutex<VecDeque<f32>>) -> Vec<f32> {
        let q = ring.lock().unwrap();
        if q.len() < WELCH_NSEG {
            return Vec::new();
        }
        q.iter().copied().collect()
    }

    /// Welch PSD: average the periodograms of 50%-overlapping Hann-windowed
    /// segments of `samples`, returned as single-sided [freq_Hz, dB] points.
    /// Averaging reduces variance so the trace (and thus the locked Y axis) is
    /// steady enough to read.
    fn spectrum(&mut self, samples: &[f32]) -> Vec<[f64; 2]> {
        if samples.len() < WELCH_NSEG {
            return Vec::new();
        }
        let nbins = WELCH_NSEG / 2;
        let hop = WELCH_NSEG / 2;
        // Remove DC before windowing: the raw AM-envelope tap carries a large
        // carrier/DC term that otherwise dominates bin 0, swamps the audio bins,
        // and skews the auto-fit — making the raw trace unreadable.
        let mean = samples.iter().sum::<f32>() / samples.len() as f32;
        let mut psd = vec![0f32; nbins];
        let mut segs = 0u32;
        let mut start = 0;
        while start + WELCH_NSEG <= samples.len() {
            for i in 0..WELCH_NSEG {
                self.buf[i] = Complex32::new((samples[start + i] - mean) * self.window[i], 0.0);
            }
            self.fft
                .process_with_scratch(&mut self.buf, &mut self.scratch);
            for (k, p) in psd.iter_mut().enumerate() {
                *p += self.buf[k].norm_sqr();
            }
            segs += 1;
            start += hop;
        }
        if segs == 0 {
            return Vec::new();
        }
        // RMS magnitude per bin (avg power -> sqrt); Hann CG=0.5 (×2) and
        // single-sided (×2) folded into 4/N, matching the single-window scale.
        let inv = 1.0 / segs as f32;
        let scale = 4.0 / WELCH_NSEG as f32;
        (0..nbins)
            .map(|k| {
                let rms = (psd[k] * inv).sqrt();
                let db = 20.0 * (rms * scale).max(1e-9).log10();
                [(k as f32 * self.rate / WELCH_NSEG as f32) as f64, db as f64]
            })
            .collect()
    }
}

impl eframe::App for FftApp {
    /// Non-painting logic (also runs while the window is hidden): drive the
    /// window visibility from the `g` toggle, quit when `running` clears, and
    /// keep polling for those flags via scheduled repaints.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.shared.running.load(Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        // Closing the window (X) just hides it, like pressing `g` again.
        if ctx.input(|i| i.viewport().close_requested()) {
            self.shared.fft_open.store(false, Ordering::Relaxed);
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        let open = self.shared.fft_open.load(Ordering::Relaxed);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(open));
        ctx.request_repaint_after(Duration::from_millis(if open { 33 } else { 150 }));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.shared.fft_open.load(Ordering::Relaxed) {
            return;
        }
        let raw_samples = Self::snapshot(&self.shared.fft_raw);
        let filt_samples = Self::snapshot(&self.shared.fft_samples);
        let raw_pts = self.spectrum(&raw_samples);
        let filt_pts = self.spectrum(&filt_samples);
        let sel = self.shared.selected.load(Ordering::Relaxed);
        let mhz = FREQS_MHZ.get(sel).copied().unwrap_or(0.0);
        ui.label(format!(
            "ch {sel} ({mhz:.3} MHz) — raw (pre-DSP) vs filtered (post-DSP), Welch {WELCH_NSEG}-pt Hann, {:.0} sps. Hover to read freq/dB; drag/scroll to adjust, double-click to reset.",
            self.rate
        ));
        Plot::new("fft_plot")
            .x_axis_label("frequency (Hz)")
            .y_axis_label("magnitude (dBFS)")
            .legend(egui_plot::Legend::default())
            .coordinates_formatter(Corner::RightBottom, CoordinatesFormatter::default())
            // Lock the Y axis (no per-frame autoscale jumping); X auto-fits the
            // constant 0..Fs/2 range. The user can still drag/scroll to change Y,
            // and double-click resets to these default bounds.
            .auto_bounds(egui::Vec2b::new(true, false))
            .default_y_bounds(-120.0, 0.0)
            .show(ui, |pu| {
                if !raw_pts.is_empty() {
                    pu.line(Line::new("raw (pre-DSP)", PlotPoints::from(raw_pts)));
                }
                if !filt_pts.is_empty() {
                    pu.line(Line::new("filtered (post-DSP)", PlotPoints::from(filt_pts)));
                }
            });
    }
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
