use crate::TagError;
use embedded_io::{Read, SeekFrom};
use soul_audio_core::SeekSource;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Container {
    Flac,
    Ogg,
    Id3,
    Wav,
    Mp4,
    Dsf,
    Dff,
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
        Container::Id3
    } else if h.starts_with(b"RIFF") && h.len() >= 12 && &h[8..12] == b"WAVE" {
        Container::Wav
    } else if h.len() >= 8 && &h[4..8] == b"ftyp" {
        Container::Mp4
    } else if h.starts_with(b"DSD ") {
        Container::Dsf
    } else if h.starts_with(b"FRM8") && h.len() >= 16 && &h[12..16] == b"DSD " {
        Container::Dff
    } else {
        Container::Unknown
    };
    Ok(c)
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
        assert_eq!(sniff_bytes(b"\0\0\0\x20ftypM4A "), Container::Mp4);
        assert_eq!(sniff_bytes(b"DSD \0\0\0\0"), Container::Dsf);
        assert_eq!(sniff_bytes(b"FRM8\0\0\0\0\0\0\0\0DSD "), Container::Dff);
        assert_eq!(sniff_bytes(b"garbage!"), Container::Unknown);
    }
}
