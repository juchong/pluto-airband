//! Host-side reader for the Pluto airband framed-audio DMA stream.
//!
//! The Pluto (maia-httpd) serves a raw TCP stream of 64-bit little-endian
//! framed-audio records produced by the FPGA `AudioFramer`:
//!
//! ```text
//!   bits [23:0]  : audio sample, signed, 24-bit two's complement
//!   bits [31:24] : carrier level (8-bit minifloat of the AM carrier; 0 = none)
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
mod monitor;
mod mqtt;

use airband_dfn::{DfnEnhancer, DfnParams, InferencePermit, Presence};
use airband_dsp::{
    carrier_noise_threshold, decode_carrier, level_to_dbfs, Agc, Denoise, LowPass, Notch, Squelch,
    SquelchConfig, SquelchMode, VoiceFilter, SAMPLE_SCALE,
};

/// Carrier-squelch: population percentile used as the cross-channel noise
/// reference, and how often (in frames) the shared threshold is recomputed.
/// The median (0.5) matches airband-listen: at useful gain several channels are
/// elevated by the conducted comb, so a high percentile would inflate the
/// "noise" reference and push the threshold above real traffic.
const CARRIER_NOISE_PCT: f32 = 0.5;
const CARRIER_UPDATE_FRAMES: u64 = 8192;
use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use icecast::{IcecastConfig, TlsMode};
use metrics::{FeedMetric, Metrics};
use std::{
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    net::{TcpStream, ToSocketAddrs, UdpSocket},
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
        Arc, Barrier, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Makeup gain applied to the raw demod sample on the `tap=pre` monitor so the
/// (typically quiet) airband audio is audible. Debug path only; never shipped.
const MONITOR_PRE_GAIN: f64 = 8.0;

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
    /// Audio sample rate in Hz (= AD9361 Fs / 160 / 5 = 20000 at the 16 MHz build)
    #[arg(long, default_value_t = 20000)]
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
    /// Enable the STFT spectral noise reducer (off by default; the DFN-centric
    /// chain lets DeepFilterNet do the cleanup, like airband-listen).
    #[arg(long)]
    denoise: bool,
    /// Noise-reducer spectral floor in dB (more negative = more aggressive cut).
    #[arg(long, default_value_t = -18.0, allow_hyphen_values = true)]
    denoise_floor_db: f32,
    /// Enable the voice band-pass (off by default; 300-3400 Hz). The DFN-centric
    /// chain leaves it off so DeepFilterNet cleans up in isolation.
    #[arg(long)]
    filter: bool,
    /// Voice band-pass low corner in Hz (high-pass)
    #[arg(long, default_value_t = 300.0)]
    filter_low: f64,
    /// Voice band-pass high corner in Hz (low-pass)
    #[arg(long, default_value_t = 3400.0)]
    filter_high: f64,
    /// Standalone low-pass -3 dB cutoff in Hz, applied after the band-pass
    /// (a distinct LPF stage; 0 = off, the default; e.g. 2500 to enable)
    #[arg(long, default_value_t = 0.0)]
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
    /// Serve Prometheus /metrics, /healthz, and /status on this TCP port (0 = disabled).
    #[arg(long, default_value_t = 0)]
    metrics_port: u16,
    /// Pluto web port to probe for reachability ("is the Pluto connected?"). The
    /// host is taken from the stream address; this is the maia-httpd web port.
    #[arg(long, default_value_t = 8000)]
    pluto_web_port: u16,
    /// Low-latency debug-listen port (0 = disabled). Serves raw-PCM WAV at
    /// GET /listen/<ch>.wav?tap=pre|post (no Icecast/MP3). Coexists with feeds.
    #[arg(long, default_value_t = 0)]
    monitor_port: u16,
    /// MQTT broker host for Home Assistant publishing (unset = MQTT disabled).
    #[arg(long)]
    mqtt_broker: Option<String>,
    /// MQTT broker port.
    #[arg(long, default_value_t = 1883)]
    mqtt_port: u16,
    /// MQTT username (optional; use ${ENV} via the systemd EnvironmentFile).
    #[arg(long)]
    mqtt_user: Option<String>,
    /// MQTT password (optional; use ${ENV} via the systemd EnvironmentFile).
    #[arg(long)]
    mqtt_pass: Option<String>,
    /// MQTT base topic prefix and Home Assistant node id.
    #[arg(long, default_value = "pluto-airband")]
    mqtt_prefix: String,
    /// Home Assistant MQTT discovery prefix.
    #[arg(long, default_value = "homeassistant")]
    mqtt_discovery_prefix: String,
    /// Seconds between MQTT state publishes.
    #[arg(long, default_value_t = 5)]
    mqtt_interval: u64,
    /// Also publish per-channel open/carrier entities over MQTT (noisy: many
    /// channels x several entities). Off by default.
    #[arg(long)]
    mqtt_per_channel: bool,
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
    /// Disable DeepFilterNet neural speech enhancement (on by default; runs on
    /// squelch-open audio after the AGC, exactly like airband-listen).
    #[arg(long)]
    no_dfn: bool,
    /// DeepFilterNet local-SNR floor in dB (frames below are treated as noise).
    /// Lower keeps faint speech; -20 avoids the "garbled mumble" zero-gating.
    #[arg(long, default_value_t = -20.0, allow_hyphen_values = true)]
    dfn_min_snr: f32,
    /// DeepFilterNet max attenuation in dB (>=100 = unlimited). 15 caps depth so
    /// noise-like consonants are not fully muted.
    #[arg(long, default_value_t = 15.0)]
    dfn_atten_lim: f32,
    /// DeepFilterNet post-filter beta (0 = off; ~0.02 trims residual musical noise).
    #[arg(long, default_value_t = 0.02)]
    dfn_pf_beta: f32,
    /// Maximum number of channels running DeepFilterNet inference at once. Excess
    /// keyed channels wait (buffer, never drop or bypass) until a slot frees,
    /// bounding NN load on the Pi 5. Default leaves a core for I/O.
    #[arg(long, default_value_t = 3)]
    dfn_max_active: usize,
    /// Post-DFN presence/brightness high-shelf boost in dB (0 = off). Restores the
    /// upper voice band DFN trims at low SNR so pilots are not muffled.
    #[arg(long, default_value_t = 8.0, allow_hyphen_values = true)]
    presence_db: f64,
    /// Presence high-shelf corner frequency in Hz.
    #[arg(long, default_value_t = 1600.0)]
    presence_hz: f64,
    /// Presence high-shelf transition Q.
    #[arg(long, default_value_t = 0.707)]
    presence_q: f64,
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
    dfn_on: bool,
    dfn_params: DfnParams,
    dfn_max_active: usize,
    presence_db: f64,
    presence_hz: f64,
    presence_q: f64,
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

/// STFT frame size for the spectral denoiser (~13 ms at 20000 sps).
const DENOISE_FRAME: usize = 256;

/// Counting semaphore bounding how many DeepFilterNet **inferences** run at once
/// (`--dfn-max-active`). It is acquired per NN hop (see [`InferencePermit`]) —
/// immediately around each forward pass, then released — so excess simultaneous
/// transmissions **buffer** (their unbounded input queue grows, adding latency)
/// instead of dropping audio or bypassing the filter, while permits free between
/// hops and rotate among all open channels (no channel starves another). This
/// keeps NN load on the Pi 5 bounded to a few cores while always filtering.
struct Semaphore {
    count: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Semaphore {
        Semaphore {
            count: Mutex::new(n.max(1)),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut c = self.count.lock().unwrap();
        while *c == 0 {
            c = self.cv.wait(c).unwrap();
        }
        *c -= 1;
    }

    fn release(&self) {
        let mut c = self.count.lock().unwrap();
        *c += 1;
        self.cv.notify_one();
    }
}

impl InferencePermit for Semaphore {
    #[inline]
    fn enter(&self) {
        self.acquire();
    }
    #[inline]
    fn leave(&self) {
        self.release();
    }
}

/// Debug-monitor tap point for the [`monitor`] HTTP endpoint.
#[derive(Copy, Clone, Debug)]
pub enum Tap {
    /// Raw demod audio (pre-DSP, pre-squelch): continuous, lowest latency, hears
    /// exactly what the receiver hears regardless of squelch/filters.
    Pre,
    /// Fully-enhanced post-DFN/presence audio: squelch-gated, byte-identical to
    /// what the feeds ship.
    Post,
}

/// A message from the router thread (or the monitor server) to a per-channel
/// worker.
enum Msg {
    /// One demultiplexed audio record: (sample, FPGA carrier byte).
    Sample(i32, u8),
    /// Stream reset (reconnect/shutdown): finalize any open recording and clear
    /// streaming DSP state so the next transmission starts clean.
    Reset,
    /// Register a runtime debug-monitor sink (added by [`monitor`] on a new
    /// listen request; pruned automatically when the client disconnects).
    AddMonitor { tap: Tap, tx: SyncSender<i16> },
}

/// Fans one PCM sample out to a set of runtime monitor sinks, dropping the
/// sample under back-pressure (slow client) and pruning a sink only when its
/// receiver has hung up (client disconnected).
fn fanout_monitors(sinks: &mut Vec<SyncSender<i16>>, s: i16) {
    sinks.retain(|tx| !matches!(tx.try_send(s), Err(TrySendError::Disconnected(_))));
}

/// Sends a state line to systemd's notify socket if running under `Type=notify`
/// (no-op otherwise). Used for readiness and the `WATCHDOG=1` keep-alive.
#[cfg(unix)]
fn sd_notify(state: &str) {
    use std::os::unix::net::UnixDatagram;
    let Ok(path) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };
    // Abstract-namespace sockets ('@'-prefixed) aren't addressable via the std
    // path API; services use a filesystem path socket, which is all we need.
    if !path.starts_with('@') {
        let _ = sock.send_to(state.as_bytes(), &path);
    }
}

#[cfg(not(unix))]
fn sd_notify(_state: &str) {}

/// Spawns the Pluto reachability probe: a periodic lightweight TCP connect to
/// the maia-httpd web port (the host is taken from the stream address). Sets the
/// `pluto_reachable` gauge and logs transitions. This deliberately does NOT open
/// a second `:30000` connection (that port is single-client).
fn spawn_pluto_probe(metrics: Arc<Metrics>, addr: &str, web_port: u16) {
    let host = addr
        .rsplit_once(':')
        .map(|(h, _)| h.to_string())
        .unwrap_or_else(|| addr.to_string());
    let target = format!("{host}:{web_port}");
    thread::spawn(move || {
        let mut prev: Option<bool> = None;
        loop {
            let ok = target
                .to_socket_addrs()
                .ok()
                .and_then(|mut a| a.next())
                .map(|sa| TcpStream::connect_timeout(&sa, Duration::from_secs(2)).is_ok())
                .unwrap_or(false);
            metrics.set_pluto_reachable(ok);
            if prev != Some(ok) {
                eprintln!("pluto: reachable={ok} ({target})");
                prev = Some(ok);
            }
            thread::sleep(Duration::from_secs(5));
        }
    });
}

/// Per-channel processing worker. Owns the channel's squelch, full audio chain
/// (band-pass/LPF/notch/denoise/AGC), DeepFilterNet enhancer, presence
/// brightness, and all output sinks (recorder/UDP/Icecast). Each runs on its own
/// thread so the Pi 5's cores process channels in parallel and a slow stage
/// (notably DFN inference) never stalls the socket-reading router.
struct Worker {
    index: usize,
    squelch: Squelch,
    /// Audio-energy VOX used only in carrier-squelch mode to drive the `above`
    /// (speech-present) flag for AGC/denoise; the main squelch then keys on the
    /// carrier, whose units don't match the audio sample magnitude.
    audio_gate: Option<Squelch>,
    /// Dedicated 300-3400 Hz band-pass used only to derive the VOX squelch level
    /// from voice-band modulation energy (mirrors airband-listen): keeps the
    /// decision independent of the playback filter and excludes the out-of-band
    /// conducted comb and low-frequency rumble.
    sq_filter: VoiceFilter,
    vf: VoiceFilter,
    lpf: Option<LowPass>,
    notch: Option<Notch>,
    denoise: Denoise,
    agc: Agc,
    /// DeepFilterNet enhancer (built eagerly at startup when enabled).
    dfn: Option<DfnEnhancer>,
    /// Post-DFN brightness high-shelf (None when `--presence-db 0`).
    presence: Option<Presence>,
    recorder: Option<Recorder>,
    udp: Option<UdpOut>,
    /// Icecast feeds for this channel (fan-out: one sender per destination).
    icecast: Vec<SyncSender<i16>>,
    /// Runtime debug-monitor sinks: `pre` taps the raw demod (continuous),
    /// `post` taps the enhanced gated audio. Added via [`Msg::AddMonitor`] and
    /// pruned when their client disconnects.
    pre_monitors: Vec<SyncSender<i16>>,
    post_monitors: Vec<SyncSender<i16>>,
    since_checkpoint: u64,
}

impl Worker {
    fn new(index: usize, cfg: &Config, udp: Option<UdpOut>, icecast: Vec<SyncSender<i16>>) -> Worker {
        let recorder = match cfg.mode {
            Mode::Stats => None,
            Mode::Wav => Some(Recorder::new(true)),
            Mode::Raw => Some(Recorder::new(false)),
        };
        Worker {
            index,
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
            sq_filter: VoiceFilter::new(cfg.rate as f64, 300.0, 3400.0),
            vf: VoiceFilter::new(cfg.rate as f64, cfg.low, cfg.high),
            lpf: (cfg.lpf_hz > 0.0).then(|| LowPass::new(cfg.rate as f64, cfg.lpf_hz)),
            notch: cfg
                .notch_freq
                .map(|f| Notch::new(f, cfg.rate as f64, cfg.notch_q)),
            denoise: Denoise::new(DENOISE_FRAME, cfg.denoise_floor_db),
            agc: Agc::new(),
            dfn: None,
            presence: (cfg.presence_db != 0.0).then(|| {
                Presence::new(cfg.rate as f64, cfg.presence_hz, cfg.presence_q, cfg.presence_db)
            }),
            recorder,
            udp,
            icecast,
            pre_monitors: Vec::new(),
            post_monitors: Vec::new(),
            since_checkpoint: 0,
        }
    }

    /// True when a statically-configured consumer needs the processed audio
    /// (gates DFN model load at startup). Runtime monitors are handled
    /// separately in [`Self::on_sample`].
    fn needs_audio(&self) -> bool {
        self.recorder.is_some() || self.udp.is_some() || !self.icecast.is_empty()
    }

    /// True when the fully-enhanced (post-DFN) sample must be computed this
    /// sample: any static sink or an attached `post` monitor.
    fn needs_post(&self) -> bool {
        self.needs_audio() || !self.post_monitors.is_empty()
    }

    /// Runs one sample through the gated audio chain (filters -> denoise -> AGC),
    /// returning a normalized f32 (DFN and presence run on this, like the
    /// listener, so the model sees a healthy in-range signal).
    fn audio_chain(&mut self, sample: i32, open: bool, just_opened: bool, above: bool, cfg: &Config) -> f32 {
        if !open {
            // Keep the AGC tail decaying so it re-seeds cleanly on the next open.
            return if cfg.agc_on {
                self.agc.process(0.0, false, false, false)
            } else {
                0.0
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
            if let Some(nf) = self.notch.as_mut() {
                v = nf.process(v);
            }
        }
        let mut vn = (v / SAMPLE_SCALE as f64) as f32;
        if cfg.denoise_on {
            // Learn the noise model from below-threshold (non-speech) samples.
            vn = self.denoise.process(vn, !above);
        }
        if !cfg.agc_on {
            return vn;
        }
        self.agc.process(vn, true, just_opened, above)
    }

    /// Applies DeepFilterNet to one chain-output sample. Runs only while the
    /// squelch is open. In gated modes each NN hop acquires a concurrency permit
    /// just for that inference and releases it immediately after, so an overran
    /// hop waits (its input buffers, never drops) but a continuously-keyed
    /// channel never holds a slot long enough to starve the others. In ungated
    /// mode (squelch off) there is no transmission structure, so it runs uncapped.
    fn apply_dfn(&mut self, sample: f32, open: bool, gated: bool, sem: &Semaphore) -> f32 {
        let Some(e) = self.dfn.as_mut() else {
            return sample;
        };
        if !open {
            e.reset();
            return sample;
        }
        if gated {
            e.process_sample_gated(sample, sem)
        } else {
            e.process_sample(sample)
        }
    }

    /// Post-DFN presence brightness lift on the open stream (reset while idle).
    fn apply_presence(&mut self, sample: f32, open: bool) -> f32 {
        let Some(p) = self.presence.as_mut() else {
            return sample;
        };
        if !open {
            p.reset();
            return sample;
        }
        p.process(sample)
    }

    /// Writes the processed PCM sample to the recorder per the split policy.
    fn record(&mut self, pcm: i16, gated: bool, just_opened: bool, just_closed: bool, cfg: &Config) -> Result<()> {
        let Some(rec) = self.recorder.as_mut() else {
            return Ok(());
        };
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
        // Periodically flush / refresh the WAV header (once per ~second).
        self.since_checkpoint += 1;
        if self.since_checkpoint >= cfg.rate as u64 {
            rec.checkpoint(cfg.rate)?;
            self.since_checkpoint = 0;
        }
        Ok(())
    }

    /// Processes one demultiplexed sample: squelch decision, audio chain, DFN,
    /// presence, then fan-out to all sinks.
    fn on_sample(
        &mut self,
        sample: i32,
        carrier: u8,
        carrier_thr: Option<f32>,
        cfg: &Config,
        sem: &Semaphore,
        metric: &metrics::ChannelMetric,
    ) -> Result<()> {
        // Voice-band magnitude drives both the VOX squelch and the speech-present
        // flag (excludes the conducted comb / rumble).
        let sq_mag = self.sq_filter.process(sample as f64).abs() as f32;
        // Carrier mode keys the squelch on the FPGA carrier level; all other
        // modes on voice-band modulation energy (VOX).
        let sq_level = match cfg.squelch_mode {
            SquelchMode::Carrier { .. } => decode_carrier(carrier),
            _ => sq_mag,
        };
        // In carrier mode, apply the latest cross-channel threshold (>0 once the
        // router has computed it; before that the squelch keeps its default).
        if let Some(thr) = carrier_thr {
            if thr > 0.0 {
                self.squelch.set_threshold(thr);
            }
        }
        self.squelch.process(sq_level);
        let gated = !matches!(cfg.squelch_mode, SquelchMode::Off);
        let open = !gated || self.squelch.is_open();
        let just_opened = gated && self.squelch.just_opened();
        let just_closed = gated && self.squelch.just_closed();
        let above = match self.audio_gate.as_mut() {
            Some(g) => {
                g.process(sq_mag);
                sq_mag >= g.threshold()
            }
            None => sq_mag >= self.squelch.threshold(),
        };

        // Publish squelch-derived gauges for stats/metrics (every channel).
        metric.set_squelch(
            self.squelch.open_count(),
            self.squelch.level(),
            self.squelch.noise_floor(),
            self.squelch.is_open(),
        );

        // Continuous raw-demod tap (pre-DSP, pre-squelch) for the debug monitor:
        // cheap fixed-gain scale, always streamed so it matches the waterfall
        // regardless of squelch. Computed only when a `pre` client is attached.
        if !self.pre_monitors.is_empty() {
            let pre = ((sample as f64 / SAMPLE_SCALE as f64) * 32767.0 * MONITOR_PRE_GAIN)
                .round()
                .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
            fanout_monitors(&mut self.pre_monitors, pre);
        }

        // Skip the (expensive) enhancement chain entirely when nothing needs the
        // post-DFN audio this sample.
        if !self.needs_post() {
            return Ok(());
        }

        // Chain: filters -> denoise -> AGC -> DFN -> presence brightness. UDP and
        // Icecast get a continuous stream (near-silence between transmissions);
        // the recorder gates it per the split policy.
        let mut n = self.audio_chain(sample, open, just_opened, above, cfg);
        n = self.apply_dfn(n, open, gated, sem);
        n = self.apply_presence(n, open);
        let pcm = if cfg.agc_on {
            (n as f64 * 32767.0).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
        } else {
            (n as f64 * SAMPLE_SCALE as f64 * 2f64.powi(-cfg.shift))
                .round()
                .clamp(i16::MIN as f64, i16::MAX as f64) as i16
        };

        if let Some(u) = self.udp.as_mut() {
            u.push(pcm);
        }
        for tx in &self.icecast {
            // Drop only under network back-pressure (slow/dead Icecast); the DFN
            // path itself never drops or bypasses.
            let _ = tx.try_send(pcm);
        }
        // Post-DFN debug-monitor tap (byte-identical to what feeds ship).
        fanout_monitors(&mut self.post_monitors, pcm);
        self.record(pcm, gated, just_opened, just_closed, cfg)
    }

    /// Finalizes streaming state on a stream reset (reconnect/shutdown).
    fn reset(&mut self, cfg: &Config) {
        if let Some(r) = self.recorder.as_mut() {
            let _ = r.close(cfg.rate, cfg.min_samples);
        }
        if let Some(e) = self.dfn.as_mut() {
            e.reset();
        }
        if let Some(p) = self.presence.as_mut() {
            p.reset();
        }
        self.since_checkpoint = 0;
    }

    /// Worker thread body: eagerly load the DFN model (so no transmission is
    /// missed to a cold model), sync at the startup barrier, then process samples
    /// until the channel sender is dropped.
    fn run(
        mut self,
        rx: Receiver<Msg>,
        cfg: Arc<Config>,
        sem: Arc<Semaphore>,
        metrics: Arc<Metrics>,
        carrier_thr: Arc<AtomicU32>,
        barrier: Arc<Barrier>,
    ) {
        if cfg.dfn_on && self.needs_audio() {
            match DfnEnhancer::new(cfg.rate as f64, cfg.dfn_params) {
                Ok(e) => self.dfn = Some(e),
                Err(err) => eprintln!(
                    "ch{:02}: DeepFilterNet init failed ({err:#}); streaming without it",
                    self.index
                ),
            }
        }
        barrier.wait();
        let carrier_mode = matches!(cfg.squelch_mode, SquelchMode::Carrier { .. });
        let metric = &metrics.channels[self.index];
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Sample(sample, carrier) => {
                    let thr = carrier_mode
                        .then(|| f32::from_bits(carrier_thr.load(Ordering::Relaxed)));
                    if let Err(e) = self.on_sample(sample, carrier, thr, &cfg, &sem, metric) {
                        eprintln!("ch{:02}: output error ({e:#})", self.index);
                    }
                }
                Msg::Reset => self.reset(&cfg),
                Msg::AddMonitor { tap, tx } => match tap {
                    Tap::Pre => self.pre_monitors.push(tx),
                    Tap::Post => self.post_monitors.push(tx),
                },
            }
        }
        if let Some(r) = self.recorder.as_mut() {
            let _ = r.close(cfg.rate, cfg.min_samples);
        }
    }
}

