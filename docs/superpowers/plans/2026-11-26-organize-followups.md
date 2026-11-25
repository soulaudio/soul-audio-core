# Organize follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the four follow-ups left by the multi-disc work (PR #133): clean rustdoc, a per-segment `{DiscDir}` / `{Album}` folder detection, partial-album selections that no longer sweep side files, and `.cue` `FILE`-line rewriting when the organizer renames tracks.

**Architecture:** All behaviour changes live in `crates/soul-audio-organize`. `template.rs` learns *which* segment is a directory (`has_disc_dir`) and *where* the `{Album}` folder ends (`render_album_folder`); `plan.rs` uses both. `side.rs`'s walk reports whether the album root (or one of its disc folders) holds non-input audio, and `plan.rs` turns that into `SideFilesLeft::PartialSelection`. A new pure module `cue.rs` rewrites `FILE` lines byte-for-byte; `plan.rs` stores the rewritten bytes on the `PlanEntry` (`rewritten: Option<Vec<u8>>`) and classifies duplicates against them; `apply.rs` writes those bytes (create-new) instead of copying, and for a Move removes the source only after the write succeeded.

**Tech Stack:** Rust 2024 (let-chains are already used in this crate), std only, no new dependencies. Tests use the crate's existing manual temp dirs and real tagged FLAC fixtures (`tests/support/mod.rs`).

## Global Constraints

- Worktree: `D:\dev\soulaudio\core-followups`, branch `feat/organize-followups` (from origin/main 3f8ac6d). All paths below are relative to it.
- Every cargo command runs with `CARGO_TARGET_DIR=E:/cargo-target/core-followups` (bash: prefix the command; PowerShell: `$env:CARGO_TARGET_DIR='E:/cargo-target/core-followups'` first).
- Commits: Conventional Commits (`type(scope): description`), **no AI / tool attribution anywhere in the message** — the commit-msg hook (`cargo xtask check-commit-msg`) rejects "claude", "anthropic", "co-author", "generated with".
- TDD: every task writes the failing test first, runs it, sees it fail for the stated reason, then implements.
- `soul-audio-organize` must not depend on `soul-audio-import` (dev-dependency only, as today). No new crates.
- Workspace lints: `clippy::all = warn`, CI runs clippy with `-D warnings`. `#![forbid(unsafe_code)]` in organize.
- Side files other than `.cue` (`.log`, covers, `Scans/`, `.lrc`, `.txt`, `.nfo`, …) stay **byte-identical**. Only a `.cue`'s `FILE` lines may change, and only when they reference audio this plan places.
- Never overwrite, never mojibake, never leave both the source and the destination of an entry missing.
- Decisions already made with the user (do not reopen): `SideFilesLeft::PartialSelection`; `OrganizePlan.cues_not_rewritten: Vec<(PathBuf, CueNotRewritten)>`; `.cue` rewriting supersedes the "side files are never modified" rule for `.cue` only.
- Interpretation recorded here (flag in the PR): a direct child of the album root that is named like a disc folder (`disc_folder::parse`) and holds non-input audio makes the selection partial **whether or not** it also holds an input (a user who selected only `CD1/` of a two-disc album selected part of the album).
- Rewritten `FILE` names use `/` as the separator (`Disc 1/01 - Intro.flac`) and are always double-quoted.

## File structure

| File | Responsibility | Tasks |
|---|---|---|
| `crates/soul-audio-index/src/lib.rs` | rustdoc fixes only | 1 |
| `crates/soul-audio-import/src/lib.rs` | rustdoc fix only | 1 |
| `xtask/src/main.rs` | `run_env` helper + rustdoc gate in `verify()` | 1 |
| `crates/soul-audio-organize/src/template.rs` | `has_disc_dir` per segment; `album_segment`, `render_album_folder` | 2, 3 |
| `crates/soul-audio-organize/src/plan.rs` | `separate_albums` suffixes the `{Album}` folder; `PartialSelection`; cue planning; `PlanEntry.rewritten`; `OrganizePlan.cues_not_rewritten`; content-aware `classify` | 3, 4, 6, 8 |
| `crates/soul-audio-organize/src/side.rs` | `Collected { files, partial }` from the walk | 4, 8 |
| `crates/soul-audio-organize/src/cue.rs` (new) | pure `FILE`-line rewriter + `plan_rewrite` glue + `CueNotRewritten` | 5, 6 |
| `crates/soul-audio-organize/src/apply.rs` | `do_write` for rewritten cues | 7 |
| `crates/soul-audio-organize/src/lib.rs` | `mod cue`, re-export `CueNotRewritten`, crate docs | 5, 8 |
| `crates/soul-audio-organize/tests/side_files.rs` | B and A integration tests | 2, 3, 4 |
| `crates/soul-audio-organize/tests/cue_rewrite.rs` (new) | cue plan/apply integration tests | 6, 7 |
| `crates/soul-audio-organize/tests/multi_disc_e2e.rs` | EAC cue in the e2e matrix | 8 |

Order: C (Task 1), B (Tasks 2–3), A (Task 4), D (Tasks 5–8), gate (Task 9).

---

### Task 1: Clean rustdoc for soul-audio-index and soul-audio-import (C)

**Files:**
- Modify: `crates/soul-audio-index/src/lib.rs:733` and `:745-746` (inside the `build`-gated `CatalogInput` docs)
- Modify: `crates/soul-audio-import/src/lib.rs:420` (`RawTrack` doc)
- Modify: `xtask/src/main.rs:62-74` (`run`) and `verify()` after the second clippy call (`:85-107`)

**Interfaces:**
- Consumes: nothing.
- Produces: `fn run_env(env: &[(&str, &str)], args: &[&str]) -> Result<(), String>` in `xtask/src/main.rs` (private); `run(args)` becomes `run_env(&[], args)`.

Current errors (verified with `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p soul-audio-index --features build -p soul-audio-import`):

```
error: unresolved link to `i`   --> crates\soul-audio-index\src\lib.rs:733:32   (index i = artists[i])
error: unresolved link to `i`   --> crates\soul-audio-index\src\lib.rs:745:20   albums[i]); `None` = no cover
error: public documentation for `RawTrack` links to private item `extract_raw`  --> crates\soul-audio-import\src\lib.rs:420:57
```

`soul-audio-organize` is already clean (`cargo doc --no-deps -p soul-audio-organize` with `-D warnings` passes) — keep it that way in Tasks 2–8.

- [ ] **Step 1: Reproduce the failure (RED)**

Run:
```bash
cd /d/dev/soulaudio/core-followups
RUSTDOCFLAGS="-D warnings" CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo doc --no-deps -p soul-audio-index --features build
RUSTDOCFLAGS="-D warnings" CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo doc --no-deps -p soul-audio-import
```
Expected: both fail with the three errors above.

- [ ] **Step 2: Fix the index links**

In `crates/soul-audio-index/src/lib.rs`, replace

```rust
        /// Per-artist sort-name ("Clapton, Eric"), aligned to `artists`
        /// (index i = artists[i]). An empty entry — or a shorter/empty vec —
```
with
```rust
        /// Per-artist sort-name ("Clapton, Eric"), aligned to `artists`
        /// (index `i` = `artists[i]`). An empty entry — or a shorter/empty vec —
```

and replace

```rust
        /// Per-album cover source bytes, aligned to `albums` (index i =
        /// albums[i]); `None` = no cover. Empty vec = no covers at all (the
```
with
```rust
        /// Per-album cover source bytes, aligned to `albums` (index `i` =
        /// `albums[i]`); `None` = no cover. Empty vec = no covers at all (the
```

- [ ] **Step 3: Fix the import link**

In `crates/soul-audio-import/src/lib.rs:420`, replace

```rust
/// re-read. `path` is the key; the rest mirrors what [`extract_raw`] pulls from
```
with
```rust
/// re-read. `path` is the key; the rest mirrors what `extract_raw` pulls from
```
(`extract_raw` is private; a plain code span is the correct form for public docs.)

- [ ] **Step 4: Verify (GREEN)**

Run the two commands from Step 1 again, plus:
```bash
RUSTDOCFLAGS="-D warnings" CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo doc --no-deps -p soul-audio-organize
```
Expected: all three finish with `Generated …index.html` and no `warning:`/`error:` lines.

- [ ] **Step 5: Add the rustdoc gate to `cargo xtask ci` (only if cheap)**

Measure first: run the three `cargo doc` commands of Step 4 once more on the now-warm target dir and time them (`time …`). If the three together take **≤ 60 s warm**, add the gate below; otherwise skip this step, leave xtask untouched, and note "rustdoc gate not added: N s" in the PR body.

In `xtask/src/main.rs`, replace the body of `run` (lines 62-74) with a call to a new `run_env`:

```rust
/// Run a cargo subcommand, returning an error string on non-zero exit.
fn run(args: &[&str]) -> Result<(), String> {
    run_env(&[], args)
}

/// [`run`] with extra environment variables set for the child cargo.
fn run_env(env: &[(&str, &str)], args: &[&str]) -> Result<(), String> {
    let shown: String = env.iter().map(|(k, v)| format!("{k}={v:?} ")).collect();
    println!("+ {shown}cargo {}", args.join(" "));
    let status = Command::new(env!("CARGO"))
        .envs(env.iter().copied())
        .args(args)
        .status()
        .map_err(|e| format!("spawning cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{shown}cargo {}` failed ({status})", args.join(" ")))
    }
}
```

In `verify()`, directly after the `soul-audio-artbake` clippy call (the one ending `])?;` at about line 107), insert:

```rust
    // Rustdoc of the std adapters whose docs link into `build`-gated and
    // private items: a broken or private intra-doc link is an error. Scoped to
    // these crates (cheap: no codegen) rather than the whole workspace.
    for args in [
        &["doc", "--no-deps", "-p", "soul-audio-index", "--features", "build"][..],
        &["doc", "--no-deps", "-p", "soul-audio-import"][..],
        &["doc", "--no-deps", "-p", "soul-audio-organize"][..],
    ] {
        run_env(&[("RUSTDOCFLAGS", "-D warnings")], args)?;
    }
