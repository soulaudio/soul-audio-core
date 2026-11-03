//! WAV/RIFF container: `fmt `/`data` chunks for stream facts, plus an
//! embedded `id3 `/`ID3 ` chunk carrying an ID3v2 tag (delegated to
//! [`crate::id3v2::read_id3v2`]).

use crate::id3v2;
use crate::locate;
use crate::util::read_exact;
use crate::{Field, StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

/// Walk a RIFF/WAVE container's top-level chunks. Returns `StreamFacts` from
/// `fmt `/`data`; delivers text fields (and any picture) found in an embedded
/// `id3 `/`ID3 ` chunk to `sink`.
pub fn read_wav<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let mut riff_header = [0u8; 12];
    read_exact(src, &mut riff_header)?;
    if &riff_header[0..4] != b"RIFF" || &riff_header[8..12] != b"WAVE" {
        return Err(TagError::Malformed);
    }

    let mut sample_rate = None;
    let mut channels = None;
    let mut block_align: Option<u32> = None;
    let mut data_size: Option<u32> = None;

    loop {
        let mut chunk_header = [0u8; 8];
        let n = locate::read_full(src, &mut chunk_header)?;
        if n == 0 {
            break; // clean EOF: no more chunks
        }
        if n < 8 {
            return Err(TagError::Malformed); // truncated chunk header
        }

        let chunk_id = &chunk_header[0..4];
        let chunk_size = u32::from_le_bytes([
            chunk_header[4],
            chunk_header[5],
            chunk_header[6],
            chunk_header[7],
        ]);
        let body_start = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
        let body_end = body_start
            .checked_add(u64::from(chunk_size))
            .ok_or(TagError::Malformed)?;

        if chunk_id == b"fmt " {
            if chunk_size < 16 {
                return Err(TagError::Malformed);
            }
            let mut fmt_body = [0u8; 16];
            read_exact(src, &mut fmt_body)?;
            channels = Some(u16::from_le_bytes([fmt_body[2], fmt_body[3]]));
            sample_rate = Some(u32::from_le_bytes([
                fmt_body[4],
                fmt_body[5],
                fmt_body[6],
                fmt_body[7],
            ]));
            block_align = Some(u32::from(u16::from_le_bytes([fmt_body[12], fmt_body[13]])));
        } else if chunk_id == b"data" {
            data_size = Some(chunk_size);
        } else if chunk_id == b"id3 " || chunk_id == b"ID3 " {
            // Cursor is already at `body_start` (right past the 8-byte chunk
            // header); the ID3v2 tag starts here, and `read_id3v2` reads picture
            // offsets from the live (absolute) source position.
            id3v2::read_id3v2(src, scratch, sink)?;
        } else if chunk_id == b"LIST" {
            // A `LIST` of form `INFO` carries text fields (INAM/IART/IPRD/...),
            // the common WAV tagging written by ffmpeg and others.
            read_info_list(src, chunk_size, body_end, scratch, sink)?;
        }

        // Re-sync to the chunk's declared end (padded to even), regardless of
        // how many bytes the handler above actually consumed.
        let padded_end = if chunk_size % 2 == 1 {
            body_end + 1
        } else {
            body_end
        };
        src.seek(SeekFrom::Start(padded_end))
            .map_err(|_| TagError::Io)?;
    }

    let total_frames = match (data_size, block_align) {
        (Some(size), Some(ba)) if ba > 0 => Some(u64::from(size) / u64::from(ba)),
        _ => None,
    };

    Ok(StreamFacts {
        sample_rate,
        channels,
        total_frames,
    })
}

/// Parse a RIFF `LIST` chunk of form `INFO` (the common WAV text tags). The
/// cursor is at the LIST body start; `body_end` is its declared end. Each entry
/// is `<4cc><u32 LE size><bytes, NUL-padded to even>`. Leaves the cursor
/// anywhere — the caller re-syncs to the (padded) chunk end.
fn read_info_list<S: SeekSource>(
    src: &mut S,
    chunk_size: u32,
    body_end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    if chunk_size < 4 {
        return Ok(());
    }
    let mut form = [0u8; 4];
    read_exact(src, &mut form)?;
    if &form != b"INFO" {
        return Ok(()); // a non-INFO LIST (e.g. `adtl`) — nothing to read here
    }
    loop {
        let pos = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
        if pos + 8 > body_end {
            break;
        }
        let mut hdr = [0u8; 8];
        if locate::read_full(src, &mut hdr)? < 8 {
            break;
        }
        let id = [hdr[0], hdr[1], hdr[2], hdr[3]];
        let size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let want = (size as usize).min(scratch.len());
        read_exact(src, &mut scratch[..want])?;
        let end = scratch[..want].iter().position(|&b| b == 0).unwrap_or(want);
        if let Ok(s) = core::str::from_utf8(&scratch[..end]) {
            let s = s.trim();
            if !s.is_empty() {
                let raw = core::str::from_utf8(&id).unwrap_or("");
                sink.text(map_info_field(&id), raw, s);
            }
        }
        // Absolute-seek to the next entry (word-aligned), fixing any under-read.
        let next = pos + 8 + u64::from(size) + u64::from(size & 1);
        src.seek(SeekFrom::Start(next)).map_err(|_| TagError::Io)?;
    }
    Ok(())
}

