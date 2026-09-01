//! The concrete desktop `Player` specialization plus the pure functions that
//! translate an app [`Command`] into `Player` calls and project the `Player`'s
//! state into UI [`Tick`]/[`QueueState`] events. Generic over the sink and
//! decoder so the whole thing is unit-tested headlessly (NullSink + a const
//! decoder) exactly as it runs with a real device.

use std::collections::VecDeque;

use soul_audio_control::Player;
use soul_audio_core::{DeckError, SeekableDecoder, Sink};
use soul_audio_queue::{SliceContext, TwoLevelQueue};

use crate::types::{Command, PlayState, QueueState, Tick};

/// The desktop player: unbounded (`VecDeque`) Up-Next/history, `SliceContext`
/// over `u32` track ids, `PRELOAD = 2`. Generic over the sink `K` and decoder
/// `D` (production: `CpalSink`/`OwnedDecoder`; tests: `NullSink`/`ConstDecoder`).
pub type ServicePlayer<K, D> =
    Player<K, D, SliceContext<u32, Vec<u32>>, VecDeque<u32>, VecDeque<(u32, usize)>, 2>;

/// The concrete desktop queue: `SliceContext` over `u32` track ids, unbounded
/// (`VecDeque`) Up-Next/history. Factored out of `queue_for`'s signature per
/// `clippy::type_complexity`.
type ServiceQueue =
    TwoLevelQueue<SliceContext<u32, Vec<u32>>, VecDeque<u32>, VecDeque<(u32, usize)>>;

/// Build the concrete desktop queue over a set of track ids (empty at start).
pub fn queue_for(ids: Vec<u32>) -> ServiceQueue {
    TwoLevelQueue::new(SliceContext::new(ids), VecDeque::new(), VecDeque::new())
}

/// Apply one UI command to the player. `seed` is used only by `SetShuffle`
/// (the caller supplies a fresh value each time). Transport errors propagate;
/// bounded-queue-full is impossible here (unbounded backings) but `play_next`/
/// `enqueue` still swallow it for uniformity.
pub fn apply_command<K: Sink, D: SeekableDecoder>(
    player: &mut ServicePlayer<K, D>,
    cmd: Command,
    seed: u64,
) -> Result<(), DeckError> {
    match cmd {
        Command::PlayContext { ids, start } => {
            player.set_source(SliceContext::new(ids), start);
            Ok(())
        }
        Command::TogglePlay => {
            player.toggle_play();
            Ok(())
        }
        Command::Play => {
            player.play();
            Ok(())
        }
        Command::Pause => {
            player.pause();
            Ok(())
        }
        Command::Stop => {
            player.stop();
            Ok(())
        }
        Command::Next => player.next(),
        Command::Prev => player.prev(),
        Command::Seek { secs } => {
            let frame = (secs.max(0.0) * f64::from(player.sample_rate())) as u64;
            player.seek_frames(frame).map(|_| ())
        }
        Command::SetVolume(v) => {
            player.set_volume(v);
            Ok(())
        }
        Command::PlayNext(id) => {
            let _ = player.play_next(id);
            Ok(())
        }
        Command::Enqueue(id) => {
            let _ = player.enqueue(id);
            Ok(())
        }
        Command::ClearQueue => {
            player.clear_queue();
            Ok(())
        }
        Command::SetShuffle(on) => {
            player.set_shuffle(on, seed);
            Ok(())
        }
        Command::SetRepeat(mode) => {
            player.set_repeat(mode.into());
            Ok(())
        }
        Command::Goto(i) => player.goto(i),
    }
}

fn frames_to_secs(frames: u64, rate: u32) -> f64 {
    if rate == 0 {
        0.0
    } else {
        frames as f64 / f64::from(rate)
    }
}

/// Project the now-playing/transport snapshot. `volume` is tracked by the caller
/// (the `Player` has no volume getter).
pub fn project_tick<K: Sink, D: SeekableDecoder>(
    player: &ServicePlayer<K, D>,
    volume: f32,
) -> Tick {
    let rate = player.sample_rate();
    Tick {
        now_playing_id: player.current(),
        position_secs: frames_to_secs(player.position_frames(), rate),
        duration_secs: player.duration_frames().map(|f| frames_to_secs(f, rate)),
        state: PlayState::from(player.state()),
        shuffle: player.is_shuffled(),
        repeat: player.repeat().into(),
        volume,
    }
}

