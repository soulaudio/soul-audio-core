//! The organize e2e matrix (multi-disc spec, "Testing"): three source layouts ×
//! every preset (one-folder + `+disc-folders` album presets, and `flat`) ×
//! Copy/Move. Each case asserts the exact destination tree (audio + side-file
//! placement + collision prefixes), that nothing but a .cue's FILE lines was
//! modified (the EAC cue names the renamed tracks, the log stays
//! byte-identical), the source left intact (Copy) or pruned (Move), re-run
//! idempotence, and finally that
//! re-importing the destination yields the same albums (organize ∘ import).
//!
//! The matrix plans with a source root (`plan_with`), as the app does. Extra
//! cases: `plan()` without one sweeps and prunes nothing; the album-identity
//! edges (same-title albums, `CD0`, partial / split MusicBrainz tagging)
//! survive the round-trip; existing destination files are never overwritten.

mod support;

use std::collections::BTreeSet;
use std::path::Path;

use soul_audio_organize::{
    Action, EntryKind, PlanOptions, SideFilesLeft, Strategy, Template, apply, plan, plan_with,
    preset,
};
use support::{at, audio_files, flac, mp3, put, rel, tree, unique_dir};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Multi-disc album with disc tags only (`DISCNUMBER=n/2`), all in one folder.
    FlatTagged,
    /// Multi-disc album in `CD1/`/`CD2/` sub-folders, disc-tagged.
    DiscSubfolders,
    /// FlatTagged's album + an album in UNTAGGED `Disc N - Subtitle/` folders.
    Mixed,
}

const PRESETS: [&str; 7] = [
    "artist-album",
    "artist-album+disc-folders",
    "artist-year-album",
    "artist-year-album+disc-folders",
    "genre-artist-album",
    "genre-artist-album+disc-folders",
    "flat",
];

fn track(album: &str, title: &str, n: u32, disc: Option<&str>) -> Vec<u8> {
    let mut c = vec![
        format!("ALBUM={album}"),
        format!("TITLE={title}"),
        "ARTIST=HOYO-MiX".to_owned(),
        "ALBUMARTIST=HOYO-MiX".to_owned(),
        "DATE=2020".to_owned(),
        "GENRE=Game".to_owned(),
        format!("TRACKNUMBER={n}"),
    ];
    if let Some(d) = disc {
        c.push(format!("DISCNUMBER={d}"));
    }
    let refs: Vec<&str> = c.iter().map(String::as_str).collect();
    flac(&refs)
}

/// An EAC-style CRLF cue for two tracks, `FILE "a"` and `FILE "b"`.
fn eac_cue(a: &str, b: &str) -> Vec<u8> {
    format!(
        "REM GENRE Game\r\nREM COMMENT \"ExactAudioCopy v1.6\"\r\nPERFORMER \"HOYO-MiX\"\r\n\
TITLE \"Box\"\r\nFILE \"{a}\" WAVE\r\n  TRACK 01 AUDIO\r\n    INDEX 01 00:00:00\r\n\
FILE \"{b}\" WAVE\r\n  TRACK 02 AUDIO\r\n    INDEX 01 00:00:00\r\n"
    )
    .into_bytes()
}

/// The Box cue's place inside the album folder and its rewritten bytes.
fn expected_cue(layout: Layout, df: bool) -> (&'static str, Vec<u8>) {
    match (layout, df) {
        (_, false) => ("Box.cue", eac_cue("1-01 - Intro.flac", "1-02 - Theme.flac")),
        (Layout::DiscSubfolders, true) => (
            "Disc 1/Box.cue",
            eac_cue("01 - Intro.flac", "02 - Theme.flac"),
        ),
        (_, true) => (
            "Box.cue",
            eac_cue("Disc 1/01 - Intro.flac", "Disc 1/02 - Theme.flac"),
        ),
    }
}

