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

/// Per-channel metric cells, updated from the processing loop.
pub struct ChannelMetric {
    pub samples: AtomicU64,
    pub drops: AtomicU64,
    pub transmissions: AtomicU64,
    /// Smoothed audio level (raw 24-bit units) as f32 bits.
    pub level_bits: AtomicU32,
    /// Noise floor (raw 24-bit units) as f32 bits.
    pub floor_bits: AtomicU32,
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
            open: AtomicBool::new(false),
        }
    }

    /// Stores a snapshot of a channel's current state.
    pub fn set(&self, samples: u64, drops: u64, transmissions: u64, level: f32, floor: f32, open: bool) {
        self.samples.store(samples, Ordering::Relaxed);
        self.drops.store(drops, Ordering::Relaxed);
        self.transmissions.store(transmissions, Ordering::Relaxed);
        self.level_bits.store(level.to_bits(), Ordering::Relaxed);
        self.floor_bits.store(floor.to_bits(), Ordering::Relaxed);
        self.open.store(open, Ordering::Relaxed);
    }
}

/// All exported metrics.
pub struct Metrics {
    pub channels: Vec<ChannelMetric>,
}

impl Metrics {
    pub fn new(channels: usize) -> Arc<Metrics> {
        Arc::new(Metrics {
            channels: (0..channels).map(|_| ChannelMetric::new()).collect(),
        })
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
