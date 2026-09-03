//! App-managed player state: owns the `AudioPlayer` actor (spawned lazily over
//! `CpalSink` on first folder open) and the current `Library`, and forwards the
//! actor's `Event`s to the webview over the Tauri event system.
//!
//! `SOUL_PLAYER_OFFLINE=1` swaps in a no-op `NullSink` so the full command/event
//! wiring can be smoke-tested without an audio device.
#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use soul_audio_core::Sink;
use soul_audio_desktop::sink::CpalSink;
use soul_audio_service::{AudioPlayer, Event, Library};
use tauri::{AppHandle, Emitter};

use crate::offline::NullSink;

/// Tauri-managed state. `player` is `None` until a folder has been opened —
/// we deliberately do NOT grab the audio device at app startup.
pub struct PlayerState {
    player: Mutex<Option<AudioPlayer>>,
    library: Mutex<Option<Arc<Library>>>,
}

impl PlayerState {
    pub fn new() -> Self {
        Self {
            player: Mutex::new(None),
            library: Mutex::new(None),
        }
    }

    /// The current library (if a folder has been opened).
    pub fn library(&self) -> Option<Arc<Library>> {
        self.library.lock().expect("library mutex").clone()
    }

    /// Replace the library the actor resolves track ids against.
    ///
    /// Track ids are per-library scan indices (`0..N`), so a re-opened folder's
    /// ids collide with the previous one's. The lazily-built actor keeps the
    /// library it was born with, so we must also push the new library into its
    /// running loader — otherwise clicking a track in the newly-opened folder
    /// resolves that id against the OLD library and plays the wrong file.
    pub fn set_library(&self, lib: Arc<Library>) {
        // Update the app-side cache first, releasing that lock before taking the
        // player lock. `with_actor` locks player→library; keeping these two
        // acquisitions un-nested (never both held here) avoids a lock-order
        // inversion / deadlock.
        {
            *self.library.lock().expect("library mutex") = Some(Arc::clone(&lib));
        }
        if let Some(actor) = self.player.lock().expect("player mutex").as_ref() {
            actor.set_library(lib);
        }
    }

    /// Lazily build the actor (over `CpalSink`, or `NullSink` in offline mode),
    /// then run `f` with a guard for sending commands.
    fn with_actor<R>(
        &self,
        app: &AppHandle,
        f: impl FnOnce(&AudioPlayer) -> R,
    ) -> Result<R, String> {
        let mut guard = self.player.lock().expect("player mutex");
        if guard.is_none() {
            let lib = self
                .library()
                .ok_or_else(|| "open a folder first".to_string())?;
            *guard = Some(build_actor(app.clone(), lib));
        }
        Ok(f(guard.as_ref().expect("actor just built")))
    }

    /// Fire a command at the actor (no-op if no folder opened yet).
    pub fn send_command(&self, app: &AppHandle, cmd: soul_audio_service::Command) {
        let _ = self.with_actor(app, |p| p.send(cmd));
    }
}

/// Build the actor over a real `CpalSink`, or a no-op `NullSink` when
/// `SOUL_PLAYER_OFFLINE=1` (device-free smoke runs). The sink factory runs ON
/// the pump thread (the Plan 3 design), and every service `Event` is re-emitted
/// to the frontend as a single `"player"` channel.
fn build_actor(app: AppHandle, lib: Arc<Library>) -> AudioPlayer {
    if std::env::var_os("SOUL_PLAYER_OFFLINE").is_some() {
        return build_actor_with(app, lib, offline_sink);
    }
    build_actor_with(app, lib, CpalSink::new)
}

fn build_actor_with<K: Sink + 'static>(
    app: AppHandle,
    lib: Arc<Library>,
    make_sink: fn() -> anyhow::Result<K>,
) -> AudioPlayer {
    AudioPlayer::spawn(make_sink, lib, move |event: Event| {
        // Forward every backend event to the webview.
        let _ = app.emit("player", event);
    })
}

/// Offline-mode sink factory (no device; playback runs instantly).
fn offline_sink() -> anyhow::Result<NullSink> {
    Ok(NullSink::new(48_000, 2))
}
