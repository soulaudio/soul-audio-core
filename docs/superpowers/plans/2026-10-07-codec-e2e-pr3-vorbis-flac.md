# Codec E2E Completion — PR 3: Vorbis Tier-1 + native-FLAC golden Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Close the remaining *correctness/e2e* audit gaps: wire Vorbis into the
Tier-1 app e2e duration/decode matrix, and add a committed core-suite golden
locking native-FLAC decode bit-exact to `tone.wav`. (The #34 Vorbis fast-Huffman
*perf* optimization is a separate PR — isolated because it is bit-exactness
critical.)

**Architecture:** Both are test-only additions over existing fixtures
(`tone-vorbis.ogg`, `tone.flac`, `tone.wav` already committed). The native-FLAC
golden decodes `tone.flac` and `tone.wav` through the same `open()` path and
asserts identical interleaved PCM — bit-exact lossless, source-of-truth WAV, no
ffmpeg hash and no new committed binary. It moves the "FLAC == WAV" guarantee
(today only in the CLI e2e) into the core decode suite gated by `xtask verify`.

**Tech Stack:** Rust (edition 2024), `cargo xtask verify`, app workspace tests.

**Branch:** `feat/codec-e2e-vorbis`, stacked on `feat/codec-e2e-aac` (PR #46).

## Global Constraints

- No AI trailers; GPG-signed commits (lefthook). Never `--no-gpg-sign`.
- `soul-audio-core` stays `no_std`/no-alloc; `cargo xtask verify` green.
- `app/src-tauri` is a separate workspace — run its tests inside it.

---

## Task 1: Native-FLAC core golden (bit-exact vs WAV)

**Files:**
- Modify: `crates/soul-audio-core/tests/decode.rs` (add one `#[test]`).

**Interfaces:**
- Consumes: `open`, `SliceSource`, the `decode_all`/`fnv1a` helpers already in
  `decode.rs`; fixtures `testdata/tone.flac` + `testdata/tone.wav`.

- [ ] **Step 1: Write the golden**

Add to `decode.rs` (near the FLAC-in-Ogg test):

```rust
/// Native `.flac` decodes **bit-exact** to the WAV it was encoded from — FLAC is
/// lossless, and `tone.wav` is the ground-truth source both fixtures share. This
/// is a stronger check than an ffmpeg hash (equality against the actual source),
/// and it lives in the core suite (gated by `cargo xtask verify`), complementing
/// the CLI e2e's own FLAC==WAV render check.
#[test]
fn open_decodes_native_flac_bit_exact_vs_wav() {
    let flac = decode_all(include_bytes!("../../../testdata/tone.flac"));
    let wav = decode_all(include_bytes!("../../../testdata/tone.wav"));
    assert_eq!(flac.len(), wav.len(), "FLAC/WAV sample counts differ");
    assert_eq!(fnv1a(&flac), fnv1a(&wav), "native FLAC decode != source WAV");
}
```

> **Implementer note:** confirm the `include_bytes!` path to the repo-root
> `testdata/` from `crates/soul-audio-core/tests/` (it is `../../../testdata/`).
> If other tests in this file embed root-`testdata/` fixtures with a different
> relative prefix, match theirs. (The codec *vectors* live in
> `crates/soul-audio-core/testdata/` via `../testdata/`; the *playback* tones
> live in the repo-root `testdata/`.)

- [ ] **Step 2: Run it**

Run: `cargo test -p soul-audio-core --test decode open_decodes_native_flac`
Expected: PASS (bit-exact).

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-core/tests/decode.rs
git commit -m "test(core): native FLAC decode bit-exact vs source WAV golden"
```

---

## Task 2: Vorbis → Tier-1 app e2e

**Files:**
- Modify: `app/src-tauri/tests/e2e.rs` (`per_format_durations`: add
  `tone-vorbis.ogg`; pcm 6→7).
- Modify: `app/src-tauri/tests/fixtures.rs` (`playable_fixtures_decode_nonsilent`
  += `tone-vorbis.ogg`).

- [ ] **Step 1: Add Vorbis to the duration matrix**

In `per_format_durations_are_reported`, add `"tone-vorbis.ogg"` to `library_of`,
and update the PCM count + comment: Vorbis is a ~2.0 s lossy stream whose
duration the tags layer reads from the Ogg granule → PCM bucket grows 6→7.

```rust
    assert_eq!(pcm, 7, "seven PCM fixtures ~2.0s (incl. ADTS + Vorbis): {secs:?}");
```

(Keep `dsd == 3`, `untagged == 0`.)

- [ ] **Step 2: Add Vorbis to decode-nonsilent**

In `playable_fixtures_decode_nonsilent`, add `"tone-vorbis.ogg"` to the array.

- [ ] **Step 3: Run the app Tier-1 tests**

Run (inside `app/src-tauri`): `SOUL_PLAYER_OFFLINE=1 cargo test --test e2e --test fixtures`
Expected: green — Vorbis reports ~2.0 s and decodes non-silent.

- [ ] **Step 4: Commit**

```bash
git add app/src-tauri/tests/e2e.rs app/src-tauri/tests/fixtures.rs
git commit -m "test(app): wire Vorbis into Tier-1 duration + decode e2e"
```

---

## Task 3: Docs + full verify + review + stacked PR

- [ ] **Step 1: Update `testdata/README.md`**

Add `tone-vorbis.ogg` to the fixture table (Vorbis I in Ogg) if not present, and
note in the "Automated checks" section that native FLAC is now bit-exact-vs-WAV
in the core suite too, and that Vorbis is covered in Tier-1.

- [ ] **Step 2: Root verify**

Run: `cargo xtask verify`
Expected: `verify: OK`.

- [ ] **Step 3: App verify**

Run (inside `app/src-tauri`): `SOUL_PLAYER_OFFLINE=1 cargo test`
Expected: green.

- [ ] **Step 4: Review, push, open the stacked PR**

Review the diff; then:
```bash
git push -u origin feat/codec-e2e-vorbis
tea pr create --base feat/codec-e2e-aac --head feat/codec-e2e-vorbis --title "Vorbis Tier-1 e2e + native-FLAC golden" --description "…"
```
PR body: Vorbis into Tier-1, native-FLAC core golden (bit-exact vs WAV), docs.
Note the #34 Vorbis fast-Huffman perf lands in the following PR.

---

## Self-Review

**Spec coverage:** Vorbis→Tier-1 (Task 2); native-FLAC golden (Task 1 — realized
as bit-exact-vs-WAV, which supersedes the spec's ffmpeg-ref idea since WAV is the
lossless source of truth and needs no new binary). MP1/MP2 playback: deferred per
the brainstorming default. #34 perf: separate PR. Issue closing: #25/#27/#28 via
PR #45/#46 squash-merges; #34 via PR 4.

**Placeholder scan:** the two "Implementer note" items are verify-against-code
steps, not missing content. PR body is author-time.
