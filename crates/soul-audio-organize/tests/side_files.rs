//! Planner + executor integration tests for multi-disc albums and side files,
//! over real tagged FLAC fixtures written into temp dirs.

mod support;

use std::collections::BTreeSet;
use std::path::Path;

use soul_audio_organize::{
    Action, EntryKind, LeftInPlace, OrganizePlan, PlanOptions, SideFilesLeft, Strategy, Template,
    apply, plan, plan_with, preset,
};
use support::{audio_files, flac, put, rel, tree, unique_dir};

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

fn opts(src: &Path) -> PlanOptions {
    PlanOptions {
        source_root: Some(src.to_path_buf()),
    }
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
        p.side_files_left,
        vec![LeftInPlace {
            album_root: src.join("Dump"),
            reason: SideFilesLeft::SharedAlbumRoot,
            files: 1,
        }]
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
        p.side_files_left,
        vec![LeftInPlace {
            album_root: src.join("Artist"),
            reason: SideFilesLeft::SharedAlbumRoot,
            files: 2,
        }]
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
        p.side_files_left,
        vec![LeftInPlace {
            album_root: src.clone(),
            reason: SideFilesLeft::AtSourceRoot,
            files: 1,
        }]
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
        p.side_files_left,
        vec![LeftInPlace {
            album_root: src.join("Box"),
            reason: SideFilesLeft::NoAlbumFolder,
            files: 1,
        }]
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
        p.side_files_left,
        vec![LeftInPlace {
            album_root: src.join("Rel"),
            reason: SideFilesLeft::SharedAlbumRoot,
            files: 1,
        }]
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
