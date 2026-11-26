//! **Crossfeed** for headphone listening: bleed a filtered copy of each channel
//! into the other, relaxing the hard left/right separation that headphones
//! impose and speakers never do.
//!
//! On speakers both ears hear both drivers — delayed by the width of your head
//! and shadowed by it at high frequencies. Headphones deliver each channel to
//! one ear alone, which is why hard-panned 1960s stereo is fatiguing on them.
//! Crossfeed reinstates the two cues that matter: an **interaural time
//! difference** (~250 µs, the time sound takes to get round a head) and a **head
//! shadow** (the far ear hears mostly low frequencies, since highs diffract
//! poorly). This is the bs2b / Meier / Linkwitz family of designs.
//!
//! # The structure, and why this one
//!
//! Each channel gets the filtered *difference* between the channels mixed in:
//!
//! ```text
//! out_L = x_L + (strength/2) · lowpass(x_R − x_L)
//! out_R = x_R + (strength/2) · lowpass(x_L − x_R)
//! ```
//!
//! a single one-pole lowpass, no explicit delay. In mid/side terms the **mid
//! passes untouched** and only the side signal is filtered:
//!
//! ```text
//! S → (1 − strength·LP(f)) · S
//! ```
//!
//! Three properties follow, and they are the reason for this shape rather than
//! the simpler "add a lowpassed copy of the other channel":
//!
//! - **Mono content is a bit-exact passthrough.** With `x_L == x_R` the two
//!   filter states stay equal and their difference is identically zero, so
//!   centred material comes out as the same samples — not merely "about the
//!   same level". A naive crossfeed adds a lowpassed copy of an identical
//!   signal, colouring every mono mix (bass up, treble down by several dB):
//!   engaging it sounds like a tone control. (Precisely: exact for any
//!   *history* that is mono, including from a [`DspStage::reset`]. Stereo
//!   content leaves the two states asymmetric, and that asymmetry decays with
//!   the filter's ~0.2 ms time constant once the signal centres — which is the
//!   correct behaviour, not a caveat: the crossfeed is still ringing out.)
//! - **It never boosts any frequency.** A one-pole lowpass has the exact
//!   relation `|LP| = cos θ` for its own phase lag `θ`, so
//!   `|1 − s·LP|² = 1 − s·(2 − s)·cos²θ ≤ 1` for any `s` in `0..=1`: the side
//!   gain is at or below unity at *every* frequency, and the mid is exactly
//!   unity. So no output normalization is needed, and — unlike bs2b — there is
//!   no level drop to compensate for.
//!
//!   The one caveat, which this design shares with every crossfeed of this
//!   shape (bs2b included): a *transient* in pure anti-phase content can
//!   overshoot, because the filter's step response lags the direct path. The
//!   bound is `1 + strength` (the L1 norm of `δ − s·LP`), reached only by
//!   full-scale anti-phase square-wave edges — content with no mid at all. Real
//!   music's side signal sits far below full scale, and the deck's existing
//!   output clamp catches the pathological case; a listener who wants headroom
//!   guarantees on such material can trim with the EQ stage's preamp.
//! - **The ITD comes free, from the same filter.** A one-pole's group delay at
//!   low frequencies is `1/(2π·fc)` — 227 µs at 700 Hz, 245 µs at 650 Hz,
//!   squarely in the physiological range. Adding an *explicit* delay on top (as
//!   a naive "ITD + shadow" reading of the physics suggests) would break the
//!   per-frequency bound above: past roughly `1/(4·delay)` the delayed
//!   difference arrives in anti-phase and the side signal is genuinely
//!   *boosted* — around 2 dB at these settings, measurably, at steady state —
//!   which combs the image as well as eating headroom. That is why bs2b has no
//!   delay element either.
//!
//! So `strength` is the fraction of the **low-frequency side signal** folded
//! toward the centre — exactly, in the limit below the cutoff, where `LP → 1`
//! and the side gain is `|1 − strength|`: `0.6` leaves 40 % of it, `1.0` folds
//! the bass fully to mono (the "keep the bass mono" mastering rule, applied at
//! the crossfeed corner). Nearer the corner the filter's phase lag leaves a
//! little more than that in place. Above the cutoff the side signal is left alone, so the
//! stage narrows the image where the ear cannot localize anyway and leaves the
//! air intact.
//!
//! `no_std`, no-alloc, libm-free: the one-pole coefficient needs `exp`, so this
//! carries a small bit-trick/Taylor `exp` in the house style of
//! [`crate::deck`]'s `pow10_db` and `formats::aac::sbr`'s `pow2`, cross-checked
//! against `std` in the tests.

