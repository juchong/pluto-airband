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

mod icecast;
mod metrics;

use airband_dsp::{
    level_to_dbfs, Agc, Notch, Squelch, SquelchConfig, SquelchMode, VoiceFilter, SAMPLE_SCALE,
};
use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use icecast::IcecastConfig;
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
const SAMPLE_FIELD: u32 = 32;
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
    /// Audio sample rate in Hz (= AD9361 Fs / 128 / 7); used for WAV headers
    #[arg(long, default_value_t = 15625)]
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
    /// Disable the voice band-pass (on by default; 300-3400 Hz).
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
    /// Stream this channel to an Icecast mount (needs --icecast-host/password).
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
    notch_freq: Option<f64>,
    notch_q: f64,
    low: f64,
    high: f64,
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
fn unpack(word: u64) -> (u32, u8, i32) {
    let sample = (word & ((1u64 << SAMPLE_FIELD) - 1)) as u32 as i32;
    let chan = ((word >> SAMPLE_FIELD) & ((1u64 << CHAN_FIELD) - 1)) as u8;
    let seq = ((word >> (SAMPLE_FIELD + CHAN_FIELD)) & ((1u64 << SEQ_FIELD) - 1)) as u32;
    (seq, chan, sample)
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

/// All per-channel state: drop tracking, DSP, recorder, and stats.
struct Channel {
    index: usize,
    last_seq: Option<u32>,
    squelch: Squelch,
    vf: VoiceFilter,
    notch: Option<Notch>,
    agc: Agc,
    recorder: Option<Recorder>,
    udp: Option<UdpOut>,
    icecast: Option<SyncSender<i16>>,
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
            vf: VoiceFilter::new(cfg.rate as f64, cfg.low, cfg.high),
            notch: cfg
                .notch_freq
                .map(|f| Notch::new(f, cfg.rate as f64, cfg.notch_q)),
            agc: Agc::new(),
            recorder,
            udp: None,
            icecast: None,
            interval_count: 0,
            interval_peak: 0,
            total: 0,
            drops: 0,
        }
    }

    /// True when some consumer needs the processed audio for this sample.
    fn needs_audio(&self) -> bool {
        self.recorder.is_some() || self.udp.is_some() || self.icecast.is_some()
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
        if cfg.notch_freq.is_some() {
            if let Some(n) = self.notch.as_mut() {
                v = n.process(v);
            }
        }
        if cfg.agc_on {
            let y = self.agc.process((v / SAMPLE_SCALE as f64) as f32, true, just_opened, above);
            (y as f64 * 32767.0).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
        } else {
            let scaled = v * 2f64.powi(-cfg.shift);
            scaled.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
        }
    }

    fn push(&mut self, seq: u32, sample: i32, cfg: &Config) -> Result<()> {
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

        let mag = sample.unsigned_abs() as f32;
        self.squelch.process(mag);
        let gated = !matches!(cfg.squelch_mode, SquelchMode::Off);
        let open = !gated || self.squelch.is_open();
        let just_opened = gated && self.squelch.just_opened();
        let just_closed = gated && self.squelch.just_closed();
        let above = mag >= self.squelch.threshold();

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
        if let Some(tx) = self.icecast.as_ref() {
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

    loop {
        reader.read_exact(&mut frame)?;
        let (seq, chan, sample) = unpack(u64::from_le_bytes(frame));
        if let Some(ch) = channels.get_mut(usize::from(chan)) {
            ch.push(seq, sample, cfg)?;
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
        notch_freq: args.notch,
        notch_q: args.notch_q,
        low: args.filter_low,
        high: args.filter_high,
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

    // Optional Icecast MP3 source.
    if let Some(ch) = args.icecast_channel {
        let target = channels
            .get_mut(ch)
            .with_context(|| format!("--icecast-channel {ch} out of range"))?;
        let icfg = IcecastConfig {
            host: args.icecast_host.clone(),
            port: args.icecast_port,
            mount: args.icecast_mount.clone(),
            user: args.icecast_user.clone(),
            password: args.icecast_password.clone(),
            bitrate: args.icecast_bitrate,
            out_rate: args.icecast_samplerate,
            in_rate: cfg.rate,
            name: args.icecast_name.clone(),
            channel: ch,
        };
        target.icecast = Some(icecast::spawn(icfg));
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
        let word = (0x00ABCDEFu64 << 40) | (0x07u64 << 32) | (0x0012_3456u64 & 0xffff_ffff);
        let (seq, chan, sample) = unpack(word);
        assert_eq!(seq, 0x00ABCDEF & 0xff_ffff);
        assert_eq!(chan, 7);
        assert_eq!(sample, 0x0012_3456);
    }
}
