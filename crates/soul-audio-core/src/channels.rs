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

/// The AAC default `channel_configuration` channel order for a count (ISO/IEC 14496-3): the
/// decoder emits elements in bitstream order, which for the standard configs puts the centre
/// FIRST (the leading SCE), then the front L/R pair, the surround pair(s), and the LFE last.
/// Differs from WAVEFORMATEXTENSIBLE and from the Vorbis order. The standard configs are
/// count-distinct (config 3..6 → 3..6 channels, config 7 → 8), so the count names the order;
/// `None` for 1/2 (not a downmix), 7 (no standard AAC config), and counts past
/// [`MAX_CHANNELS`]. The exact side-vs-back label of a surround channel does not affect the
/// stereo fold (both fold to their side at −3 dB), only its L/R sidedness does.
#[must_use]
pub fn aac_layout(channels: u16) -> Option<ChannelLayout> {
    use speaker::*;
    let order: &[u32] = match channels {
        3 => &[FC, FL, FR],                      // config 3
        4 => &[FC, FL, FR, BC],                  // config 4: centre, front L/R, rear centre
        5 => &[FC, FL, FR, BL, BR],              // config 5: 5.0
        6 => &[FC, FL, FR, BL, BR, LFE],         // config 6: 5.1
        8 => &[FC, FL, FR, SL, SR, BL, BR, LFE], // config 7: 7.1
        _ => return None,
    };
    ChannelLayout::from_order(order)
}

/// The ALAC (Apple Lossless) default channel order for a count, from the ALAC per-count
/// channel-layout tags. Centre first (like AAC) for 3..6 channels; 6.1 adds a rear centre,
/// and 7.1 adds the front left/right-of-centre pair. `None` for 1/2 (not a downmix) and
/// counts past [`MAX_CHANNELS`].
#[must_use]
pub fn alac_layout(channels: u16) -> Option<ChannelLayout> {
    use speaker::*;
    let order: &[u32] = match channels {
        3 => &[FC, FL, FR],                        // C L R
        4 => &[FC, FL, FR, BC],                    // C L R Cs
        5 => &[FC, FL, FR, BL, BR],                // C L R Ls Rs
        6 => &[FC, FL, FR, BL, BR, LFE],           // C L R Ls Rs LFE (5.1)
        7 => &[FC, FL, FR, BL, BR, BC, LFE],       // C L R Ls Rs Cs LFE (6.1)
        8 => &[FC, FLC, FRC, FL, FR, BL, BR, LFE], // C Lc Rc L R Ls Rs LFE (7.1)
        _ => return None,
    };
    ChannelLayout::from_order(order)
}

/// A CoreAudio `AudioChannelLayout` channel **label** (`kAudioChannelLabel_*`, the
/// `mChannelLabel` of an `AudioChannelDescription`) as a WAVEFORMATEXTENSIBLE speaker bit.
/// `None` for a label this downmix does not place — height/ambisonic/discrete channels and
/// `Unknown`/`Unused` — so an unplaceable label makes the whole layout `None` rather than
/// silently folding a height channel into the front plane.
///
/// The surround labels are the distinction worth getting right: `LeftSurround` (5) is the
/// *rear* pair of a 5.1 layout while `LeftSurroundDirect` (10) is the *side* pair, which is
/// how CoreAudio tells 7.1's two surround pairs apart (`RearSurroundLeft`, 33, is the other
/// rear spelling). Matches ffmpeg's `mov_chan.c` mapping.
#[must_use]
pub fn coreaudio_label_speaker(label: u32) -> Option<u32> {
    use speaker::*;
    Some(match label {
        1 => FL,   // Left
        2 => FR,   // Right
        3 => FC,   // Center
        4 => LFE,  // LFEScreen
        5 => BL,   // LeftSurround (rear)
        6 => BR,   // RightSurround (rear)
        7 => FLC,  // LeftCenter
        8 => FRC,  // RightCenter
        9 => BC,   // CenterSurround (rear centre)
        10 => SL,  // LeftSurroundDirect (side)
        11 => SR,  // RightSurroundDirect (side)
        33 => BL,  // RearSurroundLeft
        34 => BR,  // RearSurroundRight
        35 => FLC, // LeftWide
        36 => FRC, // RightWide
        _ => return None,
    })
}

