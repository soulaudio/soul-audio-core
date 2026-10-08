//! DFF / DSDIFF (Philips) container tags: stream facts from `PROP`/`SND `
//! (`FS `, `CHNL`) plus the sound-chunk byte size, and human tags from a `DIIN`
//! (edited info) chunk (`DITI` title / `DIAR` artist) and/or a trailing `ID3 `
//! chunk (delegated to [`crate::id3v2::read_id3v2`]).
//!
//! Big-endian IFF (ground truth: the shipping
//! `soul_audio_core::formats::dff::DffDecoder`, whose container parse this
//! mirrors); chunk sizes **exclude** the 12-byte header and odd sizes pad to
//! even. The top-level walk re-aligns to each chunk's padded end, so a `DIIN`
//! or `ID3 ` placed *after* the sound data (the common DSDIFF ordering) is still
//! read.

use crate::util::read_exact;
use crate::{Field, StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::{SeekSource, Source};

/// Read big-endian IFF facts (`FS `/`CHNL` + sound-chunk size) and human tags
/// (`DIIN` → `DITI`/`DIAR`, and/or `ID3 `) from a DSDIFF stream.
///
/// `total_frames` is the per-channel 1-bit sample count: for a raw `DSD ` chunk
/// it is `bytes / channels × 8`; for a DST-compressed `DST ` chunk it is derived
/// from the `FRTE` header (`numFrames × sample_rate / frameRate`), the same
/// figure the decoder's `open_dst` computes.
pub fn read_dff<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let mut top = [0u8; 16];
    read_exact(src, &mut top)?;
    if &top[0..4] != b"FRM8" || &top[12..16] != b"DSD " {
        return Err(TagError::Malformed);
    }

    let mut facts = StreamFacts::default();
    let mut channels: u64 = 0;

    // Walk top-level chunks until a header can't be read (clean EOF).
    while let Ok((id, size)) = read_chunk_header(src) {
        let padded = size.saturating_add(size & 1);
        let body_start = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
        let body_end = body_start.saturating_add(padded);

        match &id {
            b"PROP" => {
                let (sr, ch) = read_prop(src, size)?;
                facts.sample_rate = Some(sr);
                facts.channels = Some(ch);
                channels = u64::from(ch);
            }
            b"DSD " => {
                if channels > 0 {
                    facts.total_frames = Some(size / channels * 8);
                }
            }
            b"DST " => {
                // DST-compressed sound data: its first sub-chunk is
                // `FRTE {numFrames:u32, frameRate:u16}`. Per-channel 1-bit
                // samples = numFrames × (sample_rate / frameRate) — the same
                // figure the decoder's `open_dst` derives, without decoding any
                // frame. (PROP precedes DST, so `sample_rate` is already known.)
                let mut fb = [0u8; 6];
                if let Some(rate) = facts.sample_rate
                    && let Ok((fid, _)) = read_chunk_header(src)
                    && &fid == b"FRTE"
                    && read_exact(src, &mut fb).is_ok()
                {
                    let num_frames = u64::from(u32::from_be_bytes([fb[0], fb[1], fb[2], fb[3]]));
                    let frame_rate = u16::from_be_bytes([fb[4], fb[5]]);
                    if frame_rate != 0 {
                        let spf = u64::from(rate) / u64::from(frame_rate);
                        facts.total_frames = Some(num_frames.saturating_mul(spf));
                    }
                }
            }
            b"DIIN" => read_diin(src, size, scratch, sink)?,
            b"ID3 " => crate::id3v2::read_id3v2(src, scratch, sink)?,
            _ => {}
        }

        // Re-align to the padded chunk boundary regardless of how much the
        // handler consumed (keeps the walk robust to trailing/odd bytes). A
        // seek past EOF on a malformed trailing size ends the walk with the
        // facts gathered so far rather than failing the whole read.
        if src.seek(SeekFrom::Start(body_end)).is_err() {
            break;
        }
    }
    Ok(facts)
}

/// Read a 12-byte IFF header: 4-byte id + big-endian u64 size.
fn read_chunk_header<S: Source>(src: &mut S) -> Result<([u8; 4], u64), TagError> {
    let mut h = [0u8; 12];
    read_exact(src, &mut h)?;
    Ok((
        [h[0], h[1], h[2], h[3]],
        u64::from_be_bytes([h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11]]),
    ))
}

