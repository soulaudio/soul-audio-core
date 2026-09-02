//! The player actor: two non-real-time threads around a `Player`.
//! - **pump**: owns the `Player` + sink, drains the command channel, services
//!   decoder opens, pumps one block per cycle, and emits throttled state.
//! - **loader**: resolves `id → path` against the current `Library` and opens
//!   the (large, `Send`) `OwnedDecoder` off the pump thread.
//!
//! Sink-generic via a factory run ON the pump thread, so a `!Send` device sink
//! (`CpalSink`) works and this crate never names `cpal`.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use soul_audio_control::Player;
use soul_audio_core::{Deck, Progress, Quality, Sink};
use soul_audio_desktop::{OwnedDecoder, open_track};
use soul_audio_engine::{Engine, Provided};

use crate::driver::{ServicePlayer, apply_command, project_queue, project_tick, queue_for};
use crate::library::Library;
use crate::types::{Command, Event};

/// Golden-ratio odd constant; advanced per shuffle to vary the permutation seed
/// without an RNG dependency (deterministic across runs).
const SEED_STEP: u64 = 0x9E37_79B9_7F4A_7C15;

/// Handle to the running player actor. Dropping it stops the threads.
pub struct AudioPlayer {
    cmd_tx: Option<Sender<Command>>,
    library: Arc<Mutex<Arc<Library>>>,
    pump: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    /// Spawn the actor. `make_sink` runs on the pump thread (so `K` need not be
    /// `Send`). `on_event` is called from the pump thread on every emitted
    /// event. `library` is the initial `id → path` map (swap later via
    /// [`set_library`](Self::set_library)).
    pub fn spawn<K, Mk, Ev>(make_sink: Mk, library: Arc<Library>, on_event: Ev) -> Self
    where
        K: Sink + 'static,
        Mk: FnOnce() -> anyhow::Result<K> + Send + 'static,
        Ev: Fn(Event) + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let library = Arc::new(Mutex::new(library));
        let loader_lib = Arc::clone(&library);

        let pump = thread::Builder::new()
            .name("player-pump".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || pump_thread(make_sink, cmd_rx, loader_lib, on_event))
            .expect("spawn pump thread");

        Self {
            cmd_tx: Some(cmd_tx),
            library,
            pump: Some(pump),
        }
    }

    /// Queue a command (fire-and-forget). No-op if the actor has stopped.
    pub fn send(&self, cmd: Command) {
        if let Some(tx) = &self.cmd_tx {
            let _ = tx.send(cmd);
        }
    }

    /// Replace the library used by the loader to resolve `id → path`.
    pub fn set_library(&self, library: Arc<Library>) {
        *self.library.lock().expect("library mutex") = library;
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        // Disconnect the command channel FIRST (drop cmd_tx) so the pump loop
        // observes `Disconnected`, then joins. The pump only exits when the
        // channel is disconnected, so joining before dropping cmd_tx deadlocks.
        drop(self.cmd_tx.take());
        if let Some(h) = self.pump.take() {
            let _ = h.join();
        }
    }
}

/// The loader thread: `id → path → OwnedDecoder`, sent back to the pump.
fn loader_thread(
    req_rx: Receiver<u32>,
    res_tx: Sender<(u32, anyhow::Result<OwnedDecoder>)>,
    library: Arc<Mutex<Arc<Library>>>,
) {
    for id in req_rx {
        let opened = {
            let lib = library.lock().expect("library mutex").clone();
            match lib.path_of(id) {
                Some(path) => open_track(path),
                None => Err(anyhow::anyhow!("no path for track id {id}")),
            }
        };
        if res_tx.send((id, opened)).is_err() {
            break; // pump gone
        }
    }
}

