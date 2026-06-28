//! Minimal metrics + health endpoint (no external HTTP dependency).
//!
//! Exposes per-channel counters/gauges, pipeline-health signals, and per-feed
//! status in the Prometheus text exposition format on a plain TCP port (modeled
//! on the stats RTLSDR-Airband publishes), plus two extra routes on the same
//! port: `/healthz` (a 200/503 liveness probe) and `/status` (the curated JSON
//! snapshot, which the MQTT publisher reuses verbatim). A background thread
//! answers each connection with the current snapshot; the processing loop
//! refreshes the snapshot once per stats interval (no per-sample cost).

use airband_dsp::level_to_dbfs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

/// How stale the last received sample may be before `data_flowing` reads false.
/// The Pluto streams continuously, so a multi-second gap means the stream
/// stalled even if the TCP socket is still nominally connected.
const DATA_FLOW_TIMEOUT_S: f64 = 5.0;

/// Per-channel metric cells. The router updates the sample/drop/peak counters
/// (it sees every record); each channel worker updates the squelch-derived
/// gauges. All cells are atomic so they double as the cross-thread stats bus
/// regardless of whether the metrics endpoint is enabled.
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

/// Per-feed (Icecast/LiveATC destination) health cells, updated by the feed's
/// own streaming thread (see [`crate::icecast`]). `connected` reflects whether
/// the source is currently established and writable; `bytes` is the MP3 bytes
/// actually shipped to that mount.
pub struct FeedMetric {
    pub channel: usize,
    pub mount: String,
    pub connected: AtomicBool,
    pub reconnects: AtomicU64,
    pub bytes: AtomicU64,
}

impl FeedMetric {
    pub fn new(channel: usize, mount: String) -> Arc<FeedMetric> {
        Arc::new(FeedMetric {
            channel,
            mount,
            connected: AtomicBool::new(false),
            reconnects: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        })
    }

    pub fn set_connected(&self, up: bool) {
        self.connected.store(up, Ordering::Relaxed);
    }
    pub fn note_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }
}

/// All exported metrics.
pub struct Metrics {
    pub channels: Vec<ChannelMetric>,
    pub feeds: Vec<Arc<FeedMetric>>,
    /// Cross-channel carrier noise reference (median, raw units) as f32 bits.
    /// Each channel's carrier is metered in dB over this, so an idle channel sits
    /// ~0 dB and a keyed station reads positive — matches `airband-listen`.
    pub carrier_noise: AtomicU32,
    /// The Pluto's web port (:8000) answered the last reachability probe.
    pub pluto_reachable: AtomicBool,
    /// The framed-audio stream (:30000) is currently connected (maia-httpd's
    /// airband task is serving). Set by the router on connect, cleared on error.
    pub stream_up: AtomicBool,
    start: Instant,
    /// `start.elapsed()` in ms at the last received sample.
    last_sample_ms: AtomicU64,
}

