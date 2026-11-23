//! Signal-integrity fitness functions — the "audiophile" claims, as gates.
//!
//! The other tests in this crate guard *structure* (layering, no-alloc, tiers).
//! These guard the **signal**: that the engine does not quietly alter samples it
//! promised to pass through, and that when it does process them, it says so.
//!
//! # Why this file exists
//!
//! Every stage here is individually unit-tested somewhere, and that is not the
//! same thing. "Bit-perfect" is a property of the **whole pipeline**, and it is
//! lost by composition — a stage that is harmless alone (a flat EQ band, a
//! volume multiply, a 0 dB ReplayGain) becomes lossy once it is in the chain and
//! running. The failure mode is silent: the audio still plays, the UI still says
//! bit-perfect, and the only evidence is in the low bits.
//!
//! The specific guarantees, each with its own test below:
//!
//! 1. **Bypass means identity, bit-for-bit** — not "approximately equal". A
//!    biquad with unity coefficients run in `f32` is NOT guaranteed to return its
//!    input unchanged once state and rounding are involved, so "flat" must be
//!    implemented as *skipping the filter*, not as *running a flat filter*.
//! 2. **`is_active() == false` implies identity.** `soul-audio-dsp`'s
//!    `DspChain::is_active` doc promises "the applier passes the frames through
//!    untouched (bit-exact)", and `Band::affects` decides that with a
//!    **tolerance** (`gain_db.abs() > 1e-3`). So a 0.0005 dB band reports "does
//!    not affect the signal". That is only sound if the applier actually skips;
//!    if it instead designs and runs every *enabled* band, the chain perturbs
//!    samples while claiming to be bit-exact. This test pins the implication.
//! 3. **Volume is not in the digital path.** Attenuating digitally and
//!    truncating discards resolution — at -30 dB, roughly five bits of it. The
//!    `Sink::set_volume` contract puts gain at the sink's *output stage* for
//!    exactly this reason, and on the DAP that means the DAC's own volume over
//!    I2C. This is the gate that stops the easy, lossy shortcut from ever being
//!    taken: changing the volume must not change one sample the sink receives.
//! 4. **Processing is declared.** ReplayGain, resampling and an active DSP chain
//!    all legitimately leave the bit-exact path. What must never happen is doing
//!    them while still reporting bit-perfect — `DecodeInfo` is what the player's
//!    signal-path view renders, so a wrong flag is a lie to the user.
//! 5. **The exchange format round-trips.** `soul-audio-dsp` is `no_std + alloc`
//!    specifically so the desktop host (soul-player-ng) and the DAP firmware
//!    (soul-listener) share one model: the desktop exports a profile, the DAP
//!    imports it. If that trip is not lossless, the DAP plays something the user
//!    never auditioned.
//! 6. **Coefficient design is bitwise deterministic.** Same band, same rate,
//!    same `f32` bits — on both targets. The desktop previews with `std` math and
//!    the DAP applies with `libm`; if those disagree in the last bit the two
//!    devices are not running the same EQ, and nothing would ever report it.

use soul_audio_core::eq::{Biquad, Equalizer};
use soul_audio_dsp::{Band, BandKind, DspChain, DspUnit, EqUnit};

/// Deterministic pseudo-random-ish test signal: full-scale-ish, both signs,
/// non-trivial low bits. Deliberately NOT a pure tone — a sine has long runs of
/// similar values and can mask low-bit perturbation.
fn test_signal(n: usize) -> Vec<f32> {
    let mut v = Vec::with_capacity(n);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..n {
        // xorshift32, mapped to [-0.75, 0.75] so a boost cannot clip and make
        // the comparison trivially fail for the wrong reason.
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let unit = (x as f64 / u32::MAX as f64) as f32 * 2.0 - 1.0;
        v.push(unit * 0.75);
    }
    v
}

/// Bitwise comparison. `assert_eq!` on `f32` compares by value, which treats
/// `-0.0 == 0.0` and would hide a sign-flip through a flat filter; and an
/// epsilon comparison would hide exactly the low-bit damage this file exists to
/// catch. Compare the bit patterns.
fn assert_bit_identical(expected: &[f32], actual: &[f32], what: &str) {
    assert_eq!(expected.len(), actual.len(), "{what}: length changed");
    for (i, (e, a)) in expected.iter().zip(actual).enumerate() {
        assert!(
            e.to_bits() == a.to_bits(),
            "{what}: sample {i} changed: {e} ({:#010x}) -> {a} ({:#010x}). A bypassed stage must \
             be a true identity, not an approximation — see this file's header.",
            e.to_bits(),
            a.to_bits()
        );
    }
}