use super::{DspStage, StageKind};

/// Crossfeed is a stereo effect: there is no "other channel" to cross in mono,
/// and a multichannel mix carries its own spatialization.
const STEREO: usize = 2;

/// Cutoff bounds, Hz. Below ~100 Hz the "shadow" covers nothing; above ~2 kHz
/// it is no longer a head shadow (and the implied ITD drops under 80 µs).
const FC_MIN: f32 = 100.0;
const FC_MAX: f32 = 2_000.0;

/// How strong the crossfeed is: a (cutoff, strength) pair, where `strength` is
/// the fraction of the low-frequency side signal folded to the centre.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preset {
    /// Subtle: 700 Hz, 40 %. Takes the edge off hard-panned material with the
    /// image essentially as mixed.
    Light,
    /// The default: 700 Hz, 60 % — a speaker-like image without obviously
    /// narrowing the stage.
    Moderate,
    /// Strong: 650 Hz, 80 %. The Meier end of the family: clearly narrower,
    /// most forgiving of extreme 1960s stereo.
    Strong,
}

impl Preset {
    /// `(cutoff Hz, strength 0..=1)`.
    #[must_use]
    pub fn params(self) -> (f32, f32) {
        match self {
            Preset::Light => (700.0, 0.4),
            Preset::Moderate => (700.0, 0.6),
            Preset::Strong => (650.0, 0.8),
        }
    }
}

/// `e^-t` for `t >= 0`, without `std`/`libm`.
///
/// `e^-t = 2^(-t·log2 e)`: split the exponent into its integer part (built by
/// direct IEEE-754 exponent-field construction) and a fraction in `[0,1)`
/// evaluated as the truncated Taylor series of `2^f = e^{f·ln2}` (coefficients
/// `(ln2)^k / k!`, accurate to under `1e-5` across the interval) — the same
/// construction as [`crate::deck`]'s `pow10_db`. Underflows to `0.0` for large
/// `t` rather than producing a denormal exponent, and returns `1.0` for a
/// non-positive or NaN argument (a unity pole, not a NaN factory).
fn exp_neg(t: f32) -> f32 {
    use core::f32::consts::{LN_2, LOG2_E};
    if t.is_nan() || t <= 0.0 {
        return 1.0;
    }
    let y = -t * LOG2_E;
    if y <= -126.0 {
        return 0.0;
    }
    // floor without `f32::floor` (std-only): the cast truncates toward zero.
    let trunc = y as i32;
    let ip = if (trunc as f32) > y { trunc - 1 } else { trunc };
    let f = y - ip as f32; // [0, 1)
    let p = 1.0
        + f * (LN_2
            + f * (0.240_226_5
                + f * (0.055_504_11
                    + f * (0.009_618_129
                        + f * (0.001_333_356 + f * (0.000_154_035 + f * 0.000_015_253))))));
    f32::from_bits(((ip + 127) as u32) << 23) * p
}

/// The crossfeed stage (see the module docs): one one-pole lowpass per channel,
/// mixed complementarily, so mono is bit-exact and no frequency is boosted.
///
/// Starts **disabled** at zero strength, i.e. inactive — adding it to a chain
/// changes nothing until it is configured and enabled.
#[derive(Clone, Copy, Debug)]
pub struct CrossfeedStage {
    /// Head-shadow cutoff, Hz.
    cutoff_hz: f32,
    /// Fraction of the low-frequency side signal folded to the centre, `0..=1`.
    strength: f32,
    enabled: bool,

