//! Single-channel spectral noise reduction for airband AM audio.
//!
//! AM voice from the FPGA carries broadband hiss in the voice band that the
//! band-pass and AGC cannot remove (it lives *under* the speech). This module is
//! a short-time Fourier transform (STFT) denoiser using the decision-directed
//! Wiener gain of Ephraim & Malah - the standard low-"musical-noise" approach:
//!
//! * 50%-overlapping frames, analysis+synthesis with a sqrt-Hann window (so the
//!   product is a Hann window, which is COLA at hop `N/2` with unity sum -> exact
//!   reconstruction when the gain is 1.0).
//! * A per-bin noise power estimate is tracked (EWMA) only while the caller says
//!   the frame is noise (below the squelch threshold), so speech never leaks into
//!   the noise model. The estimate persists across transmissions.
//! * Per bin: a decision-directed a-priori SNR drives a Wiener gain, clamped to a
//!   spectral floor so the residual stays natural instead of warbling.
//!
//! Latency is one frame (`frame` samples). The transform works on whatever linear
//! amplitude the caller feeds (raw magnitude or full-scale-normalized); the gain
//! is a pure ratio, so scaling is irrelevant as long as signal and noise share it.

use std::collections::VecDeque;
use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

/// Decision-directed smoothing factor for the a-priori SNR (Ephraim-Malah).
const DD_ALPHA: f32 = 0.98;
/// EWMA factor for the per-bin noise power update while learning.
const NOISE_DECAY: f32 = 0.9;
/// Over-subtraction applied to the noise estimate when forming the SNR.
const OVERSUB: f32 = 1.5;
/// Guard against divide-by-zero in the SNR ratios.
const EPS: f32 = 1e-12;

/// STFT spectral-subtraction / Wiener denoiser for one audio stream.
pub struct Denoise {
    frame: usize,
    hop: usize,
    win: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    scratch: Vec<Complex32>,

    /// Carryover overlap block (the previous `hop` input samples).
    prev: Vec<f32>,
    prev_learn: usize,
    /// Newest input samples not yet formed into a frame.
    pending: Vec<f32>,
    pending_learn: usize,

    spec: Vec<Complex32>,
    noise_pow: Vec<f32>,
    prev_clean_pow: Vec<f32>,
    ola: Vec<f32>,
    out_q: VecDeque<f32>,

    /// Spectral floor as a linear gain (e.g. -18 dB -> 0.126).
    floor: f32,
    primed: usize,
}

