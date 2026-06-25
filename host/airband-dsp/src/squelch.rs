//! Per-channel squelch, ported from RTLSDR-Airband's `Squelch` (`src/squelch.cpp`).
//!
//! Differences from upstream, all due to this project's architecture:
//!
//! * Upstream squelches on the AM *carrier* magnitude before demodulation. Here the
//!   FPGA already demodulates and DC-blocks the audio (`maia-hdl` `am_audio.py`), so
//!   we squelch on the magnitude of the AC audio sample. Inter-transmission receiver
//!   noise is low and keyed voice is high, which is exactly what the user wants to
//!   gate, so the noise-floor/SNR machinery still applies.
//! * Levels are kept in raw 24-bit sample units (`|sample|`, up to 2**23), matching
//!   the domain RTLSDR-Airband works in, so the same EWMA constants and the `1e-6`
//!   noise-floor epsilon carry over unchanged.
//!
//! The state machine (CLOSED -> OPENING -> OPEN -> CLOSING -> CLOSED, plus a fast
//! LOW_SIGNAL_ABORT path) and the moving-average / noise-floor updates mirror
//! upstream; delays are specified in milliseconds and converted with the audio rate.

use crate::{dbfs_to_level, SAMPLE_SCALE};

/// EWMA weight for the running audio level (upstream `pre_filter` decay 0.99).
const LEVEL_DECAY: f32 = 0.99;
/// EWMA weight for the noise floor (upstream `noise_floor` decay 0.97).
const NOISE_DECAY: f32 = 0.97;
/// Noise floor is re-estimated every N samples while the squelch is shut.
const NOISE_UPDATE_INTERVAL: u64 = 16;
/// Small additive term so the noise floor can never collapse to exactly zero.
const NOISE_EPSILON: f32 = 1e-6;

/// How the open/close threshold is chosen.
#[derive(Clone, Copy, Debug)]
pub enum SquelchMode {
    /// Squelch disabled; audio always passes.
    Off,
    /// Automatic: threshold is `10^(snr_db/20)` above the tracked noise floor.
    AutoSnr { snr_db: f32 },
    /// Manual: fixed threshold at the given dBFS level.
    ManualDbfs(f32),
    /// Carrier-power: identical SNR machinery to [`SquelchMode::AutoSnr`], but the
    /// caller feeds the per-channel AM **carrier** level (from the FPGA's DC-block
    /// estimate) instead of the audio magnitude. The carrier is steady through
    /// speech pauses, so this never chatters and needs no hang time. Requires a
    /// bitstream that ships the carrier byte (see `hdl/audio_framer.py`).
    Carrier { snr_db: f32 },
}

/// Squelch configuration.
#[derive(Clone, Copy, Debug)]
pub struct SquelchConfig {
    pub mode: SquelchMode,
    /// Audio sample rate (Hz), used to convert the delay constants to samples.
    pub rate: u32,
    /// Time the level must stay above threshold before OPEN (ms).
    pub open_delay_ms: f32,
    /// Hang time: how long the level may stay below threshold before CLOSED (ms).
    ///
    /// This is what bridges intra-transmission gaps. RTLSDR-Airband can use a tiny
    /// value (~25 ms) because it squelches on the AM *carrier*, which persists
    /// through speech pauses. Here the FPGA already DC-blocks the audio, so we only
    /// see the modulation and must hang long enough to ride over word/phrase gaps
    /// (otherwise the squelch chatters on continuous speech such as AWOS/ATIS).
    pub close_delay_ms: f32,
    /// Consecutive below-threshold samples that force a fast close (ms).
    ///
    /// Modeled on a carrier-loss detector. With DC-blocked audio there is no
    /// carrier to lose, so a normal speech gap would trip it and reintroduce
    /// chatter; **disabled by default** (`0.0`). Set > 0 only with a real carrier.
    pub low_signal_abort_ms: f32,
}

impl SquelchConfig {
    /// Defaults tuned for DC-blocked audio: 1 s hang, carrier-loss abort off.
    pub fn new(mode: SquelchMode, rate: u32) -> SquelchConfig {
        SquelchConfig {
            mode,
            rate,
            open_delay_ms: 5.0,
            close_delay_ms: 1000.0,
            low_signal_abort_ms: 0.0,
        }
    }

    /// Sets the hang time (close delay) in milliseconds (builder style).
    pub fn with_hang_ms(mut self, ms: f32) -> SquelchConfig {
        self.close_delay_ms = ms;
        self
    }
}