```

Check it compiles and lints:
```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo clippy -p xtask --all-targets -- -D warnings
```
Expected: `Finished` with no warnings. (The full `cargo xtask ci` runs in Task 9.)

- [ ] **Step 6: Commit**

```bash
git add crates/soul-audio-index/src/lib.rs crates/soul-audio-import/src/lib.rs xtask/src/main.rs
git commit -m "docs: fix rustdoc link errors in index and import; deny rustdoc warnings in xtask ci"
```
(If Step 5 was skipped, drop `xtask/src/main.rs` and use `docs: fix rustdoc link errors in index and import`.)

---

### Task 2: `{DiscDir}` counts only in a directory segment (B, part 1)

**Files:**
- Modify: `crates/soul-audio-organize/src/template.rs:81-84` (`has_disc_dir`)
- Test: `crates/soul-audio-organize/src/template.rs` (tests module, after `neutral_disc_dir_renders_empty_and_gives_the_album_folder`, ~line 382)
- Test: `crates/soul-audio-organize/tests/side_files.rs` (append)

**Interfaces:**
- Consumes: nothing new.
- Produces: `Template::has_disc_dir(&self) -> bool` (same signature, `pub(crate)`) now true only when a **directory** segment (any segment but the last) contains `{DiscDir}`. Callers unchanged: `plan.rs` `folder_inferred_discs` (line 409-419) and `AlbumPlacement.disc_dirs` (line 344).

- [ ] **Step 1: Write the failing unit test**

Add to the `tests` module of `crates/soul-audio-organize/src/template.rs`:

```rust
    #[test]
    fn disc_dir_counts_only_in_a_directory_segment() {
        assert!(Template::new("{Album}/{DiscDir}/{TrackNo}").has_disc_dir());
        assert!(Template::new("{Album}/CD {DiscDir}/{TrackNo}").has_disc_dir());
        assert!(
            !Template::new("{Album}/{DiscDir} {TrackNo} - {Title}").has_disc_dir(),
            "in the file name it is no disc folder"
        );
        assert!(!Template::new("{DiscDir}").has_disc_dir());
        assert!(Template::new(preset("artist-album+disc-folders").unwrap()).has_disc_dir());
        assert!(!Template::new(preset("artist-album").unwrap()).has_disc_dir());
    }
```

- [ ] **Step 2: Write the failing integration test**

Append to `crates/soul-audio-organize/tests/side_files.rs`:

```rust
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
```

- [ ] **Step 3: Run both to verify they fail**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize --lib disc_dir_counts_only_in_a_directory_segment
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize --test side_files disc_dir_in_the_file_name_is_no_disc_folder_layout
```
Expected: the unit test FAILS with `in the file name it is no disc folder`; the integration test FAILS on `folder_inferred_discs` (left `[]`, right `[…/Suite]`).

- [ ] **Step 4: Implement**

Replace `has_disc_dir` in `template.rs` (lines 81-84):

```rust
    /// Whether the template lays discs out as folders: a **directory**
    /// segment (not the file name) uses `{DiscDir}`. A `{DiscDir}` in the file
    /// name keeps every disc in one folder.
    pub(crate) fn has_disc_dir(&self) -> bool {
        let segments: Vec<&str> = self.raw.split('/').collect();
        segments[..segments.len() - 1]
            .iter()
            .any(|s| s.contains("{DiscDir}"))
    }
```
(`split` always yields at least one item, so `len() - 1` cannot underflow.)

- [ ] **Step 5: Run the tests to verify they pass, then the crate**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize
```
Expected: all tests PASS (incl. `folder_inferred_discs_are_reported_unless_the_layout_keeps_disc_folders` and the e2e matrix).

- [ ] **Step 6: Commit**

```bash
git add crates/soul-audio-organize/src/template.rs crates/soul-audio-organize/tests/side_files.rs
git commit -m "fix(organize): only a {DiscDir} directory segment lays discs out as folders"
```

---

### Task 3: Number the `{Album}` folder, not a folder below it (B, part 2)

**Files:**
- Modify: `crates/soul-audio-organize/src/template.rs:72-79` (`has_album_dir`) + new `album_segment`, `render_album_folder`
- Modify: `crates/soul-audio-organize/src/plan.rs:257-267` (render loop), `:421-473` (`separate_albums` + doc)
- Test: `crates/soul-audio-organize/src/template.rs` tests module; `crates/soul-audio-organize/tests/side_files.rs` (append)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `Template::render_album_folder(&self, tags: &Tags, src: &Path) -> Option<PathBuf>` (`pub(crate)`): the rendered path up to and including the **first** directory segment that uses `{Album}` (`{DiscDir}` neutralized, empty directory levels dropped exactly like `render_inner`); `None` when no directory segment uses `{Album}`.
  - `fn separate_albums(albums: &[AlbumGroup], rel_dsts: &mut [PathBuf], album_dirs: &mut [PathBuf], album_folders: &[PathBuf])` in `plan.rs` (private).
  - `fn under(base: &Path, rest: &Path) -> PathBuf` in `plan.rs` (private).

Behaviour: *which* albums collide is unchanged (keyed on the full album dir, so `Hits/R` vs `Hits/P` in `a_replan_after_apply_picks_the_same_side_file_names` stays as is). Only *where* the ` (n)` goes changes: on the `{Album}` folder. If the `{Album}` folder is not a prefix of the album dir (a `{DiscDir}` before `{Album}` — unsupported oddity), the old behaviour (suffix the last folder) is kept.

- [ ] **Step 1: Write the failing unit test**

Add to the `tests` module of `template.rs`:

```rust
    #[test]
    fn the_album_folder_is_the_album_segment_not_the_last_folder() {
        let tagged = |date: Option<&str>| Tags {
            title: Some("T".into()),
            album: Some("Alb".into()),
            album_artist: Some("A".into()),
            date: date.map(str::to_owned),
            track: Some(1),
            ..Default::default()
        };
        let src = Path::new("x.flac");
        assert_eq!(
            Template::new("{AlbumArtist}/{Album}/{Year}/{TrackNo}")
                .render_album_folder(&tagged(Some("2020")), src),
            Some(expect(&["A", "Alb"]))
        );
        assert_eq!(
            Template::new(preset("artist-album+disc-folders").unwrap())
                .render_album_folder(&disc_tags(2, Some(2)), src),
            Some(expect(&["Unknown Artist", "Alb"])),
            "{{DiscDir}} after the album folder is not part of it"
        );
        assert_eq!(
            Template::new("{Year}/{Album}/{TrackNo}").render_album_folder(&tagged(None), src),
            Some(expect(&["Alb"])),
            "an empty level before it is dropped, as in render"
        );
        assert_eq!(
            Template::new(preset("flat").unwrap()).render_album_folder(&tagged(None), src),
            None
        );
        assert_eq!(
            Template::new("{Artist}/{Album} - {Title}").render_album_folder(&tagged(None), src),
            None,
            "{{Album}} only in the file name"
        );
    }
```

- [ ] **Step 2: Write the failing integration test**

Append to `tests/side_files.rs`:

```rust
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
}
```

- [ ] **Step 3: Run both to verify they fail**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize --lib the_album_folder_is_the_album_segment
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize --test side_files the_numbered_suffix_goes_on_the_album_folder
```
Expected: the unit test fails to COMPILE (`no method named render_album_folder`); the integration test FAILS — actual contains `HOYO-MiX/Box/2020 (2)/01 - a.flac`.

- [ ] **Step 4: Implement the template side**

In `template.rs`, replace `has_album_dir` (lines 72-79) with:

```rust
    /// Whether the template gives an album a folder of its own: a directory
    /// segment uses `{Album}`. Only then does the planner number the folders of
    /// distinct same-title albums (never for `flat` or `{Artist}/{Title}`).
    pub(crate) fn has_album_dir(&self) -> bool {
        self.album_segment().is_some()
    }

    /// The index of the first **directory** segment that uses `{Album}`.
    fn album_segment(&self) -> Option<usize> {
        let segments: Vec<&str> = self.raw.split('/').collect();
        segments[..segments.len() - 1]
            .iter()
            .position(|s| s.contains("{Album}"))
    }

    /// The `{Album}` folder a track renders into, relative to the destination
    /// root: the path up to and including the first directory segment using
    /// `{Album}`, with `{DiscDir}` neutralized and empty levels dropped exactly
    /// as in [`render`](Self::render). `None` without such a segment. The
    /// planner puts the ` (2)` of a second same-title album on this folder.
    pub(crate) fn render_album_folder(&self, tags: &Tags, src: &Path) -> Option<PathBuf> {
        let last = self.album_segment()?;
        let mut path = PathBuf::new();
        for segment in self.raw.split('/').take(last + 1) {
            let substituted = substitute(segment, tags, src, true);
            if !substituted.trim().is_empty() {
                path.push(sanitize_segment(&substituted));
            }
        }
        Some(path)
    }
```

- [ ] **Step 5: Implement the planner side**

In `plan.rs` `plan_with`, replace lines 257-267:

```rust
    let mut rel_dsts: Vec<PathBuf> = Vec::with_capacity(files.len());
    let mut album_dirs: Vec<PathBuf> = Vec::with_capacity(files.len());
    for (src, info) in files.iter().zip(&tracks) {
        rel_dsts.push(template.render(&info.tags, src));
        album_dirs.push(template.render_album_dir(&info.tags, src));
    }
    if template.has_album_dir() {
        separate_albums(&albums, &mut rel_dsts, &mut album_dirs);
    }
```
with
```rust
    let mut rel_dsts: Vec<PathBuf> = Vec::with_capacity(files.len());
    let mut album_dirs: Vec<PathBuf> = Vec::with_capacity(files.len());
    let mut album_folders: Vec<PathBuf> = Vec::with_capacity(files.len());
    for (src, info) in files.iter().zip(&tracks) {
        rel_dsts.push(template.render(&info.tags, src));
        album_dirs.push(template.render_album_dir(&info.tags, src));
        album_folders.push(
            template
                .render_album_folder(&info.tags, src)
                .unwrap_or_default(),
        );
    }
    if template.has_album_dir() {
        separate_albums(&albums, &mut rel_dsts, &mut album_dirs, &album_folders);
    }
```

Replace `separate_albums` and its doc comment (lines 421-473) with:

```rust
/// Give every **distinct** album its own album folder within this batch.
///
/// Albums are walked in plan (first-seen input) order. The first album to
/// render into an album dir keeps it; a later album (a different identity
/// key) rendering into an already-claimed dir gets its **`{Album}` folder**
/// (`album_folders[i]`, see [`Template::render_album_folder`]) suffixed
/// ` (2)`, ` (3)`, … — the lowest `n` whose resulting album dir no album in
/// this batch claims or renders to natively — and its destinations are
/// re-rooted under it (`{AlbumArtist}/{Album}/{Year}/…` gives
/// `Box (2)/2020`, never `Box/2020 (2)`). When the `{Album}` folder is not a
/// prefix of the album dir (`{DiscDir}` before `{Album}`) the dir's last
/// folder is suffixed instead. Dirs compare as [`paths::key`]s (`Abc` and
/// `ABC` are one folder on Windows/macOS). Folders already on disk are not
/// considered: a re-run plans the same names, and its files classify as
/// `SkipDuplicate` as before. A track with no album dir (it renders into the
/// destination root) is left alone.
fn separate_albums(
    albums: &[AlbumGroup],
    rel_dsts: &mut [PathBuf],
    album_dirs: &mut [PathBuf],
    album_folders: &[PathBuf],
) {
    let native: HashSet<PathBuf> = album_dirs
        .iter()
        .filter(|d| d.components().next().is_some())
        .map(|d| paths::key(d))
        .collect();
    let mut owner: HashMap<PathBuf, usize> = HashMap::new();
    for (a, album) in albums.iter().enumerate() {
        // This album's native dir (as a key) → its final `{Album}` folder.
        let mut mine: HashMap<PathBuf, PathBuf> = HashMap::new();
        for &i in &album.tracks {
            let dir = album_dirs[i].clone();
            let folder = Some(&album_folders[i])
                .filter(|f| f.components().next().is_some() && dir.starts_with(f))
                .unwrap_or(&dir)
                .clone();
            let Some(name) = folder.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let below = dir
                .strip_prefix(&folder)
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let fin = mine
                .entry(paths::key(&dir))
                .or_insert_with(|| {
                    let fin = match owner.get(&paths::key(&dir)) {
                        Some(&o) if o != a => (2u32..)
                            .map(|n| folder.with_file_name(format!("{name} ({n})")))
                            .find(|c| {
                                let k = paths::key(&under(c, &below));
                                !owner.contains_key(&k) && !native.contains(&k)
                            })
                            .unwrap_or_else(|| folder.clone()),
                        _ => folder.clone(),
                    };
                    owner.insert(paths::key(&under(&fin, &below)), a);
                    fin
                })
                .clone();
            if fin != folder
                && let Ok(rest) = rel_dsts[i].strip_prefix(&folder)
            {
                rel_dsts[i] = fin.join(rest);
                album_dirs[i] = under(&fin, &below);
            }
        }
    }
}

/// `base/rest`, or `base` itself when `rest` is empty (no trailing separator).
fn under(base: &Path, rest: &Path) -> PathBuf {
    if rest.as_os_str().is_empty() {
        base.to_path_buf()
    } else {
        base.join(rest)
    }
}
```

