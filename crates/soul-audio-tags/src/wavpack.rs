//! WavPack (`.wv`): stream facts from the first 32-byte block header, plus
//! title/artist/album from a trailing APEv2 tag (the format ffmpeg/`wvtag`
//! write). The core decoder [`soul_audio_core::formats::wavpack`] owns full
//! decoding; this reader only needs the header's sample count / rate / channel
//! flags and the APEv2 items at the file tail.

use crate::util::read_exact;
use crate::{Field, StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

/// Standard WavPack sample rates, indexed by the 4-bit `srate` flags field;
/// index 15 means "non-standard, in a sub-block" (left unknown here).
const SAMPLE_RATES: [u32; 15] = [
    6000, 8000, 9600, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000,
    192_000,
];

/// Read WavPack stream facts + a trailing APEv2 tag.
pub fn read_wavpack<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    // First block's 32-byte header: `wvpk` ckSize(4) version(2) · idx/total hi
    // bytes(2) · total_samples(4)@12 · block_index(4) · block_samples(4) ·
    // flags(4)@24 · crc(4). All little-endian.
    let mut h = [0u8; 32];
    read_exact(src, &mut h)?;
    if &h[0..4] != b"wvpk" {
        return Err(TagError::Malformed);
    }
    let total_samples = u32::from_le_bytes([h[12], h[13], h[14], h[15]]);
    let flags = u32::from_le_bytes([h[24], h[25], h[26], h[27]]);

    let mut facts = StreamFacts::default();
    let srate_idx = ((flags & 0x0780_0000) >> 23) as usize;
    if srate_idx < SAMPLE_RATES.len() {
        facts.sample_rate = Some(SAMPLE_RATES[srate_idx]);
    }
    facts.channels = Some(if flags & 0x0000_0004 != 0 { 1 } else { 2 }); // FLAG_MONO
    // 0xFFFF_FFFF = "unknown length"; 0 is also treated as unknown.
    if total_samples != 0 && total_samples != u32::MAX {
        facts.total_frames = Some(u64::from(total_samples));
    }

    // APEv2 tag (if any) sits at the very end: a 32-byte footer whose
    // `APETAGEX` magic we locate, then walk its items back-to-front-sized.
    read_apev2(src, scratch, sink)?;
    Ok(facts)
}

/// Parse a trailing APEv2 tag if present. Footer (last 32 bytes): `APETAGEX`(8)
/// version(4) tag_size(4, incl. footer) item_count(4) flags(4) reserved(8).
/// Each item: value_size(4) flags(4) key(NUL-terminated) value(value_size).
/// Absent/short/garbled tag → `Ok(())` (facts still stand).
fn read_apev2<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let end = src.seek(SeekFrom::End(0)).map_err(|_| TagError::Io)?;
    if end < 32 {
        return Ok(());
    }
    src.seek(SeekFrom::Start(end - 32))
        .map_err(|_| TagError::Io)?;
    let mut foot = [0u8; 32];
    if read_exact(src, &mut foot).is_err() || &foot[0..8] != b"APETAGEX" {
        return Ok(());
    }
    let tag_size = u32::from_le_bytes([foot[12], foot[13], foot[14], foot[15]]) as u64;
    let item_count = u32::from_le_bytes([foot[16], foot[17], foot[18], foot[19]]);
    if tag_size < 32 || tag_size > end {
        return Ok(());
    }
    // Items occupy [end - tag_size, end - 32) (tag_size includes the footer).
    let mut pos = end - tag_size;
    let items_end = end - 32;

    for _ in 0..item_count {
        if pos + 8 > items_end {
            break;
        }
        src.seek(SeekFrom::Start(pos)).map_err(|_| TagError::Io)?;
        let mut hdr = [0u8; 8];
        if read_exact(src, &mut hdr).is_err() {
            break;
        }
        let value_size = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;

        // Read a bounded probe to find the NUL-terminated key; the value starts
        // right after the NUL.
        let key_start = pos + 8;
        let probe_len = (items_end - key_start).min(64) as usize;
        let mut key_buf = [0u8; 64];
        if read_exact(src, &mut key_buf[..probe_len]).is_err() {
            break;
        }
        let Some(nul) = key_buf[..probe_len].iter().position(|&b| b == 0) else {
            break;
        };
        let field = map_key(&key_buf[..nul]);
        let value_start = key_start + nul as u64 + 1;

        if let Some(field) = field {
            let want = value_size.min(scratch.len() as u64) as usize;
            src.seek(SeekFrom::Start(value_start))
                .map_err(|_| TagError::Io)?;
            if read_exact(src, &mut scratch[..want]).is_ok()
                && let Ok(s) = core::str::from_utf8(&scratch[..want])
            {
                let s = s.trim_end_matches('\0').trim();
                if !s.is_empty() {
                    let raw = core::str::from_utf8(&key_buf[..nul]).unwrap_or("");
                    sink.text(Some(field), raw, s);
                }
            }
        }
        pos = value_start + value_size;
    }
    Ok(())
}

