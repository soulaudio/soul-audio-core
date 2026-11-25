//! `.cue` `FILE` rewriting through the planner and apply: the cue's planned
//! copy names the renamed tracks, relative to the cue's new folder; everything
//! else stays byte-identical; cues that cannot be rewritten safely are copied
//! unchanged and reported.

mod support;

use std::path::Path;

use soul_audio_organize::{
    Action, CueNotRewritten, EntryKind, OrganizePlan, PlanEntry, PlanOptions, Strategy, Template,
    apply, plan_with, preset,
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

/// An EAC-style CRLF cue for two tracks, `FILE "a"` and `FILE "b"`.
fn eac_cue(a: &str, b: &str) -> Vec<u8> {
    format!(
        "REM GENRE Game\r\nREM COMMENT \"ExactAudioCopy v1.6\"\r\nPERFORMER \"HOYO-MiX\"\r\n\
TITLE \"Box\"\r\nFILE \"{a}\" WAVE\r\n  TRACK 01 AUDIO\r\n    INDEX 01 00:00:00\r\n\
FILE \"{b}\" WAVE\r\n  TRACK 02 AUDIO\r\n    INDEX 01 00:00:00\r\n"
    )
    .into_bytes()
}

const LOG: &[u8] = b"EAC log, checksum 1234";

/// `Box`: disc 1 = 101/102, disc 2 = 201, all in one folder, with a log and a
/// cue for disc 1.
fn eac_box(src: &Path) {
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
    put(src, "Box/Box.log", LOG);
    put(
        src,
        "Box/Box.cue",
        &eac_cue("101 Intro.flac", "102 Theme.flac"),
    );
}

fn entry<'a>(p: &'a OrganizePlan, src: &Path) -> &'a PlanEntry {
    p.entries
        .iter()
        .find(|e| e.src == src)
        .unwrap_or_else(|| panic!("no entry for {src:?}: {:#?}", p.entries))
}

fn plan_box(src: &Path, dest: &Path, preset_name: &str, strategy: Strategy) -> OrganizePlan {
    plan_with(
        &audio_files(src),
        dest,
        &tpl(preset_name),
        strategy,
        &PlanOptions::new(src),
    )
}

#[test]
fn the_cue_names_the_renamed_tracks_in_both_layouts() {
    for (preset_name, a, b) in [
        ("artist-album", "1-01 - Intro.flac", "1-02 - Theme.flac"),
        (
            "artist-album+disc-folders",
            "Disc 1/01 - Intro.flac",
            "Disc 1/02 - Theme.flac",
        ),
    ] {
        let (src, dest) = (unique_dir("cue-src"), unique_dir("cue-dest"));
        eac_box(&src);
        let p = plan_box(&src, &dest, preset_name, Strategy::Copy);
        let cue = entry(&p, &at(&src, "Box/Box.cue"));
        assert_eq!(
            rel(&dest, &cue.dst),
            "HOYO-MiX/Box/Box.cue",
            "{preset_name}"
        );
        assert_eq!(cue.kind, EntryKind::SideFile);
        assert_eq!(cue.action, Action::Copy);
        assert_eq!(cue.rewritten, Some(eac_cue(a, b)), "{preset_name}");
        assert_eq!(entry(&p, &at(&src, "Box/Box.log")).rewritten, None);
        assert!(
            p.entries
                .iter()
                .filter(|e| e.kind == EntryKind::Audio)
                .all(|e| e.rewritten.is_none())
        );
        assert!(
            p.cues_not_rewritten.is_empty(),
            "{:?}",
            p.cues_not_rewritten
        );
        // Planning only reads.
        assert_eq!(
            std::fs::read(at(&src, "Box/Box.cue")).unwrap(),
            eac_cue("101 Intro.flac", "102 Theme.flac")
        );
        assert!(tree(&dest).is_empty());
    }
}

