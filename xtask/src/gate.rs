//! Change-scoped gating for the pre-push hook and `dev`-targeting CI.
//!
//! The full gate (`verify()` / `cargo xtask ci`) builds + tests the whole
//! workspace and cross-builds every no_std crate for `thumbv7em` — tens of
//! minutes. That stays the **authoritative** gate and runs unchanged for
//! anything landing on `main` (the `dev -> main` PR and pushes to `main`).
//!
//! For day-to-day feature work — PRs into `dev`, local `git push` — we run only
//! what the change touches: `clippy` + tests for the changed **host** crates. A
//! change to a *shared* no_std crate (which feeds the DAP cross-builds) or to a
//! workspace-level file (`xtask/`, `.github/`, root `Cargo.toml`,
//! `rust-toolchain.toml`, …) falls back to the full gate. CI on `dev -> main`
//! is the backstop for anything the scoped local/PR gate doesn't rebuild
//! (e.g. a reverse-dependency of a changed leaf crate).

use std::collections::BTreeSet;
use std::process::Command;

use crate::{run, verify};

/// no_std / shared crates that feed the `thumbv7em` cross-builds (or are
/// depended on by crates that do). A change to any of these runs the FULL gate,
/// because a scoped host-only run can't prove the embedded build still links.
const SHARED_CRATES: &[&str] = &[
    "soul-audio-core",
    "soul-audio-catalog",
    "soul-audio-index",
    "soul-audio-artbake",
    "soul-audio-engine",
    "soul-audio-queue",
    "soul-audio-control",
    "soul-audio-loudness",
    "soul-audio-dsp",
    // soul-audio-eq is strictly no_std / no-alloc (its own module doc) and feeds
    // soul-audio-dsp, soul-audio-core's `Equalizer` and the DAP firmware. It was
    // missing here, which meant a change to it took the SCOPED gate: a host-only
    // `cargo test -p soul-audio-eq` that cannot prove the thumbv7em cross-build
    // still links. It also happens to own every value
    // `architecture-tests/tests/signal_integrity.rs` pins — `Band::affects`'
    // 1e-3 tolerance, `coeffs`, `format_autoeq`'s precision — so its absence let
    // the crate under test skip the tests that guard it.
    "soul-audio-eq",
    "soul-audio-playlist",
    "soul-audio-tags",
    "soul-audio-firmware",
];

