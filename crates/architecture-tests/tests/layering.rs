//! Enforce architectural layering via the resolved dependency graph:
//! `soul-audio-core` stays dependency-lean and platform-agnostic, and the
//! shells depend on it (never the reverse).

use cargo_metadata::{DependencyKind, MetadataCommand, Package};

/// Normal (non-dev, non-build) dependency names `soul-audio-core` may have.
const CORE_ALLOWLIST: &[&str] = &["embedded-io"];

fn pkg<'a>(md: &'a cargo_metadata::Metadata, name: &str) -> &'a Package {
    md.packages
        .iter()
        .find(|p| p.name.as_str() == name)
        .unwrap_or_else(|| panic!("package {name} not found in workspace metadata"))
}

fn normal_dep_names(p: &Package) -> Vec<String> {
    p.dependencies
        .iter()
        // `cargo_metadata` lists an optional dependency's manifest entry
        // regardless of whether the feature that gates it is active (e.g.
        // soul-audio-index's `build` -> soul-audio-artbake). Excluding
        // `optional` deps here is what makes this helper reflect the
        // default/no_std surface instead of every feature-gated manifest line.
        .filter(|d| d.kind == DependencyKind::Normal && !d.optional)
        .map(|d| d.name.clone())
        .collect()
}

#[test]
fn core_only_depends_on_allowlist() {
    let md = MetadataCommand::new().exec().unwrap();
    let core = pkg(&md, "soul-audio-core");
    for dep in normal_dep_names(core) {
        assert!(
            CORE_ALLOWLIST.contains(&dep.as_str()),
            "soul-audio-core has a disallowed normal dependency: {dep}"
        );
    }
}

#[test]
fn engine_depends_on_core_and_stays_lean() {
    let md = MetadataCommand::new().exec().unwrap();

    // core must not depend on the engine (layering points one way).
    let core_deps = normal_dep_names(pkg(&md, "soul-audio-core"));
    assert!(
        !core_deps.contains(&"soul-audio-engine".to_string()),
        "soul-audio-core must not depend on soul-audio-engine"
    );

    // The engine sits directly on core and carries nothing else — it stays
    // `no_std` and platform-agnostic like core.
    let engine_deps = normal_dep_names(pkg(&md, "soul-audio-engine"));
    assert_eq!(
        engine_deps,
        vec!["soul-audio-core".to_string()],
        "soul-audio-engine should depend on soul-audio-core and nothing else"
    );
}

#[test]
fn shells_depend_on_core_not_the_reverse() {
    let md = MetadataCommand::new().exec().unwrap();
    let core_deps = normal_dep_names(pkg(&md, "soul-audio-core"));

    for shell in [
        "soul-audio-cli",
        "soul-audio-desktop",
        "soul-audio-service",
        "soul-audio-firmware",
    ] {
        assert!(
            !core_deps.contains(&shell.to_string()),
            "soul-audio-core must not depend on {shell}"
        );
        let deps = normal_dep_names(pkg(&md, shell));
        assert!(
            deps.contains(&"soul-audio-core".to_string()),
            "{shell} should depend on soul-audio-core"
        );
    }
}

// --- Shared data layer (catalog + index) boundaries. These are the
// machine-enforced half of the "shared 1:1, platform via adapters" design (see
// docs/superpowers/specs/2026-11-08-shared-catalog-index-architecture.md). The
// no_std/no-alloc/thumbv7em build gates enforce the other half. ---

/// `soul-audio-catalog` (the shared model + ports) must stay as lean as
/// core/engine so it builds no_std for the DAP.
const CATALOG_ALLOWLIST: &[&str] = &["soul-audio-core", "embedded-io"];
/// The `soul-audio-index` reader may additionally sit on the catalog.
const INDEX_ALLOWLIST: &[&str] = &["soul-audio-core", "soul-audio-catalog", "embedded-io"];

#[test]
fn catalog_stays_lean() {
    let md = MetadataCommand::new().exec().unwrap();
    for dep in normal_dep_names(pkg(&md, "soul-audio-catalog")) {
        assert!(
            CATALOG_ALLOWLIST.contains(&dep.as_str()),
            "soul-audio-catalog has a disallowed normal dependency: {dep} \
             (the shared model must stay no_std-able — move platform deps to an adapter)"
        );
    }
}

#[test]
fn index_sits_on_core_and_catalog_only() {
    let md = MetadataCommand::new().exec().unwrap();
    for dep in normal_dep_names(pkg(&md, "soul-audio-index")) {
        assert!(
            INDEX_ALLOWLIST.contains(&dep.as_str()),
            "soul-audio-index has a disallowed normal dependency: {dep} \
             (the reader is no-alloc; the std baker lives behind the `build` feature)"
        );
    }
}

#[test]
fn core_does_not_depend_on_shared_data_layer() {
    let md = MetadataCommand::new().exec().unwrap();
    let core_deps = normal_dep_names(pkg(&md, "soul-audio-core"));
    for data_crate in ["soul-audio-catalog", "soul-audio-index"] {
        assert!(
            !core_deps.contains(&data_crate.to_string()),
            "soul-audio-core must not depend on {data_crate} (layering points one way)"
        );
    }
}

#[test]
fn index_reader_has_no_art_build_deps() {
    let md = MetadataCommand::new().exec().unwrap();
    let deps = normal_dep_names(pkg(&md, "soul-audio-index"));
    assert!(
        !deps.contains(&"soul-audio-artbake".to_string()),
        "soul-audio-index must not NORMALLY depend on soul-audio-artbake (it is build-only); got {deps:?}"
    );
    assert!(
        !deps.contains(&"image".to_string()),
        "the `image` crate must stay behind artbake's `build` feature, off the no_std index reader; got {deps:?}"
    );
}

#[test]
fn import_sits_above_the_shared_layer() {
    let md = MetadataCommand::new().exec().unwrap();
    // The desktop importer (std) sits on the shared layer + the tag reader.
    let import_deps = normal_dep_names(pkg(&md, "soul-audio-import"));
    for needed in [
        "soul-audio-core",
        "soul-audio-catalog",
        "soul-audio-index",
        "soul-audio-tags",
    ] {
        assert!(
            import_deps.contains(&needed.to_string()),
            "soul-audio-import should depend on {needed}"
        );
    }
    // The shared no_std layer must never depend on the std importer (it's a
    // leaf desktop adapter — the DAP builds without it).
    for shared in ["soul-audio-core", "soul-audio-catalog", "soul-audio-index"] {
        assert!(
            !normal_dep_names(pkg(&md, shared)).contains(&"soul-audio-import".to_string()),
            "{shared} must not depend on soul-audio-import"
        );
    }
}
