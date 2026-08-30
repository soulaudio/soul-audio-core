//! Serializable queue state for persistence. The `Context` itself is app-owned
//! (identified by an opaque `context_id`); only positions/handles/flags persist.

#[cfg(feature = "alloc")]
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
    /// The actual current track (`None` = nothing playing). Persisted directly
    /// rather than re-derived from `pos`/`seed`: whenever the Up-Next queue last
    /// supplied the current track, `pos` (the source anchor) stays put and no
    /// longer identifies `cur`.
    pub cur: Option<H>,
    pub pos: usize,
    pub seed: Option<u64>,
    pub repeat: Repeat,
    pub stop_after: StopAfter,
    pub upnext: alloc::vec::Vec<H>,
    /// `prev` history: `(handle, natural source index)`. The natural index is
    /// seed-invariant, so `prev` is correct even after a restore under a
    /// different shuffle seed than the one active when the entry was pushed.
    pub history: alloc::vec::Vec<(H, usize)>,
}
