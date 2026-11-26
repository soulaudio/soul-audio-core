//! Planner + executor integration tests for multi-disc albums and side files,
//! over real tagged FLAC fixtures written into temp dirs.

mod support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use soul_audio_organize::{
    Action, EntryKind, OrganizePlan, PlanOptions, SideFilesLeft, Strategy, Template, apply, plan,
    plan_with, preset,
};
use support::{at, audio_files, flac, put, rel, tree, unique_dir};

fn tpl(name: &str) -> Template {
    Template::new(preset(name).expect("preset"))
}

/// A tagged FLAC track of `album` by HOYO-MiX (`disc` = `DISCNUMBER` value).
fn track(album: &str, title: &str, n: u32, disc: Option<&str>) -> Vec<u8> {
    let mut c = vec![
        format!("ALBUM={album}"),
        format!("TITLE={title}"),
        "ARTIST=HOYO-MiX".to_owned(),
        "ALBUMARTIST=HOYO-MiX".to_owned(),
        format!("TRACKNUMBER={n}"),
    ];
    if let Some(d) = disc {
        c.push(format!("DISCNUMBER={d}"));
    }
    let refs: Vec<&str> = c.iter().map(String::as_str).collect();
    flac(&refs)
}

/// The plan's destinations of one kind, relative to `dest`, `/`-separated.
fn dsts(p: &OrganizePlan, dest: &Path, kind: EntryKind) -> BTreeSet<String> {
    p.entries
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| rel(dest, &e.dst))
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// `(album root, reason, file count)` of every root left in place.
fn left(p: &OrganizePlan) -> Vec<(PathBuf, SideFilesLeft, usize)> {
    p.side_files_left
        .iter()
        .map(|l| (l.album_root.clone(), l.reason, l.files))
        .collect()
}

fn opts(src: &Path) -> PlanOptions {
    PlanOptions::new(src)
}

#[test]
fn disc_two_track_one_no_longer_collides_with_disc_one_track_one() {
    let (src, dest) = (unique_dir("collide-src"), unique_dir("collide-dest"));
    put(
        &src,
        "Box/101 Intro.flac",
        &track("Box", "Intro", 1, Some("1/2")),
    );
    put(
        &src,
        "Box/201 Intro.flac",
        &track("Box", "Intro", 1, Some("2/2")),
    );
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&[
            "HOYO-MiX/Box/1-01 - Intro.flac",
            "HOYO-MiX/Box/2-01 - Intro.flac"
        ])
    );
    assert!(
        p.entries.iter().all(|e| e.action == Action::Move),
        "no SkipDuplicate data loss: {:#?}",
        p.entries
    );
}

#[test]
fn single_disc_album_names_are_unchanged() {
    let (src, dest) = (unique_dir("single-src"), unique_dir("single-dest"));
    put(&src, "Single/01.flac", &track("Single", "Solo", 1, None));
    let p = plan(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&["HOYO-MiX/Single/01 - Solo.flac"])
    );
}

#[test]
fn side_files_travel_with_the_album_and_junk_stays() {
    let (src, dest) = (unique_dir("side-src"), unique_dir("side-dest"));
    put(
        &src,
        "Box/101 Intro.flac",
        &track("Box", "Intro", 1, Some("1/2")),
    );
    put(
        &src,
        "Box/201 Reprise.flac",
        &track("Box", "Reprise", 1, Some("2/2")),
    );
    for (f, b) in [
        ("Box/Box.log", &b"EAC log"[..]),
        ("Box/Box.cue", b"FILE \"101 Intro.flac\" WAVE"),
        ("Box/cover.jpg", b"jpg"),
        ("Box/Scans/Front.jpg", b"front"),
        ("Box/info.txt", b"notes"),
        ("Box/Thumbs.db", b"junk"),
        ("Box/desktop.ini", b"junk"),
        ("Box/.DS_Store", b"junk"),
        ("Box/._101 Intro.flac", b"appledouble"),
    ] {
        put(&src, f, b);
    }
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&[
            "HOYO-MiX/Box/Box.cue",
            "HOYO-MiX/Box/Box.log",
            "HOYO-MiX/Box/Scans/Front.jpg",
            "HOYO-MiX/Box/cover.jpg",
            "HOYO-MiX/Box/info.txt",
        ])
    );
    assert!(p.side_files_left.is_empty());
    // Audio first (input order), then the side files.
    let first_side = p
        .entries
        .iter()
        .position(|e| e.kind == EntryKind::SideFile)
        .unwrap();
    assert_eq!(first_side, 2);
    assert!(
        p.entries[first_side..]
            .iter()
            .all(|e| e.kind == EntryKind::SideFile)
    );
}

#[test]
fn lyrics_follow_their_track_and_take_its_new_name() {
    let (src, dest) = (unique_dir("lrc-src"), unique_dir("lrc-dest"));
    put(
        &src,
        "Box/101 Intro.flac",
        &track("Box", "Intro", 1, Some("1/2")),
    );
    put(
        &src,
        "Box/201 Reprise.flac",
        &track("Box", "Reprise", 1, Some("2/2")),
    );
    put(&src, "Box/101 Intro.lrc", b"[00:00.00]Intro");
    put(&src, "Box/201 Reprise.LRC", b"[00:00.00]Reprise");
    let files = audio_files(&src);
    let one = plan_with(
        &files,
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&one, &dest, EntryKind::SideFile),
        set(&[
            "HOYO-MiX/Box/1-01 - Intro.lrc",
            "HOYO-MiX/Box/2-01 - Reprise.LRC"
        ])
    );
    let df = plan_with(
        &files,
        &dest,
        &tpl("artist-album+disc-folders"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&df, &dest, EntryKind::SideFile),
        set(&[
            "HOYO-MiX/Box/Disc 1/01 - Intro.lrc",
            "HOYO-MiX/Box/Disc 2/01 - Reprise.LRC"
        ])
    );
}

#[test]
fn disc_folder_side_files_get_a_disc_prefix_only_when_they_collide() {
    let (src, dest) = (unique_dir("prefix-src"), unique_dir("prefix-dest"));
    put(
        &src,
        "Box/CD1/01 Intro.flac",
        &track("Box", "Intro", 1, Some("1")),
    );
    put(
        &src,
        "Box/CD2/01 Reprise.flac",
        &track("Box", "Reprise", 1, Some("2")),
    );
    put(&src, "Box/CD1/Box.log", b"disc 1 log");
    put(&src, "Box/CD2/Box.log", b"disc 2 log!");
    put(&src, "Box/CD2/notes.txt", b"notes");
    let files = audio_files(&src);
    let one = plan_with(
        &files,
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&one, &dest, EntryKind::SideFile),
        set(&[
            "HOYO-MiX/Box/Disc 1 - Box.log",
            "HOYO-MiX/Box/Disc 2 - Box.log",
            "HOYO-MiX/Box/notes.txt",
        ])
    );
    let df = plan_with(
        &files,
        &dest,
        &tpl("artist-album+disc-folders"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&df, &dest, EntryKind::SideFile),
        set(&[
            "HOYO-MiX/Box/Disc 1/Box.log",
            "HOYO-MiX/Box/Disc 2/Box.log",
            "HOYO-MiX/Box/Disc 2/notes.txt",
        ])
    );
}

