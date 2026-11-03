//! The two-level Up-Next queue: a virtual source context + a manual Up-Next
//! queue + a `prev` history, implementing `Sequencer`. Up-Next wins on a normal
//! advance; `Repeat::One` is the sole override (it freezes the queue and loops
//! the current track on a natural end).

use soul_audio_engine::{Advance, Sequencer};

use crate::buf::{Full, QueueBuf};
use crate::context::Context;
use crate::shuffle::{shuffle_index, unshuffle_index};
use crate::{Repeat, StopAfter};

pub struct TwoLevelQueue<C, Up, Hist>
where
    C: Context,
    Up: QueueBuf<C::Handle>,
    Hist: QueueBuf<(C::Handle, usize)>,
{
    ctx: C,
    /// The actual current track (None = nothing playing).
    cur: Option<C::Handle>,
    /// Source anchor: the play-order position of the current/last SOURCE track.
    pos: usize,
    /// `Some` = shuffled by this seed; `None` = linear.
    seed: Option<u64>,
    upnext: Up,
    /// `prev` history: `(handle, natural source index)`. The natural index is
    /// seed-invariant, so `prev` resolves the correct source anchor even if
    /// `set_shuffle`/`reshuffle` changed the seed since the entry was pushed.
    history: Hist,
    repeat: Repeat,
    // Read/written by the mutation API (Task 6); unread by this task's Sequencer
    // impl alone.
    stop_after: StopAfter,
}

