//! Host-side reader for the Pluto airband framed-audio DMA stream.
//!
//! The Pluto (maia-httpd) serves a raw TCP stream of 64-bit little-endian
//! framed-audio records produced by the FPGA `AudioFramer`:
//!
//! ```text
//!   bits [31:0]  : audio sample, signed, sign-extended to 32 bits
//!   bits [39:32] : channel index (0..N-1)
//!   bits [63:40] : per-channel sample sequence counter (wraps at 2**24)
//! ```
//!
//! This tool connects to that stream (over USB-ethernet or Ethernet),
//! demultiplexes the records by channel, uses the per-channel sequence counter
//! to detect dropped samples, runs the shared `airband-dsp` chain (squelch,
//! voice band-pass, optional notch, AGC), and either prints live per-channel
//! statistics or records audio to disk.
//!
//! Recording defaults to **split-on-transmission**: while the squelch is open a
//! timestamped file is written, and it is finalized when the squelch closes, so
//! each file holds a single transmission with no inter-transmission static
//! (mirrors RTLSDR-Airband's `split_on_transmission`). Use `--no-split` for one
//! continuous file per channel, or `--squelch off` to disable gating entirely.
//!
//! It reconnects automatically so it can run unattended.
//!
//! Examples:
//! ```text
//!   airband-reader 192.168.2.1:30000                          # live stats
//!   airband-reader 192.168.2.1:30000 --mode wav --out-dir caps
//!   airband-reader 192.168.2.1:30000 --mode wav --no-split --squelch off
//!   airband-reader 192.168.2.1:30000 --mode raw --no-agc --shift 6
//! ```

mod feeds;
mod icecast;
mod metrics;

use airband_dsp::{
    carrier_noise_threshold, decode_carrier, level_to_dbfs, Agc, Denoise, LowPass, Notch, Squelch,
    SquelchConfig, SquelchMode, VoiceFilter, SAMPLE_SCALE,
};