/// Squelch state, mirroring RTLSDR-Airband's `enum class State`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SquelchState {
    /// Muted; tracking the noise floor.
    Closed,
    /// Above threshold, waiting out `open_delay` before audio passes.
    Opening,
    /// Audio passing.
    Open,
    /// Below threshold, waiting out `close_delay` (audio still passes).
    Closing,
    /// Sharp signal loss; muted immediately, waiting out `close_delay`.
    LowSignalAbort,
}

/// Per-channel squelch.
pub struct Squelch {
    enabled: bool,
    /// Fixed threshold in raw units for manual mode; `None` for automatic.
    manual_level: Option<f32>,
    /// Linear SNR ratio (`10^(snr_db/20)`) for automatic mode.
    snr_ratio: f32,

    noise_floor: f32,
    level: f32,

    state: SquelchState,
    delay: u32,
    open_delay: u32,
    close_delay: u32,
    abort_enabled: bool,
    low_signal_abort: u32,
    low_signal_count: u32,

    sample_count: u64,
    open_count: u64,
    just_opened: bool,
    just_closed: bool,
}

impl Squelch {
    pub fn new(cfg: SquelchConfig) -> Squelch {
        let (enabled, manual_level, snr_ratio) = match cfg.mode {
            SquelchMode::Off => (false, None, 1.0),
            SquelchMode::AutoSnr { snr_db } => (true, None, 10f32.powf(snr_db / 20.0)),
            SquelchMode::ManualDbfs(dbfs) => (true, Some(dbfs_to_level(dbfs)), 1.0),
            // Carrier mode is driven by a *fixed* threshold derived from a
            // cross-channel noise estimate and pushed in via `set_threshold`. It
            // starts effectively closed (huge threshold) until the first update,
            // because an adaptive per-channel floor can't hold a continuous
            // carrier (e.g. AWOS) open.
            SquelchMode::Carrier { .. } => (true, Some(f32::MAX), 1.0),
        };
        let ms_to_samples = |ms: f32| ((ms / 1000.0) * cfg.rate as f32).round().max(1.0) as u32;
        let abort_enabled = cfg.low_signal_abort_ms > 0.0;
        Squelch {
            enabled,
            manual_level,
            snr_ratio,
            // Start the floor high (-40 dBFS) so the squelch begins CLOSED and the
            // floor converges downward to the true inter-transmission noise level.
            noise_floor: SAMPLE_SCALE * 0.01,
            level: 0.0,
            state: SquelchState::Closed,
            delay: 0,
            open_delay: ms_to_samples(cfg.open_delay_ms),
            close_delay: ms_to_samples(cfg.close_delay_ms),
            abort_enabled,
            low_signal_abort: if abort_enabled {
                ms_to_samples(cfg.low_signal_abort_ms)
            } else {
                u32::MAX
            },
            low_signal_count: 0,
            sample_count: 0,
            open_count: 0,
            just_opened: false,
            just_closed: false,
        }
    }

    /// Current open/close threshold in raw sample units.
    #[inline]
    pub fn threshold(&self) -> f32 {
        self.manual_level.unwrap_or(self.snr_ratio * self.noise_floor)
    }

    /// Overrides the fixed (manual) threshold at runtime. Used by carrier-power
    /// squelch, whose threshold tracks a cross-channel noise estimate
    /// ([`carrier_noise_threshold`]) rather than a per-channel adaptive floor.
    #[inline]
    pub fn set_threshold(&mut self, level: f32) {
        self.manual_level = Some(level);
    }

    /// Tracked noise floor in raw sample units.
    #[inline]
    pub fn noise_floor(&self) -> f32 {
        self.noise_floor
    }

    /// Smoothed audio level in raw sample units.
    #[inline]
    pub fn level(&self) -> f32 {
        self.level
    }

    #[inline]
    pub fn state(&self) -> SquelchState {
        self.state
    }

    /// True while audio should be heard (OPEN or CLOSING).
    #[inline]
    pub fn is_open(&self) -> bool {
        !self.enabled || matches!(self.state, SquelchState::Open | SquelchState::Closing)
    }

    /// True on the single sample where the squelch just fully opened.
    #[inline]
    pub fn just_opened(&self) -> bool {
        self.just_opened
    }

    /// True on the single sample where audio just stopped passing.
    #[inline]
    pub fn just_closed(&self) -> bool {
        self.just_closed
    }

    /// Number of completed openings (transmission count).
    #[inline]
    pub fn open_count(&self) -> u64 {
        self.open_count
    }

