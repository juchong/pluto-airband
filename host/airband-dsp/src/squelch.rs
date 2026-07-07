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
/// Hard lower bound on the tracked noise floor, in **raw 24-bit sample units**.
///
/// The floor re-estimates toward the *minimum* level seen while shut. On a
/// channel with no signal the demodulated audio sits at the ADC **quantization
/// floor** — mostly-zero samples with sparse ±1–2 LSB ticks — so without a floor
/// the EWMA collapses toward `NOISE_EPSILON / (1 - NOISE_DECAY) ≈ 3e-5` (≈ 0
/// against the `2**23` full scale). The open threshold (`snr_ratio × floor`) then
/// becomes ~0 and a single 1-LSB quantization tick false-opens the squelch —
/// which downstream AGC/DeepFilterNet amplify into "robotic" static. Pinning the
/// floor at a few LSB keeps the threshold (e.g. ~11 LSB at the 9 dB default) above
/// quantization noise on an empty channel while staying far below any genuine
/// transmission (tens–hundreds of LSB), so adaptation is unaffected once real
/// receiver noise is present. `NOISE_EPSILON` (carried from RTLSDR-Airband's
/// normalized [0,1] domain) is far too small to serve this role in raw units.
const MIN_NOISE_FLOOR: f32 = 4.0;

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
    /// Open/close hysteresis in dB: the *close* threshold sits this many dB below
    /// the *open* threshold, so a transmission that has opened holds until the
    /// level falls to `open_threshold / 10^(hysteresis_db/20)` (a distinct, lower
    /// bar than the one it opened on). This lets a weak station ride through fades
    /// and lets the open threshold be run low for sensitivity without chatter.
    /// `0.0` = a single threshold (original behaviour).
    pub hysteresis_db: f32,
}

impl SquelchConfig {
    /// Defaults tuned for DC-blocked audio: 1 s hang, carrier-loss abort off,
    /// single threshold (no hysteresis).
    pub fn new(mode: SquelchMode, rate: u32) -> SquelchConfig {
        SquelchConfig {
            mode,
            rate,
            open_delay_ms: 5.0,
            close_delay_ms: 1000.0,
            low_signal_abort_ms: 0.0,
            hysteresis_db: 0.0,
        }
    }

    /// Sets the hang time (close delay) in milliseconds (builder style).
    pub fn with_hang_ms(mut self, ms: f32) -> SquelchConfig {
        self.close_delay_ms = ms;
        self
    }

    /// Sets the open/close hysteresis in dB (builder style; clamped to >= 0).
    pub fn with_hysteresis_db(mut self, db: f32) -> SquelchConfig {
        self.hysteresis_db = db.max(0.0);
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
    /// Close threshold as a fraction of the open threshold (`10^(-hysteresis_db/20)`,
    /// so `1.0` = no hysteresis). See [`SquelchConfig::hysteresis_db`].
    close_ratio: f32,

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
            // Carrier mode's threshold is pushed in via `set_threshold` from a
            // per-channel adaptive floor ([`CarrierFloor`]), so it starts
            // effectively closed (huge threshold) until that floor is seeded.
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
            close_ratio: 10f32.powf(-cfg.hysteresis_db.max(0.0) / 20.0),
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
        self.process_gated(mag, true)
    }

    /// Like [`process`](Self::process) but an external discriminator may veto a
    /// *new* opening. While `allow_open` is false the squelch cannot transition
    /// CLOSED->OPEN (used to require voice-like structure, not just energy — see
    /// [`crate::SpectralFlatness`]), but an already-open transmission is **not**
    /// force-closed by it: energy + hysteresis + hang still manage closing, so a
    /// brief non-voice frame mid-transmission does not chop the audio.
    ///
    /// Opening is gated by the open threshold; closing by the (lower) close
    /// threshold `open_threshold * close_ratio` (see [`SquelchConfig::hysteresis_db`]).
    pub fn process_gated(&mut self, mag: f32, allow_open: bool) -> SquelchState {
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
            self.noise_floor = (self.noise_floor * NOISE_DECAY + m * (1.0 - NOISE_DECAY)
                + NOISE_EPSILON)
                .max(MIN_NOISE_FLOOR);
        }

        let open_thr = self.threshold();
        let close_thr = open_thr * self.close_ratio;

        // Fast drop detection works on the instantaneous magnitude against the
        // lower (close) bar, so a marginal open signal is not aborted prematurely.
        if mag < close_thr {
            self.low_signal_count = self.low_signal_count.saturating_add(1);
        } else {
            self.low_signal_count = 0;
        }