/// Map a RIFF `INFO` four-cc to a normalized [`Field`], if one applies.
fn map_info_field(id: &[u8; 4]) -> Option<Field> {
    match id {
        b"INAM" => Some(Field::Title),
        b"IART" => Some(Field::Artist),
        b"IPRD" => Some(Field::Album),
        b"ICRD" => Some(Field::Date),
        b"IGNR" => Some(Field::Genre),
        b"ITRK" => Some(Field::TrackNumber),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::Field;
    use crate::testfix::*;
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    /// A generic RIFF chunk: 4-byte id + u32 le size + body, padded to even.
    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(id);
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        if body.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    /// A minimal PCM `fmt ` chunk body (16 bytes: wFormatTag, channels,
    /// sample rate, byte rate, block align, bits per sample).
    fn fmt_chunk(channels: u16, sample_rate: u32, bits_per_sample: u16) -> Vec<u8> {
        let block_align = channels * (bits_per_sample / 8);
        let byte_rate = sample_rate * u32::from(block_align);
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_le_bytes()); // PCM
        body.extend_from_slice(&channels.to_le_bytes());
        body.extend_from_slice(&sample_rate.to_le_bytes());
        body.extend_from_slice(&byte_rate.to_le_bytes());
        body.extend_from_slice(&block_align.to_le_bytes());
        body.extend_from_slice(&bits_per_sample.to_le_bytes());
        chunk(b"fmt ", &body)
    }

    /// A minimal WAV: `fmt ` + `data` (100 stereo 16-bit frames) + an `id3 `
    /// chunk holding a tiny ID3v2.3 tag with a `TIT2` frame.
    fn wav_with_id3(title: &str) -> Vec<u8> {
        let fmt = fmt_chunk(2, 44_100, 16);
        let data_body = alloc::vec![0u8; 4 * 100]; // 100 frames * 4-byte block align
        let data = chunk(b"data", &data_body);
        let id3_tag = id3v23(&[text_frame(b"TIT2", title)]);
        let id3_chunk = chunk(b"id3 ", &id3_tag);

        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(&fmt);
        body.extend_from_slice(&data);
        body.extend_from_slice(&id3_chunk);

        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(&body);
        v
    }

    /// A minimal WAV with no `id3 `/`ID3 ` chunk at all.
    fn wav_without_id3() -> Vec<u8> {
        let fmt = fmt_chunk(1, 48_000, 16);
        let data_body = alloc::vec![0u8; 2 * 10]; // 10 mono 16-bit frames
        let data = chunk(b"data", &data_body);

        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(&fmt);
        body.extend_from_slice(&data);

        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(&body);
        v
    }

    /// A `LIST`/`INFO` chunk: form `INFO` then `INAM`/`IART`/`IPRD` entries —
    /// which share RIFF's own 4cc + u32-le size + even-padding layout.
    fn info_list(title: &str, artist: &str, album: &str) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"INFO");
        body.extend_from_slice(&chunk(b"INAM", title.as_bytes()));
        body.extend_from_slice(&chunk(b"IART", artist.as_bytes()));
        body.extend_from_slice(&chunk(b"IPRD", album.as_bytes()));
        chunk(b"LIST", &body)
    }

    /// A minimal WAV whose tags live in a `LIST`/`INFO` chunk (what ffmpeg
    /// writes), not an `id3 ` chunk.
    fn wav_with_info(title: &str, artist: &str, album: &str) -> Vec<u8> {
        let fmt = fmt_chunk(2, 44_100, 16);
        let data = chunk(b"data", &alloc::vec![0u8; 4 * 50]); // 50 stereo frames
        let list = info_list(title, artist, album);

        let mut body = Vec::new();
        body.extend_from_slice(b"WAVE");
        body.extend_from_slice(&fmt);
        body.extend_from_slice(&list);
        body.extend_from_slice(&data);

        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn wav_facts_and_embedded_id3() {
        let bytes = wav_with_id3("Song X");
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = super::read_wav(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(100));
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::Title) && v == "Song X")
        );
    }

    #[test]
    fn wav_reads_list_info_tags() {
        let bytes = wav_with_info("Aria", "Kimiko Ishizaka", "Open Goldberg Variations");
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = super::read_wav(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.total_frames, Some(50));
        assert!(
            c.fields
                .iter()
                .any(|(f, k, v)| *f == Some(Field::Title) && k == "INAM" && v == "Aria")
        );
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::Artist) && v == "Kimiko Ishizaka")
        );
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::Album) && v == "Open Goldberg Variations")
        );
    }

    #[test]
    fn wav_without_id3_chunk_returns_facts_only() {
        let bytes = wav_without_id3();
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = super::read_wav(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(48_000));
        assert_eq!(facts.channels, Some(1));
        assert_eq!(facts.total_frames, Some(10));
        assert!(c.fields.is_empty());
    }
}
