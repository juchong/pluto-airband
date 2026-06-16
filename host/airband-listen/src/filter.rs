//! Minimal biquad voice band-pass for airband AM audio.
//!
//! Airband voice occupies ~300-3400 Hz. The Pluto front-end picks up RF-borne
//! interference outside that band (a low ~90/330 Hz hum and, on some channels, a
//! ~7.6 kHz whine just under Nyquist), which the host makeup gain then amplifies
//! into an audible buzz. A 4th-order Butterworth band-pass (cascaded RBJ biquads:
//! two high-pass + two low-pass) removes the out-of-band buzz while leaving voice
//! intelligibility intact.

use std::f64::consts::PI;

#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    fn norm(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Biquad {
        Biquad {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// RBJ cookbook 2nd-order high-pass at `f0` (Hz) with quality factor `q`.
    fn high_pass(f0: f64, fs: f64, q: f64) -> Biquad {
        let w0 = 2.0 * PI * f0 / fs;
        let (cos_w0, sin_w0) = (w0.cos(), w0.sin());
        let alpha = sin_w0 / (2.0 * q);
        Biquad::norm(
            (1.0 + cos_w0) / 2.0,
            -(1.0 + cos_w0),
            (1.0 + cos_w0) / 2.0,
            1.0 + alpha,
            -2.0 * cos_w0,
            1.0 - alpha,
        )
    }

    /// RBJ cookbook 2nd-order low-pass at `f0` (Hz) with quality factor `q`.
    fn low_pass(f0: f64, fs: f64, q: f64) -> Biquad {
        let w0 = 2.0 * PI * f0 / fs;
        let (cos_w0, sin_w0) = (w0.cos(), w0.sin());
        let alpha = sin_w0 / (2.0 * q);
        Biquad::norm(
            (1.0 - cos_w0) / 2.0,
            1.0 - cos_w0,
            (1.0 - cos_w0) / 2.0,
            1.0 + alpha,
            -2.0 * cos_w0,
            1.0 - alpha,
        )
    }

    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// 4th-order Butterworth band-pass (two high-pass + two low-pass biquads).
#[derive(Clone)]
pub struct VoiceFilter {
    stages: [Biquad; 4],
}

impl VoiceFilter {
    // Butterworth 4th-order section quality factors.
    const Q1: f64 = 0.541_196_10;
    const Q2: f64 = 1.306_562_96;

    /// Band-pass with `low`/`high` -3 dB corners (Hz) at sample rate `fs` (Hz).
    pub fn new(fs: f64, low: f64, high: f64) -> VoiceFilter {
        VoiceFilter {
            stages: [
                Biquad::high_pass(low, fs, Self::Q1),
                Biquad::high_pass(low, fs, Self::Q2),
                Biquad::low_pass(high, fs, Self::Q1),
                Biquad::low_pass(high, fs, Self::Q2),
            ],
        }
    }

    #[inline]
    pub fn process(&mut self, x: f64) -> f64 {
        let mut v = x;
        for s in self.stages.iter_mut() {
            v = s.process(v);
        }
        v
    }

    pub fn reset(&mut self) {
        for s in self.stages.iter_mut() {
            s.reset();
        }
    }
}
