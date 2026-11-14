//! Portable **parametric-EQ coefficient design** — the transcendental half of the
//! EQ, factored out of the desktop host so the DAP firmware reuses the exact same
//! pipeline.
//!
//! [`soul_audio_core`] only **applies** biquad coefficients (`Biquad`,
//! `Equalizer`) — a few multiply-adds per sample, strictly `no_std` / no-alloc /
//! libm-free. This crate does the **design**: turning a parametric [`Band`]
//! (`bell` / `shelf` / `cut` / `notch`, each with its own frequency, gain and Q)
//! into those coefficients via the RBJ "Audio EQ Cookbook", and evaluating the
//! magnitude response for a plot. It needs `sin`/`cos`/`pow`/`sqrt`/`log`, so it
//! pulls in [`libm`] — the one external dependency — and stays `no_std` and
//! no-alloc (every entry point works on borrowed slices and fixed arrays).
//!
//! Both heads build identical EQ behaviour on top:
//! - desktop (`soul-player-host`) wraps these in a `Sink` decorator;
//! - the DAP firmware calls [`design`] per band and feeds `Equalizer::set_band`.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::f32::consts::PI;

use soul_audio_core::Biquad;

/// Maximum number of parametric EQ bands a head should allocate for (the size of
/// the core filter cascade; unused slots run as bypass). Bands can be added up to
/// this, and an import past it is truncated.
pub const MAX_BANDS: usize = 16;

/// Gain limit (± dB) for each band and the preamp — heads clamp to this.
pub const GAIN_LIMIT_DB: f32 = 12.0;

/// Q (resonance / bandwidth) limits heads clamp to: very wide to surgical.
pub const Q_MIN: f32 = 0.3;
pub const Q_MAX: f32 = 12.0;

/// Displayed / draggable frequency range (Hz).
pub const FREQ_MIN: f32 = 20.0;
pub const FREQ_MAX: f32 = 20_000.0;

/// A band's filter shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandKind {
    /// Boost/cut everything below the corner (gain applies).
    LowShelf,
    /// Boost/cut a bell around the frequency (gain applies).
    Peaking,
    /// Boost/cut everything above the corner (gain applies).
    HighShelf,
    /// High-pass: attenuate below the corner (gain ignored).
    LowCut,
    /// Low-pass: attenuate above the corner (gain ignored).
    HighCut,
    /// Reject a narrow band around the frequency (gain ignored).
    Notch,
}

impl BandKind {
    /// Whether this shape uses the `gain_db` field. Cuts and notches don't.
    #[must_use]
    pub fn uses_gain(self) -> bool {
        matches!(self, Self::LowShelf | Self::Peaking | Self::HighShelf)
    }
}

/// One parametric band: its shape, corner/centre frequency (Hz), gain (dB, for the
/// gain shapes), Q, and whether it is engaged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub kind: BandKind,
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
    pub enabled: bool,
}

impl Band {
    /// Whether this band, as configured, actually changes the signal: an engaged
    /// gain shape with a non-trivial gain, or any engaged cut/notch.
    #[must_use]
    pub fn affects(&self) -> bool {
        self.enabled
            && self.freq > 0.0
            && (if self.kind.uses_gain() {
                self.gain_db.abs() > 1e-3
            } else {
                true
            })
    }
}

/// The default 8-band layout: a low shelf, six bells spread over the spectrum, and
/// a high shelf — all flat (0 dB), so the default EQ is a passthrough. Returned as
/// a fixed array (no-alloc); a head wraps it in whatever container it uses.
#[must_use]
pub fn default_bands() -> [Band; 8] {
    const LAYOUT: [(BandKind, f32, f32); 8] = [
        (BandKind::LowShelf, 80.0, 0.7),
        (BandKind::Peaking, 150.0, 1.0),
        (BandKind::Peaking, 350.0, 1.0),
        (BandKind::Peaking, 800.0, 1.0),
        (BandKind::Peaking, 2000.0, 1.0),
        (BandKind::Peaking, 4500.0, 1.0),
        (BandKind::Peaking, 9000.0, 1.0),
        (BandKind::HighShelf, 15_000.0, 0.7),
    ];
    LAYOUT.map(|(kind, freq, q)| Band {
        kind,
        freq,
        gain_db: 0.0,
        q,
        enabled: true,
    })
}

