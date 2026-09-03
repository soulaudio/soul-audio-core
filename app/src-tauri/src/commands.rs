//! Tauri command layer over `soul-audio-service`: folder open → scan → actor,
//! library views, and transport/queue command dispatch.
#![forbid(unsafe_code)]

use std::sync::Arc;

use soul_audio_service::{Command, Library, TrackView};
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

use crate::actor::PlayerState;

/// Ask the user for a folder, scan it into a `Library`, hand it to the actor,
/// and start playing it from the first track.
#[tauri::command]
#[specta::specta]
pub fn open_folder(
    state: State<'_, PlayerState>,
    app: AppHandle,
) -> Result<Vec<TrackView>, String> {
    // Native folder picker (blocks on the dialog thread).
    let picked = app
        .dialog()
        .file()
        .blocking_pick_folder()
        .ok_or_else(|| "no folder selected".to_string())?;
    let dir = picked.into_path().map_err(|e| e.to_string())?;

    // Scan off the UI thread is the caller's job per the service docs; a
    // folder scan of a music library is fast enough here for the MVP.
    let lib = Arc::new(Library::scan(&dir).map_err(|e| format!("scan failed: {e:#}"))?);
    let views = lib.views();
    if views.is_empty() {
        return Err("no supported audio files in that folder".to_string());
    }

    state.set_library(lib);
    // Open the folder = play its whole context from track 0.
    let ids: Vec<u32> = views.iter().map(|v| v.id).collect();
    state.send_command(&app, Command::PlayContext { ids, start: 0 });

    Ok(views)
}

/// The current library's views (used to render the list without re-scanning).
#[tauri::command]
#[specta::specta]
pub fn library_views(state: State<'_, PlayerState>) -> Vec<TrackView> {
    state.library().map(|l| l.views()).unwrap_or_default()
}

/// Fire one transport/queue command at the actor (fire-and-forget semantics).
#[tauri::command]
#[specta::specta]
pub fn send_command(
    state: State<'_, PlayerState>,
    app: AppHandle,
    cmd: Command,
) -> Result<(), String> {
    state.send_command(&app, cmd);
    Ok(())
}

/// Front-cover art for a track id as a `data:<mime>;base64,<…>` URL, or `None`.
///
/// Resolution is delegated to `soul_audio_service::cover_bytes`, which tries the
/// file's embedded art first and then an external cover image next to the track
/// (e.g. `Scans/Front-1.jpg`, the common layout for DSD/SACD rips that carry
/// only text tags). Missing library / path / art all resolve to `Ok(None)` —
/// art is optional decoration, never worth erroring the UI over. Returning a
/// base64 data URL (rather than a raw byte array) keeps multi-MB external scans
/// cheap over IPC and lets the frontend use it directly as an `<img>` src.
#[tauri::command]
#[specta::specta]
pub fn track_art(state: State<'_, PlayerState>, id: u32) -> Result<Option<String>, String> {
    let Some(lib) = state.library() else {
        return Ok(None);
    };
    let Some(path) = lib.path_of(id).map(|p| p.to_owned()) else {
        return Ok(None);
    };
    match soul_audio_service::cover_bytes(&path) {
        Some(cover) => Ok(Some(to_data_url(&cover.mime, &cover.bytes))),
        None => Ok(None),
    }
}

/// Build a `data:<mime>;base64,<…>` URL from raw image bytes.
fn to_data_url(mime: &str, bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + mime.len() + 16);
    out.push_str("data:");
    out.push_str(mime);
    out.push_str(";base64,");
    base64_encode_into(bytes, &mut out);
    out
}

/// Standard-alphabet base64 encode, appending to `out`. (Encode side only; the
/// no_std `soul-audio-tags::base64` module owns the decode side.)
fn base64_encode_into(bytes: &[u8], out: &mut String) {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut chunks = bytes.chunks_exact(3);
    for c in chunks.by_ref() {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        out.push(ALPHA[(n >> 18 & 63) as usize] as char);
        out.push(ALPHA[(n >> 12 & 63) as usize] as char);
        out.push(ALPHA[(n >> 6 & 63) as usize] as char);
        out.push(ALPHA[(n & 63) as usize] as char);
    }
    match chunks.remainder() {
        [a] => {
            let n = u32::from(*a) << 16;
            out.push(ALPHA[(n >> 18 & 63) as usize] as char);
            out.push(ALPHA[(n >> 12 & 63) as usize] as char);
            out.push_str("==");
        }
        [a, b] => {
            let n = (u32::from(*a) << 16) | (u32::from(*b) << 8);
            out.push(ALPHA[(n >> 18 & 63) as usize] as char);
            out.push(ALPHA[(n >> 12 & 63) as usize] as char);
            out.push(ALPHA[(n >> 6 & 63) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{base64_encode_into, to_data_url};

    fn enc(b: &[u8]) -> String {
        let mut s = String::new();
        base64_encode_into(b, &mut s);
        s
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(enc(b""), "");
        assert_eq!(enc(b"f"), "Zg==");
        assert_eq!(enc(b"fo"), "Zm8=");
        assert_eq!(enc(b"foo"), "Zm9v");
        assert_eq!(enc(b"foob"), "Zm9vYg==");
        assert_eq!(enc(b"fooba"), "Zm9vYmE=");
        assert_eq!(enc(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn data_url_wraps_mime_and_payload() {
        assert_eq!(
            to_data_url("image/png", b"foo"),
            "data:image/png;base64,Zm9v"
        );
    }
}
