use crate::TagError;
use embedded_io::{Read, SeekFrom};
use soul_audio_core::SeekSource;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Container {
    Flac,
    Ogg,
    Id3,
    Wav,
    Aiff,
    Mp4,
    Dsf,
    Dff,
    Adts,
    WavPack,
    Caf,
    Matroska,
    Unknown,
}

/// Read the leading bytes, classify by magic, then rewind to the start.
pub fn sniff<S: SeekSource>(src: &mut S) -> Result<Container, TagError> {
    let mut head = [0u8; 16];
    let n = read_full(src, &mut head)?;
    src.seek(SeekFrom::Start(0)).map_err(|_| TagError::Io)?;
    let h = &head[..n];
    let c = if h.starts_with(b"fLaC") {
        Container::Flac
    } else if h.starts_with(b"OggS") {
        Container::Ogg
    } else if h.starts_with(b"ID3") {
        // An ID3v2 can front a raw ADTS (`.aac`) stream (the iTunes/FAAC
        // convention); if ADTS frames follow the tag, route to the ADTS reader
        // (it reads the tag + frame-scans). Otherwise it's an ID3-tagged MP3.
        if adts_follows_id3(src, h) {
            Container::Adts
        } else {
            Container::Id3
        }
    } else if h.starts_with(b"RIFF") && h.len() >= 12 && &h[8..12] == b"WAVE" {
        Container::Wav
    } else if h.starts_with(b"FORM")
        && h.len() >= 12
        && (&h[8..12] == b"AIFF" || &h[8..12] == b"AIFC")
    {
        Container::Aiff
    } else if h.len() >= 8 && &h[4..8] == b"ftyp" {
        Container::Mp4
    } else if h.starts_with(b"DSD ") {
        Container::Dsf
    } else if h.starts_with(b"FRM8") && h.len() >= 16 && &h[12..16] == b"DSD " {
        Container::Dff
    } else if h.starts_with(b"wvpk") {
        Container::WavPack
    } else if h.starts_with(b"caff") {
        Container::Caf
    } else if h.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        // EBML magic → Matroska or WebM (same container; the DocType differs).
        Container::Matroska
    } else if h.len() >= 2 && h[0] == 0xFF && (h[1] & 0xF6) == 0xF0 {
        // ADTS syncword 0xFFF + layer == 00 (the 0x06 mask rules out an MP3
        // frame, whose layer bits are non-zero).
        Container::Adts
    } else {
        Container::Unknown
    };
    Ok(c)
}

/// Does a raw ADTS stream follow the leading ID3v2 tag described by `head` (the
/// sniffed prefix)? Reads the tag's synchsafe size, peeks the two bytes past it
/// for an ADTS syncword, then rewinds `src` to the start. `false` if the prefix
/// is too short or the tag isn't followed by ADTS.
fn adts_follows_id3<S: SeekSource>(src: &mut S, head: &[u8]) -> bool {
    if head.len() < 10 {
        return false;
    }
    let size = (u32::from(head[6] & 0x7F) << 21)
        | (u32::from(head[7] & 0x7F) << 14)
        | (u32::from(head[8] & 0x7F) << 7)
        | u32::from(head[9] & 0x7F);
    let start = 10u64 + u64::from(size);
    let mut adts = false;
    if src.seek(SeekFrom::Start(start)).is_ok() {
        let mut s = [0u8; 2];
        if read_full(src, &mut s).unwrap_or(0) == 2 && s[0] == 0xFF && (s[1] & 0xF6) == 0xF0 {
            adts = true;
        }
    }
    let _ = src.seek(SeekFrom::Start(0));
    adts
}

/// Read up to `buf.len()` bytes; returns how many were read (may be < len at EOF).
pub(crate) fn read_full<S: Read>(src: &mut S, buf: &mut [u8]) -> Result<usize, TagError> {
    let mut off = 0;
    while off < buf.len() {
        match src.read(&mut buf[off..]).map_err(|_| TagError::Io)? {
            0 => break,
            n => off += n,
        }
    }
    Ok(off)
}

#[cfg(test)]
mod tests {
    use super::*;
    use soul_audio_core::SliceSource;

    fn sniff_bytes(head: &[u8]) -> Container {
        let mut buf = [0u8; 64];
        buf[..head.len()].copy_from_slice(head);
        let mut src = SliceSource::new(&buf);
        sniff(&mut src).unwrap()
    }

    #[test]
    fn sniffs_each_container_by_magic() {
        assert_eq!(sniff_bytes(b"fLaC"), Container::Flac);
        assert_eq!(sniff_bytes(b"OggS"), Container::Ogg);
        assert_eq!(sniff_bytes(b"ID3\x03"), Container::Id3);
        assert_eq!(sniff_bytes(b"RIFF\0\0\0\0WAVE"), Container::Wav);
        assert_eq!(sniff_bytes(b"FORM\0\0\0\0AIFF"), Container::Aiff);
        assert_eq!(sniff_bytes(b"FORM\0\0\0\0AIFC"), Container::Aiff);
        assert_eq!(sniff_bytes(b"\0\0\0\x20ftypM4A "), Container::Mp4);
        assert_eq!(sniff_bytes(b"DSD \0\0\0\0"), Container::Dsf);
        assert_eq!(sniff_bytes(b"FRM8\0\0\0\0\0\0\0\0DSD "), Container::Dff);
        assert_eq!(sniff_bytes(b"\xFF\xF1\x50\x80"), Container::Adts);
        assert_eq!(sniff_bytes(b"wvpk\0\0\0\0"), Container::WavPack);
        assert_eq!(sniff_bytes(b"caff\0\x01\0\0"), Container::Caf);
        assert_eq!(sniff_bytes(b"\x1A\x45\xDF\xA3"), Container::Matroska);
        assert_eq!(sniff_bytes(b"garbage!"), Container::Unknown);
    }
}