- [ ] **Step 6: Run the tests**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize
```
Expected: all PASS — in particular the two new tests, `same_title_distinct_albums_get_numbered_folders_not_merged`, `numbered_folders_follow_disc_folders_and_skip_taken_names`, `album_folders_differing_only_by_case_are_numbered` (Windows/macOS), `a_replan_after_apply_picks_the_same_side_file_names`, and the e2e matrix.

- [ ] **Step 7: Lint, then commit**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo clippy -p soul-audio-organize --all-targets -- -D warnings
git add crates/soul-audio-organize/src/template.rs crates/soul-audio-organize/src/plan.rs crates/soul-audio-organize/tests/side_files.rs
git commit -m "fix(organize): number the {Album} folder, not a folder below it"
```

---

### Task 4: A partial album selection leaves its side files in place (A)

**Files:**
- Modify: `crates/soul-audio-organize/src/side.rs:47-74` (`Seen`, `collect` → returns `Collected`), `:76-122` (`walk`)
- Modify: `crates/soul-audio-organize/src/plan.rs:58-81` (`SideFilesLeft` + `PartialSelection`), `:83-89` (`LeftInPlace` doc), `:208-213` (`plan_with` doc, side-files paragraph), `:313-316` (use `Collected`)
- Test: `crates/soul-audio-organize/tests/side_files.rs` (append)

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `SideFilesLeft::PartialSelection` (public; enum is `#[non_exhaustive]`, so additive).
  - `pub(crate) struct Collected { pub(crate) files: Vec<PathBuf>, pub(crate) partial: bool }` and `pub(crate) fn collect(album_root: &Path, skip: &Path, inputs: &HashSet<PathBuf>) -> Collected` in `side.rs`.

Rule: the album root's **own** non-input audio files, or non-input audio anywhere under a **direct child named like a disc folder** (`disc_folder::parse`), make the selection partial. Non-input audio in any other sub-folder (`Bonus Album/`, `Extras/Live/`) is another album — skipped as today (`a_nested_unselected_album_keeps_its_files` must keep passing). `PartialSelection` is checked after the album gets a folder of its own and before `AudioConflict`; it reports the walked side-file list (like `AudioConflict`).

- [ ] **Step 1: Write the failing tests**

Append to `tests/side_files.rs`:

```rust
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
```

- [ ] **Step 2: Run them to verify they fail**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize --test side_files partial
```
Expected: compile error `no variant named PartialSelection found for enum SideFilesLeft`.

- [ ] **Step 3: Add the enum variant and docs (plan.rs)**

In `SideFilesLeft` (plan.rs:58-81), after `AudioConflict,` add:

```rust
    /// Only part of the album was selected: its root, or one of its disc
    /// folders (`CD2/`), still holds audio that is not among the inputs. The
    /// side files belong with the whole album, so none of them move.
    PartialSelection,
```

Update the `LeftInPlace` doc (plan.rs:85-88) from

```rust
/// For [`SideFilesLeft::AudioConflict`] these are all the album's side files
/// (its root lies in the source root and holds only this album, so it is
/// walked); for every other reason only the album root's **own** files are
/// counted — the planner never walks a folder whose files it will not plan.
```
to
```rust
/// For [`SideFilesLeft::AudioConflict`] and [`SideFilesLeft::PartialSelection`]
/// these are all the album's side files (its root lies in the source root and
/// holds only this album, so it is walked); for every other reason only the
/// album root's **own** files are counted — the planner never walks a folder
/// whose files it will not plan.
```

In the `plan_with` doc, replace the **Side files** paragraph (plan.rs:208-213):

```rust
/// **Side files**: for every album root inside the source root that holds
/// exactly one album with a folder of its own, each non-audio, non-junk file
/// under it (recursively; not into a sub-folder holding another album's audio)
/// is planned too — see [`EntryKind::SideFile`] — unless one of the album's
/// audio files is a [`SkipConflict`](Action::SkipConflict). Otherwise the root
/// is reported in [`OrganizePlan::side_files_left`].
```
with
```rust
/// **Side files**: for every album root inside the source root that holds
/// exactly one album with a folder of its own, each non-audio, non-junk file
/// under it (recursively; not into a sub-folder holding another album's audio)
/// is planned too — see [`EntryKind::SideFile`] — unless only part of the
/// album was selected (its root or a disc folder holds non-input audio:
/// [`SideFilesLeft::PartialSelection`]) or one of the album's audio files is a
/// [`SkipConflict`](Action::SkipConflict). Otherwise the root is reported in
/// [`OrganizePlan::side_files_left`].
```

- [ ] **Step 4: Report `partial` from the walk (side.rs)**

Replace `Seen` and `collect` (side.rs:47-74) with:

```rust
/// What a folder walk saw besides side files.
#[derive(Default)]
struct Seen {
    /// Audio that is not one of the planner's inputs (in this folder or below).
    foreign_audio: bool,
    /// One of the planner's inputs, directly in this folder.
    input: bool,
    /// Set at depth 0 only: the album root itself, or one of its disc folders,
    /// holds audio that is not an input — only part of the album was selected.
    partial: bool,
}

/// An album root's side files, and whether its album was only partly selected.
pub(crate) struct Collected {
    /// Every side file under the root, sorted (see [`collect`]).
    pub(crate) files: Vec<PathBuf>,
    /// The album root, or a direct child named like a disc folder, holds audio
    /// that is not an input.
    pub(crate) partial: bool,
}

/// Every side file under `album_root`, recursively, sorted. Skips junk/hidden
/// entries, audio files, symlinks, the planner's `inputs` (keyed by
/// [`paths::key`]; an input is never also a side file), and the `skip`
/// directory (the destination root, when it sits inside a source album root).
///
/// A sub-folder holding audio that is **not** an input — another album nested
/// in this one — is skipped whole: its files belong with that audio. The
/// exception is one of this album's own disc folders: a direct child of the
/// album root named like a disc folder ([`disc_folder::parse`]) that directly
/// holds an input. Non-input audio in the album root itself or in **any** of
/// its disc folders sets [`Collected::partial`] (the planner then moves no
/// side file at all).
///
/// Non-UTF-8 names are side files like any other (never renamed lossily — see
/// [`place`]).
pub(crate) fn collect(album_root: &Path, skip: &Path, inputs: &HashSet<PathBuf>) -> Collected {
    let mut files = Vec::new();
    let seen = walk(album_root, &paths::key(skip), inputs, 0, &mut files);
    files.sort();
    Collected {
        files,
        partial: seen.partial,
    }
}
```

In `walk` (side.rs:102-119), replace the `if ft.is_dir() { … } else if ft.is_file() { … }` block with:

```rust
        if ft.is_dir() {
            let mark = out.len();
            let sub = walk(&path, skip, inputs, depth + 1, out);
            let disc = depth == 0 && name.to_str().and_then(disc_folder::parse).is_some();
            seen.partial |= disc && sub.foreign_audio;
            let own_disc = disc && sub.input;
            if sub.foreign_audio && !own_disc {
                out.truncate(mark); // another album's folder: none of it is ours
                seen.foreign_audio = true;
            }
        } else if ft.is_file() {
            if inputs.contains(&paths::key(&path)) {
                seen.input = true;
            } else if is_audio(&path) {
                seen.foreign_audio = true;
                seen.partial |= depth == 0;
            } else {
                out.push(path);
            }
        }
```

- [ ] **Step 5: Use it in the planner (plan.rs)**

Replace plan.rs:313-316

```rust
        let side = side::collect(&album.root, dest_root, &inputs);
        if side.is_empty() {
            continue;
        }
```
with
```rust
        let side::Collected {
            files: side,
            partial,
        } = side::collect(&album.root, dest_root, &inputs);
        if side.is_empty() {
            continue;
        }
        if partial {
            side_files_left.push(LeftInPlace::new(
                &album.root,
                SideFilesLeft::PartialSelection,
                side,
            ));
            continue;
        }
```

- [ ] **Step 6: Run the crate tests**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize
```
Expected: all PASS, including the two new tests, `a_nested_unselected_album_keeps_its_files`, `an_input_is_never_also_a_side_file` and the e2e matrix (the matrix always selects every track).

- [ ] **Step 7: Lint, then commit**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo clippy -p soul-audio-organize --all-targets -- -D warnings
git add crates/soul-audio-organize/src/side.rs crates/soul-audio-organize/src/plan.rs crates/soul-audio-organize/tests/side_files.rs
git commit -m "feat(organize): leave side files in place on a partial album selection"
```

---

### Task 5: The cue sheet `FILE`-line rewriter (D, pure core)

**Files:**
- Create: `crates/soul-audio-organize/src/cue.rs`
- Modify: `crates/soul-audio-organize/src/lib.rs:51-65` (`mod cue;`, `pub use cue::CueNotRewritten;`)

**Interfaces:**
- Consumes: `crate::paths::key` (existing, `pub(crate) fn key(p: &Path) -> PathBuf`).
- Produces (used by Task 6):
  - `pub enum CueNotRewritten { UnsupportedEncoding, Unrepresentable, Unreadable }` — `#[derive(Clone, Copy, Debug, PartialEq, Eq)] #[non_exhaustive]`, re-exported from the crate root.
  - `pub(crate) fn is_cue(path: &Path) -> bool`
  - `pub(crate) enum Outcome { Unchanged, Rewritten(Vec<u8>), NotRewritten(CueNotRewritten) }` (`Debug, PartialEq, Eq`)
  - `pub(crate) fn rewrite(bytes: &[u8], target: impl FnMut(&str) -> Option<PathBuf>) -> Outcome` — `target(name_as_written)` returns the new path **relative to the cue's destination folder**, or `None` to keep the line.
  - `pub(crate) fn relative(from_dir: &Path, to: &Path) -> PathBuf`
  - `pub(crate) fn plan_rewrite(src: &Path, dst: &Path, audio_to: &HashMap<PathBuf, PathBuf>) -> Result<Option<Vec<u8>>, CueNotRewritten>` — `audio_to` maps `paths::key(audio src)` → audio dst; `Ok(None)` = copy unchanged; `Ok(Some(bytes))` = write these; `Err(why)` = copy unchanged and report.