#[test]
fn two_albums_in_one_folder_leave_side_files_in_place() {
    let (src, dest) = (unique_dir("shared-src"), unique_dir("shared-dest"));
    put(&src, "Dump/01.flac", &track("A", "a", 1, None));
    put(&src, "Dump/02.flac", &track("B", "b", 1, None));
    put(&src, "Dump/cover.jpg", b"whose cover?");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert_eq!(
        left(&p),
        vec![(src.join("Dump"), SideFilesLeft::SharedAlbumRoot, 1)]
    );
}

#[test]
fn a_nested_album_makes_the_outer_root_ambiguous() {
    let (src, dest) = (unique_dir("nested-src"), unique_dir("nested-dest"));
    put(&src, "Artist/01.flac", &track("Loose", "l", 1, None));
    put(&src, "Artist/Y/01.flac", &track("Y", "y", 1, None));
    put(&src, "Artist/artist.jpg", b"artist photo");
    put(&src, "Artist/Y/cover.jpg", b"y cover");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Y/cover.jpg"])
    );
    assert_eq!(
        // Only the ambiguous root's own files are counted (it is not walked);
        // Y's cover travels with Y.
        left(&p),
        vec![(src.join("Artist"), SideFilesLeft::SharedAlbumRoot, 1)]
    );
    assert_eq!(
        p.side_files_left[0].paths,
        vec![at(&src, "Artist/artist.jpg")]
    );
}

#[test]
fn loose_tracks_in_the_source_root_never_sweep_it() {
    let (src, dest) = (unique_dir("loose-src"), unique_dir("loose-dest"));
    put(&src, "01.flac", &track("Loose", "l", 1, None));
    put(&src, "tax-return.pdf", b"not album material");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert_eq!(
        left(&p),
        vec![(src.clone(), SideFilesLeft::AtSourceRoot, 1)]
    );
}

#[test]
fn flat_layout_leaves_side_files_in_place() {
    let (src, dest) = (unique_dir("flat-src"), unique_dir("flat-dest"));
    put(
        &src,
        "Box/101 Intro.flac",
        &track("Box", "Intro", 1, Some("1/2")),
    );
    put(&src, "Box/cover.jpg", b"jpg");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("flat"),
        Strategy::Copy,
        &opts(&src),
    );
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert_eq!(
        left(&p),
        vec![(src.join("Box"), SideFilesLeft::NoAlbumFolder, 1)]
    );
}

#[test]
fn a_side_file_never_overwrites_a_different_file() {
    let (src, dest) = (unique_dir("conflict-src"), unique_dir("conflict-dest"));
    put(&src, "Single/01.flac", &track("Single", "Solo", 1, None));
    put(&src, "Single/cover.jpg", b"new cover");
    let theirs = put(
        &dest,
        "HOYO-MiX/Single/cover.jpg",
        b"someone else's, longer cover",
    );
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    let cover = p
        .entries
        .iter()
        .find(|e| e.kind == EntryKind::SideFile)
        .unwrap();
    assert_eq!(cover.action, Action::SkipConflict);
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty());
    assert_eq!(
        std::fs::read(&theirs).unwrap(),
        b"someone else's, longer cover"
    );
}

#[test]
fn copy_rerun_is_all_skip_duplicate_and_side_files_are_byte_identical() {
    let (src, dest) = (unique_dir("rerun-src"), unique_dir("rerun-dest"));
    put(
        &src,
        "Box/CD1/01 Intro.flac",
        &track("Box", "Intro", 1, Some("1")),
    );
    put(
        &src,
        "Box/CD2/01 Reprise.flac",
        &track("Box", "Reprise", 1, Some("2")),
    );
    put(&src, "Box/CD1/Box.log", b"Log checksum 0xDEADBEEF");
    put(&src, "Box/cover.jpg", b"jpg");
    let files = audio_files(&src);
    let p = plan_with(
        &files,
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    for e in &p.entries {
        assert_eq!(
            std::fs::read(&e.src).unwrap(),
            std::fs::read(&e.dst).unwrap(),
            "{:?}",
            e.dst
        );
    }
    let again = plan_with(
        &files,
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(again.entries.len(), p.entries.len());
    assert!(
        again
            .entries
            .iter()
            .all(|e| e.action == Action::SkipDuplicate),
        "{:#?}",
        again.entries
    );
    assert!(tree(&dest).contains("HOYO-MiX/Box/Box.log"));
}

/// A tagged FLAC of `album` with an optional MusicBrainz release id.
fn mb_track(album: &str, title: &str, n: u32, mb: Option<&str>) -> Vec<u8> {
    let mut c = vec![
        format!("ALBUM={album}"),
        format!("TITLE={title}"),
        "ALBUMARTIST=HOYO-MiX".to_owned(),
        format!("TRACKNUMBER={n}"),
    ];
    if let Some(id) = mb {
        c.push(format!("MUSICBRAINZ_ALBUMID={id}"));
    }
    let refs: Vec<&str> = c.iter().map(String::as_str).collect();
    flac(&refs)
}

#[test]
fn a_cd0_folder_folds_into_the_album_but_is_no_disc_number() {
    // Mirrors the importer: `CD0` is a disc folder for the album-root rule, but
    // its number is absent as a disc fallback (→ 1), unlike `CD2` (→ 2).
    let (src, dest) = (unique_dir("cd0-src"), unique_dir("cd0-dest"));
    put(
        &src,
        "Box/CD0/01 Intro.flac",
        &track("Box", "Intro", 1, None),
    );
    put(
        &src,
        "Box/CD2/01 Reprise.flac",
        &track("Box", "Reprise", 1, None),
    );
    put(&src, "Box/cover.jpg", b"jpg");
    let t = Template::new("{Album}/{Disc}-{TrackNo} {Title} of {DiscTotal}");
    let p = plan_with(&audio_files(&src), &dest, &t, Strategy::Copy, &opts(&src));
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&["Box/1-01 Intro of 2.flac", "Box/2-01 Reprise of 2.flac"])
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["Box/cover.jpg"])
    );
    assert!(
        p.side_files_left.is_empty(),
        "one album root: {:?}",
        p.side_files_left
    );
}