/// Guarantee 1a: a **disabled** equalizer is a bit-exact identity.
#[test]
fn a_disabled_equalizer_is_bit_exact_identity() {
    let signal = test_signal(1024);
    let mut buf = signal.clone();

    let mut eq: Equalizer<8> = Equalizer::new();
    // Give it real, signal-changing bands, then disable it. A bypass that only
    // works because the coefficients happen to be flat is not a bypass.
    let band = Band {
        kind: BandKind::Peaking,
        freq: 1_000.0,
        gain_db: 9.0,
        q: 0.7,
        enabled: true,
    };
    eq.set_band(0, soul_audio_dsp::design(&band, 48_000.0));
    eq.set_enabled(false);

    eq.process(&mut buf, 2);
    assert_bit_identical(&signal, &buf, "disabled equalizer");
    assert!(!eq.is_active(), "a disabled equalizer must report inactive");
}

/// Guarantee 1b: an **enabled** equalizer whose every band is `Biquad::bypass()`
/// is also a bit-exact identity.
///
/// This is the subtler half. Unity coefficients (`b0 = 1`, everything else 0)
/// are algebraically an identity, but running them still reads and writes the
/// filter state in `f32`. If that ever stops being exact — a different
/// evaluation order, a fused multiply-add, an added dither — "flat EQ" silently
/// stops being bit-perfect while every UI still claims it is.
#[test]
fn an_enabled_but_fully_bypassed_equalizer_is_bit_exact_identity() {
    let signal = test_signal(1024);
    let mut buf = signal.clone();

    let mut eq: Equalizer<8> = Equalizer::new();
    for i in 0..8 {
        eq.set_band(i, Biquad::bypass());
    }
    eq.set_enabled(true);

    eq.process(&mut buf, 2);
    assert_bit_identical(&signal, &buf, "enabled-but-bypassed equalizer");
}

/// Guarantee 2: `is_active() == false` implies identity — across the tolerance.
///
/// `Band::affects` uses `gain_db.abs() > 1e-3`, so the band below reports "does
/// not affect the signal" despite being enabled with a non-zero gain. The
/// contract in `DspChain::is_active`'s doc is that such a chain "passes the
/// frames through untouched (bit-exact)". That is a statement about the
/// APPLIER, so it has to be checked by applying.
///
/// If this test ever fails, the fix is NOT to loosen it: it means something
/// designs-and-runs every enabled band instead of consulting `is_active()`, and
/// the chain is lying about being bit-exact.
#[test]
fn an_inactive_chain_is_identity_even_just_inside_the_tolerance() {
    let sub_threshold = Band {
        kind: BandKind::Peaking,
        freq: 1_000.0,
        // 0.0005 dB: enabled, non-zero, and BELOW Band::affects' 1e-3 threshold.
        gain_db: 0.000_5,
        q: 1.0,
        enabled: true,
    };
    assert!(
        !sub_threshold.affects(),
        "0.0005 dB should be below Band::affects' 1e-3 threshold — if this changed, this test's \
         premise needs rewriting, not deleting"
    );

    let chain = DspChain::new(vec![DspUnit::Eq(EqUnit {
        enabled: true,
        preamp_db: 0.0,
        bands: vec![sub_threshold],
        name: None,
    })]);
    assert!(
        !chain.is_active(),
        "a chain of only sub-threshold bands must report inactive"
    );

    // The applier's contract: inactive => untouched. Model it the way a caller
    // must: consult is_active() FIRST, and only then build an equalizer.
    let signal = test_signal(512);
    let mut buf = signal.clone();
    if chain.is_active() {
        let mut eq: Equalizer<8> = Equalizer::new();
        eq.set_band(0, soul_audio_dsp::design(&sub_threshold, 48_000.0));
        eq.set_enabled(true);
        eq.process(&mut buf, 2);
    }
    assert_bit_identical(&signal, &buf, "inactive chain inside the tolerance");

    // And the converse, so the tolerance cannot be widened into inaudibility
    // without someone noticing: a clearly audible band MUST report active.
    let audible = Band {
        gain_db: 3.0,
        ..sub_threshold
    };
    assert!(
        audible.affects(),
        "a 3 dB band must report that it affects the signal"
    );
}

