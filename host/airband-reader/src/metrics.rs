//! Minimal Prometheus metrics endpoint (no external HTTP dependency).
//!
//! Exposes per-channel counters and gauges in the text exposition format on a
//! plain TCP port, modeled on the stats RTLSDR-Airband publishes. A background
//! thread answers each connection with the current snapshot; the processing
//! loop refreshes the snapshot once per stats interval (no per-sample cost).

use airband_dsp::level_to_dbfs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

/// Per-channel metric cells. The router updates the sample/drop/peak counters
/// (it sees every record); each channel worker updates the squelch-derived
/// gauges. All cells are atomic so they double as the cross-thread stats bus
/// regardless of whether the Prometheus endpoint is enabled.
pub struct ChannelMetric {
    pub samples: AtomicU64,
    pub drops: AtomicU64,
    pub transmissions: AtomicU64,
    /// Smoothed audio level (raw 24-bit units) as f32 bits.
    pub level_bits: AtomicU32,
    /// Noise floor (raw 24-bit units) as f32 bits.
    pub floor_bits: AtomicU32,
    /// Peak audio magnitude since the last stats interval (raw units) as f32 bits.
    pub peak_bits: AtomicU32,
    /// Latest decoded FPGA carrier level (raw units) as f32 bits — the meter
    /// metric, like `airband-listen`. Independent of the demod audio level.
    pub carrier_bits: AtomicU32,
    pub open: AtomicBool,
}

impl ChannelMetric {
    fn new() -> ChannelMetric {
        ChannelMetric {
            samples: AtomicU64::new(0),
            drops: AtomicU64::new(0),
            transmissions: AtomicU64::new(0),
            level_bits: AtomicU32::new(0),
            floor_bits: AtomicU32::new(0),
            peak_bits: AtomicU32::new(0),
            carrier_bits: AtomicU32::new(0),
            open: AtomicBool::new(false),
        }
    }

    /// Router: publish this channel's latest decoded FPGA carrier level (raw units).
    pub fn set_carrier(&self, carrier: f32) {
        self.carrier_bits.store(carrier.to_bits(), Ordering::Relaxed);
    }

    /// Router: count one received sample and track the interval peak magnitude.
    /// `mag` is a non-negative raw amplitude, so its bit pattern is monotonic and
    /// can be max-reduced directly.
    pub fn note_sample(&self, mag: f32) {
        self.samples.fetch_add(1, Ordering::Relaxed);
        let bits = mag.to_bits();
        let mut cur = self.peak_bits.load(Ordering::Relaxed);
        while bits > cur {
            match self.peak_bits.compare_exchange_weak(
                cur,
                bits,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(c) => cur = c,
            }
        }
    }

    /// Router: record `n` dropped samples detected from the sequence counter.
    pub fn add_drops(&self, n: u64) {
        self.drops.fetch_add(n, Ordering::Relaxed);
    }

    /// Worker: publish the channel's current squelch-derived gauges.
    pub fn set_squelch(&self, transmissions: u64, level: f32, floor: f32, open: bool) {
        self.transmissions.store(transmissions, Ordering::Relaxed);
        self.level_bits.store(level.to_bits(), Ordering::Relaxed);
        self.floor_bits.store(floor.to_bits(), Ordering::Relaxed);
        self.open.store(open, Ordering::Relaxed);
    }

    /// Stats printer: read and reset the interval peak magnitude (raw units).
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.swap(0, Ordering::Relaxed))
    }
}

/// All exported metrics.
pub struct Metrics {
    pub channels: Vec<ChannelMetric>,
    /// Cross-channel carrier noise reference (median, raw units) as f32 bits.
    /// Each channel's carrier is metered in dB over this, so an idle channel sits
    /// ~0 dB and a keyed station reads positive — matches `airband-listen`.
    pub carrier_noise: AtomicU32,
}

impl Metrics {
    pub fn new(channels: usize) -> Arc<Metrics> {
        Arc::new(Metrics {
            channels: (0..channels).map(|_| ChannelMetric::new()).collect(),
            carrier_noise: AtomicU32::new(0),
        })
    }

