//! MPEG audio frame header + Xing/Info/VBRI duration parsing for MP3 — a
//! cheap header scan, never a decode. Called by [`crate::id3v2::read_mp3`]
//! once `src` is positioned at `audio_start` (just past any leading ID3v2
//! tag). Mirrors the frame-sync rules, bitrate/sample-rate tables, and
//! Xing/Info/VBRI offsets used by `soul_audio_core`'s MP3 demuxer
//! (`crates/soul-audio-core/src/demux/mp3.rs`) so the duration reported here
//! lines up with the one the playback engine computes — but this module has
//! no dependency on it: duplicated independently so `soul-audio-tags` stays
//! decoupled from the decoder/demux stack.
//!
//! Deliberate scope limits:
//! - All three MPEG layers (I/II/III) are parsed; only a reserved-layer or
//!   reserved-version header is treated as sync noise and skipped. Xing/Info/
//!   VBRI tags are a Layer III convention, so Layer I/II duration always comes
//!   from the CBR file-length estimate below.
//! - Free-format bitrate (index 0) is declined — its frame length isn't
//!   table-derivable, and it's vanishingly rare in the wild.
//! - Without a Xing/Info/VBRI header, `total_frames` is estimated from the
//!   file length and the first frame's bitrate (assumes CBR); documented as
//!   an approximation, not exact.
//! - Any failure (no sync found within the scan window, truncated header,
//!   I/O error) yields `StreamFacts::default()` — this scan never fails the
//!   overall tag read.

use crate::StreamFacts;
use crate::util::read_exact;
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

/// How far past `audio_start` to scan for the first valid frame sync before
/// giving up. Generous relative to any realistic post-ID3v2 padding; a real
/// MP3 syncs within the first few bytes.
const SYNC_SCAN_WINDOW: usize = 8192;

/// Bitrate (kbps) by 4-bit index, 0 = free/reserved (never a valid frame —
/// the free-format case is out of scope, see module docs). The table differs
/// by MPEG version (1 vs 2/2.5) and by layer, so there is one per combination.
const BR_V1_L1: [u32; 16] = [
    0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
];
const BR_V1_L2: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
];
const BR_V1_L3: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];
/// MPEG2/2.5 Layer I.
const BR_V2_L1: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
];
/// MPEG2/2.5 Layers II and III share one bitrate table.
const BR_V2_L23: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];

/// Sample rate (Hz) by the header's 2-bit version field (00=MPEG2.5,
/// 01=reserved, 10=MPEG2, 11=MPEG1) and 2-bit sample-rate index.
const SAMPLE_RATES: [[u32; 4]; 4] = [
    [11025, 12000, 8000, 0],
    [0, 0, 0, 0],
    [22050, 24000, 16000, 0],
    [44100, 48000, 32000, 0],
];

/// A parsed, validated MPEG (Layer I/II/III) frame header.
#[derive(Clone, Copy)]
struct FrameHeader {
    mpeg1: bool,
    sample_rate: u32,
    channels: u16,
    samples_per_frame: u32,
    /// Bitrate in bits/second (from the header's bitrate index). The CBR
    /// duration is derived from *this*, not `frame_len`: a per-frame length is
    /// padding-biased (CBR pads only some frames, so the first frame isn't
    /// representative), whereas `audio_bytes·8 ÷ bitrate` is exact for CBR.
    bitrate_bps: u32,
    /// 2 if the protection bit indicates a trailing 16-bit CRC, else 0 — the
    /// Xing/Info tag sits after this, in addition to the side-info bytes.
    crc_bytes: u64,
}

