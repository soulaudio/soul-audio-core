//! Apple CAF (`caff`): stream facts from `desc` (sample rate, channels) and
//! `pakt` (valid frame count), plus title/artist/album from the `info` chunk.
//! A CAF file is an 8-byte `caff` header then chunks of `[type:4][size:i64 BE]
//! [body]`. The core decoder [`soul_audio_core::formats::caf`] handles decoding;
//! this walks the same chunks for metadata.

use crate::locate;
use crate::util::read_exact;
use crate::{Field, StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Walk a CAF container's chunks for facts + `info` tags.
pub fn read_caf<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let mut header = [0u8; 8];
    read_exact(src, &mut header)?;
    if &header[0..4] != b"caff" {
        return Err(TagError::Malformed);
    }

    let mut facts = StreamFacts::default();
    loop {
        let mut ch = [0u8; 12];
        let n = locate::read_full(src, &mut ch)?;
        if n == 0 {
            break; // clean EOF
        }
        if n < 12 {
            return Err(TagError::Malformed);
        }
        let ctype = [ch[0], ch[1], ch[2], ch[3]];
        let size = i64::from_be_bytes([ch[4], ch[5], ch[6], ch[7], ch[8], ch[9], ch[10], ch[11]]);
        let body_start = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;

        if &ctype == b"desc" && size >= 32 {
            // mSampleRate(f64 BE)@0, ... mChannelsPerFrame(u32 BE)@24.
            let mut d = [0u8; 32];
            read_exact(src, &mut d)?;
            let rate = f64::from_be_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
            if rate > 0.0 {
                facts.sample_rate = Some(rate as u32);
            }
            facts.channels = Some(be_u32(&d[24..28]) as u16);
        } else if &ctype == b"pakt" && size >= 16 {
            // mNumberPackets(i64)@0, mNumberValidFrames(i64)@8 — the real length.
            let mut d = [0u8; 16];
            read_exact(src, &mut d)?;
            let valid = i64::from_be_bytes([d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15]]);
            if valid > 0 {
                facts.total_frames = Some(valid as u64);
            }
        } else if &ctype == b"info" && size >= 4 {
            read_info(src, size, scratch, sink)?;
        }

        if size < 0 {
            break; // a `data` chunk may declare size -1 (extends to EOF): stop
        }
        let next = body_start
            .checked_add(size as u64)
            .ok_or(TagError::Malformed)?;
        src.seek(SeekFrom::Start(next)).map_err(|_| TagError::Io)?;
    }

    Ok(facts)
}

/// Parse the `info` chunk body: `u32 BE numEntries`, then NUL-terminated
/// `key`/`value` UTF-8 string pairs. Streamed (not pre-buffered): a value
/// longer than `scratch` (e.g. a long `comment` URL) is consumed to its NUL and
/// its captured prefix dropped, so entries after it — `artist`/`album` — still
/// parse. The caller re-syncs to the chunk end regardless.
fn read_info<S: SeekSource>(
    src: &mut S,
    size: i64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let mut cnt = [0u8; 4];
    read_exact(src, &mut cnt)?;
    let count = u32::from_be_bytes(cnt);
    let limit = size as u64;
    let mut consumed = 4u64;

    for _ in 0..count {
        if consumed >= limit {
            break;
        }
        let mut key = [0u8; 48];
        let (klen, kc, kterm) = read_cstr(src, &mut key, limit - consumed);
        consumed += kc;
        if !kterm || consumed >= limit {
            break;
        }
        let (vlen, vc, vterm) = read_cstr(src, scratch, limit - consumed);
        consumed += vc;
        if !vterm {
            break;
        }
        if let Some(field) = map_key(&key[..klen])
            && let (Ok(k), Ok(v)) = (
                core::str::from_utf8(&key[..klen]),
                core::str::from_utf8(&scratch[..vlen]),
            )
        {
            let v = v.trim();
            if !v.is_empty() {
                sink.text(Some(field), k, v);
            }
        }
    }
    Ok(())
}

