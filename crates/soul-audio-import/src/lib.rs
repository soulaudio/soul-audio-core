//! Desktop library importer (std).
//!
//! Walks a music tree, reads tags with `soul-audio-tags`, interns names into a
//! dense-id catalog with **folder-isolation** (album identity = album root
//! folder + title + MusicBrainz release id; a `CD1`/`Disc 2` folder folds into
//! its parent), and bakes a `.soulidx` the DAP reads verbatim. This is the "desktop bakes,
//! device reads" split from the architecture spec — the heavy scan happens here,
//! never on the DAP.
#![forbid(unsafe_code)]
#![deny(unconditional_recursion)]

use std::collections::HashMap;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use soul_audio_catalog::NO_FOLDER;
use soul_audio_index::build::{AlbumMeta, CatalogInput, FolderInput, TrackInput};
use soul_audio_tags::disc_folder::{self, DiscFolder};
use soul_audio_tags::{StreamFacts, Tags, read_owned};

mod cover;

/// Extensions the decoders support (lower-cased comparison).
///
/// Every entry here is also gated on its decoder actually being compiled in —
/// see [`is_supported`], which is what `"aac"` depends on.
const SUPPORTED: &[&str] = &[
    "wav", "flac", "mp3", "mp2", "mp1", "m4a", "aac", "ogg", "oga", "opus", "aiff", "aif", "dsf",
    "dff", "mka", "webm", "wv", "caf",
];
/// Audio formats this library does not decode, but which a music collection
/// plausibly contains. Listed so a scan can SAY it skipped them instead of
/// silently dropping files — a user whose `.wma` rips vanish has no way to tell
/// a deliberate limitation from a broken scan.
///
/// Best-effort and deliberately short: the point is to name the common cases,
/// not to enumerate every container in existence. Anything not here (artwork,
/// cue sheets, text) is simply not audio and is never mentioned.
const KNOWN_AUDIO_WE_DO_NOT_DECODE: &[&str] = &[
    "wma", "ape", "mpc", "tta", "shn", "ra", "m4b", "m4p", "aifc", "au", "ac3", "dts", "amr",
];

/// Why a file the scan walked past was not imported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SkipReason {
    /// The format is one this library knows, but its decoder was not compiled
    /// into this build — AAC is gated behind an opt-in feature (patent). A
    /// different build of the same app WOULD play these files.
    DecoderDisabled,
    /// A recognised audio format this library does not decode at all.
    UnsupportedFormat,
}

/// Files the scan passed over, grouped by extension: "12 .aac files, because the
/// decoder is disabled". Grouped rather than listed because a real library
/// yields thousands of paths and a user only needs the shape of the problem.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SkippedGroup {
    /// Lower-cased, without the dot.
    pub extension: String,
    pub reason: SkipReason,
    pub count: usize,
}

/// Accumulates skips during a walk. Counts only — see [`SkippedGroup`].
#[derive(Debug, Default)]
pub(crate) struct Skips {
    by_ext: BTreeMap<String, (SkipReason, usize)>,
}

impl Skips {
    /// Note that `path` was not imported, if it is audio worth mentioning.
    pub(crate) fn record(&mut self, path: &Path) {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return;
        };
        let ext = ext.to_ascii_lowercase();
        let reason = if SUPPORTED.contains(&ext.as_str()) {
            // In the supported set yet skipped: the decoder is not built in.
            SkipReason::DecoderDisabled
        } else if KNOWN_AUDIO_WE_DO_NOT_DECODE.contains(&ext.as_str()) {
            SkipReason::UnsupportedFormat
        } else {
            return; // not audio — nothing to tell the user
        };
        let entry = self.by_ext.entry(ext).or_insert((reason, 0));
        entry.1 += 1;
    }

    pub(crate) fn into_groups(self) -> Vec<SkippedGroup> {
        self.by_ext
            .into_iter()
            .map(|(extension, (reason, count))| SkippedGroup {
                extension,
                reason,
                count,
            })
            .collect()
    }
}

/// Last-resort recursion guard against a pathological tree.
const MAX_DEPTH: usize = 64;

/// Whether a scan should import `path`.
///
/// The extension must be one the decoders handle AND its decoder must be built
/// into this binary. The second half matters because AAC is gated behind an
/// opt-in feature (patent): without the check, a product that does not take the
/// licence still imports the user's `.aac` files, shows them in the library, and
/// only fails when they press play. Not listing them is the honest behaviour.
///
/// `.m4a` cannot be filtered this way — the container carries ALAC as well as
/// AAC, and only a decode can tell them apart — so an AAC-in-MP4 file stays
/// listed and unplayable in an AAC-less build.
fn is_supported(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    if !SUPPORTED.contains(&ext.as_str()) {
        return false;
    }
    // `AAC_ENABLED` reflects the feature as resolved across the whole dependency
    // graph, so this agrees with whatever decoder was actually compiled.
    ext != "aac" || soul_audio_core::AAC_ENABLED
}

/// Skip dotfiles / dot-dirs (macOS `._*` AppleDouble sidecars, `.DS_Store`, …).
fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
}

/// Recursively collect supported files under `dir`, sorted for deterministic ids.
/// Robust like the desktop service scan: symlink cycles terminate (canonical
/// visited-set), an unreadable subdir is skipped with a warning, and only the
/// root's own failure propagates.
fn walk(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    skips: &mut Skips,
    depth: usize,
) -> std::io::Result<()> {
    if depth >= MAX_DEPTH {
        return Ok(());
    }
    let canon = std::fs::canonicalize(dir)?;
    if !visited.insert(canon) {
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if is_hidden(&path) {
            continue;
        }
        if path.is_dir() {
            if let Err(e) = walk(&path, out, visited, skips, depth + 1) {
                eprintln!("import: skipping unreadable dir {path:?}: {e}");
            }
        } else if is_supported(&path) {
            out.push(path);
        } else {
            // Not imported — tell the caller why, so the app can say so rather
            // than leaving the user to wonder where their files went.
            skips.record(&path);
        }
    }
    Ok(())
}