/// A CoreAudio `AudioChannelLayout` **layout tag** (`kAudioChannelLayoutTag_*`: a constant
/// in the high 16 bits, the channel count in the low 16) as an ordered layout. This is the
/// CAF `chan` chunk's compact form, and unlike a channel mask it can express orders that
/// are *not* WAVEFORMATEXTENSIBLE — `MPEG_5_1_B` is `L R Ls Rs C LFE`, `MPEG_5_1_D` is
/// `C L R Ls Rs LFE` — which is exactly why the count alone cannot stand in for it.
///
/// Covers the tags that name a speaker layout our stereo fold can place: the MPEG/AAC/ITU
/// families, `Quadraphonic`/`Pentagonal`/`Hexagonal`, and the DVD/AudioUnit aliases. `None`
/// for mono/stereo (a fold does not apply), for the two "look elsewhere in the struct" tags
/// (`UseChannelDescriptions` = 0, `UseChannelBitmap` = `1 << 16`), and for anything else —
/// including `DiscreteInOrder`, ambisonics, `Cube`, and the height layouts — so an unknown
/// tag falls back to the caller's count heuristic instead of a guessed order.
#[must_use]
pub fn coreaudio_layout_tag(tag: u32) -> Option<ChannelLayout> {
    use speaker::*;
    // Tag constants are `(id << 16) | channel_count`.
    const fn t(id: u32, ch: u32) -> u32 {
        (id << 16) | ch
    }
    let order: &[u32] = match tag {
        // Quadraphonic (108): L R Ls Rs — also `AAC_Quadraphonic`.
        x if x == t(108, 4) => &[FL, FR, BL, BR],
        // Pentagonal (109): L R Ls Rs C.
        x if x == t(109, 5) => &[FL, FR, BL, BR, FC],
        // Hexagonal (110): L R Ls Rs C Cs — also `AAC_6_0`'s non-AAC sibling.
        x if x == t(110, 6) => &[FL, FR, BL, BR, FC, BC],
        // MPEG_3_0_A (113): L R C. MPEG_3_0_B (114) / AAC_3_0: C L R.
        x if x == t(113, 3) => &[FL, FR, FC],
        x if x == t(114, 3) => &[FC, FL, FR],
        // MPEG_4_0_A (115): L R C Cs. MPEG_4_0_B (116) / AAC_4_0: C L R Cs.
        x if x == t(115, 4) => &[FL, FR, FC, BC],
        x if x == t(116, 4) => &[FC, FL, FR, BC],
        // MPEG_5_0_A..D (117..120): the four 5.0 orders.
        x if x == t(117, 5) => &[FL, FR, FC, BL, BR],
        x if x == t(118, 5) => &[FL, FR, BL, BR, FC],
        x if x == t(119, 5) => &[FL, FC, FR, BL, BR],
        x if x == t(120, 5) => &[FC, FL, FR, BL, BR], // AAC_5_0
        // MPEG_5_1_A..D (121..124): the four 5.1 orders. `A` is WAVEFORMATEXTENSIBLE's.
        x if x == t(121, 6) => &[FL, FR, FC, LFE, BL, BR],
        x if x == t(122, 6) => &[FL, FR, BL, BR, FC, LFE],
        x if x == t(123, 6) => &[FL, FC, FR, BL, BR, LFE],
        x if x == t(124, 6) => &[FC, FL, FR, BL, BR, LFE], // AAC_5_1
        // MPEG_6_1_A (125): L R C LFE Ls Rs Cs.
        x if x == t(125, 7) => &[FL, FR, FC, LFE, BL, BR, BC],
        // MPEG_7_1_A (126): L R C LFE Ls Rs Lc Rc.
        x if x == t(126, 8) => &[FL, FR, FC, LFE, BL, BR, FLC, FRC],
        // MPEG_7_1_B (127) / AAC_7_1: C Lc Rc L R Ls Rs LFE.
        x if x == t(127, 8) => &[FC, FLC, FRC, FL, FR, BL, BR, LFE],
        // MPEG_7_1_C (128): L R C LFE Ls Rs Rls Rrs — the side pair, then the rear pair.
        x if x == t(128, 8) => &[FL, FR, FC, LFE, SL, SR, BL, BR],
        // Emagic_Default_7_1 (129): L R Ls Rs C LFE Lc Rc.
        x if x == t(129, 8) => &[FL, FR, BL, BR, FC, LFE, FLC, FRC],
        // AudioUnit_6_0 (139): L R Ls Rs C Cs. AudioUnit_7_0 (140): L R Ls Rs C Rls Rrs.
        x if x == t(139, 6) => &[FL, FR, BL, BR, FC, BC],
        x if x == t(140, 7) => &[FL, FR, SL, SR, FC, BL, BR],
        // AAC_6_0 (141): C L R Ls Rs Cs. AAC_6_1 (142): C L R Ls Rs Cs LFE.
        x if x == t(141, 6) => &[FC, FL, FR, BL, BR, BC],
        x if x == t(142, 7) => &[FC, FL, FR, BL, BR, BC, LFE],
        // AAC_7_0 (143): C L R Ls Rs Rls Rrs.
        x if x == t(143, 7) => &[FC, FL, FR, SL, SR, BL, BR],
        // AudioUnit_7_0_Front (148): L R Ls Rs C Lc Rc.
        x if x == t(148, 7) => &[FL, FR, BL, BR, FC, FLC, FRC],
        // Deliberately unmapped, and therefore `None` (the caller falls back rather than
        // mis-routing): the `DVD_*`/`ITU_*` aliases (whose tag ids this does not claim to
        // know by heart — a wrong id→order pairing with a matching channel count would
        // silently send the centre to a surround), `SMPTE_DTV` (its trailing `Lt`/`Rt` are
        // a matrix-stereo downmix, not speakers, so it must not fold like one),
        // `DiscreteInOrder`, `Cube`, the ambisonic B-format, and every height layout.
        // A file using one of those can still fold if it carries the descriptions or
        // bitmap form, which name their channels outright.
        _ => return None,
    };
    ChannelLayout::from_order(order)
}

