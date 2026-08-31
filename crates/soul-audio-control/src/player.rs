use soul_audio_core::{Deck, DeckError, DeckState, Progress, SeekableDecoder, Sink};
use soul_audio_engine::{Engine, Provided};
use soul_audio_queue::{Context, QueueBuf, Repeat, StopAfter, TwoLevelQueue};

/// The full queue type the [`Player`] drives.
type Queue<C, Up, Hist> = TwoLevelQueue<C, Up, Hist>;

/// A portable playback controller over an [`Engine`] + [`TwoLevelQueue`].
///
/// Generic so the desktop (cpal sink, alloc backings) and the DAP (DMA sink,
/// heapless backings) share one implementation. Holds the one driver-layer policy
/// the queue design deferred: the `prev` restart threshold (§ [`Player::prev`]).
pub struct Player<K, D, C, Up, Hist, const PRELOAD: usize>
where
    K: Sink,
    D: SeekableDecoder,
    C: Context,
    Up: QueueBuf<C::Handle>,
    Hist: QueueBuf<(C::Handle, usize)>,
{
    engine: Engine<K, D, Queue<C, Up, Hist>, PRELOAD>,
    /// If the current track is past this many per-channel frames, `prev` restarts
    /// it instead of stepping back (the ~3-second rule).
    ///
    /// Unit is per-channel **frames**, not seconds/milliseconds. A caller wanting
    /// "~3 s" must convert using a representative sample rate — the true
    /// wall-clock threshold is rate-dependent across a mixed-rate library.
    prev_restart_frames: u64,
}

