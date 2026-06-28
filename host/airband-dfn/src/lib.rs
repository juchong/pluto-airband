//! DeepFilterNet speech-enhancement stage shared by the Pluto airband host tools
//! (`airband-listen`, `airband-reader`).
//!
//! DeepFilterNet (`deep_filter`'s `DfTract`) is a 48 kHz, hop-based neural noise
//! suppressor. Our audio is 20000 sps, so this wraps the model with streaming
//! linear resamplers (20000 -> 48000 in, 48000 -> 20000 out) and a small priming
//! buffer. The signal is already low-passed to <=2.5 kHz upstream, far below both
//! Nyquists, so linear interpolation introduces no audible error.
//!
//! It is intentionally driven only while the squelch is open (NN inference is
//! expensive and there is nothing to enhance in muted silence); call
//! [`DfnEnhancer::reset`] when the squelch closes.
//!
//! [`Presence`] is the post-DFN brightness lift (high-shelf + soft clip) used to
//! restore consonant energy DFN trims at low SNR; both binaries share it so all
//! outputs sound identical.

use airband_dsp::HighShelf;
use anyhow::Result;
use df::tract::DfTract;
use ndarray::Array2;
use std::collections::VecDeque;

/// Streaming linear resampler: push one input sample, emit the output samples
/// whose fractional time falls in the newly-available input interval.
struct LinResampler {
    step: f64, // input samples per output sample (= fs_in / fs_out)
    next_t: f64,
    in_t: i64, // index of the latest input sample (-1 = none yet)
    prev: f32,
    cur: f32,
}

impl LinResampler {
    fn new(fs_in: f64, fs_out: f64) -> LinResampler {
        LinResampler {
            step: fs_in / fs_out,
            next_t: 0.0,
            in_t: -1,
            prev: 0.0,
            cur: 0.0,
        }
    }

    fn push(&mut self, x: f32, out: &mut Vec<f32>) {
        self.prev = self.cur;
        self.cur = x;
        self.in_t += 1;
        // Emit outputs with time in [in_t-1, in_t): interpolate prev..cur.
        while self.next_t < self.in_t as f64 {
            let frac = (self.next_t - (self.in_t - 1) as f64) as f32; // [0,1)
            out.push(self.prev + frac * (self.cur - self.prev));
            self.next_t += self.step;
        }
    }

    fn reset(&mut self) {
        self.next_t = 0.0;
        self.in_t = -1;
        self.prev = 0.0;
        self.cur = 0.0;
    }
}

/// Tunable DeepFilterNet runtime parameters (applied to `DfTract` after load).
#[derive(Clone, Copy, Debug)]
pub struct DfnParams {
    /// Local-SNR floor in dB. DFN treats any frame whose estimated local SNR is
    /// below this as noise-only and applies a **zero mask** (mutes the hop). Its
    /// stock value (-10) chops quiet/low-SNR speech into silence frame-by-frame —
    /// the "garbled mumble" artifact — so we lower it to keep faint speech.
    pub min_snr_db: f32,
    /// Maximum noise attenuation in dB (>= 100 = unlimited). Lower values mix some
    /// of the noisy signal back in, trading suppression depth for fewer artifacts.
    pub atten_lim_db: f32,
    /// Post-filter beta (0 = off). A small value (~0.02) trims residual musical
    /// noise around the speech for a crisper result.
    pub pf_beta: f32,
}

impl Default for DfnParams {
    fn default() -> Self {
        DfnParams {
            min_snr_db: -20.0,
            atten_lim_db: 15.0,
            pf_beta: 0.02,
        }
    }
}

/// Bounds how many DeepFilterNet inferences may run concurrently across threads.
///
/// [`DfnEnhancer::process_sample_gated`] calls [`enter`](InferencePermit::enter)
/// immediately before each NN forward pass and [`leave`](InferencePermit::leave)
/// immediately after, so a caller can cap simultaneous inference (e.g. to the
/// number of CPU cores) without ever dropping or bypassing audio: a worker that
/// can't get a slot **blocks around that one hop only** — its input buffers and
/// the audio is delayed, not lost. Because the permit is scoped to the inference
/// (not the whole transmission), a continuously-keyed channel cannot starve the
/// others; permits free between hops and rotate among all open channels.
pub trait InferencePermit {
    /// Blocks until an inference slot is free. Called once per NN hop.
    fn enter(&self);
    /// Releases the inference slot. Called once per NN hop.
    fn leave(&self);
}