fn read_meta(path: &Path) -> Option<(Tags, StreamFacts)> {
    let file = std::fs::File::open(path).ok()?;
    let mut src = embedded_io_adapters::std::FromStd::new(file);
    read_owned(&mut src).ok()
}

/// Dense-id interner for the three entity kinds, preserving first-seen order so
/// ids are deterministic given the sorted walk.
#[derive(Default)]
struct Interner {
    artist_ids: HashMap<String, u32>,
    artists: Vec<String>,
    // Parallel to `artists`: each artist's sort-name ("Clapton, Eric"), defaulting
    // to the display name and upgraded in place the first time an occurrence
    // carries a sort tag.
    artist_sorts: Vec<String>,
    // Genre names have no sort variant (unlike artists), so a plain
    // dedupe-by-name table — `genres[id]` is the display name for genre `id`.
    genre_ids: HashMap<String, u32>,
    genres: Vec<String>,
    folder_ids: HashMap<PathBuf, u32>,
    folders: Vec<FolderInput>,
    // album key = (album-root folder id, title, effective MB release id) → folder-isolation
    // that folds disc folders and splits releases dumped into one folder.
    album_ids: HashMap<(u32, String, String), u32>,
    albums: Vec<AlbumMeta>,
    // Parallel to `albums`, recorded on first sight: the album's filesystem
    // folder and one representative track path — cover-art discovery's inputs.
    album_dirs: Vec<PathBuf>,
    album_tracks: Vec<PathBuf>,
}

impl Interner {
    /// Intern an artist by display `name`, recording its `sort` name (e.g. from
    /// `ARTISTSORT`/`ALBUMARTISTSORT`). The sort-name defaults to the display name
    /// and is upgraded in place the first time an occurrence supplies a real one,
    /// so a sort tag on any sighting wins even if the first sighting lacked it.
    fn artist(&mut self, name: &str, sort: Option<&str>) -> u32 {
        if let Some(&id) = self.artist_ids.get(name) {
            if let Some(s) = sort.filter(|s| !s.is_empty()) {
                let i = id as usize;
                if self.artist_sorts[i] == self.artists[i] {
                    self.artist_sorts[i] = s.to_owned(); // still default → upgrade
                }
            }
            return id;
        }
        let id = self.artists.len() as u32;
        self.artists.push(name.to_owned());
        self.artist_sorts
            .push(sort.filter(|s| !s.is_empty()).unwrap_or(name).to_owned());
        self.artist_ids.insert(name.to_owned(), id);
        id
    }

    /// Intern a genre by display `name`, returning its dense id. Unlike
    /// [`Interner::artist`] there is no sort-name variant — genres collate on
    /// the display name itself — so this is a plain dedupe-by-name, appending to
    /// `genres` on first sight and returning the stable id otherwise.
    fn genre(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.genre_ids.get(name) {
            return id;
        }
        let id = self.genres.len() as u32;
        self.genres.push(name.to_owned());
        self.genre_ids.insert(name.to_owned(), id);
        id
    }

    /// Intern `dir` (and, recursively, its ancestors up to `root`) into the
    /// folder table, returning its dense id. `path` is stored relative to `root`
    /// with `/` separators (the on-card, platform-neutral form); `parent` is the
    /// enclosing folder's id, or [`NO_FOLDER`] for the root / a top-level folder.
    /// Parents are interned first, so a folder's parent id is always already
    /// assigned and the table is a valid tree. `root` itself is folder-table
    /// entry with an empty path.
    fn folder(&mut self, dir: &Path, root: &Path) -> u32 {
        if let Some(&id) = self.folder_ids.get(dir) {
            return id;
        }
        let parent = if dir == root {
            NO_FOLDER
        } else {
            match dir.parent() {
                Some(p) => self.folder(p, root),
                None => NO_FOLDER,
            }
        };
        let rel = dir.strip_prefix(root).unwrap_or(dir);
        let path = normalize_rel(rel);
        let id = self.folders.len() as u32;
        self.folders.push(FolderInput { path, parent });
        self.folder_ids.insert(dir.to_path_buf(), id);
        id
    }

    /// Intern an album under folder-isolation, keyed on its ALBUM ROOT folder,
    /// title and MusicBrainz release id; fills its metadata on first sight
    /// (first track wins), along with `dir` (the album root on disk) and
    /// `track_path` (a representative track, for embedded cover-art discovery).
    /// `disc_count` starts at 1 and grows via [`Interner::note_disc`].
    #[allow(clippy::too_many_arguments)] // album identity + both dates + cover-discovery inputs
    fn album(
        &mut self,
        title: &str,
        folder: u32,
        mb_album_id: &str,
        album_artist: u32,
        original_date: soul_audio_catalog::PackedDate,
        release_date: soul_audio_catalog::PackedDate,
        dir: &Path,
        track_path: &Path,
    ) -> u32 {
        let key = (folder, title.to_owned(), mb_album_id.to_owned());
        if let Some(&id) = self.album_ids.get(&key) {
            return id;
        }
        let id = self.albums.len() as u32;
        self.albums.push(AlbumMeta {
            title: title.to_owned(),
            album_artist,
            original_date,
            release_date,
            folder,
            disc_count: 1,
            mb_album_id: mb_album_id.to_owned(),
        });
        self.album_dirs.push(dir.to_owned());
        self.album_tracks.push(track_path.to_owned());
        self.album_ids.insert(key, id);
        id
    }

    /// Fold one track's disc facts into its album:
    /// `disc_count = max(max disc_total tag, max disc_no seen, 1)`.
    fn note_disc(&mut self, album: u32, disc_no: u16, disc_total: u16) {
        let a = &mut self.albums[album as usize];
        a.disc_count = a.disc_count.max(disc_no).max(disc_total).max(1);
    }
}