/// A CoreAudio `mChannelBitmap` (`kAudioChannelBit_*`) as a layout. The first eleven bits
/// are defined identically to the WAVEFORMATEXTENSIBLE speaker bits (`Left` = `1 << 0` …
/// `RightSurroundDirect` = `1 << 10`), in the same ascending-bit channel order, so this is
/// [`ChannelLayout::from_mask`] — but only once the higher (height / top-centre) bits are
/// ruled out: those have no WAVEFORMATEXTENSIBLE counterpart, and treating them as one
/// would mis-route a height channel into the front plane. `None` if any is set, or if the
/// bit count doesn't match `channels`.
#[must_use]
pub fn coreaudio_bitmap_layout(bitmap: u32, channels: u16) -> Option<ChannelLayout> {
    // Bits 0..=10 are the WAVEFORMATEXTENSIBLE speakers; 11+ (TopCenterSurround and the
    // vertical-height set) are not.
    const PLACEABLE: u32 = 0x7FF;
    if bitmap == 0 || bitmap & !PLACEABLE != 0 {
        return None;
    }
    if bitmap.count_ones() != u32::from(channels) {
        return None; // the bitmap doesn't describe this stream
    }
    Some(ChannelLayout::from_mask(bitmap))
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

    /// A CoreAudio layout tag: constant id in the high 16 bits, channel count in the low.
    const fn ca_tag(id: u32, ch: u32) -> u32 {
        (id << 16) | ch
    }

    #[test]
    fn coreaudio_tags_keep_their_own_channel_order() {
        use speaker::*;
        // The four 5.1 orders are the whole point: same six channels, four positions for
        // the centre and the LFE. A count-based guess can only ever match one of them.
        assert_eq!(
            coreaudio_layout_tag(ca_tag(121, 6)).unwrap().speakers(), // MPEG_5_1_A
            &[FL, FR, FC, LFE, BL, BR]
        );
        assert_eq!(
            coreaudio_layout_tag(ca_tag(122, 6)).unwrap().speakers(), // MPEG_5_1_B
            &[FL, FR, BL, BR, FC, LFE]
        );
        assert_eq!(
            coreaudio_layout_tag(ca_tag(123, 6)).unwrap().speakers(), // MPEG_5_1_C
            &[FL, FC, FR, BL, BR, LFE]
        );
        assert_eq!(
            coreaudio_layout_tag(ca_tag(124, 6)).unwrap().speakers(), // MPEG_5_1_D / AAC_5_1
            &[FC, FL, FR, BL, BR, LFE]
        );
        // MPEG_5_1_A is WAVEFORMATEXTENSIBLE's order — the one the count fallback assumes.
        assert_eq!(
            coreaudio_layout_tag(ca_tag(121, 6)).unwrap(),
            ChannelLayout::standard(6).unwrap()
        );
        // Quadraphonic resolves an ambiguous count; every tag's layout folds.
        assert_eq!(
            coreaudio_layout_tag(ca_tag(108, 4)).unwrap().speakers(),
            &[FL, FR, BL, BR]
        );
        for tag in [
            108, 109, 110, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126,
            127, 128, 129, 139, 140, 141, 142, 143, 148,
        ] {
            // The low 16 bits are each tag's own channel count, so sweep every plausible
            // one and check the layouts we do claim are all foldable.
            for ch in 3..=MAX_CHANNELS as u32 {
                if let Some(l) = coreaudio_layout_tag(ca_tag(tag, ch)) {
                    assert_eq!(l.channels(), ch as usize, "tag {tag} count {ch}");
                    assert!(
                        downmix_to_stereo_coeffs(&l).is_some(),
                        "tag {tag} count {ch} has an unfoldable speaker"
                    );
                }
            }
        }
    }

    #[test]
    fn unmappable_coreaudio_tags_and_labels_are_none() {
        // The two "look elsewhere in the struct" tags are not layouts themselves.
        assert_eq!(coreaudio_layout_tag(0), None); // UseChannelDescriptions
        assert_eq!(coreaudio_layout_tag(1 << 16), None); // UseChannelBitmap
        // Mono/stereo are not a downmix, and an unknown tag must not be guessed.
        assert_eq!(coreaudio_layout_tag(ca_tag(100, 1)), None); // Mono
        assert_eq!(coreaudio_layout_tag(ca_tag(101, 2)), None); // Stereo
        assert_eq!(coreaudio_layout_tag(ca_tag(112, 8)), None); // Cube
        assert_eq!(coreaudio_layout_tag(ca_tag(147, 6)), None); // DiscreteInOrder
        assert_eq!(coreaudio_layout_tag(ca_tag(9999, 6)), None);
        // A known tag id with the wrong channel count in its low bits is not that tag.
        assert_eq!(coreaudio_layout_tag(ca_tag(121, 5)), None);

        // Labels: the placeable set maps, everything else (height, ambisonic, Unknown,
        // Unused, discrete) stays None so an unplaceable channel can't be folded blind.
        use speaker::*;
        assert_eq!(coreaudio_label_speaker(1), Some(FL));
        assert_eq!(coreaudio_label_speaker(3), Some(FC));
        assert_eq!(coreaudio_label_speaker(4), Some(LFE));
        assert_eq!(coreaudio_label_speaker(5), Some(BL)); // LeftSurround = rear
        assert_eq!(coreaudio_label_speaker(10), Some(SL)); // LeftSurroundDirect = side
        assert_eq!(coreaudio_label_speaker(0), None); // Unknown
        assert_eq!(coreaudio_label_speaker(12), None); // TopCenterSurround
        assert_eq!(coreaudio_label_speaker(13), None); // VerticalHeightLeft
        assert_eq!(coreaudio_label_speaker(0xFFFF_FFFF), None); // Unknown sentinel
    }

    #[test]
    fn coreaudio_bitmap_matches_the_wave_mask_and_rejects_height_bits() {
        use speaker::*;
        // Bits 0..=10 are the WAVEFORMATEXTENSIBLE speakers, same ascending order.
        let m = FL | FR | FC | LFE | BL | BR;
        assert_eq!(
            coreaudio_bitmap_layout(m, 6).unwrap(),
            ChannelLayout::from_mask(m)
        );
        // A bit past the shared range (TopCenterSurround, 1 << 11) has no
        // WAVEFORMATEXTENSIBLE counterpart — fold nothing rather than the wrong thing.
        assert_eq!(coreaudio_bitmap_layout(m | (1 << 11), 7), None);
        // A bitmap that doesn't describe this stream is not this stream's layout.
        assert_eq!(coreaudio_bitmap_layout(m, 5), None);
        assert_eq!(coreaudio_bitmap_layout(0, 6), None);
    }

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
    fn aac_five_one_puts_centre_first_and_folds_correctly() {
        use speaker::*;
        let l = aac_layout(6).unwrap();
        // AAC 5.1 decode order: C L R Ls Rs LFE (centre FIRST).
        assert_eq!(l.speakers(), &[FC, FL, FR, BL, BR, LFE]);
        let c = downmix_to_stereo_coeffs(&l).unwrap();
        assert_eq!(c[0], (C, C)); // FC (first!)
        assert_eq!(c[1], (1.0, 0.0)); // FL
        assert_eq!(c[2], (0.0, 1.0)); // FR
        assert_eq!(c[3], (C, 0.0)); // BL
        assert_eq!(c[4], (0.0, C)); // BR
        assert_eq!(c[5], (0.0, 0.0)); // LFE dropped
        // No standard AAC config is 7-channel.
        assert_eq!(aac_layout(7), None);
    }

    #[test]
    fn alac_layouts_are_centre_first_and_map_7_1_centre_channels() {
        use speaker::*;
        // 5.1 shares AAC's centre-first order.
        assert_eq!(
            alac_layout(6).unwrap().speakers(),
            aac_layout(6).unwrap().speakers()
        );
        // 7.1 differs: C Lc Rc L R Ls Rs LFE (front left/right-of-centre present).
        assert_eq!(
            alac_layout(8).unwrap().speakers(),
            &[FC, FLC, FRC, FL, FR, BL, BR, LFE]
        );
        // Every ALAC layout folds (FLC/FRC are placed).
        for ch in 3..=8u16 {
            assert!(
                downmix_to_stereo_coeffs(&alac_layout(ch).unwrap()).is_some(),
                "{ch}ch"
            );
        }
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
