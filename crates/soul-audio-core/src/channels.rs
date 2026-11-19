//! Channel layouts and stereo downmix coefficients.
//!
//! A multichannel source folded onto a stereo sink needs to know *which* speaker each
//! source channel drives, not just how many there are. [`ChannelLayout`] is that mapping:
//! an ordered per-channel speaker list, in the stream's own channel order. Crucially the
//! order is explicit, because formats disagree on it: WAV/FLAC use the WAVEFORMATEXTENSIBLE
//! order (`FL FR FC LFE …`, which a channel mask's ascending bits already encode), but
//! Vorbis and Opus-family-1 put the centre channel second (`FL C FR …`). A plain mask can't
//! express that, so the layout carries the order directly and the downmix maps each channel
//! to its speaker independently of position.

/// The widest channel count the deck's scratch buffers are sized for.
pub const MAX_CHANNELS: usize = 8;

/// WAVEFORMATEXTENSIBLE speaker-position bits.
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

/// Which speaker each source channel drives, in the stream's channel order. Built from a
/// WAVEFORMATEXTENSIBLE mask (ascending bits), the canonical layout for a count, or an
/// explicit per-channel order a codec fixes differently (Vorbis/Opus).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChannelLayout {
    speakers: [u32; MAX_CHANNELS],
    len: u8,
}

impl ChannelLayout {
    /// From an explicit per-channel speaker order (stream order). `None` if empty or wider
    /// than [`MAX_CHANNELS`].
    #[must_use]
    pub fn from_order(order: &[u32]) -> Option<Self> {
        if order.is_empty() || order.len() > MAX_CHANNELS {
            return None;
        }
        let mut speakers = [0u32; MAX_CHANNELS];
        speakers[..order.len()].copy_from_slice(order);
        Some(Self {
            speakers,
            len: order.len() as u8,
        })
    }

    /// From a WAVEFORMATEXTENSIBLE channel mask: one channel per set bit, in ascending
    /// speaker-bit order (the order such a stream interleaves its channels).
    #[must_use]
    pub fn from_mask(mask: u32) -> Self {
        let mut speakers = [0u32; MAX_CHANNELS];
        let mut bits = mask;
        let mut len = 0;
        while bits != 0 && len < MAX_CHANNELS {
            let bit = bits & bits.wrapping_neg(); // lowest set bit
            bits &= bits - 1;
            speakers[len] = bit;
            len += 1;
        }
        Self {
            speakers,
            len: len as u8,
        }
    }

    /// The canonical WAVEFORMATEXTENSIBLE layout for a plain channel count — what WAV without
    /// an explicit mask and FLAC (spec-ordered like WAVEFORMATEXTENSIBLE) use. `None` for
    /// 1/2 (passthrough / mono-upmix, not a downmix) and counts past [`MAX_CHANNELS`].
    #[must_use]
    pub fn standard(channels: u16) -> Option<Self> {
        standard_layout_mask(channels).map(Self::from_mask)
    }

    /// The speaker driven by each source channel, in stream order.
    #[must_use]
    pub fn speakers(&self) -> &[u32] {
        &self.speakers[..self.len as usize]
    }

    /// Number of channels in the layout.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.len as usize
    }
}

/// The canonical WAVEFORMATEXTENSIBLE channel mask for a plain channel **count**. `None` for
/// 1/2 and counts past [`MAX_CHANNELS`].
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

/// The Vorbis / Opus-mapping-family-1 channel order for a count (Vorbis I spec §4.3.9 /
/// RFC 7845 §5.1.1.2). It fixes the order per count but DIFFERS from WAVEFORMATEXTENSIBLE:
/// the centre channel is second (`FL C FR`), and the LFE comes last. `None` for 1/2 (not a
/// downmix) and counts past [`MAX_CHANNELS`].
#[must_use]
pub fn vorbis_family1_layout(channels: u16) -> Option<ChannelLayout> {
    use speaker::*;
    let order: &[u32] = match channels {
        3 => &[FL, FC, FR],
        4 => &[FL, FR, BL, BR], // quad: no centre, same as WAVEFORMATEXTENSIBLE
        5 => &[FL, FC, FR, BL, BR],
        6 => &[FL, FC, FR, BL, BR, LFE],         // 5.1
        7 => &[FL, FC, FR, SL, SR, BC, LFE],     // 6.1
        8 => &[FL, FC, FR, SL, SR, BL, BR, LFE], // 7.1
        _ => return None,
    };
    ChannelLayout::from_order(order)
}