/// Carrier-squelch: population percentile used as the cross-channel noise
/// reference, and how often (in frames) the shared threshold is recomputed.
const CARRIER_NOISE_PCT: f32 = 0.75;
const CARRIER_UPDATE_FRAMES: u64 = 8192;
use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use icecast::{IcecastConfig, TlsMode};
use metrics::Metrics;
use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    net::{TcpStream, UdpSocket},
    path::PathBuf,
    sync::{mpsc::SyncSender, Arc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const FRAME_BYTES: usize = 8;
const AUDIO_FIELD: u32 = 24;
const CARRIER_FIELD: u32 = 8;
const CHAN_FIELD: u32 = 8;
const SEQ_FIELD: u32 = 24;
const SEQ_MOD: u32 = 1 << SEQ_FIELD;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Print per-channel statistics (rate, drops, level, noise floor, transmissions)
    Stats,
    /// Write WAV files (16-bit PCM mono)
    Wav,
    /// Write raw little-endian s16 PCM
    Raw,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum SquelchArg {
    Off,
    Auto,
    Manual,
    /// Carrier-power squelch (needs a bitstream that ships the carrier byte).
    Carrier,
}

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Pluto airband stream address (host:port)
    #[arg(default_value = "192.168.2.1:30000")]
    addr: String,
    /// Number of channels to demultiplex
    #[arg(long, default_value_t = 21)]
    channels: usize,
    /// Audio sample rate in Hz (= AD9361 Fs / 128 / 5); used for WAV headers
    #[arg(long, default_value_t = 21875)]
    rate: u32,
    /// Output mode
    #[arg(long, value_enum, default_value_t = Mode::Stats)]
    mode: Mode,
    /// Output directory for wav/raw modes
    #[arg(long, default_value = "airband-out")]
    out_dir: PathBuf,
    /// Bit-shift applied to the 24-bit sample before clamping to i16 (--no-agc only).
    ///
    /// Positive = right-shift (attenuate); negative = left-shift (makeup gain).
    /// Ignored when the AGC is on (it normalizes loudness itself).
    #[arg(long, default_value_t = -6, allow_hyphen_values = true)]
    shift: i32,
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
    /// pauses in continuous speech like AWOS/ATIS so it does not chatter, and keeps
    /// a single transmission in one file).
    #[arg(long, default_value_t = 1000.0)]
    squelch_hang_ms: f32,
    /// Write one continuous file per channel instead of one file per transmission.
    #[arg(long)]
    no_split: bool,
    /// Discard split recordings shorter than this many milliseconds.
    #[arg(long, default_value_t = 300)]
    min_transmission_ms: u64,
    /// Disable the AGC (use raw, band-passed audio scaled by --shift).
    #[arg(long)]
    no_agc: bool,
    /// Disable the spectral noise reducer (on by default).
    #[arg(long)]
    no_denoise: bool,
    /// Noise-reducer spectral floor in dB (more negative = more aggressive cut).
    #[arg(long, default_value_t = -18.0, allow_hyphen_values = true)]
    denoise_floor_db: f32,
    /// Disable the voice band-pass (on by default; 300-3400 Hz).
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
    /// Notch (band-stop) center frequency in Hz to kill a tonal spur (off if unset).
    #[arg(long)]
    notch: Option<f64>,
    /// Notch quality factor (higher = narrower).
    #[arg(long, default_value_t = 10.0)]
    notch_q: f64,
    /// Statistics print interval in seconds
    #[arg(long, default_value_t = 5)]
    stats_interval: u64,
    /// Serve Prometheus metrics on this TCP port (0 = disabled).
    #[arg(long, default_value_t = 0)]
    metrics_port: u16,
    /// Stream this channel's processed PCM over UDP (needs --udp-dest).
    #[arg(long)]
    udp_channel: Option<usize>,
    /// UDP destination host:port for --udp-channel.
    #[arg(long)]
    udp_dest: Option<String>,
    /// Stream many channels to one or more Icecast servers from a JSON feeds
    /// file (see README). Use this to feed all configured channels at once;
    /// combinable with the single-stream --icecast-* flags below.
    #[arg(long)]
    feeds: Option<PathBuf>,
    /// Stream this channel to an Icecast mount (needs --icecast-host/password).
    /// A single-stream shortcut; for many channels/servers use --feeds.
    #[arg(long)]
    icecast_channel: Option<usize>,
    /// Icecast server host.
    #[arg(long, default_value = "localhost")]
    icecast_host: String,
    /// Icecast server port.
    #[arg(long, default_value_t = 8000)]
    icecast_port: u16,
    /// Icecast mount point.
    #[arg(long, default_value = "/airband.mp3")]
    icecast_mount: String,
    /// Icecast source username.
    #[arg(long, default_value = "source")]
    icecast_user: String,
    /// Icecast source password.
    #[arg(long, default_value = "")]
    icecast_password: String,
    /// Icecast MP3 bitrate in kbps (LiveATC uses 16).
    #[arg(long, default_value_t = 16)]
    icecast_bitrate: u32,
    /// Icecast MP3 output sample rate (LiveATC uses 22050).
    #[arg(long, default_value_t = 22050)]
    icecast_samplerate: u32,
    /// Icecast stream name.
    #[arg(long, default_value = "Pluto airband")]
    icecast_name: String,
    /// Icecast ICY genre tag (optional).
    #[arg(long)]
    icecast_genre: Option<String>,
    /// Icecast ICY description tag (optional).
    #[arg(long)]
    icecast_description: Option<String>,
    /// TLS mode to the Icecast server: disabled, transport, auto, auto_no_plain,
    /// or upgrade (default disabled = plain TCP).
    #[arg(long, default_value = "disabled")]
    icecast_tls: String,
    /// Testing only: accept invalid/self-signed TLS certs and hostname
    /// mismatches (disables MITM protection; never use for a public feed).
    #[arg(long)]
    icecast_tls_insecure: bool,
}

/// Resolved, immutable runtime configuration.
struct Config {
    mode: Mode,
    out_dir: PathBuf,
    rate: u32,
    shift: i32,
    squelch_mode: SquelchMode,
    squelch_hang_ms: f32,
    split: bool,
    min_samples: u64,
    agc_on: bool,
    filter_on: bool,
    denoise_on: bool,
    denoise_floor_db: f32,
    notch_freq: Option<f64>,
    notch_q: f64,
    low: f64,
    high: f64,
    lpf_hz: f64,
}

/// Buffered UDP PCM sink: little-endian s16 datagrams of ~512 samples.
struct UdpOut {
    sock: UdpSocket,
    buf: Vec<u8>,
}

impl UdpOut {
    /// 512 samples * 2 bytes = 1024-byte datagrams (under the typical MTU).
    const PACKET_SAMPLES: usize = 512;

