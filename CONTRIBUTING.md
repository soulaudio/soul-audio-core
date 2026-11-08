# Contributing to soul-audio-core

Thanks for your interest! This is a `no_std`, no-alloc, `#![forbid(unsafe_code)]`
audio library — contributions are welcome as long as they keep those invariants.

## Getting started

```sh
# Run the full gate locally (what CI runs):
cargo xtask verify          # fmt, clippy -D warnings, tests, no_std cross-builds

# Or the individual pieces:
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all
```

The pinned toolchain is in `rust-toolchain.toml`; `rustup` installs it (and the
`thumbv7em-none-eabihf` target) automatically.

## Ground rules

- **No `unsafe`** in `soul-audio-core` — it is `#![forbid(unsafe_code)]`. The
  library must also stay **`no_std` and allocation-free**; `cargo xtask verify`
  cross-builds for `thumbv7em-none-eabihf` to enforce this.
- **Decoders are bit-exact** where the format allows. New codec work must come
  with fixtures and goldens cross-checked against a reference (ffmpeg, libopus,
  the relevant RFC/spec vectors).
- **Format the code** (`cargo fmt`) and keep `clippy -D warnings` clean.

## Commit & PR conventions

- Commit messages and **PR titles** must follow
  [Conventional Commits](https://www.conventionalcommits.org): `feat(core): …`,
  `fix(tags): …`, `test(core): …`, etc. CI enforces this on PR titles (the repo
  squash-merges, so the PR title becomes the commit subject).
- Keep history free of tool-attribution trailers — no `Co-Authored-By` for AI
  tools, no "Generated with …" lines. A local `commit-msg` hook (via
  [lefthook](https://github.com/evilmartians/lefthook)) enforces both rules; run
  `lefthook install` once after cloning.
- One logical change per PR. Include tests. Make sure `cargo xtask verify`
  passes before opening it.

## Reporting bugs / proposing features

Use the issue templates. For **security** issues, do **not** open a public
issue — see [SECURITY.md](SECURITY.md).

## Licensing

By contributing, you agree that your contributions are dual-licensed under
[MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE), the same as the project, with
no additional terms.
