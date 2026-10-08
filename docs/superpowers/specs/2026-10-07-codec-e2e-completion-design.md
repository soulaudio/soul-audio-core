# Codec E2E Completion — Design Spec

**Date:** 2026-10-07
**Goal:** Every audio codec in `soul-audio-core` is *properly working* (decodes,
reports correct duration, seeks) and *tested end-to-end* with a committed
fixture + golden. Performance work for the two codecs with a real-time concern
(Vorbis residue, DSD at the top rates) is in scope.

This is an audit-driven completion of the codec surface. It closes the four
tracked codec issues (#25, #27, #28, #34) **and** the e2e-coverage gaps the audit
surfaced (Vorbis/SBR/DST/DFF absent from the playback tiers), which were never
ticketed.

## Scope decisions (settled)

- **Breadth:** comprehensive — close the 4 tickets *and* fill every e2e gap the
  audit found.
- **Perf:** in scope (Vorbis fast-Huffman #34; DSD512 real-time headroom #28).
- **AAC PS (HE-AACv2):** get a *committed* e2e golden. The existing PS fixture is
  gitignored because it is the license-restricted **Coding Technologies aacPlus
  DecoderCheck** material — the restriction is on *that specific material*, not
  on PS bitstreams in general. A **synthetic** PS tone we encode ourselves has no
  licensing issue. **Prerequisite:** a PS-capable encoder (fdk_aac-enabled ffmpeg
  or the `fdkaac` CLI); the bundled MediaFoundation `aac_mf` emits HE-AAC (SBR)
  only, never HE-AACv2 (PS). The PS fixture-generation step is BLOCKED until that
  encoder is present; everything else proceeds without it.
- **Four defaults confirmed:** (1) add a native-FLAC committed `*.ref.pcm`
  golden; (2) *defer* MP1/MP2 playback-tier fixtures (they keep their existing
  bit-exact decode goldens and only flow the generic path); (3) the #34 dense
  stimulus is **synthetic** (real music can't be committed); (4) the DST
  playback-tier fixture **reuses** the committed FATE slice (no DST *encoder*
  exists in any available toolchain).

## Current state (audit summary)

Every decoder is already correctness-verified by committed external-oracle
goldens (ffmpeg / libopus / minimp3 / kjmp2). The gaps are:

**Functional (codec not fully working):**
- **#25 DFF** — `DffDecoder` computes `audio_total` but never exposes
  `total_samples()`, so `total_frames()` is `None` → no duration/seek bar. No DFF
  tag reader in `soul-audio-tags`. No `.dff` fixture exists at all.
- **#27 bare `.aac` (ADTS)** — reports duration `0.0` (no frame-scan in tags);
  Tier-1 currently asserts it *as* the untagged/0.0 case.
- **#28 DSD256/512** — decode is rate-agnostic but only DSD64/128 have real-file
  e2e; 256/512 are unit-mapping-only. Top-rate real-time headroom is unmeasured.

**E2E coverage (works + has decode goldens, but absent from any playback tier):**
- **Vorbis** — strong decode goldens + CLI render, but not in Tier-1 app e2e.
- **AAC SBR** — decode golden only; no playback fixture; not in CLI e2e / Tier-1
  / qa-audio.
- **AAC PS** — only unit-DSP + fuzz committed; its end-to-end golden is the
  license-restricted local-only fixture.
- **DST (in DFF)** — bit-exact FATE-slice golden exists, but no playback-tier
  fixture.
- **Native FLAC** — no committed `*.ref.pcm` vs ffmpeg (relies on "FLAC==WAV" in
  CLI e2e).

**Perf:**
- **#34 Vorbis** — `Codebook::decode_scalar` is a linear per-bit scan; residue
  hot loop. ~0.40× RT on a light synthetic tone; a dense track is the worry.
- **#28 DSD512** — may not sustain real-time without the release profile.

## Two e2e layers (important for "tested e2e")

- **Root-workspace CLI e2e** — `crates/soul-audio-cli/tests/e2e.rs`. Runs inside
  `cargo xtask verify`. Bit-exact for lossless, pitch-only for lossy, signal
  analysis on rendered output.
- **Desktop app e2e (Tier 1)** — `app/src-tauri/tests/`. A **separate Cargo
  workspace** that `cargo xtask verify` does **not** reach; gated by the CI `app`
  job. Holds `per_format_durations` + transport/seek/volume/queue/art.

Each codec addition below states which layer(s) it lands in. A codec is
"tested e2e" when it has (a) a committed decode golden and (b) at least one
playback-tier test (CLI render and/or Tier-1).

## Architecture / components touched

- `crates/soul-audio-core/src/formats/dff.rs` — expose `total_samples()`.
- `crates/soul-audio-tags/` — DFF (DSDIFF) tag reader; ADTS frame-scan duration.
- `testdata/generate_dsd.py` — add a DSDIFF (`FRM8`/`PROP`/`DSD`) writer; emit
  DSD256/512 via the existing rate arg.
- `testdata/generate.sh` — new fixture encode lines (DFF, DSD256/512, SBR, PS,
  native-FLAC ref, dense Vorbis).
- `testdata/` + `crates/soul-audio-core/testdata/` — committed fixtures + refs.
- `crates/soul-audio-core/src/formats/vorbis/` — FAST_HUFFMAN acceleration table,
  profile-const sized.
- `crates/soul-audio-core/tests/decode.rs` — new/extended decode goldens (native
  FLAC, DSD256/512 decimation, SBR/PS/DST/DFF render-tier goldens where they live
  in core).
- `crates/soul-audio-cli/tests/e2e.rs` — SBR / PS / DST / DFF render cases.
- `app/src-tauri/tests/{e2e.rs,fixtures.rs}` — Vorbis + DFF into Tier-1; ADTS
  duration assertion flip; stale Opus comment fix.
- `xtask/src/main.rs` — DSD per-rate real-time measurement; qa-audio additions.
- `testdata/README.md`, `docs/superpowers/e2e-suite-followups.md` — document new
  fixtures and the DSD512 ceiling.

## Fixtures (committed test material)

| Fixture | Produced by | Oracle / ref | Lands in |
|---|---|---|---|
| `tone.dff` (DSD64, 0.5 s) | new DSDIFF writer in `generate_dsd.py` | round-trip + decimation (DSD has no bit-exact PCM golden) | CLI e2e, Tier-1, qa |
| `tone-dsd256.dsf`, `tone-dsd512.dsf` | `generate_dsd.py` rate arg | decimation-rate check | core decode goldens |
| `tone-sbr.m4a` (+ `tone-sbr.aac`) | `ffmpeg aac_mf -b:a 32k` (HE-AAC) | ffmpeg decode, pitch-only | CLI e2e, Tier-1, qa |
| `tone-ps.m4a` + `tone-ps.ref.pcm` | **PREREQ** fdk_aac encoder → ffmpeg decode | ffmpeg PS decode, per-channel SNR | core golden, CLI e2e, Tier-1, qa |
| DST playback | **reuse** `dst-64fs44-2ch.slice.dff` | existing FNV golden vs ffmpeg | CLI e2e, qa |
| `tone-flac.ref.pcm` | ffmpeg decode of `tone.flac` | bit-exact | core decode golden |
| dense Vorbis stimulus | synthetic broadband/high-q `libvorbis` | decode-vs-RT margin + unchanged SNR | #34 bench/CLI |

Fixtures are binary and committed (mirrors the e2e-suite plan). Generation
requires `ffmpeg` (present: 8.0.1 with `aac`/`flac`/`libopus`/`aac_mf`), `python3
+ numpy` (present), and — for PS only — a PS-capable encoder (to be installed).

## Functional fixes

1. **#25a DFF duration.** Add `DffDecoder::total_samples() -> Option<u64>`
   returning the per-channel sample count (`audio_total / channels * 8`), covering
   both the raw-`DSD ` and DST paths. It surfaces through
   `DecimateAdapter::total_frames()` exactly as DSF's does. Pin with a fixture
   that reports the correct 0.5 s duration end-to-end.
2. **#25b DFF tag reader.** In `soul-audio-tags`, read DSDIFF metadata: the `DIIN`
   (edited-info: title/artist) chunk, and an embedded `ID3 ` chunk if present
   (reuse the existing ID3v2 reader). No-alloc, thumbv7em-clean, matching the
   crate's conventions.
3. **#27 ADTS duration.** In `soul-audio-tags`, estimate bare-ADTS duration by
   scanning frame headers: `frames × 1024 / sample_rate` (1024 samples/AAC frame),
   handling VBR by summing actual frame sizes. Flip the Tier-1
   `per_format_durations` assertion for `tone.aac` from "0.0/untagged" to ~2.0 s
   (±0.15 s lossy tolerance).

## E2E wiring (per codec)

- **Vorbis** → add `tone-vorbis.ogg` to Tier-1 `per_format_durations` + a
  `fixtures.rs` decode-nonsilent case. (CLI render golden already exists.)
- **SBR** → CLI render (pitch-only, like AAC-LC lossy: FFT-assert L≈440/R≈659 past
  priming) + Tier-1 decode-nonsilent + qa-audio play.
- **PS** → committed per-channel segmental-SNR golden (synthetic fixture) + the
  1→2 stereo-promotion assertion (channels promote once the first PS frame
  applies) + CLI render + Tier-1 + qa.
- **DFF** → CLI render + Tier-1 duration + qa (uses `tone.dff`).
- **DST** → CLI render + qa, from the committed slice.
- **DSD256/512** → core decode goldens: decode end-to-end and assert the 8:1
  decimated output rate (256 → 1.4112 MHz, 512 → 2.8224 MHz), extending
  `dsd128_decimates_to_double_rate`.
- **Native FLAC** → committed `tone-flac.ref.pcm` + a bit-exact decode golden in
  `decode.rs`.

## Performance

- **#34 Vorbis fast-Huffman.** Add a `FAST_HUFFMAN_LENGTH` acceleration table
  (e.g. 2¹⁰ entries per codebook) consulted first in `decode_scalar`, built at
  setup, sized by a profile const (smaller/off on the embedded profile to save
  RAM). SNR goldens must stay **unchanged**. Prove decode-vs-real-time stays
  comfortably < 1× on the committed dense synthetic fixture (vs the ~0.40× tone
  baseline, which is misleadingly light). Re-run under
  `--features vorbis-embedded-profile` to confirm the embedded sizing path.
- **#28 DSD512 real-time headroom.** Measure decode-vs-real-time per DSD rate
  (64/128/256/512) — extend `xtask qa-audio` or add a small bench — and document
  the practical ceiling (release vs dev profile) in `testdata/README.md`.

## Error handling

- DFF `total_samples()` returns `None` (not a panic) for a malformed/empty sound
  chunk — `total_frames()` stays `None`, consistent with the current no-duration
  behavior.
- ADTS frame-scan fails closed: an unparseable stream degrades to `None`/`0.0`
  (today's behavior), never a panic; bounded scan (no unbounded read).
- Tag readers follow the crate's fail-closed convention: a parse error discards
  metadata rather than aborting the scan.
- New fixtures decode on the CLI/app big-stack thread (the `AnyDecoder` enum is
  ~150 KiB; the desktop loader already runs decode on a 16 MiB thread).

## Testing strategy

- **TDD per task:** add the fixture + a failing golden/e2e assertion first, then
  implement, then green — matching the repo's subagent-driven SDD style.
- **Gate placement:** CLI-e2e and core-decode additions are gated by
  `cargo xtask verify`; Tier-1 app-e2e additions are gated by the CI `app` job.
  Each task states which.
- **Generated-table drift:** if the Vorbis accel table is checked-in-generated,
  add an `xtask check-*` gate; otherwise it is built at setup from existing tables
  and needs no generator.
- **no_std/no-alloc preserved:** every `soul-audio-core` and `soul-audio-tags`
  change keeps `#![forbid(unsafe_code)]`, no new deps, and the thumbv7em
  cross-build green (verify step 6).
- **Full gate per PR:** `cargo xtask verify` green; the CI `app` job green for any
  Tier-1 change.

## Sequencing (one PR per codec family)

- **PR 1 — DSD/DSDIFF:** #25a DFF duration, #25b DFF tag reader, `tone.dff`
  fixture, DST playback fixture + e2e, DFF e2e, DSD256/512 fixtures + decode
  goldens, #28 DSD512 real-time headroom + documented ceiling. *No external
  blockers.*
- **PR 2 — AAC family e2e:** #27 ADTS duration + assertion flip, SBR playback
  fixture + e2e, PS synthetic fixture + e2e. *PS sub-task gated on the encoder
  install; SBR + ADTS proceed regardless.*
- **PR 3 — Vorbis + remaining:** Vorbis → Tier-1, #34 fast-Huffman + dense
  fixture, native-FLAC committed golden, stale `fixtures.rs` Opus comment fix,
  `testdata/README.md` + followups doc, close issues #25/#27/#28/#34.

## Prerequisites & risks

- **PS encoder (hard prereq for the PS sub-task only).** Needs fdk_aac-enabled
  ffmpeg or `fdkaac`. Until present, PR 2's PS fixture generation is BLOCKED;
  SBR + ADTS land without it, and PS lands as a follow-up commit on PR 2.
- **Two-workspace split.** Tier-1 additions aren't covered by `xtask verify`;
  they must be run in `app/src-tauri` (CI `app` job). Don't claim e2e-green from a
  root `verify` alone for Tier-1 work.
- **Commit hygiene.** lefthook rejects AI trailers (`Co-Authored-By: Claude`,
  `Claude-Session`); commits are GPG-signed by default — never pass
  `--no-gpg-sign`. Pre-commit runs workspace-wide clippy, so an API change must
  commit atomically with its consumers.
- **DSD512 may not sustain real-time in dev** — that is an expected *finding* to
  document (the acceptance is "a documented note on DSD512 viability"), not a
  failure to fix.

## Acceptance (per codec — "existing" = already green; **bold** = new in this effort)

| Codec | Decode golden (committed) | Duration/seek | Playback-tier e2e |
|---|---|---|---|
| WAV, MP3, ALAC, Opus | existing | existing | existing |
| FLAC | **+ native `*.ref.pcm`** | existing | existing |
| Vorbis | existing | existing | **+ Tier-1** |
| AAC-LC | existing | **+ ADTS duration (#27)** | existing |
| AAC SBR | existing | n/a (container) | **+ CLI + Tier-1 + qa** |
| AAC PS | **+ synthetic committed** | n/a | **+ CLI + Tier-1 + qa** |
| DSD DSF | existing | existing | existing |
| DSD256/512 | **+ decimation goldens** | n/a | **+ core e2e** |
| DSD DFF | existing | **+ total_samples (#25a)** | **+ CLI + Tier-1 + qa** |
| DFF tags | n/a | n/a | **+ tag reader (#25b)** |
| DST | existing (FATE slice) | n/a | **+ CLI + qa** |

Perf acceptance: #34 decode-vs-RT < 1× on the dense fixture, SNR goldens
unchanged, embedded profile builds; #28 per-rate RT measured and the DSD512
ceiling documented.