/// Parse+validate a 4-byte MPEG frame header. `None` for anything that isn't
/// a supported (non-reserved layer/version, non-free-format) frame — the
/// caller treats that as sync noise and keeps scanning.
fn parse_header(h: &[u8; 4]) -> Option<FrameHeader> {
    if h[0] != 0xFF || (h[1] & 0xE0) != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 0x3; // 0=2.5, 1=reserved, 2=v2, 3=v1
    let layer = (h[1] >> 1) & 0x3; // 01=III, 10=II, 11=I, 00=reserved
    if version == 1 || layer == 0 {
        return None; // reserved MPEG version / reserved layer
    }
    let br_idx = ((h[2] >> 4) & 0xF) as usize;
    let sr_idx = ((h[2] >> 2) & 0x3) as usize;
    let chan_mode = (h[3] >> 6) & 0x3;
    if br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }

    let mpeg1 = version == 3;
    let sample_rate = SAMPLE_RATES[version as usize][sr_idx];
    if sample_rate == 0 {
        return None;
    }
    // Bitrate table depends on both the MPEG version and the layer: the three
    // layers use different tables, and MPEG2/2.5 differs from MPEG1.
    let bitrate_kbps = match (mpeg1, layer) {
        (true, 3) => BR_V1_L1[br_idx],       // MPEG1 Layer I
        (true, 2) => BR_V1_L2[br_idx],       // MPEG1 Layer II
        (true, 1) => BR_V1_L3[br_idx],       // MPEG1 Layer III
        (false, 3) => BR_V2_L1[br_idx],      // MPEG2/2.5 Layer I
        (false, 1 | 2) => BR_V2_L23[br_idx], // MPEG2/2.5 Layers II & III
        _ => return None,                    // reserved layer (filtered above)
    };
    if bitrate_kbps == 0 {
        return None;
    }

    let channels = if chan_mode == 3 { 1 } else { 2 };
    // Samples per frame are fixed per layer: Layer I = 384, Layer II = 1152,
    // Layer III = 1152 (MPEG1) / 576 (MPEG2/2.5).
    let samples_per_frame = match layer {
        3 => 384,           // Layer I
        2 => 1152,          // Layer II
        1 if mpeg1 => 1152, // Layer III, MPEG1
        1 => 576,           // Layer III, MPEG2/2.5
        _ => return None,   // reserved layer (filtered above)
    };
    let crc_bytes = if h[1] & 0x1 == 0 { 2 } else { 0 }; // protection bit 0 = CRC present

    // The CBR duration is bitrate-based (see `cbr_total_samples`), so only the
    // bitrate is carried — not a per-frame length, which would be padding-biased.
    let bitrate_bps = bitrate_kbps * 1000;

    Some(FrameHeader {
        mpeg1,
        sample_rate,
        channels,
        samples_per_frame,
        bitrate_bps,
        crc_bytes,
    })
}

/// Total per-channel samples for a header-less CBR stream, derived from the
/// bitrate rather than a per-frame length. For CBR, `audio_bytes·8 ÷ bitrate`
/// is the exact duration in seconds regardless of how padding is distributed
/// across frames, so `round(audio_bytes·8·sample_rate ÷ bitrate)` is the
/// padding-immune sample count. Integer math, round-half-up, `no_std`.
fn cbr_total_samples(audio_bytes: u64, sample_rate: u32, bitrate_bps: u32) -> u64 {
    if bitrate_bps == 0 {
        return 0;
    }
    let num = u128::from(audio_bytes) * 8 * u128::from(sample_rate);
    let den = u128::from(bitrate_bps);
    ((num + den / 2) / den) as u64
}

/// Side-info byte length between the header (+ optional CRC) and the
/// Xing/Info tag: MPEG1 17 (mono) / 32 (stereo), MPEG2/2.5 9 (mono) / 17
/// (stereo).
fn side_info_len(mpeg1: bool, mono: bool) -> u64 {
    match (mpeg1, mono) {
        (true, true) => 17,
        (true, false) => 32,
        (false, true) => 9,
        (false, false) => 17,
    }
}

/// Scan forward from `audio_start` (`src`'s position on entry must match),
/// within [`SYNC_SCAN_WINDOW`] bytes, for the first valid MPEG frame sync.
/// On success, `src` is left positioned just past the 4-byte header
/// (`header_end`); returns the header and its absolute start offset.
fn find_first_frame<S: SeekSource>(src: &mut S, audio_start: u64) -> Option<(FrameHeader, u64)> {
    let mut window = [0u8; 4];
    read_exact(src, &mut window).ok()?;
    let mut offset = 0usize;
    loop {
        if let Some(f) = parse_header(&window) {
            return Some((f, audio_start + offset as u64));
        }
        offset += 1;
        if offset > SYNC_SCAN_WINDOW {
            return None;
        }
        window[0] = window[1];
        window[1] = window[2];
        window[2] = window[3];
        let mut b = [0u8; 1];
        read_exact(src, &mut b).ok()?;
        window[3] = b[0];
    }
}

