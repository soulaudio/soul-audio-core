//! Channel layouts and stereo downmix coefficients.
//!
//! A multichannel source folded onto a stereo sink needs to know *which* speaker each
//! source channel drives, not just how many there are. For the unambiguous counts (3.0 /
//! 5.1 / 7.1) the count alone fixes the layout, but 4/5/7 channels are ambiguous (quad vs
//! 3.1, 5.0 vs 4.1, the 6.1 variants) and need an explicit layout. This module models that
//! layout as a WAVEFORMATEXTENSIBLE **channel mask** (the representation WAV/CAF carry
//! directly, and the one FLAC/Vorbis/Opus/AIFF fix by spec per count), and turns a mask
//! into per-channel stereo downmix gains.

/// The widest channel count the deck's scratch buffers are sized for.
pub const MAX_CHANNELS: usize = 8;

/// WAVEFORMATEXTENSIBLE speaker-position bits. A multichannel stream's channels are ordered
/// by ascending bit value, so the mask both identifies and orders the channels.
pub mod speaker {
    pub const FL: u32 = 0x1; // front left
    pub const FR: u32 = 0x2; // front right
    pub const FC: u32 = 0x4; // front center
    pub const LFE: u32 = 0x8; // low frequency
    pub const BL: u32 = 0x10; // back left
    pub const BR: u32 = 0x20; // back right
    pub const FLC: u32 = 0x40; // front left of center
    pub const FRC: u32 = 0x80; // front right of center
    pub const BC: u32 = 0x100; // back center
    pub const SL: u32 = 0x200; // side left
    pub const SR: u32 = 0x400; // side right
}

/// The canonical WAVEFORMATEXTENSIBLE channel mask for a plain channel **count** — the
/// layout the formats that fix channel order by spec (FLAC, Vorbis, Opus mapping-family 1,
/// AIFF) imply from the count alone. `None` for 1/2 (handled by passthrough / mono-upmix,
/// not a downmix) and counts past [`MAX_CHANNELS`].
#[must_use]
pub fn standard_layout_mask(channels: u16) -> Option<u32> {
    use speaker::*;
    Some(match channels {
        3 => FL | FR | FC,                           // 3.0
        4 => FL | FR | BL | BR,                      // quad
        5 => FL | FR | FC | BL | BR,                 // 5.0
        6 => FL | FR | FC | LFE | BL | BR,           // 5.1
        7 => FL | FR | FC | LFE | BC | SL | SR,      // 6.1
        8 => FL | FR | FC | LFE | BL | BR | SL | SR, // 7.1
        _ => return None,
    })
}

/// Per-channel `(Lo, Ro)` stereo downmix gains for a `mask`-described layout with
/// `channels` channels, in the stream's canonical (ascending speaker-bit) order.
///
/// ITU-R BS.775: front-center and the surrounds fold in at −3 dB (`1/√2`); the LFE is
/// dropped (standard for a stereo downmix). Back-center folds equally to both outputs at
/// −3 dB. Returns `None` when the mask cannot drive a defined stereo fold — its set-bit
/// count disagrees with `channels`, `channels` exceeds [`MAX_CHANNELS`], or it carries a
/// speaker this fold does not place (don't guess a layout we can't name).
#[must_use]
pub fn downmix_to_stereo_coeffs(mask: u32, channels: usize) -> Option<[(f32, f32); MAX_CHANNELS]> {
    use speaker::*;
    const C: f32 = core::f32::consts::FRAC_1_SQRT_2; // −3 dB

    if channels == 0 || channels > MAX_CHANNELS || mask.count_ones() as usize != channels {
        return None;
    }
    let mut coeffs = [(0.0_f32, 0.0_f32); MAX_CHANNELS];
    let mut bits = mask;
    let mut ch = 0;
    // Walk the set bits low-to-high = the stream's channel order.
    while bits != 0 {
        let bit = bits & bits.wrapping_neg(); // lowest set bit
        bits &= bits - 1;
        coeffs[ch] = match bit {
            FL => (1.0, 0.0),
            FR => (0.0, 1.0),
            FC => (C, C),
            LFE => (0.0, 0.0), // dropped
            BL | SL | FLC => (C, 0.0),
            BR | SR | FRC => (0.0, C),
            BC => (C, C),
            _ => return None, // a speaker we don't fold → don't guess
        };
        ch += 1;
    }
    Some(coeffs)
}

#[cfg(test)]
mod tests {
    use super::*;
    const C: f32 = core::f32::consts::FRAC_1_SQRT_2;

    #[test]
    fn standard_masks_have_the_right_channel_counts() {
        for (ch, want_bits) in [(3u16, 3u32), (4, 4), (5, 5), (6, 6), (7, 7), (8, 8)] {
            let m = standard_layout_mask(ch).expect("mapped");
            assert_eq!(m.count_ones(), want_bits, "{ch}ch mask {m:#x}");
        }
        assert_eq!(standard_layout_mask(1), None);
        assert_eq!(standard_layout_mask(2), None);
        assert_eq!(standard_layout_mask(9), None);
    }

    #[test]
    fn five_one_coeffs_match_itu() {
        // FL FR FC LFE BL BR.
        let m = standard_layout_mask(6).unwrap();
        let c = downmix_to_stereo_coeffs(m, 6).unwrap();
        assert_eq!(c[0], (1.0, 0.0)); // FL
        assert_eq!(c[1], (0.0, 1.0)); // FR
        assert_eq!(c[2], (C, C)); // FC
        assert_eq!(c[3], (0.0, 0.0)); // LFE dropped
        assert_eq!(c[4], (C, 0.0)); // BL
        assert_eq!(c[5], (0.0, C)); // BR
    }

    #[test]
    fn quad_has_no_center_and_folds_the_back_pair() {
        let m = speaker::FL | speaker::FR | speaker::BL | speaker::BR;
        let c = downmix_to_stereo_coeffs(m, 4).unwrap();
        assert_eq!(c[0], (1.0, 0.0)); // FL
        assert_eq!(c[1], (0.0, 1.0)); // FR
        assert_eq!(c[2], (C, 0.0)); // BL
        assert_eq!(c[3], (0.0, C)); // BR
    }

    #[test]
    fn mismatched_mask_and_count_is_rejected() {
        // A 6-bit mask with a 5-channel claim: the mask does not describe this stream.
        let m = standard_layout_mask(6).unwrap();
        assert_eq!(downmix_to_stereo_coeffs(m, 5), None);
        // Too wide.
        assert_eq!(downmix_to_stereo_coeffs(0x3FF | 0x800, 9), None);
    }

    #[test]
    fn unknown_speaker_bit_is_rejected_not_guessed() {
        // FL FR + a high bit this fold does not place (top-front-left, 0x1000).
        assert_eq!(
            downmix_to_stereo_coeffs(speaker::FL | speaker::FR | 0x1000, 3),
            None
        );
    }
}
