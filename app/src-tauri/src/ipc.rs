//! tauri-specta (v2) bridge: exports typed TS bindings for the frontend from
//! the service DTOs (single source of truth) + the app commands.
#![forbid(unsafe_code)]

use specta_typescript::Typescript;
use tauri::Wry;
use tauri_specta::{Builder, collect_commands};

use crate::commands;

/// Build the typed IPC surface. Call `.export()` in debug builds to regenerate
/// `app/src/lib/bindings.ts`, then hand `.invoke_handler()` to the Tauri
/// builder (see main.rs). Events are emitted manually from `actor.rs` over a
/// plain `"player"` channel using the specta-registered `Event` type.
pub fn builder() -> Builder<Wry> {
    Builder::<Wry>::new()
        // The actor emits the service `Event` enum on the "player" channel;
        // register it (plus its Tick/QueueState payloads transitively) so the
        // generated bindings carry the event types for the frontend.
        .typ::<soul_audio_service::Event>()
        .commands(collect_commands![
            commands::library_views,
            commands::open_folder,
            commands::send_command,
            commands::track_art,
        ])
}

/// Regenerate `app/src/lib/bindings.ts` from the current command signatures.
/// Run via `cargo run --bin codegen` in `app/src-tauri` (or it auto-runs in
/// debug builds from main.rs).
pub fn export(b: &Builder<Wry>) {
    // Resolve from the manifest dir (not CWD) so the export lands in app/src/lib
    // regardless of where cargo runs.
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/lib/bindings.ts");
    b.export(Typescript::default(), out)
        .expect("failed to export typescript bindings");
}