/// Read a Xing/Info tag at absolute offset `off` (`header_end + crc_bytes +
/// side_info`). Returns the declared total MPEG-frame count if the tag is
/// present and its "frame count present" flag (bit 0) is set.
fn read_xing_or_info<S: SeekSource>(src: &mut S, off: u64) -> Option<u64> {
    src.seek(SeekFrom::Start(off)).ok()?;
    let mut magic = [0u8; 4];
    read_exact(src, &mut magic).ok()?;
    if &magic != b"Xing" && &magic != b"Info" {
        return None;
    }
    let mut flags_buf = [0u8; 4];
    read_exact(src, &mut flags_buf).ok()?;
    if u32::from_be_bytes(flags_buf) & 0x1 == 0 {
        return None; // frame count not present
    }
    let mut frames_buf = [0u8; 4];
    read_exact(src, &mut frames_buf).ok()?;
    let frames = u32::from_be_bytes(frames_buf);
    if frames == 0 {
        None
    } else {
        Some(u64::from(frames))
    }
}

/// Read a VBRI tag at the fixed absolute offset `header_end + 32` (VBRI's
/// position doesn't depend on side-info size or the CRC bit — it's always
/// this far past the header). Returns the declared total MPEG-frame count
/// (the 4-byte BE integer at offset +14 into the tag).
fn read_vbri<S: SeekSource>(src: &mut S, off: u64) -> Option<u64> {
    src.seek(SeekFrom::Start(off)).ok()?;
    let mut magic = [0u8; 4];
    read_exact(src, &mut magic).ok()?;
    if &magic != b"VBRI" {
        return None;
    }
    src.seek(SeekFrom::Current(10)).ok()?; // +14 into the tag, 4 already consumed
    let mut frames_buf = [0u8; 4];
    read_exact(src, &mut frames_buf).ok()?;
    let frames = u32::from_be_bytes(frames_buf);
    if frames == 0 {
        None
    } else {
        Some(u64::from(frames))
    }
}

/// The fallible core of [`read_facts`]: `None` on any parse failure, which
/// the caller turns into `StreamFacts::default()`.
fn try_read_facts<S: SeekSource>(src: &mut S) -> Option<StreamFacts> {
    let audio_start = src.seek(SeekFrom::Current(0)).ok()?;
    let (header, frame_start) = find_first_frame(src, audio_start)?;
    let header_end = frame_start + 4;

    let mono = header.channels == 1;
    let xing_off = header_end + header.crc_bytes + side_info_len(header.mpeg1, mono);

    let mpeg_frames = read_xing_or_info(src, xing_off).or_else(|| read_vbri(src, header_end + 32));

    let total_frames = match mpeg_frames {
        Some(frames) => frames.saturating_mul(u64::from(header.samples_per_frame)),
        None => {
            // No VBR header: derive the CBR duration from the bitrate, NOT a
            // per-frame length. CBR pads only some frames (+1 slot), so the first
            // frame's length isn't representative — dividing the byte span by it
            // reads long (MP1 +1.4 s / MP2 +0.44 s, the 5:00-vs-4:59 bug).
            // `audio_bytes·8 ÷ bitrate` is the exact CBR duration (padding-immune),
            // and `cbr_total_samples` turns it into a sample count. This matches
            // the decoder's `resolve_cbr_length`, keeping LIST == PLAYBACK. Measured
            // from `frame_start` (the first sync, past any garbage); a trailing
            // 128-byte ID3v1 "TAG" is metadata.
            let end = src.seek(SeekFrom::End(0)).ok()?;
            let mut trailer = 0u64;
            if end >= frame_start + 128 && src.seek(SeekFrom::Start(end - 128)).is_ok() {
                let mut tag = [0u8; 3];
                if read_exact(src, &mut tag).is_ok() && &tag == b"TAG" {
                    trailer = 128;
                }
            }
            let audio_bytes = end.saturating_sub(frame_start).saturating_sub(trailer);
            cbr_total_samples(audio_bytes, header.sample_rate, header.bitrate_bps)
        }
    };

    Some(StreamFacts {
        total_frames: Some(total_frames),
        sample_rate: Some(header.sample_rate),
        channels: Some(header.channels),
    })
}

