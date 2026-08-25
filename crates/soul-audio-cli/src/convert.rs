//! Canonical-24-bit → device-format sample conversion: the arithmetic the cpal
//! real-time callback runs on each output buffer.
//!
//! It lives here — free of cpal and of any audio device — so it is unit-tested in
//! the ordinary (default-feature) test run. That means the exact samples that
//! reach the DAC are verified in CI without hardware; [`crate::cpal_sink`] just
//! wires these functions into cpal's callback.

/// Canonical bit depth the deck normalizes to (`±2^23`).
pub const CANONICAL_BITS: u8 = 24;

/// Fill `out` with f32 device samples. Each is popped from `next` (a `None` means
/// the ring underran → silence) and scaled so the canonical peak `±2^23` maps to
/// `±1.0` full scale.
pub fn fill_f32(mut next: impl FnMut() -> Option<i32>, out: &mut [f32]) {
    let scale = 2f32.powi(i32::from(CANONICAL_BITS) - 1);
    for s in out.iter_mut() {
        *s = next().unwrap_or(0) as f32 / scale;
    }
}

/// Fill `out` with i16 device samples: pop from `next` (silence on underrun),
/// shift 24→16 bit, and clamp to the i16 range.
pub fn fill_i16(mut next: impl FnMut() -> Option<i32>, out: &mut [i16]) {
    let shift = i32::from(CANONICAL_BITS) - 16;
    for s in out.iter_mut() {
        let v = next().unwrap_or(0) >> shift;
        *s = v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pop from a slice, then `None` (underrun) once exhausted.
    fn feeder(samples: &[i32]) -> impl FnMut() -> Option<i32> + '_ {
        let mut it = samples.iter().copied();
        move || it.next()
    }

    #[test]
    fn f32_scales_full_scale_and_silences_on_underrun() {
        let src = [8_388_607, -8_388_608, 0]; // +peak, -peak, zero
        let mut out = [0f32; 5];
        fill_f32(feeder(&src), &mut out);
        assert!((out[0] - 0.999_999_9).abs() < 1e-3, "+peak → ~+1.0");
        assert!((out[1] + 1.0).abs() < 1e-6, "-peak → -1.0");
        assert_eq!(out[2], 0.0);
        assert_eq!(out[3], 0.0, "underrun → silence");
        assert_eq!(out[4], 0.0, "underrun → silence");
    }

    #[test]
    fn i16_shifts_clamps_and_silences_on_underrun() {
        // 256000 >> 8 = 1000; full-scale maps exactly to i16 bounds; an
        // over-range value (should never occur — the deck clamps — but defended)
        // clamps; then underrun → 0.
        let src = [
            256_000,
            -256_000,
            8_388_607,
            -8_388_608,
            12_000_000,
            -12_000_000,
        ];
        let mut out = [0i16; 8];
        fill_i16(feeder(&src), &mut out);
        assert_eq!(out[0], 1000);
        assert_eq!(out[1], -1000);
        assert_eq!(out[2], i16::MAX);
        assert_eq!(out[3], i16::MIN);
        assert_eq!(out[4], i16::MAX, "over-range clamps");
        assert_eq!(out[5], i16::MIN, "over-range clamps");
        assert_eq!(out[6], 0, "underrun → silence");
        assert_eq!(out[7], 0, "underrun → silence");
    }
}