/// Periodic stats reporter, reading the shared `Metrics` atomics updated by the
/// router (samples/drops/peak) and workers (squelch gauges).
struct StatsPrinter {
    interval: Duration,
    last: Instant,
    prev_samples: Vec<u64>,
    mode: Mode,
}

impl StatsPrinter {
    fn new(interval_secs: u64, n: usize, mode: Mode) -> StatsPrinter {
        StatsPrinter {
            interval: Duration::from_secs(interval_secs.max(1)),
            last: Instant::now(),
            prev_samples: vec![0; n],
            mode,
        }
    }

    fn due(&self) -> bool {
        self.last.elapsed() >= self.interval
    }

    fn emit(&mut self, metrics: &Metrics) {
        let elapsed = self.last.elapsed().as_secs_f64();
        if self.mode == Mode::Stats {
            println!("---- airband {elapsed:.1}s ----");
            println!("  ch    sps   total      drops   peak(dBFS)  carrier(dB·c)  tx");
        }
        let mut active = 0;
        let mut total_drops = 0u64;
        let mut total_active_channels = 0;
        let mut total_tx = 0u64;
        for (i, m) in metrics.channels.iter().enumerate() {
            let total = m.samples.load(Ordering::Relaxed);
            let drops = m.drops.load(Ordering::Relaxed);
            let tx = m.transmissions.load(Ordering::Relaxed);
            total_drops += drops;
            total_tx += tx;
            let interval_samples = total.saturating_sub(self.prev_samples[i]);
            self.prev_samples[i] = total;
            if total > 0 {
                total_active_channels += 1;
            }
            if interval_samples > 0 {
                active += 1;
                if self.mode == Mode::Stats {
                    let sps = interval_samples as f64 / elapsed;
                    let peak_db = level_to_dbfs(m.take_peak());
                    let cdb = metrics.carrier_dbc(i);
                    println!(
                        "  {i:2}  {sps:6.0}  {total:9}  {drops:9}  {peak_db:9.1}  {cdb:11.1}  {tx:4}"
                    );
                }
            } else {
                let _ = m.take_peak();
            }
        }
        if self.mode == Mode::Stats {
            println!("  active channels: {active}, cumulative drops: {total_drops}");
        } else {
            eprintln!(
                "[{elapsed:.0}s] {total_active_channels} channels active, {total_tx} transmissions, cumulative drops {total_drops}"
            );
        }
        self.last = Instant::now();
    }
}

