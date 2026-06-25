//! Voice band-pass and notch filters for airband AM audio.
//!
//! Airband voice occupies ~300-3400 Hz. The Pluto front-end picks up RF-borne
//! interference outside that band (a low ~90/330 Hz hum and, on some channels, a
//! ~7.6 kHz whine just under Nyquist), which the host makeup gain then amplifies
//! into an audible buzz. A 4th-order Butterworth band-pass (cascaded RBJ biquads:
//! two high-pass + two low-pass) removes the out-of-band buzz while leaving voice
//! intelligibility intact.
//!
//! The [`Notch`] is a direct port of RTLSDR-Airband's `NotchFilter`
//! (`src/filters.cpp`) - a 2nd-order IIR band-stop, good for killing a narrow
//! tonal spur in the audio band.

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

    /// RBJ cookbook 2nd-order high-shelf at `f0` (Hz), quality `q`, gain `db`.
    /// Used to de-droop the FPGA audio-CIC roll-off (boosts the high voice band).
    fn high_shelf(f0: f64, fs: f64, q: f64, db: f64) -> Biquad {
        let a = 10f64.powf(db / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let (cos_w0, sin_w0) = (w0.cos(), w0.sin());
        let alpha = sin_w0 / (2.0 * q);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        Biquad::norm(
            a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0),
            a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
            (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
            2.0 * ((a - 1.0) - (a + 1.0) * cos_w0),
            (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
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

/// 4th-order Butterworth band-pass (two high-pass + two low-pass biquads) plus a
/// fixed high-shelf that de-droops the FPGA audio-CIC roll-off so the widened
/// voice band stays flat to the top.
#[derive(Clone)]
pub struct VoiceFilter {
    stages: [Biquad; 5],
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
                // de-droop the order-4 audio CIC (~-4.2 dB @6 kHz at 21875 sps):
                // gentle high-shelf, +4.5 dB above ~3 kHz.
                Biquad::high_shelf(3000.0, fs, 0.5, 4.5),
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

/// 2nd-order IIR notch (band-stop), ported from RTLSDR-Airband `NotchFilter`
/// (`src/filters.cpp`, based on <https://www.dsprelated.com/showcode/173.php>).
#[derive(Clone)]
pub struct Notch {
    d0: f64,
    d1: f64,
    d2: f64,
    x: [f64; 3],
    y: [f64; 3],
}

impl Notch {
    /// Notch at `notch_freq` (Hz) with selectivity `q` at sample rate `fs` (Hz).
    pub fn new(notch_freq: f64, fs: f64, q: f64) -> Notch {
        let wo = 2.0 * PI * (notch_freq / fs);
        let e = 1.0 / (1.0 + (wo / (q * 2.0)).tan());
        let p = wo.cos();
        Notch {
            d0: e,
            d1: 2.0 * e * p,
            d2: 2.0 * e - 1.0,
            x: [0.0; 3],
            y: [0.0; 3],
        }
    }

    #[inline]
    pub fn process(&mut self, value: f64) -> f64 {
        self.x[0] = self.x[1];
        self.x[1] = self.x[2];
        self.x[2] = value;

        self.y[0] = self.y[1];
        self.y[1] = self.y[2];
        self.y[2] = self.d0 * self.x[2] - self.d1 * self.x[1] + self.d0 * self.x[0]
            + self.d1 * self.y[1]
            - self.d2 * self.y[0];

        self.y[2]
    }

    pub fn reset(&mut self) {
        self.x = [0.0; 3];
        self.y = [0.0; 3];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// RMS of a pure tone at `freq` after passing through a closure-built filter.
    fn tone_rms(freq: f64, fs: f64, mut f: impl FnMut(f64) -> f64) -> f64 {
        let n = 8000;
        // discard a startup transient, then measure
        let mut acc = 0.0;
        for i in 0..n {
            let x = (2.0 * PI * freq * i as f64 / fs).sin();
            let y = f(x);
            if i >= n / 2 {
                acc += y * y;
            }
        }
        (acc / (n as f64 / 2.0)).sqrt()
    }

    #[test]
    fn bandpass_passes_voice_rejects_out_of_band() {
        let fs = 15625.0;
        let mut mid = VoiceFilter::new(fs, 300.0, 3400.0);
        let mid_rms = tone_rms(1000.0, fs, |x| mid.process(x));
        let mut low = VoiceFilter::new(fs, 300.0, 3400.0);
        let low_rms = tone_rms(60.0, fs, |x| low.process(x));
        let mut high = VoiceFilter::new(fs, 300.0, 3400.0);
        let high_rms = tone_rms(7000.0, fs, |x| high.process(x));
        // ~0.707 for a passband tone, near zero for out-of-band
        assert!(mid_rms > 0.6, "1 kHz should pass (rms {mid_rms})");
        assert!(low_rms < 0.1, "60 Hz should be rejected (rms {low_rms})");
        assert!(high_rms < 0.1, "7 kHz should be rejected (rms {high_rms})");
    }

    #[test]
    fn notch_rejects_tone_and_passes_others() {
        let fs = 15625.0;
        let f0 = 1000.0;
        let mut at = Notch::new(f0, fs, 10.0);
        let at_rms = tone_rms(f0, fs, |x| at.process(x));
        let mut away = Notch::new(f0, fs, 10.0);
        let away_rms = tone_rms(2500.0, fs, |x| away.process(x));
        assert!(at_rms < 0.2, "tone at notch freq should be attenuated (rms {at_rms})");
        assert!(away_rms > 0.6, "tone away from notch should pass (rms {away_rms})");
    }
}