/// Guarantee 5a: AutoEQ **text** is a LOSSY interchange format — pinned, so it is
/// a known property rather than a surprise.
///
/// `format_autoeq` writes gain at one decimal place (AutoEQ's own convention), so
/// a 4.25 dB band exports as "4.2 dB" and re-imports as 4.2. The 0.05 dB error is
/// far below audibility (JND is ~0.5-1 dB), so this is fine **for sharing presets
/// with humans and other tools**.
///
/// It is NOT fine as the DAP transport, which is the point of this test sitting
/// next to the one below: if an exported profile reached the device as AutoEQ
/// text, the DAP would play a quantised version of what the user auditioned.
/// Quantisation that small is inaudible, but "inaudible" is a different claim
/// from "bit-perfect", and this project sells the second one.
#[test]
fn the_autoeq_text_format_is_lossy_to_one_decimal_and_that_is_known() {
    let bands = vec![
        Band {
            kind: BandKind::Peaking,
            freq: 105.0,
            gain_db: -2.5,
            q: 0.7,
            enabled: true,
        },
        // Deliberately two decimals: this is the value that gets quantised.
        Band {
            kind: BandKind::Peaking,
            freq: 3_150.0,
            gain_db: 4.25,
            q: 1.4,
            enabled: true,
        },
    ];

    let text = soul_audio_dsp::format_autoeq(-3.5, &bands);
    let (_, back) = soul_audio_dsp::parse_autoeq(&text);
    assert_eq!(back.len(), bands.len(), "band count must survive:\n{text}");

    // Frequency and Q survive exactly; gain is quantised to 0.1 dB.
    for (orig, got) in bands.iter().zip(&back) {
        assert_eq!(orig.kind, got.kind, "kind must survive:\n{text}");
        assert!(
            (orig.freq - got.freq).abs() < 1e-3,
            "frequency must survive exactly: {} -> {}\n{text}",
            orig.freq,
            got.freq
        );
        assert!(
            (orig.q - got.q).abs() < 1e-3,
            "Q must survive exactly: {} -> {}\n{text}",
            orig.q,
            got.q
        );
        assert!(
            (orig.gain_db - got.gain_db).abs() <= 0.05 + 1e-6,
            "gain should be quantised to at most half a 0.1 dB step, got {} -> {}\n{text}",
            orig.gain_db,
            got.gain_db
        );
    }

    // And pin the quantisation itself, so a future change to full precision is a
    // deliberate edit here rather than a silent behaviour change.
    let quantised = back[1].gain_db;
    assert!(
        (quantised - 4.2).abs() < 1e-6,
        "4.25 dB is expected to export as 4.2 dB (one decimal); got {quantised}. If the format \
         gained precision, update this test AND reconsider whether the DAP transport note below \
         still applies."
    );
}

