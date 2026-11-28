//! Fitness function: **no new panicking constructs** in the crates the DAP runs.
//!
//! On the desktop a panic is a crash and a bug report. On the player it is a
//! dead device mid-album, with no console to read and nothing to skip past —
//! and the crates scanned here are exactly the ones in that binary. The whole
//! untrusted-input effort (#151, #153, #154, #156–#159) was about removing
//! panics reachable from a file; this keeps the result from quietly eroding.
//!
//! # What this is and is not
//!
//! It is a **ratchet**, not a proof. `unwrap`/`expect`/`panic!`/`unreachable!`
//! are sometimes the honest expression of an invariant the type system cannot
//! carry — `self.current.as_ref().unwrap()` after the caller has already
//! established there is a current track, or `comm[8..18].try_into().unwrap()`
//! on a fixed-size array, which is infallible and only looks fallible. Banning
//! them outright would push authors toward *worse* code (silent fallbacks that
//! hide a broken invariant instead of reporting it). So the gate pins the
//! audited count and makes any increase deliberate.
//!
//! Eleven of the thirteen crates below are at **zero** — including
//! `soul-audio-firmware`, the device binary — and that is the valuable part:
//! for them this is not a budget but an absolute, and a single new `unwrap` in
//! the firmware, the engine or the catalog reader fails CI.
//!
//! `todo!`/`unimplemented!` are **banned everywhere**, with no budget: an
//! unfinished branch in a shipping decoder is not an invariant, it is a hole.
//!
//! # If this test fails
//!
//! It names the file and the delta. Either remove the construct, or — if it
//! genuinely expresses an invariant — raise that crate's number here **in the
//! same commit**, with the reasoning in the commit message. Removals never fail
//! the gate (the counts are ceilings), so refactoring down is free.
//!
//! # Known limitation
//!
//! The scanner is textual: it reads each file up to the first `#[cfg(test)]`
//! (or `#[cfg(all(test, …))]`) attribute, which is where every file in this
//! workspace puts its tests. It therefore cannot see a panicking construct
//! *below* that line, and does not understand macros. It is a ratchet on the
//! obvious cases, which is what makes it cheap enough to keep green.

use std::path::{Path, PathBuf};

/// Per-crate ceilings for `unwrap`/`expect`/`panic!`/`unreachable!` in
/// production code, audited at the commit that introduced this file.
///
/// # Scope: what "runs on the DAP" means here
///
/// The list is the set `cargo xtask verify` cross-builds for
/// `thumbv7em-none-eabihf` — which is the only authoritative answer, since that
/// build is what must link for the device — plus the `no_std` libraries those
/// crates pull in (`soul-audio-catalog` and `soul-audio-index` are dependencies
/// of the firmware binary; `soul-audio-eq` is a dependency of
/// `soul-audio-dsp`). A crate absent from both is desktop-only and deliberately
/// not gated: `soul-audio-multicatalog`, for instance, carries two
/// mutex-poisoning `expect`s that are perfectly reasonable on a host and cannot
/// reach a player.
///
/// # The numbers
///
/// `soul-audio-core`'s 46 break down as: deck invariants where a caller has
/// already established the state (11), `dff`'s DST-substream accessors behind
/// an `Option` the construction guarantees (8), AAC dispatch `unreachable!`s
/// whose arms are chosen by the caller (7), `try_into().unwrap()` on
/// constant-width slices of fixed-size arrays in the Ogg/Vorbis/DST readers
/// (15), and five more of the same two shapes. `soul-audio-tags`'s 1 is
/// `comm[8..18]` on a `[u8; 18]` local — infallible.
///
/// **Everything else is zero, including the firmware binary itself.** That is
/// the part of this gate with teeth: eleven of thirteen crates have no
/// panicking construct at all, so for them this is an absolute and a single
/// new `unwrap` fails CI.
const BUDGET: &[(&str, usize)] = &[
    // Cross-built for thumbv7em by `xtask verify`.
    ("soul-audio-core", 46),
    ("soul-audio-tags", 1),
    ("soul-audio-artbake", 0),
    ("soul-audio-control", 0),
    ("soul-audio-dsp", 0),
    ("soul-audio-engine", 0),
    ("soul-audio-firmware", 0),
    ("soul-audio-loudness", 0),
    ("soul-audio-playlist", 0),
    ("soul-audio-queue", 0),
    // `no_std` libraries the above link: catalog + index are firmware
    // dependencies, eq is a `soul-audio-dsp` dependency.
    ("soul-audio-catalog", 0),
    ("soul-audio-index", 0),
    ("soul-audio-eq", 0),
];

/// Matches the attribute that opens a test module, in either spelling used in
/// this workspace (`#[cfg(test)]`, `#[cfg(all(test, feature = "…"))]`).
fn is_cfg_test(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("#[cfg(test)]") || t.starts_with("#[cfg(all(test")
}