        match self.state {
            SquelchState::Closed => {
                if allow_open && self.level >= open_thr {
                    self.state = SquelchState::Opening;
                    self.delay = 0;
                }
            }
            SquelchState::Opening => {
                // Revert if the discriminator withdraws consent or the level drops
                // below the (high) open bar before the open delay elapses.
                if !allow_open || self.level < open_thr {
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
                } else if self.level < close_thr {
                    self.state = SquelchState::Closing;
                    self.delay = 0;
                }
            }
            SquelchState::Closing => {
                if self.abort_enabled && self.low_signal_count >= self.low_signal_abort {
                    self.state = SquelchState::LowSignalAbort;
                    self.delay = 0;
                    self.just_closed = true;
                } else if self.level >= close_thr {
                    // Recovered above the low bar within the hang window: stay open.
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

/// Per-channel adaptive carrier-noise floor for carrier-power squelch.
///
/// The cross-channel reference ([`carrier_noise_threshold`]) gives every channel
/// one shared *absolute* threshold, which cannot separate a quiet channel's weak
/// keyings from a comb-hot channel's noise transients — and per-channel baselines
/// drift through the day, so no static value holds. This instead tracks each
/// channel's *own* carrier baseline with an EWMA sampled **only while that
/// channel's squelch is shut** (idle), so the floor follows the conducted comb up
/// and down but never learns a transmission into itself. The open threshold is
/// then `floor × snr_ratio` — a fixed dB margin above each channel's own noise.
///
/// Seeded once from the cross-channel reference so it starts at a sane level (and
/// reads as always-shut until then). Because the floor freezes whenever the
/// squelch is open, a continuously-keyed station (e.g. ATIS) opens on the seed
/// and is never learned away — the failure mode that made carrier mode use a
/// shared reference in the first place.
pub struct CarrierFloor {
    floor: f32,
    seeded: bool,
    /// Per-sample EWMA weight, from the idle time constant and audio rate.
    alpha: f32,
}

impl CarrierFloor {
    /// Idle time constant (seconds) over which the floor tracks baseline drift.
    const TAU_S: f32 = 5.0;
    /// Hard lower bound (raw carrier units) so a dead channel cannot collapse the
    /// threshold to zero (mirrors [`MIN_NOISE_FLOOR`]'s role for the audio floor).
    const MIN: f32 = 1.0;

    /// Builds a floor whose EWMA settles over ~`TAU_S` of idle audio at `rate` Hz.
    pub fn new(rate: u32) -> CarrierFloor {
        let tau_samples = (Self::TAU_S * rate as f32).max(1.0);
        CarrierFloor {
            floor: 0.0,
            seeded: false,
            alpha: 1.0 - (-1.0 / tau_samples).exp(),
        }
    }

    /// Seeds the floor once from the cross-channel noise reference (raw carrier
    /// units). No-op once seeded or while `noise <= 0` (router not yet warmed up).
    pub fn seed(&mut self, noise: f32) {
        if !self.seeded && noise > 0.0 {
            self.floor = noise;
            self.seeded = true;
        }
    }

    /// Advances the floor by one sample. `carrier` is the channel's current AM
    /// carrier level (raw units); `closed` must be true **only** while the squelch
    /// is fully shut, so a held transmission never pulls the floor up under itself.
    #[inline]
    pub fn update(&mut self, carrier: f32, closed: bool) {
        if self.seeded && closed {
            self.floor += self.alpha * (carrier - self.floor);
        }
    }

    /// Open/close threshold in raw carrier units (`floor × snr_ratio`), clamped to
    /// [`MIN`](Self::MIN). Returns `f32::MAX` (always shut) until seeded.
    #[inline]
    pub fn threshold(&self, snr_ratio: f32) -> f32 {
        self.threshold_common(snr_ratio, 0.0)
    }

    /// Threshold using the larger of this channel's slow per-channel floor and a
    /// live `common_mode` noise reference (the ~real-time cross-channel median
    /// from [`carrier_noise_threshold`]). The per-channel floor handles each
    /// channel's steady conducted-comb baseline; the common-mode term tracks the
    /// fast, band-wide fluctuation a nearby broadband emitter causes on **all**
    /// channels at once — which the 5 s per-channel EWMA is far too slow to
    /// follow. Taking the max means a band-wide noise burst raises the threshold
    /// immediately (no false open), and the moment it passes the threshold drops
    /// back so a weak station is not masked. Returns `f32::MAX` until seeded.
    #[inline]
    pub fn threshold_common(&self, snr_ratio: f32, common_mode: f32) -> f32 {
        if self.seeded {
            self.floor.max(common_mode).max(Self::MIN) * snr_ratio
        } else {
            f32::MAX
        }
    }
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
    fn quantization_floor_does_not_false_open() {
        // A dead channel sits at the demod quantization floor: mostly-zero
        // samples with sparse 1-2 LSB ticks. The adaptive floor must not collapse
        // and let those ticks false-open the squelch (which would otherwise feed
        // AGC/DeepFilterNet pure quantization noise -> "robotic" static).
        let mut sq = auto(15625, 9.0);
        let mut opened = false;
        for i in 0..1_000_000u64 {
            let mag = match i % 7 {
                0 => 1.0,
                3 => 2.0,
                _ => 0.0,
            };
            sq.process(mag);
            if sq.is_open() {
                opened = true;
                break;
            }
        }
        assert!(!opened, "empty quantization-floor channel must stay shut");
        assert!(
            sq.noise_floor() >= MIN_NOISE_FLOOR,
            "floor must not collapse below MIN_NOISE_FLOOR, got {}",
            sq.noise_floor()
        );
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

    #[test]
    fn carrier_floor_unseeded_is_shut() {
        let f = CarrierFloor::new(20000);
        assert_eq!(
            f.threshold(2.0),
            f32::MAX,
            "an unseeded floor must read as always-shut"
        );
    }

    #[test]
    fn carrier_floor_tracks_idle_up_and_down() {
        let mut f = CarrierFloor::new(20000);
        f.seed(100.0);
        // a rising idle baseline pulls the floor up toward it...
        for _ in 0..400_000 {
            f.update(500.0, true);
        }
        let up = f.threshold(1.0);
        assert!(up > 400.0, "floor should climb toward 500, got {up}");
        // ...and a falling one pulls it back down (the min-only audio floor can't).
        for _ in 0..400_000 {
            f.update(50.0, true);
        }
        let down = f.threshold(1.0);
        assert!(down < 100.0, "floor should fall toward 50, got {down}");
    }

    #[test]
    fn carrier_floor_frozen_while_open() {
        let mut f = CarrierFloor::new(20000);
        f.seed(100.0);
        let before = f.threshold(1.0);
        // a held transmission (closed = false) must not move the floor.
        for _ in 0..400_000 {
            f.update(1.0e6, false);
        }
        assert_eq!(f.threshold(1.0), before, "floor must not move while open");
    }

    #[test]
    fn hysteresis_holds_open_below_open_threshold() {
        // With 12 dB hysteresis (close bar at 1/4 of the open bar), a signal that
        // opened then fades to *below* the open threshold but *above* the close
        // threshold must stay open — where a single-threshold squelch would close.
        let rate = 15625;
        let cfg = SquelchConfig::new(SquelchMode::AutoSnr { snr_db: 9.0 }, rate)
            .with_hysteresis_db(12.0);
        let mut sq = Squelch::new(cfg);
        for _ in 0..200_000 {
            sq.process(50.0); // settle floor ~50 -> open thr ~141, close thr ~35
        }
        for _ in 0..4000 {
            sq.process(750.0);
        }
        assert!(sq.is_open(), "strong signal opens");
        let open_thr = sq.threshold();
        // Fade to just under the open threshold but well over the close bar.
        for _ in 0..4000 {
            sq.process(open_thr * 0.5);
        }
        assert!(sq.is_open(), "must hold open between the close and open bars");
        // A single-threshold squelch (no hysteresis) closes at the same level.
        let mut plain = Squelch::new(SquelchConfig::new(SquelchMode::AutoSnr { snr_db: 9.0 }, rate));
        for _ in 0..200_000 {
            plain.process(50.0);
        }
        for _ in 0..4000 {
            plain.process(750.0);
        }
        let pthr = plain.threshold();
        for _ in 0..30_000 {
            plain.process(pthr * 0.5);
        }
        assert!(!plain.is_open(), "no-hysteresis squelch closes below its threshold");
    }

    #[test]
    fn gate_vetoes_open_but_does_not_chop() {
        // allow_open=false must block a fresh open even on a strong level, but must
        // NOT close an already-open transmission (a brief non-voice frame).
        let mut sq = auto(15625, 9.0);
        for _ in 0..200_000 {
            sq.process_gated(50.0, true);
        }
        // Strong level, but discriminator says "not voice" -> stays shut.
        for _ in 0..4000 {
            sq.process_gated(750.0, false);
        }
        assert!(!sq.is_open(), "veto must block opening on non-voice energy");
        // Now allow it -> opens.
        for _ in 0..4000 {
            sq.process_gated(750.0, true);
        }
        assert!(sq.is_open(), "opens once the discriminator consents");
        // A single vetoed sample mid-transmission must not close it.
        sq.process_gated(750.0, false);
        assert!(sq.is_open(), "a brief non-voice frame must not chop an open tx");
    }

    #[test]
    fn threshold_common_takes_the_larger_floor() {
        let mut f = CarrierFloor::new(20000);
        f.seed(100.0);
        // Per-channel floor at ~100; a live common-mode burst of 500 dominates.
        assert_eq!(f.threshold_common(1.0, 500.0), 500.0);
        // When the common-mode is below the per-channel floor, the floor wins.
        assert_eq!(f.threshold_common(1.0, 10.0), 100.0);
        // Unseeded stays always-shut regardless of the common-mode reference.
        let g = CarrierFloor::new(20000);
        assert_eq!(g.threshold_common(1.0, 500.0), f32::MAX);
    }

    #[test]
    fn per_channel_floors_yield_different_thresholds() {
        // The whole point: a comb-hot channel and a quiet channel, fed the same
        // global SNR ratio, must end up with very different thresholds.
        let ratio = 10f32.powf(10.0 / 20.0);
        let mut quiet = CarrierFloor::new(20000);
        let mut comb = CarrierFloor::new(20000);
        quiet.seed(100.0);
        comb.seed(100.0);
        for _ in 0..400_000 {
            quiet.update(80.0, true);
            comb.update(800.0, true);
        }
        assert!(
            comb.threshold(ratio) > quiet.threshold(ratio) * 5.0,
            "comb-hot channel must demand a far higher threshold ({} vs {})",
            comb.threshold(ratio),
            quiet.threshold(ratio)
        );
    }
}