/// The `(Lo, Ro)` stereo downmix gain for one speaker (ITU-R BS.775): front-centre and the
/// surrounds fold in at −3 dB (`1/√2`), the LFE is dropped, back-centre folds equally to
/// both. `None` for a speaker this fold does not place (don't guess).
#[must_use]
pub fn speaker_stereo_gains(speaker: u32) -> Option<(f32, f32)> {
    use speaker::*;
    const C: f32 = core::f32::consts::FRAC_1_SQRT_2; // −3 dB
    Some(match speaker {
        FL => (1.0, 0.0),
        FR => (0.0, 1.0),
        FC => (C, C),
        LFE => (0.0, 0.0), // dropped
        BL | SL | FLC => (C, 0.0),
        BR | SR | FRC => (0.0, C),
        BC => (C, C),
        _ => return None,
    })
}

/// Per-channel `(Lo, Ro)` stereo downmix gains for `layout`, in its channel order (indexed
/// by source channel; only the first `layout.channels()` entries are meaningful). `None` if
/// the layout carries a speaker [`speaker_stereo_gains`] can't place.
#[must_use]
pub fn downmix_to_stereo_coeffs(layout: &ChannelLayout) -> Option<[(f32, f32); MAX_CHANNELS]> {
    let mut coeffs = [(0.0_f32, 0.0_f32); MAX_CHANNELS];
    for (i, &sp) in layout.speakers().iter().enumerate() {
        coeffs[i] = speaker_stereo_gains(sp)?;
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
    fn from_mask_orders_by_ascending_bit() {
        // 5.1 mask → FL FR FC LFE BL BR (ascending bit order).
        let l = ChannelLayout::from_mask(standard_layout_mask(6).unwrap());
        use speaker::*;
        assert_eq!(l.speakers(), &[FL, FR, FC, LFE, BL, BR]);
    }

    #[test]
    fn standard_five_one_coeffs_match_itu() {
        let l = ChannelLayout::standard(6).unwrap();
        let c = downmix_to_stereo_coeffs(&l).unwrap();
        assert_eq!(c[0], (1.0, 0.0)); // FL
        assert_eq!(c[1], (0.0, 1.0)); // FR
        assert_eq!(c[2], (C, C)); // FC
        assert_eq!(c[3], (0.0, 0.0)); // LFE dropped
        assert_eq!(c[4], (C, 0.0)); // BL
        assert_eq!(c[5], (0.0, C)); // BR
    }

    #[test]
    fn vorbis_five_one_puts_centre_second_and_folds_correctly() {
        use speaker::*;
        let l = vorbis_family1_layout(6).unwrap();
        // Vorbis order: FL C FR BL BR LFE (centre is channel 1, LFE last).
        assert_eq!(l.speakers(), &[FL, FC, FR, BL, BR, LFE]);
        let c = downmix_to_stereo_coeffs(&l).unwrap();
        assert_eq!(c[0], (1.0, 0.0)); // FL
        assert_eq!(c[1], (C, C)); // FC (second!)
        assert_eq!(c[2], (0.0, 1.0)); // FR
        assert_eq!(c[3], (C, 0.0)); // BL
        assert_eq!(c[4], (0.0, C)); // BR
        assert_eq!(c[5], (0.0, 0.0)); // LFE dropped
    }

    #[test]
    fn quad_has_no_centre_and_folds_the_back_pair() {
        let l = ChannelLayout::from_order(&[speaker::FL, speaker::FR, speaker::BL, speaker::BR])
            .unwrap();
        let c = downmix_to_stereo_coeffs(&l).unwrap();
        assert_eq!(c[0], (1.0, 0.0)); // FL
        assert_eq!(c[1], (0.0, 1.0)); // FR
        assert_eq!(c[2], (C, 0.0)); // BL
        assert_eq!(c[3], (0.0, C)); // BR
    }

    #[test]
    fn unknown_speaker_bit_is_rejected_not_guessed() {
        // FL FR + a high bit this fold does not place (top-front-left, 0x1000).
        let l = ChannelLayout::from_order(&[speaker::FL, speaker::FR, 0x1000]).unwrap();
        assert_eq!(downmix_to_stereo_coeffs(&l), None);
    }

    #[test]
    fn from_order_rejects_empty_and_too_wide() {
        assert_eq!(ChannelLayout::from_order(&[]), None);
        assert_eq!(
            ChannelLayout::from_order(&[speaker::FL; MAX_CHANNELS + 1]),
            None
        );
    }
}
