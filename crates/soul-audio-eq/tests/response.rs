//! EQ frequency-response FIDELITY (host-only; uses `rustfft`).
//!
//! The parametric bands, as **applied to audio** (designed into
//! [`soul_audio_core::Equalizer`] and run over a unit impulse), must match the
//! **analytical** magnitude the crate reports for the UI plot
//! ([`band_magnitude_db`]) — so the head never draws a curve different from what the
//! ears get. Each band type must also show its characteristic shape (bell / shelf /
//! cut / notch) with accurate gain and Q, and apply without NaN/blow-up at extremes.
//!
//! Measured response: impulse → `Equalizer::process` → FFT → `|H(f)|` (the impulse's
//! spectrum is flat at 1.0, so the output spectrum IS the transfer function).

use rustfft::{FftPlanner, num_complex::Complex};
use soul_audio_core::Equalizer;
use soul_audio_eq::{Band, BandKind, band_magnitude_db, design};

const FS: f32 = 48_000.0;
const N: usize = 16_384;

fn band(kind: BandKind, freq: f32, gain_db: f32, q: f32) -> Band {
    Band {
        kind,
        freq,
        gain_db,
        q,
        enabled: true,
    }
}

/// Measured magnitude response (dB) of `band` at the FFT bin nearest `freq`: design the
/// biquad, apply it to a unit impulse via the real `Equalizer`, FFT, read `|H|` at the
/// bin. Returns `(bin_freq, response_db)` so the caller can evaluate the analytical
/// response at the SAME frequency the bin represents (exact comparison, no bin offset).
fn measured_response(band: &Band, freq: f32) -> (f32, f32) {
    let mut eq: Equalizer<1> = Equalizer::new();
    eq.set_band(0, design(band, FS));
    let mut buf = vec![0.0f32; N];
    buf[0] = 1.0;
    eq.process(&mut buf, 1);
    let mut c: Vec<Complex<f32>> = buf.iter().map(|&re| Complex { re, im: 0.0 }).collect();
    FftPlanner::new().plan_fft_forward(N).process(&mut c);
    let bin = ((freq / FS) * N as f32).round() as usize;
    let bin_freq = bin as f32 * FS / N as f32;
    (bin_freq, 20.0 * c[bin].norm().log10())
}

/// THE centerpiece: for every band type, the APPLIED filter response must equal the
/// ANALYTICAL magnitude the crate plots — within a tight tolerance where the response is
/// shallow, and "also deep" where it's a stopband (the f32 FFT noise floor caps how deep
/// a null reads). A divergence here means the UI curve lies about the audible result.
#[test]
fn applied_response_matches_the_analytical_plot_for_every_band_type() {
    let bands = [
        band(BandKind::Peaking, 1_000.0, 6.0, 2.0),
        band(BandKind::Peaking, 3_000.0, -8.0, 4.0),
        band(BandKind::LowShelf, 200.0, 6.0, 0.7),
        band(BandKind::HighShelf, 5_000.0, -6.0, 0.7),
        band(BandKind::LowCut, 100.0, 0.0, 0.707),
        band(BandKind::HighCut, 10_000.0, 0.0, 0.707),
        band(BandKind::Notch, 1_000.0, 0.0, 4.0),
    ];
    let freqs = [
        50.0, 100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0, 15_000.0,
    ];
    for b in &bands {
        for &f in &freqs {
            let (bf, measured) = measured_response(b, f);
            let analytical = band_magnitude_db(b, bf, FS);
            if analytical < -40.0 {
                assert!(
                    measured < -30.0,
                    "{:?} @ {bf:.0} Hz: analytical {analytical:.1} dB (deep) but applied only {measured:.1} dB",
                    b.kind
                );
            } else {
                assert!(
                    (measured - analytical).abs() < 0.6,
                    "{:?} @ {bf:.0} Hz: APPLIED {measured:.2} dB != ANALYTICAL plot {analytical:.2} dB",
                    b.kind
                );
            }
        }
    }
}