impl Metrics {
    pub fn new(channels: usize, feeds: Vec<Arc<FeedMetric>>) -> Arc<Metrics> {
        Arc::new(Metrics {
            channels: (0..channels).map(|_| ChannelMetric::new()).collect(),
            feeds,
            carrier_noise: AtomicU32::new(0),
            pluto_reachable: AtomicBool::new(false),
            stream_up: AtomicBool::new(false),
            start: Instant::now(),
            last_sample_ms: AtomicU64::new(0),
        })
    }

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Router: mark that audio is currently arriving (drives `data_flowing`).
    pub fn note_active(&self) {
        self.last_sample_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    pub fn set_pluto_reachable(&self, up: bool) {
        self.pluto_reachable.store(up, Ordering::Relaxed);
    }
    pub fn set_stream_up(&self, up: bool) {
        self.stream_up.store(up, Ordering::Relaxed);
    }

    pub fn uptime_secs(&self) -> u64 {
        self.start.elapsed().as_secs()
    }

    /// Seconds since the last received sample (large before the first sample).
    pub fn seconds_since_last_sample(&self) -> f64 {
        let last = self.last_sample_ms.load(Ordering::Relaxed);
        (self.now_ms().saturating_sub(last)) as f64 / 1000.0
    }

    /// "Is data flowing?" — stream connected AND a sample seen recently.
    pub fn data_flowing(&self) -> bool {
        self.stream_up.load(Ordering::Relaxed)
            && self.seconds_since_last_sample() < DATA_FLOW_TIMEOUT_S
    }

    /// Headline tile: the capture side (Pluto -> Pi) is healthy.
    pub fn system_healthy(&self) -> bool {
        self.pluto_reachable.load(Ordering::Relaxed)
            && self.stream_up.load(Ordering::Relaxed)
            && self.data_flowing()
    }

    /// Headline tile: every output feed is connected (what we ship to LiveATC).
    /// Vacuously true when no feeds are configured.
    pub fn liveatc_healthy(&self) -> bool {
        self.feeds
            .iter()
            .all(|f| f.connected.load(Ordering::Relaxed))
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

    /// The curated JSON snapshot served on `/status` and republished over MQTT.
    /// One source of truth so the HA dashboard and the `/status` debug view never
    /// diverge.
    pub fn status_json(&self) -> String {
        let mut active = 0u64;
        let mut total_drops = 0u64;
        let mut total_tx = 0u64;
        for m in &self.channels {
            if m.samples.load(Ordering::Relaxed) > 0 {
                active += 1;
            }
            total_drops += m.drops.load(Ordering::Relaxed);
            total_tx += m.transmissions.load(Ordering::Relaxed);
        }
        let pluto_reachable = self.pluto_reachable.load(Ordering::Relaxed);
        let stream_up = self.stream_up.load(Ordering::Relaxed);

        let mut feeds = String::from("[");
        for (i, f) in self.feeds.iter().enumerate() {
            if i > 0 {
                feeds.push(',');
            }
            feeds.push_str(&format!(
                "{{\"channel\":{},\"mount\":{:?},\"connected\":{},\"reconnects\":{},\"bytes\":{}}}",
                f.channel,
                f.mount,
                f.connected.load(Ordering::Relaxed),
                f.reconnects.load(Ordering::Relaxed),
                f.bytes.load(Ordering::Relaxed),
            ));
        }
        feeds.push(']');

        let mut channels = String::from("[");
        for (i, m) in self.channels.iter().enumerate() {
            if i > 0 {
                channels.push(',');
            }
            let lvl = f32::from_bits(m.level_bits.load(Ordering::Relaxed));
            channels.push_str(&format!(
                "{{\"ch\":{},\"open\":{},\"carrier_dbc\":{:.1},\"drops\":{},\"tx\":{},\"level_dbfs\":{:.1}}}",
                i,
                m.open.load(Ordering::Relaxed),
                self.carrier_dbc(i),
                m.drops.load(Ordering::Relaxed),
                m.transmissions.load(Ordering::Relaxed),
                level_to_dbfs(lvl),
            ));
        }
        channels.push(']');

        format!(
            "{{\
\"pluto_reachable\":{pluto_reachable},\
\"maia_httpd_up\":{stream_up},\
\"stream_up\":{stream_up},\
\"data_flowing\":{data},\
\"system_healthy\":{sys},\
\"liveatc_healthy\":{live},\
\"seconds_since_last_sample\":{since:.1},\
\"uptime_secs\":{up},\
\"active_channels\":{active},\
\"total_drops\":{total_drops},\
\"total_transmissions\":{total_tx},\
\"feeds\":{feeds},\
\"channels\":{channels}}}",
            data = self.data_flowing(),
            sys = self.system_healthy(),
            live = self.liveatc_healthy(),
            since = self.seconds_since_last_sample(),
            up = self.uptime_secs(),
        )
    }

    /// Renders the Prometheus text exposition for the current snapshot.
    pub fn render(&self) -> String {
        let mut s = String::with_capacity(4096);

        // Pipeline-health gauges (the three questions + the two headline tiles).
        let b = |x: bool| if x { 1 } else { 0 };
        s.push_str("# HELP airband_pluto_reachable Pluto web port (:8000) answered the last probe.\n");
        s.push_str("# TYPE airband_pluto_reachable gauge\n");
        s.push_str(&format!("airband_pluto_reachable {}\n", b(self.pluto_reachable.load(Ordering::Relaxed))));
        s.push_str("# HELP airband_link_up Framed-audio stream (:30000) is connected.\n");
        s.push_str("# TYPE airband_link_up gauge\n");
        s.push_str(&format!("airband_link_up {}\n", b(self.stream_up.load(Ordering::Relaxed))));
        s.push_str("# HELP airband_data_flowing Samples are arriving from the Pluto.\n");
        s.push_str("# TYPE airband_data_flowing gauge\n");
        s.push_str(&format!("airband_data_flowing {}\n", b(self.data_flowing())));
        s.push_str("# HELP airband_seconds_since_last_sample Age of the most recent sample.\n");
        s.push_str("# TYPE airband_seconds_since_last_sample gauge\n");
        s.push_str(&format!("airband_seconds_since_last_sample {:.1}\n", self.seconds_since_last_sample()));
        s.push_str("# HELP airband_uptime_seconds Reader process uptime.\n");
        s.push_str("# TYPE airband_uptime_seconds gauge\n");
        s.push_str(&format!("airband_uptime_seconds {}\n", self.uptime_secs()));
        s.push_str("# HELP airband_system_healthy Capture side (Pluto->Pi) healthy.\n");
        s.push_str("# TYPE airband_system_healthy gauge\n");
        s.push_str(&format!("airband_system_healthy {}\n", b(self.system_healthy())));
        s.push_str("# HELP airband_liveatc_healthy All output feeds connected.\n");
        s.push_str("# TYPE airband_liveatc_healthy gauge\n");
        s.push_str(&format!("airband_liveatc_healthy {}\n", b(self.liveatc_healthy())));

        // Per-feed health.
        s.push_str("# HELP airband_feed_connected Feed source connection is established.\n");
        s.push_str("# TYPE airband_feed_connected gauge\n");
        for f in &self.feeds {
            s.push_str(&format!(
                "airband_feed_connected{{mount=\"{}\",channel=\"{}\"}} {}\n",
                f.mount, f.channel, b(f.connected.load(Ordering::Relaxed))
            ));
        }
        s.push_str("# HELP airband_feed_reconnects_total Feed reconnect attempts.\n");
        s.push_str("# TYPE airband_feed_reconnects_total counter\n");
        for f in &self.feeds {
            s.push_str(&format!(
                "airband_feed_reconnects_total{{mount=\"{}\",channel=\"{}\"}} {}\n",
                f.mount, f.channel, f.reconnects.load(Ordering::Relaxed)
            ));
        }
        s.push_str("# HELP airband_feed_bytes_total MP3 bytes shipped to the feed.\n");
        s.push_str("# TYPE airband_feed_bytes_total counter\n");
        for f in &self.feeds {
            s.push_str(&format!(
                "airband_feed_bytes_total{{mount=\"{}\",channel=\"{}\"}} {}\n",
                f.mount, f.channel, f.bytes.load(Ordering::Relaxed)
            ));
        }

        // Per-channel counters/gauges (unchanged from the original exposition).
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

/// Spawns a background thread serving `/metrics`, `/healthz`, and `/status` on `port`.
pub fn serve(metrics: Arc<Metrics>, port: u16) {
    thread::spawn(move || {
        let listener = match TcpListener::bind(("0.0.0.0", port)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("metrics: failed to bind port {port}: {e}");
                return;
            }
        };
        eprintln!("metrics: serving /metrics /healthz /status on :{port}");
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            // Read the request line to route by path; one short read is enough.
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");

            let (status, ctype, body) = if path.starts_with("/healthz") {
                let ok = metrics.system_healthy() && metrics.liveatc_healthy();
                let code = if ok { "200 OK" } else { "503 Service Unavailable" };
                (code, "text/plain", if ok { "ok\n".to_string() } else { "unhealthy\n".to_string() })
            } else if path.starts_with("/status") {
                ("200 OK", "application/json", metrics.status_json())
            } else {
                ("200 OK", "text/plain; version=0.0.4", metrics.render())
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
}
