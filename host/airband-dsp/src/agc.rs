//! AM automatic gain control, adapted from RTLSDR-Airband's per-channel AGC
//! (`src/rtl_airband.cpp`, the `agcavgfast` / `agcavgslow` loop).
//!
//! Goal: even out loudness between strong and weak transmissions and across
//! channels, then soft-limit peaks so a hot signal does not clip the output.
//!
//! Unlike upstream we run on the FPGA's DC-blocked AC audio, so there is no carrier
//! DC to subtract - the gain target is simply the running magnitude of the audio.
//! AGC adaptation is driven by the squelch: it only learns from samples the squelch
//! considers signal, holds its gain across brief drops, and fades to silence when
//! the squelch shuts (preventing the click/burst RTLSDR-Airband avoids with its
//! `AGC_EXTRA` ramp).
//!
//! All amplitudes here are normalized to full scale (`sample / 2**23`, i.e. -1..1).

/// EWMA weight applied to the gain estimate while tracking signal (slow).
const AVG_DECAY: f32 = 0.995;
/// EWMA weight for the "recent background" estimate used to bootstrap on open.
const RECENT_DECAY: f32 = 0.9;
/// Per-sample decay of the output tail after the squelch shuts (click-free fade).
const FADE_DECAY: f32 = 0.94;
/// Output magnitude above which the soft limiter's knee engages.
const LIMIT_THRESHOLD: f32 = 0.8;
/// Upward nudge to the gain estimate when limiting, so the AGC adapts downward
/// over the next samples instead of riding the limiter (cf. RTLSDR-Airband).
const LIMIT_AVG_BOOST: f32 = 1.15;
/// Floor for the gain divisor, guarding against divide-by-zero on silence.
const AVG_FLOOR: f32 = 1e-9;

/// Smooth soft clip: identity below `LIMIT_THRESHOLD`, then a tanh knee that
/// asymptotes to full scale. C1-continuous at the threshold and strictly bounded
/// in (-1, 1), so a hot transient can never clip the 16-bit output.
#[inline]
fn soft_clip(v: f32) -> f32 {
    let t = LIMIT_THRESHOLD;
    let a = v.abs();
    if a <= t {
        v
    } else {
        v.signum() * (t + (1.0 - t) * ((a - t) / (1.0 - t)).tanh())
    }
}

/// Per-channel AM AGC.
pub struct Agc {
    /// Tracked signal magnitude that the gain normalizes against.
    avg: f32,
    /// Fast background magnitude estimate, used to seed `avg` when squelch opens.
    recent: f32,
    /// Normalization aggressiveness; output rides near `1.0 / k` of full scale.
    k: f32,
    /// Last emitted output, for the fade tail.
    prev_out: f32,
}

impl Default for Agc {
    fn default() -> Agc {
        Agc::new()
    }
}

impl Agc {
    pub fn new() -> Agc {
        Agc {
            avg: 1e-3,
            recent: 1e-3,
            k: 1.5,
            prev_out: 0.0,
        }
    }

    /// Builds an AGC with a custom aggressiveness factor `k` (smaller = louder).
    pub fn with_k(k: f32) -> Agc {
        Agc { k, ..Agc::new() }
    }

    /// Current gain estimate (normalized magnitude the output is divided by).
    #[inline]
    pub fn estimate(&self) -> f32 {
        self.avg
    }

    /// Processes one normalized sample.
    ///
    /// * `x` - normalized input sample (`sample / 2**23`).
    /// * `open` - whether the squelch currently passes audio.
    /// * `just_opened` - true on the first open sample (re-seed the gain).
    /// * `above_threshold` - whether this sample is above the squelch threshold
    ///   (only such samples train the gain, matching upstream).
    ///
    /// Returns the normalized, gain-corrected output sample.
    pub fn process(&mut self, x: f32, open: bool, just_opened: bool, above_threshold: bool) -> f32 {
        let mag = x.abs();
        self.recent = self.recent * RECENT_DECAY + mag * (1.0 - RECENT_DECAY);

        if !open {
            // Fade the previous output to zero rather than hard-muting.
            let y = self.prev_out * FADE_DECAY;
            self.prev_out = y;
            return y;
        }

        if just_opened {
            self.avg = self.recent.max(AVG_FLOOR);
        }
        if above_threshold {
            self.avg = self.avg * AVG_DECAY + mag * (1.0 - AVG_DECAY);
        }

        let denom = (self.avg * self.k).max(AVG_FLOOR);
        let raw = x / denom;
        if raw.abs() > LIMIT_THRESHOLD {
            // Adapt the gain down so we stop hitting the knee on sustained loud audio.
            self.avg *= LIMIT_AVG_BOOST;
        }
        let y = soft_clip(raw);
        self.prev_out = y;
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the AGC with a constant-magnitude square-ish input and returns the
    /// steady-state output magnitude.
    fn steady_output(amp: f32) -> f32 {
        let mut agc = Agc::new();
        let mut last = 0.0;
        for i in 0..20_000 {
            // alternate sign so it looks like AC audio, constant magnitude
            let x = if i % 2 == 0 { amp } else { -amp };
            last = agc.process(x, true, i == 0, true).abs();
        }
        last
    }

    #[test]
    fn normalizes_regardless_of_input_level() {
        let quiet = steady_output(1e-4);
        let loud = steady_output(1e-2);
        // both should land near 1/k = 0.667 and close to each other
        assert!((quiet - loud).abs() < 0.1, "quiet {quiet} vs loud {loud}");
        assert!(quiet > 0.4 && quiet < 0.8, "expected ~0.667, got {quiet}");
    }

    #[test]
    fn soft_limiter_caps_peaks() {
        let mut agc = Agc::new();
        // converge on a small signal
        for i in 0..20_000 {
            let x = if i % 2 == 0 { 1e-3 } else { -1e-3 };
            agc.process(x, true, i == 0, true);
        }
        // then a sudden 50x spike must be bounded (never clips), but still loud
        let y = agc.process(5e-2, true, false, true).abs();
        assert!(y <= 1.0, "peak must not exceed full scale, got {y}");
        assert!(y > LIMIT_THRESHOLD, "limited peak should still be loud, got {y}");
    }

    #[test]
    fn fades_to_silence_when_closed() {
        let mut agc = Agc::new();
        for i in 0..20_000 {
            let x = if i % 2 == 0 { 1e-3 } else { -1e-3 };
            agc.process(x, true, i == 0, true);
        }
        let mut prev = f32::MAX;
        let mut last = 1.0;
        for _ in 0..500 {
            last = agc.process(0.0, false, false, false).abs();
            assert!(last <= prev + 1e-9, "fade must be monotonic non-increasing");
            prev = last;
        }
        assert!(last < 1e-3, "should decay toward silence, got {last}");
    }
}
