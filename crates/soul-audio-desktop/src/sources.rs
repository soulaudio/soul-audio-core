//! Concrete desktop byte sources, mapped onto the core capability ladder.
//!
//! Each real `std` backend becomes a [`Source`]/[`BufSource`]/[`SeekSource`] by
//! bridging through `embedded-io-adapters` (`FromStd`) and wrapping in the
//! core's caller-buffered [`BufReader`]. The tier each backend reaches is a
//! *compile-time* fact — you cannot seek a stream:
//!
//! | backend          | Source | BufSource | SeekSource |
//! |------------------|:------:|:---------:|:----------:|
//! | file             |   ✓    |     ✓     |     ✓      |
//! | tcp stream       |   ✓    |     ✓     |     ✗      |
//! | in-memory bytes  |   ✓    |     ✓     |     ✓      |

use std::fs::File;
use std::net::TcpStream;

use embedded_io_adapters::std::FromStd;
use soul_audio_core::BufReader;

/// Buffered, **seekable** source over a file. `File` is `Read + Seek`, so the
/// wrapped source reaches the full ladder (`SeekSource`).
pub fn file_source(file: File, buf: &mut [u8]) -> BufReader<'_, FromStd<File>> {
    BufReader::new(FromStd::new(file), buf)
}

/// Buffered, **forward-only** source over a TCP stream. `TcpStream` is not
/// `Seek`, so this is a `Source`/`BufSource` but *not* a `SeekSource` — the type
/// system forbids seeking a live stream.
pub fn tcp_source(stream: TcpStream, buf: &mut [u8]) -> BufReader<'_, FromStd<TcpStream>> {
    BufReader::new(FromStd::new(stream), buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use soul_audio_core::{BufSource, DecodeError, SeekSource, Source, open};

    /// In-memory bytes as a full-ladder source (test parity with `SliceSource`).
    fn bytes_source(bytes: Vec<u8>, buf: &mut [u8]) -> BufReader<'_, FromStd<Cursor<Vec<u8>>>> {
        BufReader::new(FromStd::new(Cursor::new(bytes)), buf)
    }

    fn assert_source<S: Source>() {}
    fn assert_buf<S: BufSource>() {}
    fn assert_seek<S: SeekSource>() {}

    #[test]
    fn file_backend_reaches_full_ladder() {
        assert_source::<BufReader<'static, FromStd<File>>>();
        assert_buf::<BufReader<'static, FromStd<File>>>();
        assert_seek::<BufReader<'static, FromStd<File>>>();
    }

    #[test]
    fn tcp_backend_is_forward_only() {
        assert_source::<BufReader<'static, FromStd<TcpStream>>>();
        assert_buf::<BufReader<'static, FromStd<TcpStream>>>();
        // Intentionally NOT `assert_seek` — a live stream cannot seek, and the
        // type system enforces it (BufReader<FromStd<TcpStream>>: !SeekSource).
    }

    #[test]
    fn open_over_in_memory_bytes_routes_by_magic() {
        // Minimal valid FLAC: signature + STREAMINFO (44.1 kHz stereo 16-bit).
        let mut si = [0u8; 34];
        let v: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36);
        si[10..18].copy_from_slice(&v.to_be_bytes());
        let mut flac = Vec::new();
        flac.extend_from_slice(b"fLaC");
        flac.push(0x80);
        flac.extend_from_slice(&[0, 0, 34]);
        flac.extend_from_slice(&si);

        let mut buf = [0u8; 64];
        let src = bytes_source(flac, &mut buf);
        assert!(open(src).is_ok()); // fLaC → FLAC variant, real StreamInfo
    }

    #[test]
    fn open_over_in_memory_bytes_rejects_unknown() {
        let mut buf = [0u8; 64];
        let src = bytes_source(b"XXXXYYYY".to_vec(), &mut buf);
        assert_eq!(open(src).err(), Some(DecodeError::Unsupported));
    }
}