/// Each band type shows its defining shape. (Uses the analytical response, which the
/// test above proves equals the applied filter.)
#[test]
fn each_band_type_has_its_characteristic_shape() {
    // Peaking: a bell — boosts fc, flat far away.
    let pk = band(BandKind::Peaking, 1_000.0, 6.0, 2.0);
    assert!((band_magnitude_db(&pk, 1_000.0, FS) - 6.0).abs() < 0.3);
    assert!(band_magnitude_db(&pk, 100.0, FS).abs() < 0.5);
    assert!(band_magnitude_db(&pk, 10_000.0, FS).abs() < 0.5);

    // Low shelf: plateau below the corner, flat above.
    let ls = band(BandKind::LowShelf, 200.0, 6.0, 0.7);
    assert!(
        (band_magnitude_db(&ls, 40.0, FS) - 6.0).abs() < 0.6,
        "low-shelf plateau"
    );
    assert!(
        band_magnitude_db(&ls, 4_000.0, FS).abs() < 0.3,
        "low-shelf flat above"
    );

    // High shelf: flat below, plateau above.
    let hs = band(BandKind::HighShelf, 4_000.0, 6.0, 0.7);
    assert!(
        band_magnitude_db(&hs, 200.0, FS).abs() < 0.3,
        "high-shelf flat below"
    );
    assert!(
        (band_magnitude_db(&hs, 16_000.0, FS) - 6.0).abs() < 0.6,
        "high-shelf plateau"
    );

    // Low cut (high-pass): ~0 dB in the passband, deep below, ~-3 dB at the corner.
    let lc = band(BandKind::LowCut, 500.0, 0.0, 0.707);
    assert!(
        band_magnitude_db(&lc, 5_000.0, FS).abs() < 0.3,
        "low-cut passes highs"
    );
    assert!(
        band_magnitude_db(&lc, 100.0, FS) < -10.0,
        "low-cut attenuates lows"
    );
    assert!(
        (band_magnitude_db(&lc, 500.0, FS) + 3.0).abs() < 1.0,
        "low-cut ~-3 dB at corner"
    );

    // High cut (low-pass): ~0 dB in the passband, deep above.
    let hc = band(BandKind::HighCut, 2_000.0, 0.0, 0.707);
    assert!(
        band_magnitude_db(&hc, 200.0, FS).abs() < 0.3,
        "high-cut passes lows"
    );
    assert!(
        band_magnitude_db(&hc, 12_000.0, FS) < -10.0,
        "high-cut attenuates highs"
    );

    // Notch: a deep null at fc, flat away.
    let nt = band(BandKind::Notch, 1_000.0, 0.0, 8.0);
    assert!(
        band_magnitude_db(&nt, 1_000.0, FS) < -20.0,
        "notch deep null"
    );
    assert!(
        band_magnitude_db(&nt, 250.0, FS).abs() < 0.5,
        "notch flat below"
    );
    assert!(
        band_magnitude_db(&nt, 4_000.0, FS).abs() < 0.5,
        "notch flat above"
    );
}

/// Gain accuracy across the full ±12 dB range, for both a bell and a shelf.
#[test]
fn peaking_and_shelf_gain_accuracy_across_the_range() {
    for &g in &[-12.0f32, -9.0, -6.0, -3.0, 3.0, 6.0, 9.0, 12.0] {
        let pk = band(BandKind::Peaking, 1_000.0, g, 2.0);
        let at_fc = band_magnitude_db(&pk, 1_000.0, FS);
        assert!(
            (at_fc - g).abs() < 0.3,
            "peaking {g:+} dB reads {at_fc:.2} dB at fc"
        );

        let ls = band(BandKind::LowShelf, 150.0, g, 0.7);
        let plateau = band_magnitude_db(&ls, 30.0, FS);
        assert!(
            (plateau - g).abs() < 0.6,
            "low-shelf {g:+} dB plateau reads {plateau:.2} dB"
        );
    }
}

/// Q controls bandwidth: a high-Q bell is far narrower than a low-Q one at the same gain.
#[test]
fn peaking_q_controls_bandwidth() {
    let fc = 1_000.0f32;
    let wide = band(BandKind::Peaking, fc, 6.0, 0.7);
    let narrow = band(BandKind::Peaking, fc, 6.0, 8.0);
    // One octave above fc: the wide band still boosts; the narrow one is back to ~flat.
    let wide_oct = band_magnitude_db(&wide, fc * 2.0, FS);
    let narrow_oct = band_magnitude_db(&narrow, fc * 2.0, FS);
    assert!(
        wide_oct > narrow_oct + 1.5,
        "higher Q must be narrower: wide {wide_oct:.2} dB vs narrow {narrow_oct:.2} dB one octave up"
    );
    assert!(
        narrow_oct.abs() < 1.0,
        "a Q=8 bell should be ~flat one octave out ({narrow_oct:.2} dB)"
    );
}

/// Extreme bands (max gain/Q, corners near DC and Nyquist) must APPLY without producing
/// NaN/Inf or blowing up — the stability guard the filter must never regress.
#[test]
fn extreme_bands_apply_without_nan_or_blowup() {
    let extremes = [
        band(BandKind::Peaking, 20.0, 12.0, 10.0),
        band(BandKind::Peaking, 23_000.0, 12.0, 10.0),
        band(BandKind::LowShelf, 20.0, 12.0, 0.1),
        band(BandKind::HighShelf, 23_000.0, -12.0, 2.0),
        band(BandKind::Notch, 50.0, 0.0, 20.0),
        band(BandKind::LowCut, 20.0, 0.0, 5.0),
        band(BandKind::HighCut, 23_500.0, 0.0, 5.0),
    ];
    for b in &extremes {
        let mut eq: Equalizer<1> = Equalizer::new();
        eq.set_band(0, design(b, FS));
        // A near-full-scale two-tone drive for a second of audio.
        let mut buf: Vec<f32> = (0..48_000)
            .map(|n| ((n as f32 * 0.37).sin() + (n as f32 * 0.011).sin()) * 4_000_000.0)
            .collect();
        eq.process(&mut buf, 1);
        assert!(
            buf.iter().all(|x| x.is_finite()),
            "{:?}: produced non-finite output",
            b.kind
        );
        let peak = buf.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(peak < 1e12, "{:?}: output blew up (peak {peak:e})", b.kind);
    }
}
