//! Cue-sheet expansion: a single-file album becomes the tracks it indexes.
//!
//! A cue rip is one audio file plus an `album.cue` naming the tracks inside it.
//! Imported literally that is one 50-minute "track" with the album's name; what
//! the user wants is the tracks. This module turns the pair into N library
//! entries, each carrying its own title/artist/number and its **slice** of the
//! shared file (`RawTrack::cue_range`, which the baker stores and
//! `soul_audio_core::SubRange` plays).
//!
//! Three decisions worth stating, because each is a way to lose someone's
//! album:
//!
//! - **The whole-file entry is replaced, not added to.** Otherwise the album
//!   appears twice: once as its tracks and once as a single 50-minute item.
//! - **A cue that does not line up with reality is ignored**, and the file
//!   imports as itself. A sheet naming a file that is not there (a renamed
//!   rip), a sheet whose audio we cannot decode, a sheet with no usable index
//!   points — in every case the honest outcome is the plain file, not a
//!   library of tracks that cannot play.
//! - **Encoding is tolerated.** Cue sheets in the wild are UTF-8 or a legacy
//!   code page; a sheet that is not UTF-8 is read as Windows-1252 so the splits
//!   are still right even when a title's accents are not. Mojibake in a title
//!   is recoverable by re-tagging; a missing album is not.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use soul_audio_playlist::cue::{self, CueSheet};

use crate::RawTrack;

/// Decode cue-sheet bytes to text: UTF-8 when it is valid, else Windows-1252
/// (whose first 256 code points are Latin-1 for everything a cue carries).
/// Never fails — the splits matter more than the accents.
fn decode(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(s) => s,
        // Latin-1: byte → the same code point. A real Windows-1252 sheet with
        // smart quotes renders them as control pictures, which is cosmetic.
        Err(e) => e.into_bytes().into_iter().map(char::from).collect(),
    }
}

/// Resolve a cue `FILE` name against the sheet's own directory, tolerating the
/// case mismatch a cross-platform rip leaves behind (`Album.FLAC` written on
/// Windows, read on Linux). `None` if nothing matches.
///
/// **Only the leaf of the name is used**, and that is a deliberate boundary, not
/// a simplification: a cue sheet is untrusted text that names a path, and
/// `FILE "../../../../etc/passwd"` must not make the importer read, index or
/// later play a file outside the user's library. Taking the leaf also happens to
/// be right for the common case — a sheet sits beside its audio, and a rip moved
/// between machines carries a stale absolute path that only its file name
/// survives. (`tests/cue_album.rs` pins the escape attempts.)
fn resolve_file(cue_dir: &Path, name: &str) -> Option<PathBuf> {
    // Split on BOTH separators whatever the host OS: a Windows-written sheet is
    // read on Linux, where `\` is an ordinary filename character and would
    // otherwise sail through as part of the "leaf".
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or(name);
    // A leaf of `.`/`..` names a directory, not a file; `is_file` below would
    // reject it anyway, but refusing here keeps the intent obvious.
    if leaf.is_empty() || leaf == "." || leaf == ".." {
        return None;
    }
    let direct = cue_dir.join(leaf);
    if direct.is_file() {
        return Some(direct);
    }
    let want = leaf.to_ascii_lowercase();
    std::fs::read_dir(cue_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_ascii_lowercase() == want)
        })
}

/// The audio file's length in frames, for the last cue track's end: the
/// container's own count when it declares one, else derived from the duration
/// the tag scan already read. `None` when neither is known (then the last
/// track runs to the end of the file and is simply not length-capped).
fn total_frames(rt: &RawTrack) -> Option<u64> {
    rt.total_frames.or_else(|| {
        (rt.sample_rate > 0 && rt.duration_ms > 0)
            .then(|| u64::from(rt.duration_ms) * u64::from(rt.sample_rate) / 1000)
    })
}

