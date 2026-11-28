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