#[test]
fn a_disc_folder_cue_names_tracks_relative_to_its_new_folder() {
    for (preset_name, cue_dst, a, b) in [
        (
            "artist-album",
            "HOYO-MiX/Box/Box.cue",
            "1-01 - Intro.flac",
            "1-02 - Theme.flac",
        ),
        (
            "artist-album+disc-folders",
            "HOYO-MiX/Box/Disc 1/Box.cue",
            "01 - Intro.flac",
            "02 - Theme.flac",
        ),
    ] {
        let (src, dest) = (unique_dir("cue-df-src"), unique_dir("cue-df-dest"));
        put(
            &src,
            "Box/CD1/01 Intro.flac",
            &track("Box", "Intro", 1, Some("1")),
        );
        put(
            &src,
            "Box/CD1/02 Theme.flac",
            &track("Box", "Theme", 2, Some("1")),
        );
        put(
            &src,
            "Box/CD2/01 Reprise.flac",
            &track("Box", "Reprise", 1, Some("2")),
        );
        put(
            &src,
            "Box/CD1/Box.cue",
            &eac_cue("01 Intro.flac", "02 Theme.flac"),
        );
        let p = plan_box(&src, &dest, preset_name, Strategy::Copy);
        let cue = entry(&p, &at(&src, "Box/CD1/Box.cue"));
        assert_eq!(rel(&dest, &cue.dst), cue_dst, "{preset_name}");
        assert_eq!(cue.rewritten, Some(eac_cue(a, b)), "{preset_name}");
    }
}

#[test]
fn a_single_image_cue_follows_its_image_and_keeps_unrelated_file_lines() {
    let (src, dest) = (unique_dir("cue-img-src"), unique_dir("cue-img-dest"));
    put(&src, "Live/Live.flac", &track("Live", "Live", 1, None));
    put(
        &src,
        "Live/Live.cue",
        b"TITLE \"Live\"\nFILE \"Live.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n\
FILE \"Encore.wav\" WAVE\n  TRACK 02 AUDIO\n    INDEX 01 00:00:00\n",
    );
    let p = plan_box(&src, &dest, "artist-album", Strategy::Copy);
    let cue = entry(&p, &at(&src, "Live/Live.cue"));
    // Same stem as the image: the cue follows the image's new name.
    assert_eq!(rel(&dest, &cue.dst), "HOYO-MiX/Live/01 - Live.cue");
    assert_eq!(
        cue.rewritten.as_deref(),
        Some(
            &b"TITLE \"Live\"\nFILE \"01 - Live.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n\
FILE \"Encore.wav\" WAVE\n  TRACK 02 AUDIO\n    INDEX 01 00:00:00\n"[..]
        ),
        "a FILE this plan does not place stays as it was"
    );
}

#[test]
fn a_legacy_cue_with_a_non_ascii_name_is_copied_unchanged_and_reported() {
    let (src, dest) = (unique_dir("cue-1252-src"), unique_dir("cue-1252-dest"));
    put(&src, "Box/101 Café.flac", &track("Box", "Café", 1, None));
    let cue_src = put(&src, "Box/Box.cue", b"FILE \"101 Caf\xE9.flac\" WAVE\r\n");
    let p = plan_box(&src, &dest, "artist-album", Strategy::Copy);
    assert_eq!(entry(&p, &cue_src).rewritten, None);
    assert_eq!(
        p.cues_not_rewritten,
        vec![(cue_src.clone(), CueNotRewritten::UnsupportedEncoding)]
    );
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(
        std::fs::read(at(&dest, "HOYO-MiX/Box/Box.cue")).unwrap(),
        b"FILE \"101 Caf\xE9.flac\" WAVE\r\n".to_vec(),
        "never mojibake: byte-identical"
    );
}