Rules implemented:
- Lines split on `\r\n`, `\n` or a lone `\r`; each line's terminator is kept as it was; an unterminated last line stays unterminated.
- A `FILE` line: optional leading spaces/tabs, `FILE` (any case), at least one space/tab, then a `"quoted name"` or an unquoted token (up to the next space/tab). Everything before the name (`head`) and after it (`tail`, e.g. ` WAVE`) is kept byte-for-byte; the new name is always written quoted.
- Encoding: a UTF-8 BOM is kept. UTF-16 (`FF FE` / `FE FF`) → `NotRewritten(UnsupportedEncoding)`. Body valid UTF-8 → any name may be matched and written. Otherwise (legacy single-byte code page) only ASCII names: a non-ASCII `FILE` name as written, or a non-ASCII new name → `NotRewritten(UnsupportedEncoding)` (the whole cue is left unchanged).
- New name: path components joined with `/` (`..` allowed); not UTF-8, empty, or containing `"`, `\r`, `\n` → `NotRewritten(Unrepresentable)`.
- A line whose new name equals the old one is kept; if no line changes → `Unchanged`.

- [ ] **Step 1: Write the module skeleton with the failing tests**

Create `crates/soul-audio-organize/src/cue.rs` with the docs, the public enum, stub functions, and the tests:

```rust
//! `.cue` sheet `FILE`-line rewriting.
//!
//! When the organizer renames or moves the audio files a cue sheet references,
//! the cue's planned copy gets its `FILE` lines pointed at the new names,
//! relative to the cue's **new** folder (`Disc 1/01 - Intro.flac` when the cue
//! stays at the album root and the tracks move into disc folders). Every other
//! byte — a UTF-8 BOM, CRLF/LF line endings, `REM` lines and every other
//! command, a `FILE` line's indentation and file type — is kept as it was. A
//! cue that cannot be rewritten without risking mojibake is left
//! byte-identical and reported ([`CueNotRewritten`]). This is the **only**
//! side file the organizer ever modifies: a `.log` (whose checksum must stay
//! valid) and everything else is copied/moved byte for byte.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::paths;

/// Why a `.cue` side file was planned **unchanged** although it may reference
/// audio this plan renames ([`OrganizePlan::cues_not_rewritten`](crate::OrganizePlan::cues_not_rewritten)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CueNotRewritten {
    /// The cue is not UTF-8 (a legacy code page such as Windows-1252, or
    /// UTF-16) and a `FILE` name — as written, or the new one — is not plain
    /// ASCII: it can be neither matched nor written without risking mojibake.
    UnsupportedEncoding,
    /// A new name cannot be written into a `FILE` line (it is not UTF-8, or it
    /// holds a `"` or a line break).
    Unrepresentable,
    /// The cue could not be read while planning, or is larger than 1 MiB (no
    /// real cue sheet is).
    Unreadable,
}

/// A larger `.cue` is not a cue sheet; it is copied as-is.
const MAX_CUE_BYTES: u64 = 1 << 20;

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// Whether `path` is a cue sheet (`.cue`, any case).
pub(crate) fn is_cue(path: &Path) -> bool {
    let _ = path;
    false
}

/// What [`rewrite`] made of a cue sheet.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// No `FILE` line needs a new name: copy the cue as-is.
    Unchanged,
    /// The cue with its `FILE` lines rewritten.
    Rewritten(Vec<u8>),
    /// Some `FILE` line could not be handled safely: copy the cue as-is and
    /// report why.
    NotRewritten(CueNotRewritten),
}

/// Rewrite the `FILE` lines of the cue sheet `bytes`. `target` gets each
/// `FILE` name as written and returns its new path relative to the cue's
/// destination folder, or `None` to keep the line.
pub(crate) fn rewrite(bytes: &[u8], target: impl FnMut(&str) -> Option<PathBuf>) -> Outcome {
    let _ = (bytes, target);
    Outcome::Unchanged
}

/// `to` relative to the folder `from_dir` (both under one destination root),
/// walking up with `..` where needed.
pub(crate) fn relative(from_dir: &Path, to: &Path) -> PathBuf {
    let _ = from_dir;
    to.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::{CueNotRewritten, Outcome, is_cue, relative, rewrite};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// `rewrite` against a fixed `old name → new relative path` table.
    fn run(bytes: &[u8], table: &[(&str, &str)]) -> Outcome {
        let map: HashMap<&str, PathBuf> = table
            .iter()
            .map(|(old, new)| (*old, PathBuf::from(new)))
            .collect();
        rewrite(bytes, |name| map.get(name).cloned())
    }

    fn rewritten(o: Outcome) -> Vec<u8> {
        match o {
            Outcome::Rewritten(b) => b,
            other => panic!("expected Rewritten, got {other:?}"),
        }
    }

    #[test]
    fn crlf_lines_and_every_other_byte_are_kept() {
        let src = b"REM GENRE Game\r\nREM COMMENT \"ExactAudioCopy v1.6\"\r\n\
PERFORMER \"HOYO-MiX\"\r\nFILE \"01 Intro.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n\
    INDEX 01 00:00:00\r\n";
        let want = b"REM GENRE Game\r\nREM COMMENT \"ExactAudioCopy v1.6\"\r\n\
PERFORMER \"HOYO-MiX\"\r\nFILE \"1-01 - Intro.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n\
    INDEX 01 00:00:00\r\n";
        assert_eq!(
            rewritten(run(src, &[("01 Intro.flac", "1-01 - Intro.flac")])),
            want.to_vec()
        );
    }

    #[test]
    fn lf_lone_cr_and_an_unterminated_last_line_are_kept() {
        assert_eq!(
            rewritten(run(
                b"TITLE \"x\"\nFILE \"a.flac\" WAVE\rTRACK 01 AUDIO\nFILE \"b.flac\" WAVE",
                &[("a.flac", "A.flac"), ("b.flac", "B.flac")]
            )),
            b"TITLE \"x\"\nFILE \"A.flac\" WAVE\rTRACK 01 AUDIO\nFILE \"B.flac\" WAVE".to_vec()
        );
    }

    #[test]
    fn a_utf8_bom_is_kept() {
        assert_eq!(
            rewritten(run(
                b"\xEF\xBB\xBFFILE \"a.flac\" WAVE\r\n",
                &[("a.flac", "Disc 1/01 - A.flac")]
            )),
            b"\xEF\xBB\xBFFILE \"Disc 1/01 - A.flac\" WAVE\r\n".to_vec()
        );
    }

    #[test]
    fn several_file_lines_and_an_unrelated_one() {
        let src = b"FILE \"01 a.flac\" WAVE\n  TRACK 01 AUDIO\n\
FILE \"Album Image.wav\" WAVE\n  TRACK 02 AUDIO\n\
REM FILE \"01 a.flac\"\nFILE \"02 b c.flac\" WAVE\n  TRACK 03 AUDIO\n";
        let want = b"FILE \"1-01 - a.flac\" WAVE\n  TRACK 01 AUDIO\n\
FILE \"Album Image.wav\" WAVE\n  TRACK 02 AUDIO\n\
REM FILE \"01 a.flac\"\nFILE \"1-02 - b c.flac\" WAVE\n  TRACK 03 AUDIO\n";
        assert_eq!(
            rewritten(run(
                src,
                &[("01 a.flac", "1-01 - a.flac"), ("02 b c.flac", "1-02 - b c.flac")]
            )),
            want.to_vec(),
            "the image no track maps to and the REM line stay as they are"
        );
    }

    #[test]
    fn unquoted_names_and_an_indented_lowercase_command() {
        assert_eq!(
            rewritten(run(
                b"\tfile 01.flac WAVE\n",
                &[("01.flac", "Disc 1/01 - A.flac")]
            )),
            b"\tfile \"Disc 1/01 - A.flac\" WAVE\n".to_vec()
        );
    }

    #[test]
    fn unicode_names_in_a_utf8_cue() {
        assert_eq!(
            rewritten(run(
                "TITLE \"璃月\"\r\nFILE \"01 璃月.flac\" WAVE\r\n".as_bytes(),
                &[("01 璃月.flac", "1-01 - 璃月の夜.flac")]
            )),
            "TITLE \"璃月\"\r\nFILE \"1-01 - 璃月の夜.flac\" WAVE\r\n"
                .as_bytes()
                .to_vec()
        );
    }

    #[test]
    fn a_legacy_cue_with_a_non_ascii_name_is_not_rewritten() {
        // Windows-1252 `Café.flac` — not UTF-8, so the name cannot be matched.
        assert_eq!(
            run(b"FILE \"Caf\xE9.flac\" WAVE\r\n", &[("Café.flac", "01 - Café.flac")]),
            Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding)
        );
    }

    #[test]
    fn a_legacy_cue_with_ascii_names_is_rewritten_byte_for_byte() {
        assert_eq!(
            rewritten(run(
                b"REM COMMENT \"Caf\xE9\"\r\nFILE \"01.flac\" WAVE\r\n",
                &[("01.flac", "01 - Intro.flac")]
            )),
            b"REM COMMENT \"Caf\xE9\"\r\nFILE \"01 - Intro.flac\" WAVE\r\n".to_vec()
        );
    }

    #[test]
    fn a_legacy_cue_is_never_given_a_non_ascii_name() {
        assert_eq!(
            run(
                b"REM COMMENT \"Caf\xE9\"\r\nFILE \"01.flac\" WAVE\r\n",
                &[("01.flac", "01 - Café.flac")]
            ),
            Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding)
        );
    }

    #[test]
    fn utf16_is_not_rewritten() {
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("FILE \"a.flac\" WAVE\r\n".bytes().flat_map(|b| [b, 0]))
            .collect();
        assert_eq!(
            run(&utf16, &[("a.flac", "A.flac")]),
            Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding)
        );
    }

    #[test]
    fn nothing_to_rewrite_is_unchanged() {
        assert_eq!(run(b"cue 1", &[("a.flac", "A.flac")]), Outcome::Unchanged);
        assert_eq!(
            run(b"FILE \"x.flac\" WAVE\n", &[("a.flac", "A.flac")]),
            Outcome::Unchanged
        );
        assert_eq!(
            run(b"FILE \"a.flac\" WAVE\n", &[("a.flac", "a.flac")]),
            Outcome::Unchanged,
            "a name that does not change"
        );
        assert_eq!(
            run(b"FILEX \"a.flac\"\nFILE \"a.flac\n", &[("a.flac", "A.flac")]),
            Outcome::Unchanged,
            "not a FILE command / no closing quote"
        );
    }

    #[test]
    fn a_name_with_a_quote_is_unrepresentable() {
        assert_eq!(
            run(b"FILE \"a.flac\" WAVE\n", &[("a.flac", "a\"b.flac")]),
            Outcome::NotRewritten(CueNotRewritten::Unrepresentable)
        );
    }

    #[test]
    fn relative_paths_walk_down_and_up() {
        assert_eq!(
            relative(Path::new("d/Box"), Path::new("d/Box/Disc 1/01.flac")),
            Path::new("Disc 1").join("01.flac")
        );
        assert_eq!(
            relative(Path::new("d/Box/Disc 1"), Path::new("d/Box/Disc 2/01.flac")),
            Path::new("..").join("Disc 2").join("01.flac")
        );
        assert_eq!(
            relative(Path::new("d/Box"), Path::new("d/Box/01.flac")),
            PathBuf::from("01.flac")
        );
    }

    #[test]
    fn cue_extension_any_case() {
        assert!(is_cue(Path::new("a/Box.cue")) && is_cue(Path::new("a/Box.CUE")));
        assert!(!is_cue(Path::new("a/Box.log")) && !is_cue(Path::new("a/cue")));
    }
}
```

In `crates/soul-audio-organize/src/lib.rs`, add `mod cue;` to the module list (alphabetical, after `mod apply;`) and `pub use cue::CueNotRewritten;` after `pub use apply::{…};`.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize --lib cue::
```
Expected: compiles (with dead-code warnings), every `cue::tests::*` test except possibly `nothing_to_rewrite_is_unchanged` FAILS (`expected Rewritten, got Unchanged`, `is_cue` false, wrong `relative`).

