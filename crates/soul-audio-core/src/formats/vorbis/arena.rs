//! The bump arena: a transient, borrowing **view** over caller-owned storage,
//! used only locally within a setup-parse method — never stored as a field.
//!
//! # Why a transient view, not a stored field (the self-reference trap)
//!
//! [`VorbisDecoder`](super::VorbisDecoder) owns its byte storage `A` (plus a
//! bump cursor and, from Task 4 on, the parsed setup as byte **offsets** into
//! that storage). It must never hold an `Arena<'a>` as a field: an `Arena`
//! borrows a `&'a mut [u8]`, and if that slice came from `self`'s own storage,
//! the struct would borrow itself — a self-referential type that does not
//! compile in safe Rust (and this crate is `#![forbid(unsafe_code)]`, so
//! there is no escape hatch via `Pin`/raw pointers either).
//!
//! [`Arena`] sidesteps this by being **transient**: a caller borrows storage
//! into an `Arena` locally (`Arena::new(storage.as_mut())`), bump-allocates,
//! records the returned **offsets** (plain `usize`, not references), and lets
//! the borrow end when the `Arena` goes out of scope. The owning struct then
//! only ever stores the offsets plus the owned storage `A` itself — no
//! borrow, no self-reference. Later **reads** don't need an `Arena` at all:
//! they are plain indexing, `storage.as_ref()[offset..offset + len]`.
//!
//! # Allocation model
//!
//! A bump allocator: [`Arena::alloc`] hands out monotonically increasing,
//! aligned byte ranges and never frees individually — [`Arena::reset`] is the
//! only way to reclaim space (a whole-arena reset between streams, the
//! Rockbox `codeclib` model). Sizing is fixed at construction (the caller's
//! `A`); running out of room is a graceful [`DecodeError::Unsupported`], never
//! a panic or a silent overrun.

use crate::decoder::DecodeError;

/// A bump-allocator view over a borrowed byte slice. See the module docs for
/// why this type is always used transiently, never stored as a field.
pub struct Arena<'a> {
    buf: &'a mut [u8],
    top: usize,
}

impl<'a> Arena<'a> {
    /// Wrap `buf` as an empty arena (first [`Self::alloc`] starts at offset 0,
    /// modulo alignment).
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, top: 0 }
    }

    /// Reclaim the whole arena — the next [`Self::alloc`] starts from offset 0
    /// again. Bytes are not zeroed (a bump allocator's `free` is a no-op; only
    /// the cursor moves), so stale bytes from a previous stream may still be
    /// present until overwritten.
    pub fn reset(&mut self) {
        self.top = 0;
    }

    /// Bump-allocate `len` bytes aligned to `align` (rounded up from the
    /// current cursor), returning the byte offset of the start of the
    /// allocation. `align == 0` is treated as `align == 1` (no alignment
    /// requirement).
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] if the aligned allocation would run past
    /// the end of the backing buffer (or if the alignment padding/length
    /// arithmetic itself would overflow `usize`) — the caller's arena is too
    /// small for this stream, never a panic or an out-of-bounds write.
    pub fn alloc(&mut self, len: usize, align: usize) -> Result<usize, DecodeError> {
        let align = align.max(1);
        let rem = self.top % align;
        let padding = if rem == 0 { 0 } else { align - rem };
        let start = self
            .top
            .checked_add(padding)
            .ok_or(DecodeError::Unsupported)?;
        let end = start.checked_add(len).ok_or(DecodeError::Unsupported)?;
        if end > self.buf.len() {
            return Err(DecodeError::Unsupported);
        }
        self.top = end;
        Ok(start)
    }

    /// The current bump cursor — the number of bytes handed out so far
    /// (including alignment padding). The offset the next [`Self::alloc`] with
    /// no alignment requirement would return. Lets a caller record how much of
    /// its storage a setup-parse consumed (e.g. to resume allocating in a
    /// later parse stage over the same region).
    #[must_use]
    pub fn top(&self) -> usize {
        self.top
    }

    /// The whole backing buffer (not just the allocated prefix) — lets a
    /// caller index by the offsets [`Self::alloc`] returned.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.buf
    }

    /// The whole backing buffer, mutably — for writing into a just-allocated
    /// region by offset.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_bumps_the_cursor_sequentially() {
        let mut storage = [0u8; 16];
        let mut arena = Arena::new(&mut storage);
        assert_eq!(arena.alloc(4, 1).unwrap(), 0);
        assert_eq!(arena.alloc(4, 1).unwrap(), 4);
        assert_eq!(arena.alloc(2, 1).unwrap(), 8);
    }

    #[test]
    fn alloc_rounds_up_to_the_requested_alignment() {
        let mut storage = [0u8; 16];
        let mut arena = Arena::new(&mut storage);
        // First alloc leaves the cursor at 1 (unaligned).
        assert_eq!(arena.alloc(1, 1).unwrap(), 0);
        // A 4-byte-aligned request must skip forward to offset 4, not 1.
        assert_eq!(arena.alloc(4, 4).unwrap(), 4);
        assert_eq!(arena.alloc(1, 1).unwrap(), 8); // cursor now at 8, unaligned again
    }

    #[test]
    fn alloc_exactly_fills_the_arena() {
        let mut storage = [0u8; 8];
        let mut arena = Arena::new(&mut storage);
        assert_eq!(arena.alloc(8, 1).unwrap(), 0);
    }

    #[test]
    fn alloc_past_capacity_is_unsupported_not_a_panic() {
        let mut storage = [0u8; 4];
        let mut arena = Arena::new(&mut storage);
        assert_eq!(arena.alloc(4, 1).unwrap(), 0); // fills it exactly
        assert_eq!(arena.alloc(1, 1), Err(DecodeError::Unsupported));
    }

    #[test]
    fn alloc_alignment_padding_that_overflows_capacity_is_unsupported() {
        // Cursor at 6 in an 8-byte arena; aligning to 8 would need to jump to
        // offset 8, leaving no room for even a 1-byte allocation.
        let mut storage = [0u8; 8];
        let mut arena = Arena::new(&mut storage);
        assert_eq!(arena.alloc(6, 1).unwrap(), 0);
        assert_eq!(arena.alloc(1, 8), Err(DecodeError::Unsupported));
    }

    #[test]
    fn reset_reclaims_the_whole_arena() {
        let mut storage = [0u8; 8];
        let mut arena = Arena::new(&mut storage);
        assert_eq!(arena.alloc(8, 1).unwrap(), 0);
        assert_eq!(arena.alloc(1, 1), Err(DecodeError::Unsupported));
        arena.reset();
        assert_eq!(arena.alloc(8, 1).unwrap(), 0); // full capacity available again
    }

    #[test]
    fn bytes_and_bytes_mut_expose_the_whole_backing_buffer() {
        let mut storage = [0u8; 4];
        let mut arena = Arena::new(&mut storage);
        let off = arena.alloc(2, 1).unwrap();
        arena.bytes_mut()[off..off + 2].copy_from_slice(&[0xAB, 0xCD]);
        assert_eq!(arena.bytes().len(), 4); // whole buffer, not just the allocated prefix
        assert_eq!(&arena.bytes()[off..off + 2], &[0xAB, 0xCD]);
    }

    #[test]
    fn zero_len_alloc_at_capacity_still_succeeds() {
        // A zero-byte allocation must not spuriously fail even when the
        // arena is already exactly full.
        let mut storage = [0u8; 4];
        let mut arena = Arena::new(&mut storage);
        assert_eq!(arena.alloc(4, 1).unwrap(), 0);
        assert_eq!(arena.alloc(0, 1).unwrap(), 4);
    }
}
