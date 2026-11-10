//! Bare ADTS (`.aac`) duration facts. A raw AAC elementary stream has no
//! metadata container, so this only frame-scans the 7-byte ADTS headers —
//! advancing by the 13-bit `aac_frame_length`, counting 1024 PCM samples per
//! frame — to recover a duration the library view can show. Mirrors the header
//! fields `soul_audio_core::demux::adts` parses.

use crate::util::read_exact;
use crate::{StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

/// `sampling_frequency_index` → Hz (reserved indices `>= 13` are rejected).
const AAC_SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Parse a 7-byte ADTS header → (core sample rate, channels, whole-frame length
/// in bytes). `Err` on a bad syncword/layer or a reserved sample-rate index.
fn parse_header(h: &[u8; 7]) -> Result<(u32, u16, usize), TagError> {
    // syncword(12)==0xFFF and layer(2)==00 (0x06 mask); layer!=00 is an MP3 frame.
    if h[0] != 0xFF || (h[1] & 0xF6) != 0xF0 {
        return Err(TagError::Malformed);
    }
    let sfi = usize::from((h[2] >> 2) & 0xF);
    let rate = *AAC_SAMPLE_RATES.get(sfi).ok_or(TagError::Malformed)?;
    let chan_cfg = ((h[2] & 0x1) << 2) | ((h[3] >> 6) & 0x3);
    let channels = match chan_cfg {
        0 => 2, // "from PCE" — assume stereo for a facts estimate
        7 => 8, // 7.1
        c => u16::from(c),
    };
    let frame_len = (usize::from(h[3] & 0x03) << 11)
        | (usize::from(h[4]) << 3)
        | (usize::from(h[5] >> 5) & 0x07);
    Ok((rate, channels, frame_len))
}

/// How far forward [`resync`] scans for the next valid frame boundary before
/// giving up. The 13-bit `aac_frame_length` caps one frame at 8191 bytes, so a
/// two-frame span always contains a full candidate frame plus the following
/// header used to double-validate it — enough to step over any single garbled
/// frame or inter-frame gap.
const RESYNC_SPAN: u64 = 2 * 8192;

/// Outcome of reading a 7-byte header at an absolute offset.
enum At {
    /// A valid header whose `aac_frame_length` is `usize`.
    Valid(usize),
    /// Clean end of stream (nothing, or a short tail, at this boundary).
    Eof,
    /// Bytes present but not a valid ADTS header (garbage / false sync).
    Bad,
}

/// Seek to `off` and classify the 7 bytes there. A clean 0-byte read, or a
/// short read, is end-of-stream; a full-but-unparsable read is [`At::Bad`].
fn header_at<S: SeekSource>(src: &mut S, off: u64, h: &mut [u8; 7]) -> Result<At, TagError> {
    if src.seek(SeekFrom::Start(off)).is_err() {
        return Ok(At::Bad);
    }
    match read_up_to_7(src, h)? {
        7 => Ok(parse_header(h).map_or(At::Bad, |(_, _, len)| At::Valid(len))),
        _ => Ok(At::Eof), // 0 bytes = clean EOF; 1..7 = truncated tail, treat as end
    }
}

/// Scan forward from `from` for the next ADTS frame boundary, validated by the
/// frame that *follows* it (two consecutive valid syncwords), so a false sync
/// inside an AAC payload isn't mistaken for a boundary. Mirrors the resync in
/// `soul_audio_core::demux::adts`. Returns the boundary offset and its frame
/// length, or `None` at genuine EOF / when nothing validates within
/// [`RESYNC_SPAN`]. Candidates with `frame_len < 7` are rejected so a boundary
/// always makes forward progress.
fn resync<S: SeekSource>(
    src: &mut S,
    from: u64,
    h: &mut [u8; 7],
) -> Result<Option<(u64, usize)>, TagError> {
    let mut p = from;
    let end = from + RESYNC_SPAN;
    while p <= end {
        // `len >= 7` keeps a resynced boundary strictly ahead of the previous
        // frame; a degenerate sub-header length can't stall the walk.
        if let At::Valid(len) = header_at(src, p, h)?
            && len >= 7
        {
            // Double-validate: the next frame's header must also parse, or this
            // must be the final frame (its successor at/after EOF).
            match header_at(src, p + len as u64, h)? {
                At::Valid(_) | At::Eof => return Ok(Some((p, len))),
                At::Bad => {} // false sync inside a payload — keep scanning
            }
        }
        p += 1;
    }
    Ok(None)
}

/// Frame-scan a raw ADTS stream for duration facts. No tags are emitted (raw
/// ADTS carries no metadata container), so `scratch`/`sink` are unused.
///
/// Walks the 7-byte ADTS headers, counting 1024 PCM samples per frame. On a
/// garbled frame or an inter-frame gap (a header that fails to parse where the
/// next one is expected) it **resyncs forward** to the next double-validated
/// syncword and keeps counting, rather than stopping and under-reporting the
/// duration — matching the robustness of the core `demux::adts` demuxer. The
/// scan ends only at a genuine end of stream. `pos` strictly increases every
/// step (each accepted boundary is past the previous one, and [`resync`]
/// rejects sub-7-byte frames), so the walk always terminates.
pub fn read_adts<S: SeekSource>(
    src: &mut S,
    _scratch: &mut [u8],
    _sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    // `src` is positioned at the first ADTS header (byte 0 of a bare `.aac`, or
    // just past a leading ID3v2 tag the MP3 container reader skipped for us).
    let start = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
    let mut h = [0u8; 7];
    read_exact(src, &mut h)?; // at least one frame, else genuinely malformed
    let (rate, channels, first_len) = parse_header(&h)?;

    let mut frames: u64 = 1; // the first header, already validated, counts
    let mut pos = start;
    let mut frame_len = first_len;
    loop {
        // Next frame begins `frame_len` bytes on; `.max(pos + 1)` guarantees
        // progress even for a degenerate (<7) first-frame length.
        let next = (pos + frame_len as u64).max(pos + 1);
        match header_at(src, next, &mut h)? {
            At::Valid(len) => {
                frames += 1;
                pos = next;
                frame_len = len;
            }
            At::Eof => break, // genuine end of stream — done
            At::Bad => match resync(src, next, &mut h)? {
                Some((boundary, len)) => {
                    frames += 1;
                    pos = boundary;
                    frame_len = len;
                }
                None => break, // no valid frame ahead within the span → end
            },
        }
    }

    Ok(StreamFacts {
        sample_rate: Some(rate),
        channels: Some(channels),
        total_frames: Some(frames * 1024),
    })
}

/// Read up to 7 bytes; returns how many were read (0 at clean EOF).
fn read_up_to_7<S: SeekSource>(src: &mut S, buf: &mut [u8; 7]) -> Result<usize, TagError> {
    let mut off = 0;
    while off < buf.len() {
        match embedded_io::Read::read(src, &mut buf[off..]).map_err(|_| TagError::Io)? {
            0 => break,
            k => off += k,
        }
    }
    Ok(off)
}

#[cfg(test)]
mod tests {
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    /// Build `n` ADTS frames, each a 7-byte header + `payload` bytes, at 44.1 kHz
    /// stereo (sfi=4, channel_config=2). frame_len = 7 + payload.
    fn adts(n: usize, payload: usize) -> Vec<u8> {
        let frame_len = (7 + payload) as u32;
        let mut v = Vec::new();
        for _ in 0..n {
            v.push(0xFF); // sync hi
            v.push(0xF1); // sync lo + layer 00 + protection_absent=1
            v.push(0x50); // profile=01(LC) | sfi=0100(4) | priv=0 | chan_cfg hi=0
            v.push(0x80 | ((frame_len >> 11) & 0x03) as u8); // chan_cfg lo=10 | fl[12:11]
            v.push(((frame_len >> 3) & 0xFF) as u8); // fl[10:3]
            v.push((((frame_len & 0x07) << 5) as u8) | 0x1F); // fl[2:0] | buffer-fullness hi
            v.push(0xFC); // buffer-fullness lo | num_raw_blocks=0
            v.extend(core::iter::repeat_n(0u8, payload));
        }
        v
    }

    #[test]
    fn frame_scan_reports_duration_facts() {
        // 86 frames × 1024 / 44100 ≈ 1.997 s.
        let bytes = adts(86, 100);
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 16];
        let mut sink = crate::testfix::Collect::default();
        let facts = super::read_adts(&mut src, &mut scratch, &mut sink).unwrap();
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(86 * 1024));
        assert!(sink.fields.is_empty()); // raw ADTS carries no tags
    }

    #[test]
    fn empty_or_nonadts_is_malformed() {
        let mut src = SliceSource::new(b"\0\0\0\0\0\0\0");
        let mut scratch = [0u8; 16];
        let mut sink = crate::testfix::Collect::default();
        assert_eq!(
            super::read_adts(&mut src, &mut scratch, &mut sink),
            Err(crate::TagError::Malformed)
        );
    }

    /// A garbled frame / inter-frame gap mid-stream must be **resynced past**,
    /// not treated as the end: two 50-frame runs separated by a non-sync junk
    /// gap total 100 frames. The old scanner broke at the first unparsable
    /// header and kept only the 50 frames before the gap (under-reporting the
    /// duration by half); the resync walk counts all 100.
    #[test]
    fn resync_skips_midstream_gap_and_counts_all_frames() {
        let mut bytes = adts(50, 100); // run 1: 50 frames
        bytes.extend_from_slice(&[0u8; 40]); // non-sync junk (never 0xFFF)
        bytes.extend_from_slice(&adts(50, 100)); // run 2: 50 frames
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 16];
        let mut sink = crate::testfix::Collect::default();
        let facts = super::read_adts(&mut src, &mut scratch, &mut sink).unwrap();
        assert_eq!(facts.sample_rate, Some(44_100));
        // RED (old): Some(50 * 1024). GREEN (resync): Some(100 * 1024).
        assert_eq!(facts.total_frames, Some(100 * 1024));
    }

    /// A false resync must not inflate the count: a lone trailing `0xFF` that
    /// looks like the start of a sync — but has no validating successor frame —
    /// is junk at the tail, so the two clean frames before it are the whole
    /// count (double-validation rejects the false boundary).
    #[test]
    fn resync_rejects_false_sync_at_tail() {
        let mut bytes = adts(2, 50);
        bytes.extend_from_slice(&[0xFF, 0xF1, 0x50, 0x80]); // sync-like, but truncated
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 16];
        let mut sink = crate::testfix::Collect::default();
        let facts = super::read_adts(&mut src, &mut scratch, &mut sink).unwrap();
        assert_eq!(facts.total_frames, Some(2 * 1024));
    }

    /// An ID3v2-wrapped raw ADTS (`.aac`) stream must report the **same**
    /// duration as its un-wrapped twin: both go to the ADTS frame scanner. The
    /// magic sniff sees only "ID3" and routes to the MPEG container reader,
    /// which (before the fix) latched a false MPEG sync inside the AAC payload
    /// and over-reported the duration. These are the committed `aac`/`aac-id3`
    /// fixtures (same audio; one carries a leading ID3v2 tag).
    #[test]
    fn id3_wrapped_adts_matches_unwrapped_duration() {
        const AAC: &[u8] = include_bytes!("../../soul-audio-core/testdata/aac.aac");
        const AAC_ID3: &[u8] = include_bytes!("../../soul-audio-core/testdata/aac-id3.aac");
        let read = |bytes: &[u8]| {
            let mut src = SliceSource::new(bytes);
            let mut scratch = [0u8; 4096];
            let mut sink = crate::testfix::Collect::default();
            crate::read(&mut src, &mut scratch, &mut sink).unwrap()
        };
        let plain = read(AAC);
        let wrapped = read(AAC_ID3);
        assert_eq!(plain.sample_rate, Some(44_100));
        assert_eq!(plain.total_frames, Some(23 * 1024)); // 23 ADTS frames
        // RED (old): wrapped.total_frames was Some(49_536) via a false MP3 sync.
        assert_eq!(wrapped.total_frames, plain.total_frames);
        assert_eq!(wrapped.sample_rate, plain.sample_rate);
    }
}