    /// The sample rate the coefficients below were designed for; `0` means
    /// "not designed yet". Processing at a different rate redesigns them, so a
    /// native-rate device reopen cannot leave the filter stale.
    designed_rate: u32,
    /// One-pole lowpass: `y[n] = a0·x[n] + b1·y[n-1]` (`a0 = 1 − b1`, unity at
    /// DC).
    a0: f32,
    b1: f32,
    /// `strength / 2`: the difference is added to one channel and subtracted
    /// from the other, so each side carries half.
    mix: f32,
    /// Lowpass state, one per channel.
    lp: [f32; STEREO],
}

impl Default for CrossfeedStage {
    fn default() -> Self {
        Self::new()
    }
}

impl CrossfeedStage {
    /// A disabled, zero-strength stage (inactive).
    #[must_use]
    pub fn new() -> Self {
        Self {
            cutoff_hz: 700.0,
            strength: 0.0,
            enabled: false,
            designed_rate: 0,
            a0: 1.0,
            b1: 0.0,
            mix: 0.0,
            lp: [0.0; STEREO],
        }
    }

    /// Configure from a [`Preset`] and enable the stage.
    pub fn set_preset(&mut self, preset: Preset) {
        let (cutoff, strength) = preset.params();
        self.set_params(cutoff, strength);
        self.enabled = true;
    }

    /// Set the parameters directly: `cutoff_hz` is clamped to 100 Hz..2 kHz
    /// (outside that it is not a head shadow any more) and `strength` to
    /// `0..=1` — that cap being what keeps the side gain at or under unity at
    /// every frequency. NaN is treated as "unset" rather than
    /// propagating. Coefficients are (re)designed on the next block, at
    /// whatever rate the chain is running; the filter state survives, so a
    /// parameter change mid-playback does not click.
    pub fn set_params(&mut self, cutoff_hz: f32, strength: f32) {
        self.cutoff_hz = if cutoff_hz.is_nan() {
            700.0
        } else {
            cutoff_hz.clamp(FC_MIN, FC_MAX)
        };
        self.strength = if strength.is_nan() {
            0.0
        } else {
            strength.clamp(0.0, 1.0)
        };
        self.designed_rate = 0; // force a redesign
    }

    /// Enable/disable without losing the configuration. A disabled stage is
    /// inactive, so the chain stays bit-exact.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Whether the stage is enabled (it is only *active* if it is also
    /// configured to a non-zero strength — see [`DspStage::is_active`]).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The configured `(cutoff Hz, strength)`.
    #[must_use]
    pub fn params(&self) -> (f32, f32) {
        (self.cutoff_hz, self.strength)
    }

    /// The interaural time difference this configuration implies, in
    /// microseconds — the one-pole's low-frequency group delay `1/(2π·fc)`,
    /// which is where the ITD comes from (see the module docs). ~227 µs at
    /// 700 Hz; for a signal-path display.
    #[must_use]
    pub fn itd_us(&self) -> f32 {
        1e6 / (2.0 * core::f32::consts::PI * self.cutoff_hz)
    }

    /// Design the one-pole for `rate`.
    fn design(&mut self, rate: u32) {
        let sr = rate.max(1) as f32;
        let b1 = exp_neg(2.0 * core::f32::consts::PI * self.cutoff_hz / sr);
        self.b1 = b1;
        self.a0 = 1.0 - b1;
        self.mix = self.strength * 0.5;
        self.designed_rate = rate;
    }
}

impl DspStage for CrossfeedStage {
    /// Crossfeed a stereo block in place. A non-stereo block passes through
    /// untouched: there is no other channel to cross in mono, and a
    /// multichannel mix carries its own spatialization.
    fn process(&mut self, buf: &mut [f32], channels: usize, sample_rate: u32) {
        if !self.is_active() || channels != STEREO {
            return;
        }
        if self.designed_rate != sample_rate {
            self.design(sample_rate);
        }
        let (a0, b1, mix) = (self.a0, self.b1, self.mix);
        for frame in buf.chunks_exact_mut(STEREO) {
            let (l, r) = (frame[0], frame[1]);
            // Head shadow, per channel.
            self.lp[0] = a0 * l + b1 * self.lp[0];
            self.lp[1] = a0 * r + b1 * self.lp[1];
            // Mix in the filtered *difference*: each ear gains the far
            // channel's shadow and loses its own. For mono the two lowpass
            // states are identical, so this is exactly zero and mono stays
            // bit-identical (see the module docs).
            let cross = mix * (self.lp[1] - self.lp[0]);
            frame[0] = l + cross;
            frame[1] = r - cross;
        }
    }