#[test]
fn an_existing_destination_cue_is_compared_against_the_rewritten_bytes() {
    let (src, dest) = (unique_dir("cue-dup-src"), unique_dir("cue-dup-dest"));
    eac_box(&src);
    let want = eac_cue("1-01 - Intro.flac", "1-02 - Theme.flac");
    let cue_dst = at(&dest, "HOYO-MiX/Box/Box.cue");
    std::fs::create_dir_all(cue_dst.parent().unwrap()).unwrap();

    // Exactly the rewritten bytes: already organized.
    std::fs::write(&cue_dst, &want).unwrap();
    let p = plan_box(&src, &dest, "artist-album", Strategy::Copy);
    assert_eq!(
        entry(&p, &at(&src, "Box/Box.cue")).action,
        Action::SkipDuplicate
    );

    // Same length, different bytes: a different file — never overwritten.
    let mut other = want.clone();
    other[0] = b'X';
    std::fs::write(&cue_dst, &other).unwrap();
    let p = plan_box(&src, &dest, "artist-album", Strategy::Copy);
    assert_eq!(
        entry(&p, &at(&src, "Box/Box.cue")).action,
        Action::SkipConflict
    );

    // The source's bytes (the un-rewritten cue): also a conflict.
    std::fs::write(&cue_dst, eac_cue("101 Intro.flac", "102 Theme.flac")).unwrap();
    let p = plan_box(&src, &dest, "artist-album", Strategy::Copy);
    assert_eq!(
        entry(&p, &at(&src, "Box/Box.cue")).action,
        Action::SkipConflict
    );
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
fn file_names_match_case_insensitively_on_windows_and_macos() {
    let (src, dest) = (unique_dir("cue-case-src"), unique_dir("cue-case-dest"));
    put(&src, "Box/101 Intro.flac", &track("Box", "Intro", 1, None));
    put(&src, "Box/Box.cue", b"FILE \"101 INTRO.FLAC\" WAVE\n");
    let p = plan_box(&src, &dest, "artist-album", Strategy::Copy);
    assert_eq!(
        entry(&p, &at(&src, "Box/Box.cue")).rewritten.as_deref(),
        Some(&b"FILE \"01 - Intro.flac\" WAVE\n"[..])
    );
}

#[test]
fn a_cue_in_a_sub_folder_reaching_up_with_dot_dot_is_rewritten() {
    let (src, dest) = (unique_dir("cue-up-src"), unique_dir("cue-up-dest"));
    put(&src, "Album/01 A.flac", &track("Album", "A", 1, None));
    put(
        &src,
        "Album/CUE/Album.cue",
        b"FILE \"..\\01 A.flac\" WAVE\r\n",
    );
    let p = plan_box(&src, &dest, "artist-album", Strategy::Copy);
    let cue = entry(&p, &at(&src, "Album/CUE/Album.cue"));
    assert_eq!(rel(&dest, &cue.dst), "HOYO-MiX/Album/CUE/Album.cue");
    assert_eq!(
        cue.rewritten.as_deref(),
        Some(&b"FILE \"../01 - A.flac\" WAVE\r\n"[..])
    );
    assert!(
        p.cues_not_rewritten.is_empty(),
        "{:?}",
        p.cues_not_rewritten
    );
}

/// A verbatim (`\\?\`, as `canonicalize` returns) source root, inputs and/or
/// destination root never make a same-tree cue `Unrepresentable`.
#[cfg(windows)]
#[test]
fn verbatim_prefixed_roots_still_rewrite_the_cue() {
    use std::path::PathBuf;
    let verbatim = |p: &Path| PathBuf::from(format!(r"\\?\{}", p.display()));
    let pick = |v: bool, p: &Path| if v { verbatim(p) } else { p.to_path_buf() };
    // (verbatim source root, verbatim inputs, verbatim destination root)
    for case in [
        (true, false, false),
        (true, true, false),
        (false, true, false),
        (false, false, true),
        (true, true, true),
    ] {
        let (src, dest) = (unique_dir("cue-vb-src"), unique_dir("cue-vb-dest"));
        eac_box(&src);
        let dest = pick(case.2, &dest);
        let p = plan_with(
            &audio_files(&pick(case.1, &src)),
            &dest,
            &tpl("artist-album+disc-folders"),
            Strategy::Copy,
            &PlanOptions::new(pick(case.0, &src)),
        );
        assert!(
            p.cues_not_rewritten.is_empty(),
            "{case:?}: {:?}",
            p.cues_not_rewritten
        );
        let cue = p
            .entries
            .iter()
            .find(|e| e.src.file_name().is_some_and(|n| n == "Box.cue"))
            .unwrap_or_else(|| panic!("{case:?}: {:#?}", p.entries));
        assert_eq!(rel(&dest, &cue.dst), "HOYO-MiX/Box/Box.cue", "{case:?}");
        assert_eq!(
            cue.rewritten,
            Some(eac_cue("Disc 1/01 - Intro.flac", "Disc 1/02 - Theme.flac")),
            "{case:?}"
        );
    }
}

#[test]
fn copy_writes_the_rewritten_cue_and_a_rerun_skips_it() {
    for preset_name in ["artist-album", "artist-album+disc-folders"] {
        let (src, dest) = (unique_dir("cue-copy-src"), unique_dir("cue-copy-dest"));
        eac_box(&src);
        let initial = tree(&src);
        let p = plan_box(&src, &dest, preset_name, Strategy::Copy);
        let want = entry(&p, &at(&src, "Box/Box.cue"))
            .rewritten
            .clone()
            .unwrap();
        let s = apply(&p, |_| {}).unwrap();
        assert!(s.failed.is_empty(), "{:?}", s.failed);
        assert_eq!(s.copied, p.entries.len());
        assert_eq!(
            std::fs::read(at(&dest, "HOYO-MiX/Box/Box.cue")).unwrap(),
            want,
            "{preset_name}"
        );
        assert_eq!(
            std::fs::read(at(&dest, "HOYO-MiX/Box/Box.log")).unwrap(),
            LOG,
            "{preset_name}: the log is byte-identical"
        );
        assert_eq!(tree(&src), initial, "{preset_name}: Copy leaves the source");
        let again = plan_box(&src, &dest, preset_name, Strategy::Copy);
        assert!(
            again
                .entries
                .iter()
                .all(|e| e.action == Action::SkipDuplicate),
            "{preset_name}: {:#?}",
            again.entries
        );
        assert_eq!(apply(&again, |_| {}).unwrap().copied, 0);
    }
}

#[test]
fn move_writes_the_rewritten_cue_removes_the_source_and_prunes() {
    let (src, dest) = (unique_dir("cue-move-src"), unique_dir("cue-move-dest"));
    eac_box(&src);
    let p = plan_box(&src, &dest, "artist-album+disc-folders", Strategy::Move);
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.moved, p.entries.len());
    assert_eq!(
        std::fs::read(at(&dest, "HOYO-MiX/Box/Box.cue")).unwrap(),
        eac_cue("Disc 1/01 - Intro.flac", "Disc 1/02 - Theme.flac")
    );
    assert!(!src.join("Box").exists(), "emptied album folder pruned");
    let again = apply(&p, |_| {}).unwrap();
    assert_eq!((again.moved, again.skipped), (0, p.entries.len()));
    assert!(again.failed.is_empty(), "{:?}", again.failed);
}

