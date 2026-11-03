# Codec Seeking Completion — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans. Steps use checkbox (`- [ ]`). TDD per task; commit per task on branch `feat/codec-seeking`. One PR at the end (atomic with dispatch consumers). Build from the **main checkout** (nested-worktree `.cargo` FLASH pitfall).

**Goal:** `SeekableDecoder` for ALAC, AAC (MP4+ADTS), and FLAC-in-Ogg so every `AnyDecoder` variant with a seekable source seeks.

**Architecture:** MP4 codecs seek via streamed sample-table re-read (no-alloc) + per-codec reset/pre-roll/trim; ADTS via decode-discard from start; FLAC-in-Ogg via `OggPackets` granule bisection. Design spec: `docs/superpowers/specs/2026-10-08-codec-seeking-completion-design.md`.

**Tech Stack:** Rust `no_std`/no-alloc/`forbid(unsafe_code)`, `embedded_io` Seek.

## Global Constraints

- No-alloc: no `Vec` for sample tables — record box offsets, re-read streamingly.
- Seeking gated on `S: SeekSource`. Out-of-range clamps (never errors).
- Each task ends green on the tests it adds; `cargo xtask verify` green before the PR.

---

## Task 1: MP4 sample-table offset recording + streaming `sample_offset`

**Files:** `src/demux/mp4.rs`.

**Interfaces — Produces:**
- `Mp4Reader` gains: `pos: u64` (running absolute read offset), and a `SampleTableLoc { stts, stsc, stsz, stco }: Option<...>` of `(offset: u64, size: u64)` per box + `co64: bool` + `stsz_const: Option<u32>`.
- `pub fn frame_file_offset(&mut self, frame_index: u32) -> Result<u64, DecodeError>` (requires `S: SeekSource`) — absolute file offset of coded-sample `frame_index` (0-based). Re-reads stsc/stsz/stco streamingly.
- `pub fn samples_per_frame(&self) -> u32` (ALAC frame_length / AAC 1024, from config).

- [ ] **Step 1 (test):** add `mp4_frame_offset_matches_sequential_read` in `#[cfg(test)]` — open `alac.m4a`, walk frames sequentially recording each frame's start offset by summing stsz sizes from the first chunk offset; assert `frame_file_offset(k)` equals the sequential value for k = 0, 1, mid, last. Run → fails to compile (`frame_file_offset` missing).
- [ ] **Step 2:** track `pos` — increment in every `src.read`/skip in `new`/`parse_moov`/box walk. Record the four table-box `(offset,size)` as the `stbl` walk encounters `stts`/`stsc`/`stsz`(`stz2`)/`stco`(`co64`), instead of skipping them (`:260-264`).
- [ ] **Step 3:** implement `frame_file_offset` (SeekSource): seek to `stsc`, find the chunk + first-sample-of-chunk for `frame_index`; seek to `stco`/`co64`, read that chunk's absolute offset; seek to `stsz`, sum sizes of samples before `frame_index` within the chunk (or `stsz_const * n`). Restore the source to where decode expects (re-seek to the mdat read cursor) — or document that the caller re-seeks. Return the offset.
- [ ] **Step 4:** run the unit test → pass. **Commit.**

---

## Task 2: ALAC seeking (sample-accurate)

**Files:** `src/formats/alac.rs`.

**Interfaces — Consumes** Task 1. **Produces:** `pub fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError>` + `seek_to_time`, and `impl<S: SeekSource> SeekableDecoder for AlacDecoder<S>`.

- [ ] **Step 1 (test):** inline `#[cfg(test)]` `alac_seek_round_trip_bit_exact` — decode all of `alac.m4a` to a ref vec; for several targets, `seek_to(t)` then decode the rest, assert **bit-exact** equality with `ref[t*ch..]`. Plus `alac_seek_to_zero_matches_fresh`, backward/repeated, `alac_seek_beyond_end_clamps`. Run → fails (no `seek_to`).
- [ ] **Step 2:** implement: `frame_index = sample / frame_length`; `off = reader.frame_file_offset(frame_index)`; `reader.src_mut().seek(SeekFrom::Start(off))`; reset `filled/used/out_len/out_pos`, `samples_left = total_samples - frame_index*frame_length`, `mdat_remaining` appropriately; decode frames forward, discarding `sample - frame_index*frame_length` samples; return `sample` (clamped). `seek_to_time = seek_to(secs_to_samples(secs, sample_rate))`.
- [ ] **Step 3:** run inline tests → pass. **Commit.**

---

## Task 3: AAC/MP4 seeking (sample-accurate, pre-roll)

**Files:** `src/formats/aac/mod.rs`; maybe a `reset()` on `Filterbank` (`filterbank.rs`) + reuse `SbrState::reset_channels`.

**Interfaces — Consumes** Task 1. **Produces:** `AacDecoder::seek_to`/`seek_to_time` (MP4 arm), `impl<S: SeekSource> SeekableDecoder`.