    fn new(dest: &str) -> Result<UdpOut> {
        let sock = UdpSocket::bind("0.0.0.0:0").context("binding UDP socket")?;
        sock.connect(dest).with_context(|| format!("connecting UDP to {dest}"))?;
        Ok(UdpOut {
            sock,
            buf: Vec::with_capacity(Self::PACKET_SAMPLES * 2),
        })
    }

    fn push(&mut self, s16: i16) {
        self.buf.extend_from_slice(&s16.to_le_bytes());
        if self.buf.len() >= Self::PACKET_SAMPLES * 2 {
            let _ = self.sock.send(&self.buf);
            self.buf.clear();
        }
    }
}

/// Decodes one 64-bit framed record into (seq, channel, sample).
fn unpack(word: u64) -> (u32, u8, i32, u8) {
    // audio occupies bits [23:0], sign-extend from 24 to 32 bits
    let raw = (word & ((1u64 << AUDIO_FIELD) - 1)) as u32;
    let sample = ((raw << (32 - AUDIO_FIELD)) as i32) >> (32 - AUDIO_FIELD);
    let carrier = ((word >> AUDIO_FIELD) & ((1u64 << CARRIER_FIELD) - 1)) as u8;
    let chan = ((word >> (AUDIO_FIELD + CARRIER_FIELD)) & ((1u64 << CHAN_FIELD) - 1)) as u8;
    let seq = ((word >> (AUDIO_FIELD + CARRIER_FIELD + CHAN_FIELD)) & ((1u64 << SEQ_FIELD) - 1))
        as u32;
    (seq, chan, sample, carrier)
}

/// Formats a UTC timestamp `YYYYmmdd_HHMMSS` for a filename (no extra deps).
fn utc_stamp(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    format!("{year:04}{m:02}{d:02}_{hh:02}{mm:02}{ss:02}")
}

/// Writes/refreshes a canonical 44-byte PCM WAV header (mono, 16-bit).
fn write_wav_header<W: Write + Seek>(w: &mut W, data_bytes: u32, rate: u32) -> Result<()> {
    w.seek(SeekFrom::Start(0))?;
    let byte_rate = rate * 2;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_bytes).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?; // PCM
    w.write_all(&1u16.to_le_bytes())?; // mono
    w.write_all(&rate.to_le_bytes())?;
    w.write_all(&byte_rate.to_le_bytes())?;
    w.write_all(&2u16.to_le_bytes())?; // block align
    w.write_all(&16u16.to_le_bytes())?; // bits/sample
    w.write_all(b"data")?;
    w.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}

/// Manages the on-disk file(s) for one channel.
struct Recorder {
    is_wav: bool,
    writer: Option<BufWriter<File>>,
    path: Option<PathBuf>,
    data_bytes: u64,
    file_samples: u64,
}

impl Recorder {
    fn new(is_wav: bool) -> Recorder {
        Recorder {
            is_wav,
            writer: None,
            path: None,
            data_bytes: 0,
            file_samples: 0,
        }
    }

    fn is_open(&self) -> bool {
        self.writer.is_some()
    }

    fn open(&mut self, path: PathBuf, rate: u32) -> Result<()> {
        if self.writer.is_some() {
            self.close(rate, 0)?;
        }
        let mut f = BufWriter::new(File::create(&path)?);
        if self.is_wav {
            write_wav_header(&mut f, 0, rate)?;
        }
        self.writer = Some(f);
        self.path = Some(path);
        self.data_bytes = 0;
        self.file_samples = 0;
        Ok(())
    }

    fn write(&mut self, s16: i16) -> Result<()> {
        if let Some(w) = self.writer.as_mut() {
            w.write_all(&s16.to_le_bytes())?;
            self.data_bytes += 2;
            self.file_samples += 1;
        }
        Ok(())
    }

    /// Finalizes the current file, deleting it if shorter than `min_samples`.
    fn close(&mut self, rate: u32, min_samples: u64) -> Result<()> {
        let Some(mut w) = self.writer.take() else {
            return Ok(());
        };
        w.flush()?;
        if self.is_wav {
            let f = w.get_mut();
            write_wav_header(f, self.data_bytes as u32, rate)?;
        }
        drop(w);
        if self.file_samples < min_samples {
            if let Some(p) = self.path.as_ref() {
                let _ = fs::remove_file(p);
            }
        }
        self.path = None;
        Ok(())
    }