/// Project the Up-Next + context-position snapshot.
pub fn project_queue<K: Sink, D: SeekableDecoder>(player: &ServicePlayer<K, D>) -> QueueState {
    let q = player.sequencer();
    QueueState {
        up_next_ids: q.upnext_iter().collect(),
        context_pos: q.pos(),
        context_len: q.source_len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{ConstDecoder, NullSink};
    use crate::types::RepeatMode;
    use soul_audio_control::Player;
    use soul_audio_core::{Deck, Quality};
    use soul_audio_engine::Engine;

    const RATE: u32 = 48_000;
    const FRAMES: u64 = 4_000; // >> deck block, so a track spans several pumps

    /// A player over NullSink/ConstDecoder with a 1-second prev threshold.
    fn player() -> ServicePlayer<NullSink, ConstDecoder> {
        player_with_threshold(u64::from(RATE))
    }

    /// A player over NullSink/ConstDecoder with a custom prev threshold, so a
    /// short ConstDecoder track can deterministically outrun (or stay under)
    /// it — the 1 s default is unreachable on a 4000-frame track.
    fn player_with_threshold(prev_restart_frames: u64) -> ServicePlayer<NullSink, ConstDecoder> {
        let deck = Deck::new(NullSink::new(RATE, 2), Quality::Hq);
        let engine = Engine::new(deck, queue_for(Vec::new()));
        Player::new(engine, prev_restart_frames)
    }

    /// Run the open/pump loop `cycles` times, feeding a ConstDecoder for each
    /// track the player asks to open (id used as the constant sample so tracks
    /// are distinguishable). Deterministic — no threads, no device.
    fn drive(p: &mut ServicePlayer<NullSink, ConstDecoder>, cycles: usize) {
        for _ in 0..cycles {
            while let Some(id) = p.poll_open() {
                let dec = ConstDecoder::new(RATE, 2, FRAMES, id as i32 + 1);
                p.provide(id, dec).unwrap();
            }
            let _ = p.pump().unwrap();
        }
    }

    #[test]
    fn play_context_makes_the_start_track_current() {
        let mut p = player();
        apply_command(
            &mut p,
            Command::PlayContext {
                ids: vec![0, 1, 2],
                start: 0,
            },
            0,
        )
        .unwrap();
        drive(&mut p, 3);
        assert_eq!(project_tick(&p, 1.0).now_playing_id, Some(0));
        assert_eq!(project_tick(&p, 1.0).state, PlayState::Playing);
    }

    #[test]
    fn pause_and_toggle_reflect_in_state() {
        let mut p = player();
        apply_command(
            &mut p,
            Command::PlayContext {
                ids: vec![0],
                start: 0,
            },
            0,
        )
        .unwrap();
        drive(&mut p, 3);
        apply_command(&mut p, Command::Pause, 0).unwrap();
        assert_eq!(project_tick(&p, 1.0).state, PlayState::Paused);
        apply_command(&mut p, Command::TogglePlay, 0).unwrap();
        assert_eq!(project_tick(&p, 1.0).state, PlayState::Playing);
    }

    #[test]
    fn play_next_then_enqueue_populate_upnext_in_order() {
        let mut p = player();
        apply_command(
            &mut p,
            Command::PlayContext {
                ids: vec![0, 1, 2],
                start: 0,
            },
            0,
        )
        .unwrap();
        drive(&mut p, 3);
        apply_command(&mut p, Command::Enqueue(2), 0).unwrap();
        apply_command(&mut p, Command::PlayNext(1), 0).unwrap();
        // PlayNext jumps the front; Enqueue is behind it.
        assert_eq!(project_queue(&p).up_next_ids, vec![1, 2]);
        apply_command(&mut p, Command::ClearQueue, 0).unwrap();
        assert!(project_queue(&p).up_next_ids.is_empty());
    }

    #[test]
    fn shuffle_and_repeat_flags_project() {
        let mut p = player();
        apply_command(
            &mut p,
            Command::PlayContext {
                ids: vec![0, 1, 2, 3],
                start: 0,
            },
            0,
        )
        .unwrap();
        drive(&mut p, 3);
        apply_command(&mut p, Command::SetShuffle(true), 12345).unwrap();
        assert!(project_tick(&p, 1.0).shuffle);
        apply_command(&mut p, Command::SetRepeat(RepeatMode::All), 0).unwrap();
        assert_eq!(project_tick(&p, 1.0).repeat, RepeatMode::All);
        apply_command(&mut p, Command::SetShuffle(false), 0).unwrap();
        assert!(!project_tick(&p, 1.0).shuffle);
    }

    #[test]
    fn prev_restarts_when_past_threshold_else_steps_back() {
        // Use a small threshold (FRAMES/4 = 1000 frames) so the 4000-frame
        // ConstDecoder track can both outrun it (restart branch) and sit under
        // it (step-back branch). Advance to track 1 and play past the threshold,
        // then Prev must restart track 1 (seek 0, cursor stays).
        let mut p = player_with_threshold(FRAMES / 4);
        apply_command(
            &mut p,
            Command::PlayContext {
                ids: vec![0, 1, 2],
                start: 0,
            },
            0,
        )
        .unwrap();
        drive(&mut p, 3);
        apply_command(&mut p, Command::Next, 0).unwrap();
        drive(&mut p, 2);
        assert_eq!(p.current(), Some(1), "on track 1");
        // Advance enough pumps (BLOCK=256 frames each) to exceed FRAMES/4.
        drive(&mut p, 4);
        assert!(
            p.position_frames() > FRAMES / 4,
            "past threshold: pos {} > {}",
            p.position_frames(),
            FRAMES / 4
        );
        // Past threshold → Prev restarts track 1 (seek 0, cursor stays).
        apply_command(&mut p, Command::Prev, 0).unwrap();
        assert_eq!(
            p.current(),
            Some(1),
            "prev past threshold restarts on track 1"
        );
        assert_eq!(p.position_frames(), 0, "restart resets position");
        // Now at the track start (under threshold) → Prev steps back to track 0.
        apply_command(&mut p, Command::Prev, 0).unwrap();
        assert_eq!(p.current(), Some(0), "prev under threshold steps back");
    }

    #[test]
    fn goto_jumps_within_context() {
        let mut p = player();
        apply_command(
            &mut p,
            Command::PlayContext {
                ids: vec![0, 1, 2, 3],
                start: 0,
            },
            0,
        )
        .unwrap();
        drive(&mut p, 3);
        apply_command(&mut p, Command::Goto(2), 0).unwrap();
        drive(&mut p, 3);
        assert_eq!(p.current(), Some(2));
    }
}
