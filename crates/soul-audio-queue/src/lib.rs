#![cfg_attr(not(any(test, feature = "std")), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-queue` — a portable `no_std` two-level Up-Next playback queue that
//! implements [`soul_audio_engine::Sequencer`]. The source context is virtual
//! (positions → handles, O(1) memory at any library size); the Up-Next queue and
//! `prev` history are bounded; shuffle is a keyed permutation over a `u64` seed.

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "alloc")]
mod balanced;
mod buf;
mod context;
mod queue;
mod shuffle;
mod snapshot;

#[cfg(feature = "alloc")]
pub use balanced::balanced_shuffle;
pub use buf::{Full, QueueBuf};
pub use context::Context;
pub use queue::TwoLevelQueue;
pub use shuffle::{shuffle_index, unshuffle_index};
pub use snapshot::RestoreError;
#[cfg(feature = "alloc")]
pub use snapshot::Snapshot;

/// Behaviour at the source-tier boundary/end.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
///
/// The queue only STORES this request and exposes it via
/// [`stop_after`](TwoLevelQueue::stop_after); ACTING on it — stopping playback
/// at the boundary — is the driver/engine's responsibility, not the queue's.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