/// RBJ "Audio EQ Cookbook" biquad coefficients, already `a0`-normalised, as
/// `[b0, b1, b2, a1, a2]` — or `None` (a flat passthrough) for a band that cannot
/// or should not filter: out of range, non-positive Q, or a gain shape at ~0 dB.
///
/// Shelves are parameterised by Q (`alpha = sin(w0)/(2Q)`) rather than the
/// cookbook's shelf-slope S, the modern parametric-EQ convention (at `Q = 1/√2` it
/// matches the classic `S = 1` shelf). Shared by [`design`] (which applies the
/// coefficients) and [`band_magnitude_db`] (which evaluates them).
#[must_use]
pub fn coeffs(kind: BandKind, f0: f32, gain_db: f32, q: f32, fs: f32) -> Option<[f32; 5]> {
    if f0 <= 0.0 || f0 >= fs / 2.0 || q <= 0.0 {
        return None;
    }
    if kind.uses_gain() && gain_db.abs() < 1e-3 {
        return None; // a flat bell/shelf is a passthrough
    }
    let w0 = 2.0 * PI * f0 / fs;
    let (sin_w0, cos_w0) = (libm::sinf(w0), libm::cosf(w0));
    let alpha = sin_w0 / (2.0 * q);
    // `A` is the *amplitude* √(linear gain): exponent /40, not /20 (the classic
    // RBJ transcription gotcha).
    let a = libm::powf(10.0, gain_db / 40.0);

    let (b0, b1, b2, a0, a1, a2) = match kind {
        BandKind::Peaking => (
            1.0 + alpha * a,
            -2.0 * cos_w0,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos_w0,
            1.0 - alpha / a,
        ),
        BandKind::LowShelf => {
            let two_sqrt_a_alpha = 2.0 * libm::sqrtf(a) * alpha;
            (
                a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0),
                a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
                (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0),
                (a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
            )
        }
        BandKind::HighShelf => {
            let two_sqrt_a_alpha = 2.0 * libm::sqrtf(a) * alpha;
            (
                a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0),
                a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
                (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
                2.0 * ((a - 1.0) - (a + 1.0) * cos_w0),
                (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
            )
        }
        BandKind::LowCut => (
            (1.0 + cos_w0) / 2.0,
            -(1.0 + cos_w0),
            (1.0 + cos_w0) / 2.0,
            1.0 + alpha,
            -2.0 * cos_w0,
            1.0 - alpha,
        ),
        BandKind::HighCut => (
            (1.0 - cos_w0) / 2.0,
            1.0 - cos_w0,
            (1.0 - cos_w0) / 2.0,
            1.0 + alpha,
            -2.0 * cos_w0,
            1.0 - alpha,
        ),
        BandKind::Notch => (
            1.0,
            -2.0 * cos_w0,
            1.0,
            1.0 + alpha,
            -2.0 * cos_w0,
            1.0 - alpha,
        ),
    };
    Some([b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0])
}

/// The RBJ biquad for a band — the transcendental design the `no_std` core
/// delegates here. Flat ([`Biquad::bypass`]) for a disabled/flat/out-of-range band.
#[must_use]
pub fn design(band: &Band, fs: f32) -> Biquad {
    if !band.enabled {
        return Biquad::bypass();
    }
    match coeffs(band.kind, band.freq, band.gain_db, band.q, fs) {
        Some([b0, b1, b2, a1, a2]) => Biquad::new(b0, b1, b2, a1, a2),
        None => Biquad::bypass(),
    }
}

/// Magnitude response (dB) of a biquad `[b0, b1, b2, a1, a2]` at normalised angular
/// frequency `w` (0..π): `20·log₁₀(|H(e^jw)|)`.
fn biquad_mag_db(c: [f32; 5], w: f32) -> f32 {
    let (cw, sw) = (libm::cosf(w), libm::sinf(w));
    let (c2, s2) = (libm::cosf(2.0 * w), libm::sinf(2.0 * w));
    let num_re = c[0] + c[1] * cw + c[2] * c2;
    let num_im = -(c[1] * sw + c[2] * s2);
    let den_re = 1.0 + c[3] * cw + c[4] * c2;
    let den_im = -(c[3] * sw + c[4] * s2);
    let num = libm::sqrtf(num_re * num_re + num_im * num_im);
    let den = libm::sqrtf(den_re * den_re + den_im * den_im);
    if den <= 0.0 {
        0.0
    } else {
        20.0 * libm::log10f((num / den).max(1e-9))
    }
}

/// A single band's magnitude response (dB) at `freq` Hz for device rate `fs` — the
/// thin per-band curve a head draws. `0.0` (flat) for a band that doesn't filter.
#[must_use]
pub fn band_magnitude_db(band: &Band, freq: f32, fs: f32) -> f32 {
    if !band.enabled {
        return 0.0;
    }
    match coeffs(band.kind, band.freq, band.gain_db, band.q, fs) {
        Some(c) => biquad_mag_db(c, 2.0 * PI * freq / fs),
        None => 0.0,
    }
}

/// The **combined parametric-EQ magnitude response** (dB) at `freq` Hz for a device
/// rate `fs`: the preamp plus each band's response (cascaded filters multiply in
/// magnitude = add in dB). This is the overall curve a head draws.
#[must_use]
pub fn eq_magnitude_db(bands: &[Band], preamp_db: f32, freq: f32, fs: f32) -> f32 {
    let mut db = preamp_db;
    for band in bands {
        db += band_magnitude_db(band, freq, fs);
    }
    db
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = 48_000.0;

    fn bell(freq: f32, gain_db: f32, q: f32) -> Band {
        Band {
            kind: BandKind::Peaking,
            freq,
            gain_db,
            q,
            enabled: true,
        }
    }

    #[test]
    fn a_flat_or_out_of_range_band_is_bypass() {
        assert_eq!(design(&bell(1000.0, 0.0, 1.0), FS), Biquad::bypass());
        assert_eq!(
            design(&bell(16_000.0, 6.0, 1.0), 30_000.0),
            Biquad::bypass(),
            "a band at/above Nyquist can't be realised"
        );
        let mut disabled = bell(1000.0, 6.0, 1.0);
        disabled.enabled = false;
        assert_eq!(design(&disabled, FS), Biquad::bypass());
        assert_ne!(design(&bell(1000.0, 6.0, 1.0), FS), Biquad::bypass());
    }

    #[test]
    fn peaking_hits_its_gain_at_centre_and_is_flat_far_away() {
        let b = bell(1000.0, 6.0, 1.0);
        assert!((band_magnitude_db(&b, 1000.0, FS) - 6.0).abs() < 0.2);
        assert!(band_magnitude_db(&b, 125.0, FS).abs() < 1.0);
        assert!(band_magnitude_db(&b, 8000.0, FS).abs() < 1.0);
        let c = bell(1000.0, -6.0, 1.0);
        assert!((band_magnitude_db(&c, 1000.0, FS) + 6.0).abs() < 0.2);
    }

    #[test]
    fn q_controls_peaking_bandwidth() {
        let wide = band_magnitude_db(&bell(1000.0, 10.0, 0.5), 1400.0, FS);
        let narrow = band_magnitude_db(&bell(1000.0, 10.0, 6.0), 1400.0, FS);
        assert!(
            wide > narrow + 3.0,
            "a wider band (low Q) boosts 1.4 kHz far more: wide {wide:.2} narrow {narrow:.2}"
        );
    }

    #[test]
    fn shelves_lift_one_side_and_leave_the_other() {
        let low = Band {
            kind: BandKind::LowShelf,
            freq: 120.0,
            gain_db: 6.0,
            q: 0.7,
            enabled: true,
        };
        assert!((band_magnitude_db(&low, 30.0, FS) - 6.0).abs() < 0.6);
        assert!(band_magnitude_db(&low, 10_000.0, FS).abs() < 0.5);
        let high = Band {
            kind: BandKind::HighShelf,
            freq: 8000.0,
            gain_db: -6.0,
            q: 0.7,
            enabled: true,
        };
        assert!((band_magnitude_db(&high, 18_000.0, FS) + 6.0).abs() < 0.8);
        assert!(band_magnitude_db(&high, 200.0, FS).abs() < 0.5);
    }

    #[test]
    fn cuts_attenuate_the_stopband_and_pass_the_passband() {
        let hp = Band {
            kind: BandKind::LowCut,
            freq: 500.0,
            gain_db: 0.0,
            q: 0.707,
            enabled: true,
        };
        assert!(band_magnitude_db(&hp, 60.0, FS) < -15.0);
        assert!((band_magnitude_db(&hp, 500.0, FS) + 3.0).abs() < 1.0);
        assert!(band_magnitude_db(&hp, 8000.0, FS).abs() < 0.3);
        let lp = Band {
            kind: BandKind::HighCut,
            freq: 2000.0,
            gain_db: 0.0,
            q: 0.707,
            enabled: true,
        };
        assert!(band_magnitude_db(&lp, 16_000.0, FS) < -15.0);
        assert!(band_magnitude_db(&lp, 200.0, FS).abs() < 0.3);
    }

    #[test]
    fn notch_rejects_its_centre_and_passes_around_it() {
        let n = Band {
            kind: BandKind::Notch,
            freq: 1000.0,
            gain_db: 0.0,
            q: 4.0,
            enabled: true,
        };
        assert!(band_magnitude_db(&n, 1000.0, FS) < -30.0);
        assert!(band_magnitude_db(&n, 250.0, FS).abs() < 0.5);
        assert!(band_magnitude_db(&n, 4000.0, FS).abs() < 0.5);
    }

    #[test]
    fn combined_response_sums_bands_and_preamp() {
        let mut bands = default_bands();
        bands[4].gain_db = 6.0; // 2 kHz bell
        bands[0].gain_db = 3.0; // low shelf
        assert!((eq_magnitude_db(&bands, -2.0, 2000.0, FS) - 4.0).abs() < 0.6);
        assert!((eq_magnitude_db(&bands, -2.0, 30.0, FS) - 1.0).abs() < 0.8);
    }

    #[test]
    fn default_layout_is_a_flat_passthrough() {
        let bands = default_bands();
        assert!(
            !bands.iter().any(Band::affects),
            "every default band is flat"
        );
        assert!(eq_magnitude_db(&bands, 0.0, 1000.0, FS).abs() < 0.01);
    }
}