/// Router/demux thread: reads framed records from the socket, detects drops,
/// maintains the carrier-mode noise reference, and routes each sample to its
/// channel worker over an unbounded queue (never blocks the socket, never drops).
fn run_session(
    addr: &str,
    n: usize,
    cfg: &Config,
    senders: &[Sender<Msg>],
    metrics: &Metrics,
    carrier_thr: &AtomicU32,
    stats: &mut StatsPrinter,
) -> Result<()> {
    let stream = TcpStream::connect(addr).with_context(|| format!("connecting to {addr}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    eprintln!("connected to {addr}");
    metrics.set_stream_up(true);
    let mut reader = BufReader::with_capacity(1 << 16, stream);
    let mut frame = [0u8; FRAME_BYTES];

    let carrier_mode = matches!(cfg.squelch_mode, SquelchMode::Carrier { .. });
    let carrier_snr_ratio = match cfg.squelch_mode {
        SquelchMode::Carrier { snr_db } => 10f32.powf(snr_db / 20.0),
        _ => 1.0,
    };
    let mut last_carrier = vec![0f32; n];
    let mut last_seq = vec![Option::<u32>::None; n];
    let mut since_carrier_update: u64 = 0;
    let mut since_active: u64 = 0;

    loop {
        reader.read_exact(&mut frame)?;
        let (seq, chan, sample, carrier) = unpack(u64::from_le_bytes(frame));
        let ci = usize::from(chan);
        if ci >= n {
            continue;
        }

        // Drop detection from the per-channel sequence counter.
        if let Some(prev) = last_seq[ci] {
            let expected = (prev + 1) % SEQ_MOD;
            if seq != expected {
                metrics.channels[ci].add_drops(u64::from((seq + SEQ_MOD - expected) % SEQ_MOD));
            }
        }
        last_seq[ci] = Some(seq);
        metrics.channels[ci].note_sample(sample.unsigned_abs() as f32);
        // Refresh the "data flowing" timestamp periodically (cheap; the stream
        // carries ~360k samples/s so a coarse update is ample resolution).
        since_active += 1;
        if since_active >= 256 {
            metrics.note_active();
            since_active = 0;
        }

        // Always track the per-channel FPGA carrier and periodically recompute the
        // cross-channel noise reference, so the stats meter can show carrier-over-
        // noise (dB·c) — the same signal-presence metric airband-listen displays —
        // regardless of squelch mode and independent of the demod audio level. In
        // carrier-squelch mode the same reference (× SNR) also sets the threshold.
        last_carrier[ci] = decode_carrier(carrier);
        since_carrier_update += 1;
        if since_carrier_update >= CARRIER_UPDATE_FRAMES {
            let noise = carrier_noise_threshold(&last_carrier, CARRIER_NOISE_PCT, 1.0);
            metrics.carrier_noise.store(noise.to_bits(), Ordering::Relaxed);
            for (cm, &c) in metrics.channels.iter().zip(last_carrier.iter()) {
                cm.set_carrier(c);
            }
            if carrier_mode {
                carrier_thr.store((noise * carrier_snr_ratio).to_bits(), Ordering::Relaxed);
            }
            since_carrier_update = 0;
        }

        // Unbounded, non-blocking handoff: never stall the socket, never drop.
        let _ = senders[ci].send(Msg::Sample(sample, carrier));

        if stats.due() {
            stats.emit(metrics);
            // Pet the systemd watchdog from the live read loop, so a hung-but-
            // alive reader (no data progressing) is restarted (no-op off-systemd).
            sd_notify("WATCHDOG=1");
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
    let cfg = Arc::new(Config {
        mode: args.mode,
        out_dir: args.out_dir.clone(),
        rate: args.rate,
        shift: args.shift,
        squelch_mode,
        squelch_hang_ms: args.squelch_hang_ms,
        split: !args.no_split,
        min_samples: (args.min_transmission_ms * args.rate as u64) / 1000,
        agc_on: !args.no_agc,
        filter_on: args.filter,
        denoise_on: args.denoise,
        denoise_floor_db: args.denoise_floor_db,
        notch_freq: args.notch,
        notch_q: args.notch_q,
        low: args.filter_low,
        high: args.filter_high,
        lpf_hz: args.lpf_hz,
        dfn_on: !args.no_dfn,
        dfn_params: DfnParams {
            min_snr_db: args.dfn_min_snr,
            atten_lim_db: args.dfn_atten_lim,
            pf_beta: args.dfn_pf_beta,
        },
        dfn_max_active: args.dfn_max_active,
        presence_db: args.presence_db,
        presence_hz: args.presence_hz,
        presence_q: args.presence_q,
    });

    if cfg.mode != Mode::Stats {
        fs::create_dir_all(&cfg.out_dir)
            .with_context(|| format!("creating output dir {:?}", cfg.out_dir))?;
    }

    let n = args.channels;

    // Per-channel output sinks, assembled before the workers are built so each
    // worker owns its own recorder/UDP/Icecast senders (a channel may fan out to
    // several Icecast mounts).
    let mut udp_sinks: Vec<Option<UdpOut>> = (0..n).map(|_| None).collect();
    if let Some(ch) = args.udp_channel {
        let dest = args
            .udp_dest
            .as_ref()
            .context("--udp-channel requires --udp-dest host:port")?;
        anyhow::ensure!(ch < n, "--udp-channel {ch} out of range");
        udp_sinks[ch] = Some(UdpOut::new(dest)?);
        eprintln!("udp: streaming channel {ch} PCM to {dest}");
    }

    // Icecast feeds: a JSON feeds file (many channels / many servers) and/or the
    // single-stream --icecast-* flags. Both produce IcecastConfig entries that
    // are attached to their channel (a channel may have several = fan-out).
    let mut icecast_sinks: Vec<Vec<SyncSender<i16>>> = (0..n).map(|_| Vec::new()).collect();
    let mut feed_cfgs: Vec<IcecastConfig> = Vec::new();
    if let Some(path) = args.feeds.as_ref() {
        feed_cfgs.extend(feeds::load(path, cfg.rate, n)?);
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
    // One health cell per feed (same order as the configs), shared with both the
    // metrics bus and each feed's streaming thread.
    let feed_metrics: Vec<Arc<FeedMetric>> = feed_cfgs
        .iter()
        .map(|c| FeedMetric::new(c.channel, c.mount.clone()))
        .collect();

    // Shared cross-thread state: stats/metrics bus (incl. per-feed health), DFN
    // concurrency semaphore, and the carrier-mode noise threshold.
    let metrics = Metrics::new(n, feed_metrics.clone());

    for (icfg, fm) in feed_cfgs.into_iter().zip(feed_metrics) {
        let ch = icfg.channel;
        anyhow::ensure!(ch < n, "icecast feed channel {ch} out of range");
        let scheme = if icfg.tls == TlsMode::Disabled { "icecast" } else { "icecast+tls" };
        eprintln!(
            "icecast: feed ch{ch} -> {scheme}://{}:{}{}",
            icfg.host, icfg.port, icfg.mount
        );
        icecast_sinks[ch].push(icecast::spawn(icfg, fm));
    }

    if args.metrics_port > 0 {
        metrics::serve(Arc::clone(&metrics), args.metrics_port);
    }

    // "Is the Pluto connected?" — periodic lightweight TCP probe of the maia-httpd
    // web port (NOT a second :30000 connection; that port is single-client). The
    // stream-up gauge answers "is the application serving?" and the data-flow
    // gauge answers "is data flowing?".
    spawn_pluto_probe(Arc::clone(&metrics), &args.addr, args.pluto_web_port);

    // MQTT -> Home Assistant (optional). Reuses the metrics snapshot. An empty
    // broker (e.g. an unset `${AIRBAND_MQTT_BROKER}` expanded by systemd) means
    // "disabled", so the env-var ExecStart pattern is safe when MQTT is unused.
    if let Some(broker) = args.mqtt_broker.clone().filter(|b| !b.is_empty()) {
        eprintln!("mqtt: publishing to {broker}:{} (prefix {})", args.mqtt_port, args.mqtt_prefix);
        mqtt::spawn(
            Arc::clone(&metrics),
            mqtt::MqttConfig {
                broker,
                port: args.mqtt_port,
                user: args.mqtt_user.clone().filter(|s| !s.is_empty()),
                pass: args.mqtt_pass.clone().filter(|s| !s.is_empty()),
                prefix: args.mqtt_prefix.clone(),
                discovery_prefix: args.mqtt_discovery_prefix.clone(),
                interval: Duration::from_secs(args.mqtt_interval.max(1)),
                per_channel: args.mqtt_per_channel,
                n_channels: n,
            },
        );
    }
    let sem = Arc::new(Semaphore::new(cfg.dfn_max_active));
    let carrier_thr = Arc::new(AtomicU32::new(0));

    // Spawn one worker thread per channel. Each builds its DeepFilterNet model
    // up front and waits on the barrier, so the router only starts reading the
    // stream once every channel is ready (no transmission missed to a cold model).
    let barrier = Arc::new(Barrier::new(n + 1));
    let mut senders: Vec<Sender<Msg>> = Vec::with_capacity(n);
    let mut udp_iter = udp_sinks.into_iter();
    let mut ice_iter = icecast_sinks.into_iter();
    for c in 0..n {
        let (tx, rx) = mpsc::channel::<Msg>();
        senders.push(tx);
        let udp = udp_iter.next().unwrap();
        let ice = ice_iter.next().unwrap();
        let cfg_c = Arc::clone(&cfg);
        let sem_c = Arc::clone(&sem);
        let metrics_c = Arc::clone(&metrics);
        let thr_c = Arc::clone(&carrier_thr);
        let bar_c = Arc::clone(&barrier);
        // Build the worker (and its non-Send DeepFilterNet model) inside its own
        // thread so nothing DFN-related ever crosses a thread boundary.
        thread::spawn(move || {
            let worker = Worker::new(c, &cfg_c, udp, ice);
            worker.run(rx, cfg_c, sem_c, metrics_c, thr_c, bar_c);
        });
    }

    eprintln!(
        "airband-reader: {} channels, audio {} sps, mode {:?}, squelch {:?}, split {}, agc {}, dfn {} (max-active {})",
        n, cfg.rate, cfg.mode, args.squelch, cfg.split, cfg.agc_on, cfg.dfn_on, args.dfn_max_active
    );
    if cfg.dfn_on {
        eprintln!("loading DeepFilterNet models for output channels (eager startup)...");
    }
    barrier.wait();
    eprintln!("ready");

    // Low-latency debug monitor: registers runtime sinks into the workers via
    // their existing senders (clone so the router keeps its own borrow).
    if args.monitor_port > 0 {
        monitor::serve(args.monitor_port, senders.clone(), cfg.rate);
    }

    // Signal readiness for systemd Type=notify (no-op otherwise).
    sd_notify("READY=1");

    let mut stats = StatsPrinter::new(args.stats_interval, n, cfg.mode);
    loop {
        if let Err(e) =
            run_session(&args.addr, n, &cfg, &senders, &metrics, &carrier_thr, &mut stats)
        {
            metrics.set_stream_up(false);
            eprintln!("stream error ({e:#}); reconnecting in 1s");
            // Reconnect: tell workers to finalize recordings and clear state.
            for s in &senders {
                let _ = s.send(Msg::Reset);
            }
            // Keep the watchdog satisfied across a Pluto/network outage so a
            // reconnecting (but healthy) reader is not killed by WatchdogSec.
            sd_notify("WATCHDOG=1");
            thread::sleep(Duration::from_secs(1));
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