#[test]
fn a_partially_mb_tagged_release_stays_one_album() {
    // Only one track carries the release id; the untagged one adopts it, so
    // the folder holds ONE album (side files travel, one destination folder).
    let (src, dest) = (unique_dir("mbpart-src"), unique_dir("mbpart-dest"));
    put(&src, "Rel/01.flac", &mb_track("Rel", "a", 1, None));
    put(&src, "Rel/02.flac", &mb_track("Rel", "b", 2, Some("rel-x")));
    put(&src, "Rel/cover.jpg", b"jpg");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&["HOYO-MiX/Rel/01 - a.flac", "HOYO-MiX/Rel/02 - b.flac"])
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Rel/cover.jpg"])
    );
    assert!(p.side_files_left.is_empty(), "{:?}", p.side_files_left);
}
#[test]
fn two_releases_in_one_folder_split_and_untagged_tracks_join_the_first() {
    let (src, dest) = (unique_dir("mbsplit-src"), unique_dir("mbsplit-dest"));
    put(&src, "Rel/01.flac", &mb_track("Rel", "a", 1, Some("rel-x")));
    put(&src, "Rel/02.flac", &mb_track("Rel", "b", 2, Some("rel-y")));
    put(&src, "Rel/03.flac", &mb_track("Rel", "c", 3, None));
    put(&src, "Rel/cover.jpg", b"jpg");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    // Two albums: {01, 03} (release x — 03 adopts the first id) and {02}.
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&[
            "HOYO-MiX/Rel/01 - a.flac",
            "HOYO-MiX/Rel (2)/02 - b.flac",
            "HOYO-MiX/Rel/03 - c.flac"
        ])
    );
    assert_eq!(
        left(&p),
        vec![(src.join("Rel"), SideFilesLeft::SharedAlbumRoot, 1)]
    );
}

/// A `Footprints` track whose bytes differ per `edition` (same album identity
/// tags, different files — two editions of one title).
fn edition_track(title: &str, n: u32, edition: &str) -> Vec<u8> {
    let c = [
        "ALBUM=Footprints".to_owned(),
        format!("TITLE={title}"),
        "ARTIST=HOYO-MiX".to_owned(),
        "ALBUMARTIST=HOYO-MiX".to_owned(),
        format!("TRACKNUMBER={n}"),
        format!("COMMENT={edition}"),
    ];
    let refs: Vec<&str> = c.iter().map(String::as_str).collect();
    flac(&refs)
}

#[test]
fn same_title_distinct_albums_get_numbered_folders_not_merged() {
    let (src, dest) = (unique_dir("editions-src"), unique_dir("editions-dest"));
    let (a, b) = (
        "Footprints of the Traveler",
        "Footprints of the Traveler(Chinese Version)",
    );
    put(
        &src,
        &format!("{a}/01.flac"),
        &edition_track("a", 1, "original"),
    );
    put(
        &src,
        &format!("{a}/02.flac"),
        &edition_track("b", 2, "original"),
    );
    put(&src, &format!("{a}/cover.jpg"), b"original cover");
    put(
        &src,
        &format!("{b}/01.flac"),
        &edition_track("a", 1, "chinese version"),
    );
    put(
        &src,
        &format!("{b}/02.flac"),
        &edition_track("b", 2, "chinese version"),
    );
    put(&src, &format!("{b}/cover.jpg"), b"chinese cover, longer");
    // Input order decides who keeps the plain name: edition A first. (A sorted
    // listing would put `…(Chinese Version)` first — `(` sorts before `/`.)
    let files: Vec<_> = [
        format!("{a}/01.flac"),
        format!("{a}/02.flac"),
        format!("{b}/01.flac"),
        format!("{b}/02.flac"),
    ]
    .iter()
    .map(|r| support::at(&src, r))
    .collect();
    let p = plan_with(
        &files,
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    let got: Vec<(String, String, Action)> = p
        .entries
        .iter()
        .map(|e| (rel(&src, &e.src), rel(&dest, &e.dst), e.action))
        .collect();
    let want: Vec<(String, String, Action)> = [
        (format!("{a}/01.flac"), "HOYO-MiX/Footprints/01 - a.flac"),
        (format!("{a}/02.flac"), "HOYO-MiX/Footprints/02 - b.flac"),
        (
            format!("{b}/01.flac"),
            "HOYO-MiX/Footprints (2)/01 - a.flac",
        ),
        (
            format!("{b}/02.flac"),
            "HOYO-MiX/Footprints (2)/02 - b.flac",
        ),
        (format!("{a}/cover.jpg"), "HOYO-MiX/Footprints/cover.jpg"),
        (
            format!("{b}/cover.jpg"),
            "HOYO-MiX/Footprints (2)/cover.jpg",
        ),
    ]
    .into_iter()
    .map(|(s, d)| (s, d.to_owned(), Action::Copy))
    .collect();
    assert_eq!(got, want);
    assert!(p.side_files_left.is_empty(), "{:?}", p.side_files_left);

    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.copied, 6);
    for e in &p.entries {
        assert_eq!(
            std::fs::read(&e.src).unwrap(),
            std::fs::read(&e.dst).unwrap()
        );
    }

    // Re-plan: same suffix (deterministic input order), everything already there.
    let again = plan_with(
        &files,
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    let again_got: Vec<(String, String, Action)> = again
        .entries
        .iter()
        .map(|e| (rel(&src, &e.src), rel(&dest, &e.dst), e.action))
        .collect();
    let want_again: Vec<(String, String, Action)> = want
        .into_iter()
        .map(|(s, d, _)| (s, d, Action::SkipDuplicate))
        .collect();
    assert_eq!(again_got, want_again);
}

#[test]
fn numbered_folders_follow_disc_folders_and_skip_taken_names() {
    // Disc-folder layout: the suffix goes on the album folder, not `Disc N`.
    // A third album genuinely titled `Footprints (2)` keeps that name, so the
    // second `Footprints` edition takes ` (3)`.
    let (src, dest) = (
        unique_dir("editions-df-src"),
        unique_dir("editions-df-dest"),
    );
    put(&src, "A/CD1/01.flac", &edition_track("a", 1, "one"));
    put(&src, "A/CD2/01.flac", &edition_track("b", 1, "one"));
    put(&src, "B/CD1/01.flac", &edition_track("a", 1, "two"));
    put(&src, "B/CD2/01.flac", &edition_track("b", 1, "two"));
    put(&src, "B/CD2/B.log", b"disc 2 log");
    put(&src, "C/01.flac", &track("Footprints (2)", "c", 1, None));
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album+disc-folders"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&[
            "HOYO-MiX/Footprints/Disc 1/01 - a.flac",
            "HOYO-MiX/Footprints/Disc 2/01 - b.flac",
            "HOYO-MiX/Footprints (3)/Disc 1/01 - a.flac",
            "HOYO-MiX/Footprints (3)/Disc 2/01 - b.flac",
            "HOYO-MiX/Footprints (2)/01 - c.flac",
        ])
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Footprints (3)/Disc 2/B.log"])
    );
    assert!(
        p.entries.iter().all(|e| e.action == Action::Copy),
        "{:#?}",
        p.entries
    );
}