/// A single-disc MP3 album (exercises the ID3 path end to end).
fn single(src: &Path) {
    put(
        src,
        "Single/01 Solo.mp3",
        &mp3(&[
            ("TIT2", "Solo"),
            ("TPE1", "HOYO-MiX"),
            ("TPE2", "HOYO-MiX"),
            ("TALB", "Single"),
            ("TRCK", "1/1"),
            ("TDRC", "2020"),
            ("TCON", "Game"),
        ]),
    );
    put(src, "Single/cover.png", b"png-bytes");
}

/// The flat-tagged two-disc `Box` (with junk that must stay behind).
fn flat_box(src: &Path) {
    put(
        src,
        "Box/101 Intro.flac",
        &track("Box", "Intro", 1, Some("1/2")),
    );
    put(
        src,
        "Box/102 Theme.flac",
        &track("Box", "Theme", 2, Some("1/2")),
    );
    put(
        src,
        "Box/201 Reprise.flac",
        &track("Box", "Reprise", 1, Some("2/2")),
    );
    put(src, "Box/101 Intro.lrc", b"[00:00.00]Intro");
    put(src, "Box/Box.log", b"EAC log, checksum 1234");
    put(
        src,
        "Box/Box.cue",
        &eac_cue("101 Intro.flac", "102 Theme.flac"),
    );
    put(src, "Box/cover.jpg", b"jpg-bytes");
    put(src, "Box/Scans/Front.jpg", b"front-scan");
    put(src, "Box/info.txt", b"notes");
    put(src, "Box/Thumbs.db", b"junk");
    put(src, "Box/.DS_Store", b"junk");
}

fn build_source(layout: Layout, src: &Path) {
    match layout {
        Layout::FlatTagged => flat_box(src),
        Layout::DiscSubfolders => {
            put(
                src,
                "Box/CD1/01 Intro.flac",
                &track("Box", "Intro", 1, Some("1")),
            );
            put(
                src,
                "Box/CD1/02 Theme.flac",
                &track("Box", "Theme", 2, Some("1")),
            );
            put(
                src,
                "Box/CD2/01 Reprise.flac",
                &track("Box", "Reprise", 1, Some("2")),
            );
            put(src, "Box/CD1/01 Intro.lrc", b"[00:00.00]Intro");
            put(src, "Box/CD1/Box.log", b"disc 1 log");
            put(src, "Box/CD2/Box.log", b"disc 2 log!");
            put(
                src,
                "Box/CD1/Box.cue",
                &eac_cue("01 Intro.flac", "02 Theme.flac"),
            );
            put(src, "Box/cover.jpg", b"jpg-bytes");
            put(src, "Box/Scans/Front.jpg", b"front-scan");
            put(src, "Box/info.nfo", b"nfo");
        }
        Layout::Mixed => {
            flat_box(src);
            put(
                src,
                "Suite/Disc 1 - Mondstadt/01 Dawn.flac",
                &track("Suite", "Dawn", 1, None),
            );
            put(
                src,
                "Suite/Disc 1 - Mondstadt/02 Dusk.flac",
                &track("Suite", "Dusk", 2, None),
            );
            put(
                src,
                "Suite/Disc 2 - Liyue/01 Night.flac",
                &track("Suite", "Night", 1, None),
            );
            put(src, "Suite/Disc 1 - Mondstadt/Suite.cue", b"cue 1");
            put(src, "Suite/Disc 2 - Liyue/Suite.cue", b"cue 2");
            put(src, "Suite/Disc 2 - Liyue/01 Night.lrc", b"[00:00.00]Night");
            put(src, "Suite/cover.jpg", b"suite-jpg");
        }
    }
    single(src);
}

fn albums_of(layout: Layout) -> &'static [&'static str] {
    match layout {
        Layout::Mixed => &["Box", "Suite", "Single"],
        _ => &["Box", "Single"],
    }
}

