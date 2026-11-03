# Playback Queue — Phase 2: `soul-audio-queue` crate — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `soul-audio-queue` crate — a portable, `no_std` two-level Up-Next playback queue that implements the `Sequencer` trait (merged in Phase 1) and scales to arbitrarily large libraries in O(1) memory.

**Architecture:** A `TwoLevelQueue<C, Up, Hist>` where the source context `C` is *virtual* (positions → handles, never materialized), a manual Up-Next queue and a `prev` history are bounded/materialized behind a `QueueBuf` abstraction (heapless on the DAP, `alloc` on desktop), and shuffle is a stored `u64` seed evaluated through a keyed Feistel permutation (O(1), invertible, no permutation array). It impls `soul_audio_engine::Sequencer`, so it plugs straight into the reconcile `Engine` from Phase 1.

**Tech Stack:** Rust 2024, `no_std`, `forbid(unsafe_code)`, no-alloc core; deps: `soul-audio-engine` (for the `Sequencer` trait), `heapless`; features: `alloc`, `serde`.

## Global Constraints

- `#![cfg_attr(not(any(test, feature = "std")), no_std)]` + `#![forbid(unsafe_code)]`; **no heap allocation** in the default (no-feature) build. `alloc`/`serde`/`std` are opt-in cargo features.
- Edition `2024`, `rust-version` `1.85` (workspace-inherited).
- `Sequencer::Handle` and `Context::Handle` are the same opaque `Copy + Eq` type.
- The crate is **metadata-agnostic**: it never sees paths/tags, only opaque handles and positions.
- `cargo xtask verify` must stay green (fmt, clippy `-D warnings`, all tests, thumbv7em core/engine/firmware builds). The new crate must build `no_std` for `thumbv7em-none-eabihf` with `--no-default-features`.
- Add `soul-audio-queue` to the workspace `members` AND `default-members` in the root `Cargo.toml`.
- Commits: Conventional Commits; **NO `Co-Authored-By: Claude` / `Claude-Session` trailers** (the commit-msg hook rejects them). Because the pre-commit hook runs **workspace-wide** clippy, every commit must leave the whole workspace clippy-clean.
- Run this crate's tests with `cargo test -p soul-audio-queue`. Tests may use `--features alloc`.
- **Test/feature strategy (important):** under `cfg(test)` the crate compiles as `std` (per the `cfg_attr` on `lib.rs`), so test code can use `Vec`/`vec!` from the std prelude directly — write `Vec`, NOT `alloc::vec::Vec`. Keep the CORE unit tests (shuffle, buf-heapless, context, queue, engine-integration) on **`heapless` backings** so they compile and run under plain `cargo test` — which is what `xtask verify` runs, and the `QueueBuf for heapless::Deque` impl is never feature-gated. The alloc-only surface — the `QueueBuf for VecDeque` impl, `Snapshot`/`snapshot`/`restore`, and `balanced_shuffle` — is gated `#[cfg(feature = "alloc")]`, and so are the tests that exercise it; run those with `cargo test -p soul-audio-queue --features alloc`. Task 9's gate runs `--all-features`. Do NOT make a feature-gated impl visible to tests via `#[cfg(any(feature = "alloc", test))]` hacks — use heapless in the core tests instead.

---

## File Structure

- **Create** `crates/soul-audio-queue/Cargo.toml` — package + deps + features.
- **Create** `crates/soul-audio-queue/src/lib.rs` — crate attrs, `mod` decls, re-exports, `Full` error, `Repeat`/`StopAfter` enums, aliases.
- **Create** `crates/soul-audio-queue/src/shuffle.rs` — keyed Feistel permutation `shuffle_index` + `unshuffle_index` (+ property tests).
- **Create** `crates/soul-audio-queue/src/buf.rs` — `QueueBuf` trait + `heapless::Deque` impl + (feature `alloc`) `VecDeque` impl.
- **Create** `crates/soul-audio-queue/src/context.rs` — `Context` trait.
- **Create** `crates/soul-audio-queue/src/queue.rs` — `TwoLevelQueue` (the core) + `impl Sequencer` + mutation API + tests.
- **Create** `crates/soul-audio-queue/src/snapshot.rs` — `Snapshot` + `restore` (+ `serde` feature).
- **Create** `crates/soul-audio-queue/src/balanced.rs` — `balanced_shuffle` utility (feature `alloc`).
- **Modify** `Cargo.toml` (root) — add the crate to `members` + `default-members`.

---

## Task 1: Scaffold the crate + keyed Feistel shuffle

**Files:**
- Create: `crates/soul-audio-queue/Cargo.toml`, `crates/soul-audio-queue/src/lib.rs`, `crates/soul-audio-queue/src/shuffle.rs`
- Modify: `Cargo.toml` (root)

**Interfaces:**
- Produces: `pub fn shuffle_index(seed: u64, i: usize, n: usize) -> usize` and `pub fn unshuffle_index(seed: u64, j: usize, n: usize) -> usize` — a bijection on `[0, n)` and its exact inverse (`unshuffle_index(seed, shuffle_index(seed, i, n), n) == i`).

- [ ] **Step 1: Create the crate manifest + workspace wiring**

Create `crates/soul-audio-queue/Cargo.toml`:

```toml
[package]
name = "soul-audio-queue"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Portable no_std two-level Up-Next playback queue (virtual context, keyed-permutation shuffle, history) implementing soul-audio-engine's Sequencer."

[lints]
workspace = true

[dependencies]
soul-audio-engine = { path = "../soul-audio-engine" }
heapless = "0.8"

[features]
default = []
alloc = []
std = ["alloc"]
serde = ["dep:serde"]

[dependencies.serde]
version = "1"
optional = true
default-features = false
features = ["derive"]
```