    /// Flush and (continuous WAV) refresh the header in place without closing.
    fn checkpoint(&mut self, rate: u32) -> Result<()> {
        if let Some(w) = self.writer.as_mut() {
            w.flush()?;
            if self.is_wav {
                let f = w.get_mut();
                let pos = f.stream_position()?;
                write_wav_header(f, self.data_bytes as u32, rate)?;
                f.seek(SeekFrom::Start(pos))?;
            }
        }
        Ok(())
    }
}

/// STFT frame size for the spectral denoiser (~16 ms at 15625 sps).
const DENOISE_FRAME: usize = 256;

/// All per-channel state: drop tracking, DSP, recorder, and stats.
struct Channel {
    index: usize,
    last_seq: Option<u32>,
    squelch: Squelch,
    /// Audio-energy VOX used only in carrier-squelch mode to drive the `above`
    /// (speech-present) flag for AGC/denoise; the main squelch then keys on the
    /// carrier, whose units don't match the audio sample magnitude.
    audio_gate: Option<Squelch>,
    vf: VoiceFilter,
    lpf: Option<LowPass>,
    notch: Option<Notch>,
    denoise: Denoise,
    agc: Agc,
    recorder: Option<Recorder>,
    udp: Option<UdpOut>,
    /// Icecast feeds for this channel (fan-out: one sender per destination).
    icecast: Vec<SyncSender<i16>>,
    // stats since last report
    interval_count: u64,
    interval_peak: i32,
    // totals
    total: u64,
    drops: u64,
}

impl Channel {
    fn new(index: usize, cfg: &Config) -> Channel {
        let recorder = match cfg.mode {
            Mode::Stats => None,
            Mode::Wav => Some(Recorder::new(true)),
            Mode::Raw => Some(Recorder::new(false)),
        };
        Channel {
            index,
            last_seq: None,
            squelch: Squelch::new(
                SquelchConfig::new(cfg.squelch_mode, cfg.rate).with_hang_ms(cfg.squelch_hang_ms),
            ),
            audio_gate: match cfg.squelch_mode {
                SquelchMode::Carrier { snr_db } => Some(Squelch::new(SquelchConfig::new(
                    SquelchMode::AutoSnr { snr_db },
                    cfg.rate,
                ))),
                _ => None,
            },
            vf: VoiceFilter::new(cfg.rate as f64, cfg.low, cfg.high),
            lpf: (cfg.lpf_hz > 0.0).then(|| LowPass::new(cfg.rate as f64, cfg.lpf_hz)),
            notch: cfg
                .notch_freq
                .map(|f| Notch::new(f, cfg.rate as f64, cfg.notch_q)),
            denoise: Denoise::new(DENOISE_FRAME, cfg.denoise_floor_db),
            agc: Agc::new(),
            recorder,
            udp: None,
            icecast: Vec::new(),
            interval_count: 0,
            interval_peak: 0,
            total: 0,
            drops: 0,
        }
    }

    /// True when some consumer needs the processed audio for this sample.
    fn needs_audio(&self) -> bool {
        self.recorder.is_some() || self.udp.is_some() || !self.icecast.is_empty()
    }

    /// Produces the gated/processed 16-bit sample for this input sample.
    fn audio_pcm(&mut self, sample: i32, open: bool, just_opened: bool, above: bool, cfg: &Config) -> i16 {
        if !open {
            return if cfg.agc_on {
                (self.agc.process(0.0, false, false, false) * 32767.0) as i16
            } else {
                0
            };
        }
        let mut v = sample as f64;
        if cfg.filter_on {
            v = self.vf.process(v);
        }
        // Distinct standalone LPF stage (independent of the band-pass).
        if let Some(lp) = self.lpf.as_mut() {
            v = lp.process(v);
        }
        if cfg.notch_freq.is_some() {
            if let Some(n) = self.notch.as_mut() {
                v = n.process(v);
            }
        }
        let mut vn = (v / SAMPLE_SCALE as f64) as f32;
        if cfg.denoise_on {
            // Learn the noise model from below-threshold (non-speech) samples.
            vn = self.denoise.process(vn, !above);
        }
        if cfg.agc_on {
            let y = self.agc.process(vn, true, just_opened, above);
            (y as f64 * 32767.0).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
        } else {
            let scaled = vn as f64 * SAMPLE_SCALE as f64 * 2f64.powi(-cfg.shift);
            scaled.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
        }
    }