/// Expected files inside one album's destination folder.
fn album_files(layout: Layout, album: &str, df: bool) -> Vec<&'static str> {
    match (album, layout, df) {
        ("Single", _, _) => vec!["01 - Solo.mp3", "cover.png"],
        ("Box", Layout::DiscSubfolders, false) => vec![
            "1-01 - Intro.flac",
            "1-02 - Theme.flac",
            "2-01 - Reprise.flac",
            "1-01 - Intro.lrc",
            "Disc 1 - Box.log",
            "Disc 2 - Box.log",
            "Box.cue",
            "cover.jpg",
            "Scans/Front.jpg",
            "info.nfo",
        ],
        ("Box", Layout::DiscSubfolders, true) => vec![
            "Disc 1/01 - Intro.flac",
            "Disc 1/02 - Theme.flac",
            "Disc 2/01 - Reprise.flac",
            "Disc 1/01 - Intro.lrc",
            "Disc 1/Box.log",
            "Disc 2/Box.log",
            "Disc 1/Box.cue",
            "cover.jpg",
            "Scans/Front.jpg",
            "info.nfo",
        ],
        ("Box", _, false) => vec![
            "1-01 - Intro.flac",
            "1-02 - Theme.flac",
            "2-01 - Reprise.flac",
            "1-01 - Intro.lrc",
            "Box.log",
            "Box.cue",
            "cover.jpg",
            "Scans/Front.jpg",
            "info.txt",
        ],
        ("Box", _, true) => vec![
            "Disc 1/01 - Intro.flac",
            "Disc 1/02 - Theme.flac",
            "Disc 2/01 - Reprise.flac",
            "Disc 1/01 - Intro.lrc",
            "Box.log",
            "Box.cue",
            "cover.jpg",
            "Scans/Front.jpg",
            "info.txt",
        ],
        ("Suite", _, false) => vec![
            "1-01 - Dawn.flac",
            "1-02 - Dusk.flac",
            "2-01 - Night.flac",
            "2-01 - Night.lrc",
            "Disc 1 - Suite.cue",
            "Disc 2 - Suite.cue",
            "cover.jpg",
        ],
        ("Suite", _, true) => vec![
            "Disc 1/01 - Dawn.flac",
            "Disc 1/02 - Dusk.flac",
            "Disc 2/01 - Night.flac",
            "Disc 2/01 - Night.lrc",
            "Disc 1/Suite.cue",
            "Disc 2/Suite.cue",
            "cover.jpg",
        ],
        other => panic!("no expectation for {other:?}"),
    }
}

fn album_dir(preset_name: &str, album: &str) -> String {
    match preset_name.trim_end_matches("+disc-folders") {
        "artist-album" => format!("HOYO-MiX/{album}"),
        "artist-year-album" => format!("HOYO-MiX/2020 - {album}"),
        "genre-artist-album" => format!("Game/HOYO-MiX/{album}"),
        other => panic!("no album folder for {other}"),
    }
}

fn expected_dest(layout: Layout, preset_name: &str) -> BTreeSet<String> {
    if preset_name == "flat" {
        // `{Artist} - {Title}`: audio only — side files have no album folder.
        let mut titles = vec!["Intro.flac", "Theme.flac", "Reprise.flac", "Solo.mp3"];
        if layout == Layout::Mixed {
            titles.extend(["Dawn.flac", "Dusk.flac", "Night.flac"]);
        }
        return titles
            .into_iter()
            .map(|t| format!("HOYO-MiX - {t}"))
            .collect();
    }
    let df = preset_name.ends_with("+disc-folders");
    albums_of(layout)
        .iter()
        .flat_map(|a| {
            let dir = album_dir(preset_name, a);
            album_files(layout, a, df)
                .into_iter()
                .map(move |f| format!("{dir}/{f}"))
        })
        .collect()
}

fn is_audio(rel: &str) -> bool {
    rel.ends_with(".flac") || rel.ends_with(".mp3")
}

fn is_junk(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.starts_with('.') || name == "Thumbs.db"
}