    /// Active only when enabled **and** at a non-zero strength: a disabled or
    /// zero-strength stage must cost neither the chain's bit-exact bypass nor
    /// the path's bit-perfect claim.
    fn is_active(&self) -> bool {
        self.enabled && self.strength > 0.0
    }

    fn reset(&mut self) {
        self.lp = [0.0; STEREO];
    }

    fn kind(&self) -> StageKind {
        StageKind::Crossfeed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn stage(preset: Preset) -> CrossfeedStage {
        let mut s = CrossfeedStage::new();
        s.set_preset(preset);
        s
    }

    fn interleave(l: &[f32], r: &[f32]) -> Vec<f32> {
        l.iter().zip(r).flat_map(|(&a, &b)| [a, b]).collect()
    }

    fn deinterleave(buf: &[f32], ch: usize) -> Vec<f32> {
        buf.iter().skip(ch).step_by(2).copied().collect()
    }

    /// A sine at `hz`, `n` samples at [`SR`].
    fn tone(hz: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / SR as f32;
                (2.0 * core::f32::consts::PI * hz * t).sin()
            })
            .collect()
    }

    /// RMS of the second half (past the filter's settling transient).
    fn rms_tail(x: &[f32]) -> f32 {
        let tail = &x[x.len() / 2..];
        let sum: f32 = tail.iter().map(|v| v * v).sum();
        (sum / tail.len() as f32).sqrt()
    }

    fn mid(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(x, y)| (x + y) * 0.5).collect()
    }
    fn side(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(x, y)| (x - y) * 0.5).collect()
    }

    #[test]
    fn exp_neg_matches_std_exp() {
        for t in [0.0f32, 0.01, 0.0997, 0.5, 1.0, 2.5, 10.0, 40.0] {
            let got = exp_neg(t);
            let want = (-t).exp();
            assert!(
                (got - want).abs() <= 1e-5 * want.max(1e-6),
                "exp_neg({t}) = {got}, std = {want}"
            );
        }
        // Degenerate inputs are a unity pole, not NaN.
        assert_eq!(exp_neg(0.0), 1.0);
        assert_eq!(exp_neg(-1.0), 1.0);
        assert_eq!(exp_neg(f32::NAN), 1.0);
        // Far underflow is zero, not a garbage exponent.
        assert_eq!(exp_neg(1e9), 0.0);
    }

    #[test]
    fn a_new_or_disabled_or_zero_strength_stage_is_an_exact_passthrough() {
        let input = interleave(&tone(440.0, 256), &tone(1_000.0, 256));

        let mut s = CrossfeedStage::new();
        assert!(!s.is_active(), "a fresh stage is inactive");
        let mut buf = input.clone();
        s.process(&mut buf, 2, SR);
        assert_eq!(buf, input, "a fresh stage must not touch the block");

        let mut s = stage(Preset::Strong);
        assert!(s.is_active());
        s.set_enabled(false);
        assert!(!s.is_active());
        let mut buf = input.clone();
        s.process(&mut buf, 2, SR);
        assert_eq!(buf, input, "a disabled stage must not touch the block");

        let mut s = stage(Preset::Strong);
        s.set_params(700.0, 0.0);
        assert!(!s.is_active());
        let mut buf = input.clone();
        s.process(&mut buf, 2, SR);
        assert_eq!(buf, input, "zero strength must not touch the block");
    }

    #[test]
    fn non_stereo_blocks_pass_through() {
        let mut s = stage(Preset::Moderate);
        for channels in [1usize, 4, 6] {
            let input: Vec<f32> = (0..240).map(|i| (i % 7) as f32 - 3.0).collect();
            let mut buf = input.clone();
            s.process(&mut buf, channels, SR);
            assert_eq!(buf, input, "{channels}-channel block must pass through");
        }
    }

    /// Mono content is a **bit-exact** passthrough at every strength: from a
    /// symmetric state (a fresh stage, or any mono history) the two filter
    /// states stay equal and their difference is identically zero. A naive
    /// crossfeed (add a lowpassed copy of the other channel) fails this
    /// outright — it tilts every mono mix by several dB.
    #[test]
    fn mono_content_is_bit_exact_at_every_strength() {
        for preset in [Preset::Light, Preset::Moderate, Preset::Strong] {
            let mut s = stage(preset);
            let n = 4_096;
            // Broadband-ish, not a pure tone: a tone could pass by luck.
            let mono: Vec<f32> = (0..n)
                .map(|i| {
                    let t = i as f32 / SR as f32;
                    let w = |hz: f32| (2.0 * core::f32::consts::PI * hz * t).sin();
                    0.4 * w(500.0) + 0.3 * w(3_000.0) + 0.2 * w(11_000.0)
                })
                .collect();
            let input = interleave(&mono, &mono);
            let mut buf = input.clone();
            s.process(&mut buf, 2, SR);
            assert_eq!(
                buf, input,
                "{preset:?}: mono content must be bit-identical, sample for sample"
            );
        }
    }

    /// The symmetry that mono-exactness rests on is *maintained*, not just
    /// initial: a long mono run stays bit-exact block after block, so a quiet
    /// centred passage cannot drift.
    #[test]
    fn a_long_mono_run_stays_bit_exact_block_after_block() {
        let mut s = stage(Preset::Strong);
        for block in 0..16 {
            let n = 256;
            let mono: Vec<f32> = (0..n)
                .map(|i| {
                    let t = (block * n + i) as f32 / SR as f32;
                    0.6 * (2.0 * core::f32::consts::PI * 220.0 * t).sin()
                })
                .collect();
            let input = interleave(&mono, &mono);
            let mut buf = input.clone();
            s.process(&mut buf, 2, SR);
            assert_eq!(buf, input, "block {block} of a mono run must be exact");
        }
    }

    /// The headline behaviour: a hard-panned channel appears in the other, and
    /// the direct channel loses exactly what crossed over (complementary).
    #[test]
    fn a_hard_panned_channel_bleeds_into_the_other() {
        let mut s = stage(Preset::Moderate);
        let n = 4_096;
        let left = tone(300.0, n);
        let mut buf = interleave(&left, &vec![0.0f32; n]);
        s.process(&mut buf, 2, SR);
        let (out_l, out_r) = (deinterleave(&buf, 0), deinterleave(&buf, 1));
        assert!(
            rms_tail(&out_r) > 0.05,
            "the left channel must audibly bleed into the right: {}",
            rms_tail(&out_r)
        );
        assert!(
            rms_tail(&out_r) < rms_tail(&out_l),
            "the crossed copy must be quieter than the direct signal"
        );
        assert!(
            rms_tail(&out_l) < rms_tail(&left),
            "the direct channel must lose what crossed over"
        );
    }

    /// The head shadow: a high-frequency hard-panned tone crosses far less
    /// than a low-frequency one. Without the lowpass this ratio would be flat.
    #[test]
    fn the_crossed_copy_is_lowpassed() {
        let crossed_ratio = |hz: f32| {
            let mut s = stage(Preset::Moderate);
            let n = 8_192;
            let mut buf = interleave(&tone(hz, n), &vec![0.0; n]);
            s.process(&mut buf, 2, SR);
            rms_tail(&deinterleave(&buf, 1)) / rms_tail(&deinterleave(&buf, 0))
        };
        let low = crossed_ratio(200.0);
        let high = crossed_ratio(8_000.0);
        assert!(
            low > high * 8.0,
            "200 Hz must cross far more than 8 kHz (head shadow): {low:.4} vs {high:.4}"
        );
    }

    /// The mid/side consequence on real stereo content: the **mid** survives
    /// untouched, and the **low side** signal is folded by the configured
    /// strength — `strength` means exactly that, at low frequencies.
    #[test]
    fn the_mid_is_preserved_and_the_low_side_is_folded_by_the_strength() {
        let n = 16_384;
        for strength in [0.4f32, 0.6, 0.8, 1.0] {
            let mut s = CrossfeedStage::new();
            s.set_params(700.0, strength);
            s.set_enabled(true);
            // A 40 Hz side-only component (hard left) over a centred 1.5 kHz
            // tone. 40 Hz is far below the 700 Hz corner, so |LP| ≈ 0.998 and
            // the predicted side gain is |1 − strength| — the claim is exact
            // only in that limit (nearer the corner the filter's phase lag
            // leaves more of the side signal in place).
            let centre = tone(1_500.0, n);
            let left_only = tone(40.0, n);
            let l: Vec<f32> = centre
                .iter()
                .zip(&left_only)
                .map(|(c, x)| 0.4 * c + 0.4 * x)
                .collect();
            let r: Vec<f32> = centre.iter().map(|c| 0.4 * c).collect();

            let mut buf = interleave(&l, &r);
            s.process(&mut buf, 2, SR);
            let (out_l, out_r) = (deinterleave(&buf, 0), deinterleave(&buf, 1));

            // The mid is untouched, to f32 rounding.
            let (mid_in, mid_out) = (mid(&l, &r), mid(&out_l, &out_r));
            for (i, (a, b)) in mid_in.iter().zip(&mid_out).enumerate() {
                assert!(
                    (a - b).abs() < 1e-6,
                    "strength {strength}: the mid must be untouched (frame {i}: {a} vs {b})"
                );
            }
            // The side is folded by |1 − strength|, within 0.06 (40 Hz is not
            // quite DC).
            let got = rms_tail(&side(&out_l, &out_r)) / rms_tail(&side(&l, &r));
            let want = 1.0 - strength;
            assert!(
                (got - want).abs() < 0.06,
                "strength {strength}: low side gain {got:.3}, expected ≈ {want:.3}"
            );
        }
    }

    /// The bound that lets the stage run without output normalization: the side
    /// gain never exceeds unity **at any frequency**, so the output cannot
    /// exceed full scale. Swept across the audible range at every strength —
    /// this is exactly what an added explicit ITD delay would break (it boosts
    /// the side signal above the cutoff; see the module docs).
    #[test]
    fn the_side_gain_never_exceeds_unity_at_any_frequency() {
        let n = 8_192;
        for strength in [0.25f32, 0.5, 0.75, 1.0] {
            for hz in [
                20.0f32, 50.0, 100.0, 200.0, 400.0, 700.0, 1_000.0, 1_700.0, 3_000.0, 6_000.0,
                12_000.0, 20_000.0,
            ] {
                let mut s = CrossfeedStage::new();
                s.set_params(700.0, strength);
                s.set_enabled(true);
                // Pure side signal: antiphase at `hz`.
                let sig = tone(hz, n);
                let anti: Vec<f32> = sig.iter().map(|v| -v).collect();
                let mut buf = interleave(&sig, &anti);
                s.process(&mut buf, 2, SR);
                let (out_l, out_r) = (deinterleave(&buf, 0), deinterleave(&buf, 1));
                let gain = rms_tail(&side(&out_l, &out_r)) / rms_tail(&side(&sig, &anti));
                assert!(
                    gain <= 1.001,
                    "strength {strength} at {hz} Hz: side gain {gain:.4} exceeds unity — the \
                     stage would boost (and could clip)"
                );
            }
        }
    }

    /// The time-domain bound, pinned as the known caveat rather than left to be
    /// discovered: a full-scale **anti-phase square wave** — pure side signal,
    /// no mid, the worst case there is — can overshoot while the filter's step
    /// response catches up, by at most `1 + strength` (the L1 norm of
    /// `δ − s·LP`). Nothing else can exceed that, and steady-state content at
    /// any frequency stays at or below unity (the test above). Real music's
    /// side signal is far below full scale; the deck's output clamp covers the
    /// pathological case.
    #[test]
    fn transient_antiphase_overshoot_is_bounded_by_one_plus_strength() {
        let n = 4_096;
        for strength in [0.25f32, 0.5, 0.75, 1.0] {
            let mut s = CrossfeedStage::new();
            s.set_params(2_000.0, strength);
            s.set_enabled(true);
            // Worst case: full-scale antiphase square-ish content, with energy
            // at every frequency including DC.
            let l: Vec<f32> = (0..n)
                .map(|i| if (i / 37) % 2 == 0 { 1.0 } else { -1.0 })
                .collect();
            let r: Vec<f32> = l.iter().map(|v| -v).collect();
            let mut buf = interleave(&l, &r);
            s.process(&mut buf, 2, SR);
            let peak = buf.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(
                buf.iter().all(|v| v.is_finite()),
                "strength {strength}: non-finite output"
            );
            assert!(
                peak <= 1.0 + strength + 1e-3,
                "strength {strength}: peak {peak} exceeds the 1 + strength bound"
            );
            // And a mid-only signal stays in range: the overshoot needs
            // anti-phase content to appear at all. Reset first — the state left
            // by the anti-phase block above is asymmetric, and mono is exact
            // only from a symmetric state (see `process`).
            s.reset();
            let mono: Vec<f32> = (0..n)
                .map(|i| if (i / 37) % 2 == 0 { 1.0 } else { -1.0 })
                .collect();
            let mut buf = interleave(&mono, &mono);
            s.process(&mut buf, 2, SR);
            let peak = buf.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(
                peak <= 1.000_001,
                "strength {strength}: full-scale mono must stay at full scale, got {peak}"
            );
        }
    }

    /// A rate change (a native-rate device reopen) redesigns the filter rather
    /// than leaving it tuned for the old rate: the same tone must cross by the
    /// same amount at 48 and 96 kHz.
    #[test]
    fn changing_the_sample_rate_redesigns_the_filter() {
        let crossed_at = |rate: u32| {
            let mut s = stage(Preset::Moderate);
            let n = 16_384;
            let sig: Vec<f32> = (0..n)
                .map(|i| {
                    let t = i as f32 / rate as f32;
                    (2.0 * core::f32::consts::PI * 300.0 * t).sin()
                })
                .collect();
            let mut buf = interleave(&sig, &vec![0.0; n]);
            s.process(&mut buf, 2, rate);
            rms_tail(&deinterleave(&buf, 1)) / rms_tail(&deinterleave(&buf, 0))
        };
        let (a, b) = (crossed_at(48_000), crossed_at(96_000));
        assert!(
            (a - b).abs() < 0.02,
            "the crossfeed must be rate-independent: {a:.4} at 48k vs {b:.4} at 96k"
        );
    }

    #[test]
    fn reset_clears_the_filter_state() {
        let mut s = stage(Preset::Moderate);
        let input = interleave(&tone(300.0, 512), &vec![0.0; 512]);
        let mut first = input.clone();
        s.process(&mut first, 2, SR);
        let mut again = input.clone();
        s.process(&mut again, 2, SR);
        assert_ne!(first, again, "state carries across blocks (as it must)");
        s.reset();
        let mut third = input.clone();
        s.process(&mut third, 2, SR);
        assert_eq!(first, third, "reset must restore the initial state");
    }

    #[test]
    fn parameters_are_clamped_and_reported() {
        let mut s = CrossfeedStage::new();
        s.set_params(5.0, 9.0);
        assert_eq!(s.params(), (FC_MIN, 1.0));
        s.set_params(50_000.0, -1.0);
        assert_eq!(s.params(), (FC_MAX, 0.0));
        // NaN is not a configuration.
        s.set_params(f32::NAN, f32::NAN);
        assert_eq!(s.params(), (700.0, 0.0));
        // The implied ITD is the one-pole's group delay: ~227 µs at 700 Hz.
        assert!((s.itd_us() - 227.4).abs() < 1.0, "itd {}", s.itd_us());
        s.set_params(650.0, 0.8);
        assert!((s.itd_us() - 244.9).abs() < 1.0, "itd {}", s.itd_us());
    }

    #[test]
    fn presets_get_stronger_in_order() {
        let crossed = |preset: Preset| {
            let mut s = stage(preset);
            let n = 8_192;
            let mut buf = interleave(&tone(300.0, n), &vec![0.0; n]);
            s.process(&mut buf, 2, SR);
            rms_tail(&deinterleave(&buf, 1)) / rms_tail(&deinterleave(&buf, 0))
        };
        let (light, moderate, strong) = (
            crossed(Preset::Light),
            crossed(Preset::Moderate),
            crossed(Preset::Strong),
        );
        assert!(
            light < moderate && moderate < strong,
            "presets must be monotonic: {light:.3} < {moderate:.3} < {strong:.3}"
        );
    }
}