/// No-op permit: unbounded concurrency (used by [`DfnEnhancer::process_sample`]).
impl InferencePermit for () {
    #[inline]
    fn enter(&self) {}
    #[inline]
    fn leave(&self) {}
}

/// DeepFilterNet enhancer operating on a single mono stream at `native_rate`.
pub struct DfnEnhancer {
    df: DfTract,
    hop: usize,
    up: LinResampler,   // native -> 48 kHz
    down: LinResampler, // 48 kHz -> native
    in48: VecDeque<f32>,
    out_native: VecDeque<f32>,
    up_scratch: Vec<f32>,
    down_scratch: Vec<f32>,
    noisy: Array2<f32>,
    enh: Array2<f32>,
    cushion: usize,
    primed: bool,
    errored: bool,
}

impl DfnEnhancer {
    /// Loads the embedded DeepFilterNet3 model, applies `params`, and builds the
    /// resamplers.
    pub fn new(native_rate: f64, params: DfnParams) -> Result<DfnEnhancer> {
        let mut df = DfTract::default(); // bundled DFN3 model (default-model feature)
        // Keep faint speech instead of zero-gating it (the main "garble" fix), and
        // optionally enable the post-filter / cap attenuation. These are plain
        // public knobs / setters on DfTract.
        df.min_db_thresh = params.min_snr_db;
        df.set_atten_lim(params.atten_lim_db);
        df.set_pf_beta(params.pf_beta);
        let hop = df.hop_size;
        let sr = df.sr as f64;
        // ~2 DFN hops of native audio as a jitter cushion before steady output.
        let cushion = (2.0 * hop as f64 * native_rate / sr).ceil() as usize;
        Ok(DfnEnhancer {
            df,
            hop,
            up: LinResampler::new(native_rate, sr),
            down: LinResampler::new(sr, native_rate),
            in48: VecDeque::new(),
            out_native: VecDeque::new(),
            up_scratch: Vec::new(),
            down_scratch: Vec::new(),
            noisy: Array2::zeros((1, hop)),
            enh: Array2::zeros((1, hop)),
            cushion,
            primed: false,
            errored: false,
        })
    }

    /// True once the priming cushion has filled (steady-state output).
    #[cfg(test)]
    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// True if any model inference call returned an error.
    #[cfg(test)]
    pub fn errored(&self) -> bool {
        self.errored
    }

    /// Enhances one native-rate sample. Output is delayed by the model lookahead
    /// plus the priming cushion (~tens of ms); during priming it returns silence.
    pub fn process_sample(&mut self, x: f32) -> f32 {
        self.run(x, &())
    }

    /// Like [`process_sample`](Self::process_sample) but bounds concurrent NN
    /// inference via `permit`: each forward pass is wrapped in
    /// [`enter`](InferencePermit::enter)/[`leave`](InferencePermit::leave). A
    /// hop that can't get a slot blocks (buffering, never dropping); the permit
    /// is released between hops so no channel starves another.
    pub fn process_sample_gated<P: InferencePermit>(&mut self, x: f32, permit: &P) -> f32 {
        self.run(x, permit)
    }

    /// Shared streaming body: resample up, run any ready hops through the model
    /// (each gated by `permit`), resample down, and emit one buffered output.
    fn run<P: InferencePermit>(&mut self, x: f32, permit: &P) -> f32 {
        self.up_scratch.clear();
        self.up.push(x, &mut self.up_scratch);
        for &s in &self.up_scratch {
            self.in48.push_back(s);
        }
        while self.in48.len() >= self.hop {
            for i in 0..self.hop {
                self.noisy[[0, i]] = self.in48.pop_front().unwrap();
            }
            // Cap concurrent inference to the permit's slots, wrapping only the
            // NN forward pass (not the cheap resampling) so slots free quickly.
            permit.enter();
            let res = self.df.process(self.noisy.view(), self.enh.view_mut());
            permit.leave();
            // Ignore the returned LSNR; record (don't crash on) any inference error.
            if res.is_err() {
                self.errored = true;
            }
            self.down_scratch.clear();
            for i in 0..self.hop {
                self.down.push(self.enh[[0, i]], &mut self.down_scratch);
            }
            for &d in &self.down_scratch {
                self.out_native.push_back(d);
            }
        }
        if !self.primed {
            if self.out_native.len() >= self.cushion {
                self.primed = true;
            } else {
                return 0.0;
            }
        }
        self.out_native.pop_front().unwrap_or(0.0)
    }

