//! [`BufReader`] — adds [`BufSource`](crate::BufSource) to any
//! [`Source`](crate::Source) using a caller-provided buffer.

use embedded_io::{BufRead, ErrorType, Read, Seek, SeekFrom};

use crate::io::Source;

/// Wraps any [`Source`] and makes it a [`BufSource`](crate::BufSource) using a
/// **caller-provided** scratch buffer — the no-alloc analogue of
/// `std::io::BufReader`.
///
/// The caller owns the buffer, so nothing is allocated. A bigger buffer means
/// fewer reads from the inner source. If the inner source is also seekable, so
/// is the `BufReader`: a seek discards any buffered bytes and repositions the
/// inner source, so subsequent reads reflect the new position.
pub struct BufReader<'buf, S> {
    inner: S,
    buf: &'buf mut [u8],
    filled: usize, // number of valid bytes in `buf`
    pos: usize,    // read cursor within `buf[..filled]`
}

impl<'buf, S: Source> BufReader<'buf, S> {
    /// Wrap `inner`, buffering reads through `buf`.
    pub fn new(inner: S, buf: &'buf mut [u8]) -> Self {
        Self {
            inner,
            buf,
            filled: 0,
            pos: 0,
        }
    }
}

impl<S: Source> ErrorType for BufReader<'_, S> {
    type Error = S::Error;
}

impl<S: Source> Read for BufReader<'_, S> {
    fn read(&mut self, out: &mut [u8]) -> Result<usize, Self::Error> {
        let available = self.fill_buf()?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl<S: Source> BufRead for BufReader<'_, S> {
    fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
        if self.pos >= self.filled {
            // Buffer drained — refill from the inner source.
            self.filled = self.inner.read(self.buf)?;
            self.pos = 0;
        }
        Ok(&self.buf[self.pos..self.filled])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.filled);
    }
}

impl<S: Source + Seek> Seek for BufReader<'_, S> {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
        // Buffered bytes belong to the old position — drop them before moving.
        self.filled = 0;
        self.pos = 0;
        self.inner.seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{BufSource, SeekSource, SliceSource};

    #[test]
    fn bufreader_reads_across_refills() {
        let data = [1u8, 2, 3, 4, 5];
        let mut scratch = [0u8; 2]; // smaller than data → forces multiple refills
        let mut r = BufReader::new(SliceSource::new(&data), &mut scratch);
        let mut out = [0u8; 5];
        let mut total = 0;
        while total < out.len() {
            let n = r.read(&mut out[total..]).unwrap();
            assert!(n > 0, "unexpected EOF at {total}");
            total += n;
        }
        assert_eq!(out, [1, 2, 3, 4, 5]);
        assert_eq!(r.read(&mut out).unwrap(), 0); // EOF
    }

    #[test]
    fn bufreader_peeks_then_consumes_and_refills() {
        let data = [9u8, 8, 7, 6];
        let mut scratch = [0u8; 3];
        let mut r = BufReader::new(SliceSource::new(&data), &mut scratch);

        assert_eq!(r.fill_buf().unwrap(), &[9, 8, 7]); // first refill (3 bytes)
        r.consume(1);
        assert_eq!(r.fill_buf().unwrap(), &[8, 7]); // same buffer, advanced
        r.consume(2);
        assert_eq!(r.fill_buf().unwrap(), &[6]); // buffer drained → refill remainder
        r.consume(1);
        assert_eq!(r.fill_buf().unwrap(), &[]); // EOF
    }

    #[test]
    fn bufreader_seek_discards_buffer() {
        let data = [0u8, 1, 2, 3, 4, 5];
        let mut scratch = [0u8; 4];
        let mut r = BufReader::new(SliceSource::new(&data), &mut scratch);

        assert_eq!(r.fill_buf().unwrap(), &[0, 1, 2, 3]); // prime buffer
        r.consume(1);
        assert_eq!(r.seek(SeekFrom::Start(2)).unwrap(), 2); // discards buffered bytes
        let mut b = [0u8; 2];
        assert_eq!(r.read(&mut b).unwrap(), 2);
        assert_eq!(b, [2, 3]); // read reflects the seek, not the stale buffer
    }

    #[test]
    fn bufreader_over_seekable_climbs_the_ladder() {
        fn requires_buf<S: BufSource>() {}
        fn requires_seek<S: SeekSource>() {}
        requires_buf::<BufReader<'static, SliceSource<'static>>>();
        requires_seek::<BufReader<'static, SliceSource<'static>>>();
    }
}