fn git(args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("spawning git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn ref_exists(r: &str) -> bool {
    git(&["rev-parse", "--verify", "--quiet", r])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// The base ref to diff against: an explicit `--base <ref>`, else the first of
/// `origin/dev` / `origin/main` that exists (feature branches fork from `dev`).
fn base_ref(args: &[String]) -> Option<String> {
    if let Some(i) = args.iter().position(|a| a == "--base") {
        return args.get(i + 1).cloned();
    }
    ["origin/dev", "origin/main"]
        .into_iter()
        .find(|r| ref_exists(r))
        .map(str::to_string)
}

/// Files this branch changes vs `base` (merge-base..HEAD) plus any uncommitted
/// edits — exactly what a push would carry.
fn changed_files(base: &str) -> Result<Vec<String>, String> {
    let mb = git(&["merge-base", base, "HEAD"]).unwrap_or_else(|_| base.to_string());
    let mb = mb.trim();
    let committed = git(&["diff", "--name-only", &format!("{mb}..HEAD")])?;
    let uncommitted = git(&["diff", "--name-only", "HEAD"])?;
    let mut v: BTreeSet<String> = BTreeSet::new();
    for line in committed.lines().chain(uncommitted.lines()) {
        if !line.is_empty() {
            v.insert(line.to_string());
        }
    }
    Ok(v.into_iter().collect())
}

enum Scope {
    /// Run the full gate; the string is the reason (logged).
    Full(String),
    /// Run scoped clippy+test on these host packages (possibly empty = docs only).
    Packages(Vec<String>),
}

/// Map changed files to a gating scope. Non-crate, non-doc files force the full
/// gate; a shared no_std crate forces it; otherwise collect the host packages.
fn classify(files: &[String]) -> Scope {
    let mut pkgs = BTreeSet::new();
    for f in files {
        if let Some(rest) = f.strip_prefix("crates/") {
            let pkg = rest.split('/').next().unwrap_or("");
            if pkg.is_empty() {
                return Scope::Full(format!("unclassified path: {f}"));
            }
            if SHARED_CRATES.contains(&pkg) {
                return Scope::Full(format!("shared no_std crate changed ({pkg})"));
            }
            pkgs.insert(pkg.to_string());
        } else if f.starts_with("docs/") || f.ends_with(".md") || f == "LICENSE" {
            // Docs never affect a build — ignore.
        } else {
            // xtask/, .github/, root Cargo.toml, Cargo.lock, rust-toolchain.toml, …
            return Scope::Full(format!("workspace-level change ({f})"));
        }
    }
    Scope::Packages(pkgs.into_iter().collect())
}

/// Cross-crate fitness functions, run by the scoped gate NO MATTER what changed.
///
/// Scoping tests by "which crate changed" is the right call for a crate's own
/// unit tests and a category error for these. `architecture-tests` asserts
/// properties of the COMPOSITION — layering, the no-alloc surface, the tier
/// split, and (since `signal_integrity.rs`) that a bypassed stage is bit-exact
/// and that the desktop -> DAP profile transport is lossless. Any crate change
/// can break a property like that, including a change in a crate the test file
/// never names, so there is no file-path rule that would select them correctly.
///
/// Cheap enough to be unconditional: the suite is pure computation over small
/// buffers, no fixtures and no I/O.
const ALWAYS_TESTED: &[&str] = &["architecture-tests"];

fn scoped_clippy_and_test(pkgs: &[String]) -> Result<(), String> {
    let mut clippy = vec!["clippy"];
    for p in pkgs {
        clippy.push("-p");
        clippy.push(p);
    }
    clippy.extend(["--all-targets", "--", "-D", "warnings"]);
    run(&clippy)?;

    let mut test = vec!["test"];
    for p in pkgs {
        test.push("-p");
        test.push(p);
    }
    // The cross-crate gates, always — see ALWAYS_TESTED. Skipped only if the
    // scope already names them, so the flag is not passed twice.
    for p in ALWAYS_TESTED {
        if !pkgs.iter().any(|x| x == p) {
            test.push("-p");
            test.push(p);
        }
    }
    run(&test)
}

/// `cargo xtask verify` — the scoped gate (pre-push hook, `dev` PRs).
pub fn verify_scoped(args: &[String]) -> Result<(), String> {
    // Formatting is global and cheap — always enforce it tree-wide.
    run(&["fmt", "--all", "--", "--check"])?;

    let Some(base) = base_ref(args) else {
        println!("verify: no origin/dev or origin/main to diff against -> full gate");
        return verify();
    };
    let files = changed_files(&base)?;
    if files.is_empty() {
        println!("verify: no changes vs {base} -> fmt only");
        return Ok(());
    }
    match classify(&files) {
        Scope::Full(reason) => {
            println!("verify: {reason} -> full gate");
            verify()
        }
        Scope::Packages(pkgs) if pkgs.is_empty() => {
            println!("verify: only docs/non-build files changed -> fmt only");
            Ok(())
        }
        Scope::Packages(pkgs) => {
            println!("verify: scoped to {pkgs:?} (full gate runs in CI on dev->main / main)");
            scoped_clippy_and_test(&pkgs)?;
            println!("verify: scoped OK");
            Ok(())
        }
    }
}

/// `cargo xtask precommit` — fmt + clippy scoped to the STAGED crates.
pub fn precommit() -> Result<(), String> {
    run(&["fmt", "--all", "--", "--check"])?;
    let staged = git(&["diff", "--cached", "--name-only"])?;
    let files: Vec<String> = staged
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if files.is_empty() {
        return Ok(());
    }
    match classify(&files) {
        Scope::Full(_) => run(&["clippy", "--all-targets", "--", "-D", "warnings"]),
        Scope::Packages(pkgs) if pkgs.is_empty() => Ok(()),
        Scope::Packages(pkgs) => {
            let mut clippy = vec!["clippy"];
            for p in &pkgs {
                clippy.push("-p");
                clippy.push(p);
            }
            clippy.extend(["--all-targets", "--", "-D", "warnings"]);
            run(&clippy)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Scope, classify};

    /// Classify a file set, returning the scoped package list or the full-gate
    /// reason as `Err`.
    fn scope(files: &[&str]) -> Result<Vec<String>, String> {
        let f: Vec<String> = files.iter().map(|s| (*s).to_string()).collect();
        match classify(&f) {
            Scope::Packages(p) => Ok(p),
            Scope::Full(r) => Err(r),
        }
    }

    #[test]
    fn leaf_crate_scopes_to_itself() {
        assert_eq!(
            scope(&["crates/soul-audio-multicatalog/src/lib.rs"]).unwrap(),
            vec!["soul-audio-multicatalog".to_string()]
        );
    }

    #[test]
    fn multiple_leaf_crates_collect_deduped() {
        assert_eq!(
            scope(&[
                "crates/soul-audio-import/src/lib.rs",
                "crates/soul-audio-cli/src/main.rs",
                "crates/soul-audio-import/Cargo.toml",
            ])
            .unwrap(),
            vec![
                "soul-audio-cli".to_string(),
                "soul-audio-import".to_string()
            ]
        );
    }

    /// soul-audio-eq must force the full gate like every other shared no_std
    /// crate. It did not, and the consequence was two-fold: a host-only scoped
    /// run could not prove the thumbv7em cross-build still linked, and the crate
    /// owning `Band::affects`' tolerance, `coeffs` and `format_autoeq`'s
    /// precision could change without running the signal-integrity gates that
    /// pin exactly those values.
    #[test]
    fn the_eq_crate_forces_full_because_it_is_shared_and_no_std() {
        assert!(
            scope(&["crates/soul-audio-eq/src/lib.rs"]).is_err(),
            "a change to soul-audio-eq must force the FULL gate: it is strictly no_std, it feeds \
             the DAP cross-build, and it owns the values signal_integrity.rs pins"
        );
    }

    /// The cross-crate fitness functions must run whatever the scope is.
    ///
    /// `classify` is about which crate changed; `architecture-tests` is about the
    /// composition, so no file-path rule selects it correctly. Without this, a
    /// change to a leaf crate would run that crate's tests and silently skip
    /// every architectural invariant.
    #[test]
    fn the_cross_crate_gates_are_never_scoped_out() {
        assert!(
            super::ALWAYS_TESTED.contains(&"architecture-tests"),
            "architecture-tests must be in ALWAYS_TESTED — its assertions are about the \
             composition, not about any one crate, so scoping them by changed path is a category \
             error"
        );
        // And a leaf-crate change must still be a SCOPED run (not full), so the
        // always-tested list is doing real work rather than being masked by
        // everything forcing full anyway.
        assert!(
            scope(&["crates/soul-audio-multicatalog/src/lib.rs"]).is_ok(),
            "a leaf crate should still scope, otherwise ALWAYS_TESTED is untested in practice"
        );
    }

    #[test]
    fn shared_crate_forces_full() {
        assert!(scope(&["crates/soul-audio-index/src/lib.rs"]).is_err());
        assert!(scope(&["crates/soul-audio-core/src/decode.rs"]).is_err());
        assert!(scope(&["crates/soul-audio-firmware/src/main.rs"]).is_err());
    }

    #[test]
    fn workspace_level_files_force_full() {
        for f in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "xtask/src/main.rs",
            ".github/workflows/ci-rust.yml",
            "lefthook.yml",
        ] {
            assert!(scope(&[f]).is_err(), "{f} should force full");
        }
    }

    #[test]
    fn docs_only_is_empty_scope() {
        assert_eq!(
            scope(&["README.md", "docs/superpowers/specs/x.md"]).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_shared_change_dominates_a_leaf_change() {
        assert!(
            scope(&[
                "crates/soul-audio-multicatalog/src/lib.rs",
                "crates/soul-audio-tags/src/lib.rs",
            ])
            .is_err()
        );
    }

    #[test]
    fn docs_alongside_a_leaf_change_keep_the_leaf() {
        assert_eq!(
            scope(&["README.md", "crates/soul-audio-service/src/library.rs",]).unwrap(),
            vec!["soul-audio-service".to_string()]
        );
    }
}