    /// Each channel's carrier in dB over the cross-channel noise reference
    /// (`dB·c`), the same signal-presence metric `airband-listen` shows. Both
    /// terms are clamped to ≥1 raw unit so an all-silent/old bitstream reads 0 dB.
    pub fn carrier_dbc(&self, channel: usize) -> f32 {
        let noise_ref = f32::from_bits(self.carrier_noise.load(Ordering::Relaxed)).max(1.0);
        let carrier = f32::from_bits(self.channels[channel].carrier_bits.load(Ordering::Relaxed))
            .max(1.0);
        20.0 * (carrier / noise_ref).log10()
    }

    /// Renders the Prometheus text exposition for the current snapshot.
    pub fn render(&self) -> String {
        let mut s = String::with_capacity(2048);
        s.push_str("# HELP airband_samples_total Audio samples received per channel.\n");
        s.push_str("# TYPE airband_samples_total counter\n");
        for (c, m) in self.channels.iter().enumerate() {
            let v = m.samples.load(Ordering::Relaxed);
            s.push_str(&format!("airband_samples_total{{channel=\"{c}\"}} {v}\n"));
        }
        s.push_str("# HELP airband_drops_total Dropped samples per channel.\n");
        s.push_str("# TYPE airband_drops_total counter\n");
        for (c, m) in self.channels.iter().enumerate() {
            let v = m.drops.load(Ordering::Relaxed);
            s.push_str(&format!("airband_drops_total{{channel=\"{c}\"}} {v}\n"));
        }
        s.push_str("# HELP airband_transmissions_total Squelch openings per channel.\n");
        s.push_str("# TYPE airband_transmissions_total counter\n");
        for (c, m) in self.channels.iter().enumerate() {
            let v = m.transmissions.load(Ordering::Relaxed);
            s.push_str(&format!("airband_transmissions_total{{channel=\"{c}\"}} {v}\n"));
        }
        s.push_str("# HELP airband_level_dbfs Smoothed audio level per channel (dBFS).\n");
        s.push_str("# TYPE airband_level_dbfs gauge\n");
        for (c, m) in self.channels.iter().enumerate() {
            let lvl = f32::from_bits(m.level_bits.load(Ordering::Relaxed));
            s.push_str(&format!("airband_level_dbfs{{channel=\"{c}\"}} {:.1}\n", level_to_dbfs(lvl)));
        }
        s.push_str("# HELP airband_noise_floor_dbfs Tracked noise floor per channel (dBFS).\n");
        s.push_str("# TYPE airband_noise_floor_dbfs gauge\n");
        for (c, m) in self.channels.iter().enumerate() {
            let f = f32::from_bits(m.floor_bits.load(Ordering::Relaxed));
            s.push_str(&format!("airband_noise_floor_dbfs{{channel=\"{c}\"}} {:.1}\n", level_to_dbfs(f)));
        }
        s.push_str("# HELP airband_carrier_dbc FPGA carrier level over the cross-channel noise reference (dB).\n");
        s.push_str("# TYPE airband_carrier_dbc gauge\n");
        for (c, _m) in self.channels.iter().enumerate() {
            s.push_str(&format!("airband_carrier_dbc{{channel=\"{c}\"}} {:.1}\n", self.carrier_dbc(c)));
        }
        s.push_str("# HELP airband_squelch_open Whether the channel squelch is currently open.\n");
        s.push_str("# TYPE airband_squelch_open gauge\n");
        for (c, m) in self.channels.iter().enumerate() {
            let v = m.open.load(Ordering::Relaxed) as u8;
            s.push_str(&format!("airband_squelch_open{{channel=\"{c}\"}} {v}\n"));
        }
        s
    }
}

/// Spawns a background thread serving `GET /metrics` (any path) on `port`.
pub fn serve(metrics: Arc<Metrics>, port: u16) {
    thread::spawn(move || {
        let listener = match TcpListener::bind(("0.0.0.0", port)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("metrics: failed to bind port {port}: {e}");
                return;
            }
        };
        eprintln!("metrics: serving Prometheus exposition on :{port}/metrics");
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Drain the request line(s); we serve the same body for any path.
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = metrics.render();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
}
