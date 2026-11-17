//! A transient bump-allocator **cursor** over caller-owned `i32` scratch
//! storage — the i32-slot-indexed sibling of
//! [`crate::formats::vorbis::arena::Arena`] (which bump-allocates byte
//! ranges). Used by decoders (FLAC, ALAC/CAF, Opus) that carve several working
//! buffers out of one caller-provided `P: AsRef<[i32]> + AsMut<[i32]>` scratch
//! — the SAME shared scratch [`crate::formats::wavpack::WavpackDecoder`] owns
//! outright for its single block-staging buffer (decoder-memory-
//! externalization spec §7: one scratch per `Deck` slot, carved differently by
//! whichever format is live).
//!
//! # Why a transient cursor, not a stored field (the self-reference trap)
//!
//! Exactly the rationale [`crate::formats::vorbis::arena::Arena`] documents: a
//! decoder must never hold a `&mut` view into its own owned storage as a
//! field (self-referential, impossible in safe Rust and this crate is
//! `#![forbid(unsafe_code)]`). [`ScratchCursor`] only ever borrows `P::as_ref()`
//! transiently (to read its length) during construction; the decoder then
//! stores the **owned** `P` plus the plain `usize` offsets [`ScratchCursor::alloc`]
//! returned — never a borrow.
//!
//! No alignment parameter (unlike the byte [`Arena`](crate::formats::vorbis::arena::Arena)):
//! every allocation here is already i32-slot-aligned, so a plain running
//! offset suffices.

use crate::decoder::DecodeError;

/// A bump-allocator cursor over i32-slot offsets into a scratch store of a
/// known length. See the module docs for why this is only ever used
/// transiently (never stored as a field).
pub(crate) struct ScratchCursor {
    len: usize,
    top: usize,
}

impl ScratchCursor {
    /// Wrap a scratch store of `len` i32 slots as an empty cursor (the first
    /// [`Self::alloc`] starts at offset 0).
    pub(crate) fn new(len: usize) -> Self {
        Self { len, top: 0 }
    }

    /// Bump-allocate `n` i32 slots, returning the starting offset.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] if `n` slots would run past the end of the
    /// backing store (or the running total would overflow `usize`) — the
    /// caller's scratch is too small for this decoder, never a panic or an
    /// out-of-bounds index.
    pub(crate) fn alloc(&mut self, n: usize) -> Result<usize, DecodeError> {
        let start = self.top;
        let end = start.checked_add(n).ok_or(DecodeError::Unsupported)?;
        if end > self.len {
            return Err(DecodeError::Unsupported);
        }
        self.top = end;
        Ok(start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_bumps_the_cursor_sequentially() {
        let mut c = ScratchCursor::new(16);
        assert_eq!(c.alloc(4).unwrap(), 0);
        assert_eq!(c.alloc(4).unwrap(), 4);
        assert_eq!(c.alloc(2).unwrap(), 8);
    }

    #[test]
    fn alloc_exactly_fills_the_scratch() {
        let mut c = ScratchCursor::new(8);
        assert_eq!(c.alloc(8).unwrap(), 0);
    }

    #[test]
    fn alloc_past_capacity_is_unsupported_not_a_panic() {
        let mut c = ScratchCursor::new(4);
        assert_eq!(c.alloc(4).unwrap(), 0); // fills it exactly
        assert_eq!(c.alloc(1), Err(DecodeError::Unsupported));
    }

    #[test]
    fn zero_len_alloc_at_capacity_still_succeeds() {
        let mut c = ScratchCursor::new(4);
        assert_eq!(c.alloc(4).unwrap(), 0);
        assert_eq!(c.alloc(0).unwrap(), 4);
    }
}
