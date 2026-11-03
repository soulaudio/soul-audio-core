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
        // The inner source has been read `filled` bytes ahead of where our
        // logical cursor sits (`pos`), so it is `filled - pos` bytes past our
        // position. A relative (`Current`) seek must be rebased onto the logical
        // position, or it would move relative to the buffered-ahead inner cursor.
        // Absolute (`Start`/`End`) seeks need no adjustment.
        let target = match pos {
            SeekFrom::Current(n) => SeekFrom::Current(n - (self.filled - self.pos) as i64),
            other => other,
        };
        // Buffered bytes belong to the old position — drop them before moving.
        self.filled = 0;
        self.pos = 0;
        self.inner.seek(target)
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

    #[test]
    fn bufreader_current_seek_accounts_for_buffered_bytes() {
        // Regression: a relative (`Current`) seek must rebase onto the logical
        // read cursor, not the buffered-ahead inner position. (This bug surfaced
        // as `Unsupported`/`Malformed` for MP4 with `moov` after `mdat`, whose
        // reader uses `Current(0)` to capture the `mdat` offset.)
        let data: [u8; 10] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let mut scratch = [0u8; 8]; // buffers ahead of the logical cursor
        let mut r = BufReader::new(SliceSource::new(&data), &mut scratch);

        let mut b = [0u8; 3];
        assert_eq!(r.read(&mut b).unwrap(), 3);
        assert_eq!(b, [0, 1, 2]); // logical pos = 3; inner buffered to 8

        // `Current(0)` must report the LOGICAL position (3), not the inner's 8.
        assert_eq!(r.seek(SeekFrom::Current(0)).unwrap(), 3);
        let mut b2 = [0u8; 2];
        assert_eq!(r.read(&mut b2).unwrap(), 2);
        assert_eq!(b2, [3, 4]); // continues from 3, not 8

        // `Current(+2)` from logical pos 5 -> 7.
        assert_eq!(r.seek(SeekFrom::Current(2)).unwrap(), 7);
        let mut b3 = [0u8; 1];
        assert_eq!(r.read(&mut b3).unwrap(), 1);
        assert_eq!(b3, [7]);
    }
}