impl Denoise {
    /// Builds a denoiser with `frame`-point FFTs (must be even) at 50% overlap.
    /// `floor_db` bounds how far any bin may be attenuated (more negative =
    /// deeper cut, more aggressive). Typical: `frame=256`, `floor_db=-18`.
    pub fn new(frame: usize, floor_db: f32) -> Denoise {
        assert!(frame >= 16 && frame.is_multiple_of(2), "frame must be even and >= 16");
        let hop = frame / 2;
        // sqrt-Hann: win[n]^2 = Hann, and sum of overlapping Hann at hop N/2 == 1.
        let win: Vec<f32> = (0..frame)
            .map(|n| {
                let h =
                    0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / frame as f32).cos();
                h.sqrt()
            })
            .collect();
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(frame);
        let ifft = planner.plan_fft_inverse(frame);
        let scratch = vec![Complex32::new(0.0, 0.0); fft.get_inplace_scratch_len().max(ifft.get_inplace_scratch_len())];
        let nbins = frame / 2 + 1;
        Denoise {
            frame,
            hop,
            win,
            fft,
            ifft,
            scratch,
            prev: vec![0.0; hop],
            prev_learn: 0,
            pending: Vec::with_capacity(hop),
            pending_learn: 0,
            spec: vec![Complex32::new(0.0, 0.0); frame],
            noise_pow: vec![EPS; nbins],
            prev_clean_pow: vec![0.0; nbins],
            ola: vec![0.0; frame],
            out_q: VecDeque::with_capacity(frame),
            floor: 10f32.powf(floor_db / 20.0),
            primed: frame,
        }
    }

    /// Clears all transform state (call on a stream discontinuity). The learned
    /// noise estimate is intentionally preserved.
    pub fn reset(&mut self) {
        self.prev.iter_mut().for_each(|v| *v = 0.0);
        self.pending.clear();
        self.prev_learn = 0;
        self.pending_learn = 0;
        self.ola.iter_mut().for_each(|v| *v = 0.0);
        self.prev_clean_pow.iter_mut().for_each(|v| *v = 0.0);
        self.out_q.clear();
        self.primed = self.frame;
    }

    /// Processes one sample. `learn_noise` should be true when this sample is
    /// background noise (squelch below threshold) so the noise model can adapt.
    /// Returns the denoised sample, delayed by one frame.
    pub fn process(&mut self, x: f32, learn_noise: bool) -> f32 {
        self.pending.push(x);
        if learn_noise {
            self.pending_learn += 1;
        }
        if self.pending.len() == self.hop {
            self.run_frame();
            self.prev.copy_from_slice(&self.pending);
            self.prev_learn = self.pending_learn;
            self.pending.clear();
            self.pending_learn = 0;
        }
        // One frame of latency: emit silence until the pipeline has filled.
        if self.primed > 0 {
            self.primed -= 1;
            return 0.0;
        }
        self.out_q.pop_front().unwrap_or(0.0)
    }

    fn run_frame(&mut self) {
        let n = self.frame;
        // frame = [prev (hop) | pending (hop)], windowed into the FFT input.
        for i in 0..self.hop {
            self.spec[i] = Complex32::new(self.prev[i] * self.win[i], 0.0);
        }
        for i in 0..self.hop {
            let s = self.pending[i] * self.win[self.hop + i];
            self.spec[self.hop + i] = Complex32::new(s, 0.0);
        }
        self.fft.process_with_scratch(&mut self.spec, &mut self.scratch);

        let learn = (self.prev_learn + self.pending_learn) * 2 >= n;
        let nbins = n / 2 + 1;
        for k in 0..nbins {
            let power = self.spec[k].norm_sqr();
            if learn {
                self.noise_pow[k] = NOISE_DECAY * self.noise_pow[k] + (1.0 - NOISE_DECAY) * power;
            }
            let noise = (OVERSUB * self.noise_pow[k]).max(EPS);
            let post = power / noise;
            let prior = DD_ALPHA * (self.prev_clean_pow[k] / noise)
                + (1.0 - DD_ALPHA) * (post - 1.0).max(0.0);
            let gain = (prior / (prior + 1.0)).max(self.floor);
            self.spec[k] *= gain;
            self.prev_clean_pow[k] = gain * gain * power;
            // Mirror to the negative-frequency half for the real inverse.
            if k > 0 && k < n - k {
                self.spec[n - k] = self.spec[k].conj();
            }
        }

        self.ifft.process_with_scratch(&mut self.spec, &mut self.scratch);
        let norm = 1.0 / n as f32;
        for i in 0..n {
            self.ola[i] += self.spec[i].re * norm * self.win[i];
        }
        // The first `hop` samples of the accumulator are now final.
        for i in 0..self.hop {
            self.out_q.push_back(self.ola[i]);
        }
        self.ola.copy_within(self.hop..n, 0);
        self.ola[(n - self.hop)..n].iter_mut().for_each(|v| *v = 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn rms(xs: &[f32]) -> f32 {
        (xs.iter().map(|v| v * v).sum::<f32>() / xs.len() as f32).sqrt()
    }

    /// A deterministic pseudo-random noise generator (no rand dependency).
    fn noise(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    }

    #[test]
    fn improves_snr_on_tone_in_noise() {
        let fs = 15625.0;
        let f0 = 1000.0;
        let mut dn = Denoise::new(256, -18.0);
        let mut seed = 12345u32;
        let amp = 0.2;
        let nsd = 0.1;

        // Phase 1: pure noise, learning the noise floor (squelch "below threshold").
        for _ in 0..20_000 {
            dn.process(nsd * noise(&mut seed), true);
        }
        // Phase 2: tone + noise, not learning; collect output after the latency.
        let mut clean_in = Vec::new();
        let mut noisy = Vec::new();
        let mut out = Vec::new();
        for i in 0..20_000 {
            let s = amp * (2.0 * PI * f0 * i as f32 / fs).sin();
            let nz = nsd * noise(&mut seed);
            clean_in.push(s);
            noisy.push(s + nz);
            out.push(dn.process(s + nz, false));
        }
        // Discard the first frame (latency / transient).
        let tail = 2048;
        let n_rms = rms(&noisy[noisy.len() - tail..]);
        let o_rms = rms(&out[out.len() - tail..]);
        // Output should retain the tone but be quieter overall (noise removed).
        assert!(o_rms < n_rms, "denoised rms {o_rms} should be < noisy rms {n_rms}");
        assert!(o_rms > amp * 0.4, "tone must survive, got rms {o_rms}");
    }

    #[test]
    fn passes_clean_signal_when_no_noise_learned() {
        // With no noise learned (estimate stays ~0), gain ~1 -> near-unity passthrough.
        let fs = 15625.0;
        let f0 = 1200.0;
        let mut dn = Denoise::new(256, -18.0);
        let mut out = Vec::new();
        let mut inp = Vec::new();
        for i in 0..8000 {
            let s = 0.3 * (2.0 * PI * f0 * i as f32 / fs).sin();
            inp.push(s);
            out.push(dn.process(s, false));
        }
        let tail = 2048;
        let i_rms = rms(&inp[inp.len() - tail..]);
        let o_rms = rms(&out[out.len() - tail..]);
        assert!((o_rms - i_rms).abs() / i_rms < 0.1, "passthrough rms {o_rms} vs {i_rms}");
    }
}