- [ ] **Step 3: Implement**

Replace the three stubs (`is_cue`, `rewrite`, `relative`) in `cue.rs` with the real code below, and add the private helpers and `plan_rewrite`:

```rust
/// Whether `path` is a cue sheet (`.cue`, any case).
pub(crate) fn is_cue(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cue"))
}

/// Rewrite the `FILE` lines of the cue sheet `bytes`. `target` gets each
/// `FILE` name as written and returns its new path relative to the cue's
/// destination folder, or `None` to keep the line. See the module docs for
/// what is kept and when a cue is [`NotRewritten`](Outcome::NotRewritten).
pub(crate) fn rewrite(bytes: &[u8], mut target: impl FnMut(&str) -> Option<PathBuf>) -> Outcome {
    if bytes.starts_with(b"\xFF\xFE") || bytes.starts_with(b"\xFE\xFF") {
        return Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding);
    }
    let (bom, body): (&[u8], &[u8]) = match bytes.strip_prefix(UTF8_BOM) {
        Some(rest) => (UTF8_BOM, rest),
        None => (b"", bytes),
    };
    let utf8 = std::str::from_utf8(body).is_ok();
    let mut out = Vec::with_capacity(bytes.len() + 64);
    out.extend_from_slice(bom);
    let mut changed = false;
    for (content, eol) in lines(body) {
        let Some(line) = parse_file_line(content) else {
            out.extend_from_slice(content);
            out.extend_from_slice(eol);
            continue;
        };
        let old = match std::str::from_utf8(line.name) {
            Ok(s) if utf8 || s.is_ascii() => s,
            _ => return Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding),
        };
        let new = match target(old) {
            None => None,
            Some(rel) => match cue_name(&rel) {
                None => return Outcome::NotRewritten(CueNotRewritten::Unrepresentable),
                Some(n) if !utf8 && !n.is_ascii() => {
                    return Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding);
                }
                Some(n) => Some(n),
            },
        };
        match new {
            Some(n) if n != old => {
                out.extend_from_slice(line.head);
                out.push(b'"');
                out.extend_from_slice(n.as_bytes());
                out.push(b'"');
                out.extend_from_slice(line.tail);
                changed = true;
            }
            _ => out.extend_from_slice(content),
        }
        out.extend_from_slice(eol);
    }
    if changed {
        Outcome::Rewritten(out)
    } else {
        Outcome::Unchanged
    }
}

/// `body` as `(content, terminator)` pairs: split after `\r\n`, `\n` or a
/// lone `\r`; the last line's terminator may be empty. Concatenating every
/// pair gives `body` back exactly.
fn lines(body: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < body.len() {
        let end = match body[i] {
            b'\n' => i + 1,
            b'\r' if body.get(i + 1) == Some(&b'\n') => i + 2,
            b'\r' => i + 1,
            _ => {
                i += 1;
                continue;
            }
        };
        out.push((&body[start..i], &body[i..end]));
        start = end;
        i = end;
    }
    if start < body.len() {
        out.push((&body[start..], &body[body.len()..]));
    }
    out
}

/// One `FILE` line, split around its file name.
struct FileLine<'a> {
    /// Everything before the name (indentation, `FILE`, whitespace).
    head: &'a [u8],
    /// The name, without quotes.
    name: &'a [u8],
    /// Everything after the name (closing quote excluded), e.g. ` WAVE`.
    tail: &'a [u8],
}

/// `line` (no terminator) as a `FILE` command, or `None`.
fn parse_file_line(line: &[u8]) -> Option<FileLine<'_>> {
    let is_ws = |b: &u8| *b == b' ' || *b == b'\t';
    let start = line.iter().position(|b| !is_ws(b))?;
    let rest = &line[start..];
    if rest.len() < 5 || !rest[..4].eq_ignore_ascii_case(b"FILE") || !is_ws(&rest[4]) {
        return None;
    }
    let name_at = start + 4 + rest[4..].iter().position(|b| !is_ws(b))?;
    if line[name_at] == b'"' {
        let close = name_at + 1 + line[name_at + 1..].iter().position(|&b| b == b'"')?;
        Some(FileLine {
            head: &line[..name_at],
            name: &line[name_at + 1..close],
            tail: &line[close + 1..],
        })
    } else {
        let end = line[name_at..]
            .iter()
            .position(is_ws)
            .map_or(line.len(), |n| name_at + n);
        Some(FileLine {
            head: &line[..name_at],
            name: &line[name_at..end],
            tail: &line[end..],
        })
    }
}

/// `rel` as a `FILE` name: components joined with `/`. `None` when it cannot
/// be written into a quoted `FILE` argument.
fn cue_name(rel: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?),
            Component::ParentDir => parts.push(".."),
            _ => return None,
        }
    }
    let name = parts.join("/");
    (!name.is_empty() && !name.contains(['"', '\r', '\n'])).then_some(name)
}

/// `to` relative to the folder `from_dir` (both under one destination root),
/// walking up with `..` where needed.
pub(crate) fn relative(from_dir: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component<'_>> = from_dir.components().collect();
    let to: Vec<Component<'_>> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c.as_os_str());
    }
    out
}

/// `dir` joined with a `FILE` name as written (`/` or `\` separated; empty and
/// `.` parts dropped).
fn join_cue_name(dir: &Path, name: &str) -> PathBuf {
    let mut p = dir.to_path_buf();
    for part in name.split(['/', '\\']).filter(|s| !s.is_empty() && *s != ".") {
        p.push(part);
    }
    p
}

/// Plan the cue side file `src` → `dst`: read it and point each `FILE` line
/// that references (relative to the cue's own folder, compared as
/// [`paths::key`]s — case-insensitive on Windows/macOS) an audio source in
/// `audio_to` (`key(src)` → its planned destination) at that destination,
/// relative to `dst`'s folder. `Ok(None)`: copy as-is; `Ok(Some(bytes))`:
/// write these instead; `Err(why)`: copy as-is and report.
pub(crate) fn plan_rewrite(
    src: &Path,
    dst: &Path,
    audio_to: &HashMap<PathBuf, PathBuf>,
) -> Result<Option<Vec<u8>>, CueNotRewritten> {
    match std::fs::metadata(src) {
        Ok(m) if m.len() <= MAX_CUE_BYTES => {}
        _ => return Err(CueNotRewritten::Unreadable),
    }
    let bytes = std::fs::read(src).map_err(|_| CueNotRewritten::Unreadable)?;
    let src_dir = src.parent().unwrap_or(Path::new(""));
    let dst_dir = dst.parent().unwrap_or(Path::new(""));
    let outcome = rewrite(&bytes, |name| {
        audio_to
            .get(&paths::key(&join_cue_name(src_dir, name)))
            .map(|to| relative(dst_dir, to))
    });
    match outcome {
        Outcome::Unchanged => Ok(None),
        Outcome::Rewritten(b) => Ok(Some(b)),
        Outcome::NotRewritten(why) => Err(why),
    }
}
```

Nothing in the library target calls this module until Task 6, so the lib build would warn `dead_code` (and clippy `-D warnings` would fail). Put this inner attribute directly below the module doc comment of `cue.rs` (before the `use` lines), **and delete it in Task 6**:

```rust
#![allow(dead_code)] // wired into plan.rs by the next task, which removes this
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize --lib cue::
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo clippy -p soul-audio-organize --all-targets -- -D warnings
```
Expected: 14 `cue::tests` PASS; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/soul-audio-organize/src/cue.rs crates/soul-audio-organize/src/lib.rs
git commit -m "feat(organize): cue sheet FILE-line rewriter"
```

---

### Task 6: Plan rewritten `.cue` side files (D, planner)

**Files:**
- Modify: `crates/soul-audio-organize/src/plan.rs`
  - imports (`:7-14`): add `use crate::cue::{self, CueNotRewritten};`
  - `PlanEntry` (`:45-56`): new field `rewritten`
  - `OrganizePlan` (`:119-140`): new field `cues_not_rewritten`
  - `plan_with`: audio push (`:274`), new `audio_to` map after the audio loop (`:277`), side-file push loop (`:348-351`), final struct (`:358-364`)
  - `Dsts::push` (`:379-405`), `classify` (`:521-539`)
- Modify: `crates/soul-audio-organize/src/cue.rs` (drop the two `#[allow(dead_code)]`)
- Create: `crates/soul-audio-organize/tests/cue_rewrite.rs`

**Interfaces:**
- Consumes (Task 5): `cue::is_cue(&Path) -> bool`, `cue::plan_rewrite(&Path, &Path, &HashMap<PathBuf, PathBuf>) -> Result<Option<Vec<u8>>, CueNotRewritten>`, `CueNotRewritten`.
- Produces (Tasks 7, 8 and the PR):
  - `pub struct PlanEntry { pub src, pub dst, pub action, pub kind, pub rewritten: Option<Vec<u8>> }`
  - `OrganizePlan { …, pub cues_not_rewritten: Vec<(PathBuf, CueNotRewritten)> }`
  - `fn classify(src: &Path, dst: &Path, strategy: Strategy, rewritten: Option<&[u8]>) -> Action` — a rewritten cue is `SkipDuplicate` only if the existing destination holds **exactly** the rewritten bytes, else `SkipConflict`.
  - `Dsts::push(&mut self, entries: &mut Vec<PlanEntry>, src: &Path, dst: PathBuf, strategy: Strategy, kind: EntryKind, rewritten: Option<Vec<u8>>)`

