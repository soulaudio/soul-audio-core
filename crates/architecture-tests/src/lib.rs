//! Home for cross-cutting architecture / invariant tests (see `tests/`).
//!
//! - `no_alloc` — proves `soul-audio-core` allocates nothing at runtime.
//! - `layering` — proves the dependency graph respects the intended layering.
//!
//! To also assert "this must not compile" invariants, add a `trybuild`
//! dev-dependency and a `tests/compile_fail.rs`. Note that trybuild's `.stderr`
//! snapshots are compiler-version specific, so pin the toolchain in
//! `rust-toolchain.toml` before relying on them in CI.