/// Build the per-track [`RawTrack`]s for one `FILE` of a sheet, from the
/// whole-file `base` the tag scan produced. Empty when the sheet gives this
/// file no usable audio tracks (then the caller keeps `base`).
fn tracks_for_file(sheet: &CueSheet, file: &cue::CueFile, base: &RawTrack) -> Vec<RawTrack> {
    let rate = base.sample_rate;
    if rate == 0 {
        return Vec::new(); // unknown rate: index points cannot be placed
    }
    let ranges = file.ranges(rate, total_frames(base));
    let mut out = Vec::with_capacity(ranges.len());
    for range in ranges {
        let Some(frames) = range.frames else {
            // No known file length and no following track — the slice is
            // open-ended, which the catalog cannot express as (start, len).
            continue;
        };
        if frames == 0 {
            continue; // a zero-length slice is not a track
        }
        let (Ok(start32), Ok(frames32)) = (u32::try_from(range.start_frame), u32::try_from(frames))
        else {
            // Past ~27 h at CD rate / 6 h at 192 kHz. Not a real rip; keep the
            // whole-file entry rather than truncating a slice.
            return Vec::new();
        };
        let track = &file.tracks[range.track];
        let mut rt = base.clone();
        // Per-track metadata from the sheet, falling back to the file's tags —
        // a sheet always titles its tracks, but rarely repeats the artist.
        if let Some(title) = track
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            rt.title = title.to_owned();
        }
        if let Some(artist) = track
            .performer
            .as_deref()
            .or(sheet.performer.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            rt.artist_name = artist.to_owned();
        }
        if let Some(album) = sheet
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            rt.album_name = album.to_owned();
        }
        if let Some(performer) = sheet
            .performer
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            rt.album_artist_name = performer.to_owned();
        }
        if let Some(genre) = sheet
            .genre
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            rt.genre_name = genre.to_owned();
        }
        if track.number > 0 {
            rt.track_no = track.number;
        }
        // The track's own duration, not the album file's.
        rt.duration_ms = (frames.saturating_mul(1000) / u64::from(rate)) as u32;
        rt.cue_range = Some((start32, frames32));
        // The MusicBrainz recording id came from the file's tags and identifies
        // the *album* file, not this track — carrying it onto every track would
        // claim N recordings share one MBID.
        rt.mbid.clear();
        out.push(rt);
    }
    out
}

/// Expand every cue sheet in `cues` against the scanned `raw` tracks, replacing
/// each indexed whole-file entry with the tracks the sheet describes.
///
/// Walk order is preserved: an expansion lands where its audio file was, so
/// track ids stay deterministic for a given tree.
pub(crate) fn expand(cues: &[PathBuf], raw: Vec<RawTrack>) -> Vec<RawTrack> {
    if cues.is_empty() {
        return raw;
    }
    // audio file path → the tracks that replace it.
    let mut replacements: HashMap<PathBuf, Vec<RawTrack>> = HashMap::new();
    let by_path: HashMap<&Path, &RawTrack> = raw.iter().map(|r| (r.path.as_path(), r)).collect();

    for cue_path in cues {
        let Some(dir) = cue_path.parent() else {
            continue;
        };
        let Ok(bytes) = std::fs::read(cue_path) else {
            continue; // unreadable sheet: the files import as themselves
        };
        // A cue sheet is kilobytes; anything far larger is not one.
        if bytes.len() > 1 << 20 {
            continue;
        }
        let Ok(sheet) = cue::parse(&decode(bytes)) else {
            continue; // unparseable (no tracks, bad timestamp) — leave it alone
        };
        for file in &sheet.files {
            let Some(audio) = resolve_file(dir, &file.name) else {
                continue; // the sheet names a file that is not here
            };
            let Some(base) = by_path.get(audio.as_path()) else {
                continue; // not a file the scan imported (unsupported codec)
            };
            let tracks = tracks_for_file(&sheet, file, base);
            if tracks.is_empty() {
                continue;
            }
            // Two sheets indexing the same file (a stray duplicate): the first
            // in walk order wins, deterministically.
            replacements.entry(audio).or_insert(tracks);
        }
    }
    if replacements.is_empty() {
        return raw;
    }
    let mut out = Vec::with_capacity(raw.len() + replacements.len());
    for rt in raw {
        match replacements.remove(rt.path.as_path()) {
            Some(tracks) => out.extend(tracks),
            None => out.push(rt),
        }
    }
    out
}
