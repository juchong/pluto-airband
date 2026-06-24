//! Shared DSP for the Pluto airband host tools (`airband-listen`, `airband-reader`).
//!
//! Ports the audio-quality pieces of RTLSDR-Airband
//! (<https://github.com/rtl-airband/RTLSDR-Airband>, commit f8a17d7f) - per-channel
//! squelch, AM AGC, and a notch filter - alongside the existing voice band-pass.
//! All of it operates on the FPGA's already-demodulated, DC-blocked audio sample
//! (see `maia-hdl` `am_audio.py`), so there is no IQ/de-emphasis stage here.

mod agc;
mod filter;
mod squelch;

pub use agc::Agc;
pub use filter::{Notch, VoiceFilter};
pub use squelch::{Squelch, SquelchConfig, SquelchMode, SquelchState};

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
