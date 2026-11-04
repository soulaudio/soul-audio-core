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
//! - Layer I/II/III are all parsed (so `.mp2`/`.mp1` report a duration too);
//!   a reserved-layer or reserved-version header is treated as sync noise and
//!   skipped over. Xing/Info/VBRI is a Layer III convention — Layer I/II (CBR
//!   in practice) fall through to the file-length × bitrate estimate below.
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

/// Bitrate (kbps) by 4-bit index per (MPEG version, layer), `0` = free/reserved
/// (never a valid frame here — the free-format case is out of scope, see module
/// docs). MPEG-2/2.5 share one table for Layers II and III.
const BR_V1_L1: [u32; 16] = [
    0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
];
const BR_V1_L2: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
];
const BR_V1_L3: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];
const BR_V2_L1: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
];
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

/// A parsed, validated MPEG audio frame header (Layer I/II/III).
#[derive(Clone, Copy)]
struct FrameHeader {
    mpeg1: bool,
    sample_rate: u32,
    channels: u16,
    samples_per_frame: u32,
    bitrate_bps: u32,
    /// 2 if the protection bit indicates a trailing 16-bit CRC, else 0 — the
    /// Xing/Info tag sits after this, in addition to the side-info bytes.
    crc_bytes: u64,
}

/// Parse+validate a 4-byte MPEG audio frame header (Layer I/II/III). `None` for
/// anything that isn't a supported (non-free-format, non-reserved) frame — the
/// caller treats that as sync noise and keeps scanning. Duration is derived from
/// the bitrate + samples-per-frame (both layer-dependent), so Layer II (`.mp2`)
/// and Layer I (`.mp1`) report a track length too, not just Layer III.
fn parse_header(h: &[u8; 4]) -> Option<FrameHeader> {
    if h[0] != 0xFF || (h[1] & 0xE0) != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 0x3; // 0=2.5, 1=reserved, 2=v2, 3=v1
    let layer_code = (h[1] >> 1) & 0x3; // 01=III, 10=II, 11=I, 00=reserved
    if version == 1 || layer_code == 0 {
        return None;
    }
    let layer = match layer_code {
        1 => 3,
        2 => 2,
        _ => 1,
    };
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
    let bitrate_kbps = match (mpeg1, layer) {
        (true, 1) => BR_V1_L1[br_idx],
        (true, 2) => BR_V1_L2[br_idx],
        (true, _) => BR_V1_L3[br_idx],
        (false, 1) => BR_V2_L1[br_idx],
        (false, _) => BR_V2_L23[br_idx], // Layer II and III share the MPEG-2/2.5 table
    };
    if bitrate_kbps == 0 {
        return None;
    }

    let channels = if chan_mode == 3 { 1 } else { 2 };
    // Samples per frame: Layer I = 384; Layer II = 1152 (all versions); Layer III
    // = 1152 (MPEG-1) / 576 (MPEG-2/2.5).
    let samples_per_frame = match layer {
        1 => 384,
        2 => 1152,
        _ if mpeg1 => 1152,
        _ => 576,
    };
    let crc_bytes = if h[1] & 0x1 == 0 { 2 } else { 0 }; // protection bit 0 = CRC present

    Some(FrameHeader {
        mpeg1,
        sample_rate,
        channels,
        samples_per_frame,
        bitrate_bps: bitrate_kbps * 1000,
        crc_bytes,
    })
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
/// within [`SYNC_SCAN_WINDOW`] bytes, for the first valid MPEG audio frame
/// sync (any layer). On success, `src` is left positioned just past the 4-byte header
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
            // No VBR header: approximate from the file length and this frame's
            // (assumed constant) bitrate. Compute per-channel samples in one
            // u128 expression — `bytes*8*rate/bitrate` — so sub-second files
            // keep their length instead of truncating to 0 whole seconds. Still
            // an approximation (wrong for an untagged VBR stream).
            let file_len = src.seek(SeekFrom::End(0)).ok()?;
            let audio_bytes = file_len.saturating_sub(audio_start);
            let samples = u128::from(audio_bytes) * 8 * u128::from(header.sample_rate)
                / u128::from(header.bitrate_bps);
            u64::try_from(samples).unwrap_or(u64::MAX)
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
        assert_eq!(f.bitrate_bps, 128_000);
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
    fn parses_layer2_and_rejects_reserved_headers() {
        // Layer II (layer bits = 10) is now parsed: 1152 samples/frame and the
        // Layer II bitrate table (MPEG-1 idx 9 = 160 kbps).
        let mut l2 = header_v1(9, 0, false, true);
        l2[1] = (l2[1] & !0b0000_0110) | (0b10 << 1);
        let f = parse_header(&l2).expect("Layer II is a valid header");
        assert_eq!(f.samples_per_frame, 1152);
        assert_eq!(f.bitrate_bps, 160_000);

        // Reserved layer (bits 00) is still rejected.
        let mut resv = header_v1(9, 0, false, true);
        resv[1] &= !0b0000_0110;
        assert!(parse_header(&resv).is_none());

        // Reserved version (bits 3-4 of byte1 = 01).
        let mut v = header_v1(9, 0, false, true);
        v[1] = (v[1] & !0b0001_1000) | (0b01 << 3);
        assert!(parse_header(&v).is_none());

        // Free-format bitrate (index 0).
        assert!(parse_header(&header_v1(0, 0, false, true)).is_none());
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
        // 128 kbps = 16,000 bytes/sec; a 16,000-byte "file" is ~1 second of
        // audio at 44,100 Hz => total_frames ~= 44,100. All-zero body (after
        // the header) never spells "Xing"/"Info"/"VBRI", so this exercises
        // the CBR estimate, not the VBR-tag path.
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
}