In the root `Cargo.toml`, add `"crates/soul-audio-queue"` to BOTH the `[workspace] members` list and the `default-members` list (place it after `"crates/soul-audio-engine"` in each).

- [ ] **Step 2: Write the failing Feistel bijection tests**

Create `crates/soul-audio-queue/src/shuffle.rs`:

```rust
//! A keyed pseudo-random permutation over `[0, n)`, seeded by a `u64`, evaluated
//! positionally with O(1) time and space (no stored permutation array). Used to
//! shuffle a source context of any size. Built from a small Feistel network over
//! a power-of-two domain plus cycle-walking down to `n`.

const ROUNDS: u32 = 4;

/// A strong 64-bit mix (splitmix64 finalizer) of the round key material.
fn mix(seed: u64, round: u32, r: u64) -> u64 {
    let mut z = seed
        .wrapping_add(0x9e37_79b9_7f4a_7c15u64.wrapping_mul(round as u64 + 1))
        .wrapping_add(r.wrapping_mul(0xff51_afd7_ed55_8ccd));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Number of bits to represent `[0, n)`, rounded UP to an even number so the
/// Feistel halves are equal. `n >= 2`. Computed in `u64` (NOT `usize::BITS`) so
/// it is target-independent — on a 32-bit target `usize::BITS` would collapse the
/// domain to a constant and hang the cycle-walk.
fn even_bits(n: usize) -> u32 {
    let need = (64 - (n as u64 - 1).leading_zeros()).max(1);
    if need % 2 == 0 { need } else { need + 1 }
}

/// Feistel encrypt over the `2*half`-bit domain.
fn feistel(seed: u64, half: u32, x: u64) -> u64 {
    let mask = (1u64 << half) - 1;
    let mut l = x >> half;
    let mut r = x & mask;
    for round in 0..ROUNDS {
        let f = mix(seed, round, r) & mask;
        let nl = r;
        let nr = l ^ f;
        l = nl;
        r = nr;
    }
    (l << half) | r
}

/// Feistel decrypt (exact inverse of `feistel`).
fn feistel_inv(seed: u64, half: u32, y: u64) -> u64 {
    let mask = (1u64 << half) - 1;
    let mut l = y >> half;
    let mut r = y & mask;
    for round in (0..ROUNDS).rev() {
        // forward was: nl=r_prev, nr=l_prev ^ f(r_prev). Given (l=nl, r=nr):
        // r_prev = l ; l_prev = r ^ f(round, r_prev)
        let f = mix(seed, round, l) & mask;
        let pl = r ^ f;
        let pr = l;
        l = pl;
        r = pr;
    }
    (l << half) | r
}

/// The shuffled position of `i` under `seed` over `[0, n)`. Bijection on `[0, n)`.
#[must_use]
pub fn shuffle_index(seed: u64, i: usize, n: usize) -> usize {
    if n <= 1 {
        return i;
    }
    let half = even_bits(n) / 2;
    let mut x = i as u64;
    loop {
        x = feistel(seed, half, x);
        if (x as usize) < n {
            return x as usize;
        }
    }
}

/// Inverse of [`shuffle_index`]: `unshuffle_index(s, shuffle_index(s, i, n), n) == i`.
#[must_use]
pub fn unshuffle_index(seed: u64, j: usize, n: usize) -> usize {
    if n <= 1 {
        return j;
    }
    let half = even_bits(n) / 2;
    let mut x = j as u64;
    loop {
        x = feistel_inv(seed, half, x);
        if (x as usize) < n {
            return x as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The crate compiles as std under cfg(test), so `Vec`/`vec!` come from the
    // std prelude — no `extern crate alloc` needed.

    fn assert_bijection(seed: u64, n: usize) {
        let mut seen = vec![false; n];
        for i in 0..n {
            let s = shuffle_index(seed, i, n);
            assert!(s < n, "n={n} seed={seed} i={i} -> {s} out of range");
            assert!(!seen[s], "n={n} seed={seed}: collision at {s}");
            seen[s] = true;
            assert_eq!(unshuffle_index(seed, s, n), i, "inverse mismatch n={n} i={i}");
        }
        assert!(seen.into_iter().all(|b| b), "not a full permutation n={n}");
    }

    #[test]
    fn is_a_bijection_across_sizes_and_seeds() {
        for &n in &[1usize, 2, 3, 4, 7, 8, 9, 16, 17, 31, 100, 256, 1000, 4095] {
            for &seed in &[0u64, 1, 42, 0xdead_beef, u64::MAX] {
                assert_bijection(seed, n);
            }
        }
    }

    #[test]
    fn seed_zero_is_still_a_permutation_not_identity_for_large_n() {
        // Not asserting non-identity strictly, but the round keys depend on the
        // position, so seed 0 still permutes; just re-check bijection at a big n.
        assert_bijection(0, 4096);
    }
}
```

Add to `crates/soul-audio-queue/src/lib.rs` (create it now with the crate attrs):

```rust
#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-queue` — a portable `no_std` two-level Up-Next playback queue that
//! implements [`soul_audio_engine::Sequencer`]. The source context is virtual
//! (positions → handles, O(1) memory at any library size); the Up-Next queue and
//! `prev` history are bounded; shuffle is a keyed permutation over a `u64` seed.

#[cfg(feature = "alloc")]
extern crate alloc;

mod shuffle;