- [ ] **Step 1 (test):** inline `aac_mp4_seek_round_trip_segsnr` using `aac.m4a` — decode-all ref; for targets, seek + decode rest, assert `seg_snr_at` ≥ bar (reuse the decode.rs metric; import or duplicate minimal helper) vs `ref[t..]`. Add `sbr_m4a_seek_*` with `sbr.m4a` (bar per existing HE-AAC golden). seek-to-0===fresh; beyond-end clamp. Run → fails.
- [ ] **Step 2:** add `Filterbank::reset(&mut self)` (zero `overlap`, `prev_window_shape`). In `seek_to` (MP4): `frame = sample/1024`; `preroll = PREROLL_LC` (start 1) or `PREROLL_SBR` (start 4) when `sbr_confirmed`; `land = frame.saturating_sub(preroll)`; `off = reader.frame_file_offset(land)`; seek; reset `Filterbank` + `SbrState::reset_channels` + clear header/active; reset `pcm_len/pcm_pos`, `mp4_filled/used`; decode-discard from `land` to `sample` (accounting for SBR output doubling via `output_rate`), trim. Tune `PREROLL_*` until the SNR bar passes.
- [ ] **Step 3:** run → pass. **Commit.**

---

## Task 4: AAC/ADTS seeking (decode-discard from start)

**Files:** `src/formats/aac/mod.rs` (ADTS arm of `seek_to`), `src/demux/adts.rs` (add `rewind` via `SeekSource`).

- [ ] **Step 1 (test):** inline `aac_adts_seek_round_trip_segsnr` (`aac.aac`) + `sbr_aac_seek_*` (`sbr.aac`) + seek-to-0===fresh + backward. Run → fails.
- [ ] **Step 2:** `AdtsReader::rewind(&mut self)` (SeekSource → seek start, re-read first header). In `seek_to` (ADTS): rewind, fresh `Filterbank`/`SbrState`, reset carries, then `fill_next_frame` + discard output samples until `sample`, trim. Clamp at EOF.
- [ ] **Step 3:** run → pass. **Commit.**

---

## Task 5: FLAC-in-Ogg seeking (sample-accurate, granule bisection)

**Files:** `src/demux/ogg.rs` (granule-aware + `SeekSource` seek API) OR route FLAC-in-Ogg through `OggPackets`; `src/formats/flac.rs` (Ogg seek path); `src/dispatch.rs` (`OggFlac` variant).

**Decision:** add to `OggReader` a `S: SeekSource` `seek_to_granule(target) -> landed_floor` that bisects pages (port/reuse `OggPackets` bisection), repositioning so subsequent reads deliver the landed page's audio; track the current granule. Then a FLAC-over-Ogg seek: `reader.seek_to_granule(sample)`, reset FLAC frame/bit state (`decoded_per_ch = floor`), decode-discard FLAC frames to `sample`.

- [ ] **Step 1 (test):** inline/`tests/decode.rs` `ogg_flac_seek_round_trip_bit_exact` (`flac-in-ogg.ogg`, mono) — bit-exact round-trip vs from-start; seek-to-0===fresh; beyond-end clamp. Run → fails.
- [ ] **Step 2:** implement the granule-aware seekable `OggReader` + the FLAC-in-Ogg seek path.
- [ ] **Step 3:** run → pass. **Commit.**

---

## Task 6: Dispatch wiring, stale-test update, docs

**Files:** `src/dispatch.rs`, `tests/decode.rs`, `README.md`, `src/formats/mod.rs`.

- [ ] **Step 1:** replace `AnyDecoder::seek_to`/`seek_to_time` `Unsupported` arms for `Alac`/`Aac`/`OggFlac` with real forwards (`:88-89`, `:104-105`). AAC/ALAC time uses the right rate.
- [ ] **Step 2:** rewrite `tests/decode.rs::ogg_flac_reports_duration_but_rejects_seek` → `ogg_flac_seeks` asserting a round-trip (drop the `Unsupported` assertions). Add `AnyDecoder`-level seek smoke tests for alac/aac/ogg-flac via `open`.
- [ ] **Step 3:** update README seeking matrix (AAC/ALAC/Ogg-FLAC now seek; note granularity) + `formats/mod.rs` if it mentions seeking.
- [ ] **Step 4:** `cargo xtask verify` → green. **Commit.**

---

## Self-Review

- **Spec coverage:** ALAC (T2), AAC/MP4 (T3), AAC/ADTS (T4), FLAC-in-Ogg (T5), no-alloc MP4 tables (T1), dispatch+docs (T6). All mapped.
- **No-alloc:** T1 records box offsets + re-reads; no `Vec`. ADTS/Ogg use streaming + existing bounded buffers.
- **Types:** `frame_file_offset(u32)->u64`, `seek_to(u64)->u64`, all `S: SeekSource`. Consistent with `SeekableDecoder`.
- **Risk:** T5 (Ogg) is the biggest; if it balloons, land T1–T4 + T6-partial first (ALAC/AAC seeking, still a major win) and track Ogg-FLAC as a follow-up rather than block the PR.
