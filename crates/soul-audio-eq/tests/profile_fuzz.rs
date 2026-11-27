//! Deterministic mutation fuzzer for the **AutoEq profile path**: text a user
//! downloaded → parsed bands → designed biquads → audio.
//!
//! Invariant: for *any* profile text, `parse_autoeq` must not panic, and
//! everything it produces must be **finite** — the preamp, every designed
//! coefficient, and the samples that come out of running a signal through the
//! designed chain.
//!
//! Why finiteness, specifically, rather than "no panic": a profile is text from
//! the internet (AutoEq publishes per-headphone corrections; Equalizer-APO
//! users paste them around), and Rust's float parser accepts `NaN`, `inf` and
//! `1e40` without complaint. A NaN that reaches a biquad does not merely sound
//! wrong for one block: it multiplies into the delay line, and the delay line
//! is then NaN for every later sample, so playback stays dead after the profile
//! is corrected — until something calls `reset`. In the deck the NaN then meets
//! `as i32`, which yields `0`, so the symptom is **silence with no error**,
//! which is about the worst failure shape available.
//!
//! That is what this found: `coeffs`'s range checks (`f0 <= 0.0`,
//! `f0 >= fs / 2.0`, `q <= 0.0`) are each *false* for a NaN, so a NaN centre
//! frequency passed every guard and designed five NaN coefficients.
//!
//! House pattern: xorshift PRNG, seeded corpus, `IDX`-style env knobs
//! (`EQ_FUZZ_SEED`, `EQ_FUZZ_ITERS`).

use soul_audio_core::eq::{Biquad, Equalizer};
use soul_audio_eq::{Band, BandKind, coeffs, design, eq_magnitude_db, format_autoeq, parse_autoeq};

const CI_SEED: u64 = 0x4551_5F46_5A5A;
const CI_ITERS: u64 = 20_000;

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        // SplitMix64 then the nonzero guard, matching the other harnesses (a
        // bare `seed | 1` makes consecutive seeds the same corpus).
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Rng(z | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse().ok()
}

/// Numeric tokens: the plausible, the extreme, and the ones Rust's float parser
/// accepts but no filter can use.
const NUMBERS: &[&str] = &[
    "1000",
    "20",
    "20000",
    "0",
    "-0",
    "0.0001",
    "48000",
    "24000",
    "96000",
    "1.5",
    "-1.5",
    "6",
    "-6",
    "120",
    "-120",
    "0.1",
    "10",
    "NaN",
    "nan",
    "-NaN",
    "inf",
    "-inf",
    "infinity",
    "1e40",
    "-1e40",
    "1e-45",
    "340282350000000000000000000000000000000",
    "3.4e38",
    "",
    "-",
    ".",
    "1e",
    "0x10",
    "١٠٠٠",
    "1_000",
    "+1000",
];

/// Filter shape tokens, including unknown ones (which must be skipped).
const SHAPES: &[&str] = &[
    "PK", "LS", "HS", "LSC", "HSC", "LP", "HP", "PEQ", "XX", "", "ON",
];

/// A real AutoEq profile, for the unmutated baseline and as splice material.
const REAL: &str = "Preamp: -6.8 dB\n\
     Filter 1: ON PK Fc 21 Hz Gain 4.2 dB Q 0.72\n\
     Filter 2: ON PK Fc 105 Hz Gain -3.1 dB Q 1.10\n\
     Filter 3: ON PK Fc 2200 Hz Gain 2.4 dB Q 2.00\n\
     Filter 4: ON LSC Fc 105 Hz Gain -1.0 dB Q 0.70\n\
     Filter 5: OFF HSC Fc 10000 Hz Gain 1.5 dB Q 0.70\n";

/// Build one profile line from the token pools, so a mutation can land on any
/// field (shape, Fc, Gain, Q, the ON/OFF state, the units).
fn line(rng: &mut Rng, n: usize) -> String {
    let state = if rng.below(4) == 0 { "OFF" } else { "ON" };
    format!(
        "Filter {n}: {state} {} Fc {} Hz Gain {} dB Q {}",
        rng.pick(SHAPES),
        rng.pick(NUMBERS),
        rng.pick(NUMBERS),
        rng.pick(NUMBERS),
    )
}