    /// Advances the state machine by one audio sample.
    ///
    /// `mag` is the instantaneous magnitude `|sample|` in raw 24-bit units. Returns
    /// the post-update state. When the squelch is disabled, always reports `Open`.
    pub fn process(&mut self, mag: f32) -> SquelchState {
        self.just_opened = false;
        self.just_closed = false;

        if !self.enabled {
            self.state = SquelchState::Open;
            return self.state;
        }

        self.sample_count += 1;
        self.level = self.level * LEVEL_DECAY + mag * (1.0 - LEVEL_DECAY);

        // Re-estimate the noise floor only while shut, so a held transmission does
        // not pull the floor up under itself.
        if !self.is_open() && self.sample_count.is_multiple_of(NOISE_UPDATE_INTERVAL) {
            let m = self.level.min(self.noise_floor);
            self.noise_floor = self.noise_floor * NOISE_DECAY + m * (1.0 - NOISE_DECAY) + NOISE_EPSILON;
        }

        let thr = self.threshold();

        // Fast drop detection works on the instantaneous magnitude.
        if mag < thr {
            self.low_signal_count = self.low_signal_count.saturating_add(1);
        } else {
            self.low_signal_count = 0;
        }

        match self.state {
            SquelchState::Closed => {
                if self.level >= thr {
                    self.state = SquelchState::Opening;
                    self.delay = 0;
                }
            }
            SquelchState::Opening => {
                if self.level < thr {
                    self.state = SquelchState::Closed;
                } else {
                    self.delay += 1;
                    if self.delay >= self.open_delay {
                        self.state = SquelchState::Open;
                        self.open_count += 1;
                        self.just_opened = true;
                    }
                }
            }
            SquelchState::Open => {
                if self.abort_enabled && self.low_signal_count >= self.low_signal_abort {
                    self.state = SquelchState::LowSignalAbort;
                    self.delay = 0;
                    self.just_closed = true;
                } else if self.level < thr {
                    self.state = SquelchState::Closing;
                    self.delay = 0;
                }
            }
            SquelchState::Closing => {
                if self.abort_enabled && self.low_signal_count >= self.low_signal_abort {
                    self.state = SquelchState::LowSignalAbort;
                    self.delay = 0;
                    self.just_closed = true;
                } else if self.level >= thr {
                    self.state = SquelchState::Open;
                } else {
                    self.delay += 1;
                    if self.delay >= self.close_delay {
                        self.state = SquelchState::Closed;
                        self.just_closed = true;
                    }
                }
            }
            SquelchState::LowSignalAbort => {
                self.delay += 1;
                if self.delay >= self.close_delay {
                    self.state = SquelchState::Closed;
                }
            }
        }

        self.state
    }
}

