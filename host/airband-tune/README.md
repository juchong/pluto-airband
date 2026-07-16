# airband-tune

Offline WAV enhancer for A/B tuning the airband **DeepFilterNet + presence**
chain on recordings.

It runs a mono recording through the exact post-squelch DSP that
[`airband-reader`](../airband-reader) applies to a live open channel — optional
AGC → DeepFilterNet → presence high-shelf — with every DFN/presence knob exposed
on the command line. The flags **mirror `airband-reader` one-for-one**, so a
setting that sounds right here can be pasted straight into the reader's
`ExecStart`.

## Build

```bash
# Release is required: the bundled DFN3 model only loads under an optimized build.
cargo build --release --manifest-path host/Cargo.toml -p airband-tune
```

## Usage

```bash
airband-tune <input.wav> <output.wav> [flags]
```

Input must be **mono, 16-bit PCM** (any sample rate; the enhancer resamples to
DFN's 48 kHz internally and writes the output back at the input rate).

Render a short clip for fast iteration, then commit to the full file:

```bash
# 159 s clip starting at 26:31, tuned for intelligibility over a hissy source
airband-tune capture.wav out.wav \
  --start 1591 --dur 159 \
  --dfn-atten-lim 9 --presence-db 12 --presence-hz 1300
```

### Flags

| Flag | Default | Effect |
|---|---|---|
| `--start <s>` | 0 | Start offset into the input (seconds). |
| `--dur <s>` | 0 (to end) | Length to process (seconds). |
| `--agc` | off | Run the reader's AM AGC before DFN. Leave off for already-leveled captures (e.g. a LiveATC/Icecast recording). |
| `--dfn-atten-lim <dB>` | 15 | Max DFN attenuation. Lower recovers faint syllables but lets more broadband hiss back in. |
| `--dfn-pf-beta <b>` | 0.02 | DFN post-filter (0 = off; ~0.02 trims residual musical noise). |
| `--dfn-min-snr <dB>` | -20 | DFN local-SNR mute floor. Lower keeps fainter speech (already effectively open at -20 on typical airband audio). |
| `--presence-db <dB>` | 8 | Post-DFN high-shelf boost that rebuilds the upper-voice band DFN rolls off. 0 = off. |
| `--presence-hz <Hz>` | 1600 | Presence shelf corner. Lower brightens more of the 1.3–3.4 kHz intelligibility band. |
| `--presence-q <Q>` | 0.707 | Presence shelf transition Q. |

## Limitations

- **No carrier squelch.** That stage keys on the FPGA carrier byte, which a WAV
  does not carry. Feed audio that is already squelch-gated (a capture of a
  channel's transmissions).
- **Tuning is source-dependent.** A recording captured post-Icecast is
  band-limited to 22.05 kHz; the live Pi processes native 20 kHz demod audio, so
  presence corner/gain may land slightly differently on-air. Confirm with an A/B
  on a live channel before shipping settings to the Pi.

## No dedicated test

This crate is thin glue: it wires WAV I/O to `DfnEnhancer` → `Presence` → `Agc`,
which all live in `airband-dfn`/`airband-dsp` and carry their own tests (the DFN
model load/run, resampler ratios, soft-clip bounds, AGC normalization, filter
responses). The only logic here — sample scaling and clip slicing — fails loudly
and visibly on any real run, and the DFN3 model only loads under a release build,
so a round-trip WAV test would duplicate covered code while adding a
release-only, model-dependent fixture. Not worth the maintenance.
