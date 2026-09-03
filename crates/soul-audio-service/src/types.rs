//! App-facing command, event, and DTO types crossing the UI↔backend boundary.
//! Concrete (u32 track ids, seconds) and serde-serializable for Plan 4's IPC —
//! distinct from `soul_audio_control::Command<C>` (generic, frames).

use serde::{Deserialize, Serialize};
use soul_audio_core::DeckState;
use soul_audio_queue::Repeat;

/// A command from the UI to the player actor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum Command {
    /// Play an ordered list of track ids, starting at `start`.
    PlayContext {
        ids: Vec<u32>,
        /// Starting index into `ids` (u32: track counts fit; avoids specta's
        /// BigInt restriction on `usize` for the TS wire type).
        start: u32,
    },
    TogglePlay,
    Play,
    Pause,
    Stop,
    Next,
    Prev,
    /// Seek within the current track, in seconds from its start.
    Seek {
        secs: f64,
    },
    /// Set output volume, 0.0..=1.0.
    SetVolume(f32),
    PlayNext(u32),
    Enqueue(u32),
    ClearQueue,
    SetShuffle(bool),
    SetRepeat(RepeatMode),
    /// Jump to a natural position within the current context.
    Goto(u32),
}

/// Repeat mode as seen by the UI (mirrors `soul_audio_queue::Repeat`, decoupled
/// from that crate's `serde` feature).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl From<RepeatMode> for Repeat {
    fn from(m: RepeatMode) -> Self {
        match m {
            RepeatMode::Off => Repeat::Off,
            RepeatMode::All => Repeat::All,
            RepeatMode::One => Repeat::One,
        }
    }
}
impl From<Repeat> for RepeatMode {
    fn from(r: Repeat) -> Self {
        match r {
            Repeat::Off => RepeatMode::Off,
            Repeat::All => RepeatMode::All,
            Repeat::One => RepeatMode::One,
        }
    }
}

/// Playback state as seen by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum PlayState {
    Playing,
    Paused,
    Stopped,
}

impl From<DeckState> for PlayState {
    fn from(s: DeckState) -> Self {
        match s {
            DeckState::Playing => PlayState::Playing,
            DeckState::Paused => PlayState::Paused,
            DeckState::Stopped | DeckState::Ended => PlayState::Stopped,
        }
    }
}

/// An event from the player actor to the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub enum Event {
    Tick(Tick),
    Queue(QueueState),
}

/// Now-playing + transport snapshot, emitted ~8×/s and on any change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Tick {
    pub now_playing_id: Option<u32>,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    pub state: PlayState,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub volume: f32,
}

/// Up-Next + context position, emitted on queue mutation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct QueueState {
    pub up_next_ids: Vec<u32>,
    /// u32: positions fit; avoids specta's BigInt restriction on `usize`.
    pub context_pos: u32,
    pub context_len: u32,
}

/// A library track as sent to the UI (no path, no cover bytes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct TrackView {
    pub id: u32,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_secs: f64,
    /// Whether the file carries embedded cover art. The UI uses this to
    /// decide whether to request the art bytes (a separate command).
    pub has_art: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_mode_roundtrips_through_queue_repeat() {
        for m in [RepeatMode::Off, RepeatMode::All, RepeatMode::One] {
            assert_eq!(RepeatMode::from(Repeat::from(m)), m);
        }
    }

    #[test]
    fn command_serde_roundtrips() {
        let cmd = Command::PlayContext {
            ids: vec![3, 1, 2],
            start: 1,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
    }

    #[test]
    fn deck_state_maps_ended_to_stopped() {
        assert_eq!(PlayState::from(DeckState::Ended), PlayState::Stopped);
        assert_eq!(PlayState::from(DeckState::Playing), PlayState::Playing);
    }
}
