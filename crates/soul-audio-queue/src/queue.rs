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

    /// Rotate the Up-Next deque left by `by` positions (the front element moves to
    /// the back), preserving cyclic order. Backs the index-addressed mutators below
    /// using only the deque's own storage — no scratch buffer, so it holds on the
    /// bounded `heapless` backing the DAP uses (`no_std`, allocation-free).
    fn upnext_rotate_left(&mut self, by: usize) {
        let n = self.upnext.len();
        if n == 0 {
            return;
        }
        for _ in 0..(by % n) {
            if let Some(v) = self.upnext.pop_front() {
                // Just popped one, so there is room — this `push_back` can't be Full.
                let _ = self.upnext.push_back(v);
            }
        }
    }

    /// Rotate the Up-Next deque right by `by` positions (the back element moves to
    /// the front) — the inverse of [`upnext_rotate_left`](Self::upnext_rotate_left).
    fn upnext_rotate_right(&mut self, by: usize) {
        let n = self.upnext.len();
        if n == 0 {
            return;
        }
        for _ in 0..(by % n) {
            if let Some(v) = self.upnext.pop_back() {
                let _ = self.upnext.push_front(v);
            }
        }
    }

    /// Insert `h` into the Up-Next queue at `index` (clamped to the back), shifting
    /// the items at/after it back by one. `Err(Full)` only on a full bounded backing.
    fn upnext_insert(&mut self, index: usize, h: C::Handle) -> Result<(), Full> {
        if index >= self.upnext.len() {
            return self.upnext.push_back(h);
        }
        self.upnext_rotate_left(index);
        let r = self.upnext.push_front(h);
        self.upnext_rotate_right(index);
        r
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

    /// Remove and return the manually queued track at `index` (`0` = plays first),
    /// or `None` if out of range. The remaining Up-Next items keep their order; the
    /// current track, source anchor and history are untouched. This is the model
    /// behind a queue row's "remove from queue" (✕) affordance.
    pub fn remove_upnext(&mut self, index: usize) -> Option<C::Handle> {
        if index >= self.upnext.len() {
            return None;
        }
        self.upnext_rotate_left(index);
        let removed = self.upnext.pop_front();
        // `index` items remain ahead of where the hole was; rotate them back.
        self.upnext_rotate_right(index);
        removed
    }

    /// Move the manually queued track from `from` to `to` (both in
    /// `0..upnext_len()`), shifting the rest, so it ends up at index `to`. Returns
    /// `false` if either index is out of range. This is the model behind
    /// drag-to-reorder within the Up-Next band. Only the manual queue reorders —
    /// the source tier is never a drop target (reorder it by shuffling instead).
    pub fn move_upnext(&mut self, from: usize, to: usize) -> bool {
        let n = self.upnext.len();
        if from >= n || to >= n {
            return false;
        }
        if from == to {
            return true;
        }
        if let Some(h) = self.remove_upnext(from) {
            // The backing just shrank by one, so re-inserting cannot be Full.
            let _ = self.upnext_insert(to, h);
        }
        true
    }

    /// Make the manually queued track at `index` the current track immediately,
    /// DROPPING the items before it (`0..index`) from the queue — the "click a
    /// queue row to skip straight to it" interaction (the skipped-over rows are
    /// consumed). The outgoing current track is pushed to history (so `prev`
    /// returns to it); the source anchor is unchanged, so the source resumes after
    /// the remaining queue exactly as a natural advance would. Returns the new
    /// current, or `None` if out of range.
    pub fn play_upnext(&mut self, index: usize) -> Option<C::Handle> {
        if index >= self.upnext.len() {
            return None;
        }
        if let Some(h) = self.cur {
            self.history.push_back_evicting((h, self.natural(self.pos)));
        }
        for _ in 0..index {
            self.upnext.pop_front();
        }
        self.cur = self.upnext.pop_front();
        self.cur
    }

    /// Play the upcoming row at 1-based offset `k` — **exactly the track
    /// [`upcoming(k)`](Sequencer::upcoming) projects** — now, consuming everything
    /// above it. This is the single "click any Up Next row to skip straight to it"
    /// primitive the UI needs: `k` indexes the flat [`upcoming_iter`] list the user
    /// sees, so a UI never has to know whether the row is a manual-queue item or a
    /// source row, nor compute shuffle positions itself (it can't — that math is
    /// private). `k == 0` is a no-op returning the current track.
    ///
    /// - A manual-queue row drops the queued items above it (see [`play_upnext`]).
    /// - A source row drops the **whole** manual queue (it was all above the row)
    ///   and jumps the source there, skipping the rows between.
    ///
    /// Returns the new current, or `None` (changing nothing) if `k` is past the
    /// end of a non-repeating queue. Defined for `Repeat::Off`/`All`; under
    /// `Repeat::One` the projection is the frozen current, so only `k <= upnext_len`
    /// is meaningful.
    ///
    /// [`play_upnext`]: Self::play_upnext
    /// [`upcoming_iter`]: Self::upcoming_iter
    pub fn play_upcoming(&mut self, k: usize) -> Option<C::Handle> {
        if k == 0 {
            return self.cur;
        }
        let up = self.upnext.len();
        if k <= up {
            return self.play_upnext(k - 1);
        }
        // A source row: resolve its play-order position FIRST (no mutation yet, so
        // an out-of-range `k` leaves the queue untouched).
        let mut p = self.pos;
        for _ in 0..(k - up) {
            p = self.src_next(p)?;
        }
        // Commit: the manual queue was entirely above the clicked row, so it goes.
        self.upnext.clear();
        let natural = self.natural(p);
        self.goto(natural)
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

    /// Iterate the upcoming tracks in play order — **exactly what will play next**:
    /// the manual Up-Next items first, then the source successors — honouring the
    /// live shuffle order and repeat mode. Excludes the current track (it starts at
    /// the immediate next, i.e. [`upcoming(1)`](Sequencer::upcoming)). This is the
    /// one head-agnostic "Up Next" projection: the desktop UI and the DAP both
    /// render FROM it instead of re-deriving the order themselves (which is wrong
    /// the moment shuffle is on — play order is not library order).
    ///
    /// Under [`Repeat::Off`](crate::Repeat::Off) the iterator terminates at the end
    /// of the source. Under `Repeat::All`/`Repeat::One` the stream is **infinite**,
    /// so callers MUST bound it — use `.take(n)` or [`upcoming_n`](Self::upcoming_n).
    pub fn upcoming_iter(&self) -> impl Iterator<Item = C::Handle> + '_ {
        (1usize..).map_while(move |k| self.upcoming(k))
    }

    /// Collect up to `n` upcoming tracks (see [`upcoming_iter`](Self::upcoming_iter))
    /// into a `Vec`, in play order — the desktop's bounded "Up Next" projection,
    /// forwarded to the UI so it never guesses the order itself.
    #[cfg(feature = "alloc")]
    #[must_use]
    pub fn upcoming_n(&self, n: usize) -> alloc::vec::Vec<C::Handle> {
        self.upcoming_iter().take(n).collect()
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

    // --- bounded "Up Next" projection (upcoming_iter / upcoming_n) -----------

    #[test]
    fn upcoming_iter_is_finite_under_repeat_off_and_wraps_under_repeat_all() {
        let mut s = q(&[10, 20, 30]); // cur = 10
        assert_eq!(
            s.upcoming_iter().collect::<Vec<_>>(),
            [20, 30],
            "Repeat::Off terminates at the source end"
        );
        s.set_repeat(Repeat::All);
        assert_eq!(
            s.upcoming_iter().take(5).collect::<Vec<_>>(),
            [20, 30, 10, 20, 30],
            "Repeat::All is an infinite stream the caller bounds"
        );
    }

    #[test]
    fn upcoming_iter_merges_manual_queue_then_source() {
        let mut s = q(&[10, 20, 30]); // cur = 10
        s.enqueue(91).unwrap(); // upnext: [91]
        s.play_next(92).unwrap(); // upnext: [92, 91]
        assert_eq!(
            s.upcoming_iter().collect::<Vec<_>>(),
            [92, 91, 20, 30],
            "manual Up-Next plays first, then the source resumes"
        );
    }

    #[test]
    fn upcoming_iter_repeat_one_projects_the_looping_current() {
        let mut s = q(&[10, 20, 30]);
        s.set_repeat(Repeat::One);
        assert_eq!(
            s.upcoming_iter().take(3).collect::<Vec<_>>(),
            [10, 10, 10],
            "Repeat::One mirrors the preload: the current track, looped"
        );
    }

    #[test]
    fn upcoming_projection_follows_the_real_shuffled_play_order() {
        // The whole point of projecting from the player: under shuffle, play order
        // is NOT library order, so a UI must render what the queue says will play.
        let items = [10u32, 20, 30, 40, 50, 60];
        let seed = 0x1234_5678;

        let mut projected_q = q(&items);
        projected_q.set_shuffle(true, seed);
        projected_q.set_repeat(Repeat::All);
        let projected: Vec<u32> = projected_q.upcoming_iter().take(8).collect();

        // Ground truth: advance a second, identically-seeded queue and record it.
        let mut played_q = q(&items);
        played_q.set_shuffle(true, seed);
        played_q.set_repeat(Repeat::All);
        let played: Vec<u32> = (0..8)
            .map(|_| played_q.advance(Advance::Natural).unwrap())
            .collect();

        assert_eq!(
            projected, played,
            "Up Next must equal the actual shuffled play order"
        );
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn upcoming_n_collects_the_bounded_projection() {
        let mut s = q(&[10, 20, 30]);
        s.set_repeat(Repeat::All);
        assert_eq!(s.upcoming_n(4), vec![20u32, 30, 10, 20]);
        assert_eq!(s.upcoming_n(0), Vec::<u32>::new());
    }

    // --- manual-band mutation (remove / move / play_upnext) -----------------

    #[test]
    fn remove_upnext_by_index_preserves_order() {
        let mut s = q(&[1, 2, 3]);
        for h in [91u32, 92, 93, 94] {
            s.enqueue(h).unwrap();
        }
        assert_eq!(s.remove_upnext(1), Some(92), "removes the addressed item");
        assert_eq!(s.upnext_iter().collect::<Vec<_>>(), [91, 93, 94]);
        assert_eq!(s.remove_upnext(0), Some(91), "front");
        assert_eq!(s.remove_upnext(1), Some(94), "back");
        assert_eq!(s.upnext_iter().collect::<Vec<_>>(), [93]);
        assert_eq!(s.remove_upnext(5), None, "out of range");
        assert_eq!(s.upnext_iter().collect::<Vec<_>>(), [93], "no-op on miss");
    }

    #[test]
    fn move_upnext_reorders_within_the_band() {
        fn queued(order_to: usize) -> Vec<u32> {
            let mut s = q(&[1]);
            for h in [10u32, 20, 30, 40] {
                s.enqueue(h).unwrap();
            }
            assert!(s.move_upnext(1, order_to));
            s.upnext_iter().collect()
        }
        assert_eq!(queued(2), [10, 30, 20, 40], "forward one");
        assert_eq!(queued(3), [10, 30, 40, 20], "to the back");
        assert_eq!(queued(0), [20, 10, 30, 40], "to the front");

        let mut s = q(&[1]);
        for h in [10u32, 20] {
            s.enqueue(h).unwrap();
        }
        assert!(!s.move_upnext(0, 9), "to out of range");
        assert!(!s.move_upnext(9, 0), "from out of range");
        assert!(s.move_upnext(1, 1), "same index is a no-op success");
        assert_eq!(s.upnext_iter().collect::<Vec<_>>(), [10, 20]);
    }

    #[test]
    fn play_upnext_skips_to_item_dropping_those_above() {
        let mut s = q(&[1, 2, 3]); // cur = 1 at source pos 0
        for h in [91u32, 92, 93] {
            s.enqueue(h).unwrap();
        }
        assert_eq!(
            s.play_upnext(1),
            Some(92),
            "jumps straight to the clicked row"
        );
        assert_eq!(s.current(), Some(92));
        assert_eq!(
            s.upnext_iter().collect::<Vec<_>>(),
            [93],
            "the row above (91) is consumed/dropped"
        );
        assert_eq!(
            s.upcoming_iter().collect::<Vec<_>>(),
            [93, 2, 3],
            "after the remaining queue the source resumes where it was"
        );
        assert_eq!(
            s.prev(),
            Some(1),
            "prev returns to the track we skipped from"
        );
    }

    #[test]
    fn play_upnext_out_of_range_is_none_and_inert() {
        let mut s = q(&[1, 2]);
        s.enqueue(91).unwrap();
        assert_eq!(s.play_upnext(5), None);
        assert_eq!(s.current(), Some(1), "current untouched on a miss");
        assert_eq!(s.upnext_len(), 1, "queue untouched on a miss");
    }

    #[test]
    fn play_upcoming_plays_exactly_the_row_the_user_clicked() {
        // The crux: play_upcoming(k) must play whatever upcoming_iter projects at k,
        // whether that row is a manual-queue item or a source row — proven here by
        // comparing against the projection the UI renders.
        for k in 1..=5 {
            let mut s = q(&[10, 20, 30, 40]);
            s.enqueue(91).unwrap();
            s.enqueue(92).unwrap(); // rendered list: [91, 92, 20, 30, 40]
            let shown = s.upcoming_iter().nth(k - 1);
            assert_eq!(
                s.play_upcoming(k),
                shown,
                "clicking row {k} plays the track shown at row {k}"
            );
            assert_eq!(s.current(), shown);
        }
    }

    #[test]
    fn play_upcoming_consumes_everything_above_the_clicked_row() {
        // Click a source row below the manual queue: the manual queue (all above it)
        // is dropped and the skipped source rows are consumed.
        let mut s = q(&[10, 20, 30, 40]); // current 10
        s.enqueue(91).unwrap();
        s.enqueue(92).unwrap(); // list: [91, 92, 20, 30, 40]
        assert_eq!(s.play_upcoming(4), Some(30)); // row 4 == source track 30
        assert_eq!(s.current(), Some(30));
        assert_eq!(
            s.upnext_len(),
            0,
            "the manual queue above the row is dropped"
        );
        assert_eq!(s.upcoming_iter().collect::<Vec<_>>(), [40]);
    }

    #[test]
    fn play_upcoming_zero_is_current_and_out_of_range_is_inert() {
        let mut s = q(&[10, 20]); // Repeat::Off
        assert_eq!(s.play_upcoming(0), Some(10), "row 0 is the current track");
        assert_eq!(s.play_upcoming(99), None, "past the end of the queue");
        assert_eq!(s.current(), Some(10), "a miss changes nothing");
    }

    #[test]
    fn play_upcoming_routes_correctly_under_shuffle() {
        // Under shuffle the UI cannot compute source positions; play_upcoming must.
        let mut s = q(&[10, 20, 30, 40, 50, 60]);
        s.set_shuffle(true, 0x5EED);
        s.enqueue(91).unwrap(); // list: [91, <shuffled source successors…>]
        let shown = s.upcoming_iter().nth(2); // a shuffled source row
        assert_eq!(
            s.play_upcoming(3),
            shown,
            "plays the shuffled row that was shown"
        );
        assert_eq!(s.current(), shown);
    }
}
