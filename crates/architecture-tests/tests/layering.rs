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
        .filter(|d| d.kind == DependencyKind::Normal)
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