/// The scanned library's albums as a sorted `(title, disc_count)` multiset —
/// two distinct albums sharing a title both appear.
fn albums(root: &Path) -> Vec<(String, u16)> {
    let mut v: Vec<(String, u16)> = soul_audio_import::scan(root)
        .expect("scan")
        .albums
        .iter()
        .map(|a| (a.title.clone(), a.disc_count))
        .collect();
    v.sort();
    v
}

/// `albums` with `title`'s disc count replaced by `count` (the folder-inferred
/// disc limitation: the organizer never writes tags).
fn with_disc_count(mut v: Vec<(String, u16)>, title: &str, count: u16) -> Vec<(String, u16)> {
    for a in v.iter_mut().filter(|a| a.0 == title) {
        a.1 = count;
    }
    v.sort();
    v
}

fn run_case(layout: Layout, preset_name: &str, strategy: Strategy) {
    let tag = format!("{layout:?}-{}-{strategy:?}", preset_name.replace('+', "_"));
    let src = unique_dir(&format!("{tag}-src"));
    let dest = unique_dir(&format!("{tag}-dest"));
    build_source(layout, &src);
    let initial = tree(&src);
    let before = albums(&src);
    let files = audio_files(&src);
    let template = Template::new(preset(preset_name).expect("preset"));
    let opts = PlanOptions::new(&src);
    let df = preset_name.ends_with("+disc-folders");

    // Plan: every entry is a fresh write.
    let p = plan_with(&files, &dest, &template, strategy, &opts);
    let want = match strategy {
        Strategy::Copy => Action::Copy,
        Strategy::Move => Action::Move,
    };
    assert!(
        p.entries.iter().all(|e| e.action == want),
        "{tag}: every entry {want:?}: {:#?}",
        p.entries
    );
    // Suite's discs come only from its `Disc N - …` folders: a layout without
    // `{DiscDir}` flattens them, and the plan says so.
    let inferred = layout == Layout::Mixed && !preset_name.ends_with("+disc-folders");
    assert_eq!(
        p.folder_inferred_discs,
        if inferred {
            vec![src.join("Suite")]
        } else {
            Vec::new()
        },
        "{tag}"
    );
    // Side files: all planned (album presets) / all left, reported (flat).
    if preset_name == "flat" {
        assert!(
            p.entries.iter().all(|e| e.kind == EntryKind::Audio),
            "{tag}"
        );
        assert_eq!(
            p.side_files_left.len(),
            albums_of(layout).len(),
            "{tag}: {:?}",
            p.side_files_left
        );
        assert!(
            p.side_files_left
                .iter()
                .all(|l| l.reason == SideFilesLeft::NoAlbumFolder),
            "{tag}: {:?}",
            p.side_files_left
        );
    } else {
        assert!(
            p.side_files_left.is_empty(),
            "{tag}: {:?}",
            p.side_files_left
        );
    }
    // What each destination must hold: the source bytes, or — a rewritten
    // cue — the plan's rewritten bytes.
    let snapshot: Vec<(std::path::PathBuf, Vec<u8>)> = p
        .entries
        .iter()
        .map(|e| {
            let bytes = match &e.rewritten {
                Some(b) => b.clone(),
                None => std::fs::read(&e.src).unwrap(),
            };
            (e.dst.clone(), bytes)
        })
        .collect();

    // Cue sheets: only the Box cue names tracks (the Suite cues name none),
    // and it is rewritten; every .log stays a plain, byte-identical copy.
    assert!(
        p.cues_not_rewritten.is_empty(),
        "{tag}: {:?}",
        p.cues_not_rewritten
    );
    let rewritten: Vec<String> = p
        .entries
        .iter()
        .filter(|e| e.rewritten.is_some())
        .map(|e| rel(&dest, &e.dst))
        .collect();
    if preset_name == "flat" {
        assert!(rewritten.is_empty(), "{tag}: {rewritten:?}");
    } else {
        let (cue_rel, _) = expected_cue(layout, df);
        assert_eq!(
            rewritten,
            vec![format!("{}/{cue_rel}", album_dir(preset_name, "Box"))],
            "{tag}"
        );
    }
    assert!(
        p.entries
            .iter()
            .filter(|e| e.src.extension().is_some_and(|x| x == "log"))
            .all(|e| e.rewritten.is_none()),
        "{tag}: logs are never modified"
    );

    // Apply.
    let summary = apply(&p, |_| {}).expect("apply");
    assert!(summary.failed.is_empty(), "{tag}: {:?}", summary.failed);
    assert_eq!(summary.copied + summary.moved, p.entries.len(), "{tag}");

    // 1. The exact destination tree.
    assert_eq!(
        tree(&dest),
        expected_dest(layout, preset_name),
        "{tag}: destination tree"
    );
    // 2. Nothing else was modified on the way (audio and side files byte for byte;
    // a rewritten cue holds exactly the planned bytes).
    for (dst, bytes) in &snapshot {
        assert_eq!(
            &std::fs::read(dst).unwrap(),
            bytes,
            "{tag}: {dst:?} modified"
        );
    }
    // 2b. The EAC cue names the renamed tracks, relative to its new folder.
    if preset_name != "flat" {
        let (cue_rel, cue_bytes) = expected_cue(layout, df);
        let dir = album_dir(preset_name, "Box");
        assert_eq!(
            std::fs::read(at(&dest, &format!("{dir}/{cue_rel}"))).unwrap(),
            cue_bytes,
            "{tag}: rewritten cue"
        );
    }
    // 3. The source: untouched (Copy) / only what must stay (Move).
    match strategy {
        Strategy::Copy => {
            assert_eq!(tree(&src), initial, "{tag}: Copy leaves the source intact");
            assert_eq!(summary.pruned_dirs, 0, "{tag}: Copy never prunes");
        }
        Strategy::Move => {
            let left: BTreeSet<String> = if preset_name == "flat" {
                initial.iter().filter(|r| !is_audio(r)).cloned().collect()
            } else {
                initial.iter().filter(|r| is_junk(r)).cloned().collect()
            };
            assert_eq!(tree(&src), left, "{tag}: Move leaves only what it must");
            assert!(src.exists(), "{tag}: the source root is never pruned");
            if preset_name != "flat" {
                assert!(summary.pruned_dirs > 0, "{tag}");
                assert!(
                    !src.join("Single").exists(),
                    "{tag}: emptied album folder pruned"
                );
                assert!(
                    !src.join("Box").join("Scans").exists(),
                    "{tag}: Scans/ pruned"
                );
                assert!(!src.join("Suite").exists(), "{tag}: Suite/ pruned");
                assert_eq!(
                    src.join("Box").exists(),
                    layout != Layout::DiscSubfolders,
                    "{tag}: Box/ survives only when it still holds junk"
                );
            }
        }
    }
    // 4. Idempotent re-run.
    match strategy {
        Strategy::Copy => {
            let again = plan_with(&files, &dest, &template, strategy, &opts);
            assert_eq!(again.entries.len(), p.entries.len(), "{tag}");
            assert!(
                again
                    .entries
                    .iter()
                    .all(|e| e.action == Action::SkipDuplicate),
                "{tag}: re-run is all SkipDuplicate: {:#?}",
                again.entries
            );
            assert_eq!(apply(&again, |_| {}).unwrap().copied, 0, "{tag}");
        }
        Strategy::Move => {
            let s = apply(&p, |_| {}).unwrap();
            assert_eq!(
                (s.moved, s.skipped),
                (0, p.entries.len()),
                "{tag}: re-apply moves nothing"
            );
            assert!(s.failed.is_empty(), "{tag}");
        }
    }
    assert_eq!(
        tree(&dest),
        expected_dest(layout, preset_name),
        "{tag}: re-run"
    );
    // 5. organize ∘ import: the same albums, the same disc counts — except
    // (spec correction 5) that an untagged album's folder-inferred disc numbers
    // survive only into a `+disc-folders` layout (the organizer never writes
    // tags): the Mixed `Suite` re-imports as one disc otherwise.
    assert_eq!(
        before.len(),
        albums_of(layout).len(),
        "{tag}: source albums"
    );
    let after = albums(&dest);
    assert_eq!(after.len(), before.len(), "{tag}: album count");
    let want = if layout == Layout::Mixed && !preset_name.ends_with("+disc-folders") {
        assert!(
            before.contains(&("Suite".to_owned(), 2)),
            "{tag}: {before:?}"
        );
        with_disc_count(before.clone(), "Suite", 1)
    } else {
        before.clone()
    };
    assert_eq!(after, want, "{tag}: re-import");

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dest);
}

