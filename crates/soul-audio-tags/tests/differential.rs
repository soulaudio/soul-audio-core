//! Differential parity test: our `read_owned` vs `lofty` — the mature crate
//! `soul-audio-service` uses in production (`crates/soul-audio-service/src/library.rs`)
//! — on real, committed, lofty-parseable audio fixtures. This is the
//! de-risking check for dropping lofty (Task 10): if our from-scratch
//! parsers disagree with it on real files, we need to know before that cut.
//!
//! `std`-only: `lofty` and `embedded-io-adapters/std` are dev-dependencies,
//! meaningful only once `feature = "std"` brings in `read_owned` (behind
//! `alloc`). Two independent gates keep the default (no-feature, no_std)
//! build untouched:
//!   1. this whole file is `#![cfg(feature = "std")]`, and
//!   2. `Cargo.toml` marks the `differential` test target
//!      `required-features = ["std"]`, so Cargo skips building it — and
//!      therefore never compiles `lofty` — at all without that feature.
#![cfg(feature = "std")]

use std::borrow::Cow;
use std::fs::File;
use std::path::{Path, PathBuf};

use embedded_io_adapters::std::FromStd;
use lofty::file::TaggedFileExt;
use lofty::tag::Accessor;

/// (title, artist, album), the three fields this test compares.
type Fields = (Option<String>, Option<String>, Option<String>);

/// Read (title, artist, album) via lofty's primary/first tag. Mirrors
/// `soul_audio_service::library::read_tags` (the code this crate is meant to
/// replace) so the comparison is against what the service actually does
/// today, not just "some lofty API".
fn lofty_fields(path: &Path) -> Fields {
    let s = |f: Option<Cow<'_, str>>| f.map(|c| c.into_owned());
    match lofty::read_from_path(path) {
        Ok(tagged) => {
            let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
            match tag {
                Some(t) => (s(t.title()), s(t.artist()), s(t.album())),
                None => (None, None, None),
            }
        }
        Err(_) => (None, None, None),
    }
}

/// Read (title, artist, album) via our own `read_owned`, over a
/// `std::fs::File` bridged to `embedded_io::{Read, Seek}` through
/// `embedded-io-adapters` — the same bridge `soul-audio-desktop` uses for
/// real files (`crates/soul-audio-desktop/src/sources.rs`).
///
/// A container our reader has no tag support for at all (e.g. a bare ADTS
/// AAC elementary stream — it carries no tag structure of its own) comes
/// back `Err(Unsupported)`; normalized to all-`None` here, since for this
/// comparison "no container we can read tags from" and "a container with no
/// tags in it" are the same observable outcome (title/artist/album
/// unavailable either way).
fn our_fields(path: &Path) -> Fields {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut src = FromStd::new(file);
    match soul_audio_tags::read_owned(&mut src) {
        Ok((tags, _facts)) => (tags.title, tags.artist, tags.album),
        Err(_) => (None, None, None),
    }
}

/// `crates/soul-audio-tags` -> workspace root.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/soul-audio-tags -> workspace root")
        .to_path_buf()
}

/// Real, committed, lofty-parseable fixtures — no hand-crafted files.
/// `testdata/tagged.flac` carries known title/artist/album (the same values
/// `soul-audio-service`'s library-scan test asserts); the `tone.*` fixtures
/// carry no tags at all (ffmpeg wrote them with no `-metadata`), so both
/// readers agreeing on `None` there is still a valid parity point, not a
/// vacuous one. The `soul-audio-core/testdata` files round out formats/codecs
/// (VBR MP3, ALAC, plain AAC-in-M4A, FLAC-in-Ogg) not otherwise present under
/// the top-level `testdata/`.
fn committed_fixtures() -> Vec<PathBuf> {
    let root = workspace_root();
    let top = root.join("testdata");
    let core = root.join("crates").join("soul-audio-core").join("testdata");
    [
        "tagged.flac",
        "tone.flac",
        "tone.mp3",
        "tone.dsf",
        "tone.m4a",
        "tone-aac.m4a",
        "tone.ogg",
        "tone.wav",
        "tone.aac",
    ]
    .into_iter()
    .map(|n| top.join(n))
    .chain(
        [
            "vbr-44100-stereo.mp3",
            "aac.m4a",
            "alac.m4a",
            "flac-in-ogg.ogg",
        ]
        .into_iter()
        .map(|n| core.join(n)),
    )
    .collect()
}