    fn push(&mut self, seq: u32, sample: i32, carrier: u8, cfg: &Config) -> Result<()> {
        if let Some(prev) = self.last_seq {
            let expected = (prev + 1) % SEQ_MOD;
            if seq != expected {
                self.drops += u64::from((seq + SEQ_MOD - expected) % SEQ_MOD);
            }
        }
        self.last_seq = Some(seq);
        self.total += 1;
        self.interval_count += 1;
        self.interval_peak = self.interval_peak.max(sample.abs());

        // Carrier-power squelch keys on the FPGA carrier level; all others on the
        // audio magnitude. (Carrier is 0 on bitstreams that don't ship it.)
        let level = if matches!(cfg.squelch_mode, SquelchMode::Carrier { .. }) {
            decode_carrier(carrier)
        } else {
            sample.unsigned_abs() as f32
        };
        self.squelch.process(level);
        let gated = !matches!(cfg.squelch_mode, SquelchMode::Off);
        let open = !gated || self.squelch.is_open();
        let just_opened = gated && self.squelch.just_opened();
        let just_closed = gated && self.squelch.just_closed();
        // Speech-present flag for AGC/denoise is always audio-energy based.
        let mag = sample.unsigned_abs() as f32;
        let above = match self.audio_gate.as_mut() {
            Some(g) => {
                g.process(mag);
                mag >= g.threshold()
            }
            None => mag >= self.squelch.threshold(),
        };

        if !self.needs_audio() {
            return Ok(());
        }

        // One processed sample feeds every consumer. UDP and Icecast want a
        // continuous stream (silence between transmissions); the recorder gates
        // it per the split policy.
        let pcm = self.audio_pcm(sample, open, just_opened, above, cfg);
        if let Some(u) = self.udp.as_mut() {
            u.push(pcm);
        }
        for tx in &self.icecast {
            let _ = tx.try_send(pcm); // drop on back-pressure rather than block
        }

        if let Some(rec) = self.recorder.as_mut() {
            if cfg.split && gated {
                // One file per transmission.
                if just_opened {
                    let path = cfg.out_dir.join(format!(
                        "ch{:02}_{}.{}",
                        self.index,
                        utc_stamp(SystemTime::now()),
                        if cfg.mode == Mode::Wav { "wav" } else { "s16" }
                    ));
                    rec.open(path, cfg.rate)?;
                }
                if rec.is_open() {
                    rec.write(pcm)?;
                }
                if just_closed {
                    rec.close(cfg.rate, cfg.min_samples)?;
                }
            } else {
                // Continuous single file (gated audio flows; closed = silence).
                if !rec.is_open() {
                    let path = cfg.out_dir.join(format!(
                        "ch{:02}.{}",
                        self.index,
                        if cfg.mode == Mode::Wav { "wav" } else { "s16" }
                    ));
                    rec.open(path, cfg.rate)?;
                }
                rec.write(pcm)?;
            }
        }
        Ok(())
    }

    /// Pushes the channel's current state into the metrics snapshot.
    fn update_metric(&self, m: &metrics::ChannelMetric) {
        m.set(
            self.total,
            self.drops,
            self.squelch.open_count(),
            self.squelch.level(),
            self.squelch.noise_floor(),
            self.squelch.is_open(),
        );
    }

    /// Finalize any open split recording (called on reconnect/shutdown).
    fn finalize(&mut self, cfg: &Config) {
        if let Some(r) = self.recorder.as_mut() {
            let _ = r.close(cfg.rate, cfg.min_samples);
        }
    }

    fn checkpoint(&mut self, cfg: &Config) -> Result<()> {
        if let Some(r) = self.recorder.as_mut() {
            r.checkpoint(cfg.rate)?;
        }
        Ok(())
    }
}

fn report(channels: &mut [Channel], elapsed: f64, overflow_hint: bool) {
    let mut active = 0;
    let mut total_drops = 0u64;
    println!(
        "---- airband {:.1}s ---- {}",
        elapsed,
        if overflow_hint { "(stream gaps seen)" } else { "" }
    );
    println!("  ch    sps   total      drops   peak(dBFS)  floor(dBFS)  tx");
    for s in channels.iter_mut() {
        total_drops += s.drops;
        if s.interval_count > 0 {
            active += 1;
            let sps = s.interval_count as f64 / elapsed;
            let peak_db = level_to_dbfs(s.interval_peak as f32);
            let floor_db = level_to_dbfs(s.squelch.noise_floor());
            println!(
                "  {:2}  {:6.0}  {:9}  {:9}  {:9.1}  {:9.1}  {:4}",
                s.index, sps, s.total, s.drops, peak_db, floor_db, s.squelch.open_count()
            );
        }
        s.interval_count = 0;
        s.interval_peak = 0;
    }
    println!("  active channels: {active}, cumulative drops: {total_drops}");
}