/// Does this line hold a panicking construct? Comment lines are skipped, so a
/// doc example or a note mentioning `unwrap()` does not count.
fn panicking(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with("*") {
        return false;
    }
    t.contains(".unwrap()")
        || t.contains(".expect(")
        || t.contains("panic!(")
        || t.contains("unreachable!(")
}

fn forbidden(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("//") || t.starts_with("*") {
        return false;
    }
    t.contains("todo!(") || t.contains("unimplemented!(")
}

fn workspace_root() -> PathBuf {
    // crates/architecture-tests -> workspace root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/architecture-tests -> workspace root")
        .to_path_buf()
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.filter_map(Result::ok) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// `(panicking count, forbidden count)` in one file's production region.
fn scan(path: &Path) -> (usize, usize) {
    let text = std::fs::read_to_string(path).expect("read source");
    let mut panics = 0;
    let mut banned = 0;
    for line in text.lines() {
        if is_cfg_test(line) {
            break; // tests follow; they may panic freely
        }
        if panicking(line) {
            panics += 1;
        }
        if forbidden(line) {
            banned += 1;
        }
    }
    (panics, banned)
}

#[test]
fn production_code_adds_no_new_panicking_constructs() {
    let root = workspace_root();
    let mut failures = Vec::new();

    for &(crate_name, budget) in BUDGET {
        let src = root.join("crates").join(crate_name).join("src");
        assert!(
            src.is_dir(),
            "{crate_name}/src not found — did the crate move? \
             Update this gate rather than deleting the entry."
        );

        let mut total = 0;
        let mut per_file: Vec<(String, usize)> = Vec::new();
        for f in rust_files(&src) {
            let (n, _) = scan(&f);
            if n > 0 {
                let rel = f.strip_prefix(&root).unwrap_or(&f).display().to_string();
                per_file.push((rel, n));
                total += n;
            }
        }

        if total > budget {
            per_file.sort_by(|a, b| b.1.cmp(&a.1));
            let detail: Vec<String> = per_file
                .iter()
                .map(|(f, n)| format!("      {n:3}  {f}"))
                .collect();
            failures.push(format!(
                "  {crate_name}: {total} panicking construct(s), budget {budget} \
                 (+{})\n{}",
                total - budget,
                detail.join("\n")
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "new panicking constructs in production code:\n{}\n\n\
         On the DAP a panic is a dead player mid-album. Either remove the \
         construct, or — if it genuinely expresses an invariant the types \
         cannot carry — raise that crate's number in \
         `crates/architecture-tests/tests/panic_budget.rs` in the same commit, \
         and say why in the message.",
        failures.join("\n")
    );
}

/// `todo!`/`unimplemented!` are banned outright: an unfinished branch in a
/// shipping decoder is a hole, not an invariant, and it panics on whatever
/// input reaches it.
#[test]
fn no_unfinished_branches_anywhere() {
    let root = workspace_root();
    let mut found = Vec::new();
    for &(crate_name, _) in BUDGET {
        let src = root.join("crates").join(crate_name).join("src");
        for f in rust_files(&src) {
            let (_, banned) = scan(&f);
            if banned > 0 {
                let rel = f.strip_prefix(&root).unwrap_or(&f).display().to_string();
                found.push(format!("  {banned} in {rel}"));
            }
        }
    }
    assert!(
        found.is_empty(),
        "todo!/unimplemented! in production code:\n{}",
        found.join("\n")
    );
}

/// The scanner itself, pinned: a gate that silently stops counting is worse
/// than no gate. If the workspace adopts a different `cfg(test)` spelling this
/// fails here rather than by quietly reporting zero everywhere.
#[test]
fn the_scanner_counts_what_it_claims_to() {
    // Production constructs are counted...
    assert!(panicking("    let x = foo().unwrap();"));
    assert!(panicking("    let x = foo().expect(\"why\");"));
    assert!(panicking("    panic!(\"nope\");"));
    assert!(panicking(
        "        _ => unreachable!(\"dispatched above\"),"
    ));
    // ...comments and docs are not...
    assert!(!panicking("    // foo().unwrap() would panic here"));
    assert!(!panicking(
        "    /// Returns `None` instead of `unwrap()`-ing."
    ));
    assert!(!panicking("    //! panic!() is banned in this module"));
    // ...and the test-module boundary is recognised in both spellings.
    assert!(is_cfg_test("#[cfg(test)]"));
    assert!(is_cfg_test("    #[cfg(test)]"));
    assert!(is_cfg_test("#[cfg(all(test, feature = \"alloc\"))]"));
    assert!(!is_cfg_test("#[cfg(feature = \"std\")]"));
    assert!(forbidden("    todo!();"));
    assert!(forbidden("    unimplemented!(\"later\");"));
    assert!(!forbidden("    // todo!() one day"));

    // And it finds real files: `soul-audio-core` is known to have some, so a
    // zero here means the scan path or the walker broke.
    let core = workspace_root().join("crates/soul-audio-core/src");
    let total: usize = rust_files(&core).iter().map(|f| scan(f).0).sum();
    assert!(
        total > 0,
        "scanned soul-audio-core and found nothing — the walker is broken"
    );
}