/// Map an APEv2 key (case-insensitive) to a normalized [`Field`].
fn map_key(key: &[u8]) -> Option<Field> {
    if key.eq_ignore_ascii_case(b"Title") {
        Some(Field::Title)
    } else if key.eq_ignore_ascii_case(b"Artist") {
        Some(Field::Artist)
    } else if key.eq_ignore_ascii_case(b"Album") {
        Some(Field::Album)
    } else if key.eq_ignore_ascii_case(b"Track") {
        Some(Field::TrackNumber)
    } else if key.eq_ignore_ascii_case(b"Year") || key.eq_ignore_ascii_case(b"Date") {
        Some(Field::Date)
    } else if key.eq_ignore_ascii_case(b"Genre") {
        Some(Field::Genre)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfix::Collect;
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    fn wvpk_header(total_samples: u32, srate_idx: u32, mono: bool) -> [u8; 32] {
        let mut h = [0u8; 32];
        h[0..4].copy_from_slice(b"wvpk");
        h[12..16].copy_from_slice(&total_samples.to_le_bytes());
        let mut flags = srate_idx << 23;
        if mono {
            flags |= 0x0000_0004;
        }
        h[24..28].copy_from_slice(&flags.to_le_bytes());
        h
    }

    fn apev2(items: &[(&str, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (k, v) in items {
            body.extend_from_slice(&(v.len() as u32).to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes()); // item flags
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v.as_bytes());
        }
        let tag_size = (body.len() + 32) as u32;
        let footer = |v: &mut Vec<u8>| {
            v.extend_from_slice(b"APETAGEX");
            v.extend_from_slice(&2000u32.to_le_bytes());
            v.extend_from_slice(&tag_size.to_le_bytes());
            v.extend_from_slice(&(items.len() as u32).to_le_bytes());
            v.extend_from_slice(&0u32.to_le_bytes());
            v.extend_from_slice(&[0u8; 8]);
        };
        let mut out = body;
        footer(&mut out);
        out
    }

    #[test]
    fn facts_from_header_and_apev2_tags() {
        let mut v = Vec::new();
        v.extend_from_slice(&wvpk_header(132_300, 9, false)); // idx 9 = 44100, stereo
        v.extend_from_slice(&[0u8; 100]); // some block body
        v.extend_from_slice(&apev2(&[
            ("Title", "Aria"),
            ("Artist", "Kimiko Ishizaka"),
            ("Album", "OGV"),
        ]));

        let mut src = SliceSource::new(&v);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_wavpack(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(132_300));
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "Title".into(), "Aria".into()))
        );
        assert!(c.fields.contains(&(
            Some(Field::Artist),
            "Artist".into(),
            "Kimiko Ishizaka".into()
        )));
        assert!(
            c.fields
                .contains(&(Some(Field::Album), "Album".into(), "OGV".into()))
        );
    }

    #[test]
    fn no_apev2_tag_still_gives_facts() {
        let mut v = Vec::new();
        v.extend_from_slice(&wvpk_header(1000, 10, true)); // idx 10 = 48000, mono
        v.extend_from_slice(&[0u8; 64]);
        let mut src = SliceSource::new(&v);
        let mut scratch = [0u8; 128];
        let mut c = Collect::default();
        let facts = read_wavpack(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(48_000));
        assert_eq!(facts.channels, Some(1));
        assert_eq!(facts.total_frames, Some(1000));
        assert!(c.fields.is_empty());
    }
}