/// Parse a tag date string (leading ISO `YYYY[-MM[-DD]]`) into a `PackedDate`.
/// Year is accepted only in a fixed plausible range `1000..=2200` (no clock, so
/// parsing is deterministic); an out-of-range year yields `UNKNOWN`. Month/day
/// are optional and individually dropped to "unknown" when absent or malformed.
fn parse_date(s: Option<&str>) -> soul_audio_catalog::PackedDate {
    use soul_audio_catalog::PackedDate;
    let Some(s) = s else {
        return PackedDate::UNKNOWN;
    };
    let year = match s.get(0..4).and_then(|y| y.parse::<u16>().ok()) {
        Some(y) if (1000..=2200).contains(&y) => y,
        _ => return PackedDate::UNKNOWN,
    };
    let month = s.get(5..7).and_then(|m| m.parse::<u8>().ok()).unwrap_or(0);
    let day = s.get(8..10).and_then(|d| d.parse::<u8>().ok()).unwrap_or(0);
    PackedDate::new(year, month, day) // new() drops out-of-range month/day
}

fn duration_ms(f: &StreamFacts) -> u32 {
    match (f.total_frames, f.sample_rate) {
        (Some(n), Some(r)) if r > 0 => (n.saturating_mul(1000) / u64::from(r)) as u32,
        _ => 0,
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned()
}

/// The leaf file name incl. extension (e.g. `01 - Aria.flac`) — the DAP opens
/// `card_root / folder.path / file_name`.
fn leaf_name(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned()
}

/// A path relative to the scan root, rendered with `/` separators regardless of
/// host OS — the platform-neutral on-card form (the root itself → `""`).
fn normalize_rel(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// A track's **album root** and the disc folder it sits in, if any. The album
/// root is the track's parent folder, unless that folder's name is a disc folder
/// (`CD1`, `Disc 2 - Liyue`; [`disc_folder::parse`]) strictly below the scan
/// `root` — then it is the grandparent, so `Album/CD1` + `Album/CD2` are one
/// album. Never climbs to or above `root`. `soul-audio-organize` applies the
/// same rule with the same matcher (it may not depend on this crate).
fn album_root<'a>(track: &'a Path, root: &'a Path) -> (&'a Path, Option<DiscFolder<'a>>) {
    let parent = track.parent().unwrap_or(root);
    if parent != root
        && parent.starts_with(root)
        && let Some(df) = parent
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(disc_folder::parse)
        && let Some(grandparent) = parent.parent()
    {
        return (grandparent, Some(df));
    }
    (parent, None)
}

/// One file's tag-derived metadata, extracted BEFORE interning. This is the
/// *costly* per-file product (a file open + tag/header parse) that the
/// incremental scanner caches by [`FileStamp`] so an unchanged file is never
/// re-read. `path` is the key; the rest mirrors what [`extract_raw`] pulls from
/// `Tags`/`StreamFacts`. `Clone` so the cache can hand copies back; `Send` (all
/// fields are) so extraction can fan out across threads.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RawTrack {
    path: PathBuf,
    title: String,
    artist_name: String,
    album_artist_name: String,
    album_name: String,
    artist_sort: Option<String>,
    album_artist_sort: Option<String>,
    original_date: soul_audio_catalog::PackedDate,
    release_date: soul_audio_catalog::PackedDate,
    genre_name: String,
    track_no: u16,
    /// The tagged disc number, `None` when untagged (then the disc folder's
    /// number, else 1, is used at intern time — that needs the scan root).
    disc_no: Option<u16>,
    /// The tagged disc total (`DISCTOTAL`/`TOTALDISCS`/`n/total`), 0 when untagged.
    disc_total: u16,
    /// The tagged disc subtitle (trimmed), empty when untagged.
    disc_subtitle: String,
    /// The MusicBrainz release id (trimmed), empty when untagged — album key part.
    mb_album_id: String,
    duration_ms: u32,
    sample_rate: u32,
    channels: u8,
    file_name: String,
    /// MusicBrainz recording MBID from the tags (empty when untagged) — baked into
    /// the index (v4) as the enrichment join key.
    mbid: String,
}

/// Read + derive one file's [`RawTrack`] — the single file-opening step. Pure
/// w.r.t. shared state (safe to call from many threads at once). An unreadable /
/// untagged file degrades to the defaults (never panics), exactly as `scan` did.
fn extract_raw(path: &Path) -> RawTrack {
    let (tags, facts) = read_meta(path).unwrap_or_default();
    let title = tags.title.clone().unwrap_or_else(|| file_stem(path));
    let artist_name = tags
        .artist
        .clone()
        .unwrap_or_else(|| "Unknown Artist".to_owned());
    let album_artist_name = tags
        .album_artist
        .clone()
        .or_else(|| tags.artist.clone())
        .unwrap_or_else(|| "Unknown Artist".to_owned());
    let album_name = tags
        .album
        .clone()
        .unwrap_or_else(|| "Unknown Album".to_owned());
    // Release vs. original-release date (reissue vs. the first pressing). Both
    // `PackedDate`s carry optional month/day precision; the original date is the
    // chronological sort key within an artist (see the index baker's `year_at`).
    let release_date = parse_date(tags.date.as_deref());
    let original_date = parse_date(tags.original_date.as_deref());
    // Genre from the tag, trimmed; blank/absent → the "Unknown Genre" bucket.
    let genre_name = tags
        .genre
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Unknown Genre")
        .to_owned();
    RawTrack {
        path: path.to_path_buf(),
        title,
        artist_name,
        album_artist_name,
        album_name,
        artist_sort: tags.artist_sort.clone(),
        album_artist_sort: tags.album_artist_sort.clone(),
        original_date,
        release_date,
        genre_name,
        track_no: tags.track.unwrap_or(0) as u16,
        // `try_from`, not `as`: a bogus 65536 must not wrap to 0 / a small disc.
        disc_no: tags
            .disc
            .and_then(|d| u16::try_from(d).ok())
            .filter(|&d| d > 0),
        disc_total: tags
            .disc_total
            .and_then(|d| u16::try_from(d).ok())
            .unwrap_or(0),
        disc_subtitle: tags
            .disc_subtitle
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_owned(),
        mb_album_id: tags
            .musicbrainz_album_id
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_owned(),
        duration_ms: duration_ms(&facts),
        sample_rate: facts.sample_rate.unwrap_or(0),
        channels: facts.channels.unwrap_or(0) as u8,
        file_name: leaf_name(path),
        mbid: tags.musicbrainz_recording_id.unwrap_or_default(),
    }
}