/// Read a NUL-terminated string, capturing up to `buf.len()` bytes into `buf`
/// and discarding the rest, bounded by `max` bytes. Returns
/// `(captured_len, bytes_consumed_incl_NUL, found_nul)`.
fn read_cstr<R: embedded_io::Read>(src: &mut R, buf: &mut [u8], max: u64) -> (usize, u64, bool) {
    let mut filled = 0usize;
    let mut consumed = 0u64;
    let mut b = [0u8; 1];
    while consumed < max {
        match src.read(&mut b) {
            Ok(1) => {}
            _ => return (filled, consumed, false),
        }
        consumed += 1;
        if b[0] == 0 {
            return (filled, consumed, true);
        }
        if filled < buf.len() {
            buf[filled] = b[0];
            filled += 1;
        }
    }
    (filled, consumed, false)
}

/// Map a CAF `info` key (case-insensitive) to a normalized [`Field`].
fn map_key(key: &[u8]) -> Option<Field> {
    if key.eq_ignore_ascii_case(b"title") {
        Some(Field::Title)
    } else if key.eq_ignore_ascii_case(b"artist") {
        Some(Field::Artist)
    } else if key.eq_ignore_ascii_case(b"album") {
        Some(Field::Album)
    } else if key.eq_ignore_ascii_case(b"year") || key.eq_ignore_ascii_case(b"date") {
        Some(Field::Date)
    } else if key.eq_ignore_ascii_case(b"genre") {
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

    fn chunk(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(ty);
        v.extend_from_slice(&(body.len() as i64).to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    fn desc(rate: f64, channels: u32) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&rate.to_be_bytes()); // mSampleRate @0
        d.extend_from_slice(b"alac"); // format id @8
        d.extend_from_slice(&0u32.to_be_bytes()); // flags @12
        d.extend_from_slice(&0u32.to_be_bytes()); // bytes/packet @16
        d.extend_from_slice(&4096u32.to_be_bytes()); // frames/packet @20
        d.extend_from_slice(&channels.to_be_bytes()); // channels @24
        d.extend_from_slice(&16u32.to_be_bytes()); // bits @28
        d
    }

    fn pakt(valid_frames: i64) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&0i64.to_be_bytes()); // num packets @0
        d.extend_from_slice(&valid_frames.to_be_bytes()); // valid frames @8
        d.extend_from_slice(&0i32.to_be_bytes()); // priming
        d.extend_from_slice(&0i32.to_be_bytes()); // remainder
        d
    }

    fn info(pairs: &[(&str, &str)]) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
        for (k, v) in pairs {
            d.extend_from_slice(k.as_bytes());
            d.push(0);
            d.extend_from_slice(v.as_bytes());
            d.push(0);
        }
        d
    }

    #[test]
    fn caf_facts_and_info_tags() {
        let mut v = Vec::new();
        v.extend_from_slice(b"caff\0\x01\0\0");
        v.extend_from_slice(&chunk(b"desc", &desc(44_100.0, 2)));
        v.extend_from_slice(&chunk(b"data", &[0u8; 64]));
        v.extend_from_slice(&chunk(b"pakt", &pakt(132_300)));
        v.extend_from_slice(&chunk(
            b"info",
            &info(&[
                ("title", "Aria"),
                ("artist", "Kimiko Ishizaka"),
                ("album", "OGV"),
            ]),
        ));

        let mut src = SliceSource::new(&v);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_caf(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(132_300));
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "title".into(), "Aria".into()))
        );
        assert!(c.fields.contains(&(
            Some(Field::Artist),
            "artist".into(),
            "Kimiko Ishizaka".into()
        )));
        assert!(
            c.fields
                .contains(&(Some(Field::Album), "album".into(), "OGV".into()))
        );
    }
}
