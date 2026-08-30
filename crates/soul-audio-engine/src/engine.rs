//! The playback engine: owns a [`Deck`] + a [`Sequencer`], and reconciles the
//! deck to the sequencer. The sequencer decides *which* track; the engine keeps
//! the deck's current + gapless preloads in sync with it and does no I/O.

use soul_audio_core::{Deck, DeckError, DeckState, Progress, SeekableDecoder, Sink};

use crate::sequencer::{Advance, Sequencer};

/// Where an opened decoder ended up in [`Engine::provide`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provided {
    /// Installed as the current track.
    Loaded,
    /// Staged in a gapless preload slot.
    Preloaded,
    /// No longer wanted (the sequence moved on) — dropped.
    Discarded,
}

/// A playback engine driving a [`Deck`] from a [`Sequencer`].
///
/// `PRELOAD` is the deck's gapless preload depth. The engine treats the
/// sequencer as the single source of truth and reconciles the deck's current +
/// primed preloads to `seq.current()` / `seq.upcoming(1..=PRELOAD)` each cycle.
pub struct Engine<K, D, S: Sequencer, const PRELOAD: usize> {
    deck: Deck<K, D, PRELOAD>,
    seq: S,
    /// The deck's current slot holds `current_handle`.
    have_current: bool,
    current_handle: Option<S::Handle>,
    /// Handles installed into the deck's preload slots, front-first.
    primed: [Option<S::Handle>; PRELOAD],
    /// The engine wants tracks playing (armed by `rearm`/navigation, cleared on
    /// natural end-of-queue / `stop`).
    wants_playback: bool,
    /// A handle requested via `poll_open`, not yet returned via `provide`.
    pending: Option<S::Handle>,
}

impl<K: Sink, D: SeekableDecoder, S: Sequencer, const PRELOAD: usize> Engine<K, D, S, PRELOAD> {
    /// Wrap `deck` + `seq`. The sequencer starts wherever it is; call
    /// [`rearm`](Self::rearm) after configuring it to begin playback.
    pub fn new(deck: Deck<K, D, PRELOAD>, seq: S) -> Self {
        Self {
            deck,
            seq,
            have_current: false,
            current_handle: None,
            primed: [None; PRELOAD],
            wants_playback: false,
            pending: None,
        }
    }

    /// Borrow the sequencer (queries).
    #[must_use]
    pub fn sequencer(&self) -> &S {
        &self.seq
    }

    /// Mutably borrow the sequencer (set the queue, Play Next, shuffle, …). The
    /// engine self-reconciles on the next `poll_open`/`pump`.
    pub fn sequencer_mut(&mut self) -> &mut S {
        &mut self.seq
    }

    /// The current track handle, if any.
    #[must_use]
    pub fn current(&self) -> Option<S::Handle> {
        self.seq.current()
    }

    /// (Re)start playback from the sequencer's current track. Call after
    /// (re)configuring the sequencer.
    pub fn rearm(&mut self) {
        self.deck.stop();
        self.deck.clear_preloads();
        self.primed = [None; PRELOAD];
        self.have_current = false;
        self.current_handle = None;
        self.pending = None;
        self.wants_playback = self.seq.current().is_some();
    }

