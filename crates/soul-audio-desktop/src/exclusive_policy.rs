//! Device-free **exclusive-output policy**: which sample rate to switch a DAC to for a
//! track, and which of the device's physical formats is the bit-perfect best. Pure data
//! in, decision out — so the choices the CoreAudio exclusive sink makes against real
//! hardware are unit-tested on every OS (CI runners have no DAC).
//!
//! The model mirrors CoreAudio's `AudioStreamRangedDescription`: a device advertises a
//! list of physical formats, each valid over a sample-rate range (almost always a single
//! discrete rate, `min == max`).

use soul_audio_core::model::SampleFormat as CoreFmt;

/// One physical format a device advertises (a CoreAudio `AudioStreamRangedDescription`
/// reduced to what the policy needs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhysFormat {
    /// Lowest sample rate this format is valid at.
    pub rate_min: u32,
    /// Highest sample rate this format is valid at (`== rate_min` for a discrete rate).
    pub rate_max: u32,
    /// IEEE float (`true`) vs. signed integer (`false`).
    pub float: bool,
    /// Valid bits per sample (`mBitsPerChannel`): 16, 20, 24, 32.
    pub bits: u32,
    /// Channels the format carries.
    pub channels: u32,
}

impl PhysFormat {
    /// Whether this format can run at `rate`.
    pub fn supports_rate(&self, rate: u32) -> bool {
        (self.rate_min..=self.rate_max).contains(&rate)
    }

    /// How faithfully this format carries the deck's **canonical 24-bit** PCM — higher is
    /// better. Integer ≥ 24 bits is the DAC's native word and lossless; Float32 also holds
    /// any 24-bit value exactly (24-bit mantissa), so it is lossless too but ranked just
    /// below a true integer word (the HAL still converts it). 20/16-bit integer truncate
    /// the canonical stream, so they rank last, widest first.
    pub fn fidelity(&self) -> u32 {
        match (self.float, self.bits) {
            (false, b) if b >= 24 => 100 + b, // int32 (132) > int24 (124)
            (true, 32) => 90,
            (true, _) => 0,  // Float64 / odd floats: not something we render
            (false, b) => b, // int20 (20) > int16 (16)
        }
    }

    /// What the signal path should report as the output format for this physical format.
    pub fn report(&self) -> CoreFmt {
        if self.float {
            CoreFmt::Float { bits: 32 }
        } else {
            CoreFmt::Int {
                // A 32-bit integer word carries the canonical 24 left-justified, but the
                // DAC still receives 32 valid bits — report the device word.
                bits: self.bits.min(u32::from(u8::MAX)) as u8,
            }
        }
    }
}

/// The best (highest-[`fidelity`](PhysFormat::fidelity)) physical format that runs at
/// `rate` with at least `channels` channels, preferring an exact channel count on a tie
/// (a 2ch stream on a 2ch format, not the first two of an 8ch one). `None` if no format
/// can run at `rate` at all.
pub fn pick_physical_format(
    formats: &[PhysFormat],
    rate: u32,
    channels: u32,
) -> Option<PhysFormat> {
    formats
        .iter()
        .filter(|f| f.supports_rate(rate) && f.channels >= channels && f.fidelity() > 0)
        .max_by_key(|f| (f.fidelity(), f.channels == channels))
        .copied()
}

/// Every discrete rate the device can run `channels` channels at, ascending, deduplicated.
/// A continuous range contributes the standard rates that fall inside it.
pub fn supported_rates(formats: &[PhysFormat], channels: u32) -> Vec<u32> {
    let mut rates: Vec<u32> = Vec::new();
    for f in formats
        .iter()
        .filter(|f| f.channels >= channels && f.fidelity() > 0)
    {
        if f.rate_min == f.rate_max {
            rates.push(f.rate_min);
        } else {
            rates.extend(
                STANDARD_RATES
                    .iter()
                    .copied()
                    .filter(|&r| f.supports_rate(r)),
            );
        }
    }
    rates.sort_unstable();
    rates.dedup();
    rates
}

