//! Spectral-flatness voice/noise discriminator for the squelch open decision.
//!
//! An energy squelch cannot tell weak voice from broadband noise of the same
//! level — which is exactly the failure mode when a nearby transmitter raises the
//! whole band. But the two differ in *structure*: broadband interference has a
//! flat, noise-like spectrum, while AM voice concentrates its energy in
//! formant/harmonic peaks inside the 300-3400 Hz voice band. The spectral
//! flatness measure (geometric mean / arithmetic mean of the per-bin power) is a
//! scale-invariant number in `(0, 1]` — near `1` for white/broadband noise, well
//! below for tonal/voiced audio — so gating the squelch's *open* transition on
//! "flatness below a threshold" lets the SNR margin run low (sensitive to weak
//! stations) without opening on a band-wide noise burst.
//!
//! This is the "detect voice, not just energy" half of the classic dual-metric
//! airborne squelch (carrier-present AND voice-present); the carrier/energy half
//! is the squelch threshold itself. See the long-term spectral flatness VAD
//! literature (Springer J. Audio Speech Music Proc. 2013).

use std::f32::consts::PI;
use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// Streaming spectral-flatness voice detector over non-overlapping Hann frames.
pub struct SpectralFlatness {
    frame: usize,
    win: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,
    /// Reused as FFT input (windowed samples) then output (spectrum).
    buf: Vec<Complex32>,
    fill: usize,
    /// Voice-band bin range `[lo_bin, hi_bin]` (300-3400 Hz).
    lo_bin: usize,
    hi_bin: usize,
    /// Max flatness still treated as voice (lower = stricter).
    max_flatness: f32,
    /// Latest decision, held between frame boundaries.
    voice: bool,
}

impl SpectralFlatness {
    /// Builds a detector with `frame`-point FFTs at `rate` Hz. `max_flatness` is
    /// the flatness ceiling (`0..1`) still treated as voice; broadband noise sits
    /// near `1`, voiced audio well below (a ceiling around `0.5` separates them).
    pub fn new(rate: f64, frame: usize, max_flatness: f32) -> SpectralFlatness {
        assert!(frame >= 32 && frame.is_multiple_of(2), "frame must be even and >= 32");
        let win: Vec<f32> = (0..frame)
            .map(|n| 0.5 - 0.5 * (2.0 * PI * n as f32 / frame as f32).cos())
            .collect();
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(frame);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let bin_hz = rate / frame as f64;
        // Restrict to the voice band; skip DC (bin 0) so the carrier-DC residue
        // and any sub-300 Hz rumble do not dominate the flatness.
        let lo_bin = ((300.0 / bin_hz).floor() as usize).max(1);
        let hi_bin = ((3400.0 / bin_hz).ceil() as usize).min(frame / 2);
        SpectralFlatness {
            frame,
            win,
            fft,
            scratch,
            buf: vec![Complex32::new(0.0, 0.0); frame],
            fill: 0,
            lo_bin,
            hi_bin: hi_bin.max(lo_bin + 1),
            max_flatness,
            voice: false,
        }
    }

    /// Feeds one audio sample (any linear scale; flatness is a ratio). Returns the
    /// current voice-present estimate, refreshed once per `frame` samples and held
    /// in between.
    pub fn process(&mut self, x: f32) -> bool {
        self.buf[self.fill] = Complex32::new(x * self.win[self.fill], 0.0);
        self.fill += 1;
        if self.fill == self.frame {
            self.fill = 0;
            self.voice = self.evaluate();
        }
        self.voice
    }

    /// Latest decision without advancing.
    #[inline]
    pub fn voice(&self) -> bool {
        self.voice
    }

    /// Clears the fill buffer and decision (call on a stream discontinuity).
    pub fn reset(&mut self) {
        self.fill = 0;
        self.voice = false;
    }

    fn evaluate(&mut self) -> bool {
        self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
        // Geometric/arithmetic mean of the voice-band power. Compute the geometric
        // mean in the log domain to avoid underflow across many bins.
        let mut log_sum = 0.0f64;
        let mut lin_sum = 0.0f64;
        let mut n = 0u32;
        for k in self.lo_bin..=self.hi_bin {
            let p = self.buf[k].norm_sqr() as f64 + 1e-20;
            log_sum += p.ln();
            lin_sum += p;
            n += 1;
        }
        if n == 0 || lin_sum <= 0.0 {
            return false;
        }
        let geo = (log_sum / n as f64).exp();
        let arith = lin_sum / n as f64;
        let flatness = (geo / arith) as f32; // (0, 1]: low = tonal/voice, high = noise
        flatness < self.max_flatness
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(d: &mut SpectralFlatness, xs: &[f32]) -> bool {
        let mut v = false;
        for &x in xs {
            v = d.process(x);
        }
        v
    }

    #[test]
    fn tone_reads_as_voice_white_noise_does_not() {
        let fs = 20000.0;
        let frame = 512;
        let mut d = SpectralFlatness::new(fs, frame, 0.5);
        // A 1 kHz tone is maximally tonal (flatness ~0) -> voice.
        let tone: Vec<f32> = (0..frame * 4)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / fs as f32).sin())
            .collect();
        assert!(feed(&mut d, &tone), "a pure tone must read as voice");

        // White noise has a flat voice-band spectrum (flatness ~1) -> not voice,
        // even though its amplitude equals the tone's.
        d.reset();
        let mut s = 1u32;
        let noise: Vec<f32> = (0..frame * 8)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
            })
            .collect();
        assert!(!feed(&mut d, &noise), "flat broadband noise must not read as voice");
    }
}
