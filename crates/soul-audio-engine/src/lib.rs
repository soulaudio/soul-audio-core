#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-engine` — the portable **playback queue** on top of
//! [`soul_audio_core::Deck`].
//!
//! The [`Engine`] owns a [`Deck`] and a queue *cursor*: it decides **which track
//! index** plays next ([`RepeatMode`]-aware) and drives the deck's gapless
//! preload/advance, but it does **no I/O**. Opening a track (slow) is the
//! driver's job: the engine emits an open request through
//! [`poll_open`](Engine::poll_open) and the driver hands the opened decoder back
//! through [`provide`](Engine::provide). This is GStreamer's `about-to-finish`
//! pattern with the open offloaded (MPD/mpv).
//!
//! Layering: `soul-audio-core` (decode/resample/Sink/Deck) → **`soul-audio-engine`**
//! (queue) → app/platform (the concrete [`Sink`], the track source, the threads).
//! The same engine runs on the STM32 DAP and the desktop app. `no_std`, no-alloc,
//! sync.
//!
//! ```ignore
//! // Driver skeleton (one non-RT thread shown; see the design spec for the full split):
//! let deck = Deck::<MySink, AnyDecoder<_>, 1>::new(sink, Quality::Hq);
//! let mut engine = Engine::new(deck);
//! engine.set_queue(playlist.len(), 0);   // start playing track 0
//! loop {
//!     if let Some(i) = engine.poll_open() {
//!         let decoder = provider.open(i)?;   // slow I/O, off the pump thread
//!         engine.provide(i, decoder)?;
//!     }
//!     engine.pump()?;                        // decode→resample→…→sink, one block
//! }
//! ```

use soul_audio_core::{Deck, DeckError, DeckState, Progress, SeekableDecoder, Sink};

/// Behaviour at track boundaries and end-of-queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RepeatMode {
    /// Play through the queue once, then stop.
    #[default]
    Normal,
    /// Repeat the current track indefinitely (on natural end). Manual
    /// [`next`](Engine::next)/[`prev`](Engine::prev) still navigate linearly.
    RepeatOne,
    /// Wrap from the last track back to the first.
    RepeatAll,
}

/// Where an opened decoder ended up when handed to [`provide`](Engine::provide).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provided {
    /// Installed as the current track (playing now).
    Loaded,
    /// Staged in a preload slot for a gapless swap.
    Preloaded,
    /// No longer wanted (the cursor moved after the open was requested); the
    /// decoder was dropped.
    Discarded,
}

/// A playback queue driving a [`Deck`].
///
/// `PRELOAD` is the deck's gapless preload depth (`1` on the DAP, more on
/// desktop). The engine keeps the current track loaded and the upcoming
/// successors primed, requesting each open via [`poll_open`](Engine::poll_open).
pub struct Engine<K, D, const PRELOAD: usize> {
    deck: Deck<K, D, PRELOAD>,
    len: usize,
    cursor: usize,
    mode: RepeatMode,
    /// The current track index is loaded into the deck.
    have_current: bool,
    /// The engine wants tracks loaded (cleared on stop / natural end-of-queue).
    wants_playback: bool,
    /// An index requested via `poll_open` and not yet returned via `provide`.
    /// At most one open is outstanding at a time.
    pending: Option<usize>,
}

impl<K: Sink, D: SeekableDecoder, const PRELOAD: usize> Engine<K, D, PRELOAD> {
    /// Wrap `deck` with an empty queue. Call [`set_queue`](Engine::set_queue) to
    /// start.
    pub fn new(deck: Deck<K, D, PRELOAD>) -> Self {
        Self {
            deck,
            len: 0,
            cursor: 0,
            mode: RepeatMode::Normal,
            have_current: false,
            wants_playback: false,
            pending: None,
        }
    }