#[test]
fn flat_layout_never_numbers_folders() {
    let (src, dest) = (
        unique_dir("editions-flat-src"),
        unique_dir("editions-flat-dest"),
    );
    put(&src, "A/01.flac", &edition_track("a", 1, "one"));
    put(&src, "B/01.flac", &edition_track("a", 1, "two"));
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("flat"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&["HOYO-MiX - a.flac"])
    );
    assert_eq!(
        p.entries.iter().map(|e| e.action).collect::<Vec<_>>(),
        [Action::Copy, Action::SkipDuplicate]
    );
}

#[test]
fn plan_without_a_source_root_sweeps_and_prunes_nothing() {
    // `plan` has no source root: the album root could be `~/Downloads`, so its
    // other files are reported, never moved, and no folder is pruned.
    let (src, dest) = (unique_dir("noroot-src"), unique_dir("noroot-dest"));
    put(&src, "Box/01 Intro.flac", &track("Box", "Intro", 1, None));
    put(&src, "Box/cover.jpg", b"jpg");
    put(&src, "Box/Box.log", b"log");
    let p = plan(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&["HOYO-MiX/Box/01 - Intro.flac"])
    );
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert!(p.prune_dirs.is_empty(), "{:?}", p.prune_dirs);
    assert_eq!(
        left(&p),
        vec![(src.join("Box"), SideFilesLeft::NoSourceRoot, 2)]
    );
}

#[test]
fn move_with_a_source_root_prunes_inside_it_only() {
    let (src, dest) = (unique_dir("prune-src"), unique_dir("prune-dest"));
    put(
        &src,
        "Box/CD1/01 Intro.flac",
        &track("Box", "Intro", 1, Some("1")),
    );
    put(&src, "Box/cover.jpg", b"jpg");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    assert_eq!(
        p.prune_dirs,
        vec![src.join("Box").join("CD1"), src.join("Box")]
    );
    assert!(p.entries.iter().all(|e| e.action == Action::Move));
}