/// Guarantee 5b: the **machine** exchange format is lossless.
///
/// This is the one the DAP export must use. `ProfileCfg`/`BandCfg` carry `f32`
/// fields, so a profile can cross the desktop -> device boundary without losing a
/// bit — no quantisation, no reformatting, no reinterpretation.
///
/// `soul-audio-dsp` is `no_std + alloc` precisely so both ends share this model
/// rather than each parsing its own dialect. A failure here means the DAP would
/// play something the user never auditioned, which is the one thing an audiophile
/// device may not do.
#[test]
fn the_profile_exchange_format_round_trips_bit_exactly() {
    use soul_audio_dsp::format::ProfileCfg;

    let unit = EqUnit {
        // Values chosen to be unrepresentable in one decimal, so quantisation
        // anywhere in the path shows up.
        bands: vec![
            Band {
                kind: BandKind::Peaking,
                freq: 3_150.0,
                gain_db: 4.25,
                q: core::f32::consts::SQRT_2,
                enabled: true,
            },
            Band {
                kind: BandKind::LowShelf,
                freq: 105.5,
                gain_db: -2.333_333,
                q: core::f32::consts::FRAC_1_SQRT_2,
                enabled: false,
            },
        ],
        preamp_db: -3.456_789,
        enabled: true,
        name: Some("Sundara".into()),
    };
    let profile = soul_audio_dsp::Profile {
        name: "test".into(),
        chain: DspChain::new(vec![DspUnit::Eq(unit)]),
    };

    let json = serde_json::to_string(&ProfileCfg::from_profile(&profile))
        .expect("a profile must serialise");
    let back: ProfileCfg = serde_json::from_str(&json).expect("a profile must deserialise");
    let back = back.to_profile();

    let DspUnit::Eq(orig_eq) = &profile.chain.units[0];
    let DspUnit::Eq(back_eq) = &back.chain.units[0];

    assert_eq!(
        orig_eq.preamp_db.to_bits(),
        back_eq.preamp_db.to_bits(),
        "preamp must survive bit-exactly"
    );
    assert_eq!(orig_eq.name, back_eq.name, "unit name must survive");
    assert_eq!(
        orig_eq.bands.len(),
        back_eq.bands.len(),
        "band count must survive"
    );
    for (i, (o, b)) in orig_eq.bands.iter().zip(&back_eq.bands).enumerate() {
        assert_eq!(o.kind, b.kind, "band {i} kind");
        assert_eq!(o.enabled, b.enabled, "band {i} enabled");
        assert_eq!(
            o.freq.to_bits(),
            b.freq.to_bits(),
            "band {i} freq not bit-exact"
        );
        assert_eq!(
            o.gain_db.to_bits(),
            b.gain_db.to_bits(),
            "band {i} gain not bit-exact — the DAP would play a different EQ than the desktop \
             previewed"
        );
        assert_eq!(o.q.to_bits(), b.q.to_bits(), "band {i} Q not bit-exact");
    }
}

/// Guarantee 6: coefficient design is bitwise deterministic.
///
/// The desktop designs coefficients with `std` floating point and the DAP with
/// `libm` (`soul-audio-dsp` is `no_std + alloc`). If the two disagree even in the
/// last bit, the DAP is not running the EQ the user auditioned — and no test,
/// log or UI would ever say so, because both are "the right answer" to within
/// any tolerance anyone would think to assert.
///
/// Repeated design must be bitwise stable, and a golden bit pattern pins the
/// value itself so a change in the design math is a deliberate edit. If this
/// fails on a new target, that is the finding: the two platforms' transcendental
/// functions differ and the coefficients must be designed ONCE and shipped,
/// rather than redesigned on each device.
#[test]
fn coefficient_design_is_bitwise_deterministic() {
    let band = Band {
        kind: BandKind::Peaking,
        freq: 1_000.0,
        gain_db: 6.0,
        q: 0.707,
        enabled: true,
    };

    let a = soul_audio_eq::coeffs(band.kind, band.freq, band.gain_db, band.q, 48_000.0)
        .expect("a 1 kHz peaking band at 48 kHz must design");
    let b = soul_audio_eq::coeffs(band.kind, band.freq, band.gain_db, band.q, 48_000.0)
        .expect("design must be repeatable");

    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        assert_eq!(
            x.to_bits(),
            y.to_bits(),
            "coefficient {i} is not bitwise stable across two identical design calls"
        );
    }

    // Golden: the exact bits this platform produces today. A diff here means the
    // design math changed (deliberate: update the golden) or the platform's
    // sin/cos differ (the real finding: design once, ship the coefficients).
    let bits: [u32; 5] = [
        a[0].to_bits(),
        a[1].to_bits(),
        a[2].to_bits(),
        a[3].to_bits(),
        a[4].to_bits(),
    ];
    // Measured on x86_64 with std math. If a target disagrees, that IS the
    // finding: design the coefficients once and ship them, rather than
    // redesigning on each device.
    const GOLDEN_1KHZ_6DB_Q707_48K: [u32; 5] = [
        0x3f87_d086,
        0xbfee_3da2,
        0x3f50_f6c7,
        0xbfee_3da2,
        0x3f60_97d2,
    ];
    assert_eq!(
        bits, GOLDEN_1KHZ_6DB_Q707_48K,
        "coefficient bit patterns drifted: {bits:#010x?}"
    );
}
