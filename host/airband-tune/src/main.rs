//! Offline tuning harness for the airband enhancement chain.
//!
//! Runs a mono 16-bit WAV recording through the same post-squelch DSP the Pi's
//! `airband-reader` applies to an open channel — optional AGC, then DeepFilterNet,
//! then the presence high-shelf — with every DFN/presence knob exposed on the
//! command line. The flags mirror `airband-reader` one-for-one, so a setting that
//! sounds right here can be copied straight into the reader's `ExecStart`.
//!
//! It cannot reproduce the *carrier squelch* (that keys on the FPGA carrier byte,
//! which a WAV does not carry) — feed it audio that is already squelch-gated, i.e.
//! a capture of a channel's transmissions. `--start`/`--dur` render a sub-clip for
//! fast A/B iteration before committing to a long file.

use airband_dfn::{DfnEnhancer, DfnParams, Presence};
use airband_dsp::Agc;
use anyhow::{bail, Context, Result};
use clap::Parser;

/// Enhance a mono WAV through the airband DFN + presence chain (offline tuning).
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Input WAV (mono, 16-bit PCM, any sample rate).
    input: String,
    /// Output WAV (mono, 16-bit PCM, same sample rate as the input).
    output: String,
    /// Start offset into the input, in seconds.
    #[arg(long, default_value_t = 0.0)]
    start: f64,
    /// Length to process, in seconds (0 = to end of file).
    #[arg(long, default_value_t = 0.0)]
    dur: f64,
    /// Run the reader's AM AGC before DFN (off by default; leave off for a
    /// recording that is already level-normalized, e.g. a LiveATC/Icecast capture).
    #[arg(long)]
    agc: bool,
    /// DeepFilterNet local-SNR floor in dB (frames below are treated as noise).
    #[arg(long, default_value_t = -20.0, allow_hyphen_values = true)]
    dfn_min_snr: f32,
    /// DeepFilterNet max attenuation in dB (>=100 = unlimited). Lower recovers
    /// faint syllables but lets more broadband hiss back in.
    #[arg(long, default_value_t = 15.0)]
    dfn_atten_lim: f32,
    /// DeepFilterNet post-filter beta (0 = off; ~0.02 trims residual musical noise).
    #[arg(long, default_value_t = 0.02)]
    dfn_pf_beta: f32,
    /// Post-DFN presence/brightness high-shelf boost in dB (0 = off).
    #[arg(long, default_value_t = 8.0, allow_hyphen_values = true)]
    presence_db: f64,
    /// Presence high-shelf corner frequency in Hz.
    #[arg(long, default_value_t = 1600.0)]
    presence_hz: f64,
    /// Presence high-shelf transition Q.
    #[arg(long, default_value_t = 0.707)]
    presence_q: f64,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mut reader = hound::WavReader::open(&args.input)
        .with_context(|| format!("opening {}", args.input))?;
    let spec = reader.spec();
    if spec.channels != 1 || spec.bits_per_sample != 16 {
        bail!(
            "expected mono 16-bit PCM, got {} ch / {}-bit",
            spec.channels,
            spec.bits_per_sample
        );
    }
    let rate = spec.sample_rate;

    let all: Vec<f32> = reader
        .samples::<i16>()
        .map(|s| s.map(|v| v as f32 / 32768.0))
        .collect::<Result<_, _>>()
        .context("reading samples")?;
    let s0 = ((args.start * rate as f64) as usize).min(all.len());
    let n = if args.dur > 0.0 {
        (args.dur * rate as f64) as usize
    } else {
        all.len()
    };
    let input = &all[s0..(s0 + n).min(all.len())];
    eprintln!(
        "{} Hz | clip {:.1}s..{:.1}s ({} samples) | atten={} pf={} min_snr={} presence={}dB@{}Hz/Q{} agc={}",
        rate,
        args.start,
        args.start + input.len() as f64 / rate as f64,
        input.len(),
        args.dfn_atten_lim,
        args.dfn_pf_beta,
        args.dfn_min_snr,
        args.presence_db,
        args.presence_hz,
        args.presence_q,
        args.agc,
    );

    let mut dfn = DfnEnhancer::new(
        rate as f64,
        DfnParams {
            min_snr_db: args.dfn_min_snr,
            atten_lim_db: args.dfn_atten_lim,
            pf_beta: args.dfn_pf_beta,
        },
    )?;
    let mut presence =
        (args.presence_db != 0.0).then(|| Presence::new(rate as f64, args.presence_hz, args.presence_q, args.presence_db));
    let mut agc = args.agc.then(Agc::new);

    // Feed a quarter-second of trailing zeros to drain DFN's lookahead/priming tail.
    let flush = rate as usize / 4;
    let mut out = Vec::with_capacity(input.len() + flush);
    let mut run = |x: f32, first: bool| {
        let mut v = x;
        if let Some(g) = agc.as_mut() {
            v = g.process(v, true, first, true);
        }
        v = dfn.process_sample(v);
        if let Some(p) = presence.as_mut() {
            v = p.process(v);
        }
        out.push(v);
    };
    for (i, &x) in input.iter().enumerate() {
        run(x, i == 0);
    }
    for _ in 0..flush {
        run(0.0, false);
    }

    let mut writer = hound::WavWriter::create(
        &args.output,
        hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .with_context(|| format!("creating {}", args.output))?;
    for y in out {
        writer.write_sample((y * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    eprintln!("wrote {}", args.output);
    Ok(())
}
