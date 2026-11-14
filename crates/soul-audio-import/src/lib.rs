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

/// Scan `root` into a [`CatalogInput`] (the baker's input), reading tags and
/// interning artists/albums/folders with folder-isolation. An unreadable `root`
/// errors; unreadable subtrees/tags degrade (never fail the whole scan).
pub fn scan(root: &Path) -> std::io::Result<CatalogInput> {
    let mut paths = Vec::new();
    let mut visited = HashSet::new();
    walk(root, &mut paths, &mut visited, 0)?;

    let mut interner = Interner::default();
    let mut tracks = Vec::with_capacity(paths.len());

    for path in &paths {
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

        let folder = interner.folder(path.parent().unwrap_or(root), root);
        let artist = interner.artist(&artist_name, tags.artist_sort.as_deref());
        let album_artist = interner.artist(
            &album_artist_name,
            tags.album_artist_sort
                .as_deref()
                .or(tags.artist_sort.as_deref()),
        );
        // Year = leading 4 digits of the date tag (e.g. "2012-05-29" → 2012).
        let year = tags
            .date
            .as_deref()
            .and_then(|d| d.get(0..4))
            .and_then(|y| y.parse::<u16>().ok())
            .unwrap_or(0);
        let album_dir = path.parent().unwrap_or(root);
        let album = interner.album(&album_name, folder, album_artist, year, album_dir, path);

        tracks.push(TrackInput {
            title,
            artist,
            album,
            album_artist,
            // Genre is populated from tags in a later change; untagged for now.
            genre: 0,
            folder,
            track_no: tags.track.unwrap_or(0) as u16,
            disc_no: tags.disc.unwrap_or(1) as u16,
            duration_ms: duration_ms(&facts),
            sample_rate: facts.sample_rate.unwrap_or(0),
            channels: facts.channels.unwrap_or(0) as u8,
            file_name: leaf_name(path),
        });
    }

    let covers: Vec<Option<Vec<u8>>> = interner
        .album_dirs
        .iter()
        .zip(interner.album_tracks.iter())
        .map(|(dir, track)| cover::find_album_cover(dir, track, root))
        .collect();

    Ok(CatalogInput {
        tracks,
        artists: interner.artists,
        artist_sorts: interner.artist_sorts,
        genres: Vec::new(),
        albums: interner.albums,
        folders: interner.folders,
        playlists: Vec::new(),
        covers,
    })
}

/// Scan `root` and bake the `.soulidx` `index.sidx` bytes.
pub fn bake(root: &Path) -> std::io::Result<Vec<u8>> {
    Ok(soul_audio_index::build::write_index(&scan(root)?))
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