/// The standard PCM sample-rate ladder (both the 44.1 k and 48 k families).
pub const STANDARD_RATES: [u32; 13] = [
    8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000,
    352_800, 384_000,
];

/// Whether `rate` belongs to the 44.1 kHz family (an integer multiple or fraction of
/// 44 100 — 22 050, 88 200, 176 400 …). Everything else (8 k / 48 k multiples) is the
/// 48 kHz family.
fn is_44k_family(rate: u32) -> bool {
    rate % 11_025 == 0
}

/// The device rate to switch to for a track at `requested`, given the rates the device
/// supports (ascending, from [`supported_rates`]):
///
/// 1. `requested` itself when supported — native rate, **no resampling** (bit-perfect);
/// 2. otherwise the closest rate **in the same family** (44.1 k vs 48 k), preferring the
///    smallest one at or above `requested` (an exact-ratio upsample keeps all the
///    content) and else the highest below (e.g. 176.4 k on an 88.2 k-max DAC → 88.2 k);
/// 3. otherwise the closest rate overall, preferring at-or-above;
/// 4. `None` only if the device lists no rates at all.
pub fn choose_rate(supported: &[u32], requested: u32) -> Option<u32> {
    if supported.contains(&requested) {
        return Some(requested);
    }
    let closest = |cands: &mut dyn Iterator<Item = u32>| -> Option<u32> {
        let cands: Vec<u32> = cands.collect();
        cands
            .iter()
            .copied()
            .filter(|&r| r >= requested)
            .min()
            .or_else(|| cands.iter().copied().filter(|&r| r < requested).max())
    };
    let fam = is_44k_family(requested);
    closest(
        &mut supported
            .iter()
            .copied()
            .filter(|&r| is_44k_family(r) == fam),
    )
    .or_else(|| closest(&mut supported.iter().copied()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(rate: u32, bits: u32, ch: u32) -> PhysFormat {
        PhysFormat {
            rate_min: rate,
            rate_max: rate,
            float: false,
            bits,
            channels: ch,
        }
    }
    fn flt(rate: u32, ch: u32) -> PhysFormat {
        PhysFormat {
            rate_min: rate,
            rate_max: rate,
            float: true,
            bits: 32,
            channels: ch,
        }
    }

    /// Exactly what a real "MacBook Pro Speakers" (Apple silicon) reports: Float32 only,
    /// 44.1/48/88.2/96 kHz, stereo.
    fn macbook_speakers() -> Vec<PhysFormat> {
        [96_000, 88_200, 48_000, 44_100]
            .into_iter()
            .map(|r| flt(r, 2))
            .collect()
    }

    /// Exactly what a real DisplayPort monitor reports: 24/20-bit int (in 32-bit
    /// containers) and packed 16-bit, at 32/44.1/48 kHz.
    fn displayport_monitor() -> Vec<PhysFormat> {
        let mut v = Vec::new();
        for bits in [24, 20, 16] {
            for r in [48_000, 44_100, 32_000] {
                v.push(int(r, bits, 2));
            }
        }
        v
    }

    #[test]
    fn fidelity_ranks_wide_integer_over_float_over_narrow_integer() {
        let order = [
            int(0, 32, 2),
            int(0, 24, 2),
            flt(0, 2),
            int(0, 20, 2),
            int(0, 16, 2),
        ];
        for w in order.windows(2) {
            assert!(
                w[0].fidelity() > w[1].fidelity(),
                "{:?} must outrank {:?}",
                w[0],
                w[1]
            );
        }
        let f64fmt = PhysFormat {
            bits: 64,
            ..flt(0, 2)
        };
        assert_eq!(f64fmt.fidelity(), 0, "non-32-bit float is never picked");
    }

    #[test]
    fn picks_24_bit_integer_on_the_displayport_monitor() {
        let f = pick_physical_format(&displayport_monitor(), 44_100, 2).expect("44.1k exists");
        assert_eq!((f.float, f.bits, f.rate_min), (false, 24, 44_100));
        assert_eq!(f.report(), CoreFmt::Int { bits: 24 });
    }

    #[test]
    fn picks_float32_on_float_only_speakers() {
        let f = pick_physical_format(&macbook_speakers(), 88_200, 2).expect("88.2k exists");
        assert!(f.float);
        assert_eq!(f.report(), CoreFmt::Float { bits: 32 });
    }

    #[test]
    fn no_format_at_an_unsupported_rate() {
        assert_eq!(pick_physical_format(&macbook_speakers(), 192_000, 2), None);
        assert_eq!(
            pick_physical_format(&displayport_monitor(), 96_000, 2),
            None
        );
    }

    #[test]
    fn channel_count_must_fit_and_exact_match_wins_a_tie() {
        let fmts = vec![int(48_000, 24, 8), int(48_000, 24, 2), int(48_000, 24, 1)];
        let f = pick_physical_format(&fmts, 48_000, 2).unwrap();
        assert_eq!(f.channels, 2, "exact 2ch beats the 8ch superset on a tie");
        assert_eq!(
            pick_physical_format(&[int(48_000, 24, 1)], 48_000, 2),
            None,
            "a mono-only format can't carry stereo"
        );
        // A wider word on a superset still wins over a narrower exact match.
        let fmts = vec![int(48_000, 16, 2), int(48_000, 24, 8)];
        assert_eq!(pick_physical_format(&fmts, 48_000, 2).unwrap().bits, 24);
    }

    #[test]
    fn ranged_formats_are_honoured() {
        let ranged = PhysFormat {
            rate_min: 8_000,
            rate_max: 192_000,
            float: false,
            bits: 24,
            channels: 2,
        };
        assert!(pick_physical_format(&[ranged], 176_400, 2).is_some());
        assert_eq!(
            supported_rates(&[ranged], 2),
            vec![
                8_000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400,
                192_000
            ]
        );
    }

    #[test]
    fn supported_rates_dedup_and_sort() {
        assert_eq!(
            supported_rates(&displayport_monitor(), 2),
            vec![32_000, 44_100, 48_000]
        );
        assert_eq!(
            supported_rates(&macbook_speakers(), 2),
            vec![44_100, 48_000, 88_200, 96_000]
        );
        assert!(supported_rates(&macbook_speakers(), 6).is_empty());
    }

    #[test]
    fn choose_rate_native_when_supported() {
        let s = supported_rates(&macbook_speakers(), 2);
        for r in [44_100, 48_000, 88_200, 96_000] {
            assert_eq!(choose_rate(&s, r), Some(r), "{r} is native → no resampling");
        }
    }

    #[test]
    fn choose_rate_stays_in_family() {
        let s = supported_rates(&macbook_speakers(), 2); // 44.1/48/88.2/96
        assert_eq!(
            choose_rate(&s, 176_400),
            Some(88_200),
            "176.4k → 88.2k (÷2)"
        );
        assert_eq!(choose_rate(&s, 352_800), Some(88_200));
        assert_eq!(choose_rate(&s, 192_000), Some(96_000), "192k → 96k (÷2)");
        assert_eq!(choose_rate(&s, 22_050), Some(44_100), "22.05k → 44.1k (×2)");
        assert_eq!(
            choose_rate(&s, 32_000),
            Some(48_000),
            "32k is 48k-family → 48k"
        );
        assert_eq!(choose_rate(&s, 16_000), Some(48_000));
    }

    #[test]
    fn choose_rate_crosses_family_only_when_it_must() {
        // A 48k-only device asked for 44.1k: nothing in-family, take the closest above.
        assert_eq!(choose_rate(&[48_000], 44_100), Some(48_000));
        assert_eq!(choose_rate(&[48_000, 96_000], 88_200), Some(96_000));
        assert_eq!(
            choose_rate(&[48_000], 192_000),
            Some(48_000),
            "else highest below"
        );
        assert_eq!(choose_rate(&[], 44_100), None);
    }

    #[test]
    fn choose_rate_on_the_displayport_monitor() {
        let s = supported_rates(&displayport_monitor(), 2); // 32/44.1/48
        assert_eq!(choose_rate(&s, 96_000), Some(48_000));
        assert_eq!(choose_rate(&s, 88_200), Some(44_100));
        assert_eq!(choose_rate(&s, 44_100), Some(44_100));
    }
}
