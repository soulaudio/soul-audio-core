//! A generic [`Context`](crate::Context) backed by an ordered list of handles.
//! Storage-agnostic via `AsRef<[H]>`: use `&[H]` or `heapless::Vec<H, N>` on the
//! DAP, `alloc::vec::Vec<H>` / `alloc::sync::Arc<[H]>` on the desktop. `no_std`,
//! allocation-free by itself (the caller owns the storage).

use core::marker::PhantomData;

use crate::context::Context;

/// A `Context` over any `AsRef<[H]>` handle list. `handle_at(i)` is the i-th
/// handle; `len()` is the slice length.
pub struct SliceContext<H, B> {
    backing: B,
    _marker: PhantomData<fn() -> H>,
}

impl<H, B> SliceContext<H, B> {
    /// Wrap `backing` (a handle list) as a context.
    pub fn new(backing: B) -> Self {
        Self {
            backing,
            _marker: PhantomData,
        }
    }

    /// Borrow the underlying handle list.
    pub fn backing(&self) -> &B {
        &self.backing
    }
}

impl<H: Copy + Eq, B: AsRef<[H]>> Context for SliceContext<H, B> {
    type Handle = H;

    fn len(&self) -> usize {
        self.backing.as_ref().len()
    }

    fn handle_at(&self, natural_index: usize) -> Option<H> {
        self.backing.as_ref().get(natural_index).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_context_over_an_array() {
        let c: SliceContext<u32, [u32; 3]> = SliceContext::new([10, 20, 30]);
        assert_eq!(c.len(), 3);
        assert!(!c.is_empty());
        assert_eq!(c.handle_at(0), Some(10));
        assert_eq!(c.handle_at(2), Some(30));
        assert_eq!(c.handle_at(3), None);
    }

    #[test]
    fn slice_context_over_a_vec_and_empty() {
        let v: Vec<u32> = vec![7, 8];
        let c = SliceContext::new(v);
        assert_eq!(c.len(), 2);
        assert_eq!(c.handle_at(1), Some(8));
        let empty: SliceContext<u32, [u32; 0]> = SliceContext::new([]);
        assert!(empty.is_empty());
    }
}