impl<K, D, C, Up, Hist, const PRELOAD: usize> Player<K, D, C, Up, Hist, PRELOAD>
where
    K: Sink,
    D: SeekableDecoder,
    C: Context,
    Up: QueueBuf<C::Handle>,
    Hist: QueueBuf<(C::Handle, usize)>,
{
    /// Wrap a fully-built `engine` (deck + an initial, possibly empty
    /// `TwoLevelQueue`). `prev_restart_frames` is the prev-3s threshold.
    pub fn new(
        engine: Engine<K, D, Queue<C, Up, Hist>, PRELOAD>,
        prev_restart_frames: u64,
    ) -> Self {
        Self {
            engine,
            prev_restart_frames,
        }
    }

    // --- driving (delegates the driver loop) -------------------------------

    /// The next handle to open off-thread, if any.
    pub fn poll_open(&mut self) -> Option<C::Handle> {
        self.engine.poll_open()
    }

    /// Install an opened decoder for `handle`.
    pub fn provide(&mut self, handle: C::Handle, decoder: D) -> Result<Provided, DeckError> {
        self.engine.provide(handle, decoder)
    }

    /// Produce one block of audio.
    pub fn pump(&mut self) -> Result<Progress, DeckError> {
        self.engine.pump()
    }

    // --- transport ---------------------------------------------------------

    /// Resume playback.
    pub fn play(&mut self) {
        self.engine.play();
    }

    /// Pause (retains position).
    pub fn pause(&mut self) {
        self.engine.pause();
    }

    /// Stop and disarm.
    pub fn stop(&mut self) {
        self.engine.stop();
    }

    /// Toggle play/pause based on the current deck state.
    pub fn toggle_play(&mut self) {
        if self.engine.state() == DeckState::Playing {
            self.engine.pause();
        } else {
            self.engine.play();
        }
    }

    /// Set the volume (ramped), `0.0..=1.0`.
    pub fn set_volume(&mut self, volume: f32) {
        self.engine.set_volume(volume);
    }

    /// Seek the current track to per-channel `frame`.
    ///
    /// # Errors
    /// Propagates [`DeckError`].
    pub fn seek_frames(&mut self, frame: u64) -> Result<u64, DeckError> {
        self.engine.seek(frame)
    }

    /// Replace the source context and start at `start`, then arm + play.
    pub fn set_source(&mut self, ctx: C, start: usize) {
        self.engine.sequencer_mut().set_source(ctx, start);
        self.engine.rearm();
        self.engine.play();
    }

    /// Skip to the next track. No-op at the end of a Normal queue.
    ///
    /// # Errors
    /// Propagates [`DeckError`].
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<(), DeckError> {
        self.engine.next()
    }

    /// Previous with the ~3-second rule: if the current track is past
    /// `prev_restart_frames`, restart it; otherwise step to the previous track.
    ///
    /// # Errors
    /// Propagates [`DeckError`].
    pub fn prev(&mut self) -> Result<(), DeckError> {
        if self.engine.deck().position_frames() > self.prev_restart_frames {
            self.engine.seek(0)?;
            Ok(())
        } else {
            self.engine.prev()
        }
    }

    /// Jump to source position `natural_index`.
    ///
    /// # Errors
    /// Propagates [`DeckError`].
    pub fn goto(&mut self, natural_index: usize) -> Result<(), DeckError> {
        self.engine.goto(natural_index)
    }

    /// "Play Next" — insert `handle` at the front of the Up-Next queue.
    ///
    /// # Errors
    /// Returns the queue-full error when the bounded backing is full.
    pub fn play_next(&mut self, handle: C::Handle) -> Result<(), soul_audio_queue::Full> {
        self.engine.sequencer_mut().play_next(handle)
    }

    /// "Add to Queue" — append `handle` to the Up-Next queue.
    ///
    /// # Errors
    /// Returns the queue-full error when the bounded backing is full.
    pub fn enqueue(&mut self, handle: C::Handle) -> Result<(), soul_audio_queue::Full> {
        self.engine.sequencer_mut().enqueue(handle)
    }

    /// Clear the Up-Next queue.
    pub fn clear_queue(&mut self) {
        self.engine.sequencer_mut().clear_upnext();
    }

    /// Turn shuffle on (with `seed`) or off, keeping the current track.
    pub fn set_shuffle(&mut self, on: bool, seed: u64) {
        self.engine.sequencer_mut().set_shuffle(on, seed);
    }

    /// Set the repeat mode.
    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.engine.sequencer_mut().set_repeat(repeat);
    }

    /// Set the one-shot stop request.
    pub fn set_stop_after(&mut self, stop_after: StopAfter) {
        self.engine.sequencer_mut().set_stop_after(stop_after);
    }

    /// Apply a high-level [`Command`](crate::Command). Queue-full errors from
    /// `PlayNext`/`Enqueue` are ignored (the bounded backing rejected the add);
    /// deck errors propagate.
    ///
    /// # Errors
    /// Propagates [`DeckError`] from transport commands.
    pub fn apply(&mut self, cmd: crate::Command<C>) -> Result<(), DeckError> {
        use crate::Command::*;
        match cmd {
            PlayContext { ctx, start } => self.set_source(ctx, start),
            TogglePlay => self.toggle_play(),
            Play => self.play(),
            Pause => self.pause(),
            Stop => self.stop(),
            Next => self.next()?,
            Prev => self.prev()?,
            SeekFrames(f) => {
                self.seek_frames(f)?;
            }
            SetVolume(v) => self.set_volume(v),
            PlayNext(h) => {
                let _ = self.play_next(h);
            }
            Enqueue(h) => {
                let _ = self.enqueue(h);
            }
            ClearQueue => self.clear_queue(),
            SetShuffle { on, seed } => self.set_shuffle(on, seed),
            SetRepeat(r) => self.set_repeat(r),
            SetStopAfter(s) => self.set_stop_after(s),
            Goto(pos) => self.goto(pos)?,
        }
        Ok(())
    }

    // --- state getters -----------------------------------------------------

    /// The current track handle, if any.
    #[must_use]
    pub fn current(&self) -> Option<C::Handle> {
        self.engine.current()
    }

    /// Transport state.
    #[must_use]
    pub fn state(&self) -> DeckState {
        self.engine.state()
    }

    /// Whether the player still wants tracks playing.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.engine.is_active()
    }

    /// Current per-channel position in frames.
    #[must_use]
    pub fn position_frames(&self) -> u64 {
        self.engine.deck().position_frames()
    }

    /// Current track duration in frames, if known.
    #[must_use]
    pub fn duration_frames(&self) -> Option<u64> {
        self.engine.deck().duration_frames()
    }

    /// The current track's source sample rate (for frame↔seconds).
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.engine.deck().sample_rate()
    }

    /// The shuffle state.
    #[must_use]
    pub fn is_shuffled(&self) -> bool {
        self.engine.sequencer().is_shuffled()
    }

    /// The repeat mode.
    #[must_use]
    pub fn repeat(&self) -> Repeat {
        self.engine.sequencer().repeat()
    }

    /// Read-only access to the queue (Up-Next iteration, `pos`/`source_len`,
    /// `snapshot`, etc.). No `_mut` — mutation goes through the intent methods.
    #[must_use]
    pub fn sequencer(&self) -> &Queue<C, Up, Hist> {
        self.engine.sequencer()
    }

    /// Escape hatch: the underlying deck (position/duration/sink queries).
    #[must_use]
    pub fn deck(&self) -> &Deck<K, D, PRELOAD> {
        self.engine.deck()
    }

    /// Escape hatch: mutable deck (e.g. `sink_mut().flush()`).
    pub fn deck_mut(&mut self) -> &mut Deck<K, D, PRELOAD> {
        self.engine.deck_mut()
    }
}