    /// (Re)define the queue as `len` tracks and begin at `start` (clamped). Clears
    /// the deck; the first [`poll_open`](Engine::poll_open) will request `start`.
    /// `len == 0` empties the queue and stops.
    pub fn set_queue(&mut self, len: usize, start: usize) {
        self.deck.stop();
        self.len = len;
        self.pending = None;
        self.have_current = false;
        if len == 0 {
            self.cursor = 0;
            self.wants_playback = false;
        } else {
            self.cursor = start.min(len - 1);
            self.wants_playback = true;
        }
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

    /// The current track index, or `None` when the queue is empty.
    #[must_use]
    pub fn cursor(&self) -> Option<usize> {
        (self.len > 0).then_some(self.cursor)
    }

    /// Number of tracks in the queue.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Borrow the underlying deck (position/duration/sample-rate/state queries).
    #[must_use]
    pub fn deck(&self) -> &Deck<K, D, PRELOAD> {
        &self.deck
    }

    /// Mutably borrow the underlying deck (escape hatch for deck-only controls).
    pub fn deck_mut(&mut self) -> &mut Deck<K, D, PRELOAD> {
        &mut self.deck
    }

    /// Transport state of the deck.
    #[must_use]
    pub fn state(&self) -> DeckState {
        self.deck.state()
    }

    /// Resume playback of the loaded track.
    pub fn play(&mut self) {
        self.deck.play();
    }

    /// Pause (retains position).
    pub fn pause(&mut self) {
        self.deck.pause();
    }

    /// Stop and clear the deck; the queue cursor is retained but playback is
    /// disarmed until the next navigation.
    pub fn stop(&mut self) {
        self.deck.stop();
        self.have_current = false;
        self.wants_playback = false;
        self.pending = None;
    }

    /// Set the deck volume (ramped).
    pub fn set_volume(&mut self, volume: f32) {
        self.deck.set_volume(volume);
    }

    /// Seek the current track to per-channel `frame`.
    ///
    /// # Errors
    /// Propagates [`DeckError`] from the deck (e.g. an unseekable track).
    pub fn seek(&mut self, frame: u64) -> Result<u64, DeckError> {
        self.deck.seek(frame)
    }

    /// Skip to the next track. Reuses a primed preload slot for an instant
    /// gapless swap when possible; otherwise reloads. At the end of a `Normal`
    /// queue this is a no-op.
    ///
    /// # Errors
    /// Propagates [`DeckError`] from building the promoted/loaded track.
    // `next` is the media-transport name here (skip to the next track), not an
    // iterator step.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<(), DeckError> {
        match self.manual_next(self.cursor) {
            Some(target) => self.go_to_index(target),
            None => Ok(()),
        }
    }

    /// Skip to the previous track (always reloads). At the start of a `Normal`
    /// queue this is a no-op.
    ///
    /// # Errors
    /// Propagates [`DeckError`] from loading the track.
    pub fn prev(&mut self) -> Result<(), DeckError> {
        match self.manual_prev(self.cursor) {
            Some(target) => self.go_to_index(target),
            None => Ok(()),
        }
    }

    /// Jump to an arbitrary `index` (e.g. app-supplied shuffle order or direct
    /// selection). Out-of-range indices are ignored.
    ///
    /// # Errors
    /// Propagates [`DeckError`] from loading the track.
    pub fn goto(&mut self, index: usize) -> Result<(), DeckError> {
        if index >= self.len {
            return Ok(());
        }
        self.go_to_index(index)
    }

    /// Do one block of playback and keep the queue cursor in sync with the deck's
    /// gapless swaps / end-of-track.
    ///
    /// # Errors
    /// Propagates [`DeckError`] from the deck.
    pub fn pump(&mut self) -> Result<Progress, DeckError> {
        let progress = self.deck.pump()?;
        match progress {
            // The deck promoted a preloaded track at a natural boundary.
            Progress::TrackChanged => {
                if let Some(next) = self.auto_next(self.cursor) {
                    self.cursor = next;
                }
            }
            // The current track ran out with nothing primed.
            Progress::Ended => {
                self.have_current = false;
                if let Some(next) = self.auto_next(self.cursor) {
                    // A successor exists — RepeatOne/RepeatAll, or a mid-queue
                    // preload underrun (the loader didn't prime the next track in
                    // time). Re-arm to reload it. This is NOT the end of the queue,
                    // so report `Idle` (nothing produced this block, keep pumping);
                    // `Ended` is reserved for a true end-of-queue below, which lets
                    // a driver `break` on `Ended` without dropping the rest of the
                    // queue.
                    self.cursor = next;
                    return Ok(Progress::Idle);
                }
                // Normal end of queue: disarm.
                self.wants_playback = false;
            }
            Progress::Idle | Progress::Played => {}
        }
        Ok(progress)
    }