fn generate(rng: &mut Rng) -> String {
    match rng.below(6) {
        // A synthetic profile built from the token pools.
        0..=2 => {
            let mut out = format!("Preamp: {} dB\n", rng.pick(NUMBERS));
            for n in 1..=rng.below(12) + 1 {
                out.push_str(&line(rng, n));
                out.push('\n');
            }
            out
        }
        // The real profile with one line replaced.
        3 => {
            let mut lines: Vec<String> = REAL.lines().map(str::to_owned).collect();
            let at = rng.below(lines.len());
            lines[at] = line(rng, at + 1);
            lines.join("\n")
        }
        // The real profile with bytes substituted (headers, units, separators).
        4 => {
            let mut v: Vec<u8> = REAL.as_bytes().to_vec();
            for _ in 0..rng.below(12) + 1 {
                let i = rng.below(v.len());
                // Keep it valid UTF-8: substitute printable ASCII only.
                v[i] = 0x20 + (rng.next() % 95) as u8;
            }
            String::from_utf8(v).expect("ascii substitution stays utf-8")
        }
        // Degenerate shapes.
        _ => {
            let pieces = [
                "",
                "\n",
                "Preamp:",
                "Preamp: NaN dB",
                "Filter",
                "Filter 1: ON",
                "Filter 1: ON PK Fc",
                "Filter 1: ON PK Fc NaN Hz Gain inf dB Q NaN",
                "Filter 1: ON PK Fc 1000 Hz Gain 6 dB Q 0",
                REAL,
            ];
            let mut out = String::new();
            for _ in 0..rng.below(4) + 1 {
                out.push_str(pieces[rng.below(pieces.len())]);
                out.push('\n');
            }
            out
        }
    }
}

/// Everything a host does with a profile: parse it, design every band at a
/// spread of device rates, draw the response curve, run audio through it, and
/// round-trip it back to text.
fn drive(text: &str) {
    let (preamp, bands) = parse_autoeq(text);
    assert!(
        preamp.is_finite(),
        "parsed a non-finite preamp from {text:?}"
    );

    for &fs in &[44_100.0f32, 48_000.0, 96_000.0, 192_000.0, 8_000.0] {
        // Every designed section must be usable. A non-finite coefficient here
        // is the bug this file exists for.
        for (i, band) in bands.iter().enumerate() {
            let bq = design(band, fs);
            assert!(
                bq.is_finite(),
                "band {i} ({:?} Fc {} Gain {} Q {}) designed a non-finite section at {fs} Hz",
                band.kind,
                band.freq,
                band.gain_db,
                band.q
            );
            if let Some(c) = coeffs(band.kind, band.freq, band.gain_db, band.q, fs) {
                assert!(
                    c.iter().all(|v| v.is_finite()),
                    "coeffs returned Some with a non-finite value: {c:?}"
                );
            }
        }

        // The curve a UI draws must be finite at every probe frequency.
        for &f in &[0.0f32, 1.0, 20.0, 1_000.0, 20_000.0, fs / 2.0, fs, fs * 2.0] {
            let db = eq_magnitude_db(&bands, preamp, f, fs);
            assert!(db.is_finite(), "response at {f} Hz is {db} (fs {fs})");
        }

        // And the audio: install the designed bands and push a signal through.
        // Output must stay finite — that is what a NaN coefficient destroys
        // permanently, since the delay line keeps the NaN.
        let mut eq = Equalizer::<8>::new();
        for (i, band) in bands.iter().take(8).enumerate() {
            eq.set_band(i, design(band, fs));
        }
        let mut buf: Vec<f32> = (0..256)
            .map(|n| if n % 32 == 0 { 0.5 } else { -0.25 })
            .collect();
        eq.process(&mut buf, 2);
        for (n, s) in buf.iter().enumerate() {
            assert!(
                s.is_finite(),
                "sample {n} is {s} after the designed EQ (fs {fs})"
            );
        }
    }

    // Formatting must not panic, and must re-parse (it is the share path).
    let text2 = format_autoeq(preamp, &bands);
    let (preamp2, _bands2) = parse_autoeq(&text2);
    assert!(preamp2.is_finite(), "round-tripped a non-finite preamp");
}