/// Populate [`StreamFacts`] for an MP3 by parsing the first MPEG audio frame
/// header (and its Xing/Info/VBRI tag, if any) at `src`'s current position —
/// which must be `audio_start`, i.e. just past any leading ID3v2 tag. Never
/// errors: any parse failure yields `StreamFacts::default()` rather than
/// failing the caller's tag read.
pub fn read_facts<S: SeekSource>(src: &mut S) -> StreamFacts {
    try_read_facts(src).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    /// Build a 4-byte MPEG-1 Layer III header. `protected`: CRC present
    /// (protection bit 0). `br_idx`/`sr_idx` index [`BR_V1_L3`]/row-3 of
    /// [`SAMPLE_RATES`].
    fn header_v1(br_idx: u8, sr_idx: u8, mono: bool, protected: bool) -> [u8; 4] {
        let b1 = 0xE0 | (0b11 << 3) | (0b01 << 1) | u8::from(!protected);
        let b2 = (br_idx << 4) | (sr_idx << 2);
        let b3 = if mono { 0b11 << 6 } else { 0 };
        [0xFF, b1, b2, b3]
    }

    /// Build a 4-byte MPEG-1 header for an explicit layer. `layer_bits`:
    /// `0b01` = III, `0b10` = II, `0b11` = I. `br_idx` indexes that layer's
    /// bitrate table; `sr_idx` indexes row 3 of [`SAMPLE_RATES`].
    fn header_v1_layer(
        layer_bits: u8,
        br_idx: u8,
        sr_idx: u8,
        mono: bool,
        protected: bool,
    ) -> [u8; 4] {
        let b1 = 0xE0 | (0b11 << 3) | (layer_bits << 1) | u8::from(!protected);
        let b2 = (br_idx << 4) | (sr_idx << 2);
        let b3 = if mono { 0b11 << 6 } else { 0 };
        [0xFF, b1, b2, b3]
    }

    fn xing_frame(header: [u8; 4], tag_off: usize, magic: &[u8; 4], frame_count: u32) -> Vec<u8> {
        let mut b = alloc::vec![0u8; tag_off + 12];
        b[0..4].copy_from_slice(&header);
        b[tag_off..tag_off + 4].copy_from_slice(magic);
        b[tag_off + 4..tag_off + 8].copy_from_slice(&1u32.to_be_bytes()); // flags: frame count present
        b[tag_off + 8..tag_off + 12].copy_from_slice(&frame_count.to_be_bytes());
        b
    }

    #[test]
    fn parses_mpeg1_l3_stereo_header_fields() {
        // 128 kbps (idx 9), 44100 Hz (idx 0), stereo, unprotected.
        let h = header_v1(9, 0, false, false);
        let f = parse_header(&h).expect("valid header");
        assert!(f.mpeg1);
        assert_eq!(f.sample_rate, 44_100);
        assert_eq!(f.channels, 2);
        assert_eq!(f.samples_per_frame, 1152);
        assert_eq!(f.bitrate_bps, 128_000); // 128 kbps
        assert_eq!(f.crc_bytes, 0);
    }

    #[test]
    fn parses_mpeg1_l3_mono_protected_header_fields() {
        let h = header_v1(9, 0, true, true);
        let f = parse_header(&h).expect("valid header");
        assert_eq!(f.channels, 1);
        assert_eq!(f.crc_bytes, 2);
    }

    #[test]
    fn rejects_reserved_layer_version_and_free_format() {
        // Reserved layer (layer bits = 00). Layers I/II/III are all accepted
        // now (see the parses_mpeg1_layer{1,2}_header_fields tests); only the
        // reserved layer value is noise.
        let mut h = header_v1(9, 0, false, true);
        h[1] &= !0b0000_0110; // clear layer bits -> 00
        assert!(parse_header(&h).is_none());

        // Reserved version (bits 3-4 of byte1 = 01).
        let mut h2 = header_v1(9, 0, false, true);
        h2[1] = (h2[1] & !0b0001_1000) | (0b01 << 3);
        assert!(parse_header(&h2).is_none());

        // Free-format bitrate (index 0).
        let h3 = header_v1(0, 0, false, true);
        assert!(parse_header(&h3).is_none());
    }

    #[test]
    fn xing_frame_count_gives_total_frames_stereo_unprotected() {
        // Standard MPEG1-stereo-unprotected Xing offset: 4 + 0 (crc) + 32 = 36.
        let h = header_v1(9, 0, false, false);
        let bytes = xing_frame(h, 36, b"Xing", 5_000);
        let mut src = SliceSource::new(&bytes);
        let facts = read_facts(&mut src);
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(5_000 * 1152));
    }

    #[test]
    fn info_tag_crc_protected_shifts_offset_by_two() {
        // Protected (CRC present) MPEG1 stereo: 4 + 2 (crc) + 32 = 38.
        let h = header_v1(9, 0, false, true);
        let bytes = xing_frame(h, 38, b"Info", 1_000);
        let mut src = SliceSource::new(&bytes);
        let facts = read_facts(&mut src);
        assert_eq!(facts.total_frames, Some(1_000 * 1152));
    }

    #[test]
    fn xing_tag_mono_offset() {
        // MPEG1 mono unprotected: 4 + 0 + 17 = 21.
        let h = header_v1(9, 0, true, false);
        let bytes = xing_frame(h, 21, b"Xing", 2_500);
        let mut src = SliceSource::new(&bytes);
        let facts = read_facts(&mut src);
        assert_eq!(facts.channels, Some(1));
        assert_eq!(facts.total_frames, Some(2_500 * 1152));
    }

    #[test]
    fn vbri_frame_count_gives_total_frames() {
        // VBRI sits at the fixed offset header_end + 32 = 36 regardless of
        // side-info size; its frame count is at tag offset +14.
        let h = header_v1(9, 0, false, true);
        let mut b = alloc::vec![0u8; 36 + 18];
        b[0..4].copy_from_slice(&h);
        b[36..40].copy_from_slice(b"VBRI");
        b[36 + 14..36 + 18].copy_from_slice(&2_000u32.to_be_bytes());
        let mut src = SliceSource::new(&b);
        let facts = read_facts(&mut src);
        assert_eq!(facts.total_frames, Some(2_000 * 1152));
    }

    #[test]
    fn cbr_fallback_estimates_from_file_length_when_no_vbr_header() {
        // 128 kbps MPEG-1 Layer III @ 44,100 Hz. The CBR duration is bitrate-based:
        // 16,000 B · 8 ÷ 128,000 bps = exactly 1.000 s → 44,100 samples. (The old
        // frame-count estimate — 16000 ÷ 417 B/frame × 1152 = 43,776 — divided by
        // the *first* frame's length, which is padding-biased.) All-zero body never
        // spells "Xing"/"Info"/"VBRI", so this is the CBR estimate path.
        let h = header_v1(9, 0, false, true);
        let mut b = alloc::vec![0u8; 16_000];
        b[0..4].copy_from_slice(&h);
        let mut src = SliceSource::new(&b);
        let facts = read_facts(&mut src);
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(44_100));
    }

    #[test]
    fn no_sync_found_returns_default_facts() {
        let b = [0u8; 32]; // no 0xFF sync anywhere; too short to matter either way
        let mut src = SliceSource::new(&b);
        let facts = read_facts(&mut src);
        assert_eq!(facts, StreamFacts::default());
    }

    #[test]
    fn garbage_before_sync_is_skipped() {
        let mut b = alloc::vec![0x00u8; 10];
        b.extend_from_slice(&header_v1(9, 0, false, true));
        b.resize(b.len() + 64, 0);
        let mut src = SliceSource::new(&b);
        let facts = read_facts(&mut src);
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
    }

    #[test]
    fn parses_mpeg1_layer2_header_fields() {
        // Layer II (bits 10), 128 kbps (BR_V1_L2 idx 8), 44100 Hz, stereo.
        let h = header_v1_layer(0b10, 8, 0, false, false);
        let f = parse_header(&h).expect("Layer II must parse");
        assert!(f.mpeg1);
        assert_eq!(f.sample_rate, 44_100);
        assert_eq!(f.channels, 2);
        assert_eq!(f.samples_per_frame, 1152);
        assert_eq!(f.bitrate_bps, 128_000); // Layer II, 128 kbps
    }

    #[test]
    fn parses_mpeg1_layer1_header_fields() {
        // Layer I (bits 11), 128 kbps (BR_V1_L1 idx 4), 44100 Hz, stereo.
        // Layer I carries only 384 samples per frame (vs 1152 for II/III).
        let h = header_v1_layer(0b11, 4, 0, false, false);
        let f = parse_header(&h).expect("Layer I must parse");
        assert!(f.mpeg1);
        assert_eq!(f.sample_rate, 44_100);
        assert_eq!(f.channels, 2);
        assert_eq!(f.samples_per_frame, 384);
        assert_eq!(f.bitrate_bps, 128_000); // Layer I, 128 kbps
    }

    #[test]
    fn cbr_duration_for_layer2_file() {
        // Layer II, 128 kbps @ 44,100 Hz. Bitrate-based CBR duration:
        // 16,000 B · 8 ÷ 128,000 = 1.000 s → 44,100 samples. (Old frame-count
        // estimate 16000 ÷ 417 × 1152 = 43,776 read the padding-biased first
        // frame.) Xing/Info/VBRI is a Layer III convention, absent here, so this
        // takes the CBR file-length path — the path real .mp2 files use.
        let h = header_v1_layer(0b10, 8, 0, false, false);
        let mut b = alloc::vec![0u8; 16_000];
        b[0..4].copy_from_slice(&h);
        let mut src = SliceSource::new(&b);
        let facts = read_facts(&mut src);
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(44_100));
    }

    #[test]
    fn cbr_duration_for_layer1_file() {
        // Layer I, 128 kbps @ 44,100 Hz. Bitrate-based CBR duration:
        // 16,000 B · 8 ÷ 128,000 = 1.000 s → 44,100 samples, independent of the
        // layer's samples-per-frame. (Old frame-count estimate 16000 ÷ 136 × 384 =
        // 44,928 read the padding-biased first frame.)
        let h = header_v1_layer(0b11, 4, 0, false, false);
        let mut b = alloc::vec![0u8; 16_000];
        b[0..4].copy_from_slice(&h);
        let mut src = SliceSource::new(&b);
        let facts = read_facts(&mut src);
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(44_100));
    }

    /// For header-less CBR MP1/MP2 the library LIST duration (this tag scan) and
    /// the PLAYBACK duration (the decoder's `total_frames`) must (a) equal each
    /// other and (b) land within one MPEG frame of the TRUE decoded length. Both
    /// now use the **bitrate-based** CBR formula (`round(audio_bytes · 8 ·
    /// sample_rate ÷ bitrate)`): the earlier first-frame-length frame count was
    /// padding-biased and read long (the `cbr-*-bias` clips below over-read by
    /// ≥2 frames under it — the FIX B RED→GREEN).
    ///
    /// `true_samples` is the per-channel count a full decode yields (cross-checked
    /// out-of-band with ffmpeg/the decoder), committed as a constant so the test
    /// needs no in-process decode.
    #[test]
    fn tag_scan_matches_decoder_cbr_duration_for_l1_l2() {
        use soul_audio_core::open;
        // (name, bytes, samples-per-frame, true decoded per-channel samples).
        let cases: [(&str, &[u8], u64, u64); 4] = [
            (
                "l2_s48_256.mp2",
                include_bytes!("../../soul-audio-core/testdata/l2_s48_256.mp2"),
                1152, // Layer II
                14_976,
            ),
            (
                "layer1-32k-stereo.mp1",
                include_bytes!("../../soul-audio-core/testdata/layer1-32k-stereo.mp1"),
                384, // Layer I
                18_816,
            ),
            // Padding-biased 44.1 kHz clips: old frame-count estimate over-read by
            // ≥2 frames (L2 +2304, L1 +1152); bitrate form is within one frame.
            (
                "cbr-l2-bias.mp2",
                include_bytes!("../../soul-audio-core/testdata/cbr-l2-bias.mp2"),
                1152,
                617_472,
            ),
            (
                "cbr-l1-bias.mp1",
                include_bytes!("../../soul-audio-core/testdata/cbr-l1-bias.mp1"),
                384,
                257_280,
            ),
        ];
        for (name, bytes, spf, true_samples) in cases {
            // LIST path: this tag scan → total per-channel samples.
            let mut src = SliceSource::new(bytes);
            let facts = read_facts(&mut src);
            let tag_total = facts
                .total_frames
                .unwrap_or_else(|| panic!("{name}: tag scan reported no duration"));

            // PLAYBACK path: the decoder's CBR byte estimate.
            let mut dec = open(SliceSource::new(bytes)).expect("open MP1/MP2");
            dec.resolve_lengths();
            let dec_total = dec
                .total_frames()
                .unwrap_or_else(|| panic!("{name}: decoder reported no duration"));

            // (a) The two estimators compute the SAME bitrate-based length.
            assert_eq!(
                tag_total, dec_total,
                "{name}: LIST {tag_total} != PLAYBACK {dec_total}"
            );
            // (b) ...and it is within one MPEG frame of the real decoded length.
            let diff = tag_total.abs_diff(true_samples);
            assert!(
                diff <= spf,
                "{name}: estimate {tag_total} is {diff} samples from the true decoded \
                 {true_samples} (> 1 frame of {spf})"
            );
        }
    }
}