/// Intern a walk-ordered slice of [`RawTrack`]s into a [`CatalogInput`] with
/// folder-isolation, then discover each album's cover. Deterministic: ids follow
/// the input order, so the baked index is stable given a stable walk order (the
/// invariant `scan` and `scan_incremental` share).
fn build_catalog(raw: &[RawTrack], root: &Path) -> CatalogInput {
    let mut interner = Interner::default();
    let mut tracks = Vec::with_capacity(raw.len());
    // Pre-pass (walk order): the first non-empty MB release id per (album root,
    // title). An untagged track adopts it, so a partially tagged release stays
    // one album while two distinct releases in one folder still split.
    let mut first_mb: HashMap<(&Path, &str), &str> = HashMap::new();
    for r in raw.iter().filter(|r| !r.mb_album_id.is_empty()) {
        let (album_dir, _) = album_root(&r.path, root);
        first_mb
            .entry((album_dir, r.album_name.as_str()))
            .or_insert(r.mb_album_id.as_str());
    }
    for r in raw {
        // The track's real folder (its playback locator) …
        let folder = interner.folder(r.path.parent().unwrap_or(root), root);
        // … vs. its album root (a disc folder folds into its parent).
        let (album_dir, disc_dir) = album_root(&r.path, root);
        let album_folder = interner.folder(album_dir, root);
        let mb_album_id = if r.mb_album_id.is_empty() {
            first_mb
                .get(&(album_dir, r.album_name.as_str()))
                .copied()
                .unwrap_or("")
        } else {
            r.mb_album_id.as_str()
        };
        let artist = interner.artist(&r.artist_name, r.artist_sort.as_deref());
        let album_artist = interner.artist(
            &r.album_artist_name,
            r.album_artist_sort.as_deref().or(r.artist_sort.as_deref()),
        );
        let album = interner.album(
            &r.album_name,
            album_folder,
            mb_album_id,
            album_artist,
            r.original_date,
            r.release_date,
            album_dir,
            &r.path,
        );
        // Disc: tag → disc-folder number (0, e.g. `CD0`, counts as absent) → 1.
        // Subtitle: tag → folder text → "".
        let disc_no = r
            .disc_no
            .or(disc_dir.map(|d| d.number).filter(|&n| n > 0))
            .unwrap_or(1);
        let disc_subtitle = if r.disc_subtitle.is_empty() {
            disc_dir.and_then(|d| d.subtitle).unwrap_or("").to_owned()
        } else {
            r.disc_subtitle.clone()
        };
        interner.note_disc(album, disc_no, r.disc_total);
        let genre = interner.genre(&r.genre_name);
        tracks.push(TrackInput {
            title: r.title.clone(),
            artist,
            album,
            album_artist,
            genre,
            folder,
            track_no: r.track_no,
            disc_no,
            duration_ms: r.duration_ms,
            sample_rate: r.sample_rate,
            channels: r.channels,
            file_name: r.file_name.clone(),
            mbid: r.mbid.clone(),
            disc_subtitle,
            // Loudness is a deferred Tier-2 analysis pass (full decode), not the metadata scan.
            loudness: None,
        });
    }
    let covers: Vec<Option<Vec<u8>>> = interner
        .album_dirs
        .iter()
        .zip(interner.album_tracks.iter())
        .map(|(dir, track)| cover::find_album_cover(dir, track, root))
        .collect();
    CatalogInput {
        tracks,
        artists: interner.artists,
        artist_sorts: interner.artist_sorts,
        genres: interner.genres,
        albums: interner.albums,
        folders: interner.folders,
        playlists: Vec::new(),
        covers,
    }
}

/// Scan `root` into a [`CatalogInput`] (the baker's input), reading tags and
/// interning artists/albums/folders with folder-isolation. An unreadable `root`
/// errors; unreadable subtrees/tags degrade (never fail the whole scan). This is
/// the full (non-incremental) path: every file is re-read. For repeat scans
/// prefer [`scan_incremental`].
pub fn scan(root: &Path) -> std::io::Result<CatalogInput> {
    let mut paths = Vec::new();
    let mut visited = HashSet::new();
    // `bake` has nowhere to put a report; the incremental scan carries one.
    let mut skips = Skips::default();
    walk(root, &mut paths, &mut visited, &mut skips, 0)?;
    let raw: Vec<RawTrack> = paths.iter().map(|p| extract_raw(p)).collect();
    Ok(build_catalog(&raw, root))
}

/// Scan `root` and bake the `.soulidx` `index.sidx` bytes.
pub fn bake(root: &Path) -> std::io::Result<Vec<u8>> {
    Ok(soul_audio_index::build::write_index(&scan(root)?))
}

// ── Incremental, parallel scan ───────────────────────────────────────────────

/// A file's change-detection identity: modification time + size. Cheap to `stat`
/// and cheap to compare — a mismatch against the cache means "re-read this file".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FileStamp {
    /// Modification time in nanoseconds since the Unix epoch (0 if unavailable).
    pub mtime_ns: u128,
    /// File size in bytes.
    pub size: u64,
}

fn stamp_of(meta: &std::fs::Metadata) -> FileStamp {
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    FileStamp {
        mtime_ns,
        size: meta.len(),
    }
}

/// The reusable scan cache: `path → (stamp, extracted RawTrack)`. An in-memory
/// value (the desktop head persists/reloads it to keep startup warm); `Default`
/// is empty, which makes the next [`scan_incremental`] a cold full scan.
/// Bump when [`RawTrack`]/extraction changes so a persisted cache from an older
/// build is discarded (re-extracted) instead of silently reused with stale fields.
/// The desktop head stores this alongside the serialized cache and compares on load.
/// v4 (multi-disc albums): `RawTrack` gained `disc_total`, `disc_subtitle`,
/// `mb_album_id`; `disc_no` became `Option`.
pub const SCAN_CACHE_VERSION: u32 = 4;