#[test]
fn an_album_outside_the_source_root_keeps_its_side_files() {
    let (src, other, dest) = (
        unique_dir("outside-src"),
        unique_dir("outside-other"),
        unique_dir("outside-dest"),
    );
    put(&other, "Box/01.flac", &track("Box", "Intro", 1, None));
    put(&other, "Box/cover.jpg", b"jpg");
    let p = plan_with(
        &audio_files(&other),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert!(p.prune_dirs.is_empty(), "{:?}", p.prune_dirs);
    assert_eq!(p.side_files_left[0].reason, SideFilesLeft::NoSourceRoot);
}

#[test]
fn an_input_is_never_also_a_side_file() {
    // An input with a non-audio extension (a ringtone, an extension-less rip)
    // is planned once, as Audio — never a second time as a side file.
    let (src, dest) = (unique_dir("input-src"), unique_dir("input-dest"));
    let a = put(&src, "Box/01 Intro.flac", &track("Box", "Intro", 1, None));
    let b = put(&src, "Box/02 Tone.m4r", &track("Box", "Tone", 2, None));
    let c = put(&src, "Box/03 Raw", &track("Box", "Raw", 3, None));
    put(&src, "Box/cover.jpg", b"jpg");
    let p = plan_with(
        &[a, b, c],
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&[
            "HOYO-MiX/Box/01 - Intro.flac",
            "HOYO-MiX/Box/02 - Tone.m4r",
            "HOYO-MiX/Box/03 - Raw",
        ])
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Box/cover.jpg"])
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
}

#[test]
fn a_stem_followed_lyric_never_shadows_another_side_file() {
    let (src, dest) = (unique_dir("lrcclash-src"), unique_dir("lrcclash-dest"));
    put(
        &src,
        "Box/101 Intro.flac",
        &track("Box", "Intro", 1, Some("1/2")),
    );
    put(&src, "Box/101 Intro.lrc", b"[00:00.00]follows the track");
    put(&src, "Box/1-01 - Intro.lrc", b"[00:00.00]already named so");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    // The lyric that follows the track wins the track's name; the unrelated
    // file that already carried it is numbered.
    let by_src = |name: &str| {
        let e = p
            .entries
            .iter()
            .find(|e| e.src == at(&src, &format!("Box/{name}")))
            .unwrap();
        rel(&dest, &e.dst)
    };
    assert_eq!(by_src("101 Intro.lrc"), "HOYO-MiX/Box/1-01 - Intro.lrc");
    assert_eq!(
        by_src("1-01 - Intro.lrc"),
        "HOYO-MiX/Box/1-01 - Intro (2).lrc"
    );
    assert!(
        p.entries.iter().all(|e| e.action == Action::Copy),
        "{:#?}",
        p.entries
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty());
    assert_eq!(tree(&dest).len(), 3, "every file arrived");
}

#[test]
fn move_prunes_emptied_folders_bottom_up_but_never_the_source_root() {
    let (src, dest) = (unique_dir("prune-src"), unique_dir("prune-dest"));
    put(
        &src,
        "Box/CD1/01 Intro.flac",
        &track("Box", "Intro", 1, Some("1")),
    );
    put(
        &src,
        "Box/CD2/01 Reprise.flac",
        &track("Box", "Reprise", 1, Some("2")),
    );
    put(&src, "Box/CD1/Box.log", b"log");
    put(&src, "Box/cover.jpg", b"jpg");
    put(&src, "Box/Scans/Front.jpg", b"front");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    let order: Vec<String> = p.prune_dirs.iter().map(|d| rel(&src, d)).collect();
    assert_eq!(
        order,
        ["Box/CD1", "Box/CD2", "Box/Scans", "Box"],
        "deepest first"
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.pruned_dirs, 4);
    assert!(src.exists(), "the source root itself is never removed");
    assert!(tree(&src).is_empty());
    assert!(!src.join("Box").exists());
}

#[test]
fn a_resumed_move_still_prunes_folders_emptied_by_the_first_run() {
    let (src, dest) = (unique_dir("resume-src"), unique_dir("resume-dest"));
    put(
        &src,
        "Box/CD1/01 Intro.flac",
        &track("Box", "Intro", 1, None),
    );
    put(&src, "Box/CD1/Box.log", b"log");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    assert!(!p.prune_dirs.is_empty());
    // An interrupted first run: every file moved, the cleanup never ran.
    let mut interrupted = p.clone();
    interrupted.prune_dirs.clear();
    let first = apply(&interrupted, |_| {}).unwrap();
    assert!(first.failed.is_empty(), "{:?}", first.failed);
    assert!(src.join("Box").exists());
    // The re-apply moves nothing (all already there) but still prunes.
    let again = apply(&p, |_| {}).unwrap();
    assert_eq!(again.moved, 0);
    assert_eq!(again.pruned_dirs, 2);
    assert!(!src.join("Box").exists());
}

#[test]
fn move_keeps_a_folder_that_still_holds_junk() {
    let (src, dest) = (unique_dir("junk-src"), unique_dir("junk-dest"));
    put(&src, "Box/101 Intro.flac", &track("Box", "Intro", 1, None));
    put(&src, "Box/Scans/Front.jpg", b"front");
    put(&src, "Box/Thumbs.db", b"junk");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    let s = apply(&p, |_| {}).unwrap();
    assert_eq!(s.pruned_dirs, 1, "only Scans/ emptied");
    assert_eq!(tree(&src), set(&["Box/Thumbs.db"]));
}

#[test]
fn move_with_a_source_root_prunes_emptied_parents_below_it() {
    let (src, dest) = (unique_dir("parents-src"), unique_dir("parents-dest"));
    put(&src, "Artist/Box/01.flac", &track("Box", "a", 1, None));
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    let order: Vec<String> = p.prune_dirs.iter().map(|d| rel(&src, d)).collect();
    assert_eq!(order, ["Artist/Box", "Artist"]);
    let s = apply(&p, |_| {}).unwrap();
    assert_eq!(s.pruned_dirs, 2);
    assert!(src.exists() && !src.join("Artist").exists());
}

#[test]
fn move_without_a_source_root_never_prunes() {
    let (src, dest) = (unique_dir("bound-src"), unique_dir("bound-dest"));
    put(&src, "Lib/Artist/Box/01.flac", &track("Box", "a", 1, None));
    let p = plan(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
    );
    assert!(p.prune_dirs.is_empty(), "no source root, no pruning");
    let s = apply(&p, |_| {}).unwrap();
    assert_eq!(s.moved, 1);
    assert_eq!(s.pruned_dirs, 0);
    assert!(at(&src, "Lib/Artist/Box").exists());
}

#[test]
fn copy_never_prunes() {
    let (src, dest) = (unique_dir("copyprune-src"), unique_dir("copyprune-dest"));
    put(&src, "Box/01.flac", &track("Box", "a", 1, None));
    let before = tree(&src);
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert!(p.prune_dirs.is_empty());
    let s = apply(&p, |_| {}).unwrap();
    assert_eq!(s.pruned_dirs, 0);
    assert_eq!(tree(&src), before);
}

#[test]
fn move_keeps_a_folder_with_a_left_in_place_side_file() {
    let (src, dest) = (unique_dir("left-src"), unique_dir("left-dest"));
    put(&src, "Box/01.flac", &track("Box", "a", 1, None));
    put(&src, "Box/cover.jpg", b"jpg");
    let p = plan(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
    );
    let s = apply(&p, |_| {}).unwrap();
    assert_eq!(s.moved, 1);
    assert_eq!(s.pruned_dirs, 0);
    assert_eq!(tree(&src), set(&["Box/cover.jpg"]));
}

#[test]
fn a_failed_move_keeps_its_folder() {
    let (src, dest) = (unique_dir("fail-src"), unique_dir("fail-dest"));
    put(&src, "Good/01.flac", &track("Good", "a", 1, None));
    put(&src, "Bad/01.flac", &track("Bad", "b", 1, None));
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    // A file where Bad's destination folder must go makes its move fail (the
    // parent cannot be created) while Good/ still moves.
    let bad = p
        .entries
        .iter()
        .find(|e| rel(&src, &e.src) == "Bad/01.flac")
        .unwrap();
    let blocked = bad.dst.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    std::fs::write(&blocked, b"x").unwrap();
    let s = apply(&p, |_| {}).unwrap();
    assert_eq!(s.failed.len(), 1, "{s:?}");
    assert_eq!(s.moved, 1);
    assert!(at(&src, "Bad/01.flac").exists());
    assert!(at(&src, "Bad").exists(), "a failed move keeps its folder");
    assert!(!at(&src, "Good").exists());
}

// --- final review fixes --------------------------------------------------

#[test]
fn side_files_stay_with_audio_that_conflicts_at_the_destination() {
    let (src, dest) = (unique_dir("audioclash-src"), unique_dir("audioclash-dest"));
    put(&src, "Box/01 Intro.flac", &track("Box", "Intro", 1, None));
    put(&src, "Box/01 Intro.lrc", b"[00:00.00]Intro");
    put(&src, "Box/Box.log", b"log");
    put(&src, "Box/Scans/Front.jpg", b"front");
    // A different same-title album already sits at the target path.
    put(
        &dest,
        "HOYO-MiX/Box/01 - Intro.flac",
        b"another album's track",
    );
    let before = tree(&src);
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    assert_eq!(p.entries.len(), 1, "{:#?}", p.entries);
    assert_eq!(p.entries[0].action, Action::SkipConflict);
    assert_eq!(
        left(&p),
        vec![(src.join("Box"), SideFilesLeft::AudioConflict, 3)]
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(tree(&src), before, "nothing moved away from the album");
}

#[test]
fn an_untrusted_album_root_is_never_walked() {
    // No source root: `<tmp>/CD1/x.flac` climbs to `<tmp>` (identity rule), but
    // `<tmp>` may be a drive root — only its own files are listed.
    let (tmp, dest) = (unique_dir("nowalk-src"), unique_dir("nowalk-dest"));
    put(&tmp, "CD1/x.flac", &track("X", "x", 1, None));
    put(&tmp, "readme.txt", b"top-level file");
    let mut deep = String::from("deep");
    for i in 0..12 {
        deep.push_str(&format!("/d{i}"));
        put(&tmp, &format!("{deep}/f.txt"), b"unrelated");
    }
    let p = plan(
        &audio_files(&tmp),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
    );
    assert_eq!(
        left(&p),
        vec![(tmp.clone(), SideFilesLeft::NoSourceRoot, 1)]
    );
    assert_eq!(p.side_files_left[0].paths, vec![tmp.join("readme.txt")]);
}

#[test]
fn an_input_with_a_parent_dir_component_sweeps_and_prunes_nothing() {
    let (src, dest) = (unique_dir("dotdot-src"), unique_dir("dotdot-dest"));
    put(&src, "Box/01 Intro.flac", &track("Box", "Intro", 1, None));
    put(&src, "Box/cover.jpg", b"jpg");
    let input = src.join("Box").join("..").join("Box").join("01 Intro.flac");
    let p = plan_with(
        &[input],
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert!(p.prune_dirs.is_empty(), "{:?}", p.prune_dirs);
    assert_eq!(p.side_files_left[0].reason, SideFilesLeft::NoSourceRoot);
}

/// `Box/CD1` + `Box/CD2` (untagged discs) with a cover, planned against the
/// source root `root(src)`.
#[cfg(windows)]
fn box_against_root(tag: &str, root: impl Fn(&Path) -> PathBuf) {
    let src = unique_dir(&format!("{tag}-src"));
    let dest = unique_dir(&format!("{tag}-dest"));
    put(
        &src,
        "Box/CD1/01 Intro.flac",
        &track("Box", "Intro", 1, None),
    );
    put(
        &src,
        "Box/CD2/01 Reprise.flac",
        &track("Box", "Reprise", 1, None),
    );
    put(&src, "Box/cover.jpg", b"jpg");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &PlanOptions::new(root(&src)),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&[
            "HOYO-MiX/Box/1-01 - Intro.flac",
            "HOYO-MiX/Box/2-01 - Reprise.flac"
        ]),
        "{tag}: one two-disc album"
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Box/cover.jpg"]),
        "{tag}"
    );
    assert!(
        p.side_files_left.is_empty(),
        "{tag}: {:?}",
        p.side_files_left
    );
    assert_eq!(p.prune_dirs.len(), 3, "{tag}: {:?}", p.prune_dirs);
}

#[cfg(windows)]
#[test]
fn a_verbatim_prefixed_source_root_matches_plain_inputs() {
    box_against_root("verbatim", |src| {
        PathBuf::from(format!(r"\\?\{}", src.display()))
    });
}

#[cfg(windows)]
#[test]
fn a_source_root_differing_only_by_case_matches_on_windows() {
    box_against_root("case", |src| {
        PathBuf::from(src.to_str().unwrap().to_uppercase())
    });
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn album_folders_differing_only_by_case_are_numbered() {
    let (src, dest) = (unique_dir("casefold-src"), unique_dir("casefold-dest"));
    put(&src, "One/01.flac", &track("Abc", "a", 1, None));
    put(&src, "One/cover.jpg", b"one");
    put(&src, "Two/01.flac", &track("ABC", "b", 1, None));
    put(&src, "Two/cover.jpg", b"two!");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&["HOYO-MiX/Abc/01 - a.flac", "HOYO-MiX/ABC (2)/01 - b.flac"])
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Abc/cover.jpg", "HOYO-MiX/ABC (2)/cover.jpg"])
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.copied, 4);
}

