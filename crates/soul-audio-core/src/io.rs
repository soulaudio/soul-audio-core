//! No-alloc I/O plumbing for decoders.

use embedded_io::{ErrorKind, ErrorType, Read};

/// A zero-allocation [`embedded_io::Read`] over an in-memory byte slice.
///
/// Handy for feeding a decoder from `include_bytes!` data on the firmware, or
/// from a fixture in tests. On desktop, wrap a `std::fs::File` with
/// `embedded_io_adapters` instead — the decoder is generic over the reader, so
/// the same code path works everywhere.
pub struct SliceReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceReader<'a> {
    /// Wrap `data`, starting at offset 0.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl ErrorType for SliceReader<'_> {
    type Error = ErrorKind;
}

impl Read for SliceReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let remaining = &self.data[self.pos..];
        let n = remaining.len().min(buf.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_then_eofs() {
        let data = [1u8, 2, 3, 4, 5];
        let mut r = SliceReader::new(&data);
        let mut buf = [0u8; 3];
        assert_eq!(r.read(&mut buf).unwrap(), 3);
        assert_eq!(buf, [1, 2, 3]);
        assert_eq!(r.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], &[4, 5]);
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }
}