#[test]
fn no_profile_text_yields_a_non_finite_filter() {
    let seed = env_u64("EQ_FUZZ_SEED").unwrap_or(CI_SEED);
    let iters = env_u64("EQ_FUZZ_ITERS").unwrap_or(CI_ITERS);
    let mut rng = Rng::new(seed);

    drive(REAL);
    drive("");

    for i in 0..iters {
        let text = generate(&mut rng);
        // No `catch_unwind`: a panic here *is* the failure, and the text is
        // short enough to print in the message, so the default unwind report
        // (with the assertion's own detail) is more useful than a summary.
        if std::panic::catch_unwind(|| drive(&text)).is_err() {
            panic!("iteration {i} (seed {seed:#x}) failed on profile:\n{text}");
        }
    }
}

/// The specific hole the fuzzer found, named so a regression reads as a rule:
/// every range check in `coeffs` is false for a NaN, so non-finiteness has to
/// be rejected explicitly and first.
#[test]
fn a_non_finite_parameter_is_refused_rather_than_designed() {
    let fs = 48_000.0;
    for (f0, gain, q) in [
        (f32::NAN, 6.0, 1.0),
        (1000.0, f32::NAN, 1.0),
        (1000.0, 6.0, f32::NAN),
        (f32::INFINITY, 6.0, 1.0),
        (1000.0, f32::INFINITY, 1.0),
        (1000.0, 6.0, f32::INFINITY),
        (f32::NEG_INFINITY, 6.0, 1.0),
    ] {
        assert_eq!(
            coeffs(BandKind::Peaking, f0, gain, q, fs),
            None,
            "coeffs({f0}, {gain}, {q}) must be refused"
        );
        let band = Band {
            kind: BandKind::Peaking,
            freq: f0,
            gain_db: gain,
            q,
            enabled: true,
        };
        assert_eq!(
            design(&band, fs),
            Biquad::bypass(),
            "a non-finite band must design to bypass"
        );
    }
    // A non-finite device rate is refused too.
    assert_eq!(coeffs(BandKind::Peaking, 1000.0, 6.0, 1.0, f32::NAN), None);

    // And the parser treats such a value as absent rather than carrying it in.
    let (preamp, bands) =
        parse_autoeq("Preamp: NaN dB\nFilter 1: ON PK Fc NaN Hz Gain inf dB Q NaN\n");
    assert_eq!(preamp, 0.0, "a non-finite preamp falls back to 0 dB");
    assert!(
        bands
            .iter()
            .all(|b| b.freq.is_finite() && b.gain_db.is_finite() && b.q.is_finite()),
        "parse must not emit non-finite band parameters: {bands:?}"
    );
}

/// The core filter refuses a non-finite section even when a host builds the
/// coefficients itself, because the delay line would keep the NaN forever.
#[test]
fn the_filter_refuses_a_non_finite_section_at_the_door() {
    let mut eq = Equalizer::<8>::new();
    eq.set_band(0, Biquad::new(f32::NAN, 0.0, 0.0, 0.0, 0.0));
    assert_eq!(
        eq.band(0),
        Some(Biquad::bypass()),
        "a non-finite section must be installed as bypass"
    );

    let mut buf = vec![0.25f32; 64];
    eq.process(&mut buf, 2);
    assert!(buf.iter().all(|s| s.is_finite()), "output went non-finite");

    // Infinities too, in every coefficient slot.
    for i in 0..5 {
        let mut c = [0.0f32; 5];
        c[i] = f32::INFINITY;
        let mut eq = Equalizer::<8>::new();
        eq.set_band(0, Biquad::new(c[0], c[1], c[2], c[3], c[4]));
        assert_eq!(eq.band(0), Some(Biquad::bypass()), "slot {i}");
    }
}

#[test]
fn generator_is_deterministic_and_seeds_do_not_collide() {
    let fingerprint = |seed: u64| -> Vec<(usize, u64)> {
        let mut rng = Rng::new(seed);
        (0..128)
            .map(|_| {
                let t = generate(&mut rng);
                let h = t.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, x| {
                    (h ^ u64::from(x)).wrapping_mul(0x0000_0100_0000_01B3)
                });
                (t.len(), h)
            })
            .collect()
    };
    assert_eq!(fingerprint(CI_SEED), fingerprint(CI_SEED));
    for base in [CI_SEED, 0, 1, 2, 1000] {
        assert_ne!(
            fingerprint(base),
            fingerprint(base + 1),
            "seeds {base} and {} generate the same inputs",
            base + 1
        );
    }
}
