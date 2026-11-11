//! Tiny `no_std`, `libm`-free transcendentals for the loudness analyzer.
//!
//! The core workspace forbids `libm` (enforced by `architecture-tests`); every
//! crate hand-rolls what it needs (see `soul-audio-core`'s `exp2`/`pow_43`).
//! Loudness needs only a handful of calls at low frequency:
//!
//! * [`tan`] — twice per [`crate::R128Analyzer::new`] (K-weight bilinear coeffs),
//! * [`log10`] — once per 100 ms gating block + the final integration,
//! * [`powf`] — twice at `new` (the fixed high-shelf gains `Vh`, `Vb`).
//!
//! None run per sample, so modest-order series suffice; the end-to-end
//! calibration test (a synthesized −23 LUFS sine must read −23.0 ± 0.1) is the
//! real accuracy gate on these.

const LN2: f64 = core::f64::consts::LN_2;
const LOG2_E: f64 = core::f64::consts::LOG2_E;
const LOG2_10: f64 = core::f64::consts::LOG2_10;

/// `floor` for finite `f64` without `std`/`libm` (our args are modest-magnitude).
fn floor(x: f64) -> f64 {
    let t = x as i64 as f64;
    if t > x { t - 1.0 } else { t }
}

/// Base-2 logarithm of `x > 0`. Decomposes `x = m·2^e` by IEEE-754 exponent
/// extraction, then `log2(m)` for `m ∈ [1, 2)` via the fast-converging
/// `ln(m) = 2·atanh((m−1)/(m+1))` series (`|r| < 1/3`).
pub fn log2(x: f64) -> f64 {
    let bits = x.to_bits();
    let e = (((bits >> 52) & 0x7ff) as i64) - 1023;
    let m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | (1023u64 << 52));
    let r = (m - 1.0) / (m + 1.0);
    let r2 = r * r;
    // 2·(r + r³/3 + r⁵/5 + r⁷/7 + r⁹/9)
    let series =
        r * (1.0 + r2 * (1.0 / 3.0 + r2 * (1.0 / 5.0 + r2 * (1.0 / 7.0 + r2 * (1.0 / 9.0)))));
    let ln_m = 2.0 * series;
    e as f64 + ln_m * LOG2_E
}

/// Base-10 logarithm of `x > 0`.
pub fn log10(x: f64) -> f64 {
    log2(x) / LOG2_10
}

/// `2^y` without `std`/`libm`. Integer part via exponent bit-pack, fractional
/// part via `exp(f·ln2)` Maclaurin series on `f ∈ [0, 1)`.
pub fn exp2(y: f64) -> f64 {
    let n = floor(y);
    let f = y - n;
    let t = f * LN2;
    // exp(t) for t ∈ [0, ln2): Maclaurin to t⁷/5040.
    let ef = 1.0
        + t * (1.0
            + t * (0.5
                + t * (1.0 / 6.0
                    + t * (1.0 / 24.0
                        + t * (1.0 / 120.0 + t * (1.0 / 720.0 + t * (1.0 / 5040.0)))))));
    let pow2n = f64::from_bits(((n as i64 + 1023) as u64) << 52);
    ef * pow2n
}

/// `base^exp` for `base > 0`, via `exp2(exp·log2(base))`.
pub fn powf(base: f64, exp: f64) -> f64 {
    exp2(exp * log2(base))
}

/// `tan(x)` for small `|x|` (our args are `π·f0/fs ≤ ~0.12 rad`). Maclaurin:
/// `x + x³/3 + 2x⁵/15 + 17x⁷/315`.
pub fn tan(x: f64) -> f64 {
    let x2 = x * x;
    x * (1.0 + x2 * (1.0 / 3.0 + x2 * (2.0 / 15.0 + x2 * (17.0 / 315.0))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn log2_known_values() {
        assert!(close(log2(1.0), 0.0, 1e-9), "log2(1)={}", log2(1.0));
        assert!(close(log2(2.0), 1.0, 1e-9), "log2(2)={}", log2(2.0));
        assert!(close(log2(8.0), 3.0, 1e-7), "log2(8)={}", log2(8.0));
        assert!(
            close(log2(1024.0), 10.0, 1e-6),
            "log2(1024)={}",
            log2(1024.0)
        );
        assert!(close(log2(0.5), -1.0, 1e-9), "log2(0.5)={}", log2(0.5));
    }

    #[test]
    fn log10_known_values() {
        assert!(close(log10(1.0), 0.0, 1e-9), "log10(1)={}", log10(1.0));
        assert!(close(log10(10.0), 1.0, 1e-7), "log10(10)={}", log10(10.0));
        assert!(
            close(log10(100.0), 2.0, 1e-7),
            "log10(100)={}",
            log10(100.0)
        );
        assert!(
            close(log10(1000.0), 3.0, 1e-6),
            "log10(1000)={}",
            log10(1000.0)
        );
        assert!(
            close(log10(2.0), core::f64::consts::LOG10_2, 1e-7),
            "log10(2)={}",
            log10(2.0)
        );
    }

    #[test]
    fn exp2_known_values() {
        assert!(close(exp2(0.0), 1.0, 1e-9), "exp2(0)={}", exp2(0.0));
        assert!(close(exp2(1.0), 2.0, 1e-9), "exp2(1)={}", exp2(1.0));
        assert!(close(exp2(10.0), 1024.0, 1e-4), "exp2(10)={}", exp2(10.0));
        assert!(close(exp2(-2.0), 0.25, 1e-9), "exp2(-2)={}", exp2(-2.0));
        assert!(
            close(exp2(0.5), core::f64::consts::SQRT_2, 1e-6),
            "exp2(0.5)={}",
            exp2(0.5)
        );
    }

    #[test]
    fn powf_known_values() {
        assert!(
            close(powf(2.0, 0.5), core::f64::consts::SQRT_2, 1e-6),
            "2^0.5={}",
            powf(2.0, 0.5)
        );
        assert!(
            close(powf(10.0, 0.3), 1.995_262_314_968_9, 1e-5),
            "10^0.3={}",
            powf(10.0, 0.3)
        );
        // The K-weight high-shelf gain: 10^(3.99984385397/20).
        assert!(close(
            powf(10.0, 3.999_843_853_97 / 20.0),
            1.584_864_2,
            1e-4
        ));
    }

    #[test]
    fn tan_small_angle() {
        assert!(close(tan(0.0), 0.0, 1e-12), "tan(0)={}", tan(0.0));
        assert!(
            close(tan(0.05), 0.050_041_708_375, 1e-9),
            "tan(0.05)={}",
            tan(0.05)
        );
        assert!(
            close(tan(0.1), 0.100_334_672_085, 1e-8),
            "tan(0.1)={}",
            tan(0.1)
        );
        // π·1681.97/44100 — the stage-1 bilinear warp at the lowest supported fs.
        assert!(
            close(tan(0.119_8), 0.120_375_6, 1e-5),
            "tan(0.1198)={}",
            tan(0.119_8)
        );
    }
}