#[test]
fn a_nested_unselected_album_keeps_its_files() {
    let src = unique_dir("nested-unsel-src");
    let dest = unique_dir("nested-unsel-dest");
    let input = put(&src, "Box/01 Intro.flac", &track("Box", "Intro", 1, None));
    put(&src, "Box/cover.jpg", b"box cover");
    put(
        &src,
        "Box/Bonus Album/01.flac",
        &track("Bonus", "b", 1, None),
    );
    put(&src, "Box/Bonus Album/cover.jpg", b"bonus cover");
    put(
        &src,
        "Box/Extras/Live/01.flac",
        &track("Live", "l", 1, None),
    );
    put(&src, "Box/Extras/notes.txt", b"live notes");
    put(&src, "Box/Scans/Front.jpg", b"front");
    let p = plan_with(
        &[input],
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Box/cover.jpg", "HOYO-MiX/Box/Scans/Front.jpg"])
    );
}

#[test]
fn folder_inferred_discs_are_reported_unless_the_layout_keeps_disc_folders() {
    let src = unique_dir("inferred-src");
    put(
        &src,
        "Suite/Disc 1 - Mondstadt/01 Dawn.flac",
        &track("Suite", "Dawn", 1, None),
    );
    put(
        &src,
        "Suite/Disc 2 - Liyue/01 Night.flac",
        &track("Suite", "Night", 1, None),
    );
    put(
        &src,
        "Tagged/CD1/01 a.flac",
        &track("Tagged", "a", 1, Some("1")),
    );
    put(
        &src,
        "Tagged/CD2/01 b.flac",
        &track("Tagged", "b", 1, Some("2")),
    );
    put(&src, "One/CD1/01 c.flac", &track("One", "c", 1, None));
    for (preset_name, want) in [
        ("artist-album", vec![src.join("Suite")]),
        ("flat", vec![src.join("Suite")]),
        ("artist-album+disc-folders", vec![]),
    ] {
        let dest = unique_dir("inferred-dest");
        let p = plan_with(
            &audio_files(&src),
            &dest,
            &tpl(preset_name),
            Strategy::Copy,
            &opts(&src),
        );
        assert_eq!(p.folder_inferred_discs, want, "{preset_name}");
    }
}

#[test]
fn a_template_without_an_album_folder_plans_no_side_files() {
    let (src, dest) = (unique_dir("noalbumdir-src"), unique_dir("noalbumdir-dest"));
    put(&src, "A/01.flac", &track("A", "a", 1, None));
    put(&src, "A/cover.jpg", b"a");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &Template::new("{Artist}/{Title}"),
        Strategy::Copy,
        &opts(&src),
    );
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert_eq!(
        left(&p),
        vec![(src.join("A"), SideFilesLeft::NoAlbumFolder, 1)]
    );
}

/// A FLAC of `album` by `artist`.
fn by(album: &str, artist: &str, title: &str, n: u32) -> Vec<u8> {
    let c = [
        format!("ALBUM={album}"),
        format!("ARTIST={artist}"),
        format!("TITLE={title}"),
        format!("TRACKNUMBER={n}"),
    ];
    let refs: Vec<&str> = c.iter().map(String::as_str).collect();
    flac(&refs)
}

