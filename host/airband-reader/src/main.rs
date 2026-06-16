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
//! to detect dropped samples, scales the 24-bit audio to 16-bit PCM, and either
//! prints live per-channel statistics, writes one WAV file per channel, or
//! writes raw little-endian `s16` per channel (ready to pipe into an encoder).
//!
//! It reconnects automatically so it can run unattended.
//!
//! Examples:
//! ```text
//!   airband-reader 192.168.2.1:30000                 # live stats
//!   airband-reader 192.168.2.1:30000 --mode wav --out-dir caps
//!   airband-reader 192.168.2.1:30000 --mode raw --out-dir pcm --shift 6
//! ```

mod filter;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use filter::VoiceFilter;
use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    net::TcpStream,
    path::PathBuf,
    time::{Duration, Instant},
};

const FRAME_BYTES: usize = 8;
const SAMPLE_FIELD: u32 = 32;
const CHAN_FIELD: u32 = 8;
const SEQ_FIELD: u32 = 24;
const SEQ_MOD: u32 = 1 << SEQ_FIELD;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Mode {
    /// Print per-channel statistics (sample rate, drops, peak level)
    Stats,
    /// Write one WAV file per channel (16-bit PCM mono)
    Wav,
    /// Write raw little-endian s16 PCM per channel (chNN.s16)
    Raw,
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
    /// Bit-shift applied to the 24-bit sample before clamping to i16.
    ///
    /// Positive = right-shift (attenuate); negative = left-shift (makeup gain).
    /// Airband AM audio is quiet (often tens of LSB at 24-bit), so a negative
    /// value is usually needed to bring voice up to a usable 16-bit level.
    /// Tune on a real signal: too negative clips, too positive is silent.
    #[arg(long, default_value_t = -6, allow_hyphen_values = true)]
    shift: i32,
    /// Enable the voice band-pass filter (OFF by default).
    ///
    /// Diagnostic/fallback only: a 300-3400 Hz band-pass masks out-of-voice-band
    /// artifacts but also degrades voice. It does NOT fix the on-air "buzz", which
    /// is an RF hardware spur (a comb locked to the Pluto's 40 MHz reference),
    /// upstream of all DSP -- see firmware/diagnostics/. Leave off in normal use.
    #[arg(long)]
    filter: bool,
    /// Voice band-pass low corner in Hz (high-pass)
    #[arg(long, default_value_t = 300.0)]
    filter_low: f64,
    /// Voice band-pass high corner in Hz (low-pass)
    #[arg(long, default_value_t = 3400.0)]
    filter_high: f64,
    /// Statistics print interval in seconds
    #[arg(long, default_value_t = 5)]
    stats_interval: u64,
}

/// Decodes one 64-bit framed record into (seq, channel, sample).
fn unpack(word: u64) -> (u32, u8, i32) {
    let sample = (word & ((1u64 << SAMPLE_FIELD) - 1)) as u32 as i32;
    let chan = ((word >> SAMPLE_FIELD) & ((1u64 << CHAN_FIELD) - 1)) as u8;
    let seq = ((word >> (SAMPLE_FIELD + CHAN_FIELD)) & ((1u64 << SEQ_FIELD) - 1)) as u32;
    (seq, chan, sample)
}

struct Sink {
    writer: Option<BufWriter<File>>,
    is_wav: bool,
    data_bytes: u64,
    // optional per-channel voice band-pass (applied before scaling on output)
    filter: Option<VoiceFilter>,
    // statistics (since last report)
    interval_count: u64,
    interval_peak: i32,
    // totals
    total: u64,
    drops: u64,
    last_seq: Option<u32>,
}

impl Sink {
    fn new(
        mode: Mode,
        out_dir: &PathBuf,
        chan: usize,
        rate: u32,
        filter: Option<VoiceFilter>,
    ) -> Result<Sink> {
        let (writer, is_wav) = match mode {
            Mode::Stats => (None, false),
            Mode::Wav => {
                let path = out_dir.join(format!("ch{chan:02}.wav"));
                let mut f = BufWriter::new(File::create(&path)?);
                write_wav_header(&mut f, 0, rate)?;
                (Some(f), true)
            }
            Mode::Raw => {
                let path = out_dir.join(format!("ch{chan:02}.s16"));
                (Some(BufWriter::new(File::create(&path)?)), false)
            }
        };
        Ok(Sink {
            writer,
            is_wav,
            data_bytes: 0,
            filter,
            interval_count: 0,
            interval_peak: 0,
            total: 0,
            drops: 0,
            last_seq: None,
        })
    }