impl<C, Up, Hist> TwoLevelQueue<C, Up, Hist>
where
    C: Context,
    Up: QueueBuf<C::Handle>,
    Hist: QueueBuf<(C::Handle, usize)>,
{
    /// Wrap `ctx` with empty Up-Next/history buffers, starting at source position
    /// 0 (linear). `cur` resolves to the first track if the context is non-empty.
    pub fn new(ctx: C, upnext: Up, history: Hist) -> Self {
        let mut q = Self {
            ctx,
            cur: None,
            pos: 0,
            seed: None,
            upnext,
            history,
            repeat: Repeat::Off,
            stop_after: StopAfter::None,
        };
        q.reseat_from_pos(0);
        q
    }

    /// Replace the source context, starting at source position `start`. Keeps the
    /// Up-Next queue and shuffle seed; clears history (positions no longer valid).
    pub fn set_source(&mut self, ctx: C, start: usize) {
        self.ctx = ctx;
        self.history.clear();
        self.reseat_from_pos(start);
    }

    /// Whether the source tier is currently shuffled.
    #[must_use]
    pub fn is_shuffled(&self) -> bool {
        self.seed.is_some()
    }

    /// Turn shuffle on (with `seed`) or off, keeping the CURRENT track playing:
    /// only the upcoming order changes. Turning off resumes linear order from the
    /// current track's natural position.
    pub fn set_shuffle(&mut self, on: bool, seed: u64) {
        if on == self.seed.is_some() && (!on || self.seed == Some(seed)) {
            return;
        }
        // Anchor: keep the current natural index fixed across the change.
        let natural_now = self.natural(self.pos);
        self.seed = on.then_some(seed);
        // Recompute pos so `cur` stays the same track under the new ordering.
        self.pos = self.play_pos(natural_now);
    }

    /// Regenerate the shuffle order with a new seed (must be shuffled), keeping
    /// the current track playing.
    pub fn reshuffle(&mut self, seed: u64) {
        if self.seed.is_some() {
            self.set_shuffle(true, seed);
        }
    }

    // --- internal helpers --------------------------------------------------

    /// Natural (unshuffled) index for a play-order position.
    fn natural(&self, p: usize) -> usize {
        match self.seed {
            Some(s) => shuffle_index(s, p, self.ctx.len()),
            None => p,
        }
    }

    /// Play-order position for a natural index (inverse of `natural`).
    fn play_pos(&self, natural: usize) -> usize {
        match self.seed {
            Some(s) => unshuffle_index(s, natural, self.ctx.len()),
            None => natural,
        }
    }

    /// The source handle at play-order position `p`.
    fn source_at(&self, p: usize) -> Option<C::Handle> {
        self.ctx.handle_at(self.natural(p))
    }

    /// The next SOURCE play-order position from `p` under the repeat mode
    /// (`wrap` when `Repeat::All`); `None` at the end of a non-wrapping source.
    fn src_next(&self, p: usize) -> Option<usize> {
        let len = self.ctx.len();
        if len == 0 {
            return None;
        }
        match self.repeat {
            Repeat::All => Some((p + 1) % len),
            _ => (p + 1 < len).then_some(p + 1),
        }
    }

    /// (Re)seat the current track directly at source play-order position `start`
    /// (clamped); does not touch history.
    fn reseat_from_pos(&mut self, start: usize) {
        let len = self.ctx.len();
        if len == 0 {
            self.pos = 0;
            self.cur = None;
        } else {
            self.pos = start.min(len - 1);
            self.cur = self.source_at(self.pos);
        }
    }

    /// Insert `h` at the FRONT of the Up-Next queue ("Play Next").
    pub fn play_next(&mut self, h: C::Handle) -> Result<(), Full> {
        self.upnext.push_front(h)
    }

    /// Append `h` to the BACK of the Up-Next queue ("Add to Queue").
    pub fn enqueue(&mut self, h: C::Handle) -> Result<(), Full> {
        self.upnext.push_back(h)
    }

    /// Drop all manually queued tracks (does not touch the source or current).
    pub fn clear_upnext(&mut self) {
        self.upnext.clear();
    }

    /// Number of manually queued tracks.
    #[must_use]
    pub fn upnext_len(&self) -> usize {
        self.upnext.len()
    }

    /// Iterate the Up-Next tracks, front (plays first) to back.
    pub fn upnext_iter(&self) -> impl Iterator<Item = C::Handle> + '_ {
        (0..self.upnext.len()).filter_map(move |i| self.upnext.get(i).copied())
    }

    /// Set the repeat mode.
    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
    }

    /// The current repeat mode.
    #[must_use]
    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    /// Set the one-shot stop request.
    pub fn set_stop_after(&mut self, stop_after: StopAfter) {
        self.stop_after = stop_after;
    }

    /// The current one-shot stop request.
    #[must_use]
    pub fn stop_after(&self) -> StopAfter {
        self.stop_after
    }

    /// Number of tracks in the source context.
    #[must_use]
    pub fn source_len(&self) -> usize {
        self.ctx.len()
    }

    /// Current source anchor (play-order position).
    #[must_use]
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Borrow the source context.
    #[must_use]
    pub fn context(&self) -> &C {
        &self.ctx
    }

    /// Capture the persistable state. `context_id` identifies the app's context.
    #[cfg(feature = "alloc")]
    pub fn snapshot(&self, context_id: u64) -> crate::snapshot::Snapshot<C::Handle> {
        crate::snapshot::Snapshot {
            context_id,
            cur: self.cur,
            pos: self.pos,
            seed: self.seed,
            repeat: self.repeat,
            stop_after: self.stop_after,
            upnext: (0..self.upnext.len())
                .filter_map(|i| self.upnext.get(i).copied())
                .collect(),
            history: {
                let mut v = alloc::vec::Vec::new();
                // history front-to-back = oldest-to-newest
                let n = self.history.len();
                for i in 0..n {
                    if let Some(&(h, p)) = self.history.get(i) {
                        v.push((h, p));
                    }
                }
                v
            },
        }
    }

    /// Rebuild a queue from `ctx` (matching `snap.context_id`) + fresh empty
    /// buffers `up`/`hist`, restoring `pos`/`seed`/flags and the Up-Next/history
    /// contents. Validates `pos` and capacities. `cur` is restored verbatim from
    /// `snap.cur` rather than re-derived from `pos`/`seed`, which is why this is
    /// correct even when the snapshotted current track came from Up-Next (in
    /// which case `pos` alone would not identify it).
    #[cfg(feature = "alloc")]
    pub fn restore(
        ctx: C,
        snap: &crate::snapshot::Snapshot<C::Handle>,
        mut up: Up,
        mut hist: Hist,
    ) -> Result<Self, crate::snapshot::RestoreError> {
        use crate::snapshot::RestoreError;
        let len = ctx.len();
        if len != 0 && snap.pos >= len {
            return Err(RestoreError::PosOutOfRange);
        }
        for &h in &snap.upnext {
            up.push_back(h)
                .map_err(|_| RestoreError::CapacityExceeded)?;
        }
        for &(h, p) in &snap.history {
            hist.push_back((h, p))
                .map_err(|_| RestoreError::CapacityExceeded)?;
        }
        Ok(Self {
            ctx,
            cur: snap.cur,
            pos: snap.pos.min(len.saturating_sub(1)),
            seed: snap.seed,
            upnext: up,
            history: hist,
            repeat: snap.repeat,
            stop_after: snap.stop_after,
        })
    }
}

