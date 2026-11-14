//! Desktop library importer (std).
//!
//! Walks a music tree, reads tags with `soul-audio-tags`, interns names into a
//! dense-id catalog with **folder-isolation** (album identity = parent folder),
//! and bakes a `.soulidx` the DAP reads verbatim. This is the "desktop bakes,
//! device reads" split from the architecture spec — the heavy scan happens here,
//! never on the DAP.
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use soul_audio_catalog::NO_FOLDER;
use soul_audio_index::build::{AlbumMeta, CatalogInput, FolderInput, TrackInput};
use soul_audio_tags::{StreamFacts, Tags, read_owned};

mod cover;

/// Extensions the decoders support (lower-cased comparison).
const SUPPORTED: &[&str] = &[
    "wav", "flac", "mp3", "mp2", "mp1", "m4a", "aac", "ogg", "oga", "opus", "aiff", "aif", "dsf",
    "dff", "mka", "webm", "wv", "caf",
];
/// Last-resort recursion guard against a pathological tree.
const MAX_DEPTH: usize = 64;

fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
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
            if let Err(e) = walk(&path, out, visited, depth + 1) {
                eprintln!("import: skipping unreadable dir {path:?}: {e}");
            }
        } else if is_supported(&path) {
            out.push(path);
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
    // album key = (title, folder id) → folder-isolation.
    album_ids: HashMap<(String, u32), u32>,
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

    /// Intern an album under folder-isolation; fills its metadata on first
    /// sight, along with `dir` (the album's filesystem folder) and
    /// `track_path` (a representative track, for embedded cover-art
    /// discovery) — both recorded only on first sight, same as the metadata.
    fn album(
        &mut self,
        title: &str,
        folder: u32,
        album_artist: u32,
        year: u16,
        dir: &Path,
        track_path: &Path,
    ) -> u32 {
        let key = (title.to_owned(), folder);
        if let Some(&id) = self.album_ids.get(&key) {
            return id;
        }
        let id = self.albums.len() as u32;
        self.albums.push(AlbumMeta {
            title: title.to_owned(),
            album_artist,
            year,
            folder,
        });
        self.album_dirs.push(dir.to_owned());
        self.album_tracks.push(track_path.to_owned());
        self.album_ids.insert(key, id);
        id
    }
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

/// One file's tag-derived metadata, extracted BEFORE interning. This is the
/// *costly* per-file product (a file open + tag/header parse) that the
/// incremental scanner caches by [`FileStamp`] so an unchanged file is never
/// re-read. `path` is the key; the rest mirrors what [`extract_raw`] pulls from
/// `Tags`/`StreamFacts`. `Clone` so the cache can hand copies back; `Send` (all
/// fields are) so extraction can fan out across threads.
#[derive(Clone, Debug, PartialEq)]
pub struct RawTrack {
    path: PathBuf,
    title: String,
    artist_name: String,
    album_artist_name: String,
    album_name: String,
    artist_sort: Option<String>,
    album_artist_sort: Option<String>,
    year: u16,
    genre_name: String,
    track_no: u16,
    disc_no: u16,
    duration_ms: u32,
    sample_rate: u32,
    channels: u8,
    file_name: String,
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
    // Year = leading 4 digits of the date tag (e.g. "2012-05-29" → 2012).
    let year = tags
        .date
        .as_deref()
        .and_then(|d| d.get(0..4))
        .and_then(|y| y.parse::<u16>().ok())
        .unwrap_or(0);
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
        year,
        genre_name,
        track_no: tags.track.unwrap_or(0) as u16,
        disc_no: tags.disc.unwrap_or(1) as u16,
        duration_ms: duration_ms(&facts),
        sample_rate: facts.sample_rate.unwrap_or(0),
        channels: facts.channels.unwrap_or(0) as u8,
        file_name: leaf_name(path),
    }
}

