//! High-level playback intents applied by [`Player::apply`](crate::Player::apply).
//! An app receives these over a channel and applies them between pump blocks.

use soul_audio_queue::{Context, Repeat, StopAfter};

/// A playback intent. `C::Handle` and `C` are the app's context/handle types.
pub enum Command<C: Context> {
    /// Replace the source context and play from `start`.
    PlayContext { ctx: C, start: usize },
    /// Toggle play/pause.
    TogglePlay,
    /// Resume playback (distinct from TogglePlay — never pauses).
    Play,
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
    /// Remove the manually queued track at `index` from the Up-Next queue (the
    /// per-row "remove from queue" / ✕ affordance).
    RemoveFromQueue(usize),
    /// Reorder the Up-Next queue: move the item at `from` to `to` (drag-reorder).
    MoveInQueue { from: usize, to: usize },
    /// Play the upcoming row at 1-based offset `k` now — "click an Up-Next row to
    /// skip straight to it" — consuming the rows above it.
    PlayUpcoming(usize),
    /// Turn shuffle on (seeded) or off.
    SetShuffle { on: bool, seed: u64 },
    /// Set the repeat mode.
    SetRepeat(Repeat),
    /// Set the one-shot stop request.
    SetStopAfter(StopAfter),
    /// Jump to a source position.
    Goto(usize),
}