#[derive(Default, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ScanCache {
    entries: HashMap<PathBuf, (FileStamp, RawTrack)>,
}

impl ScanCache {
    /// Number of cached files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    /// Whether the cache is empty (a cold scan).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Which stage of a [`scan_incremental`] a [`ScanProgress`] came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScanPhase {
    /// Walking the tree + `stat`ing files (cheap).
    Discovering,
    /// Opening + reading tags of new/changed files (the parallel, I/O-bound part).
    Extracting,
}

/// Progress for a running [`scan_incremental`]: a monotonic `done/total` within
/// the current [`phase`](ScanProgress::phase). The UI hook hangs off this.
#[derive(Clone, Copy, Debug)]
pub struct ScanProgress {
    pub phase: ScanPhase,
    pub done: usize,
    pub total: usize,
}

/// The result of an incremental scan.
pub struct ScanOutcome {
    /// Ready to bake via [`soul_audio_index::build::write_index`].
    pub input: CatalogInput,
    /// The updated cache — persist/keep it to make the next scan incremental.
    pub cache: ScanCache,
    /// Files new since the prior cache.
    pub added: Vec<PathBuf>,
    /// Files whose [`FileStamp`] changed since the prior cache.
    pub changed: Vec<PathBuf>,
    /// Files present in the prior cache but gone from disk (deletions).
    pub removed: Vec<PathBuf>,
    /// Audio files the scan walked past without importing, grouped by extension
    /// with the reason. Empty for a library this build can fully decode.
    ///
    /// Reported rather than dropped because a silent skip is indistinguishable
    /// from a broken scan: a user whose AAC rips simply do not appear has no way
    /// to learn that the decoder is disabled in this build.
    pub skipped: Vec<SkippedGroup>,
}

/// How often (ms) an over-target worker re-checks whether a concurrency permit has
/// freed up. Short enough that lowering/raising the target takes effect promptly,
/// long enough that idling workers don't busy-spin.
const POOL_POLL_MS: u64 = 5;

/// Run `work(i)` for every `i` in `0..n` across up to `max_threads` scoped workers,
/// but with **at most `target` running at once** — and `target` is re-read *per
/// item*, so the active concurrency follows a limit that changes mid-run (a machine
/// that gets busy can throttle a scan already in flight, and free up again later).
/// Results are placed back in `0..n` order, deterministic regardless of which worker
/// ran which index. `progress(done, total)` is polled from the calling thread.
///
/// The limit is a soft dynamic semaphore: a worker takes a permit (a CAS on a shared
/// active count) only while `active < target`, otherwise it idles [`POOL_POLL_MS`]
/// and re-checks. `target` is clamped to `1..=max_threads`, so there is always at
/// least one runnable worker (no stall) and never more than the ceiling.
fn run_adaptive_pool<T, F>(
    n: usize,
    max_threads: usize,
    target: &AtomicUsize,
    work: F,
    progress: &mut dyn FnMut(usize, usize),
) -> Vec<T>
where
    T: Send,
    F: Fn(usize) -> T + Sync,
{
    if n == 0 {
        return Vec::new();
    }
    let nthreads = max_threads.max(1).min(n);
    let cursor = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let active = AtomicUsize::new(0);
    let mut slots: Vec<Option<T>> = (0..n).map(|_| None).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..nthreads)
            .map(|_| {
                let (cursor, done, active, work) = (&cursor, &done, &active, &work);
                scope.spawn(move || {
                    let mut local: Vec<(usize, T)> = Vec::new();
                    loop {
                        // Acquire a permit: run only while under the live target.
                        loop {
                            let t = target.load(Ordering::Relaxed).clamp(1, nthreads);
                            let a = active.load(Ordering::Relaxed);
                            if a < t {
                                if active
                                    .compare_exchange_weak(
                                        a,
                                        a + 1,
                                        Ordering::Relaxed,
                                        Ordering::Relaxed,
                                    )
                                    .is_ok()
                                {
                                    break; // permit held
                                }
                                // lost the race for the slot — retry immediately
                            } else {
                                std::thread::sleep(std::time::Duration::from_millis(POOL_POLL_MS));
                            }
                        }
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            active.fetch_sub(1, Ordering::Relaxed);
                            break; // no work left — release the permit and finish
                        }
                        local.push((i, work(i)));
                        done.fetch_add(1, Ordering::Relaxed);
                        active.fetch_sub(1, Ordering::Relaxed); // release before re-acquiring
                    }
                    local
                })
            })
            .collect();
        // Report progress from this thread while the workers run.
        loop {
            let d = done.load(Ordering::Relaxed);
            progress(d.min(n), n);
            if d >= n {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        for h in handles {
            for (i, v) in h.join().expect("adaptive pool worker panicked") {
                slots[i] = Some(v);
            }
        }
    });
    slots
        .into_iter()
        .map(|o| o.expect("every index produced a result"))
        .collect()
}

/// Extract `paths` in parallel, honouring a live concurrency `target` (see
/// [`run_adaptive_pool`]). Oversubscribe `max_threads` for the I/O-bound small reads
/// (the OS reorders queued reads by physical location, so this scales on HDD *and*
/// SSD); the `target` throttles how many actually run at once. Results in `paths`
/// order. `progress(done, total)` is polled from the calling thread.
fn parallel_extract(
    paths: &[PathBuf],
    max_threads: usize,
    target: &AtomicUsize,
    progress: &mut dyn FnMut(usize, usize),
) -> Vec<RawTrack> {
    run_adaptive_pool(
        paths.len(),
        max_threads,
        target,
        |i| extract_raw(&paths[i]),
        progress,
    )
}

