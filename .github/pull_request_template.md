## Summary

<!-- What does this change, and why? Link any related issue. -->

## Checklist

- [ ] PR title is a [Conventional Commit](https://www.conventionalcommits.org) (e.g. `feat(core): …`) — it becomes the squash-merge commit subject
- [ ] `cargo xtask verify` passes locally (fmt, `clippy -D warnings`, tests, `no_std` cross-builds)
- [ ] Tests added/updated (bit-exact goldens for codec changes)
- [ ] No `unsafe` and no heap allocation added to `soul-audio-core`
- [ ] No tool-attribution trailers in commits
