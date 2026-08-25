//! Container identification by peeking magic bytes.
//!
//! [`probe`] binds [`BufSource`] and *consumes nothing* — it looks at the
//! buffered prefix via `fill_buf`, so the decoder chosen afterwards still sees
//! byte 0. That peek-without-rewind is exactly what `BufSource` is for.

use crate::decoder::DecodeError;
use crate::io::BufSource;
use crate::model::Format;

/// Identify the container of `src` from its leading bytes, without consuming
/// them.
///
/// # Errors
/// - [`DecodeError::Io`] if the source read fails.
/// - [`DecodeError::Unsupported`] if the magic matches no known container (or
///   too few bytes are buffered to decide).
pub fn probe<S: BufSource>(src: &mut S) -> Result<Format, DecodeError> {
    let head = src.fill_buf().map_err(|_| DecodeError::Io)?;
    identify(head).ok_or(DecodeError::Unsupported)
}

/// Match a leading byte window against known container magic. Returns `None`
/// when nothing matches or the window is too short to decide.
fn identify(head: &[u8]) -> Option<Format> {
    // WAV: "RIFF" at 0 and "WAVE" at 8.
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WAVE" {
        return Some(Format::Wav);
    }
    if head.len() >= 4 {
        match &head[0..4] {
            b"fLaC" => return Some(Format::Flac),
            b"DSD " => return Some(Format::Dsf),
            b"FRM8" => return Some(Format::Dff),
            // Ogg: only FLAC-in-Ogg is supported — its BOS page body begins with
            // the `\x7FFLAC` mapping header (within the first page).
            b"OggS" => {
                let window = &head[..head.len().min(64)];
                if window.windows(5).any(|w| w == b"\x7FFLAC") {
                    return Some(Format::OggFlac);
                }
                return None; // Ogg Vorbis/Opus/etc. are not supported
            }
            _ => {}
        }
    }
    // MP3: an ID3v2 tag ("ID3") or a raw MPEG frame sync (0xFF 0xEx).
    if head.len() >= 3 && &head[0..3] == b"ID3" {
        return Some(Format::Mp3);
    }
    if head.len() >= 2 && head[0] == 0xFF && (head[1] & 0xE0) == 0xE0 {
        return Some(Format::Mp3);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;
    use embedded_io::Read;

    fn probe_bytes(bytes: &[u8]) -> Result<Format, DecodeError> {
        probe(&mut SliceSource::new(bytes))
    }

    #[test]
    fn identifies_each_container() {
        assert_eq!(probe_bytes(b"RIFF\0\0\0\0WAVEfmt ").unwrap(), Format::Wav);
        assert_eq!(probe_bytes(b"fLaC\0\0\0\0").unwrap(), Format::Flac);
        assert_eq!(probe_bytes(b"DSD \0\0\0\0").unwrap(), Format::Dsf);
        assert_eq!(probe_bytes(b"FRM8\0\0\0\0").unwrap(), Format::Dff);
        assert_eq!(probe_bytes(b"ID3\x04\0\0\0").unwrap(), Format::Mp3);
        assert_eq!(probe_bytes(&[0xFF, 0xFB, 0x90, 0x00]).unwrap(), Format::Mp3);
    }

    #[test]
    fn unknown_magic_is_unsupported() {
        assert_eq!(
            probe_bytes(b"XXXXYYYY").unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn riff_without_wave_is_not_wav() {
        // RIFF/AVI etc. — RIFF present but not a WAVE payload.
        assert_eq!(
            probe_bytes(b"RIFF\0\0\0\0AVI LIST").unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn probe_consumes_nothing() {
        // After probing, a read still sees byte 0 (the magic).
        let bytes = b"fLaC\x00\x22";
        let mut src = SliceSource::new(bytes);
        assert_eq!(probe(&mut src).unwrap(), Format::Flac);
        let mut out = [0u8; 4];
        assert_eq!(src.read(&mut out).unwrap(), 4);
        assert_eq!(&out, b"fLaC");
    }
}