pub use shuffle::{shuffle_index, unshuffle_index};
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p soul-audio-queue shuffle::tests`
Expected: both tests PASS (the permutation is a verified bijection with an exact inverse across all listed sizes/seeds).

- [ ] **Step 4: Verify the crate builds no_std for the DAP**

Run: `cargo build -p soul-audio-queue --target thumbv7em-none-eabihf --no-default-features`
Expected: Finished (no_std clean).

- [ ] **Step 5: Commit**

```bash
git add crates/soul-audio-queue/Cargo.toml crates/soul-audio-queue/src/lib.rs crates/soul-audio-queue/src/shuffle.rs Cargo.toml
git commit -m "feat(queue): scaffold soul-audio-queue crate + keyed Feistel shuffle (bijection + inverse)"
```

---

## Task 2: `QueueBuf` — bounded (heapless) + alloc backings

**Files:**
- Create: `crates/soul-audio-queue/src/buf.rs`
- Modify: `crates/soul-audio-queue/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub struct Full;` (a unit error), and `pub trait QueueBuf<T> { fn len(&self)->usize; fn is_empty(&self)->bool; fn capacity(&self)->Option<usize>; fn push_back(&mut self,v:T)->Result<(),Full>; fn push_front(&mut self,v:T)->Result<(),Full>; fn pop_front(&mut self)->Option<T>; fn pop_back(&mut self)->Option<T>; fn push_back_evicting(&mut self,v:T); fn get(&self,i:usize)->Option<&T>; fn clear(&mut self); }`. Impl for `heapless::Deque<T,N>` and (feature `alloc`) `alloc::collections::VecDeque<T>`.

- [ ] **Step 1: Write the failing tests**

Create `crates/soul-audio-queue/src/buf.rs`:

```rust
//! Storage abstraction for the Up-Next queue and history: bounded `heapless`
//! on the DAP, growable `alloc::VecDeque` on the desktop (feature `alloc`).

/// The bounded buffer was full (only returned by the bounded `heapless` backing).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Full;

/// A double-ended queue the [`TwoLevelQueue`](crate::TwoLevelQueue) uses for its
/// Up-Next list and `prev` history.
pub trait QueueBuf<T> {
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// `None` = unbounded (adds never fail).
    fn capacity(&self) -> Option<usize>;
    /// Append; `Err(Full)` only when bounded and full.
    fn push_back(&mut self, v: T) -> Result<(), Full>;
    /// Prepend; `Err(Full)` only when bounded and full.
    fn push_front(&mut self, v: T) -> Result<(), Full>;
    fn pop_front(&mut self) -> Option<T>;
    fn pop_back(&mut self) -> Option<T>;
    /// Append, dropping the OLDEST (front) element first if full — for a history
    /// ring that never rejects.
    fn push_back_evicting(&mut self, v: T);
    fn get(&self, i: usize) -> Option<&T>;
    fn clear(&mut self);
}

impl<T, const N: usize> QueueBuf<T> for heapless::Deque<T, N> {
    fn len(&self) -> usize {
        heapless::Deque::len(self)
    }
    fn capacity(&self) -> Option<usize> {
        Some(N)
    }
    fn push_back(&mut self, v: T) -> Result<(), Full> {
        heapless::Deque::push_back(self, v).map_err(|_| Full)
    }
    fn push_front(&mut self, v: T) -> Result<(), Full> {
        heapless::Deque::push_front(self, v).map_err(|_| Full)
    }
    fn pop_front(&mut self) -> Option<T> {
        heapless::Deque::pop_front(self)
    }
    fn pop_back(&mut self) -> Option<T> {
        heapless::Deque::pop_back(self)
    }
    fn push_back_evicting(&mut self, v: T) {
        if self.len() == N {
            let _ = heapless::Deque::pop_front(self);
        }
        // Cannot be full now.
        let _ = heapless::Deque::push_back(self, v);
    }
    fn get(&self, i: usize) -> Option<&T> {
        self.iter().nth(i)
    }
    fn clear(&mut self) {
        heapless::Deque::clear(self);
    }
}