`EntryKind` stays `SideFile` for cues (UIs and existing tests filter on it); `rewritten.is_some()` is the flag. `audio_to` contains every audio entry except `SkipConflict` (first entry per source wins), so a re-run — audio already `SkipDuplicate` at its destination — produces the same rewritten bytes and the cue classifies `SkipDuplicate`.

- [ ] **Step 1: Write the failing integration tests**

Create `crates/soul-audio-organize/tests/cue_rewrite.rs`:

```rust
//! `.cue` `FILE` rewriting through the planner (and, from Task 7, apply):
//! the cue's planned copy names the renamed tracks, relative to the cue's new
//! folder; everything else stays byte-identical; cues that cannot be
//! rewritten safely are copied unchanged and reported.

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
    put(src, "Box/101 Intro.flac", &track("Box", "Intro", 1, Some("1/2")));
    put(src, "Box/102 Theme.flac", &track("Box", "Theme", 2, Some("1/2")));
    put(src, "Box/201 Reprise.flac", &track("Box", "Reprise", 1, Some("2/2")));
    put(src, "Box/Box.log", LOG);
    put(src, "Box/Box.cue", &eac_cue("101 Intro.flac", "102 Theme.flac"));
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
        assert_eq!(rel(&dest, &cue.dst), "HOYO-MiX/Box/Box.cue", "{preset_name}");
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
        assert!(p.cues_not_rewritten.is_empty(), "{:?}", p.cues_not_rewritten);
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
        put(&src, "Box/CD1/01 Intro.flac", &track("Box", "Intro", 1, Some("1")));
        put(&src, "Box/CD1/02 Theme.flac", &track("Box", "Theme", 2, Some("1")));
        put(&src, "Box/CD2/01 Reprise.flac", &track("Box", "Reprise", 1, Some("2")));
        put(&src, "Box/CD1/Box.cue", &eac_cue("01 Intro.flac", "02 Theme.flac"));
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
```

- [ ] **Step 2: Run them to verify they fail**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize --test cue_rewrite
```
Expected: compile errors — `no field rewritten on type &PlanEntry`, `no field cues_not_rewritten on type OrganizePlan`.

- [ ] **Step 3: Add the fields (plan.rs)**

`PlanEntry` (plan.rs:45-56) becomes:

```rust
/// One planned source → destination placement with its classified [`Action`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEntry {
    /// The source file to copy/move.
    pub src: PathBuf,
    /// Its computed destination.
    pub dst: PathBuf,
    /// How [`apply`](crate::apply) will treat this entry.
    pub action: Action,
    /// Audio input or album side file.
    pub kind: EntryKind,
    /// A `.cue` side file whose `FILE` lines name audio this plan renames:
    /// the cue with those lines pointed at the new names (see
    /// [`CueNotRewritten`] for when a cue is left as-is). When `Some`,
    /// [`apply`](crate::apply) writes these bytes at `dst` instead of copying
    /// `src` (a Move removes `src` only after the write succeeded). `None` for
    /// every other entry.
    pub rewritten: Option<Vec<u8>>,
}
```

In `OrganizePlan` (plan.rs:119-140), after `folder_inferred_discs`, add:

```rust
    /// `.cue` side files planned **unchanged** although they may name audio
    /// this plan renames, with the reason (source path, first-seen order). A
    /// UI should warn that their `FILE` lines may point at missing files.
    pub cues_not_rewritten: Vec<(PathBuf, CueNotRewritten)>,
```

Add to the imports at plan.rs:10-14: `use crate::cue::{self, CueNotRewritten};`

- [ ] **Step 4: Classify against the rewritten bytes (plan.rs)**

Replace `Dsts::push` (plan.rs:379-405) with:

```rust
    /// Classify and append one entry, claiming its destination when it will write.
    fn push(
        &mut self,
        entries: &mut Vec<PlanEntry>,
        src: &Path,
        dst: PathBuf,
        strategy: Strategy,
        kind: EntryKind,
        rewritten: Option<Vec<u8>>,
    ) {
        let key = paths::key(&dst);
        let action = if self.claimed.contains(&key) {
            Action::SkipDuplicate
        } else {
            classify(src, &dst, strategy, rewritten.as_deref())
        };
        if matches!(action, Action::Copy | Action::Move) {
            self.claimed.insert(key.clone());
        }
        self.taken.insert(key);
        entries.push(PlanEntry {
            src: src.to_path_buf(),
            dst,
            action,
            kind,
            rewritten,
        });
    }
```

Replace `classify` (plan.rs:521-539) with:

```rust
/// Classify one source against whatever currently sits on disk at `dst` (the
/// batch's own claims are checked by the caller). A `rewritten` cue is a
/// duplicate only when `dst` holds exactly those bytes (cues are small, so the
/// content is compared); any other file only by size.
fn classify(src: &Path, dst: &Path, strategy: Strategy, rewritten: Option<&[u8]>) -> Action {
    if let Ok(dst_meta) = std::fs::metadata(dst) {
        // Something already lives at `dst`: the same = treat as already
        // organized (SkipDuplicate); different = a distinct file we refuse to
        // clobber (SkipConflict).
        let same = match rewritten {
            Some(bytes) => {
                dst_meta.len() == bytes.len() as u64
                    && std::fs::read(dst).is_ok_and(|d| d == bytes)
            }
            None => std::fs::metadata(src).map(|m| m.len()).ok() == Some(dst_meta.len()),
        };
        return if same {
            Action::SkipDuplicate
        } else {
            Action::SkipConflict
        };
    }
    match strategy {
        Strategy::Copy => Action::Copy,
        Strategy::Move => Action::Move,
    }
}
```

- [ ] **Step 5: Plan the cues (plan.rs `plan_with`)**

Audio push (plan.rs:274): `dsts.push(&mut entries, src, dst, strategy, EntryKind::Audio);` → `dsts.push(&mut entries, src, dst, strategy, EntryKind::Audio, None);`

Directly after the audio loop (after plan.rs:276, before `// 2. Side files`), insert:

```rust
    // Each audio source (as a key) → where it lands: the names a cue's FILE
    // lines move to. A conflicting track does not move (its album's side
    // files stay too); the first entry for a source wins.
    let mut audio_to: HashMap<PathBuf, PathBuf> = HashMap::new();
    for e in entries.iter().filter(|e| e.action != Action::SkipConflict) {
        audio_to
            .entry(paths::key(&e.src))
            .or_insert_with(|| e.dst.clone());
    }
    let mut cues_not_rewritten: Vec<(PathBuf, CueNotRewritten)> = Vec::new();
```

Replace the side-file push loop (plan.rs:348-351)

```rust
        for (src, dst) in side::place(&placement, &side, &dsts.taken) {
            dsts.push(&mut entries, &src, dst, strategy, EntryKind::SideFile);
            prunable.push(true);
        }
```
with
```rust
        for (src, dst) in side::place(&placement, &side, &dsts.taken) {
            let rewritten = if cue::is_cue(&src) {
                cue::plan_rewrite(&src, &dst, &audio_to).unwrap_or_else(|why| {
                    cues_not_rewritten.push((src.clone(), why));
                    None
                })
            } else {
                None
            };
            dsts.push(&mut entries, &src, dst, strategy, EntryKind::SideFile, rewritten);
            prunable.push(true);
        }
```

In the final `OrganizePlan { … }` (plan.rs:358-364) add `cues_not_rewritten,` after `folder_inferred_discs: …,`.

In `cue.rs`, delete the `#![allow(dead_code)]` line added in Task 5.

Update the plan module doc (plan.rs:1-5) to:

```rust
//! Planning: group the inputs into albums (the importer's identity rule), read
//! each file's tags, compute its destination via the [`Template`], add each
//! album's side files (rewriting a `.cue`'s `FILE` lines in memory), and
//! classify every [`Action`] — **without** mutating the filesystem. `plan` only
//! *reads* tags and `.cue` side files, *lists* album folders and *stats* (for a
//! rewritten cue: reads) any existing destination; it never creates directories
//! or copies bytes.
```

- [ ] **Step 6: Run the tests**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize
```
Expected:
- every `cue_rewrite` test PASSES (6 on Windows/macOS, 5 on Linux) and every unit / `side_files` test PASSES;
- `multi_disc_e2e` `flat_tagged_copy` and `mixed_copy` FAIL at "re-run is all SkipDuplicate" on `Box.cue` (`SkipConflict`). This is expected: `flat_box`'s `Box.cue` (`FILE "101 Intro.flac" WAVE`) is now planned rewritten, but `apply` still copies the original bytes until Task 7, so the re-plan finds different content at the destination.

**Do not commit here.** Tasks 6 and 7 are committed together at Task 7 Step 7.

- [ ] **Step 7: Lint**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo clippy -p soul-audio-organize --all-targets -- -D warnings
```
Expected: clean. (Commit happens at the end of Task 7.)

---

### Task 7: `apply` writes rewritten cues (D, executor)

**Files:**
- Modify: `crates/soul-audio-organize/src/apply.rs:1-14` (docs, imports), `:87-94` (`apply_one`), new `do_write` + `write_new` after `do_move` (`:147`), tests module (`:149-355`)
- Modify: `crates/soul-audio-organize/tests/cue_rewrite.rs` (append; remove `_unused`)

**Interfaces:**
- Consumes (Task 6): `PlanEntry.rewritten: Option<Vec<u8>>`.
- Produces: `fn do_write(entry: &PlanEntry, bytes: &[u8], summary: &mut Summary)` and `fn write_new(dst: &Path, bytes: &[u8]) -> std::io::Result<()>` (both private). Semantics: never over an existing destination (`create_new`); on a write error the partial destination is removed and the source is untouched; a Move removes the source **only after** the write succeeded, and a failed removal is recorded in `Summary::failed` (both files then exist — never both missing). Counts: Copy → `copied`, Move → `moved`, existing destination → `skipped`.

- [ ] **Step 1: Write the failing unit tests**

In `apply.rs`'s `tests` module, change the imports to

```rust
    use super::{Progress, apply};
    use crate::{Action, EntryKind, OrganizePlan, PlanEntry, Strategy, Template, plan, presets};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
```

and append:

```rust
    /// A one-entry plan writing `bytes` (a rewritten cue) for `src` → `dst`.
    fn cue_plan(src: &Path, dst: &Path, action: Action, bytes: &[u8]) -> OrganizePlan {
        OrganizePlan {
            entries: vec![PlanEntry {
                src: src.to_path_buf(),
                dst: dst.to_path_buf(),
                action,
                kind: EntryKind::SideFile,
                rewritten: Some(bytes.to_vec()),
            }],
            ..Default::default()
        }
    }

    const OLD: &[u8] = b"FILE \"a.flac\" WAVE\r\n";
    const NEW: &[u8] = b"FILE \"01 - A.flac\" WAVE\r\n";

    #[test]
    fn a_rewritten_cue_copy_writes_the_new_bytes_and_keeps_the_source() {
        let dir = unique_dir("cue-copy");
        let src = dir.join("a.cue");
        std::fs::write(&src, OLD).unwrap();
        let dst = join(&dir, &["out", "a.cue"]);
        let s = apply(&cue_plan(&src, &dst, Action::Copy, NEW), |_| {}).unwrap();
        assert_eq!((s.copied, s.moved, s.skipped), (1, 0, 0));
        assert!(s.failed.is_empty(), "{:?}", s.failed);
        assert_eq!(std::fs::read(&dst).unwrap(), NEW);
        assert_eq!(std::fs::read(&src).unwrap(), OLD);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rewritten_cue_move_writes_then_removes_the_source() {
        let dir = unique_dir("cue-move");
        let src = dir.join("a.cue");
        std::fs::write(&src, OLD).unwrap();
        let dst = join(&dir, &["out", "a.cue"]);
        let p = cue_plan(&src, &dst, Action::Move, NEW);
        let s = apply(&p, |_| {}).unwrap();
        assert_eq!((s.copied, s.moved, s.skipped), (0, 1, 0));
        assert_eq!(std::fs::read(&dst).unwrap(), NEW);
        assert!(!src.exists());
        // Re-applying the same plan is a no-op.
        let again = apply(&p, |_| {}).unwrap();
        assert_eq!((again.moved, again.skipped), (0, 1));
        assert!(again.failed.is_empty(), "{:?}", again.failed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rewritten_cue_never_overwrites() {
        let dir = unique_dir("cue-exists");
        let src = dir.join("a.cue");
        std::fs::write(&src, OLD).unwrap();
        let dst = dir.join("b.cue");
        std::fs::write(&dst, b"someone else's").unwrap();
        let s = apply(&cue_plan(&src, &dst, Action::Move, NEW), |_| {}).unwrap();
        assert_eq!((s.moved, s.skipped), (0, 1));
        assert_eq!(std::fs::read(&dst).unwrap(), b"someone else's");
        assert_eq!(std::fs::read(&src).unwrap(), OLD, "the source stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_cue_write_keeps_the_source() {
        let dir = unique_dir("cue-fail");
        let src = dir.join("a.cue");
        std::fs::write(&src, OLD).unwrap();
        // The destination's parent is a FILE: the write cannot happen.
        std::fs::write(dir.join("blocker"), b"x").unwrap();
        let dst = join(&dir, &["blocker", "a.cue"]);
        let s = apply(&cue_plan(&src, &dst, Action::Move, NEW), |_| {}).unwrap();
        assert_eq!((s.moved, s.failed.len()), (0, 1));
        assert_eq!(std::fs::read(&src).unwrap(), OLD, "never both missing");
        assert!(!dst.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Write the failing integration tests**

Append to `tests/cue_rewrite.rs`:

```rust
#[test]
fn copy_writes_the_rewritten_cue_and_a_rerun_skips_it() {
    for preset_name in ["artist-album", "artist-album+disc-folders"] {
        let (src, dest) = (unique_dir("cue-copy-src"), unique_dir("cue-copy-dest"));
        eac_box(&src);
        let initial = tree(&src);
        let p = plan_box(&src, &dest, preset_name, Strategy::Copy);
        let want = entry(&p, &at(&src, "Box/Box.cue")).rewritten.clone().unwrap();
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
```

- [ ] **Step 3: Run them to verify they fail**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize --lib apply::
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize --test cue_rewrite
```
Expected: `a_rewritten_cue_copy_writes_the_new_bytes_and_keeps_the_source` and `…_move_writes_then_removes_the_source` FAIL (dst holds `OLD`); `copy_writes_the_rewritten_cue_and_a_rerun_skips_it` and `move_writes_the_rewritten_cue_…` FAIL on the cue bytes.

- [ ] **Step 4: Implement**

In `apply.rs`, change the imports (line 12-14) to:

```rust
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::{Action, OrganizePlan, PlanEntry};
```

Replace `apply_one` (line 87-94):

```rust
/// Apply a single entry, folding its outcome into `summary`.
fn apply_one(entry: &PlanEntry, summary: &mut Summary) {
    match (entry.action, &entry.rewritten) {
        (Action::Copy | Action::Move, Some(bytes)) => do_write(entry, bytes, summary),
        (Action::Copy, None) => do_copy(entry, summary),
        (Action::Move, None) => do_move(entry, summary),
        (Action::SkipDuplicate | Action::SkipConflict, _) => summary.skipped += 1,
    }
}
```

After `do_move` (after line 147) add:

```rust
/// A rewritten cue ([`PlanEntry::rewritten`]): write `bytes` at `dst` — never
/// over an existing file — and, for a Move, remove the source only once the
/// write succeeded. A failed write leaves the source untouched (and no partial
/// destination); a failed removal leaves both files and is recorded.
fn do_write(entry: &PlanEntry, bytes: &[u8], summary: &mut Summary) {
    if !ensure_parent(entry, summary) {
        return;
    }
    if entry.dst.exists() {
        summary.skipped += 1;
        return;
    }
    if let Err(e) = write_new(&entry.dst, bytes) {
        summary.failed.push((entry.src.clone(), e.to_string()));
        return;
    }
    if entry.action != Action::Move {
        summary.copied += 1;
        return;
    }
    match std::fs::remove_file(&entry.src) {
        Ok(()) => summary.moved += 1,
        Err(e) => summary.failed.push((
            entry.src.clone(),
            format!(
                "wrote {} but could not remove the source: {e}",
                entry.dst.display()
            ),
        )),
    }
}

/// Create `dst` (failing if it exists) holding exactly `bytes`; on error the
/// partial file is removed.
fn write_new(dst: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    let res = f.write_all(bytes).and_then(|()| f.sync_all());
    if res.is_err() {
        drop(f);
        let _ = std::fs::remove_file(dst);
    }
    res
}
```

Update the module doc (apply.rs:1-10): after "copying or moving each file into place," insert "— a rewritten `.cue` ([`PlanEntry::rewritten`](crate::PlanEntry::rewritten)) is written from the plan's bytes instead —". Final first sentence:

```rust
//! Execution: the one mutating layer. [`apply`] walks an
//! [`OrganizePlan`](crate::OrganizePlan), copying or moving each file into
//! place — a rewritten `.cue` ([`PlanEntry::rewritten`](crate::PlanEntry::rewritten))
//! is written from the plan's bytes instead, its source removed only after the
//! write on a Move — reporting [`Progress`] per entry and tallying a
//! [`Summary`]. It is **idempotent** — …
```
(keep the rest of the paragraph unchanged).

The e2e matrix's step "2. Nothing was modified" compares every destination with its **source** bytes, which a rewritten cue no longer matches. Make its snapshot use the planned bytes now (Task 8 adds the explicit cue assertions). In `crates/soul-audio-organize/tests/multi_disc_e2e.rs` `run_case`, replace

```rust
    let snapshot: Vec<(std::path::PathBuf, Vec<u8>)> = p
        .entries
        .iter()
        .map(|e| (e.dst.clone(), std::fs::read(&e.src).unwrap()))
        .collect();
```
with
```rust
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
```

- [ ] **Step 5: Run the whole crate**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize
```
Expected: all PASS, including the full e2e matrix (its Copy re-run now finds the rewritten cue at the destination → `SkipDuplicate`; the Move re-apply skips it).

- [ ] **Step 6: Lint**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo clippy -p soul-audio-organize --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo doc --no-deps -p soul-audio-organize
```
Expected: both clean.

- [ ] **Step 7: Commit (Tasks 6 + 7 together)**

```bash
git add crates/soul-audio-organize/src/plan.rs crates/soul-audio-organize/src/cue.rs crates/soul-audio-organize/src/apply.rs crates/soul-audio-organize/tests/cue_rewrite.rs crates/soul-audio-organize/tests/multi_disc_e2e.rs
git commit -m "feat(organize): rewrite .cue FILE lines to the renamed tracks"
```

---

### Task 8: E2E matrix asserts the rewritten cue; docs state the new rule (D, e2e + docs)

**Files:**
- Modify: `crates/soul-audio-organize/tests/multi_disc_e2e.rs:22` (imports), `:44-58` (after `track`: `eac_cue`), `:98` (`flat_box` cue), `:104-120` (`DiscSubfolders` cue), `:166-187` (`album_files` DiscSubfolders), `:284-424` (`run_case`)
- Modify: `crates/soul-audio-organize/src/lib.rs:14-19`, `:27-47` (crate docs)
- Modify: `crates/soul-audio-organize/src/side.rs:1-6` (module doc)
- Modify: `crates/soul-audio-organize/src/plan.rs:17-30` (`Action::SkipDuplicate` doc), `:32-43` (`EntryKind::SideFile` doc), `plan_with` doc (`:193-231`)
- Check: `docs/ARCHITECTURE.md` (no side-file/cue rule today — `grep -n -i "cue\|side file\|never modified" docs/ARCHITECTURE.md` returned nothing at plan time; re-run and update only if it now matches)

**Interfaces:**
- Consumes: `PlanEntry.rewritten`, `OrganizePlan.cues_not_rewritten` (Task 6).
- Produces: nothing new for code.

- [ ] **Step 1: Make the fixtures EAC-style (RED)**

In `multi_disc_e2e.rs`:

Imports (line 22): `use support::{at, audio_files, flac, mp3, put, rel, tree, unique_dir};`

After `fn track(…)` add:

```rust
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
        (Layout::DiscSubfolders, true) => {
            ("Disc 1/Box.cue", eac_cue("01 - Intro.flac", "02 - Theme.flac"))
        }
        (_, true) => (
            "Box.cue",
            eac_cue("Disc 1/01 - Intro.flac", "Disc 1/02 - Theme.flac"),
        ),
    }
}
```

In `flat_box`, replace `put(src, "Box/Box.cue", b"FILE \"101 Intro.flac\" WAVE");` with

```rust
    put(src, "Box/Box.cue", &eac_cue("101 Intro.flac", "102 Theme.flac"));
```

In `build_source`'s `Layout::DiscSubfolders` arm, after `put(src, "Box/CD2/Box.log", b"disc 2 log!");` add

```rust
            put(src, "Box/CD1/Box.cue", &eac_cue("01 Intro.flac", "02 Theme.flac"));
```

In `album_files`, add `"Box.cue",` to the `("Box", Layout::DiscSubfolders, false)` list (after `"Disc 2 - Box.log",`) and `"Disc 1/Box.cue",` to the `("Box", Layout::DiscSubfolders, true)` list (after `"Disc 2/Box.log",`).

- [ ] **Step 2: Assert the cue and the log in `run_case`**

In `run_case`, after `let opts = PlanOptions::new(&src);` add `let df = preset_name.ends_with("+disc-folders");`. (The snapshot already uses `e.rewritten` since Task 7.)

Directly before `// Apply.` insert:

```rust
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
```

After the existing "2. Nothing was modified on the way" loop add:

```rust
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
```

Update the comment heading step 2 (the loop over `snapshot`) to `// 2. Nothing else was modified on the way (audio and side files byte for byte; a rewritten cue holds exactly the planned bytes).` and extend the file's top doc: in line 4, `that nothing was modified,` → `that nothing but a .cue's FILE lines was modified (the EAC cue names the renamed tracks, the log stays byte-identical),`.

- [ ] **Step 3: Run the matrix**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize --test multi_disc_e2e
```
Expected: all 6 matrix tests and the extra cases PASS. RED check: temporarily change `expected_cue`'s `(_, false)` arm to `eac_cue("101 Intro.flac", "102 Theme.flac")` (the un-rewritten names) — `flat_tagged_copy` must then FAIL at `rewritten cue`; revert. The organize ∘ import round-trip assertion (step 5 in `run_case`) is untouched and still passes.

- [ ] **Step 4: Docs — the new side-file rule**

`side.rs:1-6` module doc becomes:

```rust
//! Side files: the non-audio files that travel with an album — rip logs and cue
//! sheets, covers, `Scans/`, lyrics, `.txt`/`.nfo`. Discovery walks the album
//! root; placement decides each file's destination in the album's new folder.
//! Side files are only copied/moved, never modified (a `.log`'s checksum must
//! stay valid) — except a `.cue` sheet, whose `FILE` lines the planner points
//! at the renamed tracks (`crate::cue`).
```

`plan.rs` `EntryKind::SideFile` doc (lines 38-42):

```rust
    /// A non-audio file travelling with its album (`.log`, `.cue`, covers,
    /// `Scans/…`, lyrics, `.txt`/`.nfo`). Copied/moved byte for byte — except
    /// a `.cue` whose `FILE` lines name renamed tracks, which is written with
    /// those lines rewritten ([`PlanEntry::rewritten`]). Renamed only when it
    /// shares a track's stem (it follows that track) or its name must be made
    /// unique / filesystem-safe.
    SideFile,
```

`plan.rs` `Action::SkipDuplicate` doc (lines 21-24):

```rust
    /// Skip: the destination is already claimed by an **earlier** entry in this
    /// batch, or already present on disk at the **same byte length** (a
    /// rewritten cue: with exactly the rewritten bytes) — treated as
    /// already-organized; see the size heuristic on [`plan`].
    SkipDuplicate,
```

`plan_with` doc: after the **Side files** paragraph add

```rust
/// **Cue sheets**: a planned `.cue` side file whose `FILE` lines name inputs
/// of this plan (relative to the cue's folder; case-insensitive on
/// Windows/macOS) gets those lines pointed at the inputs' new destinations,
/// relative to the cue's own destination folder, in
/// [`PlanEntry::rewritten`]; everything else in it (BOM, line endings, other
/// commands, `REM` lines) is kept byte for byte, and a `FILE` line naming
/// anything else is kept as-is. A cue that is not UTF-8 is rewritten only when
/// the names involved are ASCII; otherwise — or when a new name cannot be
/// written — it is planned unchanged and listed in
/// [`OrganizePlan::cues_not_rewritten`]. Every other side file is never
/// modified.
```

and in its **Duplicate heuristic** section replace "The already-exists check compares **sizes only**, not a content hash;" with "The already-exists check compares **sizes only**, not a content hash (a rewritten cue: its exact bytes);".

`lib.rs` crate doc: in the Layers paragraph (lines 14-19) replace "logs, cues, covers, scans, lyrics — and never mutates)" with "logs, cues, covers, scans, lyrics; a cue's `FILE` lines are pointed at the renamed tracks — and never mutates)". In **Known limitations** append:

```rust
//! - **Cue sheets left behind.** A `.cue` is rewritten only when its album's
//!   side files move with the album. One left in place
//!   ([`OrganizePlan::side_files_left`]) keeps naming the old paths, and one
//!   that cannot be rewritten safely (legacy encoding with non-ASCII names) is
//!   moved unchanged and listed in [`OrganizePlan::cues_not_rewritten`].
//! - **Partial selections.** Selecting only some tracks of an album (or an
//!   album folder holding audio this build never lists as an input) moves its
//!   tracks but leaves all its side files in place
//!   ([`SideFilesLeft::PartialSelection`]).
```

`docs/ARCHITECTURE.md`: run `grep -n -i "cue\|side file\|side-file\|never modified" docs/ARCHITECTURE.md`. If a line states that side files are never modified, change it to "…never modified, except a `.cue`'s `FILE` lines (rewritten to the renamed tracks)". If nothing matches (the state at planning time), leave the file alone.

- [ ] **Step 5: Verify docs and the crate**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo test -p soul-audio-organize
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo clippy -p soul-audio-organize --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo doc --no-deps -p soul-audio-organize
```
Expected: all PASS / clean (no broken intra-doc links to `PlanEntry::rewritten`, `OrganizePlan::cues_not_rewritten`, `SideFilesLeft::PartialSelection`).

- [ ] **Step 6: Commit**

```bash
git add crates/soul-audio-organize/tests/multi_disc_e2e.rs crates/soul-audio-organize/src/lib.rs crates/soul-audio-organize/src/side.rs crates/soul-audio-organize/src/plan.rs docs/ARCHITECTURE.md
git commit -m "test(organize): e2e matrix asserts the rewritten EAC cue; document the cue rule"
```
(Drop `docs/ARCHITECTURE.md` from `git add` if unchanged.)

---

### Task 9: Gate and PR notes

**Files:** none (verification + PR body).

**Interfaces:**
- Consumes: everything above.
- Produces: a green `cargo xtask ci`, a pushed branch, PR body text.

- [ ] **Step 1: Format**

```bash
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo fmt --all
git diff --stat
```
If `fmt` changed anything, commit it: `git commit -am "style(organize): cargo fmt"`.

- [ ] **Step 2: Full gate**

```bash
cd /d/dev/soulaudio/core-followups
CARGO_TARGET_DIR=E:/cargo-target/core-followups cargo xtask ci
```
Expected: ends without `xtask failed:`; every `+ cargo …` step succeeds, including (if Task 1 Step 5 added it) the three `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps …` steps. Record the number of test suites and the wall time for the PR.

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin feat/organize-followups
gh pr create --title "feat(organize): partial selections, {Album}/{DiscDir} segment detection, .cue FILE rewriting; clean rustdoc" --body-file <scratch file with the body below>
```

PR body (fill the bracketed facts from Steps 2 and Task 1 Step 5; no attribution lines):

```markdown
Follow-ups from #133 (multi-disc albums).

## Behaviour
- **Partial selections no longer sweep side files.** If the album root — or one of its disc folders — holds audio that is not among the inputs, none of the album's side files are planned; the root is reported with the new `SideFilesLeft::PartialSelection`. Selecting the rest later sweeps them as before.
  - Interpretation: an unselected disc folder (`CD2/` with no input) counts as a partial selection; an album root holding audio the app never lists as an input (e.g. an undecodable `.wma`) is therefore always partial.
- **`{DiscDir}` is a disc-folder layout only in a directory segment** (`{Album}/{DiscDir} {TrackNo}` keeps all discs in one folder → `folder_inferred_discs` reported, disc-folder side files placed flat).
- **The ` (n)` of a second same-title album goes on the `{Album}` folder** (`{AlbumArtist}/{Album}/{Year}/…` → `Box (2)/2020`, not `Box/2020 (2)`).
- **`.cue` `FILE` lines are rewritten** to the renamed/moved tracks, relative to the cue's new folder (`Disc 1/01 - Intro.flac` when tracks move into disc folders; `/` separator, always quoted). Everything else in the cue is byte-identical (BOM, CRLF/LF, REM, other commands); a `FILE` naming a file this plan does not place is kept. Non-UTF-8 cues are rewritten only with ASCII names; otherwise copied unchanged and reported. `.log` and every other side file stay byte-identical. Supersedes the "side files are never modified" rule for `.cue` only.
  - Duplicate detection for a rewritten cue compares the destination against the **rewritten bytes** (exact content), not size.
  - `apply` writes the bytes with create-new; a Move removes the source cue only after the write succeeded (never both missing).
- rustdoc: `cargo doc --no-deps -D warnings` is clean for soul-audio-index (`--features build`), soul-audio-import and soul-audio-organize [and is now part of `cargo xtask ci` | not gated: N s].

## API additions (soul-audio-organize)
- `SideFilesLeft::PartialSelection` (enum is `#[non_exhaustive]`).
- `CueNotRewritten { UnsupportedEncoding, Unrepresentable, Unreadable }` (`#[non_exhaustive]`), re-exported at the crate root.
- `OrganizePlan.cues_not_rewritten: Vec<(PathBuf, CueNotRewritten)>` (constructors using `..Default::default()` are unaffected).
- **Breaking for struct literals:** `PlanEntry.rewritten: Option<Vec<u8>>` (no new `EntryKind`; cues stay `EntryKind::SideFile`). soul-player-ng's `scene_organize_confirm` (`app/src/main.rs`) builds `PlanEntry` literals and needs `rewritten: None`.

## soul-player-ng follow-ups
- Add `rewritten: None` to the `PlanEntry` literals.
- Label `SideFilesLeft::PartialSelection` in the confirm modal ("only some tracks selected — side files stay").
- Show `cues_not_rewritten` as a warning.
- Spec `docs/superpowers/specs/2026-11-25-multi-disc-albums-design.md` lists ".cue rewriting" as out of scope / side files never modified — update it to the new rule.

## Gate
`cargo xtask ci`: [N suites, all green, M min].
```

- [ ] **Step 4: Verify the PR**

```bash
gh pr view --json title,body,headRefName | head -40
```
Expected: title/body as above, head `feat/organize-followups`.

---

## Self-review notes (for the executor)

- Spec coverage: C → Task 1; B (`has_disc_dir`) → Task 2; B (`{Album}` suffix) → Task 3; A → Task 4; D rewriter/encodings/CRLF/BOM/unicode/unrelated FILE/quoted names → Task 5; D plan field + duplicate semantics + case-insensitive matching + single-image → Task 6; D apply + Move safety + idempotence → Task 7; D e2e (one-folder + disc-folders, log byte-identical, round-trip) + docs → Task 8; gate + PR notes → Task 9.
- Names used consistently: `Collected { files, partial }`, `render_album_folder`, `album_segment`, `under`, `cue::{is_cue, rewrite, relative, plan_rewrite, Outcome}`, `CueNotRewritten::{UnsupportedEncoding, Unrepresentable, Unreadable}`, `PlanEntry.rewritten`, `OrganizePlan.cues_not_rewritten`, `do_write`, `write_new`.
- Tasks 6 and 7 commit together (Task 6 alone would leave the e2e Copy re-run red while `apply` still copies the original cue).