/// Incrementally scan `root`: walk + `stat`, diff against `prior` (so unchanged
/// files are never re-read), extract only new/changed files **in parallel**,
/// rebuild the [`CatalogInput`], and report deletions. Produces a byte-identical
/// index to [`scan`] for the same tree — it only changes *how much work* is done.
///
/// `threads` sets the extraction parallelism (oversubscribe, e.g. 8–32, for the
/// I/O-bound reads). `progress` fires through the [`Discovering`](ScanPhase::Discovering)
/// then [`Extracting`](ScanPhase::Extracting) phases. For a parallelism that adapts
/// *while the scan runs*, use [`scan_incremental_throttled`].
pub fn scan_incremental(
    root: &Path,
    prior: &ScanCache,
    threads: usize,
    progress: &mut dyn FnMut(ScanProgress),
) -> std::io::Result<ScanOutcome> {
    let threads = threads.max(1);
    scan_incremental_inner(root, prior, threads, &AtomicUsize::new(threads), progress)
}

/// Like [`scan_incremental`], but the extraction parallelism follows a **live**
/// `target` (clamped to `1..=max_threads`) that the caller may change *during* the
/// scan — so a desktop head can throttle an in-flight scan when the machine gets
/// busy and open it back up when it frees. The output is identical to
/// [`scan_incremental`] for the same tree regardless of how `target` moves.
pub fn scan_incremental_throttled(
    root: &Path,
    prior: &ScanCache,
    max_threads: usize,
    target: &AtomicUsize,
    progress: &mut dyn FnMut(ScanProgress),
) -> std::io::Result<ScanOutcome> {
    scan_incremental_inner(root, prior, max_threads, target, progress)
}

fn scan_incremental_inner(
    root: &Path,
    prior: &ScanCache,
    max_threads: usize,
    target: &AtomicUsize,
    progress: &mut dyn FnMut(ScanProgress),
) -> std::io::Result<ScanOutcome> {
    // 1. Discovery: walk (sorted, deterministic) + stat each file.
    let mut paths = Vec::new();
    let mut visited = HashSet::new();
    let mut skips = Skips::default();
    walk(root, &mut paths, &mut visited, &mut skips, 0)?;
    let total = paths.len();
    let mut current: Vec<(PathBuf, FileStamp)> = Vec::with_capacity(total);
    for (i, p) in paths.iter().enumerate() {
        let stamp = std::fs::metadata(p)
            .map(|m| stamp_of(&m))
            .unwrap_or(FileStamp {
                mtime_ns: 0,
                size: 0,
            });
        current.push((p.clone(), stamp));
        progress(ScanProgress {
            phase: ScanPhase::Discovering,
            done: i + 1,
            total,
        });
    }

    // 2. Diff vs the prior cache → new / changed (re-extract) vs unchanged (reuse).
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut to_extract: Vec<PathBuf> = Vec::new();
    for (p, stamp) in &current {
        match prior.entries.get(p) {
            Some((s, _)) if s == stamp => {} // unchanged → reuse the cached RawTrack
            Some(_) => {
                changed.push(p.clone());
                to_extract.push(p.clone());
            }
            None => {
                added.push(p.clone());
                to_extract.push(p.clone());
            }
        }
    }
    let seen: HashSet<&PathBuf> = current.iter().map(|(p, _)| p).collect();
    let removed: Vec<PathBuf> = prior
        .entries
        .keys()
        .filter(|k| !seen.contains(k))
        .cloned()
        .collect();

    // 3. Extract the new/changed files in parallel.
    let fresh = parallel_extract(&to_extract, max_threads, target, &mut |done, total| {
        progress(ScanProgress {
            phase: ScanPhase::Extracting,
            done,
            total,
        });
    });
    let mut fresh_by_path: HashMap<&Path, RawTrack> =
        to_extract.iter().map(PathBuf::as_path).zip(fresh).collect();

    // 4. Assemble the full RawTrack list in walk order (reuse cache or fresh),
    //    rebuild the next cache, and intern into the CatalogInput.
    let mut raw: Vec<RawTrack> = Vec::with_capacity(current.len());
    let mut cache = ScanCache::default();
    for (p, stamp) in &current {
        let rt = fresh_by_path
            .remove(p.as_path())
            .or_else(|| prior.entries.get(p).map(|(_, rt)| rt.clone()))
            .unwrap_or_else(|| extract_raw(p));
        cache.entries.insert(p.clone(), (*stamp, rt.clone()));
        raw.push(rt);
    }
    let input = build_catalog(&raw, root);

    Ok(ScanOutcome {
        input,
        cache,
        added,
        changed,
        removed,
        skipped: skips.into_groups(),
    })
}

#[cfg(all(test, feature = "serde"))]
mod serde_tests {
    use super::{FileStamp, RawTrack, ScanCache};

    /// The persistable cache types derive serde under the `serde` feature (the
    /// desktop head serializes the cache to keep startup warm). A compile-time
    /// assertion — if the derives are wrong this fails to build; the behavioural
    /// round-trip is exercised by the `-ng` head that picks the wire format.
    #[test]
    fn cache_types_are_serializable() {
        fn assert_serde<T: serde::Serialize + serde::de::DeserializeOwned>() {}
        assert_serde::<FileStamp>();
        assert_serde::<RawTrack>();
        assert_serde::<ScanCache>();
    }
}

/// Subdirectory the baked index lives in, at the card/library root.
pub const INDEX_DIR: &str = ".soulidx";
/// The index file inside [`INDEX_DIR`].
pub const INDEX_FILE: &str = "index.sidx";
/// The cover-art blob inside [`INDEX_DIR`].
pub const ART_FILE: &str = "art.blob";

/// Scan `root` and **write** the `.soulidx` to disk: bakes the index + cover
/// art and places them at `out_dir/.soulidx/{index.sidx,art.blob}`, creating
/// the `.soulidx/` directory. This is the "desktop bakes onto the card" step
/// (v1 model = index-the-card): for a DAP export, `out_dir == root == the SD
/// card`. The audio tree is untouched.
///
/// `extended.blob` is deferred (seam reserved).
pub fn export(root: &Path, out_dir: &Path) -> std::io::Result<()> {
    let input = scan(root)?;
    let (art_blob, album_art_id) = soul_audio_index::build::bake_art(
        &input,
        soul_audio_index::build::ART_W,
        soul_audio_index::build::ART_H,
    );
    let bytes = soul_audio_index::build::write_index_with_art(&input, &album_art_id);
    let dir = out_dir.join(INDEX_DIR);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(INDEX_FILE), &bytes)?;
    std::fs::write(dir.join(ART_FILE), &art_blob)
}

