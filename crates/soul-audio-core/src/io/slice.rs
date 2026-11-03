//! [`SliceSource`] — a zero-allocation source over an in-memory byte slice.

use embedded_io::{BufRead, ErrorKind, ErrorType, Read, Seek, SeekFrom};

/// A zero-allocation source over an in-memory byte slice.
///
/// A slice is its own buffer, so `SliceSource` implements the full capability
/// ladder — [`Source`](crate::Source), [`BufSource`](crate::BufSource), and
/// [`SeekSource`](crate::SeekSource) — with no extra memory.
///
/// Handy for feeding a decoder from `include_bytes!` data on the firmware, or
/// from a fixture in tests. On desktop, wrap a `std::fs::File` with
/// `embedded_io_adapters` instead — decoders are generic over the source, so
/// the same code path works everywhere.
pub struct SliceSource<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceSource<'a> {
    /// Wrap `data`, starting at offset 0.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bytes from the cursor to the end (empty once the cursor is at/after end).
    fn remaining(&self) -> &'a [u8] {
        self.data.get(self.pos..).unwrap_or(&[])
    }
}

impl ErrorType for SliceSource<'_> {
    type Error = ErrorKind;
}

impl Read for SliceSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let remaining = self.remaining();
        let n = remaining.len().min(buf.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        self.pos += n;
        Ok(n)
    }
}

impl BufRead for SliceSource<'_> {
    fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
        Ok(self.remaining())
    }

    fn consume(&mut self, amt: usize) {
        self.pos = self.pos.saturating_add(amt);
    }
}

impl Seek for SliceSource<'_> {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
        let len = self.data.len() as i64;
        let target = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(n) => self.pos as i64 + n,
            SeekFrom::End(n) => len + n,
        };
        if target < 0 {
            return Err(ErrorKind::InvalidInput);
        }
        // Seeking at/after end is allowed; subsequent reads just return 0.
        self.pos = target as usize;
        Ok(self.pos as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{BufSource, SeekSource, Source};

    #[test]
    fn slice_reads_then_eofs() {
        let data = [1u8, 2, 3, 4, 5];
        let mut r = SliceSource::new(&data);
        let mut buf = [0u8; 3];
        assert_eq!(r.read(&mut buf).unwrap(), 3);
        assert_eq!(buf, [1, 2, 3]);
        assert_eq!(r.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], &[4, 5]);
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn slice_bufread_peeks_without_consuming_then_advances() {
        let data = [10u8, 20, 30, 40];
        let mut r = SliceSource::new(&data);

        // fill_buf exposes the whole remaining slice without consuming it.
        assert_eq!(r.fill_buf().unwrap(), &[10, 20, 30, 40]);
        assert_eq!(r.fill_buf().unwrap(), &[10, 20, 30, 40]);

        // consume advances the cursor; fill_buf then shows the remainder.
        r.consume(2);
        assert_eq!(r.fill_buf().unwrap(), &[30, 40]);

        // A read after peeking picks up where consume left off.
        let mut b = [0u8; 2];
        assert_eq!(r.read(&mut b).unwrap(), 2);
        assert_eq!(b, [30, 40]);
        assert_eq!(r.fill_buf().unwrap(), &[]);
    }

    #[test]
    fn slice_seeks_from_start_current_and_end() {
        let data = [0u8, 1, 2, 3, 4, 5, 6, 7];
        let mut r = SliceSource::new(&data);
        let mut b = [0u8; 1];

        assert_eq!(r.seek(SeekFrom::Start(4)).unwrap(), 4);
        assert_eq!(r.read(&mut b).unwrap(), 1);
        assert_eq!(b, [4]); // cursor was at 4, advanced to 5

        assert_eq!(r.seek(SeekFrom::Current(-2)).unwrap(), 3);
        assert_eq!(r.read(&mut b).unwrap(), 1);
        assert_eq!(b, [3]);

        assert_eq!(r.seek(SeekFrom::End(-1)).unwrap(), 7);
        assert_eq!(r.read(&mut b).unwrap(), 1);
        assert_eq!(b, [7]);
    }

    #[test]
    fn slice_seek_past_end_then_reads_eof() {
        let data = [1u8, 2, 3];
        let mut r = SliceSource::new(&data);
        assert_eq!(r.seek(SeekFrom::Start(99)).unwrap(), 99);
        let mut b = [0u8; 4];
        assert_eq!(r.read(&mut b).unwrap(), 0);
    }

    #[test]
    fn slice_seek_before_start_is_error() {
        let data = [1u8, 2, 3];
        let mut r = SliceSource::new(&data);
        assert!(r.seek(SeekFrom::Current(-1)).is_err());
        assert!(r.seek(SeekFrom::End(-4)).is_err());
    }

    #[test]
    fn slice_source_satisfies_every_tier() {
        // Compile-time proof that a slice climbs the whole capability ladder.
        fn requires_source<S: Source>() {}
        fn requires_buf<S: BufSource>() {}
        fn requires_seek<S: SeekSource>() {}
        requires_source::<SliceSource<'static>>();
        requires_buf::<SliceSource<'static>>();
        requires_seek::<SliceSource<'static>>();
    }
}