// --- test harness reused by Tasks 3–5 --------------------------------------

#[cfg(test)]
pub(crate) mod testkit {
    use super::*;
    use soul_audio_core::{DecodeError, Decoder, Quality, SampleFormat, SinkError, StreamInfo};
    use soul_audio_queue::SliceContext;

    extern crate alloc;
    use alloc::vec::Vec;
    use heapless::Deque; // bounded test backings; the heapless QueueBuf impl is never feature-gated

    pub const RATE: u32 = 48_000;
    pub const FRAMES: u64 = 400;

    pub struct ConstDecoder {
        pub value: i32,
        pub frames: u64,
        pub pos: u64,
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
            out[..want].fill(self.value);
            self.pos += want as u64;
            Ok(want)
        }
    }
    impl SeekableDecoder for ConstDecoder {
        fn seek_to(&mut self, s: u64) -> Result<u64, DecodeError> {
            self.pos = s.min(self.frames);
            Ok(self.pos)
        }
        fn total_frames(&self) -> Option<u64> {
            Some(self.frames)
        }
    }

    pub struct MockSink {
        pub written: Vec<i32>,
    }
    impl Sink for MockSink {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: RATE,
                channels: 1,
                format: SampleFormat::Int { bits: 24 },
            }
        }
        fn write(&mut self, f: &[i32]) -> Result<(), SinkError> {
            self.written.extend_from_slice(f);
            Ok(())
        }
        fn flush(&mut self) -> Result<(), SinkError> {
            Ok(())
        }
    }

    pub fn decoder_for(id: u32) -> ConstDecoder {
        ConstDecoder {
            value: (id as i32 + 1) * 100,
            frames: FRAMES,
            pos: 0,
        }
    }
    pub fn sample_of(id: u32) -> i32 {
        ((id as i32 + 1) * 100) << 8
    }

    pub type Ctx = SliceContext<u32, Vec<u32>>;
    pub type P = Player<MockSink, ConstDecoder, Ctx, Deque<u32, 8>, Deque<(u32, usize), 16>, 1>;

    /// A player over an empty context; `prev_restart_frames = threshold`.
    pub fn player(threshold: u64) -> P {
        let deck = Deck::new(
            MockSink {
                written: Vec::new(),
            },
            Quality::Fast,
        );
        let queue = TwoLevelQueue::new(SliceContext::new(Vec::new()), Deque::new(), Deque::new());
        Player::new(Engine::new(deck, queue), threshold)
    }

    /// Service every open the player wants, opening the id's decoder.
    pub fn service(p: &mut P) {
        while let Some(id) = p.poll_open() {
            p.provide(id, decoder_for(id)).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use soul_audio_core::DeckState;

    #[test]
    fn empty_player_is_idle() {
        let mut p = player(0);
        assert_eq!(p.current(), None);
        assert_eq!(p.poll_open(), None);
        assert!(!p.is_active());
        assert_eq!(p.state(), DeckState::Stopped);
    }

    use super::testkit;
    use soul_audio_core::Progress;
    use soul_audio_queue::{Repeat, SliceContext};

    fn run_to_end(p: &mut P) -> usize {
        let mut swaps = 0;
        for _ in 0..100_000 {
            testkit::service(p);
            match p.pump().unwrap() {
                Progress::TrackChanged => swaps += 1,
                Progress::Ended => return swaps,
                _ => {}
            }
        }
        panic!("did not end");
    }

    #[test]
    fn play_context_plays_the_library_in_order() {
        let mut p = player(0);
        p.set_source(SliceContext::new(vec![0u32, 1, 2]), 0);
        assert_eq!(p.current(), Some(0));
        let swaps = run_to_end(&mut p);
        assert_eq!(swaps, 2);
        let w = &p.deck().sink().written;
        assert_eq!(w[10], sample_of(0));
        assert_eq!(w[FRAMES as usize + 10], sample_of(1));
    }

    #[test]
    fn prev_restarts_past_threshold_else_steps_back() {
        let mut p = player(FRAMES / 2); // threshold = mid-track
        p.set_source(SliceContext::new(vec![0u32, 1, 2]), 0); // start on track 0
        testkit::service(&mut p);
        // Play PAST the threshold, then prev → restart (position resets, cursor stays 1).
        // Timing note: the deck decodes ~256 frames/pump (BLOCK) and FRAMES=400,
        // so track 0 exhausts on pump 2 (256+144) and pump 3 lands track 1 at
        // pos ~368 > threshold (FRAMES/2 = 200). If the deck's block size changes,
        // retune this loop count.
        for _ in 0..3 {
            testkit::service(&mut p);
            p.pump().unwrap();
        }
        assert!(p.position_frames() > FRAMES / 2);
        p.prev().unwrap();
        assert_eq!(
            p.current(),
            Some(1),
            "prev past threshold restarts, stays on track 1"
        );
        assert_eq!(p.position_frames(), 0);
        // Now BEFORE the threshold (fresh track), prev → step back to track 0.
        p.prev().unwrap();
        assert_eq!(
            p.current(),
            Some(0),
            "prev under threshold steps to the previous track"
        );
    }

    #[test]
    fn play_next_reprimes_and_wins() {
        let mut p = player(0);
        p.set_source(SliceContext::new(vec![0u32, 1, 2]), 0);
        testkit::service(&mut p); // current 0, preload 1
        p.pump().unwrap();
        p.play_next(2).unwrap(); // upcoming(1) 1 -> 2
        let mut swapped_to = None;
        for _ in 0..100_000 {
            testkit::service(&mut p);
            if let Progress::TrackChanged = p.pump().unwrap() {
                swapped_to = p.current();
                break;
            }
        }
        assert_eq!(
            swapped_to,
            Some(2),
            "Play Next winner plays next, gaplessly"
        );
    }

    #[test]
    fn repeat_and_shuffle_flags_reach_the_queue() {
        let mut p = player(0);
        p.set_source(SliceContext::new(vec![0u32, 1, 2, 3]), 0);
        p.set_repeat(Repeat::All);
        assert_eq!(p.repeat(), Repeat::All);
        p.set_shuffle(true, 42);
        assert!(p.is_shuffled());
        p.set_shuffle(false, 0);
        assert!(!p.is_shuffled());
    }

    use crate::Command;

    #[test]
    fn apply_drives_a_command_sequence() {
        let mut p = player(0);
        p.apply(Command::PlayContext {
            ctx: SliceContext::new(vec![0u32, 1, 2]),
            start: 0,
        })
        .unwrap();
        assert_eq!(p.current(), Some(0));
        p.apply(Command::SetRepeat(Repeat::All)).unwrap();
        assert_eq!(p.repeat(), Repeat::All);
        p.apply(Command::PlayNext(2)).unwrap();
        testkit::service(&mut p); // loads the current decoder so the deck is actually Playing
        p.apply(Command::TogglePlay).unwrap(); // was Playing → Paused
        assert_eq!(p.state(), soul_audio_core::DeckState::Paused);
        p.apply(Command::TogglePlay).unwrap(); // → Playing
        assert_eq!(p.state(), soul_audio_core::DeckState::Playing);
    }

    #[test]
    fn sequencer_accessor_reads_the_queue() {
        let mut p = player(0);
        p.set_source(SliceContext::new(vec![0u32, 1, 2]), 0);
        p.play_next(2).unwrap();
        assert_eq!(p.sequencer().upnext_len(), 1);
        assert_eq!(p.sequencer().upnext_iter().collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn apply_play_resumes() {
        let mut p = player(0);
        p.set_source(SliceContext::new(vec![0u32, 1, 2]), 0);
        testkit::service(&mut p); // loads the current decoder so the deck is actually Playing
        p.apply(Command::Pause).unwrap();
        assert_eq!(p.state(), soul_audio_core::DeckState::Paused);
        p.apply(Command::Play).unwrap();
        assert_eq!(p.state(), soul_audio_core::DeckState::Playing);
    }
}