    /// Whether the engine still wants tracks playing.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.wants_playback
    }

    /// Transport state of the deck.
    #[must_use]
    pub fn state(&self) -> DeckState {
        self.deck.state()
    }

    /// Borrow the deck (position/duration/rate/sink queries).
    #[must_use]
    pub fn deck(&self) -> &Deck<K, D, PRELOAD> {
        &self.deck
    }

    /// Mutably borrow the deck (deck-only controls / sink flush).
    pub fn deck_mut(&mut self) -> &mut Deck<K, D, PRELOAD> {
        &mut self.deck
    }

    /// The handle the driver should open next off-thread, if any. `None` when
    /// nothing needs opening or an open is already outstanding.
    pub fn poll_open(&mut self) -> Option<S::Handle> {
        self.reconcile();
        if self.pending.is_some() {
            return None;
        }
        let handle = self.want()?;
        self.pending = Some(handle);
        Some(handle)
    }

    /// Hand an opened `decoder` (for `handle`) back. It becomes the current
    /// track, a preload, or is discarded if the sequence moved on.
    pub fn provide(&mut self, handle: S::Handle, decoder: D) -> Result<Provided, DeckError> {
        if self.pending == Some(handle) {
            self.pending = None;
        }
        if self.wants_playback {
            if !self.have_current {
                if Some(handle) == self.seq.current() {
                    self.deck.load(decoder)?;
                    self.have_current = true;
                    self.current_handle = Some(handle);
                    return Ok(Provided::Loaded);
                }
            } else if self.deck.free_preload_slots() > 0 {
                let primed = PRELOAD - self.deck.free_preload_slots();
                if self.seq.upcoming(primed + 1) == Some(handle) {
                    self.deck.preload(decoder)?;
                    self.primed[primed] = Some(handle);
                    return Ok(Provided::Preloaded);
                }
            }
        }
        Ok(Provided::Discarded)
    }

    /// One block of playback; keeps the engine's bookkeeping in step with the
    /// deck's gapless swaps / end-of-track.
    pub fn pump(&mut self) -> Result<Progress, DeckError> {
        self.reconcile();
        let progress = self.deck.pump()?;
        match progress {
            Progress::TrackChanged => {
                // The deck promoted primed[0] to current.
                let promoted = self.primed[0];
                self.shift_primed();
                self.current_handle = promoted;
                // Keep the sequencer's cursor in step with the natural advance.
                let _ = self.seq.advance(Advance::Natural);
            }
            Progress::Ended => {
                self.have_current = false;
                self.current_handle = None;
                self.primed = [None; PRELOAD];
                if self.seq.advance(Advance::Natural).is_some() {
                    // A successor exists (RepeatOne/All, or a mid-queue preload
                    // underrun) — re-arm to reload it. Report Idle, not Ended, so a
                    // driver that breaks on Ended does not drop the rest of the queue.
                    return Ok(Progress::Idle);
                }
                self.wants_playback = false;
            }
            Progress::Idle | Progress::Played => {}
        }
        Ok(progress)
    }

    /// Resume playback of the loaded track.
    pub fn play(&mut self) {
        self.deck.play();
    }

    /// Pause (retains position).
    pub fn pause(&mut self) {
        self.deck.pause();
    }

    /// Stop and clear the deck; disarms until the next navigation / `rearm`.
    pub fn stop(&mut self) {
        self.deck.stop();
        self.deck.clear_preloads();
        self.primed = [None; PRELOAD];
        self.have_current = false;
        self.current_handle = None;
        self.pending = None;
        self.wants_playback = false;
    }

    /// Set the deck volume (ramped).
    pub fn set_volume(&mut self, volume: f32) {
        self.deck.set_volume(volume);
    }

    /// Seek the current track to per-channel `frame`.
    pub fn seek(&mut self, frame: u64) -> Result<u64, DeckError> {
        self.deck.seek(frame)
    }

    /// Skip to the next track (Skip semantics). No-op at the end of a Normal queue.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<(), DeckError> {
        if self.seq.advance(Advance::Skip).is_some() {
            self.reseat_current();
        }
        Ok(())
    }

    /// Skip to the previous track. No-op at the start of a Normal queue.
    pub fn prev(&mut self) -> Result<(), DeckError> {
        if self.seq.prev().is_some() {
            self.reseat_current();
        }
        Ok(())
    }

    /// Jump to `pos`. Out-of-range positions are ignored.
    pub fn goto(&mut self, pos: usize) -> Result<(), DeckError> {
        if self.seq.goto(pos).is_some() {
            self.reseat_current();
        }
        Ok(())
    }

    // --- internals ---------------------------------------------------------

    /// The handle the engine currently wants opened.
    fn want(&self) -> Option<S::Handle> {
        if !self.wants_playback {
            return None;
        }
        if !self.have_current {
            return self.seq.current();
        }
        let free = self.deck.free_preload_slots();
        if free == 0 {
            return None;
        }
        let primed = PRELOAD - free;
        self.seq.upcoming(primed + 1)
    }

    /// Fix the deck to the sequencer: correct the current if it changed
    /// out-of-band, then discard preloads that no longer match `upcoming`.
    fn reconcile(&mut self) {
        if !self.wants_playback {
            return;
        }
        let want_current = self.seq.current();
        if self.have_current && self.current_handle != want_current {
            if want_current.is_some() && self.primed.first().copied().flatten() == want_current {
                self.deck.advance();
                self.shift_primed();
                self.current_handle = want_current;
            } else {
                self.deck.stop();
                self.deck.clear_preloads();
                self.primed = [None; PRELOAD];
                self.have_current = false;
                self.current_handle = None;
                self.wants_playback = want_current.is_some();
                self.pending = None;
            }
        }
        if self.have_current {
            let n = PRELOAD - self.deck.free_preload_slots();
            let mut stale = false;
            for j in 0..n {
                if self.primed[j] != self.seq.upcoming(j + 1) {
                    stale = true;
                    break;
                }
            }
            if stale {
                self.deck.clear_preloads();
                self.primed = [None; PRELOAD];
            }
        }
    }

    /// Reconcile the deck's current to `seq.current()` after a navigation: reuse
    /// the front primed slot for a gapless swap if it holds the target, else
    /// stop and reload.
    fn reseat_current(&mut self) {
        self.wants_playback = self.seq.current().is_some();
        self.pending = None;
        let target = self.seq.current();
        if self.have_current && target.is_some() && self.primed.first().copied().flatten() == target
        {
            self.deck.advance();
            self.shift_primed();
            self.current_handle = target;
        } else {
            self.deck.stop();
            self.deck.clear_preloads();
            self.primed = [None; PRELOAD];
            self.have_current = false;
            self.current_handle = None;
        }
    }

    /// Drop primed[0], shift the rest left, and clear the tail.
    fn shift_primed(&mut self) {
        for j in 1..PRELOAD {
            self.primed[j - 1] = self.primed[j];
        }
        if PRELOAD > 0 {
            self.primed[PRELOAD - 1] = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepeatMode;
    use crate::linear::LinearSequencer;
    use soul_audio_core::{DecodeError, Decoder, Quality, SampleFormat, SinkError, StreamInfo};

    use core::cell::Cell;

    extern crate alloc;
    use alloc::vec::Vec;

    const RATE: u32 = 48_000;
    const FRAMES: u64 = 600;

    struct ConstDecoder {
        value: i32,
        frames: u64,
        pos: u64,
    }
    impl Decoder for ConstDecoder {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: RATE,
                channels: 1,
                format: SampleFormat::Int { bits: 16 },
            }
        }
        fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
            let want = (out.len() as u64).min(self.frames - self.pos) as usize;
            for s in &mut out[..want] {
                *s = self.value;
            }
            self.pos += want as u64;
            Ok(want)
        }
    }
    impl SeekableDecoder for ConstDecoder {
        fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
            self.pos = sample.min(self.frames);
            Ok(self.pos)
        }
        fn total_frames(&self) -> Option<u64> {
            Some(self.frames)
        }
    }

    struct MockSink {
        written: Vec<i32>,
    }
    impl Sink for MockSink {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: RATE,
                channels: 1,
                format: SampleFormat::Int { bits: 24 },
            }
        }
        fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
            self.written.extend_from_slice(frames);
            Ok(())
        }
        fn flush(&mut self) -> Result<(), SinkError> {
            Ok(())
        }
    }

    fn sample_of(i: usize) -> i32 {
        ((i as i32 + 1) * 100) << 8
    }
    fn decoder_for(i: usize) -> ConstDecoder {
        ConstDecoder {
            value: (i as i32 + 1) * 100,
            frames: FRAMES,
            pos: 0,
        }
    }

    type E<const P: usize> = Engine<MockSink, ConstDecoder, LinearSequencer, P>;

    fn engine<const P: usize>(len: usize, start: usize) -> E<P> {
        let sink = MockSink {
            written: Vec::new(),
        };
        let mut e = Engine::new(Deck::new(sink, Quality::Fast), LinearSequencer::new());
        e.sequencer_mut().set_queue(len, start);
        e.rearm();
        e
    }

    /// Service every open the engine wants (acts as the driver's loader thread).
    fn service<const P: usize>(e: &mut E<P>) {
        while let Some(i) = e.poll_open() {
            let d = decoder_for(i);
            e.provide(i, d).unwrap();
        }
    }

    fn run_to_end<const P: usize>(e: &mut E<P>) -> usize {
        let mut swaps = 0;
        for _ in 0..100_000 {
            service(e);
            match e.pump().unwrap() {
                Progress::TrackChanged => swaps += 1,
                Progress::Ended => return swaps,
                _ => {}
            }
        }
        panic!("did not end");
    }

    #[test]
    fn plays_queue_in_order_then_ends() {
        let mut e = engine::<1>(3, 0);
        let swaps = run_to_end(&mut e);
        assert_eq!(swaps, 2);
        let w = &e.deck().sink().written;
        assert_eq!(w.len() as u64, FRAMES * 3);
        assert_eq!(w[100], sample_of(0));
        assert_eq!(w[FRAMES as usize + 100], sample_of(1));
        assert_eq!(w[FRAMES as usize * 2 + 100], sample_of(2));
        assert_eq!(e.state(), DeckState::Ended);
    }

    #[test]
    fn gapless_preload_is_primed_before_boundary() {
        let mut e = engine::<1>(2, 0);
        assert_eq!(e.poll_open(), Some(0));
        e.provide(0, decoder_for(0)).unwrap();
        assert_eq!(e.poll_open(), Some(1));
        assert_eq!(e.provide(1, decoder_for(1)).unwrap(), Provided::Preloaded);
        assert_eq!(e.poll_open(), None);
        assert_eq!(e.deck().free_preload_slots(), 0);
    }

    #[test]
    fn depth_two_preloads_two_successors() {
        let mut e = engine::<2>(4, 0);
        assert_eq!(e.poll_open(), Some(0));
        e.provide(0, decoder_for(0)).unwrap();
        assert_eq!(e.poll_open(), Some(1));
        assert_eq!(e.provide(1, decoder_for(1)).unwrap(), Provided::Preloaded);
        assert_eq!(e.poll_open(), Some(2));
        assert_eq!(e.provide(2, decoder_for(2)).unwrap(), Provided::Preloaded);
        assert_eq!(e.poll_open(), None);
        assert_eq!(run_to_end(&mut e), 3);
    }

    #[test]
    fn reconcile_disarms_when_sequencer_emptied_out_of_band() {
        let mut e = engine::<1>(3, 0);
        service(&mut e);
        e.pump().unwrap();
        assert!(e.is_active());
        // Empty the queue via the sequencer directly (no rearm) — the supported
        // sequencer_mut() path. The engine must self-disarm on the next cycle.
        e.sequencer_mut().set_queue(0, 0);
        assert_eq!(e.poll_open(), None);
        assert!(
            !e.is_active(),
            "engine must disarm when the sequencer empties"
        );
        assert_eq!(e.pump().unwrap(), Progress::Idle);
        assert_eq!(e.state(), DeckState::Stopped);
    }

    #[test]
    fn reconcile_reloads_when_current_changed_out_of_band() {
        let mut e = engine::<1>(5, 0);
        service(&mut e); // current 0, track 1 primed
        e.pump().unwrap();
        // Move current directly via the sequencer (bypassing engine.goto), so the
        // primed slot (1) does NOT match the new current (3) → reconcile reloads.
        e.sequencer_mut().goto(3);
        assert_eq!(e.poll_open(), Some(3));
        assert_eq!(e.provide(3, decoder_for(3)).unwrap(), Provided::Loaded);
        assert_eq!(e.current(), Some(3));
        service(&mut e);
        e.pump().unwrap();
        assert_eq!(*e.deck().sink().written.last().unwrap(), sample_of(3));
    }

    fn run_swaps<const P: usize>(e: &mut E<P>, target: usize) -> Vec<usize> {
        let mut cursors = Vec::new();
        for _ in 0..100_000 {
            service(e);
            if let Progress::TrackChanged = e.pump().unwrap() {
                cursors.push(e.current().unwrap());
                if cursors.len() == target {
                    return cursors;
                }
            }
        }
        panic!("did not reach {target} swaps");
    }

    #[test]
    fn repeat_all_wraps_forever() {
        let mut e = engine::<1>(2, 0);
        e.sequencer_mut().set_mode(RepeatMode::RepeatAll);
        assert_eq!(run_swaps(&mut e, 5), [1, 0, 1, 0, 1]);
    }

    #[test]
    fn repeat_one_keeps_the_same_track() {
        let mut e = engine::<1>(3, 1);
        e.sequencer_mut().set_mode(RepeatMode::RepeatOne);
        assert_eq!(run_swaps(&mut e, 3), [1, 1, 1]);
        assert!(e.deck().sink().written.iter().all(|&s| s == sample_of(1)));
    }

    #[test]
    fn next_reuses_primed_slot_for_instant_gapless_skip() {
        let mut e = engine::<1>(3, 0);
        service(&mut e); // track0 current, track1 primed
        assert_eq!(e.deck().free_preload_slots(), 0);
        e.pump().unwrap();
        e.next().unwrap();
        assert_eq!(e.current(), Some(1));
        assert_eq!(e.state(), DeckState::Playing);
        assert_eq!(e.deck().free_preload_slots(), 1);
        assert_eq!(run_to_end(&mut e), 1);
        assert_eq!(*e.deck().sink().written.last().unwrap(), sample_of(2));
    }

    #[test]
    fn next_without_primed_slot_reloads() {
        let mut e = engine::<1>(3, 0);
        assert_eq!(e.poll_open(), Some(0));
        e.provide(0, decoder_for(0)).unwrap();
        e.pump().unwrap();
        e.next().unwrap();
        assert_eq!(e.current(), Some(1));
        assert_eq!(e.state(), DeckState::Stopped);
        assert_eq!(e.poll_open(), Some(1));
        assert_eq!(e.provide(1, decoder_for(1)).unwrap(), Provided::Loaded);
        assert_eq!(e.state(), DeckState::Playing);
    }

    #[test]
    fn prev_goes_back_and_reloads() {
        let mut e = engine::<1>(3, 2);
        service(&mut e);
        e.pump().unwrap();
        e.prev().unwrap();
        assert_eq!(e.current(), Some(1));
        assert_eq!(e.state(), DeckState::Stopped);
        service(&mut e);
        assert!(!e.deck().sink().written.is_empty());
    }

    #[test]
    fn goto_jumps_to_arbitrary_index() {
        let mut e = engine::<1>(5, 0);
        service(&mut e);
        e.pump().unwrap();
        e.goto(3).unwrap();
        assert_eq!(e.current(), Some(3));
        service(&mut e);
        e.pump().unwrap();
        assert_eq!(*e.deck().sink().written.last().unwrap(), sample_of(3));
    }

    #[test]
    fn normal_bounds_are_no_ops() {
        let mut e = engine::<1>(3, 0);
        service(&mut e);
        e.prev().unwrap();
        assert_eq!(e.current(), Some(0));
        assert_eq!(e.state(), DeckState::Playing);
        e.goto(2).unwrap();
        service(&mut e);
        e.next().unwrap();
        assert_eq!(e.current(), Some(2));
    }

    #[test]
    fn stale_provide_is_discarded() {
        let mut e = engine::<1>(3, 0);
        assert_eq!(e.poll_open(), Some(0));
        e.goto(2).unwrap();
        assert_eq!(e.provide(0, decoder_for(0)).unwrap(), Provided::Discarded);
        assert_eq!(e.state(), DeckState::Stopped);
        assert_eq!(e.poll_open(), Some(2));
        assert_eq!(e.provide(2, decoder_for(2)).unwrap(), Provided::Loaded);
    }

    #[test]
    fn stop_disarms_and_pump_is_idle() {
        let mut e = engine::<1>(2, 0);
        service(&mut e);
        e.pump().unwrap();
        e.stop();
        assert_eq!(e.state(), DeckState::Stopped);
        assert_eq!(e.pump().unwrap(), Progress::Idle);
        assert_eq!(e.poll_open(), None);
    }

    /// A slow loader that only loads the current track (never a preload), forcing
    /// an underrun at every boundary.
    fn drive_with_underruns<const P: usize>(e: &mut E<P>, max: usize) -> Vec<usize> {
        let mut loaded = Vec::new();
        for _ in 0..100_000 {
            if matches!(e.state(), DeckState::Stopped | DeckState::Ended) {
                if let Some(i) = e.poll_open() {
                    e.provide(i, decoder_for(i)).unwrap();
                    loaded.push(e.current().unwrap());
                    if loaded.len() > max {
                        return loaded;
                    }
                }
            }
            if e.pump().unwrap() == Progress::Ended {
                return loaded;
            }
        }
        panic!("did not terminate");
    }

    #[test]
    fn underrun_reloads_next_track_instead_of_ending() {
        let mut e = engine::<1>(3, 0);
        let loaded = drive_with_underruns(&mut e, 100);
        assert_eq!(loaded, [0, 1, 2]);
        assert!(!e.is_active());
        assert_eq!(e.deck().sink().written.len() as u64, FRAMES * 3);
    }

    /// A mock sequencer whose immediate successor can be swapped at runtime,
    /// simulating a "Play Next" that changes upcoming(1) under the engine.
    struct SwapNext {
        cur: usize,
        // The handle upcoming(1) currently resolves to.
        next: Cell<usize>,
    }
    impl Sequencer for SwapNext {
        type Handle = usize;
        fn current(&self) -> Option<usize> {
            Some(self.cur)
        }
        fn upcoming(&self, k: usize) -> Option<usize> {
            match k {
                0 => Some(self.cur),
                1 => Some(self.next.get()),
                _ => None,
            }
        }
        fn advance(&mut self, _t: Advance) -> Option<usize> {
            self.cur = self.next.get();
            Some(self.cur)
        }
        fn prev(&mut self) -> Option<usize> {
            None
        }
        fn goto(&mut self, _p: usize) -> Option<usize> {
            None
        }
    }

    #[test]
    fn stale_preload_is_discarded_and_reprimed_when_upcoming_changes() {
        let sink = MockSink {
            written: Vec::new(),
        };
        let mut e: Engine<MockSink, ConstDecoder, SwapNext, 1> = Engine::new(
            Deck::new(sink, Quality::Fast),
            SwapNext {
                cur: 0,
                next: Cell::new(1),
            },
        );
        e.rearm();

        // Load current (0) and prime the successor (1).
        assert_eq!(e.poll_open(), Some(0));
        e.provide(0, decoder_for(0)).unwrap();
        assert_eq!(e.poll_open(), Some(1));
        assert_eq!(e.provide(1, decoder_for(1)).unwrap(), Provided::Preloaded);
        assert_eq!(e.deck().free_preload_slots(), 0);

        // "Play Next 2": upcoming(1) is now 2, not 1. The primed slot is stale.
        e.sequencer().upcoming(1); // (no-op read)
        e.sequencer_mut().next.set(2);

        // The next poll_open reconciles: it discards the stale preload and asks
        // for the new successor (2).
        assert_eq!(e.poll_open(), Some(2));
        assert_eq!(
            e.deck().free_preload_slots(),
            1,
            "stale preload discarded -> slot freed"
        );
        assert_eq!(e.provide(2, decoder_for(2)).unwrap(), Provided::Preloaded);
        assert_eq!(
            e.deck().free_preload_slots(),
            0,
            "re-primed with the new successor"
        );

        // Playing to the natural boundary now advances current 0 -> 2 (the Play
        // Next winner), gapless.
        let mut swapped_to = None;
        for _ in 0..100_000 {
            while let Some(i) = e.poll_open() {
                e.provide(i, decoder_for(i)).unwrap();
            }
            if let Progress::TrackChanged = e.pump().unwrap() {
                swapped_to = e.current();
                break;
            }
        }
        assert_eq!(swapped_to, Some(2));
    }

    #[test]
    fn reconcile_gapless_advances_when_current_matches_primed_slot() {
        let sink = MockSink {
            written: Vec::new(),
        };
        let mut e: Engine<MockSink, ConstDecoder, SwapNext, 1> = Engine::new(
            Deck::new(sink, Quality::Fast),
            SwapNext {
                cur: 0,
                next: Cell::new(1),
            },
        );
        e.rearm();
        // Load current 0, prime successor 1.
        assert_eq!(e.poll_open(), Some(0));
        e.provide(0, decoder_for(0)).unwrap();
        assert_eq!(e.poll_open(), Some(1));
        assert_eq!(e.provide(1, decoder_for(1)).unwrap(), Provided::Preloaded);
        e.pump().unwrap();
        assert_eq!(e.deck().free_preload_slots(), 0);
        // Move current out-of-band to EXACTLY the primed successor (1).
        e.sequencer_mut().cur = 1;
        // Reconcile must gaplessly PROMOTE the primed slot (deck.advance), not reload.
        e.poll_open();
        assert_eq!(e.current(), Some(1));
        assert_eq!(
            e.state(),
            DeckState::Playing,
            "gapless advance keeps playing (no stop/reload)"
        );
        assert_eq!(
            e.deck().free_preload_slots(),
            1,
            "primed slot consumed by the gapless advance"
        );
    }
}