#[cfg(feature = "alloc")]
impl<T> QueueBuf<T> for alloc::collections::VecDeque<T> {
    fn len(&self) -> usize {
        alloc::collections::VecDeque::len(self)
    }
    fn capacity(&self) -> Option<usize> {
        None
    }
    fn push_back(&mut self, v: T) -> Result<(), Full> {
        alloc::collections::VecDeque::push_back(self, v);
        Ok(())
    }
    fn push_front(&mut self, v: T) -> Result<(), Full> {
        alloc::collections::VecDeque::push_front(self, v);
        Ok(())
    }
    fn pop_front(&mut self) -> Option<T> {
        alloc::collections::VecDeque::pop_front(self)
    }
    fn pop_back(&mut self) -> Option<T> {
        alloc::collections::VecDeque::pop_back(self)
    }
    fn push_back_evicting(&mut self, v: T) {
        alloc::collections::VecDeque::push_back(self, v);
    }
    fn get(&self, i: usize) -> Option<&T> {
        alloc::collections::VecDeque::get(self, i)
    }
    fn clear(&mut self) {
        alloc::collections::VecDeque::clear(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heapless_push_front_back_and_full() {
        let mut q: heapless::Deque<u32, 2> = heapless::Deque::new();
        assert_eq!(q.capacity(), Some(2));
        assert!(QueueBuf::push_back(&mut q, 1).is_ok());
        assert!(QueueBuf::push_front(&mut q, 0).is_ok()); // [0,1]
        assert_eq!(QueueBuf::push_back(&mut q, 2), Err(Full));
        assert_eq!(QueueBuf::get(&q, 0), Some(&0));
        assert_eq!(QueueBuf::get(&q, 1), Some(&1));
        assert_eq!(QueueBuf::pop_front(&mut q), Some(0));
    }

    #[test]
    fn heapless_evicting_ring_drops_oldest() {
        let mut q: heapless::Deque<u32, 2> = heapless::Deque::new();
        q.push_back_evicting(1);
        q.push_back_evicting(2);
        q.push_back_evicting(3); // drops 1
        assert_eq!(QueueBuf::len(&q), 2);
        assert_eq!(QueueBuf::get(&q, 0), Some(&2));
        assert_eq!(QueueBuf::pop_back(&mut q), Some(3));
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn alloc_is_unbounded() {
        extern crate alloc;
        let mut q: alloc::collections::VecDeque<u32> = alloc::collections::VecDeque::new();
        assert_eq!(QueueBuf::capacity(&q), None);
        for i in 0..1000 {
            assert!(QueueBuf::push_back(&mut q, i).is_ok());
        }
        assert_eq!(QueueBuf::len(&q), 1000);
    }
}
```

Add to `lib.rs`:

```rust
mod buf;
pub use buf::{Full, QueueBuf};
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p soul-audio-queue buf::tests` then `cargo test -p soul-audio-queue --features alloc buf::tests`
Expected: the heapless tests pass with default features; all three pass with `--features alloc`.

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-queue/src/buf.rs crates/soul-audio-queue/src/lib.rs
git commit -m "feat(queue): QueueBuf trait + heapless (bounded) and alloc (unbounded) backings"
```

---

## Task 3: `Context` trait + `Repeat` / `StopAfter` enums

**Files:**
- Create: `crates/soul-audio-queue/src/context.rs`
- Modify: `crates/soul-audio-queue/src/lib.rs`

**Interfaces:**
- Produces: `pub trait Context { type Handle: Copy + Eq; fn len(&self) -> usize; fn is_empty(&self) -> bool { self.len() == 0 } fn handle_at(&self, natural_index: usize) -> Option<Self::Handle>; }`; `pub enum Repeat { Off, All, One }` (Default = Off); `pub enum StopAfter { None, Track, Queue }` (Default = None).

- [ ] **Step 1: Write the code + a doc-verifying test**

Create `crates/soul-audio-queue/src/context.rs`:

```rust
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
mod tests {
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
```

Add to `lib.rs`:

```rust
mod context;
pub use context::Context;

/// Behaviour at the source-tier boundary/end.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Repeat {
    /// Play the source once, then stop.
    #[default]
    Off,
    /// Wrap from the last source track back to the first.
    All,
    /// Loop the current track on a natural end (manual skips move on).
    One,
}

/// A one-shot stop request that reverts after it fires.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum StopAfter {
    /// Keep playing.
    #[default]
    None,
    /// Stop at the next track boundary.
    Track,
    /// Stop when the queue would otherwise continue past the current track's end
    /// with nothing left to play from Up-Next or the source under `Off`.
    Queue,
}
```

(`Repeat`/`StopAfter` are declared in `lib.rs` so the whole crate shares them; `serde` derives are added in Task 7 behind the feature.)

- [ ] **Step 2: Run test**

Run: `cargo test -p soul-audio-queue context::tests`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-queue/src/context.rs crates/soul-audio-queue/src/lib.rs
git commit -m "feat(queue): Context trait (virtual source) + Repeat/StopAfter enums"
```

---

## Task 4: `TwoLevelQueue` — struct + `Sequencer` impl (semantics core)

This is the load-bearing task: the two-level "what plays next" semantics that the Phase-1 `Engine` reconciles to. `Handle`s come from the `Context`. Uses linear order here; shuffle is layered in Task 5 via `natural(pos)`.

**Files:**
- Create: `crates/soul-audio-queue/src/queue.rs`
- Modify: `crates/soul-audio-queue/src/lib.rs`

**Interfaces:**
- Consumes: `Context` (Task 3), `QueueBuf`/`Full` (Task 2), `shuffle_index`/`unshuffle_index` (Task 1), `soul_audio_engine::{Sequencer, Advance}`, `Repeat`/`StopAfter`.
- Produces: `pub struct TwoLevelQueue<C: Context, Up: QueueBuf<C::Handle>, Hist: QueueBuf<(C::Handle, usize)>>` with `impl Sequencer` (`type Handle = C::Handle`), plus the constructor `new(ctx, up, hist)` and `set_source`. The mutation API (Task 6) and shuffle wiring (Task 5) are added later.

- [ ] **Step 1: Write the failing tests + the struct/Sequencer impl**

Create `crates/soul-audio-queue/src/queue.rs`:

```rust
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
    history: Hist,
    repeat: Repeat,
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
            self.history.push_back_evicting((h, self.pos));
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
        self.pos = p;
        self.cur
    }

    fn goto(&mut self, natural_index: usize) -> Option<C::Handle> {
        if natural_index >= self.ctx.len() {
            return None;
        }
        if let Some(h) = self.cur {
            self.history.push_back_evicting((h, self.pos));
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
    type Q<'a> = TwoLevelQueue<
        Slice<'a, u32>,
        heapless::Deque<u32, 8>,
        heapless::Deque<(u32, usize), 16>,
    >;

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
}
```

To make the `queue::tests` compile with the `VecDeque` backing, these tests require `--features alloc`. Add to `lib.rs`:

```rust
mod queue;
pub use queue::TwoLevelQueue;
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p soul-audio-queue --features alloc queue::tests`
Expected: all 8 tests PASS. (They need `--features alloc` for the `VecDeque` backing; the crate's non-test build stays no-alloc.)

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-queue/src/queue.rs crates/soul-audio-queue/src/lib.rs
git commit -m "feat(queue): TwoLevelQueue + Sequencer impl (up-next-wins, repeat, history prev)"
```

---

## Task 5: Shuffle integration (keyed seed + anchored toggle)

**Files:**
- Modify: `crates/soul-audio-queue/src/queue.rs`

**Interfaces:**
- Consumes: everything from Task 4; `shuffle_index`/`unshuffle_index`.
- Produces (added to `TwoLevelQueue`): `pub fn set_shuffle(&mut self, on: bool, seed: u64)`, `pub fn reshuffle(&mut self, seed: u64)`, `pub fn is_shuffled(&self) -> bool`.

- [ ] **Step 1: Write the failing tests + the methods**

Add these methods to the `impl TwoLevelQueue` inherent block in `queue.rs` (after `set_source`):

```rust
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
```

Add these tests to `queue::tests`:

```rust
    #[test]
    fn shuffle_toggle_keeps_current_track() {
        let mut s = q(&[10, 20, 30, 40, 50]);
        s.advance(Advance::Natural); // cur=20 at natural index 1
        let before = s.current();
        s.set_shuffle(true, 0xabcdef);
        assert_eq!(s.current(), before, "current unchanged when enabling shuffle");
        assert_eq!(s.natural(s.pos), 1, "still anchored to natural index 1");
        s.set_shuffle(false, 0);
        assert_eq!(s.current(), before, "current unchanged when disabling shuffle");
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
        assert_eq!(seen.len(), 4, "a full cycle visits every track exactly once");
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p soul-audio-queue --features alloc queue::tests`
Expected: all Task 4 + Task 5 tests PASS (10 total).

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-queue/src/queue.rs
git commit -m "feat(queue): keyed shuffle with anchored toggle + full-cycle coverage"
```

---

## Task 6: Mutation API — Play Next / Add to Queue / repeat / stop-after / queries

**Files:**
- Modify: `crates/soul-audio-queue/src/queue.rs`

**Interfaces:**
- Produces (added to `TwoLevelQueue`): `pub fn play_next(&mut self, h) -> Result<(), Full>`, `pub fn enqueue(&mut self, h) -> Result<(), Full>`, `pub fn clear_upnext(&mut self)`, `pub fn upnext_len(&self) -> usize`, `pub fn upnext_iter(&self) -> impl Iterator<Item = C::Handle> + '_`, `pub fn set_repeat(&mut self, Repeat)`, `pub fn repeat(&self) -> Repeat`, `pub fn set_stop_after(&mut self, StopAfter)`, `pub fn stop_after(&self) -> StopAfter`, `pub fn source_len(&self) -> usize`, `pub fn pos(&self) -> usize`, `pub fn context(&self) -> &C`.

- [ ] **Step 1: Write the failing tests + the methods**

Add to the inherent `impl` block in `queue.rs`:

```rust
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
```

Add to `queue::tests`:

```rust
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
```

Note on `set_source` and lifetimes: the test's `Slice` borrows; if the borrow checker rejects reusing `s` with a new `Slice` of a different borrow, bind the second slice to a `let` first (`let s2 = [1u32, 2]; s.set_source(Slice(&s2), 0);`). Adjust as the compiler requires without changing behavior.

- [ ] **Step 2: Run tests**

Run: `cargo test -p soul-audio-queue --features alloc queue::tests`
Expected: all Task 4–6 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-queue/src/queue.rs
git commit -m "feat(queue): mutation API (play_next/enqueue/clear, repeat, stop_after, queries)"
```

---

## Task 7: `Snapshot` / restore (+ `serde` feature)

**Files:**
- Create: `crates/soul-audio-queue/src/snapshot.rs`
- Modify: `crates/soul-audio-queue/src/queue.rs`, `crates/soul-audio-queue/src/lib.rs`

**Interfaces:**
- Produces: `pub struct Snapshot<H> { pub context_id: u64, pub pos: usize, pub seed: Option<u64>, pub repeat: Repeat, pub stop_after: StopAfter, pub upnext: heapless::Vec<H, UPCAP> ... }` — but to stay backing-agnostic, model the persistable lists as slices/iterators. Concretely: `Snapshot<H>` holds `context_id`, `pos`, `seed`, `repeat`, `stop_after`, and the Up-Next + history contents; `TwoLevelQueue::snapshot(&self, context_id) -> Snapshot<H>` (feature `alloc`, collects into `Vec`) and `TwoLevelQueue::restore(ctx, context_id, snap, up, hist) -> Result<Self, RestoreError>`.

- [ ] **Step 1: Write the code + tests**

Create `crates/soul-audio-queue/src/snapshot.rs`:

```rust
//! Serializable queue state for persistence. The `Context` itself is app-owned
//! (identified by an opaque `context_id`); only positions/handles/flags persist.

use crate::{Repeat, StopAfter};

/// Why a [`restore`](crate::TwoLevelQueue::restore) failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RestoreError {
    /// `pos` was out of range for the restored context.
    PosOutOfRange,
    /// The Up-Next or history contents exceeded the backing's capacity.
    CapacityExceeded,
}

/// A plain-data snapshot of a queue's state. `H` is the handle type.
#[cfg(feature = "alloc")]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Snapshot<H> {
    pub context_id: u64,
    pub pos: usize,
    pub seed: Option<u64>,
    pub repeat: Repeat,
    pub stop_after: StopAfter,
    pub upnext: alloc::vec::Vec<H>,
    pub history: alloc::vec::Vec<(H, usize)>,
}
```

Add `snapshot`/`restore` to the inherent `impl` in `queue.rs` (feature `alloc`):

```rust
    /// Capture the persistable state. `context_id` identifies the app's context.
    #[cfg(feature = "alloc")]
    pub fn snapshot(&self, context_id: u64) -> crate::snapshot::Snapshot<C::Handle> {
        crate::snapshot::Snapshot {
            context_id,
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
    /// contents. Validates `pos` and capacities.
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
            up.push_back(h).map_err(|_| RestoreError::CapacityExceeded)?;
        }
        for &(h, p) in &snap.history {
            hist.push_back(h_and_pos(h, p))
                .map_err(|_| RestoreError::CapacityExceeded)?;
        }
        let cur = if len == 0 { None } else { ctx.handle_at(match snap.seed {
            Some(s) => shuffle_index(s, snap.pos, len),
            None => snap.pos,
        }) };
        Ok(Self {
            ctx,
            cur,
            pos: snap.pos.min(len.saturating_sub(1)),
            seed: snap.seed,
            upnext: up,
            history: hist,
            repeat: snap.repeat,
            stop_after: snap.stop_after,
        })
    }
```

Add a tiny free helper at the bottom of `queue.rs` to keep `restore` readable (or inline the tuple):

```rust
#[cfg(feature = "alloc")]
#[inline]
fn h_and_pos<H>(h: H, p: usize) -> (H, usize) {
    (h, p)
}
```

(If clippy flags `h_and_pos` as needless, inline the `(h, p)` tuple in `restore` and delete the helper.)

Add to `lib.rs`:

```rust
mod snapshot;
#[cfg(feature = "alloc")]
pub use snapshot::Snapshot;
pub use snapshot::RestoreError;
```

Add `#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]` to the `Repeat` and `StopAfter` enum definitions in `lib.rs` (so `Snapshot` can derive serde).

Add a test to `queue::tests`:

```rust
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
        let r: Q = TwoLevelQueue::restore(
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
    }
```

- [ ] **Step 2: Run tests (alloc, and alloc+serde)**

Run: `cargo test -p soul-audio-queue --features alloc queue::tests::snapshot_restore_round_trips`
Then: `cargo build -p soul-audio-queue --features "alloc serde"` (confirms the serde derives compile).
Expected: the test passes; the `alloc serde` build succeeds.

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-queue/src/snapshot.rs crates/soul-audio-queue/src/queue.rs crates/soul-audio-queue/src/lib.rs
git commit -m "feat(queue): serializable Snapshot + restore (serde feature)"
```

---

## Task 8: `balanced_shuffle` utility (anti-clustering, feature `alloc`)

**Files:**
- Create: `crates/soul-audio-queue/src/balanced.rs`
- Modify: `crates/soul-audio-queue/src/lib.rs`

**Interfaces:**
- Produces (feature `alloc`): `pub fn balanced_shuffle<F: Fn(usize) -> u64>(n: usize, seed: u64, group_of: F) -> alloc::vec::Vec<usize>` — returns a permutation of `0..n` that spreads same-group items apart (Fiedler/Ruuda-style), for the app to feed as a bounded context's natural order.

- [ ] **Step 1: Write the code + tests**

Create `crates/soul-audio-queue/src/balanced.rs`:

```rust
//! `balanced_shuffle` — an anti-clustering shuffle for BOUNDED contexts, where
//! the app supplies each item's group id (e.g. artist). Spreads same-group items
//! as evenly as possible while staying random-feeling. For library-scale
//! "shuffle everything", use the queue's keyed permutation instead.

extern crate alloc;
use alloc::vec::Vec;

use crate::shuffle::shuffle_index;

/// A permutation of `0..n` spreading items with equal `group_of(i)` apart.
///
/// Method: bucket indices by group; shuffle within each bucket (keyed by `seed`);
/// then merge buckets by placing each bucket's items at evenly-spaced fractional
/// positions in `[0, 1)` (offset per bucket) and sorting — a dithering spread.
#[must_use]
pub fn balanced_shuffle<F: Fn(usize) -> u64>(n: usize, seed: u64, group_of: F) -> Vec<usize> {
    if n <= 1 {
        return (0..n).collect();
    }
    // 1. Bucket by group id, preserving a stable first-seen group order.
    let mut group_ids: Vec<u64> = Vec::new();
    let mut buckets: Vec<Vec<usize>> = Vec::new();
    for i in 0..n {
        let g = group_of(i);
        match group_ids.iter().position(|&x| x == g) {
            Some(b) => buckets[b].push(i),
            None => {
                group_ids.push(g);
                buckets.push(alloc::vec![i]);
            }
        }
    }
    // 2. Shuffle within each bucket (keyed), so same-group order isn't stable.
    for (b, bucket) in buckets.iter_mut().enumerate() {
        let m = bucket.len();
        let key = seed ^ ((b as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let shuffled: Vec<usize> = (0..m).map(|k| bucket[shuffle_index(key, k, m)]).collect();
        *bucket = shuffled;
    }
    // 3. Assign each item a fractional position in [0, 1): item k of a bucket of
    //    size m sits at (k + offset) / m, with a per-bucket offset in [0, 1) so
    //    buckets don't all pin to 0. Sort by fractional position.
    let mut placed: Vec<(u64, usize)> = Vec::with_capacity(n); // (fixed-point pos, item)
    const SCALE: u64 = 1 << 40;
    for (b, bucket) in buckets.iter().enumerate() {
        let m = bucket.len() as u64;
        // deterministic per-bucket offset in [0, m) from the key
        let off = shuffle_index(seed ^ (b as u64).wrapping_add(1), 0, m.max(1) as usize) as u64;
        for (k, &item) in bucket.iter().enumerate() {
            // position = (k + off/m) / m  == (k*m + off) / (m*m), scaled
            let num = (k as u64) * m + off;
            let pos = num.wrapping_mul(SCALE) / (m * m).max(1);
            placed.push((pos, item));
        }
    }
    placed.sort_by_key(|&(p, _)| p);
    placed.into_iter().map(|(_, item)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_a_permutation() {
        let order = balanced_shuffle(20, 123, |i| (i % 4) as u64);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn spreads_same_group_better_than_grouped_input() {
        // 3 groups of 6, interleaved output should have few same-group adjacencies.
        let n = 18;
        let order = balanced_shuffle(n, 7, |i| (i / 6) as u64); // groups: 0..6, 6..12, 12..18
        let group = |idx: usize| idx / 6;
        let adj = order
            .windows(2)
            .filter(|w| group(w[0]) == group(w[1]))
            .count();
        // Grouped input would have 15 same-group adjacencies; a good spread has few.
        assert!(adj <= 4, "too many same-group adjacencies: {adj}");
    }

    #[test]
    fn trivial_sizes() {
        assert_eq!(balanced_shuffle(0, 1, |_| 0), Vec::<usize>::new());
        assert_eq!(balanced_shuffle(1, 1, |_| 0), alloc::vec![0]);
    }
}
```

Add to `lib.rs`:

```rust
#[cfg(feature = "alloc")]
mod balanced;
#[cfg(feature = "alloc")]
pub use balanced::balanced_shuffle;
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p soul-audio-queue --features alloc balanced::tests`
Expected: all three pass (if `spreads_same_group_better_than_grouped_input` is borderline for a given seed, the implementer may adjust the `<= 4` bound to a value that is still clearly better than the grouped-input baseline of 15, and note it — but must NOT make it vacuous).

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-queue/src/balanced.rs crates/soul-audio-queue/src/lib.rs
git commit -m "feat(queue): balanced_shuffle utility (anti-clustering spread for bounded contexts)"
```

---

## Task 9: Aliases + engine integration test + full verify

**Files:**
- Modify: `crates/soul-audio-queue/src/lib.rs`
- Create: `crates/soul-audio-queue/tests/engine_integration.rs`

**Interfaces:**
- Produces: `pub type HeaplessQueue<C, const UP: usize, const HIST: usize> = TwoLevelQueue<C, heapless::Deque<<C as Context>::Handle, UP>, heapless::Deque<(<C as Context>::Handle, usize), HIST>>;` and (feature `alloc`) `pub type AllocQueue<C> = TwoLevelQueue<C, alloc::collections::VecDeque<<C as Context>::Handle>, alloc::collections::VecDeque<(<C as Context>::Handle, usize)>>;`.

- [ ] **Step 1: Add the aliases**

Add to `lib.rs`:

```rust
/// A bounded (`no_std`, no-alloc) queue for the DAP: `UP` Up-Next slots, `HIST`
/// history slots.
pub type HeaplessQueue<C, const UP: usize, const HIST: usize> = TwoLevelQueue<
    C,
    heapless::Deque<<C as Context>::Handle, UP>,
    heapless::Deque<(<C as Context>::Handle, usize), HIST>,
>;

/// An unbounded queue for the desktop (feature `alloc`).
#[cfg(feature = "alloc")]
pub type AllocQueue<C> = TwoLevelQueue<
    C,
    alloc::collections::VecDeque<<C as Context>::Handle>,
    alloc::collections::VecDeque<(<C as Context>::Handle, usize)>,
>;
```

- [ ] **Step 2: Write the engine-integration test (the whole point of Phase 2)**

Create `crates/soul-audio-queue/tests/engine_integration.rs` — drive the Phase-1 `Engine` with a `TwoLevelQueue` (real reconcile), using a mock `Sink` + mock `SeekableDecoder` (no real audio needed), proving Play Next re-primes gaplessly and repeat/shuffle work through the reconcile:

```rust
//! Integration: the Phase-1 reconcile `Engine` driven by a `TwoLevelQueue`.
//! Uses mock sink/decoders (behavior, not audio) to prove the queue policy and
//! the engine reconcile compose — especially that "Play Next" re-primes gaplessly.

use soul_audio_core::{
    DecodeError, Decoder, Deck, Progress, Quality, SampleFormat, SeekableDecoder, Sink, SinkError,
    StreamInfo,
};
use soul_audio_engine::{Engine, Provided};
use soul_audio_queue::{Context, HeaplessQueue};

const RATE: u32 = 48_000;
const FRAMES: u64 = 400;

struct ConstDecoder { value: i32, frames: u64, pos: u64 }
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
    fn seek_to(&mut self, s: u64) -> Result<u64, DecodeError> { self.pos = s.min(self.frames); Ok(self.pos) }
    fn total_frames(&self) -> Option<u64> { Some(self.frames) }
}

struct MockSink { written: Vec<i32> }
impl Sink for MockSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo { sample_rate: RATE, channels: 1, format: SampleFormat::Int { bits: 24 } }
    }
    fn write(&mut self, f: &[i32]) -> Result<(), SinkError> { self.written.extend_from_slice(f); Ok(()) }
    fn flush(&mut self) -> Result<(), SinkError> { Ok(()) }
}

/// The library: handle == track id; the decoder plays a constant = (id+1)*100.
struct Library { ids: Vec<u32> }
impl Context for Library {
    type Handle = u32;
    fn len(&self) -> usize { self.ids.len() }
    fn handle_at(&self, i: usize) -> Option<u32> { self.ids.get(i).copied() }
}
fn decoder_for(id: u32) -> ConstDecoder { ConstDecoder { value: (id as i32 + 1) * 100, frames: FRAMES, pos: 0 } }
fn sample_of(id: u32) -> i32 { ((id as i32 + 1) * 100) << 8 }

type Q = HeaplessQueue<Library, 4, 8>;
type E = Engine<MockSink, ConstDecoder, Q, 1>;

fn engine(ids: &[u32]) -> E {
    let ctx = Library { ids: ids.to_vec() };
    let queue: Q = HeaplessQueue::new(ctx, heapless::Deque::new(), heapless::Deque::new());
    let mut e = Engine::new(Deck::new(MockSink { written: Vec::new() }, Quality::Fast), queue);
    e.rearm();
    e
}
fn service(e: &mut E) {
    while let Some(id) = e.poll_open() {
        e.provide(id, decoder_for(id)).unwrap();
    }
}

#[test]
fn plays_library_in_order() {
    let mut e = engine(&[0, 1, 2]);
    let mut swaps = 0;
    for _ in 0..100_000 {
        service(&mut e);
        match e.pump().unwrap() {
            Progress::TrackChanged => swaps += 1,
            Progress::Ended => break,
            _ => {}
        }
    }
    assert_eq!(swaps, 2);
    let w = &e.deck().sink().written;
    assert_eq!(w[10], sample_of(0));
    assert_eq!(w[FRAMES as usize + 10], sample_of(1));
    assert_eq!(w[FRAMES as usize * 2 + 10], sample_of(2));
}

#[test]
fn play_next_reprimes_gaplessly_through_the_engine() {
    let mut e = engine(&[0, 1, 2]);
    service(&mut e); // current 0, preload 1
    e.pump().unwrap();
    // Play Next track 2: upcoming(1) changes 1 -> 2; the engine must discard the
    // stale preload and re-prime 2, then swap to 2 (not 1) at the boundary.
    e.sequencer_mut().play_next(2).unwrap();
    let mut swapped_to = None;
    for _ in 0..100_000 {
        service(&mut e);
        if let Progress::TrackChanged = e.pump().unwrap() {
            swapped_to = e.current();
            break;
        }
    }
    assert_eq!(swapped_to, Some(2), "Play Next winner played next, gaplessly");
}

#[test]
fn repeat_all_cycles_through_the_engine() {
    let mut e = engine(&[0, 1]);
    e.sequencer_mut().set_repeat(soul_audio_queue::Repeat::All);
    let mut cursors = Vec::new();
    for _ in 0..100_000 {
        service(&mut e);
        if let Progress::TrackChanged = e.pump().unwrap() {
            cursors.push(e.current().unwrap());
            if cursors.len() == 4 { break; }
        }
    }
    assert_eq!(cursors, [1, 0, 1, 0]);
}
```

- [ ] **Step 3: Run the integration test + full crate suite**

Run: `cargo test -p soul-audio-queue --features alloc`
Expected: all unit tests + the 3 integration tests pass. (`--features alloc` is needed because the integration test's `Vec` usage in the mock and `AllocQueue`-free path still compiles under alloc; the `HeaplessQueue` path itself is no-alloc.)

- [ ] **Step 4: Full workspace gate**

Run: `cargo xtask verify`
Expected: `verify: OK` — fmt, clippy `-D warnings`, all tests, and the thumbv7em `soul-audio-core`/`soul-audio-engine`/`soul-audio-firmware` builds. Then additionally confirm the new crate builds no_std for the DAP:
Run: `cargo build -p soul-audio-queue --target thumbv7em-none-eabihf --no-default-features`
Expected: Finished.

- [ ] **Step 5: Commit**

```bash
git add crates/soul-audio-queue/src/lib.rs crates/soul-audio-queue/tests/engine_integration.rs
git commit -m "feat(queue): Heapless/Alloc aliases + engine integration tests (Play Next gapless, repeat)"
```

---

## Self-Review

**Spec coverage** (against `2026-08-30-playback-queue-design.md` §5–§13):
- §5 virtual `Context` + `TwoLevelQueue` fields → Tasks 3, 4.
- §6 semantics (up-next wins, Repeat::One freeze, shuffle/repeat on source tier, `StopAfter`) → Tasks 4, 6. *(Note: `StopAfter` is stored + queryable here; the driver acts on it — the queue exposes it via `stop_after()`, consistent with the "control-layer" split. If the spec intends the queue to auto-stop, that is a driver concern, flagged.)*
- §7 keyed-permutation shuffle + `balanced_shuffle` → Tasks 1, 5, 8.
- §8 `QueueBuf` bounded/alloc + `Context` obligations + `Full` → Tasks 2, 3.
- §9 `Snapshot`/`restore` (+ serde) → Task 7.
- §10 public API surface + aliases → Tasks 4–9.
- §11 gapless look-ahead = `upcoming(k)` → proven by Task 9's `play_next_reprimes_gaplessly_through_the_engine`.
- §12 tests incl. Feistel bijection property tests → Task 1; reconcile-through-engine → Task 9.
- §13 scope: v1 items covered; `remove_upnext`/`move_upnext`, endless contexts, numeric priority, windowed smart-shuffle are correctly **omitted** (v2).

**Placeholder scan:** every code step has complete code; the two "adjust if the compiler/clippy requires" notes (the `set_source` borrow in Task 6, the `h_and_pos` helper in Task 7, the adjustable `<= 4` bound in Task 8) name the exact adjustment and forbid vacuous weakening — not placeholders.

**Type consistency:** `Context::Handle` == `Sequencer::Handle` == the queue's handle throughout; `QueueBuf<C::Handle>` / `QueueBuf<(C::Handle, usize)>` bounds match the struct fields and aliases; `shuffle_index`/`unshuffle_index` signatures match their uses; `natural`/`play_pos` are exact inverses via Task 1's tested bijection.

**Known coupling to Phase 1:** the integration test (Task 9) exercises the merged `Engine` reconcile with a real `TwoLevelQueue`; the "Play Next re-primes gaplessly" case is the concrete payoff of Phase 1's reconcile design.