#[test]
fn a_replan_after_apply_picks_the_same_side_file_names() {
    // `{Album}/{Artist}/…`: the compilation's folder is `Hits/`, the solo
    // album's is `Hits/R/` — where the compilation's `R/cover.jpg` also lands.
    let (src, dest) = (unique_dir("replan-src"), unique_dir("replan-dest"));
    put(&src, "CompA/01.flac", &by("Hits", "P", "p", 1));
    put(&src, "CompA/02.flac", &by("Hits", "Q", "q", 2));
    put(&src, "CompA/R/cover.jpg", b"compilation's R page");
    put(&src, "SoloB/01.flac", &by("Hits", "R", "r", 1));
    put(&src, "SoloB/cover.jpg", b"solo cover");
    let template = Template::new("{Album}/{Artist}/{TrackNo} - {Title}");
    let files = audio_files(&src);
    let first = plan_with(&files, &dest, &template, Strategy::Copy, &opts(&src));
    let side = |p: &OrganizePlan| -> Vec<(String, String)> {
        p.entries
            .iter()
            .filter(|e| e.kind == EntryKind::SideFile)
            .map(|e| (rel(&src, &e.src), rel(&dest, &e.dst)))
            .collect()
    };
    let names = side(&first);
    assert_eq!(
        names,
        [
            ("CompA/R/cover.jpg", "Hits/R/cover.jpg"),
            ("SoloB/cover.jpg", "Hits/R/cover (2).jpg"),
        ]
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
    );
    let s = apply(&first, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    let again = plan_with(&files, &dest, &template, Strategy::Copy, &opts(&src));
    assert_eq!(side(&again), names, "re-plan picks the same names");
    assert!(
        again
            .entries
            .iter()
            .all(|e| e.action == Action::SkipDuplicate),
        "{:#?}",
        again.entries
    );
}

#[test]
fn disc_dir_in_the_file_name_is_no_disc_folder_layout() {
    // `{DiscDir}` only in the file name: every disc lands in one folder, so
    // folder-inferred discs are reported and disc-folder side files go flat.
    let (src, dest) = (unique_dir("dd-name-src"), unique_dir("dd-name-dest"));
    put(
        &src,
        "Suite/Disc 1 - Mondstadt/01 Dawn.flac",
        &track("Suite", "Dawn", 1, None),
    );
    put(
        &src,
        "Suite/Disc 2 - Liyue/01 Night.flac",
        &track("Suite", "Night", 1, None),
    );
    put(&src, "Suite/Disc 2 - Liyue/notes.txt", b"notes");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &Template::new("{AlbumArtist}/{Album}/{DiscDir} {TrackNo} - {Title}"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&[
            "HOYO-MiX/Suite/Disc 1 01 - Dawn.flac",
            "HOYO-MiX/Suite/Disc 2 01 - Night.flac",
        ])
    );
    assert_eq!(p.folder_inferred_discs, vec![src.join("Suite")]);
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Suite/notes.txt"])
    );
}

/// A dated `album` track (DATE=2020) whose bytes differ per `edition`.
fn dated(album: &str, title: &str, n: u32, edition: &str) -> Vec<u8> {
    let c = [
        format!("ALBUM={album}"),
        format!("TITLE={title}"),
        "ARTIST=HOYO-MiX".to_owned(),
        "ALBUMARTIST=HOYO-MiX".to_owned(),
        "DATE=2020".to_owned(),
        format!("TRACKNUMBER={n}"),
        format!("COMMENT={edition}"),
    ];
    let refs: Vec<&str> = c.iter().map(String::as_str).collect();
    flac(&refs)
}

#[test]
fn the_numbered_suffix_goes_on_the_album_folder_not_a_folder_after_it() {
    let (src, dest) = (unique_dir("albseg-src"), unique_dir("albseg-dest"));
    put(&src, "A/01.flac", &dated("Box", "a", 1, "one"));
    put(&src, "A/cover.jpg", b"a cover");
    put(&src, "B/01.flac", &dated("Box", "a", 1, "two"));
    put(&src, "B/cover.jpg", b"b cover!");
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &Template::new("{AlbumArtist}/{Album}/{Year}/{TrackNo} - {Title}"),
        Strategy::Copy,
        &opts(&src),
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&[
            "HOYO-MiX/Box/2020/01 - a.flac",
            "HOYO-MiX/Box (2)/2020/01 - a.flac",
        ])
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&[
            "HOYO-MiX/Box/2020/cover.jpg",
            "HOYO-MiX/Box (2)/2020/cover.jpg",
        ])
    );
    assert!(
        p.entries.iter().all(|e| e.action == Action::Copy),
        "{:#?}",
        p.entries
    );

    // Re-plan after apply: the same numbered folders, everything already there.
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    let again = plan_with(
        &audio_files(&src),
        &dest,
        &Template::new("{AlbumArtist}/{Album}/{Year}/{TrackNo} - {Title}"),
        Strategy::Copy,
        &opts(&src),
    );
    let pairs = |p: &OrganizePlan| -> Vec<(String, String)> {
        p.entries
            .iter()
            .map(|e| (rel(&src, &e.src), rel(&dest, &e.dst)))
            .collect()
    };
    assert_eq!(pairs(&again), pairs(&p));
    assert!(
        again
            .entries
            .iter()
            .all(|e| e.action == Action::SkipDuplicate),
        "{:#?}",
        again.entries
    );
}

#[test]
fn a_partial_selection_leaves_the_album_side_files_in_place() {
    // Select 3 of 10 tracks and Move them: the cover and the log stay with
    // the 7 tracks left behind. Then select the rest: now they all go.
    let (src, dest) = (unique_dir("partial-src"), unique_dir("partial-dest"));
    let all: Vec<PathBuf> = (1..=10)
        .map(|n| {
            put(
                &src,
                &format!("Album/{n:02} t{n}.flac"),
                &track("Album", &format!("t{n}"), n, None),
            )
        })
        .collect();
    put(&src, "Album/cover.jpg", b"cover");
    put(&src, "Album/Album.log", b"EAC log");
    let p = plan_with(
        &all[..3],
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    assert_eq!(dsts(&p, &dest, EntryKind::Audio).len(), 3);
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert_eq!(
        left(&p),
        vec![(src.join("Album"), SideFilesLeft::PartialSelection, 2)]
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert!(at(&src, "Album/cover.jpg").exists());
    assert!(at(&src, "Album/Album.log").exists());

    // The rest of the album: a complete selection sweeps as before.
    let rest = audio_files(&src);
    assert_eq!(rest.len(), 7);
    let p = plan_with(
        &rest,
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &opts(&src),
    );
    assert!(p.side_files_left.is_empty(), "{:?}", p.side_files_left);
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Album/Album.log", "HOYO-MiX/Album/cover.jpg"])
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert!(!src.join("Album").exists(), "emptied album folder pruned");
}

#[test]
fn unselected_audio_in_a_disc_folder_is_a_partial_selection() {
    let (src, dest) = (unique_dir("partial-df-src"), unique_dir("partial-df-dest"));
    let a = put(&src, "Box/CD1/01 a.flac", &track("Box", "a", 1, Some("1")));
    let b = put(&src, "Box/CD1/02 b.flac", &track("Box", "b", 2, Some("1")));
    let c = put(&src, "Box/CD2/01 c.flac", &track("Box", "c", 1, Some("2")));
    put(&src, "Box/cover.jpg", b"cover");
    put(&src, "Box/CD1/Box.log", b"disc 1 log");
    for (inputs, why) in [
        (vec![a.clone()], "a track of CD1 is missing"),
        (vec![a.clone(), b.clone()], "all of CD2 is missing"),
    ] {
        let p = plan_with(
            &inputs,
            &dest,
            &tpl("artist-album"),
            Strategy::Copy,
            &opts(&src),
        );
        assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty(), "{why}");
        assert_eq!(
            left(&p),
            vec![(src.join("Box"), SideFilesLeft::PartialSelection, 2)],
            "{why}"
        );
    }
    // Every disc selected: swept.
    let p = plan_with(
        &[a, b, c],
        &dest,
        &tpl("artist-album"),
        Strategy::Copy,
        &opts(&src),
    );
    assert!(p.side_files_left.is_empty(), "{:?}", p.side_files_left);
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Box/Box.log", "HOYO-MiX/Box/cover.jpg"])
    );
}

