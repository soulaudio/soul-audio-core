//! Manifest-driven tier enforcement — the machine half of the committed
//! architecture (`docs/ARCHITECTURE.md` §3/§7). Every workspace crate is
//! classified into a tier; dependencies may only flow **downward**; the `no_std`
//! tiers carry no std adapter; the DAP firmware never pulls in a std adapter.
//!
//! This is deliberately general so it catches **future** drift: a new crate must
//! be added to `MANIFEST` (or the test fails), and it can then only depend in
//! ways its tier allows. Planned-but-unbuilt crates are listed here too and are
//! skipped until they appear in the resolved graph — so when they land, they are
//! forced into their documented tier.
//!
//! To change a tier or a rule: update `docs/ARCHITECTURE.md` §2/§3 first, then
//! this file. A PR that moves a crate between tiers without touching the doc is
//! wrong by construction.

use cargo_metadata::{DependencyKind, MetadataCommand, Package};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tier {
    /// no_std, no-alloc, platform-agnostic. Deps ⊆ {embedded-io} ∪ Kernel.
    Kernel,
    /// no_std + alloc shared logic. Deps ⊆ {embedded-io, …} ∪ Kernel ∪ NoStdAlloc.
    NoStdAlloc,
    /// std desktop/server adapters. Deps ⊆ Kernel ∪ NoStdAlloc ∪ StdAdapter.
    StdAdapter,
    /// no_std DAP binary. May depend on Kernel/NoStdAlloc + embedded HAL — but
    /// NEVER a StdAdapter (the device stays lean).
    Firmware,
    /// Test/tooling. May depend on anything; nothing may depend on it.
    Tests,
}

/// The classification. Keep in sync with `docs/ARCHITECTURE.md` §3. Entries for
/// crates not yet in the workspace are reserved and simply skipped until built.
const MANIFEST: &[(&str, Tier)] = &[
    // --- Kernel (T0) ---
    ("soul-audio-core", Tier::Kernel),
    ("soul-audio-tags", Tier::Kernel),
    ("soul-audio-catalog", Tier::Kernel),
    ("soul-audio-index", Tier::Kernel), // default surface = the no-alloc reader
    // --- no_std + alloc (T1) ---
    ("soul-audio-queue", Tier::NoStdAlloc),
    ("soul-audio-engine", Tier::NoStdAlloc),
    ("soul-audio-control", Tier::NoStdAlloc),
    // --- Firmware (DAP) ---
    ("soul-audio-firmware", Tier::Firmware),
    // --- Std adapters (desktop/server) ---
    ("soul-audio-desktop", Tier::StdAdapter),
    ("soul-audio-service", Tier::StdAdapter),
    ("soul-audio-cli", Tier::StdAdapter),
    ("soul-audio-import", Tier::StdAdapter),
    // reserved (planned) — enforced the moment they land in the workspace:
    ("soul-audio-artbake", Tier::StdAdapter),
    ("soul-audio-remote", Tier::StdAdapter),
    ("soul-audio-server", Tier::StdAdapter),
    ("soul-audio-multicatalog", Tier::StdAdapter),
    ("soul-audio-mutable", Tier::StdAdapter),
    // --- Tests ---
    ("architecture-tests", Tier::Tests),
];

/// External (non-workspace) normal deps a Kernel crate may carry.
const KERNEL_EXTERNAL_ALLOWLIST: &[&str] = &["embedded-io"];

fn tier_of(name: &str) -> Option<Tier> {
    MANIFEST.iter().find(|(n, _)| *n == name).map(|&(_, t)| t)
}

/// May a crate in `from` depend on a workspace crate in `to`?
fn may_depend(from: Tier, to: Tier) -> bool {
    use Tier::*;
    match from {
        Kernel => to == Kernel,
        NoStdAlloc => matches!(to, Kernel | NoStdAlloc),
        StdAdapter => matches!(to, Kernel | NoStdAlloc | StdAdapter),
        // The DAP must stay lean: Kernel/NoStdAlloc only, never a std adapter.
        Firmware => matches!(to, Kernel | NoStdAlloc),
        Tests => true,
    }
}

fn normal_deps(p: &Package) -> impl Iterator<Item = &str> {
    p.dependencies
        .iter()
        .filter(|d| d.kind == DependencyKind::Normal)
        .map(|d| d.name.as_str())
}

/// Workspace member packages we require to be classified (ours + the tests
/// crate; `xtask` and anything else is tooling and left alone).
fn is_ours(name: &str) -> bool {
    name.starts_with("soul-audio-") || name == "architecture-tests"
}

#[test]
fn every_crate_is_classified() {
    let md = MetadataCommand::new().exec().unwrap();
    for id in &md.workspace_members {
        let p = &md[id];
        if is_ours(p.name.as_str()) {
            assert!(
                tier_of(p.name.as_str()).is_some(),
                "{} is not classified in tiers.rs — add it to MANIFEST and \
                 docs/ARCHITECTURE.md §3 (a new crate must declare its tier)",
                p.name
            );
        }
    }
}

#[test]
fn dependencies_flow_downward_only() {
    let md = MetadataCommand::new().exec().unwrap();
    let ws: std::collections::HashSet<&str> = md
        .workspace_members
        .iter()
        .map(|id| md[id].name.as_str())
        .collect();

    for id in &md.workspace_members {
        let p = &md[id];
        let Some(from) = tier_of(p.name.as_str()) else {
            continue; // non-ours tooling (xtask); `every_crate_is_classified` covers ours
        };
        for dep in normal_deps(p) {
            if ws.contains(dep) {
                // Internal edge: must be classified and tier-legal.
                let to = tier_of(dep).unwrap_or_else(|| {
                    panic!("{} depends on unclassified workspace crate {dep}", p.name)
                });
                assert!(
                    may_depend(from, to),
                    "ARCHITECTURE VIOLATION: {} ({from:?}) must not depend on {dep} ({to:?}). \
                     Dependencies flow downward only (docs/ARCHITECTURE.md §3).",
                    p.name
                );
            } else if from == Tier::Kernel {
                // External edge from the kernel: tightly allowlisted.
                assert!(
                    KERNEL_EXTERNAL_ALLOWLIST.contains(&dep),
                    "{} is Kernel (no_std, no-alloc) and may not take external dep {dep} \
                     (allowed: {KERNEL_EXTERNAL_ALLOWLIST:?}). Move platform deps to an adapter.",
                    p.name
                );
            }
        }
    }
}

#[test]
fn nothing_depends_on_the_tests_crate() {
    let md = MetadataCommand::new().exec().unwrap();
    for id in &md.workspace_members {
        let p = &md[id];
        for dep in normal_deps(p) {
            assert!(
                tier_of(dep) != Some(Tier::Tests),
                "{} depends on a Tests-tier crate ({dep}) — tests sit at the top, \
                 nothing may depend on them",
                p.name
            );
        }
    }
}