    /// Clears streaming state (call when the squelch closes so the next
    /// transmission starts cleanly). The model's own state self-heals on silence.
    pub fn reset(&mut self) {
        self.up.reset();
        self.down.reset();
        self.in48.clear();
        self.out_native.clear();
        self.up_scratch.clear();
        self.down_scratch.clear();
        self.primed = false;
    }
}

/// Smooth bounded clip in (-1, 1): identity below 0.8, then a tanh knee, so a
/// presence-boosted peak can never clip the 16-bit output.
#[inline]
pub fn soft_clip(v: f32) -> f32 {
    let t = 0.8f32;
    let a = v.abs();
    if a <= t {
        v
    } else {
        v.signum() * (t + (1.0 - t) * ((a - t) / (1.0 - t)).tanh())
    }
}

/// Post-DFN presence (consonant) brightness lift: a high-shelf boost followed by
/// a soft clip. DFN over-attenuates the upper voice band (2-3.4 kHz) at low SNR,
/// making speech sound muffled; this restores that energy. Reset it while the
/// stream is idle so each transmission starts with clean filter state.
pub struct Presence {
    shelf: HighShelf,
}

impl Presence {
    /// High-shelf at `corner_hz`, transition `q`, `gain_db` boost, at `fs` Hz.
    pub fn new(fs: f64, corner_hz: f64, q: f64, gain_db: f64) -> Presence {
        Presence {
            shelf: HighShelf::new(fs, corner_hz, q, gain_db),
        }
    }

    /// Applies the brightness lift and soft clip to one sample.
    #[inline]
    pub fn process(&mut self, sample: f32) -> f32 {
        soft_clip(self.shelf.process(sample as f64) as f32)
    }

    /// Clears the IIR state (call when the stream goes idle).
    pub fn reset(&mut self) {
        self.shelf.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The bundled DFN3 model trips a tract 0.21.4 codegen dedup bug ("duplicate
    // name") only under the unoptimized (debug) profile; it loads fine in release,
    // which is the profile the host tools ship in. Skip in debug so `cargo test`
    // stays green; `cargo test --release` exercises the real model-load path.
    #[cfg_attr(debug_assertions, ignore)]
    #[test]
    fn dfn_model_loads_and_runs() {
        // Validates the embedded DFN3 model parses + runs under tract, and the
        // resample/hop pipeline primes without inference errors. (Output values
        // are not asserted: DFN correctly suppresses synthetic non-speech toward
        // zero; real enhancement is validated on live RF.)
        let mut e = DfnEnhancer::new(20000.0, DfnParams::default()).expect("DFN model should load");
        for i in 0..20000 {
            let x = (i as f32 * 0.02).sin() * 0.1;
            let _ = e.process_sample(x);
        }
        assert!(!e.errored(), "DFN inference returned an error");
        assert!(e.is_primed(), "pipeline never primed");
    }

    #[test]
    fn resampler_matches_target_rate() {
        // 20000 -> 48000 should emit ~ (48000/20000) outputs per input.
        let mut r = LinResampler::new(20000.0, 48000.0);
        let mut out = Vec::new();
        let n = 20000;
        for i in 0..n {
            let x = (i as f32 * 0.01).sin();
            r.push(x, &mut out);
        }
        let expected = n as f64 * 48000.0 / 20000.0;
        let err = (out.len() as f64 - expected).abs();
        assert!(err < 4.0, "got {} expected ~{:.0}", out.len(), expected);
    }

    #[test]
    fn soft_clip_is_bounded_and_identity_below_knee() {
        // Identity below the 0.8 knee.
        assert_eq!(soft_clip(0.5), 0.5);
        assert_eq!(soft_clip(-0.5), -0.5);
        // Compressive knee above 0.8 (0.9 -> ~0.89, still < input).
        assert!(soft_clip(0.9) > 0.8 && soft_clip(0.9) < 0.9);
        // Bounded to [-1, 1] for large input.
        assert!(soft_clip(10.0) <= 1.0 && soft_clip(10.0) > 0.8);
        assert!(soft_clip(-10.0) >= -1.0 && soft_clip(-10.0) < -0.8);
    }
}