#[test]
fn matches_lofty_on_committed_fixtures() {
    let mut failures = Vec::new();
    for path in committed_fixtures() {
        assert!(path.is_file(), "missing fixture: {}", path.display());
        let lof = lofty_fields(&path);
        let ours = our_fields(&path);
        if lof != ours {
            failures.push(format!(
                "{}: lofty={:?} ours={:?}",
                path.display(),
                lof,
                ours
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "parity mismatches vs lofty:\n{}",
        failures.join("\n")
    );
}

/// Recursively collect every file path under `dir` (symlinks not followed;
/// unreadable subdirectories are silently skipped rather than failing the
/// whole walk — the point is a best-effort sweep of a real library, not a
/// filesystem-correctness check).
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "dsf", "dff", "wav", "ogg", "opus", "m4a", "aac",
];

/// Local-library parity sweep. Not run by default (needs a real music
/// library on disk, so it never runs in CI) — set `SOUL_TAGS_CORPUS` to a
/// directory and run with `--ignored`:
///
/// ```sh
/// SOUL_TAGS_CORPUS=D:\content\music cargo test -p soul-audio-tags \
///     --features std --test differential -- --nocapture --ignored
/// ```
///
/// Walks every file under the directory, compares our `read_owned` against
/// lofty on title/artist/album, and prints a match/mismatch tally plus a
/// sample of any mismatches (path + both readings) — the real de-risking
/// signal for dropping lofty in Task 10.
#[test]
#[ignore = "needs a real local music library; set SOUL_TAGS_CORPUS and run with --ignored"]
fn corpus_parity_report() {
    let Ok(root) = std::env::var("SOUL_TAGS_CORPUS") else {
        eprintln!("SOUL_TAGS_CORPUS not set; skipping corpus sweep");
        return;
    };
    let root = PathBuf::from(root);
    assert!(
        root.is_dir(),
        "SOUL_TAGS_CORPUS is not a directory: {}",
        root.display()
    );

    let mut all = Vec::new();
    walk(&root, &mut all);

    let mut scanned = 0usize;
    let mut matched = 0usize;
    let mut matched_with_tags = 0usize;
    let mut mismatches: Vec<String> = Vec::new();

    for path in &all {
        let is_audio = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
        if !is_audio {
            continue;
        }
        scanned += 1;

        let lof = lofty_fields(path);
        let ours = our_fields(path);

        if lof == ours {
            matched += 1;
            if lof != (None, None, None) {
                matched_with_tags += 1;
            }
        } else {
            mismatches.push(format!(
                "{}: lofty={:?} ours={:?}",
                path.display(),
                lof,
                ours
            ));
        }
    }

    println!(
        "corpus parity: {scanned} audio files scanned, {matched} matched \
         ({matched_with_tags} with real tag values), {} mismatched",
        mismatches.len()
    );
    let sample_n = 50;
    for line in mismatches.iter().take(sample_n) {
        println!("  MISMATCH {line}");
    }
    if mismatches.len() > sample_n {
        println!("  ... and {} more", mismatches.len() - sample_n);
    }

    // Informational sweep, not a pass/fail gate: a handful of real-world
    // mismatches (unusual multi-value frames, odd encodings) are expected
    // and worth knowing about without failing the whole suite over them.
    // Read the printed tally/sample (run with --nocapture) to judge parity.
}
