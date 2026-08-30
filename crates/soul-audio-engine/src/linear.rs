//! The trivial linear sequencer: an index cursor + repeat modes. `Handle = usize`
//! (the track index). This is what the DAP's folder player and the CLI use.

use crate::sequencer::{Advance, Sequencer};

/// Behaviour at track boundaries and end-of-queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RepeatMode {
    /// Play through once, then stop.
    #[default]
    Normal,
    /// Repeat the current track on a natural end (manual skips still move on).
    RepeatOne,
    /// Wrap from the last track back to the first.
    RepeatAll,
}

/// A cursor over `0..len` with repeat modes.
#[derive(Clone, Copy, Debug, Default)]
pub struct LinearSequencer {
    len: usize,
    cursor: usize,
    mode: RepeatMode,
}

impl LinearSequencer {
    /// An empty sequencer. Call [`set_queue`](Self::set_queue) to populate.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// (Re)define the queue as `len` tracks starting at `start` (clamped).
    pub fn set_queue(&mut self, len: usize, start: usize) {
        self.len = len;
        self.cursor = if len == 0 { 0 } else { start.min(len - 1) };
    }

    /// Set the repeat mode.
    pub fn set_mode(&mut self, mode: RepeatMode) {
        self.mode = mode;
    }

    /// The current repeat mode.
    #[must_use]
    pub fn mode(&self) -> RepeatMode {
        self.mode
    }

    /// Number of tracks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Automatic successor (natural end + preload chain).
    fn auto_next(&self, i: usize) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        match self.mode {
            RepeatMode::RepeatOne => Some(i),
            RepeatMode::RepeatAll => Some((i + 1) % self.len),
            RepeatMode::Normal => (i + 1 < self.len).then_some(i + 1),
        }
    }

    /// Manual forward (RepeatOne behaves like Normal for an explicit skip).
    fn manual_next(&self, i: usize) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        match self.mode {
            RepeatMode::RepeatAll => Some((i + 1) % self.len),
            RepeatMode::Normal | RepeatMode::RepeatOne => (i + 1 < self.len).then_some(i + 1),
        }
    }

    /// Manual backward.
    fn manual_prev(&self, i: usize) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        match self.mode {
            RepeatMode::RepeatAll => Some((i + self.len - 1) % self.len),
            // `then` (lazy): `i - 1` must not evaluate at i == 0.
            RepeatMode::Normal | RepeatMode::RepeatOne => (i > 0).then(|| i - 1),
        }
    }

    /// The `k`-th auto-successor of the cursor (`k == 1` is the immediate next).
    fn nth_successor(&self, k: usize) -> Option<usize> {
        let mut i = self.cursor;
        for _ in 0..k {
            i = self.auto_next(i)?;
        }
        Some(i)
    }
}

impl Sequencer for LinearSequencer {
    type Handle = usize;

    fn current(&self) -> Option<usize> {
        (self.len > 0).then_some(self.cursor)
    }

    fn upcoming(&self, k: usize) -> Option<usize> {
        if k == 0 {
            return self.current();
        }
        self.nth_successor(k)
    }

    fn advance(&mut self, trigger: Advance) -> Option<usize> {
        let next = match trigger {
            Advance::Natural => self.auto_next(self.cursor),
            Advance::Skip => self.manual_next(self.cursor),
        }?;
        self.cursor = next;
        Some(next)
    }

    fn prev(&mut self) -> Option<usize> {
        let p = self.manual_prev(self.cursor)?;
        self.cursor = p;
        Some(p)
    }

    fn goto(&mut self, pos: usize) -> Option<usize> {
        if pos >= self.len {
            return None;
        }
        self.cursor = pos;
        Some(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(len: usize, start: usize, mode: RepeatMode) -> LinearSequencer {
        let mut s = LinearSequencer::new();
        s.set_queue(len, start);
        s.set_mode(mode);
        s
    }

    #[test]
    fn empty_is_inert() {
        let mut s = LinearSequencer::new();
        assert!(s.is_empty());
        assert_eq!(s.current(), None);
        assert_eq!(s.upcoming(1), None);
        assert_eq!(s.advance(Advance::Natural), None);
        assert_eq!(s.prev(), None);
        assert_eq!(s.goto(0), None);
    }

    #[test]
    fn normal_walks_forward_and_stops() {
        let mut s = seq(3, 0, RepeatMode::Normal);
        assert_eq!(s.current(), Some(0));
        assert_eq!(s.upcoming(1), Some(1));
        assert_eq!(s.upcoming(2), Some(2));
        assert_eq!(s.upcoming(3), None);
        assert_eq!(s.advance(Advance::Natural), Some(1));
        assert_eq!(s.advance(Advance::Natural), Some(2));
        assert_eq!(s.advance(Advance::Natural), None); // end
    }

    #[test]
    fn repeat_all_wraps_both_ways() {
        let mut s = seq(2, 0, RepeatMode::RepeatAll);
        assert_eq!(s.upcoming(1), Some(1));
        assert_eq!(s.upcoming(2), Some(0)); // wraps
        assert_eq!(s.advance(Advance::Natural), Some(1));
        assert_eq!(s.advance(Advance::Natural), Some(0));
        assert_eq!(s.prev(), Some(1)); // wraps backward
    }

    #[test]
    fn repeat_one_loops_on_natural_but_skips_on_manual() {
        let mut s = seq(3, 1, RepeatMode::RepeatOne);
        assert_eq!(s.upcoming(1), Some(1)); // preload the same track
        assert_eq!(s.advance(Advance::Natural), Some(1)); // loops
        assert_eq!(s.advance(Advance::Skip), Some(2)); // manual overrides
    }

    #[test]
    fn goto_and_prev_bounds() {
        let mut s = seq(4, 0, RepeatMode::Normal);
        assert_eq!(s.goto(3), Some(3));
        assert_eq!(s.goto(4), None); // out of range
        assert_eq!(s.current(), Some(3));
        assert_eq!(s.prev(), Some(2));
        let mut at_start = seq(4, 0, RepeatMode::Normal);
        assert_eq!(at_start.prev(), None); // Normal at start
    }
}