fn run_session(
    args: &Args,
    cfg: &Config,
    channels: &mut [Channel],
    metrics: Option<&Arc<Metrics>>,
    last_report: &mut Instant,
) -> Result<()> {
    let stream = TcpStream::connect(&args.addr)
        .with_context(|| format!("connecting to {}", args.addr))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    eprintln!("connected to {}", args.addr);
    let mut reader = BufReader::with_capacity(1 << 16, stream);
    let mut frame = [0u8; FRAME_BYTES];
    let interval = Duration::from_secs(args.stats_interval.max(1));

    // Carrier-squelch: track the latest carrier per channel and periodically push
    // a shared, cross-channel-derived threshold to every channel's squelch.
    let carrier_mode = matches!(cfg.squelch_mode, SquelchMode::Carrier { .. });
    let carrier_snr_ratio = 10f32.powf(args.squelch_snr / 20.0);
    let mut last_carrier = vec![0f32; channels.len()];
    let mut since_carrier_update: u64 = 0;

    loop {
        reader.read_exact(&mut frame)?;
        let (seq, chan, sample, carrier) = unpack(u64::from_le_bytes(frame));
        if carrier_mode {
            if let Some(c) = last_carrier.get_mut(usize::from(chan)) {
                *c = decode_carrier(carrier);
            }
            since_carrier_update += 1;
            if since_carrier_update >= CARRIER_UPDATE_FRAMES {
                let thr =
                    carrier_noise_threshold(&last_carrier, CARRIER_NOISE_PCT, carrier_snr_ratio);
                for ch in channels.iter_mut() {
                    ch.squelch.set_threshold(thr);
                }
                since_carrier_update = 0;
            }
        }
        if let Some(ch) = channels.get_mut(usize::from(chan)) {
            ch.push(seq, sample, carrier, cfg)?;
        }
        if last_report.elapsed() >= interval {
            let elapsed = last_report.elapsed().as_secs_f64();
            for ch in channels.iter_mut() {
                ch.checkpoint(cfg)?;
            }
            if let Some(m) = metrics {
                for ch in channels.iter() {
                    ch.update_metric(&m.channels[ch.index]);
                }
            }
            if cfg.mode == Mode::Stats {
                report(channels, elapsed, false);
            } else {
                let drops: u64 = channels.iter().map(|c| c.drops).sum();
                let tx: u64 = channels.iter().map(|c| c.squelch.open_count()).sum();
                eprintln!(
                    "[{:.0}s] {} channels active, {} transmissions, cumulative drops {}",
                    elapsed,
                    channels.iter().filter(|c| c.total > 0).count(),
                    tx,
                    drops
                );
            }
            *last_report = Instant::now();
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

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
    let cfg = Config {
        mode: args.mode,
        out_dir: args.out_dir.clone(),
        rate: args.rate,
        shift: args.shift,
        squelch_mode,
        squelch_hang_ms: args.squelch_hang_ms,
        split: !args.no_split,
        min_samples: (args.min_transmission_ms * args.rate as u64) / 1000,
        agc_on: !args.no_agc,
        filter_on: !args.no_filter,
        denoise_on: !args.no_denoise,
        denoise_floor_db: args.denoise_floor_db,
        notch_freq: args.notch,
        notch_q: args.notch_q,
        low: args.filter_low,
        high: args.filter_high,
        lpf_hz: args.lpf_hz,
    };

    if cfg.mode != Mode::Stats {
        fs::create_dir_all(&cfg.out_dir)
            .with_context(|| format!("creating output dir {:?}", cfg.out_dir))?;
    }
    let mut channels: Vec<Channel> = (0..args.channels).map(|c| Channel::new(c, &cfg)).collect();
    let mut last_report = Instant::now();

    // Optional UDP PCM sink.
    if let Some(ch) = args.udp_channel {
        let dest = args
            .udp_dest
            .as_ref()
            .context("--udp-channel requires --udp-dest host:port")?;
        let target = channels
            .get_mut(ch)
            .with_context(|| format!("--udp-channel {ch} out of range"))?;
        target.udp = Some(UdpOut::new(dest)?);
        eprintln!("udp: streaming channel {ch} PCM to {dest}");
    }

    // Icecast feeds: a JSON feeds file (many channels / many servers) and/or the
    // single-stream --icecast-* flags. Both produce IcecastConfig entries that
    // are attached to their channel (a channel may have several = fan-out).
    let mut feed_cfgs: Vec<IcecastConfig> = Vec::new();
    if let Some(path) = args.feeds.as_ref() {
        feed_cfgs.extend(feeds::load(path, cfg.rate, channels.len())?);
    }
    if let Some(ch) = args.icecast_channel {
        let tls = TlsMode::parse(&args.icecast_tls).map_err(|e| anyhow::anyhow!(e))?;
        feed_cfgs.push(IcecastConfig {
            host: args.icecast_host.clone(),
            port: args.icecast_port,
            mount: args.icecast_mount.clone(),
            user: args.icecast_user.clone(),
            password: args.icecast_password.clone(),
            bitrate: args.icecast_bitrate,
            out_rate: args.icecast_samplerate,
            in_rate: cfg.rate,
            name: args.icecast_name.clone(),
            genre: args.icecast_genre.clone(),
            description: args.icecast_description.clone(),
            tls,
            tls_insecure: args.icecast_tls_insecure,
            channel: ch,
        });
    }
    for icfg in feed_cfgs {
        let ch = icfg.channel;
        let target = channels
            .get_mut(ch)
            .with_context(|| format!("icecast feed channel {ch} out of range"))?;
        let scheme = if icfg.tls == TlsMode::Disabled { "icecast" } else { "icecast+tls" };
        eprintln!(
            "icecast: feed ch{ch} -> {scheme}://{}:{}{}",
            icfg.host, icfg.port, icfg.mount
        );
        target.icecast.push(icecast::spawn(icfg));
    }

    // Optional Prometheus metrics endpoint.
    let metrics = if args.metrics_port > 0 {
        let m = Metrics::new(args.channels);
        metrics::serve(Arc::clone(&m), args.metrics_port);
        Some(m)
    } else {
        None
    };

    eprintln!(
        "airband-reader: {} channels, audio {} sps, mode {:?}, squelch {:?}, split {}, agc {}",
        args.channels, cfg.rate, cfg.mode, args.squelch, cfg.split, cfg.agc_on
    );

    loop {
        if let Err(e) = run_session(&args, &cfg, &mut channels, metrics.as_ref(), &mut last_report) {
            eprintln!("stream error ({e:#}); reconnecting in 1s");
            for ch in channels.iter_mut() {
                ch.finalize(&cfg); // close any open split recording
                ch.last_seq = None; // a reconnect is an expected seq discontinuity
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_stamp_epoch() {
        assert_eq!(utc_stamp(UNIX_EPOCH), "19700101_000000");
    }

    #[test]
    fn utc_stamp_known_instant() {
        // 2021-01-01 00:00:00 UTC = 1609459200
        let t = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        assert_eq!(utc_stamp(t), "20210101_000000");
        // 2021-01-01 01:02:10 UTC = 1609459200 + 3730
        let t2 = UNIX_EPOCH + Duration::from_secs(1_609_459_200 + 3730);
        assert_eq!(utc_stamp(t2), "20210101_010210");
    }

    #[test]
    fn unpack_roundtrip() {
        // layout: seq[63:40] | chan[39:32] | carrier[31:24] | audio[23:0]
        let word =
            (0x00ABCDEFu64 << 40) | (0x07u64 << 32) | (0x05u64 << 24) | (0x12_3456u64);
        let (seq, chan, sample, carrier) = unpack(word);
        assert_eq!(seq, 0x00ABCDEF & 0xff_ffff);
        assert_eq!(chan, 7);
        assert_eq!(carrier, 0x05);
        assert_eq!(sample, 0x12_3456); // positive 24-bit value
    }

    #[test]
    fn unpack_sign_extends_negative_audio() {
        // audio 0x80_0000 is the most-negative 24-bit value (-2^23).
        let word = (0x05u64 << 24) | 0x80_0000u64;
        let (_, _, sample, carrier) = unpack(word);
        assert_eq!(carrier, 0x05);
        assert_eq!(sample, -(1 << 23));
    }
}
