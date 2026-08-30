//! The `Sequencer` seam: the "what plays next" policy the [`Engine`] drives.

/// What triggered a boundary crossing, so a sequencer can treat a natural end
/// differently from an explicit user skip (e.g. repeat-one repeats on `Natural`
/// but advances on `Skip`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Advance {
    /// The current track ended on its own.
    Natural,
    /// The user explicitly skipped forward.
    Skip,
}

/// The ordering policy the engine consults to decide which track plays. It is
/// storage-free and does **no I/O**; implementations own their storage. `Handle`
/// is an opaque track id the app maps to an openable source.
pub trait Sequencer {
    /// Opaque track handle. `Copy + Eq` so the engine can compare primed slots.
    type Handle: Copy + Eq;

    /// The track playing now, or `None` when there is nothing to play.
    fn current(&self) -> Option<Self::Handle>;

    /// The `k`-th track that would play after `current` at natural boundaries,
    /// **without mutating** (`k == 1` is the immediate next; `k == 0` is
    /// `current`). Drives the engine's gapless preload.
    fn upcoming(&self, k: usize) -> Option<Self::Handle>;

    /// Commit a boundary crossing; returns the new current (or `None` at the end
    /// of a non-repeating queue).
    fn advance(&mut self, trigger: Advance) -> Option<Self::Handle>;

    /// Step back; returns the new current.
    fn prev(&mut self) -> Option<Self::Handle>;

    /// Jump to an explicit position; returns the new current, or `None` if out of
    /// range.
    fn goto(&mut self, pos: usize) -> Option<Self::Handle>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-track linear stub used only to prove the trait is usable.
    struct TwoTrack {
        cur: usize,
    }
    impl Sequencer for TwoTrack {
        type Handle = usize;
        fn current(&self) -> Option<usize> {
            Some(self.cur)
        }
        fn upcoming(&self, k: usize) -> Option<usize> {
            (self.cur + k < 2).then_some(self.cur + k)
        }
        fn advance(&mut self, _t: Advance) -> Option<usize> {
            (self.cur + 1 < 2).then(|| {
                self.cur += 1;
                self.cur
            })
        }
        fn prev(&mut self) -> Option<usize> {
            (self.cur > 0).then(|| {
                self.cur -= 1;
                self.cur
            })
        }
        fn goto(&mut self, pos: usize) -> Option<usize> {
            (pos < 2).then(|| {
                self.cur = pos;
                pos
            })
        }
    }

    #[test]
    fn trait_is_usable_through_a_stub() {
        let mut s = TwoTrack { cur: 0 };
        assert_eq!(s.current(), Some(0));
        assert_eq!(s.upcoming(1), Some(1));
        assert_eq!(s.upcoming(2), None);
        assert_eq!(s.advance(Advance::Natural), Some(1));
        assert_eq!(s.advance(Advance::Natural), None);
        assert_eq!(s.prev(), Some(0));
        assert_eq!(s.goto(1), Some(1));
        assert_eq!(s.goto(9), None);
    }
}