fn pump_thread<K, Mk, Ev>(
    make_sink: Mk,
    cmd_rx: Receiver<Command>,
    library: Arc<Mutex<Arc<Library>>>,
    on_event: Ev,
) where
    K: Sink + 'static,
    Mk: FnOnce() -> anyhow::Result<K>,
    Ev: Fn(Event),
{
    let sink = match make_sink() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("player: sink init failed: {e:#}");
            return;
        }
    };
    let rate = sink.stream_info().sample_rate;
    let deck = Deck::new(sink, Quality::Hq);
    let engine = Engine::new(deck, queue_for(Vec::new()));
    let mut player: ServicePlayer<K, OwnedDecoder> =
        Player::new(engine, u64::from(rate).saturating_mul(3)); // ~3 s prev threshold

    let (req_tx, req_rx) = mpsc::channel::<u32>();
    let (res_tx, res_rx) = mpsc::channel::<(u32, anyhow::Result<OwnedDecoder>)>();
    let loader = {
        let lib = Arc::clone(&library);
        thread::Builder::new()
            .name("player-loader".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || loader_thread(req_rx, res_tx, lib))
            .expect("spawn loader thread")
    };

    let mut volume: f32 = 1.0;
    let mut seed: u64 = SEED_STEP;
    let mut last_tick = Instant::now();
    let tick_every = Duration::from_millis(125); // ~8 Hz
    let mut disconnected = false;

    loop {
        // 1. Drain commands.
        let mut changed = false;
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    if let Command::SetVolume(v) = cmd {
                        // Clamp at intake so projected `Tick.volume` matches what
                        // `Deck::set_volume` actually applies (0.0..=1.0).
                        volume = v.clamp(0.0, 1.0);
                    }
                    seed = seed.wrapping_add(SEED_STEP);
                    if let Err(e) = apply_command(&mut player, cmd, seed) {
                        eprintln!("player: command error: {e:?}");
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        // 2. Install opened decoders; request whatever the engine now wants.
        loop {
            match res_rx.try_recv() {
                Ok((id, Ok(dec))) => {
                    if let Ok(Provided::Loaded) = player.provide(id, dec) {
                        changed = true;
                    }
                }
                Ok((id, Err(e))) => eprintln!("player: open track {id} failed: {e:#}"),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        while let Some(id) = player.poll_open() {
            let _ = req_tx.send(id);
        }

        // 3. Pump one block (paced by the real sink's backpressure; NullSink
        //    returns immediately, so tests run briefly then drop the actor).
        let progress = match player.pump() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("player: pump error: {e:?}");
                Progress::Idle
            }
        };
        if matches!(progress, Progress::TrackChanged | Progress::Ended) {
            changed = true;
        }

        // 4. Emit throttled state (immediately on change, else ~8 Hz).
        if changed || last_tick.elapsed() >= tick_every {
            on_event(Event::Tick(project_tick(&player, volume)));
            on_event(Event::Queue(project_queue(&player)));
            last_tick = Instant::now();
        }

        // 5. Shutdown / idle pacing. The command channel disconnects only when
        //    the AudioPlayer handle is dropped — that IS the shutdown signal.
        //    Exit immediately rather than waiting for `!is_active()`: an active
        //    track would otherwise extend the join in `Drop` by the whole
        //    remaining playback (real-time-paced under CpalSink), and Repeat
        //    modes never become inactive, so the join would block forever.
        if disconnected {
            break;
        }
        if matches!(progress, Progress::Idle) {
            thread::sleep(Duration::from_millis(3));
        }
    }

    drop(req_tx); // stop the loader
    let _ = loader.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::NullSink;
    use std::path::PathBuf;
    use std::sync::mpsc::RecvTimeoutError;

    fn wav(rate: u32, frames: u32) -> Vec<u8> {
        // Minimal stereo-16-bit WAV writer (same as library.rs tests).
        let channels: u16 = 2;
        let block_align = channels * 2;
        let byte_rate = rate * u32::from(block_align);
        let data_len = frames * u32::from(block_align);
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        v.resize(v.len() + data_len as usize, 0);
        v
    }

    fn fixture_library(dir: &PathBuf) -> Arc<Library> {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("a.wav"), wav(48_000, 96_000)).unwrap(); // 2 s
        std::fs::write(dir.join("b.wav"), wav(48_000, 96_000)).unwrap();
        Arc::new(Library::scan(dir).unwrap())
    }

    #[test]
    fn actor_plays_context_and_emits_now_playing() {
        let dir = std::env::temp_dir().join(format!("sas_actor_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = fixture_library(&dir);

        let (ev_tx, ev_rx) = mpsc::channel::<Event>();
        let player = AudioPlayer::spawn(
            || Ok(NullSink::new(48_000, 2)),
            Arc::clone(&lib),
            move |ev| {
                let _ = ev_tx.send(ev);
            },
        );

        player.send(Command::PlayContext {
            ids: vec![0, 1],
            start: 0,
        });

        // Wait (bounded) for a Tick showing track 0 playing.
        let deadline = Duration::from_secs(5);
        let mut saw_playing = false;
        loop {
            match ev_rx.recv_timeout(deadline) {
                Ok(Event::Tick(t)) => {
                    if t.now_playing_id == Some(0) && t.state == crate::types::PlayState::Playing {
                        saw_playing = true;
                        break;
                    }
                }
                Ok(Event::Queue(_)) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(saw_playing, "never observed track 0 playing");

        drop(player); // clean shutdown (joins threads)
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn drop_returns_promptly_even_while_repeat_all_plays() {
        let dir = std::env::temp_dir().join(format!("sas_actor_drop_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = fixture_library(&dir);

        let (ev_tx, ev_rx) = mpsc::channel::<Event>();
        let player = AudioPlayer::spawn(
            || Ok(NullSink::new(48_000, 2)),
            Arc::clone(&lib),
            move |ev| {
                let _ = ev_tx.send(ev);
            },
        );

        // Repeat::All means the player never becomes inactive on its own;
        // dropping the handle must still shut the pump down promptly.
        // (Regression: the old shutdown condition also required `!is_active()`
        // so `Drop`'s join blocked forever while repeat playback was active.)
        player.send(Command::SetRepeat(crate::types::RepeatMode::All));
        player.send(Command::PlayContext {
            ids: vec![0, 1],
            start: 0,
        });

        // Wait until a Tick shows track 0 playing (loader round-trip done).
        let mut saw_playing = false;
        let deadline = Duration::from_secs(5);
        loop {
            match ev_rx.recv_timeout(deadline) {
                Ok(Event::Tick(t)) => {
                    if t.now_playing_id == Some(0) && t.state == crate::types::PlayState::Playing {
                        saw_playing = true;
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(saw_playing, "never observed track 0 playing");

        // Drop on a helper thread: the join must return promptly, not block
        // on Repeat::All playback that would otherwise never end.
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let dropper = std::thread::spawn(move || {
            drop(player);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "drop(player) blocked: pump thread did not exit while repeat playback was active"
        );
        let _ = dropper.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
