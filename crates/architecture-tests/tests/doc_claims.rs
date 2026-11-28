//! Fitness function: the **docs that are specifications** must agree with the
//! code they specify.
//!
//! `docs/ARCHITECTURE.md` names the `soul-audio-index` module doc as the
//! canonical `.soulidx` byte layout, and anything else that reads a card — the
//! desktop baker, a sync server, a tool in another language — is written from
//! those two documents. A number that drifts there is not a typo, it is a wrong
//! specification, and the drift is silent because nothing in a normal build
//! reads prose.
//!
//! It drifted exactly that way and this is why the gate exists: format v9
//! appended the two per-track cue fields and `RECORD_SIZE` became 99, while
//! `ARCHITECTURE.md` still announced **v8** as current. Someone baking cards
//! from the stale doc would have produced an index this reader treats as an
//! empty catalog — no error, no audio, nothing to debug but a silent library.
//!
//! The sizes and the record stride are checked inside `soul-audio-index` itself
//! (its tests read its own module doc, which stays packageable). This file
//! holds the invariants that span *files*, which no single crate owns.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/architecture-tests -> workspace root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[test]
fn the_architecture_doc_states_the_current_index_format_version() {
    let arch = read("docs/ARCHITECTURE.md");
    let want = format!("**v{}**", soul_audio_index::FORMAT_VERSION);
    assert!(
        arch.contains(&want),
        "docs/ARCHITECTURE.md does not announce the current `.soulidx` format as \
         {want}. It is the document that calls the `soul-audio-index` module doc \
         canonical, so a stale version here sends someone baking cards that the \
         reader will treat as an empty catalog."
    );
}

/// The architecture doc points at the module doc as canonical. If that pointer
/// ever stops resolving, the spec has no home and the check above is cosmetic.
#[test]
fn the_architecture_doc_points_at_a_spec_that_exists() {
    let arch = read("docs/ARCHITECTURE.md");
    let pointer = "crates/soul-audio-index/src/lib.rs";
    assert!(
        arch.contains(pointer),
        "docs/ARCHITECTURE.md no longer names {pointer} as the canonical layout"
    );
    let spec = read(pointer);
    for label in ["HEADER", "TRACK RECORD", "ALBUM RECORD", "FOLDER RECORD"] {
        assert!(
            spec.contains(&format!("//! {label} (")),
            "the canonical spec no longer documents a {label}"
        );
    }
}

/// Every container `open` can build must appear in the README's format table.
///
/// The README is where someone decides whether this crate reads their library,
/// and it had been wrong in both directions at once: it omitted **AIFF,
/// WavPack, CAF and Matroska/WebM** entirely — four containers `open` has
/// handled all along — while advertising **AAC** with no hint that it is
/// patent-gated off by default, so a default build declines the very files the
/// table promised.
///
/// The keyword map is maintained by hand on purpose: adding a format means
/// adding a row, and forgetting fails here rather than shipping a README that
/// undersells the crate. It is keyed on the `Format` variants in
/// `soul-audio-core`'s probe, so a new variant with no keyword is also a
/// failure.
#[test]
fn every_container_open_handles_is_in_the_readme_table() {
    // (probe `Format` variant, keyword that must appear in the README)
    const CONTAINERS: &[(&str, &str)] = &[
        ("Wav", "**WAV**"),
        ("Aiff", "**AIFF / AIFF-C**"),
        ("Flac", "**FLAC**"),
        ("OggFlac", "FLAC-in-Ogg"),
        ("Mp3", "**MP3**"),
        ("Mp4", "**ALAC**"), // the MP4 arm's always-on codec
        ("Adts", "**AAC**"),
        ("Opus", "**Opus**"),
        ("Vorbis", "**Vorbis**"),
        ("WavPack", "**WavPack**"),
        ("Caf", "**CAF**"),
        ("Matroska", "**Matroska / WebM**"),
        ("Dsf", "**DSD**"),
        ("Dff", "**DSD**"),
    ];

    let readme = read("README.md");
    for (variant, keyword) in CONTAINERS {
        assert!(
            readme.contains(keyword),
            "README has no format-table entry for `Format::{variant}` \
             (expected {keyword:?}) — a container `open` handles must be \
             listed, or users cannot tell this crate reads their files"
        );
    }

    // And the map must cover the enum: a new `Format` variant without a
    // keyword here would otherwise pass silently. Parsed out of the `pub enum
    // Format { … }` block in `model.rs` — my first draft read `probe.rs`,
    // where the enum is *not* defined, so this loop asserted nothing at all.
    // The variant-count floor below is what would have caught that.
    let model = read("crates/soul-audio-core/src/model.rs");
    let body = model
        .split_once("pub enum Format {")
        .expect("`pub enum Format` moved — update this gate")
        .1
        .split_once("\n}")
        .expect("unterminated `pub enum Format`")
        .0;
    let variants: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter_map(|l| l.strip_suffix(','))
        .filter(|n| {
            !n.is_empty()
                && n.chars().next().is_some_and(char::is_uppercase)
                && n.chars().all(char::is_alphanumeric)
        })
        .collect();
    assert!(
        variants.len() >= 12,
        "only {} `Format` variants parsed ({variants:?}) — the parse broke and \
         this check would pass vacuously",
        variants.len()
    );
    for name in variants {
        assert!(
            CONTAINERS.iter().any(|(v, _)| *v == name),
            "`Format::{name}` is not in this test's keyword map — add it \
             (and a README row) or the format table can drift again"
        );
    }
}

/// The README must disclose that AAC is off by default. The table lists
/// AAC-LC/HE-AAC/HE-AACv2 prominently, and a reader who takes that at face
/// value gets `Unsupported` from a default build — which looks like a bug in
/// the crate rather than a licensing decision.
#[test]
fn the_readme_discloses_the_aac_feature_gate() {
    let readme = read("README.md");
    assert!(
        readme.contains("`aac` feature"),
        "the README advertises AAC without naming the `aac` feature it needs"
    );
    assert!(
        readme.contains("off by default"),
        "the README does not say AAC is off by default"
    );
}