fn run_all(layout: Layout, strategy: Strategy) {
    for p in PRESETS {
        run_case(layout, p, strategy);
    }
}

#[test]
fn flat_tagged_copy() {
    run_all(Layout::FlatTagged, Strategy::Copy);
}
#[test]
fn flat_tagged_move() {
    run_all(Layout::FlatTagged, Strategy::Move);
}
#[test]
fn disc_subfolders_copy() {
    run_all(Layout::DiscSubfolders, Strategy::Copy);
}
#[test]
fn disc_subfolders_move() {
    run_all(Layout::DiscSubfolders, Strategy::Move);
}
#[test]
fn mixed_copy() {
    run_all(Layout::Mixed, Strategy::Copy);
}
#[test]
fn mixed_move() {
    run_all(Layout::Mixed, Strategy::Move);
}

/// `plan()` (no source root): audio only — every album's side files are
/// reported `NoSourceRoot` and stay, and a Move prunes nothing.
#[test]
fn plan_without_a_source_root_moves_audio_only() {
    let src = unique_dir("noroot-src");
    let dest = unique_dir("noroot-dest");
    build_source(Layout::Mixed, &src);
    let initial = tree(&src);
    let before = albums(&src);
    let p = plan(
        &audio_files(&src),
        &dest,
        &Template::new(preset("artist-album").unwrap()),
        Strategy::Move,
    );
    assert!(p.entries.iter().all(|e| e.kind == EntryKind::Audio));
    assert!(p.prune_dirs.is_empty(), "{:?}", p.prune_dirs);
    let mut left: Vec<(String, SideFilesLeft, usize)> = p
        .side_files_left
        .iter()
        .map(|l| (support::rel(&src, &l.album_root), l.reason, l.files))
        .collect();
    left.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        left,
        [
            // Only each album root's own files (no source root: nothing is
            // walked) — Box/Scans/ and Suite's disc folders are not counted.
            ("Box".to_owned(), SideFilesLeft::NoSourceRoot, 5),
            ("Single".to_owned(), SideFilesLeft::NoSourceRoot, 1),
            ("Suite".to_owned(), SideFilesLeft::NoSourceRoot, 1),
        ]
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.pruned_dirs, 0);
    let audio: BTreeSet<String> = expected_dest(Layout::Mixed, "artist-album")
        .into_iter()
        .filter(|r| is_audio(r))
        .collect();
    assert_eq!(tree(&dest), audio, "audio only");
    let non_audio: BTreeSet<String> = initial.into_iter().filter(|r| !is_audio(r)).collect();
    assert_eq!(tree(&src), non_audio, "every side file stays in place");
    assert!(src.join("Suite").join("Disc 1 - Mondstadt").is_dir());
    // The albums still round-trip (Suite's folder-inferred discs flatten).
    assert_eq!(albums(&dest), with_disc_count(before, "Suite", 1));

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dest);
}