    fn push(&mut self, seq: u32, sample: i32, shift: i32) -> Result<()> {
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

        if self.writer.is_some() {
            // Optional voice band-pass, then scale. Positive shift attenuates,
            // negative shift applies makeup gain; done in float so it composes
            // with the (fractional) filter output. interval_peak stays on the
            // raw 24-bit sample so stats still report the true level.
            let voice = match self.filter.as_mut() {
                Some(f) => f.process(sample as f64),
                None => sample as f64,
            };
            let scaled = voice * 2f64.powi(-shift);
            let s16 = scaled.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
            let w = self.writer.as_mut().unwrap();
            w.write_all(&s16.to_le_bytes())?;
            self.data_bytes += 2;
        }
        Ok(())
    }

    /// Flush and (for WAV) refresh the header with the current data size.
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

fn write_wav_header<W: Write + Seek>(w: &mut W, data_bytes: u32, rate: u32) -> Result<()> {
    w.seek(SeekFrom::Start(0))?;
    let byte_rate = rate * 2; // mono, 16-bit
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_bytes).to_le_bytes())?;
    w.write_all(b"WAVE")?;
    w.write_all(b"fmt ")?;
    w.write_all(&16u32.to_le_bytes())?; // fmt chunk size
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

fn report(sinks: &mut [Sink], elapsed: f64, overflow_hint: bool) {
    let mut active = 0;
    let mut total_drops = 0u64;
    println!(
        "---- airband {:.1}s ---- {}",
        elapsed,
        if overflow_hint { "(stream gaps seen)" } else { "" }
    );
    println!("  ch    sps   total      drops   peak(24b)");
    for (c, s) in sinks.iter_mut().enumerate() {
        total_drops += s.drops;
        if s.interval_count > 0 {
            active += 1;
            let sps = s.interval_count as f64 / elapsed;
            println!(
                "  {:2}  {:6.0}  {:9}  {:9}  {:9}",
                c, sps, s.total, s.drops, s.interval_peak
            );
        }
        s.interval_count = 0;
        s.interval_peak = 0;
    }
    println!("  active channels: {active}, cumulative drops: {total_drops}");
}

fn run_session(args: &Args, sinks: &mut [Sink], last_report: &mut Instant) -> Result<()> {
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
        if let Some(sink) = sinks.get_mut(usize::from(chan)) {
            sink.push(seq, sample, args.shift)?;
        }
        if last_report.elapsed() >= interval {
            let elapsed = last_report.elapsed().as_secs_f64();
            for s in sinks.iter_mut() {
                s.checkpoint(args.rate)?;
            }
            if args.mode == Mode::Stats {
                report(sinks, elapsed, false);
            } else {
                let drops: u64 = sinks.iter().map(|s| s.drops).sum();
                eprintln!(
                    "[{:.0}s] writing {} channels, cumulative drops {}",
                    elapsed,
                    sinks.iter().filter(|s| s.total > 0).count(),
                    drops
                );
            }
            *last_report = Instant::now();
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.mode != Mode::Stats {
        fs::create_dir_all(&args.out_dir)
            .with_context(|| format!("creating output dir {:?}", args.out_dir))?;
    }
    let mut sinks = (0..args.channels)
        .map(|c| {
            let filter = args
                .filter
                .then(|| VoiceFilter::new(args.rate as f64, args.filter_low, args.filter_high));
            Sink::new(args.mode, &args.out_dir, c, args.rate, filter)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut last_report = Instant::now();

    eprintln!(
        "airband-reader: {} channels, audio {} sps, mode {:?}",
        args.channels, args.rate, args.mode
    );
    // Reconnect loop for unattended operation.
    loop {
        if let Err(e) = run_session(&args, &mut sinks, &mut last_report) {
            eprintln!("stream error ({e:#}); reconnecting in 1s");
            for s in sinks.iter_mut() {
                let _ = s.checkpoint(args.rate);
                s.last_seq = None; // a reconnect is an expected seq discontinuity
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}