impl<C, Up, Hist> Sequencer for TwoLevelQueue<C, Up, Hist>
where
    C: Context,
    Up: QueueBuf<C::Handle>,
    Hist: QueueBuf<(C::Handle, usize)>,
{
    type Handle = C::Handle;

    fn current(&self) -> Option<C::Handle> {
        self.cur
    }

    fn upcoming(&self, k: usize) -> Option<C::Handle> {
        if k == 0 {
            return self.cur;
        }
        // Repeat::One freezes: the projection is the current track, looped.
        if self.repeat == Repeat::One {
            return self.cur;
        }
        let up = self.upnext.len();
        if k <= up {
            return self.upnext.get(k - 1).copied();
        }
        // Source successors beyond the Up-Next items.
        let mut p = self.pos;
        for _ in 0..(k - up) {
            p = self.src_next(p)?;
        }
        self.source_at(p)
    }

    fn advance(&mut self, trigger: Advance) -> Option<C::Handle> {
        // Repeat::One + natural end: freeze — loop the current track, touch nothing.
        if trigger == Advance::Natural && self.repeat == Repeat::One {
            return self.cur;
        }
        // Record where we were for `prev`.
        if let Some(h) = self.cur {
            self.history.push_back_evicting((h, self.natural(self.pos)));
        }
        if let Some(h) = self.upnext.pop_front() {
            // Up-Next wins; the source anchor stays put.
            self.cur = Some(h);
        } else if let Some(np) = self.src_next(self.pos) {
            self.pos = np;
            self.cur = self.source_at(np);
        } else {
            self.cur = None; // end of a non-repeating source
        }
        self.cur
    }

    fn prev(&mut self) -> Option<C::Handle> {
        let (h, p) = self.history.pop_back()?;
        self.cur = Some(h);
        self.pos = self.play_pos(p);
        self.cur
    }

    fn goto(&mut self, natural_index: usize) -> Option<C::Handle> {
        if natural_index >= self.ctx.len() {
            return None;
        }
        if let Some(h) = self.cur {
            self.history.push_back_evicting((h, self.natural(self.pos)));
        }
        self.pos = self.play_pos(natural_index);
        self.cur = self.ctx.handle_at(natural_index);
        self.cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::tests::Slice;

    // Bounded `heapless` backings so the core queue tests run under plain
    // `cargo test` (no `alloc` feature). `Vec`/`vec!` come from std under cfg(test).
    type Q<'a> =
        TwoLevelQueue<Slice<'a, u32>, heapless::Deque<u32, 8>, heapless::Deque<(u32, usize), 16>>;

    fn q(items: &[u32]) -> Q<'_> {
        TwoLevelQueue::new(Slice(items), heapless::Deque::new(), heapless::Deque::new())
    }

    #[test]
    fn linear_walk_and_end() {
        let mut s = q(&[10, 20, 30]);
        assert_eq!(s.current(), Some(10));
        assert_eq!(s.upcoming(1), Some(20));
        assert_eq!(s.upcoming(2), Some(30));
        assert_eq!(s.upcoming(3), None);
        assert_eq!(s.advance(Advance::Natural), Some(20));
        assert_eq!(s.advance(Advance::Natural), Some(30));
        assert_eq!(s.advance(Advance::Natural), None);
    }

    #[test]
    fn repeat_all_wraps() {
        let mut s = q(&[10, 20]);
        s.repeat = Repeat::All;
        assert_eq!(s.upcoming(1), Some(20));
        assert_eq!(s.upcoming(2), Some(10));
        assert_eq!(s.advance(Advance::Natural), Some(20));
        assert_eq!(s.advance(Advance::Natural), Some(10));
    }

    #[test]
    fn repeat_one_freezes_on_natural_but_skips_on_manual() {
        let mut s = q(&[10, 20, 30]);
        s.repeat = Repeat::One;
        assert_eq!(s.upcoming(1), Some(10)); // preload the same track
        assert_eq!(s.advance(Advance::Natural), Some(10)); // loop
        assert_eq!(s.advance(Advance::Skip), Some(20)); // manual overrides
    }

    #[test]
    fn upnext_wins_then_source_resumes() {
        let mut s = q(&[10, 20, 30]); // cur=10 (pos 0)
        s.upnext.push_back(99).unwrap(); // play next: 99
        assert_eq!(s.upcoming(1), Some(99));
        assert_eq!(s.upcoming(2), Some(20)); // after 99, source resumes at 20
        assert_eq!(s.advance(Advance::Natural), Some(99)); // 99 interjects; pos stays 0
        assert_eq!(s.advance(Advance::Natural), Some(20)); // resume source at 20
        assert_eq!(s.advance(Advance::Natural), Some(30));
    }

    #[test]
    fn prev_restores_handle_and_pos_exactly() {
        let mut s = q(&[10, 20, 30]);
        s.advance(Advance::Natural); // ->20 (pos1), history [(10,0)]
        s.advance(Advance::Natural); // ->30 (pos2), history [(10,0),(20,1)]
        assert_eq!(s.prev(), Some(20));
        assert_eq!(s.pos, 1);
        assert_eq!(s.advance(Advance::Natural), Some(30)); // forward again from 20
    }

    #[test]
    fn prev_after_goto_restores_pre_goto_anchor() {
        let mut s = q(&[10, 20, 30, 40, 50]);
        let before = s.current(); // Some(10), pos 0
        assert_eq!(s.goto(3), Some(40));
        assert_eq!(s.prev(), before, "prev restores the pre-goto current track");
        assert_eq!(s.pos, 0, "prev restores the pre-goto source anchor");
    }

    #[test]
    fn goto_selects_by_natural_index() {
        let mut s = q(&[10, 20, 30, 40, 50]);
        assert_eq!(s.goto(3), Some(40));
        assert_eq!(s.pos, 3);
        assert_eq!(s.goto(9), None); // out of range
        assert_eq!(s.current(), Some(40));
    }

    #[test]
    fn empty_context_is_inert() {
        let mut s = q(&[]);
        assert_eq!(s.current(), None);
        assert_eq!(s.upcoming(1), None);
        assert_eq!(s.advance(Advance::Natural), None);
        assert_eq!(s.prev(), None);
        assert_eq!(s.goto(0), None);
    }

    #[test]
    fn shuffle_toggle_keeps_current_track() {
        let mut s = q(&[10, 20, 30, 40, 50]);
        s.advance(Advance::Natural); // cur=20 at natural index 1
        let before = s.current();
        s.set_shuffle(true, 0xabcdef);
        assert_eq!(
            s.current(),
            before,
            "current unchanged when enabling shuffle"
        );
        assert_eq!(s.natural(s.pos), 1, "still anchored to natural index 1");
        s.set_shuffle(false, 0);
        assert_eq!(
            s.current(),
            before,
            "current unchanged when disabling shuffle"
        );
        assert_eq!(s.pos, 1, "linear order resumes from the current track");
    }

    #[test]
    fn shuffle_visits_every_track_once_per_cycle() {
        let mut s = q(&[10, 20, 30, 40]);
        s.set_shuffle(true, 42);
        s.repeat = Repeat::All;
        let mut seen = Vec::new();
        seen.push(s.current().unwrap());
        for _ in 0..3 {
            seen.push(s.advance(Advance::Natural).unwrap());
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            4,
            "a full cycle visits every track exactly once"
        );
    }

    #[test]
    fn play_next_jumps_the_line_enqueue_waits() {
        let mut s = q(&[10, 20, 30]);
        s.enqueue(91).unwrap(); // upnext: [91]
        s.play_next(92).unwrap(); // upnext: [92, 91]
        assert_eq!(s.upnext_iter().collect::<Vec<_>>(), [92, 91]);
        assert_eq!(s.upcoming(1), Some(92));
        assert_eq!(s.upcoming(2), Some(91));
        assert_eq!(s.upcoming(3), Some(20)); // then the source resumes
        assert_eq!(s.upnext_len(), 2);
        s.clear_upnext();
        assert_eq!(s.upnext_len(), 0);
        assert_eq!(s.upcoming(1), Some(20));
    }

    #[test]
    fn set_source_keeps_upnext() {
        let mut s = q(&[10, 20, 30]);
        s.enqueue(77).unwrap();
        s.set_source(Slice(&[1, 2]), 0);
        assert_eq!(s.current(), Some(1));
        assert_eq!(s.source_len(), 2);
        assert_eq!(s.upnext_len(), 1, "manual queue survives a new source");
        assert_eq!(s.upcoming(1), Some(77));
    }

    #[test]
    fn repeat_and_stop_after_round_trip() {
        let mut s = q(&[10, 20]);
        s.set_repeat(Repeat::All);
        assert_eq!(s.repeat(), Repeat::All);
        s.set_stop_after(StopAfter::Track);
        assert_eq!(s.stop_after(), StopAfter::Track);
    }

    #[cfg(feature = "alloc")] // snapshot/restore are alloc-gated
    #[test]
    fn snapshot_restore_round_trips() {
        let mut s = q(&[10, 20, 30, 40]);
        s.set_shuffle(true, 7);
        s.set_repeat(Repeat::All);
        s.enqueue(88).unwrap();
        s.advance(Advance::Natural); // move + push history
        let snap = s.snapshot(0xC0FFEE);
        let items = [10u32, 20, 30, 40];
        let mut r: Q = TwoLevelQueue::restore(
            Slice(&items),
            &snap,
            heapless::Deque::new(),
            heapless::Deque::new(),
        )
        .unwrap();
        assert_eq!(r.current(), s.current());
        assert_eq!(r.pos(), s.pos());
        assert_eq!(r.is_shuffled(), s.is_shuffled());
        assert_eq!(r.repeat(), s.repeat());
        assert_eq!(r.upnext_len(), s.upnext_len());
        // History fidelity: `prev` on the restored queue must yield the same
        // handle as `prev` on the original, proving history round-tripped.
        assert_eq!(r.prev(), s.prev(), "restored history matches the original");
    }

    #[test]
    fn prev_anchor_survives_a_reshuffle() {
        let mut s = q(&[10, 20, 30, 40, 50]);
        s.set_shuffle(true, 1);
        let first = s.current().unwrap();
        s.advance(Advance::Natural); // history records `first` at its natural anchor
        s.reshuffle(2); // change the seed
        let restored = s.prev().unwrap();
        assert_eq!(restored, first, "prev restores the correct handle");
        // The anchor must be seed-invariant: pos points at `restored` under the NEW
        // seed, so the source resumes correctly. Fails if history stored play-order pos.
        assert_eq!(
            s.source_at(s.pos),
            Some(restored),
            "pos anchor survives reshuffle"
        );
    }
}