/// A FLAC with an optional MusicBrainz release id and an `edition` marker
/// (different bytes for otherwise identical tags).
fn id_track(album: &str, title: &str, n: u32, mb: Option<&str>, edition: &str) -> Vec<u8> {
    let mut c = vec![
        format!("ALBUM={album}"),
        format!("TITLE={title}"),
        "ARTIST=HOYO-MiX".to_owned(),
        "ALBUMARTIST=HOYO-MiX".to_owned(),
        format!("TRACKNUMBER={n}"),
        format!("COMMENT={edition}"),
    ];
    if let Some(id) = mb {
        c.push(format!("MUSICBRAINZ_ALBUMID={id}"));
    }
    let refs: Vec<&str> = c.iter().map(String::as_str).collect();
    flac(&refs)
}

/// The album-identity edges survive organize ∘ import: distinct same-title
/// albums (→ `Footprints (2)`), a `CD0` folder (folds in, disc fallback 1), a
/// partially MB-tagged release (one album) and two releases sharing a folder
/// (split; the untagged track joins the first-seen release).
#[test]
fn identity_edges_round_trip() {
    for preset_name in ["artist-album", "artist-album+disc-folders"] {
        let df = preset_name.ends_with("+disc-folders");
        let tag = preset_name.replace('+', "_");
        let src = unique_dir(&format!("edges-{tag}-src"));
        let dest = unique_dir(&format!("edges-{tag}-dest"));
        // Same title, two folders (editions); input order: `A/` before `B/`.
        put(
            &src,
            "A/Footprints/01.flac",
            &id_track("Footprints", "a", 1, None, "orig"),
        );
        put(&src, "A/Footprints/cover.jpg", b"orig cover");
        put(
            &src,
            "B/Footprints/01.flac",
            &id_track("Footprints", "a", 1, None, "cn"),
        );
        put(&src, "B/Footprints/cover.jpg", b"cn cover, longer");
        // `CD0` + `CD2`, untagged discs.
        put(
            &src,
            "Zero/CD0/01 Intro.flac",
            &id_track("Zero", "Intro", 1, None, "z"),
        );
        put(
            &src,
            "Zero/CD2/01 Reprise.flac",
            &id_track("Zero", "Reprise", 1, None, "z"),
        );
        // Partial MB tagging: one album.
        put(&src, "Part/01.flac", &id_track("Part", "a", 1, None, "p"));
        put(
            &src,
            "Part/02.flac",
            &id_track("Part", "b", 2, Some("rel-p"), "p"),
        );
        // Two releases in one folder: {01, 03} and {02}.
        put(
            &src,
            "Split/01.flac",
            &id_track("Split", "a", 1, Some("rel-x"), "s"),
        );
        put(
            &src,
            "Split/02.flac",
            &id_track("Split", "b", 2, Some("rel-y"), "s"),
        );
        put(&src, "Split/03.flac", &id_track("Split", "c", 3, None, "s"));
        let before = albums(&src);
        assert_eq!(
            before,
            [
                ("Footprints".to_owned(), 1),
                ("Footprints".to_owned(), 1),
                ("Part".to_owned(), 1),
                ("Split".to_owned(), 1),
                ("Split".to_owned(), 1),
                ("Zero".to_owned(), 2),
            ],
            "{tag}: source"
        );

        let p = plan_with(
            &audio_files(&src),
            &dest,
            &Template::new(preset(preset_name).unwrap()),
            Strategy::Copy,
            &PlanOptions::new(&src),
        );
        let s = apply(&p, |_| {}).unwrap();
        assert!(s.failed.is_empty(), "{tag}: {:?}", s.failed);
        let zero: &[&str] = if df {
            &["Disc 1/01 - Intro.flac", "Disc 2/01 - Reprise.flac"]
        } else {
            &["1-01 - Intro.flac", "2-01 - Reprise.flac"]
        };
        let want: BTreeSet<String> = [
            "Footprints/01 - a.flac",
            "Footprints/cover.jpg",
            "Footprints (2)/01 - a.flac",
            "Footprints (2)/cover.jpg",
            "Part/01 - a.flac",
            "Part/02 - b.flac",
            "Split/01 - a.flac",
            "Split/03 - c.flac",
            "Split (2)/02 - b.flac",
        ]
        .into_iter()
        .map(|r| format!("HOYO-MiX/{r}"))
        .chain(zero.iter().map(|z| format!("HOYO-MiX/Zero/{z}")))
        .collect();
        assert_eq!(tree(&dest), want, "{tag}: destination tree");
        assert_eq!(
            std::fs::read(support::at(&dest, "HOYO-MiX/Footprints (2)/cover.jpg")).unwrap(),
            b"cn cover, longer",
            "{tag}: each edition keeps its own cover"
        );

        // Re-import: the same albums; `Zero`'s discs came from `CD0`/`CD2`
        // folders only, so they survive only into `+disc-folders`.
        let want = if df {
            before.clone()
        } else {
            with_disc_count(before.clone(), "Zero", 1)
        };
        assert_eq!(albums(&dest), want, "{tag}: re-import");

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }
}

