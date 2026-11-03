# Playback Queue — Phase 1: Engine Sequencer Seam — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refactor `soul-audio-engine` so "what plays next" is a pluggable `Sequencer` trait the `Engine` reconciles to, with a `LinearSequencer` that preserves today's behavior exactly — unblocking the `soul-audio-queue` crate (Phase 2) with zero regression.

**Architecture:** Extract a `Sequencer` trait (associated `Handle`) and an `Advance` enum. Move today's `len`/`cursor`/`RepeatMode` navigation into a `LinearSequencer` that implements the trait. Make `Engine<K, D, S, PRELOAD>` own the sequencer and drive the `Deck` by **reconciling** its current + preload slots to `seq.current()` / `seq.upcoming(k)` every cycle, so out-of-band sequencer mutations (Phase 2's "Play Next") self-correct. Migrate the CLI driver to `LinearSequencer` with `Handle = usize`.

**Tech Stack:** Rust 2024, `no_std`, `forbid(unsafe_code)`, no-alloc; `soul-audio-core`'s `Deck`.

## Global Constraints

- `#![cfg_attr(not(test), no_std)]` and `#![forbid(unsafe_code)]` on `soul-audio-engine`; **no heap allocation** in the crate (tests may use `extern crate alloc`).
- Edition `2024`, `rust-version` `1.85` (workspace-inherited).
- `Sequencer::Handle: Copy + Eq`.
- Engine preload bookkeeping uses a fixed `[Option<S::Handle>; PRELOAD]` array (no alloc).
- `cargo xtask verify` must stay green: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and the thumbv7em `soul-audio-core` / `soul-audio-engine` / `soul-audio-firmware` builds.
- Commits: Conventional Commits; **NO `Co-Authored-By: Claude` or `Claude-Session` trailers** (the repo's lefthook `commit-msg` hook rejects them).
- Engine tests run with `cargo test -p soul-audio-engine`.

---

## File Structure

- **Create** `crates/soul-audio-engine/src/sequencer.rs` — `Sequencer` trait + `Advance` enum. One responsibility: the seam contract.
- **Create** `crates/soul-audio-engine/src/linear.rs` — `RepeatMode` + `LinearSequencer` (today's index navigation behind the trait) + its unit tests.
- **Create** `crates/soul-audio-engine/src/engine.rs` — `Engine`, `Provided`, the reconcile driver + its tests (moved out of `lib.rs`).
- **Modify** `crates/soul-audio-engine/src/lib.rs` — reduce to crate attrs, `mod` decls, and re-exports.
- **Modify** `crates/soul-audio-cli/src/driver.rs` — construct `Engine::new(deck, LinearSequencer::new())`; set up the queue via `sequencer_mut().set_queue(..)` + `rearm()`; read the index via `current()`.

`RepeatMode` moves from `lib.rs` into `linear.rs` (it is `LinearSequencer`-specific; Phase 2's `TwoLevelQueue` owns its own `Repeat` enum).

---

## Task 1: The `Sequencer` seam (trait + `Advance`)

**Files:**
- Create: `crates/soul-audio-engine/src/sequencer.rs`
- Modify: `crates/soul-audio-engine/src/lib.rs`

**Interfaces:**
- Produces: `trait Sequencer { type Handle: Copy + Eq; fn current(&self) -> Option<Self::Handle>; fn upcoming(&self, k: usize) -> Option<Self::Handle>; fn advance(&mut self, trigger: Advance) -> Option<Self::Handle>; fn prev(&mut self) -> Option<Self::Handle>; fn goto(&mut self, pos: usize) -> Option<Self::Handle>; }` and `enum Advance { Natural, Skip }`.

- [ ] **Step 1: Write the failing test**

Create `crates/soul-audio-engine/src/sequencer.rs`:

```rust
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
```

Add to `crates/soul-audio-engine/src/lib.rs` (below the crate-level attrs/doc, above the existing `Engine` code for now):

```rust
mod sequencer;
pub use sequencer::{Advance, Sequencer};
```

- [ ] **Step 2: Run test to verify it fails, then compiles green**

Run: `cargo test -p soul-audio-engine sequencer::tests::trait_is_usable_through_a_stub -- --nocapture`
Expected: the file compiles and the test **passes** (this task defines new code with an inline stub; there is no pre-existing symbol to fail on — the gate is that it compiles and the stub exercises every trait method).

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-engine/src/sequencer.rs crates/soul-audio-engine/src/lib.rs
git commit -m "feat(engine): add Sequencer trait + Advance enum (the queue seam)"
```

---

## Task 2: `LinearSequencer` (today's navigation behind the trait)

**Files:**
- Create: `crates/soul-audio-engine/src/linear.rs`
- Modify: `crates/soul-audio-engine/src/lib.rs`

**Interfaces:**
- Consumes: `Sequencer`, `Advance` (Task 1).
- Produces: `enum RepeatMode { Normal, RepeatOne, RepeatAll }` (Default = Normal); `struct LinearSequencer` with `fn new() -> Self`, `fn set_queue(&mut self, len: usize, start: usize)`, `fn set_mode(&mut self, mode: RepeatMode)`, `fn mode(&self) -> RepeatMode`, `fn len(&self) -> usize`, `fn is_empty(&self) -> bool`; `impl Sequencer for LinearSequencer { type Handle = usize; }`.

- [ ] **Step 1: Write the failing test**

Create `crates/soul-audio-engine/src/linear.rs`:

```rust
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
```

Add to `crates/soul-audio-engine/src/lib.rs`:

```rust
mod linear;
pub use linear::{LinearSequencer, RepeatMode};
```

Remove the old `RepeatMode` definition that currently lives in `lib.rs` (it is replaced by the one in `linear.rs`).

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p soul-audio-engine linear::tests -- --nocapture`
Expected: all 5 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-engine/src/linear.rs crates/soul-audio-engine/src/lib.rs
git commit -m "feat(engine): LinearSequencer (index cursor + repeat modes) impl Sequencer"
```

---

## Task 3: `Engine` reconcile driver — core (load, gapless preload, play-to-end)

**Files:**
- Create: `crates/soul-audio-engine/src/engine.rs`
- Modify: `crates/soul-audio-engine/src/lib.rs`

**Interfaces:**
- Consumes: `Sequencer`, `Advance` (Task 1); `soul_audio_core::{Deck, DeckError, DeckState, Progress, SeekableDecoder, Sink}`.
- Produces: `enum Provided { Loaded, Preloaded, Discarded }`; `struct Engine<K, D, S, const PRELOAD: usize>` with `fn new(deck, seq) -> Self`, `fn sequencer(&self) -> &S`, `fn sequencer_mut(&mut self) -> &mut S`, `fn rearm(&mut self)`, `fn current(&self) -> Option<S::Handle>`, `fn poll_open(&mut self) -> Option<S::Handle>`, `fn provide(&mut self, handle, decoder) -> Result<Provided, DeckError>`, `fn pump(&mut self) -> Result<Progress, DeckError>`, `fn is_active(&self) -> bool`, `fn state(&self) -> DeckState`, `fn deck(&self) -> &Deck<..>`, `fn deck_mut(&mut self) -> &mut Deck<..>`.

- [ ] **Step 1: Write the core engine + its first tests**

Create `crates/soul-audio-engine/src/engine.rs`:

```rust
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
            if want_current.is_some() && self.primed[0] == want_current {
                self.deck.advance();
                self.shift_primed();
                self.current_handle = want_current;
            } else {
                self.deck.stop();
                self.deck.clear_preloads();
                self.primed = [None; PRELOAD];
                self.have_current = false;
                self.current_handle = None;
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
```

Append to `crates/soul-audio-engine/src/engine.rs` the test scaffolding + first tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::linear::LinearSequencer;
    use soul_audio_core::{
        DecodeError, Decoder, Quality, SampleFormat, SinkError, StreamInfo,
    };

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
            StreamInfo { sample_rate: RATE, channels: 1, format: SampleFormat::Int { bits: 16 } }
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
            StreamInfo { sample_rate: RATE, channels: 1, format: SampleFormat::Int { bits: 24 } }
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
        ConstDecoder { value: (i as i32 + 1) * 100, frames: FRAMES, pos: 0 }
    }

    type E<const P: usize> = Engine<MockSink, ConstDecoder, LinearSequencer, P>;

    fn engine<const P: usize>(len: usize, start: usize) -> E<P> {
        let sink = MockSink { written: Vec::new() };
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
}
```

Replace the module wiring in `crates/soul-audio-engine/src/lib.rs`: remove the old inline `Engine`/`Provided`/`RepeatMode` implementation and its tests entirely, leaving `lib.rs` as:

```rust
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-engine` — the portable playback queue on top of
//! [`soul_audio_core::Deck`]. The [`Engine`] drives a [`Deck`] from a
//! [`Sequencer`] policy (which track plays next); it does no I/O.

mod engine;
mod linear;
mod sequencer;

pub use engine::{Engine, Provided};
pub use linear::{LinearSequencer, RepeatMode};
pub use sequencer::{Advance, Sequencer};
```

- [ ] **Step 2: Run tests to verify they fail, then pass**

Run: `cargo test -p soul-audio-engine engine::tests -- --nocapture`
Expected: after the edits compile, the three tests (`plays_queue_in_order_then_ends`, `gapless_preload_is_primed_before_boundary`, `depth_two_preloads_two_successors`) PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-engine/src/engine.rs crates/soul-audio-engine/src/lib.rs
git commit -m "feat(engine): reconcile-to-Sequencer Engine (load + gapless preload + play-to-end)"
```

---

## Task 4: `Engine` navigation, repeat modes, and underrun re-arm

**Files:**
- Modify: `crates/soul-audio-engine/src/engine.rs`

**Interfaces:**
- Consumes: everything from Task 3.
- Produces (added to `Engine`): `fn next(&mut self) -> Result<(), DeckError>`, `fn prev(&mut self) -> Result<(), DeckError>`, `fn goto(&mut self, pos: usize) -> Result<(), DeckError>`, `fn play(&mut self)`, `fn pause(&mut self)`, `fn stop(&mut self)`, `fn seek(&mut self, frame: u64) -> Result<u64, DeckError>`, `fn set_volume(&mut self, volume: f32)`.

- [ ] **Step 1: Write the failing tests**

Add these methods to the `impl` block in `engine.rs` (before `// --- internals ---`):

```rust
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
```

Add this internal helper alongside `want`/`reconcile`/`shift_primed`:

```rust
    /// Reconcile the deck's current to `seq.current()` after a navigation: reuse
    /// the front primed slot for a gapless swap if it holds the target, else
    /// stop and reload.
    fn reseat_current(&mut self) {
        self.wants_playback = self.seq.current().is_some();
        self.pending = None;
        let target = self.seq.current();
        if self.have_current && target.is_some() && self.primed[0] == target {
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
```

Add these tests to the `tests` module in `engine.rs`:

```rust
    use crate::RepeatMode;

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
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p soul-audio-engine engine::tests -- --nocapture`
Expected: all Task 3 + Task 4 tests PASS (12 total).

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-engine/src/engine.rs
git commit -m "feat(engine): navigation (next/prev/goto), transport, repeat modes, underrun re-arm"
```

---

## Task 5: Prove the reconcile — stale preload discard on out-of-band `upcoming` change

This is the correctness crux Phase 2 (`Play Next`) depends on: when the sequencer's `upcoming(1)` changes under the engine, the engine must discard the now-stale primed slot and prime the new successor.

**Files:**
- Modify: `crates/soul-audio-engine/src/engine.rs`

**Interfaces:**
- Consumes: `Engine`, `Sequencer`, `Advance`, `Provided` (Tasks 1–4).

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `engine.rs`:

```rust
    use core::cell::Cell;

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
        let sink = MockSink { written: Vec::new() };
        let mut e: Engine<MockSink, ConstDecoder, SwapNext, 1> =
            Engine::new(Deck::new(sink, Quality::Fast), SwapNext { cur: 0, next: Cell::new(1) });
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
        assert_eq!(e.deck().free_preload_slots(), 0);
        assert_eq!(e.provide(2, decoder_for(2)).unwrap(), Provided::Preloaded);

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
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test -p soul-audio-engine engine::tests::stale_preload_is_discarded_and_reprimed_when_upcoming_changes -- --nocapture`
Expected: PASS. (If it fails because the stale slot was not discarded, the bug is in `reconcile`'s preload-staleness loop — verify it runs at the top of `poll_open`.)

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-engine/src/engine.rs
git commit -m "test(engine): reconcile discards a stale preload when upcoming() changes"
```

---

## Task 6: Migrate the CLI driver to the new `Engine` API

**Files:**
- Modify: `crates/soul-audio-cli/src/driver.rs`

**Interfaces:**
- Consumes: `Engine::new(deck, seq)`, `sequencer_mut()`, `rearm()`, `current()`, `poll_open()->Option<usize>`, `provide(usize, D)`, `pump()`; `LinearSequencer` (`Handle = usize`).

- [ ] **Step 1: Update the driver**

In `crates/soul-audio-cli/src/driver.rs`, change the import line:

```rust
use soul_audio_engine::{Engine, LinearSequencer, Provided};
```

Replace the engine construction inside `run` (currently `let mut engine = Engine::new(deck); engine.set_queue(paths.len(), 0);`) with:

```rust
    let deck = Deck::<K, OwnedDecoder, PRELOAD>::new(sink, Quality::Hq);
    let mut engine = Engine::new(deck, LinearSequencer::new());
    engine.sequencer_mut().set_queue(paths.len(), 0);
    engine.rearm();
```

In the `Progress::TrackChanged` arm, replace `engine.cursor().unwrap_or(0)` with `engine.current().unwrap_or(0)`:

```rust
                Progress::TrackChanged => {
                    println!("▶ track {} (gapless)", engine.current().unwrap_or(0));
                }
```

The `provide(i, decoder)`, `poll_open()`, `pump()`, and `deck_mut().sink_mut().flush()` calls are unchanged — `poll_open` still returns `Option<usize>` and `provide` still takes a `usize` handle, because `LinearSequencer::Handle = usize`.

- [ ] **Step 2: Run the CLI end-to-end tests to verify no regression**

Run: `cargo test -p soul-audio-cli`
Expected: all e2e tests PASS (including `render_gapless_concatenates_tracks_exactly`, `render_upsamples_44100_to_48000`, `render_keeps_stereo_channels_distinct`, and the real-fixture decode/render tests).

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-cli/src/driver.rs
git commit -m "refactor(cli): drive the engine via LinearSequencer (Handle = usize)"
```

---

## Task 7: Update the engine crate description + full verify gate

**Files:**
- Modify: `crates/soul-audio-engine/Cargo.toml`

- [ ] **Step 1: Update the crate description**

In `crates/soul-audio-engine/Cargo.toml`, change the `description` to reflect the seam:

```toml
description = "Portable no_std playback engine: drives soul-audio-core's Deck from a pluggable Sequencer policy (LinearSequencer built in) with gapless preload."
```

- [ ] **Step 2: Run the full gate**

Run: `cargo xtask verify`
Expected: `fmt` clean, `clippy` clean (`-D warnings`), all host tests pass, the `soul-audio-core` + `soul-audio-engine` no_std thumbv7em builds pass. (The `soul-audio-firmware` thumbv7em link is expected to fail **only** when building inside a nested `.claude/worktrees/` checkout — the FLASH-region duplication artifact; from a normal checkout it links. If verifying inside a worktree, additionally run `cargo test` + the two `--no-default-features` target builds and confirm only the firmware link failed.)

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-engine/Cargo.toml
git commit -m "docs(engine): update crate description for the Sequencer seam"
```

---

## Self-Review

**Spec coverage (against `2026-08-30-playback-queue-design.md` §4):** the `Sequencer` trait + `Advance` (Task 1), the reconcile-to-sequencer engine incl. the Play-Next stale-preload discard (Tasks 3+5), `LinearSequencer` = today's behavior (Task 2), `Handle`-generic `poll_open`/`provide` (Task 3), `sequencer_mut()` mutation path (Task 3), and CLI parity (Task 6) are all covered. §5–§13 (the `TwoLevelQueue`, `Context`, shuffle, persistence, `balanced_shuffle`) are **out of scope for Phase 1** — they are Phase 2, which consumes the `Sequencer` trait this plan finalizes.

**Placeholder scan:** every code step contains complete code; no TBD/TODO; ported tests are written out in full (not "similar to").

**Type consistency:** `Sequencer::Handle` is `usize` throughout the CLI/LinearSequencer path; `poll_open -> Option<S::Handle>` and `provide(S::Handle, D)` match; `primed: [Option<S::Handle>; PRELOAD]` is reset identically in `rearm`/`stop`/`reconcile`/`pump(Ended)`; `current()` replaces the removed `cursor()` in both the engine and the CLI.

**Known follow-up for Phase 2:** the base `Sequencer` trait carries only the driving methods; `TwoLevelQueue`'s mutation API (`play_next`, `set_shuffle`, …) lives on the concrete type and is reached via `engine.sequencer_mut()`, relying on the reconcile proven in Task 5.