/// Seek forward `n` bytes. A size above `i64::MAX` (malformed) caps at
/// `i64::MAX` so the seek stays forward (lands past EOF → the next read ends
/// the walk) rather than wrapping to a backward seek.
fn skip<S: SeekSource>(src: &mut S, n: u64) -> Result<(), TagError> {
    let step = i64::try_from(n).unwrap_or(i64::MAX);
    src.seek(SeekFrom::Current(step))
        .map_err(|_| TagError::Io)?;
    Ok(())
}

/// Parse `PROP`/`SND ` for `FS `(rate) and `CHNL`(channels); skip the rest.
fn read_prop<S: SeekSource>(src: &mut S, size: u64) -> Result<(u32, u16), TagError> {
    let mut snd = [0u8; 4];
    read_exact(src, &mut snd)?;
    if &snd != b"SND " {
        return Err(TagError::Malformed);
    }
    let mut rem = size.checked_sub(4).ok_or(TagError::Malformed)?;
    let (mut rate, mut channels) = (0u32, 0u16);
    while rem >= 12 {
        let (id, csize) = read_chunk_header(src)?;
        rem -= 12;
        let padded = csize.saturating_add(csize & 1);
        match &id {
            b"FS  " => {
                let mut b = [0u8; 4];
                read_exact(src, &mut b)?;
                rate = u32::from_be_bytes(b);
                skip(src, padded.saturating_sub(4))?;
            }
            b"CHNL" => {
                let mut b = [0u8; 2];
                read_exact(src, &mut b)?;
                channels = u16::from_be_bytes(b);
                skip(src, padded.saturating_sub(2))?;
            }
            _ => skip(src, padded)?,
        }
        rem = rem.saturating_sub(padded);
    }
    Ok((rate, channels))
}