/// Existing destination files are never overwritten: a different-size file is
/// a SkipConflict and survives byte for byte; a same-size one is a duplicate.
#[test]
fn existing_destination_files_are_never_overwritten() {
    for strategy in [Strategy::Copy, Strategy::Move] {
        let src = unique_dir(&format!("noclobber-{strategy:?}-src"));
        let dest = unique_dir(&format!("noclobber-{strategy:?}-dest"));
        flat_box(&src);
        let theirs = put(
            &dest,
            "HOYO-MiX/Box/cover.jpg",
            b"someone else's, longer cover",
        );
        put(
            &dest,
            "HOYO-MiX/Box/1-01 - Intro.flac",
            &track("Box", "Intro", 1, Some("1/2")),
        );
        let p = plan_with(
            &audio_files(&src),
            &dest,
            &Template::new(preset("artist-album").unwrap()),
            strategy,
            &PlanOptions::new(&src),
        );
        let action = |rel: &str| {
            let want = support::at(&dest, rel);
            p.entries.iter().find(|e| e.dst == want).unwrap().action
        };
        assert_eq!(action("HOYO-MiX/Box/cover.jpg"), Action::SkipConflict);
        assert_eq!(
            action("HOYO-MiX/Box/1-01 - Intro.flac"),
            Action::SkipDuplicate
        );
        let s = apply(&p, |_| {}).unwrap();
        assert!(s.failed.is_empty(), "{strategy:?}: {:?}", s.failed);
        assert_eq!(
            std::fs::read(&theirs).unwrap(),
            b"someone else's, longer cover",
            "{strategy:?}"
        );
        // The skipped sources stay where they are (Move removes nothing it did
        // not place).
        assert!(src.join("Box").join("cover.jpg").is_file(), "{strategy:?}");
        assert!(
            src.join("Box").join("101 Intro.flac").is_file(),
            "{strategy:?}"
        );
        // Everything else was placed.
        assert_eq!(
            s.copied + s.moved,
            p.entries.len() - 2,
            "{strategy:?}: {:#?}",
            p.entries
        );

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }
}

/// `Disc CD 0N <subtitle>` folders (doubled prefix, CJK subtitles) are disc
/// folders: one album, one destination folder — never "Album (2)".
#[test]
fn doubled_prefix_disc_folders_plan_into_one_album_folder() {
    let src = unique_dir("cdcd-src");
    let dest = unique_dir("cdcd-dest");
    for (n, sub) in [
        (1, "Sakura and Violet Thunder  稻光与雷樱的土地"),
        (2, "Stories of the Floating World 浮世之枯荣"),
        (3, "Battles of Inazuma 稻妻征战记"),
    ] {
        put(
            &src,
            &format!("Inazuma/Disc CD 0{n} {sub}/01 Song.flac"),
            &track("Inazuma", &format!("Song {n}"), 1, Some(&format!("{n}/3"))),
        );
    }
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &Template::new(preset("artist-album").unwrap()),
        Strategy::Copy,
        &PlanOptions::new(&src),
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    let out = tree(&dest);
    assert_eq!(out.len(), 3, "{out:?}");
    assert!(
        out.iter().all(|r| r.starts_with("HOYO-MiX/Inazuma/")),
        "{out:?}"
    );
    assert!(!out.iter().any(|r| r.contains("(2)")), "{out:?}");
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dest);
}