    /// Whether the engine still wants tracks playing — a track is loaded or being
    /// reloaded. `false` after a `Normal` queue finishes or after [`stop`](Engine::stop).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.wants_playback
    }

    /// The next track index the driver should open off-thread, if any. Returns
    /// `None` when nothing needs opening or an open is already outstanding. After
    /// opening, the driver must call [`provide`](Engine::provide) with the same
    /// index.
    pub fn poll_open(&mut self) -> Option<usize> {
        if self.pending.is_some() {
            return None;
        }
        let index = self.want()?;
        self.pending = Some(index);
        Some(index)
    }

    /// Hand an opened `decoder` (for `index`) back to the engine. It is loaded as
    /// the current track, staged as a preload, or discarded if the cursor moved
    /// on since the open was requested.
    ///
    /// # Errors
    /// Propagates [`DeckError`] from building the track (channel/rate mismatch).
    pub fn provide(&mut self, index: usize, decoder: D) -> Result<Provided, DeckError> {
        if self.pending == Some(index) {
            self.pending = None;
        }
        if self.wants_playback && self.len > 0 {
            if !self.have_current {
                if index == self.cursor {
                    self.deck.load(decoder)?;
                    self.have_current = true;
                    return Ok(Provided::Loaded);
                }
            } else if self.deck.free_preload_slots() > 0 {
                let primed = PRELOAD - self.deck.free_preload_slots();
                if self.nth_successor(primed + 1) == Some(index) {
                    self.deck.preload(decoder)?;
                    return Ok(Provided::Preloaded);
                }
            }
        }
        // Stale/unwanted: `decoder` is dropped here.
        Ok(Provided::Discarded)
    }

    // --- internals ---------------------------------------------------------

    /// The index the engine currently wants opened: the current track if it is
    /// not loaded, else the next un-primed successor, else nothing.
    fn want(&self) -> Option<usize> {
        if !self.wants_playback || self.len == 0 {
            return None;
        }
        if !self.have_current {
            return Some(self.cursor);
        }
        let free = self.deck.free_preload_slots();
        if free == 0 {
            return None;
        }
        let primed = PRELOAD - free;
        self.nth_successor(primed + 1)
    }

    /// Navigate to `target`, reusing the front primed slot for a gapless swap when
    /// it already holds `target`, otherwise reloading from a clean deck.
    fn go_to_index(&mut self, target: usize) -> Result<(), DeckError> {
        self.wants_playback = true;
        self.pending = None;
        let primed = PRELOAD - self.deck.free_preload_slots();
        let reuse = self.have_current && primed >= 1 && self.nth_successor(1) == Some(target);
        if reuse {
            // Infallible: the primed slot holds an already-built track.
            self.deck.advance();
            self.cursor = target;
        } else {
            self.deck.stop();
            self.have_current = false;
            self.cursor = target;
        }
        Ok(())
    }

    /// Auto-advance successor (natural end + preload chain).
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

    /// Manual forward navigation (RepeatOne behaves like Normal for manual skips).
    fn manual_next(&self, i: usize) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        match self.mode {
            RepeatMode::RepeatAll => Some((i + 1) % self.len),
            RepeatMode::Normal | RepeatMode::RepeatOne => (i + 1 < self.len).then_some(i + 1),
        }
    }

    /// Manual backward navigation.
    fn manual_prev(&self, i: usize) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        match self.mode {
            RepeatMode::RepeatAll => Some((i + self.len - 1) % self.len),
            // `then` (lazy) not `then_some`: `i - 1` must not evaluate at i == 0.
            RepeatMode::Normal | RepeatMode::RepeatOne => (i > 0).then(|| i - 1),
        }
    }

    /// The `k`-th auto-successor of the cursor (`k == 1` is the immediate next),
    /// or `None` if the chain hits the end of a `Normal` queue.
    fn nth_successor(&self, k: usize) -> Option<usize> {
        let mut i = self.cursor;
        for _ in 0..k {
            i = self.auto_next(i)?;
        }
        Some(i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soul_audio_core::{DecodeError, Decoder, Quality, SampleFormat, SinkError, StreamInfo};

    extern crate alloc;
    use alloc::vec::Vec;

    const RATE: u32 = 48_000;
    const FRAMES: u64 = 600; // ~2.3 blocks of 256 at 48k mono

    /// A mono 16-bit decoder emitting a constant per-track `value` so output can
    /// be attributed to a specific track index. Seekable, known length.
    struct ConstDecoder {
        rate: u32,
        value: i32,
        frames: u64,
        pos: u64,
    }
    impl Decoder for ConstDecoder {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: self.rate,
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

    /// Canonical 24-bit sample value a track of index `i` produces (value
    /// `(i+1)*100`, 16-bit → `<< 8`).
    fn sample_of(i: usize) -> i32 {
        ((i as i32 + 1) * 100) << 8
    }

    fn decoder_for(i: usize) -> ConstDecoder {
        ConstDecoder {
            rate: RATE,
            value: (i as i32 + 1) * 100,
            frames: FRAMES,
            pos: 0,
        }
    }

    type TestEngine<const P: usize> = Engine<MockSink, ConstDecoder, P>;

    fn engine<const P: usize>() -> TestEngine<P> {
        let sink = MockSink {
            written: Vec::new(),
        };
        Engine::new(Deck::new(sink, Quality::Fast))
    }

    /// Service every open the engine currently wants (acts as the driver's loader
    /// thread, always keeping preloads primed → gapless boundaries).
    fn service<const P: usize>(e: &mut TestEngine<P>) {
        while let Some(i) = e.poll_open() {
            let d = decoder_for(i);
            e.provide(i, d).unwrap();
        }
    }

    /// Drive to a natural `Ended`, servicing opens each block. Returns the number
    /// of gapless `TrackChanged` swaps observed.
    fn run_to_end<const P: usize>(e: &mut TestEngine<P>) -> usize {
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

    /// Drive until `target` gapless swaps have occurred (for repeat modes that
    /// never end). Returns the cursor index observed after each swap, in order.
    fn run_swaps<const P: usize>(e: &mut TestEngine<P>, target: usize) -> Vec<usize> {
        let mut cursors = Vec::new();
        for _ in 0..100_000 {
            service(e);
            if let Progress::TrackChanged = e.pump().unwrap() {
                cursors.push(e.cursor().unwrap());
                if cursors.len() == target {
                    return cursors;
                }
            }
        }
        panic!("did not reach {target} swaps");
    }

    #[test]
    fn plays_queue_in_order_then_ends() {
        let mut e = engine::<1>();
        e.set_queue(3, 0);
        let swaps = run_to_end(&mut e);
        assert_eq!(swaps, 2, "two gapless boundaries for three tracks");
        // Output is track0 then track1 then track2, no gaps.
        let w = &e.deck().sink().written;
        assert_eq!(w.len() as u64, FRAMES * 3);
        assert_eq!(w[100], sample_of(0));
        assert_eq!(w[(FRAMES as usize) + 100], sample_of(1));
        assert_eq!(w[(FRAMES as usize) * 2 + 100], sample_of(2));
        assert_eq!(e.state(), DeckState::Ended);
    }

    #[test]
    fn gapless_preload_is_primed_before_boundary() {
        let mut e = engine::<1>();
        e.set_queue(2, 0);
        // Load track 0.
        assert_eq!(e.poll_open(), Some(0));
        e.provide(0, decoder_for(0)).unwrap();
        // With track 0 current, the engine now wants track 1 preloaded.
        assert_eq!(e.poll_open(), Some(1));
        assert_eq!(e.provide(1, decoder_for(1)).unwrap(), Provided::Preloaded);
        // Slot full → nothing more to open.
        assert_eq!(e.poll_open(), None);
        assert_eq!(e.deck().free_preload_slots(), 0);
    }

    #[test]
    fn repeat_all_wraps_forever() {
        let mut e = engine::<1>();
        e.set_queue(2, 0);
        e.set_mode(RepeatMode::RepeatAll);
        // Boundaries cycle 1, 0, 1, 0, 1 — never ends.
        assert_eq!(run_swaps(&mut e, 5), [1, 0, 1, 0, 1]);
    }

    #[test]
    fn repeat_one_keeps_the_same_track() {
        let mut e = engine::<1>();
        e.set_queue(3, 1); // start on track 1
        e.set_mode(RepeatMode::RepeatOne);
        let cursors = run_swaps(&mut e, 3);
        assert_eq!(cursors, [1, 1, 1], "cursor stays on the repeated track");
        // All output is track 1's sample.
        assert!(e.deck().sink().written.iter().all(|&s| s == sample_of(1)));
    }

    #[test]
    fn next_reuses_primed_slot_for_instant_gapless_skip() {
        let mut e = engine::<1>();
        e.set_queue(3, 0);
        service(&mut e); // track0 current, track1 primed
        assert_eq!(e.deck().free_preload_slots(), 0);
        e.pump().unwrap(); // play a little of track 0

        e.next().unwrap();
        // Reused the primed slot: still Playing (no stop/reload gap), cursor moved.
        assert_eq!(e.cursor(), Some(1));
        assert_eq!(e.state(), DeckState::Playing);
        assert_eq!(e.deck().free_preload_slots(), 1, "primed slot was consumed");
        // Finishing out, the tail is track 1 then track 2.
        let swaps = run_to_end(&mut e);
        assert_eq!(swaps, 1, "only track1→track2 remains");
        assert_eq!(*e.deck().sink().written.last().unwrap(), sample_of(2));
    }

    #[test]
    fn next_without_primed_slot_reloads() {
        let mut e = engine::<1>();
        e.set_queue(3, 0);
        // Load only the current track; do NOT service the preload.
        assert_eq!(e.poll_open(), Some(0));
        e.provide(0, decoder_for(0)).unwrap();
        e.pump().unwrap();

        e.next().unwrap();
        // No primed slot → deck was stopped for a reload.
        assert_eq!(e.cursor(), Some(1));
        assert_eq!(e.state(), DeckState::Stopped);
        assert_eq!(e.poll_open(), Some(1), "reopens the new current track");
        assert_eq!(e.provide(1, decoder_for(1)).unwrap(), Provided::Loaded);
        assert_eq!(e.state(), DeckState::Playing);
    }

    #[test]
    fn prev_goes_back_and_reloads() {
        let mut e = engine::<1>();
        e.set_queue(3, 2); // start on the last track
        service(&mut e);
        e.pump().unwrap();
        e.prev().unwrap();
        assert_eq!(e.cursor(), Some(1));
        assert_eq!(e.state(), DeckState::Stopped, "prev always reloads");
        service(&mut e);
        assert_eq!(e.cursor(), Some(1));
        assert!(!e.deck().sink().written.is_empty());
    }

    #[test]
    fn goto_jumps_to_arbitrary_index() {
        let mut e = engine::<1>();
        e.set_queue(5, 0);
        service(&mut e);
        e.pump().unwrap();
        e.goto(3).unwrap();
        assert_eq!(e.cursor(), Some(3));
        service(&mut e);
        e.pump().unwrap();
        // We are now hearing track 3.
        assert_eq!(*e.deck().sink().written.last().unwrap(), sample_of(3));
    }

    #[test]
    fn normal_navigation_bounds_are_no_ops() {
        let mut e = engine::<1>();
        e.set_queue(3, 0);
        service(&mut e);
        e.prev().unwrap(); // already at 0
        assert_eq!(e.cursor(), Some(0));
        assert_eq!(e.state(), DeckState::Playing, "prev at start did nothing");

        e.goto(2).unwrap();
        service(&mut e);
        e.next().unwrap(); // already at last
        assert_eq!(e.cursor(), Some(2));
    }

    #[test]
    fn stale_provide_is_discarded() {
        let mut e = engine::<1>();
        e.set_queue(3, 0);
        // Request track 0 (an open goes "in flight").
        assert_eq!(e.poll_open(), Some(0));
        // Meanwhile the user jumps away.
        e.goto(2).unwrap();
        // The late track-0 decoder arrives — no longer wanted.
        assert_eq!(e.provide(0, decoder_for(0)).unwrap(), Provided::Discarded);
        assert_eq!(
            e.state(),
            DeckState::Stopped,
            "deck untouched by stale open"
        );
        // The engine now correctly wants track 2.
        assert_eq!(e.poll_open(), Some(2));
        assert_eq!(e.provide(2, decoder_for(2)).unwrap(), Provided::Loaded);
    }

    #[test]
    fn stop_disarms_and_pump_is_idle() {
        let mut e = engine::<1>();
        e.set_queue(2, 0);
        service(&mut e);
        e.pump().unwrap();
        e.stop();
        assert_eq!(e.state(), DeckState::Stopped);
        assert_eq!(e.pump().unwrap(), Progress::Idle);
        assert_eq!(e.poll_open(), None, "disarmed: nothing to open");
    }

    #[test]
    fn empty_queue_is_inert() {
        let mut e = engine::<1>();
        assert!(e.is_empty());
        assert_eq!(e.cursor(), None);
        assert_eq!(e.poll_open(), None);
        e.next().unwrap();
        e.prev().unwrap();
        e.goto(0).unwrap();
        assert_eq!(e.pump().unwrap(), Progress::Idle);
    }

    #[test]
    fn depth_two_preload_primes_two_successors() {
        let mut e = engine::<2>();
        e.set_queue(4, 0);
        assert_eq!(e.poll_open(), Some(0));
        e.provide(0, decoder_for(0)).unwrap();
        // Two preload slots → tracks 1 and 2 get primed, in order.
        assert_eq!(e.poll_open(), Some(1));
        assert_eq!(e.provide(1, decoder_for(1)).unwrap(), Provided::Preloaded);
        assert_eq!(e.poll_open(), Some(2));
        assert_eq!(e.provide(2, decoder_for(2)).unwrap(), Provided::Preloaded);
        assert_eq!(e.poll_open(), None, "both slots full");
        assert_eq!(e.deck().free_preload_slots(), 0);
        // Play to the end: 3 gapless boundaries across 4 tracks.
        assert_eq!(run_to_end(&mut e), 3);
    }

    /// A "slow loader" that only ever loads the current track (never a preload),
    /// forcing an underrun at every boundary. Drives like the real driver (breaks
    /// on `Ended`).
    fn drive_with_underruns<const P: usize>(e: &mut TestEngine<P>, max_swaps: usize) -> Vec<usize> {
        let mut loaded_at = Vec::new();
        for _ in 0..100_000 {
            // Only service when the deck has no current track → never preloads.
            if matches!(e.state(), DeckState::Stopped | DeckState::Ended) {
                if let Some(i) = e.poll_open() {
                    e.provide(i, decoder_for(i)).unwrap();
                    loaded_at.push(e.cursor().unwrap());
                    if loaded_at.len() > max_swaps {
                        return loaded_at;
                    }
                }
            }
            if e.pump().unwrap() == Progress::Ended {
                return loaded_at; // true end of queue (as a driver would break)
            }
        }
        panic!("did not terminate");
    }

    #[test]
    fn underrun_reloads_next_track_instead_of_ending() {
        // Regression: a mid-queue boundary with nothing primed must NOT report a
        // terminal `Ended` (which a driver would treat as end-of-queue and drop
        // the rest). It re-arms and reloads the next track.
        let mut e = engine::<1>();
        e.set_queue(3, 0);
        let loaded = drive_with_underruns(&mut e, 100);
        // Every track was reached despite never preloading.
        assert_eq!(loaded, [0, 1, 2]);
        // `pump` never surfaced `Ended` at a mid-queue boundary — the queue only
        // ends after the last track.
        assert!(!e.is_active());
        let w = &e.deck().sink().written;
        assert_eq!(w.len() as u64, FRAMES * 3);
        assert_eq!(w[(FRAMES as usize) * 2 + 100], sample_of(2));
    }

    #[test]
    fn repeat_all_continues_through_underruns() {
        // Repeat modes must survive underruns too — the pre-fix bug made them
        // unusable via a driver that breaks on `Ended`.
        let mut e = engine::<1>();
        e.set_queue(2, 0);
        e.set_mode(RepeatMode::RepeatAll);
        let mut reloads = Vec::new();
        for _ in 0..100_000 {
            if matches!(e.state(), DeckState::Stopped | DeckState::Ended) {
                if let Some(i) = e.poll_open() {
                    e.provide(i, decoder_for(i)).unwrap();
                    reloads.push(e.cursor().unwrap());
                    if reloads.len() == 5 {
                        break;
                    }
                }
            }
            // RepeatAll must never surface a terminal end.
            assert_ne!(e.pump().unwrap(), Progress::Ended, "RepeatAll must not end");
        }
        // Reloads cycle 0,1,0,1,0 (the initial load, then wrapping successors).
        assert_eq!(reloads, [0, 1, 0, 1, 0]);
    }
}