/// Parse a `DIIN` chunk: `DITI`(title)/`DIAR`(artist) sub-chunks, each
/// `<u32 count><count ASCII>`. Emits via the sink; unknown sub-chunks skip.
fn read_diin<S: SeekSource>(
    src: &mut S,
    size: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let mut rem = size;
    while rem >= 12 {
        let (id, csize) = read_chunk_header(src)?;
        rem -= 12;
        let padded = csize.saturating_add(csize & 1);
        let field = match &id {
            b"DITI" => Some(Field::Title),
            b"DIAR" => Some(Field::Artist),
            _ => None,
        };
        match field {
            Some(f) if csize >= 4 => {
                let mut cnt = [0u8; 4];
                read_exact(src, &mut cnt)?;
                let count = u32::from_be_bytes(cnt) as u64;
                let take = count.min(scratch.len() as u64).min(csize - 4) as usize;
                read_exact(src, &mut scratch[..take])?;
                if let Ok(s) = core::str::from_utf8(&scratch[..take]) {
                    sink.text(Some(f), "", s);
                }
                skip(src, padded.saturating_sub(4 + take as u64))?;
            }
            _ => skip(src, padded)?,
        }
        rem = rem.saturating_sub(padded);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::Field;
    use crate::testfix::*;
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    fn be_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(id);
        v.extend_from_slice(&(body.len() as u64).to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    /// Build a raw-DSD DSDIFF: PROP(FS,CHNL), then a `DSD ` sound chunk of
    /// `sound_len` bytes, then a trailing DIIN(DITI,DIAR) — the realistic order
    /// (tags *after* the audio), which also exercises skip-past-sound.
    fn build_dff(title: &str, artist: &str, sound_len: usize) -> Vec<u8> {
        let mut fs = Vec::new();
        fs.extend_from_slice(&2_822_400u32.to_be_bytes());
        let mut chnl = Vec::new();
        chnl.extend_from_slice(&2u16.to_be_bytes());
        chnl.extend_from_slice(b"SLFTSRGT");
        let mut prop = Vec::new();
        prop.extend_from_slice(b"SND ");
        prop.extend_from_slice(&be_chunk(b"FS  ", &fs));
        prop.extend_from_slice(&be_chunk(b"CHNL", &chnl));

        let mut diti = Vec::new();
        diti.extend_from_slice(&(title.len() as u32).to_be_bytes());
        diti.extend_from_slice(title.as_bytes());
        let mut diar = Vec::new();
        diar.extend_from_slice(&(artist.len() as u32).to_be_bytes());
        diar.extend_from_slice(artist.as_bytes());
        let mut diin = Vec::new();
        diin.extend_from_slice(&be_chunk(b"DITI", &diti));
        diin.extend_from_slice(&be_chunk(b"DIAR", &diar));

        let mut body = Vec::new();
        body.extend_from_slice(&be_chunk(b"PROP", &prop));
        body.extend_from_slice(&be_chunk(b"DSD ", &alloc::vec![0u8; sound_len]));
        body.extend_from_slice(&be_chunk(b"DIIN", &diin));

        let mut v = Vec::new();
        v.extend_from_slice(b"FRM8");
        v.extend_from_slice(&((4 + body.len()) as u64).to_be_bytes());
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&body);
        v
    }

    /// Build a DST-compressed DSDIFF: PROP(FS,CHNL) + a `DST ` sound chunk whose
    /// first sub-chunk is `FRTE {numFrames:u32, frameRate:u16}` (the only part
    /// the tags layer reads — it derives duration without decoding frames).
    fn build_dff_dst(num_frames: u32, frame_rate: u16) -> Vec<u8> {
        let mut fs = Vec::new();
        fs.extend_from_slice(&2_822_400u32.to_be_bytes());
        let mut chnl = Vec::new();
        chnl.extend_from_slice(&2u16.to_be_bytes());
        chnl.extend_from_slice(b"SLFTSRGT");
        let mut prop = Vec::new();
        prop.extend_from_slice(b"SND ");
        prop.extend_from_slice(&be_chunk(b"FS  ", &fs));
        prop.extend_from_slice(&be_chunk(b"CHNL", &chnl));

        let mut frte = Vec::new();
        frte.extend_from_slice(&num_frames.to_be_bytes());
        frte.extend_from_slice(&frame_rate.to_be_bytes());
        // DST sound chunk = FRTE then (omitted here) DSTF frames.
        let dst_body = be_chunk(b"FRTE", &frte);

        let mut body = Vec::new();
        body.extend_from_slice(&be_chunk(b"PROP", &prop));
        body.extend_from_slice(&be_chunk(b"DST ", &dst_body));

        let mut v = Vec::new();
        v.extend_from_slice(b"FRM8");
        v.extend_from_slice(&((4 + body.len()) as u64).to_be_bytes());
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn dff_dst_reports_duration_from_frte() {
        // DSD64 (2 822 400 Hz), 75 frames at 75 fps → 1 s. Per-channel samples =
        // num_frames × (sample_rate / frame_rate) = 75 × 37632 = 2 822 400.
        let bytes = build_dff_dst(75, 75);
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 64];
        let mut c = Collect::default();
        let facts = super::read_dff(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(2_822_400));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(2_822_400));
    }

    #[test]
    fn dff_facts_and_diin_tags() {
        // stereo, 16 sound bytes → (16/2)*8 = 64 per-channel samples.
        let bytes = build_dff("My Title", "My Artist", 16);
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = super::read_dff(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(2_822_400));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(64));
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::Title) && v == "My Title")
        );
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::Artist) && v == "My Artist")
        );
    }

    #[test]
    fn dff_facts_from_odd_sound_size() {
        // 32 sound bytes → (32/2)*8 = 128; empty tags → no non-empty fields.
        let bytes = build_dff("", "", 32);
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = super::read_dff(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.total_frames, Some(128));
        assert_eq!(facts.channels, Some(2));
    }

    #[test]
    fn malformed_oversized_chunk_size_degrades_gracefully() {
        // A top-level chunk claiming a u64::MAX size must not panic, wrap into a
        // backward seek, or loop — the walk ends with whatever facts it had.
        let mut body = Vec::new();
        body.extend_from_slice(b"junk");
        body.extend_from_slice(&u64::MAX.to_be_bytes()); // absurd chunk size
        let mut v = Vec::new();
        v.extend_from_slice(b"FRM8");
        v.extend_from_slice(&((4 + body.len()) as u64).to_be_bytes());
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&body);

        let mut src = SliceSource::new(&v);
        let mut scratch = [0u8; 64];
        let mut c = Collect::default();
        // Must return (Ok with no facts, or an Err) — never panic/hang.
        let _ = super::read_dff(&mut src, &mut scratch, &mut c);
    }

    #[test]
    fn non_dsdiff_is_malformed() {
        let mut src = SliceSource::new(b"RIFFxxxxWAVEfmt ");
        let mut scratch = [0u8; 64];
        let mut c = Collect::default();
        assert_eq!(
            super::read_dff(&mut src, &mut scratch, &mut c),
            Err(crate::TagError::Malformed)
        );
    }
}