/// A `Box` track by H with `DATE=year`.
fn yeared(title: &str, n: u32, year: &str) -> Vec<u8> {
    let c = [
        "ALBUM=Box".to_owned(),
        format!("TITLE={title}"),
        "ALBUMARTIST=H".to_owned(),
        format!("TRACKNUMBER={n}"),
        format!("DATE={year}"),
    ];
    let refs: Vec<&str> = c.iter().map(String::as_str).collect();
    flac(&refs)
}

#[test]
fn a_numbered_album_never_splits_across_numbers() {
    // B shares A's `Box/2020` folder but its 2021 track has `Box/2021` to
    // itself: still all of B goes to `Box (2)`, and so do its side files.
    let (src, dest) = (unique_dir("yr-src"), unique_dir("yr-dest"));
    put(&src, "A/1.flac", &yeared("a", 1, "2020"));
    put(&src, "B/1.flac", &yeared("b", 1, "2020"));
    put(&src, "B/2.flac", &yeared("c", 2, "2021"));
    put(&src, "B/b.log", b"log");
    let t = Template::new("{AlbumArtist}/{Album}/{Year}/{TrackNo} - {Title}");
    let p = plan_with(&audio_files(&src), &dest, &t, Strategy::Copy, &opts(&src));
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&[
            "H/Box/2020/01 - a.flac",
            "H/Box (2)/2020/01 - b.flac",
            "H/Box (2)/2021/02 - c.flac",
        ])
    );
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&["H/Box (2)/b.log"])
    );
    assert!(p.side_files_left.is_empty(), "{:?}", p.side_files_left);

    // Re-plan after apply: the same names, everything already there.
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    let again = plan_with(&audio_files(&src), &dest, &t, Strategy::Copy, &opts(&src));
    let pairs = |p: &OrganizePlan| -> Vec<(String, String)> {
        p.entries
            .iter()
            .map(|e| (rel(&src, &e.src), rel(&dest, &e.dst)))
            .collect()
    };
    assert_eq!(pairs(&again), pairs(&p));
    assert!(
        again
            .entries
            .iter()
            .all(|e| e.action == Action::SkipDuplicate),
        "{:#?}",
        again.entries
    );
}

#[test]
fn an_album_split_over_album_folders_keeps_its_side_files() {
    // `artist-year-album` with per-track dates: `2020 - Box` and `2021 - Box`.
    // No one folder owns the log and the cue — they must not land in `H/`.
    let (src, dest) = (unique_dir("split-src"), unique_dir("split-dest"));
    put(&src, "B/1.flac", &yeared("b", 1, "2020"));
    put(&src, "B/2.flac", &yeared("c", 2, "2021"));
    put(&src, "B/b.log", b"log");
    put(
        &src,
        "B/b.cue",
        b"FILE \"1.flac\" WAVE\nFILE \"2.flac\" WAVE\n",
    );
    for strategy in [Strategy::Copy, Strategy::Move] {
        let p = plan_with(
            &audio_files(&src),
            &dest,
            &tpl("artist-year-album"),
            strategy,
            &opts(&src),
        );
        assert_eq!(
            dsts(&p, &dest, EntryKind::Audio),
            set(&["H/2020 - Box/01 - b.flac", "H/2021 - Box/02 - c.flac"])
        );
        assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
        assert_eq!(
            left(&p),
            vec![(src.join("B"), SideFilesLeft::SplitAlbumFolders, 2)]
        );
        assert!(p.cues_not_rewritten.is_empty());
    }
}

#[test]
fn audio_that_is_never_an_input_makes_the_selection_partial() {
    // `.wma`/`.ape` are audio this build never lists as an input: the album
    // folder still holds part of the album, so its side files stay.
    for other in ["Album/03 c.wma", "Album/04 d.APE"] {
        let (src, dest) = (unique_dir("wma-src"), unique_dir("wma-dest"));
        put(&src, "Album/01 a.flac", &track("Album", "a", 1, None));
        put(&src, other, b"not decodable here");
        put(&src, "Album/cover.jpg", b"cover");
        let p = plan_with(
            &audio_files(&src),
            &dest,
            &tpl("artist-album"),
            Strategy::Move,
            &opts(&src),
        );
        assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty(), "{other}");
        assert_eq!(
            left(&p),
            vec![(src.join("Album"), SideFilesLeft::PartialSelection, 1)],
            "{other}"
        );
        let s = apply(&p, |_| {}).unwrap();
        assert!(s.failed.is_empty(), "{:?}", s.failed);
        assert!(at(&src, other).exists() && at(&src, "Album/cover.jpg").exists());
    }
}

#[test]
fn moving_only_cd1_leaves_the_album_side_files_and_folder() {
    let (src, dest) = (unique_dir("cd1-src"), unique_dir("cd1-dest"));
    let a = put(&src, "Box/CD1/01 a.flac", &track("Box", "a", 1, Some("1")));
    put(&src, "Box/CD2/01 c.flac", &track("Box", "c", 1, Some("2")));
    put(&src, "Box/cover.jpg", b"cover");
    put(&src, "Box/CD1/Box.log", b"disc 1 log");
    let p = plan_with(
        &[a],
        &dest,
        &tpl("artist-album+disc-folders"),
        Strategy::Move,
        &opts(&src),
    );
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert_eq!(
        left(&p),
        vec![(src.join("Box"), SideFilesLeft::PartialSelection, 2)]
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.moved, 1);
    assert_eq!(
        tree(&src),
        set(&["Box/CD1/Box.log", "Box/CD2/01 c.flac", "Box/cover.jpg"]),
        "the log, the cover and CD2 stay; Box/ and Box/CD1/ are not pruned"
    );
}