#[test]
fn a_cue_edited_between_plan_and_move_is_kept() {
    let (src, dest) = (unique_dir("cue-stale-src"), unique_dir("cue-stale-dest"));
    eac_box(&src);
    let p = plan_box(&src, &dest, "artist-album", Strategy::Move);
    assert!(entry(&p, &at(&src, "Box/Box.cue")).rewritten.is_some());
    let edited = b"REM EDITED BY USER\r\nFILE \"101 Intro.flac\" WAVE\r\n";
    std::fs::write(at(&src, "Box/Box.cue"), edited).unwrap();
    let s = apply(&p, |_| {}).unwrap();
    assert_eq!(s.failed.len(), 1, "{:?}", s.failed);
    assert_eq!(s.failed[0].0, at(&src, "Box/Box.cue"));
    assert_eq!(std::fs::read(at(&src, "Box/Box.cue")).unwrap(), edited);
    assert!(!at(&dest, "HOYO-MiX/Box/Box.cue").exists());
    // Everything else moved; the folder holding the kept cue is not pruned.
    assert!(at(&dest, "HOYO-MiX/Box/Box.log").exists());
    assert_eq!(tree(&src), ["Box/Box.cue".to_owned()].into_iter().collect());
}

/// A `Box` track by HOYO-MiX (one disc).
fn one(title: &str, n: u32) -> Vec<u8> {
    track("Box", title, n, None)
}