#[cfg(test)]
mod date_tests {
    use super::{extract_raw, parse_date};

    #[test]
    fn parse_date_takes_leading_iso_components_with_guards() {
        use soul_audio_catalog::PackedDate;
        assert_eq!(parse_date(Some("2012-05-29")), PackedDate::new(2012, 5, 29));
        assert_eq!(parse_date(Some("1973")), PackedDate::from_year(1973));
        assert_eq!(parse_date(Some("1973-08")), PackedDate::new(1973, 8, 0));
        assert_eq!(parse_date(Some("not a date")), PackedDate::UNKNOWN);
        assert_eq!(parse_date(None), PackedDate::UNKNOWN);
        assert_eq!(parse_date(Some("0000")), PackedDate::UNKNOWN); // year < 1000
        assert_eq!(parse_date(Some("9999")), PackedDate::UNKNOWN); // year > 2200
        assert_eq!(parse_date(Some("2001-13-40")), PackedDate::from_year(2001)); // bad m/d drop
    }

    /// Minimal FLAC bytes: STREAMINFO + a VORBIS_COMMENT (last block) carrying
    /// the given `KEY=VALUE` comments — enough for `extract_raw`'s tag read, no
    /// audio. Mirrors `tests/pipeline.rs`'s `flac_with_comments` helper (kept
    /// local here since `extract_raw` is private to this crate's root module).
    fn flac_with_comments(comments: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"fLaC");
        // STREAMINFO (type 0), 34-byte body, not last.
        v.push(0x00);
        v.extend_from_slice(&[0, 0, 34]);
        let mut si = [0u8; 34];
        let (sr, ch, bps, total): (u32, u32, u32, u64) = (44_100, 2, 16, 88_200);
        si[10] = (sr >> 12) as u8;
        si[11] = (sr >> 4) as u8;
        si[12] =
            (((sr & 0xF) as u8) << 4) | ((((ch - 1) & 0x7) as u8) << 1) | (((bps - 1) >> 4) as u8);
        si[13] = ((((bps - 1) & 0xF) as u8) << 4) | (((total >> 32) & 0xF) as u8);
        si[14] = (total >> 24) as u8;
        si[15] = (total >> 16) as u8;
        si[16] = (total >> 8) as u8;
        si[17] = total as u8;
        v.extend_from_slice(&si);
        // VORBIS_COMMENT (type 4), LAST block (0x80 | 4 = 0x84).
        let mut vc = Vec::new();
        vc.extend_from_slice(&6u32.to_le_bytes());
        vc.extend_from_slice(b"vendor");
        vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
            vc.extend_from_slice(c.as_bytes());
        }
        v.push(0x84);
        v.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]);
        v.extend_from_slice(&vc);
        v
    }

    #[test]
    fn extract_raw_captures_distinct_original_and_release_dates() {
        use soul_audio_catalog::PackedDate;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("soul_import_dates_{}_{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("01.flac");
        std::fs::write(
            &path,
            flac_with_comments(&["DATE=2015", "ORIGINALDATE=1973-08-07"]),
        )
        .unwrap();

        let raw = extract_raw(&path);
        assert_eq!(raw.release_date, PackedDate::from_year(2015));
        assert_eq!(raw.original_date, PackedDate::new(1973, 8, 7));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod pool_tests {
    use super::run_adaptive_pool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A work closure that records peak concurrency: bump an active counter, note
    /// the peak, dwell briefly, release. Returns the item index (so result order is
    /// checkable too).
    fn concurrency_probe<'a>(
        active: &'a AtomicUsize,
        peak: &'a AtomicUsize,
        dwell: Duration,
    ) -> impl Fn(usize) -> usize + Sync + 'a {
        move |i| {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(dwell);
            active.fetch_sub(1, Ordering::SeqCst);
            i
        }
    }

    #[test]
    fn never_exceeds_the_live_target_and_keeps_order() {
        let target = AtomicUsize::new(2);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let n = 200;
        let out = run_adaptive_pool(
            n,
            16, // ceiling well above the target
            &target,
            concurrency_probe(&active, &peak, Duration::from_millis(1)),
            &mut |_, _| {},
        );
        assert_eq!(
            out,
            (0..n).collect::<Vec<_>>(),
            "results come back in order"
        );
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "never more than target=2 ran concurrently (peak {})",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn widens_when_the_target_is_raised_mid_run() {
        let target = AtomicUsize::new(1);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let n = 600;
        std::thread::scope(|s| {
            // Raise the limit partway through — the running pool must pick it up.
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(40));
                target.store(8, Ordering::SeqCst);
            });
            let out = run_adaptive_pool(
                n,
                16,
                &target,
                concurrency_probe(&active, &peak, Duration::from_millis(2)),
                &mut |_, _| {},
            );
            assert_eq!(out, (0..n).collect::<Vec<_>>());
        });
        let peak = peak.load(Ordering::SeqCst);
        assert!(
            peak > 1,
            "concurrency rose above the initial target of 1 after it was raised (peak {peak})"
        );
        assert!(
            peak <= 8,
            "but never above the raised target of 8 (peak {peak})"
        );
    }

    #[test]
    fn narrows_when_the_target_is_lowered_mid_run() {
        let target = AtomicUsize::new(8);
        let active = AtomicUsize::new(0);
        // Record the peak only AFTER the target has been lowered, to prove the pool
        // backs off (not just that it started wide).
        let settled = AtomicUsize::new(0); // 0 until lowered, then 1
        let peak_after = AtomicUsize::new(0);
        let n = 800;
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(40));
                target.store(2, Ordering::SeqCst);
                // Give in-flight items time to drain to the new target before we
                // start sampling the post-lowering peak.
                std::thread::sleep(Duration::from_millis(40));
                settled.store(1, Ordering::SeqCst);
            });
            let probe = |i: usize| {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                if settled.load(Ordering::SeqCst) == 1 {
                    peak_after.fetch_max(now, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(2));
                active.fetch_sub(1, Ordering::SeqCst);
                i
            };
            let out = run_adaptive_pool(n, 16, &target, probe, &mut |_, _| {});
            assert_eq!(out, (0..n).collect::<Vec<_>>());
        });
        let peak_after = peak_after.load(Ordering::SeqCst);
        assert!(
            peak_after <= 2,
            "after the target was lowered to 2, concurrency backed off (peak-after {peak_after})"
        );
    }

    #[test]
    fn empty_input_is_a_no_op() {
        let target = AtomicUsize::new(4);
        let out = run_adaptive_pool(0, 8, &target, |i| i, &mut |_, _| {});
        assert!(out.is_empty());
    }
}

