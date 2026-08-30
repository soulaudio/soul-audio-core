//! The source context: an ordered list of tracks the queue plays *from*,
//! addressed by position and resolved on demand — never copied into the queue.

/// A source of tracks addressed by natural (unshuffled) position.
///
/// Implementations must be a **stable snapshot**: for the life of the context,
/// `len()` is constant and `handle_at(i)` returns the same handle for the same
/// `i`. `handle_at` must be **cheap and non-blocking** — it is called on the
/// pump path; the slow `open` happens later in the driver.
pub trait Context {
    /// Opaque track handle the app maps to an openable source.
    type Handle: Copy + Eq;

    /// Number of tracks (`0` = empty).
    fn len(&self) -> usize;

    /// Whether the context is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The handle at natural index `i` (`i < len()`), or `None` if out of range.
    fn handle_at(&self, natural_index: usize) -> Option<Self::Handle>;
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A trivial in-RAM context over a slice — used across the queue tests.
    pub(crate) struct Slice<'a, H>(pub &'a [H]);
    impl<H: Copy + Eq> Context for Slice<'_, H> {
        type Handle = H;
        fn len(&self) -> usize {
            self.0.len()
        }
        fn handle_at(&self, i: usize) -> Option<H> {
            self.0.get(i).copied()
        }
    }

    #[test]
    fn slice_context_resolves_positions() {
        let c = Slice(&[10u32, 20, 30]);
        assert_eq!(c.len(), 3);
        assert!(!c.is_empty());
        assert_eq!(c.handle_at(0), Some(10));
        assert_eq!(c.handle_at(2), Some(30));
        assert_eq!(c.handle_at(3), None);
        assert!(Slice::<u32>(&[]).is_empty());
    }
}
