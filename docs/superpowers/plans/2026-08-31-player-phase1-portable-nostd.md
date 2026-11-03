# Desktop Player — Plan 1: Portable `no_std` Foundation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the portable `no_std` pieces the desktop player (and the future DAP) build on — a sample-format `convert` module in `soul-audio-core`, a generic `SliceContext` in `soul-audio-queue`, and a new `soul-audio-control` crate whose `Player` owns `Engine`+`TwoLevelQueue` and applies high-level `Command` intents (incl. the prev-3s policy). All headless-testable; no app code yet.

**Architecture:** `soul-audio-control` is the portable "driver-layer" the queue design deferred — a thin, generic wrapper: `Player<K: Sink, D: SeekableDecoder, C: Context, Up, Hist, const PRELOAD>` around `Engine<K, D, TwoLevelQueue<C, Up, Hist>, PRELOAD>`, exposing transport + queue intents and a `Command` enum. It holds one bit of policy (the prev-3-second-restart threshold); the engine stays a pure mechanism.

**Tech Stack:** Rust 2024, `no_std`, `forbid(unsafe_code)`; `soul-audio-{core,engine,queue}`.

## Global Constraints

- Every crate stays `no_std` (`#![cfg_attr(not(any(test, feature = "std")), no_std)]` where features exist; `soul-audio-core`/`engine` use `#![cfg_attr(not(test), no_std)]`) + `#![forbid(unsafe_code)]`, no heap allocation in non-test/default code.
- Edition 2024, rust-version 1.85 (workspace-inherited).
- `soul-audio-control` deps: `soul-audio-core`, `soul-audio-engine`, `soul-audio-queue` (path). No `alloc` needed in `control` itself (it's generic over the queue backings the caller supplies).
- `cargo xtask verify` must stay green: fmt, clippy `-D warnings`, all tests, and the thumbv7em builds of `soul-audio-core`/`engine`/`queue`/**`control`**/firmware. Add `soul-audio-control` to `xtask`'s thumbv7em gate.
- Add `soul-audio-control` to the root `Cargo.toml` `members` AND `default-members`.
- The **pre-commit hook runs workspace-wide clippy**, so every commit must leave the whole workspace clippy-clean.
- Commits: Conventional Commits; **NO `Co-Authored-By: Claude` / `Claude-Session` trailers** (the hook rejects them). No `--no-verify`.

---

## File Structure

- **Create** `crates/soul-audio-core/src/convert.rs`; **modify** `crates/soul-audio-core/src/lib.rs` (mod + re-export).
- **Create** `crates/soul-audio-queue/src/slice.rs`; **modify** `crates/soul-audio-queue/src/lib.rs` (mod + re-export).
- **Create** `crates/soul-audio-control/{Cargo.toml, src/lib.rs, src/player.rs, src/command.rs}`.
- **Modify** `Cargo.toml` (root) — add `soul-audio-control` to `members` + `default-members`.
- **Modify** `xtask/src/main.rs` — add `soul-audio-control` to the thumbv7em `verify()` builds.

---

## Task 1: `soul-audio-core::convert` (no_std sample-format conversion)

**Files:**
- Create: `crates/soul-audio-core/src/convert.rs`
- Modify: `crates/soul-audio-core/src/lib.rs`

**Interfaces:**
- Produces: `pub const CANONICAL_BITS: u8 = 24;`, `pub fn fill_f32(next: impl FnMut() -> Option<i32>, out: &mut [f32])`, `pub fn fill_i16(next: impl FnMut() -> Option<i32>, out: &mut [i16])`.

- [ ] **Step 1: Write the module + tests**

Create `crates/soul-audio-core/src/convert.rs` (the CLI's `convert.rs` verbatim, except the `f32` scale is a `const` so it is `no_std`-safe — `f32::powi` is std-only):

```rust
//! Canonical-24-bit → device-format sample conversion — the arithmetic an output
//! sink's real-time callback runs on each buffer. `no_std`, device-free, so the
//! exact samples reaching a DAC are unit-tested without hardware. Shared by the
//! desktop cpal sink and a future SAI/I2S DMA sink.

/// Canonical bit depth the deck normalizes to (`±2^23`).
pub const CANONICAL_BITS: u8 = 24;

/// Peak magnitude of a canonical sample (`2^(CANONICAL_BITS-1)` = `2^23`), as f32.
/// A `const` (not `2f32.powi(..)`) because `f32::powi` is std-only.
const F32_SCALE: f32 = (1i32 << (CANONICAL_BITS - 1)) as f32;

/// Fill `out` with f32 device samples. Each is popped from `next` (`None` = the
/// ring underran → silence) and scaled so the canonical peak `±2^23` maps to
/// `±1.0` full scale.
pub fn fill_f32(mut next: impl FnMut() -> Option<i32>, out: &mut [f32]) {
    for s in out.iter_mut() {
        *s = next().unwrap_or(0) as f32 / F32_SCALE;
    }
}

/// Fill `out` with i16 device samples: pop from `next` (silence on underrun),
/// shift 24→16 bit, clamp to the i16 range.
pub fn fill_i16(mut next: impl FnMut() -> Option<i32>, out: &mut [i16]) {
    let shift = i32::from(CANONICAL_BITS) - 16;
    for s in out.iter_mut() {
        let v = next().unwrap_or(0) >> shift;
        *s = v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feeder(samples: &[i32]) -> impl FnMut() -> Option<i32> + '_ {
        let mut it = samples.iter().copied();
        move || it.next()
    }

    #[test]
    fn f32_scales_full_scale_and_silences_on_underrun() {
        let src = [8_388_607, -8_388_608, 0];
        let mut out = [0f32; 5];
        fill_f32(feeder(&src), &mut out);
        assert!((out[0] - 0.999_999_9).abs() < 1e-3, "+peak → ~+1.0");
        assert!((out[1] + 1.0).abs() < 1e-6, "-peak → -1.0");
        assert_eq!(out[2], 0.0);
        assert_eq!(out[3], 0.0, "underrun → silence");
        assert_eq!(out[4], 0.0, "underrun → silence");
    }

    #[test]
    fn i16_shifts_clamps_and_silences_on_underrun() {
        let src = [256_000, -256_000, 8_388_607, -8_388_608, 12_000_000, -12_000_000];
        let mut out = [0i16; 8];
        fill_i16(feeder(&src), &mut out);
        assert_eq!(out[0], 1000);
        assert_eq!(out[1], -1000);
        assert_eq!(out[2], i16::MAX);
        assert_eq!(out[3], i16::MIN);
        assert_eq!(out[4], i16::MAX, "over-range clamps");
        assert_eq!(out[5], i16::MIN, "over-range clamps");
        assert_eq!(out[6], 0, "underrun → silence");
        assert_eq!(out[7], 0, "underrun → silence");
    }
}
```

Add to `crates/soul-audio-core/src/lib.rs` (a new `pub mod` + a re-export of the two fns is optional; the module being `pub` is enough):

```rust
pub mod convert;
```

- [ ] **Step 2: Run tests + no_std build**

Run: `cargo test -p soul-audio-core convert::tests`
Expected: both pass.
Run: `cargo build -p soul-audio-core --target thumbv7em-none-eabihf --no-default-features`
Expected: Finished (confirms `convert` is `no_std` — no `powi`).

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-core/src/convert.rs crates/soul-audio-core/src/lib.rs
git commit -m "feat(core): no_std sample-format convert module (canonical-24bit -> i16/f32)"
```

---

## Task 2: `soul-audio-queue::SliceContext` (generic no_std context)

**Files:**
- Create: `crates/soul-audio-queue/src/slice.rs`
- Modify: `crates/soul-audio-queue/src/lib.rs`

**Interfaces:**
- Consumes: `Context` (from `soul-audio-queue`).
- Produces: `pub struct SliceContext<H, B>` with `pub fn new(backing: B) -> Self`, `impl<H: Copy + Eq, B: AsRef<[H]>> Context for SliceContext<H, B> { type Handle = H; }`.

- [ ] **Step 1: Write the module + tests**

Create `crates/soul-audio-queue/src/slice.rs`:

```rust
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
        Self { backing, _marker: PhantomData }
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
```

Add to `crates/soul-audio-queue/src/lib.rs`:

```rust
mod slice;
pub use slice::SliceContext;
```

(The `slice::tests` use `alloc::vec` — the crate is `std` under `cfg(test)`, so `alloc`/`Vec` are available without the `alloc` feature; write `alloc::vec!`/`alloc::vec::Vec` since the crate declares `extern crate alloc` only under the `alloc` feature — if that path fails under the plain-test build, use `Vec`/`vec!` from the std prelude instead. Match whatever compiles; do not change the non-test code.)

- [ ] **Step 2: Run tests + no_std build**

Run: `cargo test -p soul-audio-queue slice::tests`
Expected: both pass.
Run: `cargo build -p soul-audio-queue --target thumbv7em-none-eabihf --no-default-features`
Expected: Finished (SliceContext is no-alloc).

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-queue/src/slice.rs crates/soul-audio-queue/src/lib.rs
git commit -m "feat(queue): generic no_std SliceContext (Context over any AsRef<[H]>)"
```

---

## Task 3: `soul-audio-control` scaffold + `Player` transport

**Files:**
- Create: `crates/soul-audio-control/Cargo.toml`, `crates/soul-audio-control/src/lib.rs`, `crates/soul-audio-control/src/player.rs`
- Modify: `Cargo.toml` (root)

**Interfaces:**
- Consumes: `soul_audio_core::{Sink, SeekableDecoder, Deck, DeckState, Progress, DeckError}`, `soul_audio_engine::{Engine, Provided}`, `soul_audio_queue::{Context, QueueBuf, TwoLevelQueue}`.
- Produces: `pub struct Player<K, D, C, Up, Hist, const PRELOAD: usize>` with `new(engine, prev_restart_frames)`, transport (`play/pause/stop/set_volume`, `seek_frames`, `pump`, `poll_open`, `provide`), and state getters (`current`, `state`, `position_frames`, `duration_frames`, `sample_rate`, `is_active`, `engine`/`engine_mut`).

- [ ] **Step 1: Create the manifest + workspace wiring**

Create `crates/soul-audio-control/Cargo.toml`:

```toml
[package]
name = "soul-audio-control"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Portable no_std playback controller: applies high-level Command intents (transport + queue + prev-3s policy) to soul-audio-engine's Engine over a soul-audio-queue TwoLevelQueue."

[lints]
workspace = true

[dependencies]
soul-audio-core = { path = "../soul-audio-core" }
soul-audio-engine = { path = "../soul-audio-engine" }
soul-audio-queue = { path = "../soul-audio-queue" }

[dev-dependencies]
heapless = "0.8"                              # test backings (QueueBuf impl is always available, un-feature-gated)
```

> **Do NOT** add `features = ["alloc"]` to the `soul-audio-queue` dependency — that
> would force `soul-audio-queue`'s `alloc` on for every consumer (including the DAP
> build of `control`), breaking no-alloc. Tests use `heapless` backings instead.

Add `"crates/soul-audio-control"` to BOTH `members` and `default-members` in the root `Cargo.toml` (after `soul-audio-queue` in each).

- [ ] **Step 2: Write the `Player` struct + transport (with failing tests)**

Create `crates/soul-audio-control/src/lib.rs`:

```rust
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-control` — a portable `no_std` playback controller. [`Player`]
//! owns a [`soul_audio_engine::Engine`] over a
//! [`soul_audio_queue::TwoLevelQueue`] and turns high-level [`Command`] intents
//! (transport + queue + the prev-3-second-restart policy) into engine/queue
//! calls. The desktop app and the future DAP firmware share it.

mod command;
mod player;

pub use command::Command;
pub use player::Player;
```

Create `crates/soul-audio-control/src/player.rs`:

```rust
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
    pub fn new(engine: Engine<K, D, Queue<C, Up, Hist>, PRELOAD>, prev_restart_frames: u64) -> Self {
        Self { engine, prev_restart_frames }
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
    use soul_audio_core::{
        DecodeError, Decoder, Quality, SampleFormat, SinkError, StreamInfo,
    };
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
            StreamInfo { sample_rate: RATE, channels: 1, format: SampleFormat::Int { bits: 16 } }
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
            StreamInfo { sample_rate: RATE, channels: 1, format: SampleFormat::Int { bits: 24 } }
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
        ConstDecoder { value: (id as i32 + 1) * 100, frames: FRAMES, pos: 0 }
    }
    pub fn sample_of(id: u32) -> i32 {
        ((id as i32 + 1) * 100) << 8
    }

    pub type Ctx = SliceContext<u32, Vec<u32>>;
    pub type P = Player<MockSink, ConstDecoder, Ctx, Deque<u32, 8>, Deque<(u32, usize), 16>, 1>;

    /// A player over an empty context; `prev_restart_frames = threshold`.
    pub fn player(threshold: u64) -> P {
        let deck = Deck::new(MockSink { written: Vec::new() }, Quality::Fast);
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
}
```

- [ ] **Step 3: Run tests + no_std build**

Run: `cargo test -p soul-audio-control player::tests`
Expected: `empty_player_is_idle` passes.
Run: `cargo build -p soul-audio-control --target thumbv7em-none-eabihf --no-default-features`
Expected: Finished (the crate is `no_std`; the generic `Player` carries no allocation).

- [ ] **Step 4: Commit**

```bash
git add crates/soul-audio-control/ Cargo.toml
git commit -m "feat(control): soul-audio-control crate + Player transport over Engine+TwoLevelQueue"
```

---

## Task 4: `Player` — context, navigation, queue intents

**Files:**
- Modify: `crates/soul-audio-control/src/player.rs`

**Interfaces:**
- Produces (added to `Player`): `set_source(ctx, start)`, `next`, `prev` (3-s rule), `goto`, `play_next`, `enqueue`, `clear_queue`, `set_shuffle`, `set_repeat`, `set_stop_after`.

- [ ] **Step 1: Add the methods + failing tests**

Add to the `impl Player` block (after `seek_frames`, before `// --- state getters`):

```rust
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
```

Add tests to the `tests` module in `player.rs`:

```rust
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
        p.set_source(SliceContext::new(vec![0u32, 1, 2]), 1); // start on track 1
        testkit::service(&mut p);
        // Play PAST the threshold, then prev → restart (position resets, cursor stays 1).
        for _ in 0..3 {
            testkit::service(&mut p);
            p.pump().unwrap();
        }
        assert!(p.position_frames() > FRAMES / 2);
        p.prev().unwrap();
        assert_eq!(p.current(), Some(1), "prev past threshold restarts, stays on track 1");
        assert_eq!(p.position_frames(), 0);
        // Now BEFORE the threshold (fresh track), prev → step back to track 0.
        p.prev().unwrap();
        assert_eq!(p.current(), Some(0), "prev under threshold steps to the previous track");
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
        assert_eq!(swapped_to, Some(2), "Play Next winner plays next, gaplessly");
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
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p soul-audio-control player::tests`
Expected: all pass (the empty-idle test + the 4 new ones).

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-control/src/player.rs
git commit -m "feat(control): Player context/navigation/queue intents + prev-3s rule"
```

---

## Task 5: `Command` enum + `Player::apply`

**Files:**
- Create: `crates/soul-audio-control/src/command.rs`
- Modify: `crates/soul-audio-control/src/player.rs`

**Interfaces:**
- Produces: `pub enum Command<C: Context>` (variants: `PlayContext { ctx, start }`, `TogglePlay`, `Pause`, `Stop`, `Next`, `Prev`, `SeekFrames(u64)`, `SetVolume(f32)`, `PlayNext(C::Handle)`, `Enqueue(C::Handle)`, `ClearQueue`, `SetShuffle { on, seed }`, `SetRepeat(Repeat)`, `SetStopAfter(StopAfter)`, `Goto(usize)`); and `Player::apply(&mut self, cmd) -> Result<(), DeckError>`.

- [ ] **Step 1: Write the enum + `apply` + tests**

Create `crates/soul-audio-control/src/command.rs`:

```rust
//! High-level playback intents applied by [`Player::apply`](crate::Player::apply).
//! An app receives these over a channel and applies them between pump blocks.

use soul_audio_queue::{Context, Repeat, StopAfter};

/// A playback intent. `C::Handle` and `C` are the app's context/handle types.
pub enum Command<C: Context> {
    /// Replace the source context and play from `start`.
    PlayContext { ctx: C, start: usize },
    /// Toggle play/pause.
    TogglePlay,
    /// Pause.
    Pause,
    /// Stop and disarm.
    Stop,
    /// Skip to the next track.
    Next,
    /// Previous (with the prev-3s rule).
    Prev,
    /// Seek the current track to a per-channel frame.
    SeekFrames(u64),
    /// Set volume (`0.0..=1.0`).
    SetVolume(f32),
    /// "Play Next".
    PlayNext(C::Handle),
    /// "Add to Queue".
    Enqueue(C::Handle),
    /// Clear the Up-Next queue.
    ClearQueue,
    /// Turn shuffle on (seeded) or off.
    SetShuffle { on: bool, seed: u64 },
    /// Set the repeat mode.
    SetRepeat(Repeat),
    /// Set the one-shot stop request.
    SetStopAfter(StopAfter),
    /// Jump to a source position.
    Goto(usize),
}
```

Add `apply` to the `impl Player` block in `player.rs` (after `set_stop_after`):

```rust
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
```

Add a test to the `tests` module in `player.rs`:

```rust
    use crate::Command;

    #[test]
    fn apply_drives_a_command_sequence() {
        let mut p = player(0);
        p.apply(Command::PlayContext { ctx: SliceContext::new(vec![0u32, 1, 2]), start: 0 })
            .unwrap();
        assert_eq!(p.current(), Some(0));
        p.apply(Command::SetRepeat(Repeat::All)).unwrap();
        assert_eq!(p.repeat(), Repeat::All);
        p.apply(Command::PlayNext(2)).unwrap();
        p.apply(Command::TogglePlay).unwrap(); // was Playing → Paused
        assert_eq!(p.state(), soul_audio_core::DeckState::Paused);
        p.apply(Command::TogglePlay).unwrap(); // → Playing
        assert_eq!(p.state(), soul_audio_core::DeckState::Playing);
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p soul-audio-control`
Expected: all `player::tests` pass.

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-control/src/command.rs crates/soul-audio-control/src/player.rs
git commit -m "feat(control): Command intent enum + Player::apply"
```

---

## Task 6: Wire `soul-audio-control` into the verify gate + full verify

**Files:**
- Modify: `xtask/src/main.rs`

- [ ] **Step 1: Add the thumbv7em no_std build to `verify()`**

In `xtask/src/main.rs`'s `verify()`, after the `soul-audio-queue` thumbv7em build block, add a `soul-audio-control` one:

```rust
    run(&[
        "build",
        "-p",
        "soul-audio-control",
        "--target",
        "thumbv7em-none-eabihf",
        "--no-default-features",
    ])?;
```

- [ ] **Step 2: Run the full gate**

Run: `cargo xtask verify`
Expected: `verify: OK` — fmt, clippy `-D warnings`, all tests (incl. `soul-audio-core::convert`, `soul-audio-queue::slice`, `soul-audio-control`), and the thumbv7em builds of core/engine/queue/**control**/firmware.

- [ ] **Step 3: Commit**

```bash
git add xtask/src/main.rs
git commit -m "chore(xtask): add soul-audio-control to the thumbv7em no_std verify gate"
```

---

## Self-Review

**Spec coverage** (against `2026-08-31-tauri-player-design.md` §2.2): the three
portable `no_std` pieces — core `convert` (Task 1), queue `SliceContext` (Task 2),
and the `soul-audio-control` `Player` + `Command` incl. prev-3s (Tasks 3–5) — are
all covered, plus the verify-gate wiring (Task 6). The desktop lib, Tauri backend,
and Svelte UI are **later plans** (2–4), which consume these APIs.

**Placeholder scan:** every step has complete code; the one conditional note (Task
2's `alloc::vec` vs std `Vec` under `cfg(test)`) names the exact fallback and
forbids changing non-test code — not a placeholder.

**Type consistency:** `SliceContext<H, B: AsRef<[H]>>` `Handle = H` matches its use
in the control test harness (`SliceContext<u32, Vec<u32>>`); `Player`'s
`<K, D, C, Up, Hist, PRELOAD>` bounds mirror `Engine<K, D, TwoLevelQueue<C, Up,
Hist>, PRELOAD>`; every `Player` method delegates to a real `Engine`/`Deck`/
`TwoLevelQueue` method (`play`/`pause`/`stop`/`set_volume`/`seek`/`state`/`next`/
`prev`/`goto`/`rearm`/`sequencer_mut`/`deck`/`poll_open`/`provide`/`pump`;
`position_frames`/`duration_frames`/`sample_rate`; `play_next`/`enqueue`/
`clear_upnext`/`set_shuffle`/`set_repeat`/`set_stop_after`/`is_shuffled`/`repeat`).
`Command`'s variants match `Player::apply`'s arms one-to-one.