/// Computes the shared open/close threshold for carrier-power squelch from the
/// most-recent per-channel carrier levels.
///
/// All channels of one receiver see the same wideband noise, so the bulk of the
/// per-channel carrier levels are noise and a channel carrying a station is a
/// large outlier. We take a high percentile of the population (robust to a few
/// active channels) as the *noise* reference and place the threshold `snr_ratio`
/// above it. Because the threshold is derived from the *other* channels' noise,
/// it stays put under a continuous carrier — so AWOS/ATIS stays open instead of
/// being learned away by a per-channel adaptive floor.
///
/// `percentile` is in `0.0..=1.0` (e.g. `0.75`). Returns `f32::MAX` (always shut)
/// if there are no samples.
pub fn carrier_noise_threshold(carriers: &[f32], percentile: f32, snr_ratio: f32) -> f32 {
    if carriers.is_empty() {
        return f32::MAX;
    }
    let mut v: Vec<f32> = carriers.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p = percentile.clamp(0.0, 1.0);
    let idx = (((v.len() - 1) as f32) * p).round() as usize;
    let noise = v[idx.min(v.len() - 1)];
    // Never collapse to zero (a fully-silent channel set) — keep a tiny floor so
    // the threshold stays meaningful and the squelch stays shut on pure silence.
    (noise.max(NOISE_EPSILON)) * snr_ratio
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto(rate: u32, snr_db: f32) -> Squelch {
        Squelch::new(SquelchConfig::new(SquelchMode::AutoSnr { snr_db }, rate))
    }

    #[test]
    fn disabled_always_open() {
        let mut sq = Squelch::new(SquelchConfig::new(SquelchMode::Off, 15625));
        for _ in 0..100 {
            sq.process(0.0);
            assert!(sq.is_open());
        }
    }

    #[test]
    fn noise_floor_converges_downward() {
        let mut sq = auto(15625, 9.0);
        let start = sq.noise_floor();
        // feed a quiet, steady "no signal" level
        for _ in 0..200_000 {
            sq.process(50.0);
        }
        let end = sq.noise_floor();
        assert!(end < start, "floor should drop ({start} -> {end})");
        assert!(end < 80.0, "floor should approach the input level (~50), got {end}");
        assert!(!sq.is_open(), "quiet input must keep squelch shut");
    }

    #[test]
    fn opens_on_signal_then_closes_on_silence() {
        let mut sq = auto(15625, 9.0); // ratio ~2.82
        // settle the noise floor at ~50
        for _ in 0..200_000 {
            sq.process(50.0);
        }
        assert!(!sq.is_open());
        // strong sustained signal opens it
        let mut opened = false;
        for _ in 0..4000 {
            sq.process(750.0);
            if sq.is_open() {
                opened = true;
                break;
            }
        }
        assert!(opened, "sustained strong signal should open squelch");
        assert_eq!(sq.open_count(), 1);
        // silence closes it again, but only after the (1 s) hang elapses
        let mut closed = false;
        for _ in 0..30_000 {
            sq.process(0.0);
            if !sq.is_open() {
                closed = true;
                break;
            }
        }
        assert!(closed, "prolonged silence should close squelch");
    }

    #[test]
    fn hang_bridges_short_gaps() {
        // A short gap (well under the hang time) must NOT close the squelch -
        // this is what stops AWOS/ATIS chatter between words.
        let rate = 15625;
        let mut sq = Squelch::new(SquelchConfig::new(SquelchMode::AutoSnr { snr_db: 9.0 }, rate));
        for _ in 0..200_000 {
            sq.process(50.0);
        }
        for _ in 0..4000 {
            sq.process(750.0);
        }
        assert!(sq.is_open(), "should be open on signal");
        // 300 ms gap of silence (< 1 s hang) -> stays open
        let gap = (0.3 * rate as f32) as usize;
        for _ in 0..gap {
            sq.process(0.0);
        }
        assert!(sq.is_open(), "a 300 ms gap must not close the squelch");
        assert_eq!(sq.open_count(), 1, "no re-open should have occurred");
    }

    #[test]
    fn abort_disabled_by_default() {
        let sq = Squelch::new(SquelchConfig::new(SquelchMode::AutoSnr { snr_db: 9.0 }, 15625));
        assert!(!sq.abort_enabled, "carrier-loss abort must be off by default");
    }

    #[test]
    fn manual_threshold_gates_on_dbfs() {
        let mut sq = Squelch::new(SquelchConfig::new(SquelchMode::ManualDbfs(-40.0), 15625));
        let thr = sq.threshold();
        // below threshold stays shut
        for _ in 0..2000 {
            sq.process(thr * 0.1);
        }
        assert!(!sq.is_open());
        // well above threshold opens
        let mut opened = false;
        for _ in 0..4000 {
            sq.process(thr * 4.0);
            if sq.is_open() {
                opened = true;
                break;
            }
        }
        assert!(opened);
    }

    #[test]
    fn carrier_threshold_separates_station_from_noise() {
        // 20 noise channels (spread 27e6..168e6) + one station at 1.48e9, like
        // the measured AWOS case. Threshold must land between them.
        let mut c = vec![
            27e6, 33e6, 37e6, 37e6, 46e6, 46e6, 50e6, 50e6, 58e6, 67e6, 75e6, 100e6, 125e6,
            134e6, 134e6, 150e6, 150e6, 150e6, 150e6, 168e6,
        ];
        c.push(1.476e9);
        let thr = carrier_noise_threshold(&c, 0.75, 10f32.powf(9.0 / 20.0));
        assert!(thr > 168e6, "threshold {thr} must clear the noisiest empty channel");
        assert!(thr < 1.476e9, "threshold {thr} must stay below the station carrier");
    }

    #[test]
    fn carrier_squelch_opens_on_station_only() {
        let rate = 15625;
        let mut shut = Squelch::new(SquelchConfig::new(SquelchMode::Carrier { snr_db: 9.0 }, rate));
        let mut open = Squelch::new(SquelchConfig::new(SquelchMode::Carrier { snr_db: 9.0 }, rate));
        // starts effectively closed before any threshold update
        assert!(!shut.is_open());
        let thr = 423e6; // between noise (<=168e6) and station (1.48e9)
        shut.set_threshold(thr);
        open.set_threshold(thr);
        for _ in 0..4000 {
            shut.process(150e6); // noisy empty channel
            open.process(1.476e9); // station carrier (continuous)
        }
        assert!(!shut.is_open(), "empty channel must stay shut");
        assert!(open.is_open(), "continuous station carrier must stay open");
    }
}