/// Intern a walk-ordered slice of [`RawTrack`]s into a [`CatalogInput`] with
/// folder-isolation, then discover each album's cover. Deterministic: ids follow
/// the input order, so the baked index is stable given a stable walk order (the
/// invariant `scan` and `scan_incremental` share).
fn build_catalog(raw: &[RawTrack], root: &Path) -> CatalogInput {
    let mut interner = Interner::default();
    let mut tracks = Vec::with_capacity(raw.len());
    for r in raw {
        let folder = interner.folder(r.path.parent().unwrap_or(root), root);
        let artist = interner.artist(&r.artist_name, r.artist_sort.as_deref());
        let album_artist = interner.artist(
            &r.album_artist_name,
            r.album_artist_sort.as_deref().or(r.artist_sort.as_deref()),
        );
        let album_dir = r.path.parent().unwrap_or(root);
        let album = interner.album(
            &r.album_name,
            folder,
            album_artist,
            r.year,
            album_dir,
            &r.path,
        );
        let genre = interner.genre(&r.genre_name);
        tracks.push(TrackInput {
            title: r.title.clone(),
            artist,
            album,
            album_artist,
            genre,
            folder,
            track_no: r.track_no,
            disc_no: r.disc_no,
            duration_ms: r.duration_ms,
            sample_rate: r.sample_rate,
            channels: r.channels,
            file_name: r.file_name.clone(),
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
    walk(root, &mut paths, &mut visited, 0)?;
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
#[derive(Default, Clone, Debug)]
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
}

/// Extract `paths` in parallel across `threads` scoped workers (oversubscribe for
/// the I/O-bound small reads — the OS reorders the queued reads by physical
/// location, so this scales on HDD *and* SSD). Work-stealing via an atomic cursor
/// for load balance; results are placed back in `paths` order (deterministic
/// regardless of which worker read which file). `progress(done, total)` is polled
/// from the calling thread while the workers run.
fn parallel_extract(
    paths: &[PathBuf],
    threads: usize,
    progress: &mut dyn FnMut(usize, usize),
) -> Vec<RawTrack> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let n = paths.len();
    if n == 0 {
        return Vec::new();
    }
    let nthreads = threads.max(1).min(n);
    let cursor = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let mut slots: Vec<Option<RawTrack>> = (0..n).map(|_| None).collect();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..nthreads)
            .map(|_| {
                let cursor = &cursor;
                let done = &done;
                scope.spawn(move || {
                    let mut local: Vec<(usize, RawTrack)> = Vec::new();
                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        if i >= paths.len() {
                            break;
                        }
                        let rt = extract_raw(&paths[i]);
                        local.push((i, rt));
                        done.fetch_add(1, Ordering::Relaxed);
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
            for (i, rt) in h.join().expect("extract worker panicked") {
                slots[i] = Some(rt);
            }
        }
    });
    slots
        .into_iter()
        .map(|o| o.expect("every index extracted"))
        .collect()
}

/// Incrementally scan `root`: walk + `stat`, diff against `prior` (so unchanged
/// files are never re-read), extract only new/changed files **in parallel**,
/// rebuild the [`CatalogInput`], and report deletions. Produces a byte-identical
/// index to [`scan`] for the same tree — it only changes *how much work* is done.
///
/// `threads` sets the extraction parallelism (oversubscribe, e.g. 8–32, for the
/// I/O-bound reads). `progress` fires through the [`Discovering`](ScanPhase::Discovering)
/// then [`Extracting`](ScanPhase::Extracting) phases.
pub fn scan_incremental(
    root: &Path,
    prior: &ScanCache,
    threads: usize,
    progress: &mut dyn FnMut(ScanProgress),
) -> std::io::Result<ScanOutcome> {
    // 1. Discovery: walk (sorted, deterministic) + stat each file.
    let mut paths = Vec::new();
    let mut visited = HashSet::new();
    walk(root, &mut paths, &mut visited, 0)?;
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
    let fresh = parallel_extract(&to_extract, threads, &mut |done, total| {
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
    })
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
