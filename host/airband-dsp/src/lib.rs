//! Shared DSP for the Pluto airband host tools (`airband-listen`, `airband-reader`).
//!
//! Ports the audio-quality pieces of RTLSDR-Airband
//! (<https://github.com/rtl-airband/RTLSDR-Airband>, commit f8a17d7f) - per-channel
//! squelch, AM AGC, and a notch filter - alongside the existing voice band-pass.
//! All of it operates on the FPGA's already-demodulated, DC-blocked audio sample
//! (see `maia-hdl` `am_audio.py`), so there is no IQ/de-emphasis stage here.

mod agc;
mod denoise;
mod filter;
mod squelch;

pub use agc::Agc;
pub use denoise::Denoise;
pub use filter::{HighShelf, LowPass, Notch, VoiceFilter};
pub use squelch::{
    carrier_noise_threshold, Squelch, SquelchConfig, SquelchMode, SquelchState,
};

#[cfg(test)]
mod carrier_tests {
    use super::decode_carrier;

    #[test]
    fn carrier_decode_monotonic_and_zero() {
        assert_eq!(decode_carrier(0), 0.0);
        // exp increases -> value increases
        let a = decode_carrier(5 << 3);
        let b = decode_carrier(10 << 3);
        assert!(b > a && a > 0.0);
        // mantissa increases within an exponent
        let lo = decode_carrier(10 << 3);
        let hi = decode_carrier((10 << 3) | 7);
        assert!(hi > lo);
    }
}

/// 2**23 - full scale of the signed 24-bit audio sample carried in each record.
pub const SAMPLE_SCALE: f32 = 8_388_608.0;

/// Converts a linear amplitude in raw 24-bit sample units to dBFS
/// (`20*log10(level / 2**23)`), matching RTLSDR-Airband's `level_to_dBFS`.
///
/// Returns a finite floor (`-120.0`) for non-positive input instead of `-inf`.
pub fn level_to_dbfs(level: f32) -> f32 {
    let norm = level / SAMPLE_SCALE;
    if norm <= 0.0 {
        -120.0
    } else {
        (20.0 * norm.log10()).max(-120.0)
    }
}

/// Inverse of [`level_to_dbfs`]: a dBFS threshold to a raw linear amplitude.
pub fn dbfs_to_level(dbfs: f32) -> f32 {
    10f32.powf(dbfs / 20.0) * SAMPLE_SCALE
}

/// Decodes the 8-bit carrier-level minifloat carried in frame bits [31:24]
/// (see `hdl/audio_framer.py` / `hdl/am_backend_tdm.py`). Layout is
/// `[exp(5) | mant(3)]`; the approximate magnitude is `(8 + mant) << (exp - 4)`.
/// Returns `0.0` for a zero byte (no carrier / old bitstream).
pub fn decode_carrier(byte: u8) -> f32 {
    let exp = (byte >> 3) & 0x1f;
    let mant = (byte & 0x7) as f32;
    if exp == 0 {
        0.0
    } else {
        (8.0 + mant) * 2f32.powi(exp as i32 - 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbfs_roundtrip() {
        // full scale is 0 dBFS
        assert!((level_to_dbfs(SAMPLE_SCALE) - 0.0).abs() < 1e-3);
        // -6 dBFS is ~half scale
        let half = dbfs_to_level(-6.0206);
        assert!((half / SAMPLE_SCALE - 0.5).abs() < 1e-3);
        // round trip
        let lvl = dbfs_to_level(-30.0);
        assert!((level_to_dbfs(lvl) + 30.0).abs() < 1e-3);
    }

    #[test]
    fn dbfs_zero_is_floored() {
        assert_eq!(level_to_dbfs(0.0), -120.0);
        assert_eq!(level_to_dbfs(-5.0), -120.0);
    }
}
