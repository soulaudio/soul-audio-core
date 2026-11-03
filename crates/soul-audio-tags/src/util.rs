//! Small byte-parsing helpers shared across container/tag readers.

use crate::{TagError, locate};

/// Read exactly `buf.len()` bytes, treating a short read (EOF) as malformed.
pub(crate) fn read_exact<S: embedded_io::Read>(
    src: &mut S,
    buf: &mut [u8],
) -> Result<(), TagError> {
    let n = locate::read_full(src, buf)?;
    if n < buf.len() {
        Err(TagError::Malformed)
    } else {
        Ok(())
    }
}

/// The longest valid UTF-8 prefix of `bytes` (never panics on a boundary cut
/// mid-codepoint, or on bytes that aren't UTF-8 at all).
pub(crate) fn utf8_prefix(bytes: &[u8]) -> &str {
    match core::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or(""),
    }
}

/// Read and discard `n` bytes from a forward-only source, in fixed-size
/// chunks (no allocation, no seek required). Shared by `vorbis` (comment
/// vector vendor string / oversized comment tails) and `ogg` (packet bytes
/// that overflow a caller-provided reassembly buffer).
pub(crate) fn discard<S: embedded_io::Read>(src: &mut S, mut n: usize) -> Result<(), TagError> {
    let mut junk = [0u8; 64];
    while n > 0 {
        let chunk = n.min(junk.len());
        read_exact(src, &mut junk[..chunk])?;
        n -= chunk;
    }
    Ok(())
}