#[cfg(test)]
mod supported_tests {
    use super::*;

    /// A skipped file must be reported with a reason a user can act on, grouped
    /// by extension so a library of thousands yields a sentence, not a list.
    #[test]
    fn skips_are_grouped_by_extension_with_a_reason() {
        let mut skips = Skips::default();
        skips.record(Path::new("a/one.aac"));
        skips.record(Path::new("b/two.AAC"));
        skips.record(Path::new("c/three.wma"));
        // Not audio at all: nothing a music player should mention.
        skips.record(Path::new("cover.jpg"));
        skips.record(Path::new("notes.txt"));

        let mut groups = skips.into_groups();
        groups.sort_by(|a, b| a.extension.cmp(&b.extension));
        assert_eq!(
            groups.len(),
            2,
            "only audio formats are reported: {groups:?}"
        );

        assert_eq!(groups[0].extension, "aac");
        assert_eq!(groups[0].count, 2, "case-insensitive grouping");
        assert_eq!(
            groups[0].reason,
            SkipReason::DecoderDisabled,
            "AAC is skipped because its decoder is gated off, not because the \
             format is unknown — the user can act on that distinction"
        );

        assert_eq!(groups[1].extension, "wma");
        assert_eq!(groups[1].count, 1);
        assert_eq!(groups[1].reason, SkipReason::UnsupportedFormat);
    }

    /// A REAL scan of a directory reports what it passed over, so the app can
    /// show "N files skipped" with a reason instead of silently importing
    /// nothing. The unit tests above pin the grouping; this pins that the
    /// grouping actually reaches the scan's result.
    #[test]
    fn a_real_scan_reports_the_files_it_skipped() {
        let dir = std::env::temp_dir().join(format!("import_skips_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scan dir");
        // Two formats this build cannot play, plus a non-audio file that must
        // never be mentioned.
        std::fs::write(dir.join("a.aac"), b"not really aac").expect("write");
        std::fs::write(dir.join("b.wma"), b"not really wma").expect("write");
        std::fs::write(dir.join("cover.jpg"), b"not audio").expect("write");

        let outcome = scan_incremental(&dir, &ScanCache::default(), 1, &mut |_| {})
            .expect("scan the directory");

        let exts: Vec<&str> = outcome
            .skipped
            .iter()
            .map(|g| g.extension.as_str())
            .collect();
        assert!(
            !exts.contains(&"jpg"),
            "artwork is not audio and must never be reported: {exts:?}"
        );
        assert!(
            exts.contains(&"wma"),
            "a recognised audio format we cannot decode must be reported: {exts:?}"
        );
        assert_eq!(
            exts.contains(&"aac"),
            !soul_audio_core::AAC_ENABLED,
            "an .aac file is reported exactly when AAC is gated off: {exts:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// When AAC IS compiled in, `.aac` is imported and so never skipped. Guards
    /// against a report that keeps accusing a working build of skipping files.
    #[test]
    fn aac_is_only_reported_as_skipped_when_its_decoder_is_off() {
        let mut skips = Skips::default();
        skips.record(Path::new("one.aac"));
        let reported = !skips.into_groups().is_empty();
        assert_eq!(
            reported,
            !soul_audio_core::AAC_ENABLED,
            "an .aac file is reported as skipped exactly when AAC is gated off"
        );
    }

    /// A scan must only list files this BUILD can actually decode.
    ///
    /// AAC is gated behind an opt-in feature (patent), and the extension set is
    /// otherwise keyed purely on the file name — so without this an `.aac` file
    /// is imported into a library, shown to the user, and then fails the moment
    /// they press play. Asserted against `AAC_ENABLED` rather than a fixed
    /// expectation so the test states the invariant in both configurations.
    #[test]
    fn aac_is_listed_only_when_its_decoder_is_compiled_in() {
        assert_eq!(
            is_supported(Path::new("track.aac")),
            soul_audio_core::AAC_ENABLED,
            "an .aac file must be listed exactly when the AAC decoder is built in"
        );
        assert_eq!(
            is_supported(Path::new("TRACK.AAC")),
            soul_audio_core::AAC_ENABLED,
            "the extension check is case-insensitive, so the gate must be too"
        );
    }

    /// `.m4a` stays supported either way: the container carries ALAC as well as
    /// AAC, and ALAC is always available. (An AAC-in-MP4 file therefore remains
    /// listed-but-unplayable — the extension cannot tell them apart.)
    #[test]
    fn m4a_stays_supported_because_it_may_carry_alac() {
        assert!(is_supported(Path::new("track.m4a")));
    }

    /// The rest of the set is unconditional; a gate that swallowed more than AAC
    /// would silently shrink a user's library.
    #[test]
    fn every_other_extension_is_unconditionally_supported() {
        for ext in [
            "wav", "flac", "mp3", "mp2", "mp1", "ogg", "oga", "opus", "aiff", "aif", "dsf", "dff",
            "mka", "webm", "wv", "caf",
        ] {
            assert!(
                is_supported(&Path::new("track").with_extension(ext)),
                "{ext} must stay supported"
            );
        }
    }
}