const CUE_XY: &[u8] = b"FILE \"1 x.flac\" WAVE\r\nFILE \"2 y.flac\" WAVE\r\n";

#[test]
fn the_cue_of_a_partly_selected_album_is_untouched() {
    let (src, dest) = (unique_dir("cue-part-src"), unique_dir("cue-part-dest"));
    let x = put(&src, "A/1 x.flac", &one("X", 1));
    put(&src, "A/2 y.flac", &one("Y", 2));
    put(&src, "A/a.cue", CUE_XY);
    let p = plan_with(
        &[x],
        &dest,
        &tpl("artist-album"),
        Strategy::Move,
        &PlanOptions::new(&src),
    );
    assert!(p.entries.iter().all(|e| e.kind == EntryKind::Audio));
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(std::fs::read(at(&src, "A/a.cue")).unwrap(), CUE_XY);
    assert!(!at(&dest, "HOYO-MiX/Box/a.cue").exists());
}

#[test]
fn the_cue_of_an_album_with_a_conflicting_track_is_untouched() {
    let (src, dest) = (unique_dir("cue-conf-src"), unique_dir("cue-conf-dest"));
    put(&src, "A/1 x.flac", &one("X", 1));
    put(&src, "A/2 y.flac", &one("Y", 2));
    put(&src, "A/a.cue", CUE_XY);
    put(&dest, "HOYO-MiX/Box/02 - Y.flac", b"a different file");
    let p = plan_box(&src, &dest, "artist-album", Strategy::Move);
    assert!(p.entries.iter().all(|e| e.kind == EntryKind::Audio));
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(std::fs::read(at(&src, "A/a.cue")).unwrap(), CUE_XY);
    assert!(
        at(&src, "A/2 y.flac").exists(),
        "the conflicting track stays"
    );
    assert!(!at(&dest, "HOYO-MiX/Box/a.cue").exists());
}

#[test]
fn a_numbered_album_cue_names_its_own_tracks() {
    // Two distinct `Box` albums: B's cue points at `Box (2)`'s track, A's at
    // `Box`'s — the same relative name, resolved against each cue's folder.
    let (src, dest) = (unique_dir("cue-num-src"), unique_dir("cue-num-dest"));
    put(&src, "A/1 x.flac", &one("X", 1));
    put(&src, "A/a.cue", b"FILE \"1 x.flac\" WAVE\r\n");
    put(&src, "B/1 x.flac", &track("Box", "X", 1, Some("1/1")));
    put(&src, "B/b.cue", b"REM B\r\nFILE \"1 x.flac\" WAVE\r\n");
    let p = plan_box(&src, &dest, "artist-album", Strategy::Move);
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    let want: std::collections::BTreeSet<String> = [
        "HOYO-MiX/Box (2)/01 - X.flac",
        "HOYO-MiX/Box (2)/b.cue",
        "HOYO-MiX/Box/01 - X.flac",
        "HOYO-MiX/Box/a.cue",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(tree(&dest), want);
    assert_eq!(
        std::fs::read(at(&dest, "HOYO-MiX/Box (2)/b.cue")).unwrap(),
        b"REM B\r\nFILE \"01 - X.flac\" WAVE\r\n"
    );
    assert_eq!(
        std::fs::read(at(&dest, "HOYO-MiX/Box/a.cue")).unwrap(),
        b"FILE \"01 - X.flac\" WAVE\r\n"
    );
    assert!(tree(&src).is_empty());
}
