# Vorbis I Decoder — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A from-scratch, `no_std`, no-alloc Vorbis I decoder in `soul-audio-core` that decodes and plays (with seek) `.ogg`/Vorbis files on desktop and the embedded DAP. Implements the Vorbis half of Forgejo issue **#24**.

**Design spec:** `docs/superpowers/specs/2026-09-05-vorbis-decoder-design.md` (read it — it carries the rationale).

**Reference (implement FROM these, not by transcription):** Xiph Vorbis I spec `https://xiph.org/vorbis/doc/Vorbis_I_spec.html`; stb_vorbis (public-domain, fixed-footprint + IMDCT reference); lewton (MIT, cleanest Rust structure); Tremor lowmem (BSD, embedded).

## How to read this plan (important)

This is a codec. Tasks fall in two classes:
- **Integration/scaffolding tasks** (Ogg demuxer, bit reader, arena, wiring, xtask tables, tests): the plan gives **complete code** — transcribe + test.
- **DSP/codec tasks** (codebooks, floor 1, residue, IMDCT, coupling, seek): the plan gives the **exact interface (signatures), the Vorbis-spec section to implement, the reference impl to consult, the known bug-sources, and the golden/unit test that defines done.** The implementer writes the intricate code against spec + reference to pass the test. Fabricated "complete" DSP code would be wrong and misleading, so it is deliberately not provided; the **test is the contract.** Every DSP task MUST land its milestone test green.

## Global Constraints

- **No AI trailers in commits** (lefthook rejects `Co-Authored-By: Claude`/`Claude-Session`); GPG-signed.
- **`soul-audio-core` stays `no_std` + no-alloc + `#![forbid(unsafe_code)]`.** No new crate dependency (only `embedded-io`), no `alloc`, no `unsafe`. The default build and `cargo xtask verify` (incl. the `thumbv7em-none-eabihf` cross-build) must stay green **at the end of every task**.
- **No runtime transcendentals / `libm`.** Trig/window/IMDCT-twiddle tables are generated host-side by an `xtask` command and checked in (pattern: `resample_tables`/`filterbank_tables`/`sbr_tables`).
- **Vorbis is lossy** → goldens use SNR/spectral tolerance vs an ffmpeg reference (the AAC golden pattern), never bit-exact.
- **Fixture name:** `tone-vorbis.ogg` (`tone.ogg` is already FLAC-in-Ogg).
- **Arena:** the codec allocates nothing itself; it bump-allocates within a caller-provided `A: AsMut<[u8]> + AsRef<[u8]>` and stores **offsets, never references** (no self-reference, no `unsafe`). Over-cap → `DecodeError::Unsupported`, never panic.
- **Big-stack tests:** `VorbisDecoder` owned-by-value in `OwnedDecoder` is large → integration tests decode on the existing 16 MiB stack-thread helper.

## File Structure

- Create `crates/soul-audio-core/src/demux/ogg_packets.rs` — packet-preserving Ogg demuxer (page/lacing/reassembly/granule; `Source` + `SeekSource`).
- Create `crates/soul-audio-core/src/formats/vorbis/mod.rs` — `VorbisDecoder<S, A>`, `Decoder`/`SeekableDecoder` impls, the per-packet pipeline.
- Create `.../vorbis/bitread.rs` — LSb-first bit reader + `ilog`/`float32_unpack`.
- Create `.../vorbis/arena.rs` — bump allocator over `A`, offset handles.
- Create `.../vorbis/setup.rs` — codebooks + floors + residues + mappings + modes parse (into arena).
- Create `.../vorbis/codebook.rs` — Huffman build + VQ (compact type-1).
- Create `.../vorbis/floor.rs` — floor 1 decode + curve synthesis.
- Create `.../vorbis/residue.rs` — residue types 0/1/2 + inverse coupling.
- Create `.../vorbis/imdct.rs` — self-contained IMDCT + window + overlap-add.
- Create `.../vorbis/tables.rs` — `include!`d generated window/twiddle tables.
- Modify `crates/soul-audio-core/src/model.rs` — add `Format::Vorbis`.
- Modify `crates/soul-audio-core/src/probe.rs` — detect `\x01vorbis`.
- Modify `crates/soul-audio-core/src/formats/mod.rs` and `src/demux/mod.rs` — `pub mod` the new modules.
- Modify `crates/soul-audio-core/src/dispatch.rs` — `Format::Vorbis => Unsupported` in `open()` (Vorbis is consumer-dispatched).
- Modify `xtask/src/main.rs` + create `xtask/src/vorbis_tables.rs` — `gen-vorbis-tables` + `check`.
- Modify `crates/soul-audio-desktop/src/decoder.rs` — `OwnedDecoder::Vorbis(...)` + `open_track` arm (heap `Box<[u8]>` arena).
- Modify `crates/soul-audio-cli/src/…` — decode/render Vorbis dispatch arm.
- Create `testdata/tone-vorbis.ogg` + `crates/soul-audio-core/testdata/tone-vorbis.ref.pcm`; modify `testdata/generate.sh`.
- Modify `crates/soul-audio-core/tests/decode.rs`, `crates/soul-audio-cli/tests/e2e.rs`, `xtask/src/main.rs` qa list.

---

## Task 1: Packet-preserving Ogg demuxer in core

**Files:** Create `crates/soul-audio-core/src/demux/ogg_packets.rs`; modify `src/demux/mod.rs` (`pub mod ogg_packets;`).

**Interfaces (Produces):**
```rust
/// Streaming Ogg packet reader over a Source. Preserves packet boundaries and
/// the granule position of each page (Vorbis needs both). Separate from the
/// existing FLAC-flattening `OggReader`.
pub struct OggPackets<S> { /* current page header, lacing [u8;255], cursor, last_granule */ }

impl<S: Source> OggPackets<S> {
    pub fn new(src: S) -> Result<Self, DecodeError>;
    /// Read the next whole packet into `buf`; returns its length, or Ok(0) at end
    /// of stream. A packet may span pages (segment == 255 continues). Truncates
    /// (Unsupported) if `buf` is too small.
    pub fn next_packet(&mut self, buf: &mut [u8]) -> Result<usize, DecodeError>;
    /// Granule position of the most recently completed page (for total_frames).
    pub fn last_page_granule(&self) -> u64;
    pub fn serial(&self) -> u32;
}
impl<S: SeekSource> OggPackets<S> {
    /// Seek to the first page whose granule >= target via bisection over byte
    /// offsets; leaves the reader at that page boundary. Returns landed granule.
    pub fn seek_to_granule(&mut self, target: u64) -> Result<u64, DecodeError>;
    /// Scan backward from EOF (bounded, ~64 KiB) for the last real granule.
    pub fn end_granule(&mut self) -> Result<u64, DecodeError>;
}
```

**Reference:** port `crates/soul-audio-tags/src/ogg.rs`'s `PageWalker`/`PacketReader` (same page-header + 255-lacing + `<255`-terminates-packet logic) down into core; core may NOT depend on tags (layering). Ogg page header layout: `OggS`, version(1), header_type(1), granule(8 LE), serial(4), seq(4), crc(4), page_segments(1), then the lacing table.

- [ ] **Step 1: Failing unit test** — hand-build two Ogg pages splitting one packet across them (model on tags `ogg_page` helper), assert `next_packet` reassembles it and reports the right length; a second packet on page 2; `Ok(0)` at end; `last_page_granule` matches.
- [ ] **Step 2: Run it, see it fail** (`cargo test -p soul-audio-core ogg_packets`).
- [ ] **Step 3: Implement** `OggPackets` (page parse, lacing, cross-page reassembly, granule capture) per the reference. Add `seek_to_granule`/`end_granule` behind `S: SeekSource`.
- [ ] **Step 4: Tests green** + a truncated-page test returns `Unexpected­Eof`/`Malformed` (no panic).
- [ ] **Step 5: `cargo xtask verify`** stays green (incl. thumbv7em). Commit.

## Task 2: LSb-first bit reader + primitives

**Files:** Create `crates/soul-audio-core/src/formats/vorbis/bitread.rs`; create `src/formats/vorbis/mod.rs` (`pub mod bitread;` for now); modify `src/formats/mod.rs` (`pub mod vorbis;`).

**Interfaces (Produces):**
```rust
pub struct BitReader<'a> { bytes: &'a [u8], bit_pos: usize }
impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self;
    pub fn read_bits(&mut self, n: u32) -> Result<u32, DecodeError>; // LSb-first, n<=32
    pub fn read_bit(&mut self) -> Result<bool, DecodeError>;
    pub fn read_bits_u64(&mut self, n: u32) -> Result<u64, DecodeError>;
    pub fn bits_read(&self) -> usize;
}
/// bits needed to represent 0..=n-1 (spec §9.2.1). ilog(0)=0.
pub fn ilog(n: u32) -> u32;
/// spec §9.2.2: unpack a codebook float (mantissa/exponent), NOT IEEE-754.
pub fn float32_unpack(x: u32) -> f32;
```

Vorbis bit convention (spec §2): **LSb-first** — the first bit read is bit 0 of the value; multi-bit values are little-endian at the bit level. This differs from MP3/AAC (MSb-first) and is the #1 bug source — get it exactly right.

- [ ] **Step 1: Failing tests** from spec §2 / §9.2 worked examples: a known byte sequence yields known `read_bits` values; `ilog(0)=0, ilog(1)=1, ilog(7)=3, ilog(8)=4`; `float32_unpack` of a couple of known encodings.
- [ ] **Step 2: Run, see fail.**
- [ ] **Step 3: Implement** `BitReader` (LSb-first), `ilog`, `float32_unpack`.
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: verify green. Commit.**

## Task 3: Arena + `Format::Vorbis` + probe + decoder skeleton

**Files:** Create `src/formats/vorbis/arena.rs`; flesh `src/formats/vorbis/mod.rs` (`VorbisDecoder<S,A>` skeleton); modify `src/model.rs`, `src/probe.rs`, `src/dispatch.rs`.

**Interfaces (Produces):**
```rust
// arena.rs
pub struct Arena<'a> { buf: &'a mut [u8], top: usize }
impl<'a> Arena<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self;
    pub fn reset(&mut self);
    /// Bump `len` aligned bytes; returns a byte offset, or Unsupported if full.
    pub fn alloc(&mut self, len: usize, align: usize) -> Result<usize, DecodeError>;
    pub fn bytes(&self) -> &[u8];
    pub fn bytes_mut(&mut self) -> &mut [u8];
}

// mod.rs — skeleton; real decode filled by later tasks
pub struct VorbisDecoder<S, A: AsMut<[u8]> + AsRef<[u8]>> { /* src, arena=A, setup, dsp state */ }
impl<S: Source, A: AsMut<[u8]> + AsRef<[u8]>> VorbisDecoder<S, A> {
    /// Parse the 3 setup headers into the arena; Unsupported if setup > arena.
    pub fn new(src: S, arena: A) -> Result<Self, DecodeError>;
}
impl<S,A> Decoder for VorbisDecoder<S,A> { /* stream_info from id header; decode_into: todo -> Ok(0) until Task 7 */ }
impl<S: SeekSource, A> SeekableDecoder for VorbisDecoder<S,A> { /* seek_to: Unsupported until Task 10; total_frames from end granule */ }
```

- `Format::Vorbis` added to `model.rs` `Format` (`#[non_exhaustive]`).
- `probe.rs`: in the `b"OggS"` arm, before `return None`, detect a Vorbis id packet — the first page body starts with `\x01vorbis`. Return `Some(Format::Vorbis)`.
- `dispatch.rs` `open()`: `Format::Vorbis => Err(DecodeError::Unsupported)` (Vorbis is consumer-dispatched, not an `AnyDecoder` variant — see spec).

- [ ] **Step 1: Failing tests** — `Arena::alloc` bumps + aligns + returns `Unsupported` when full and after `reset`; `probe` returns `Format::Vorbis` for a hand-built `\x01vorbis` BOS page (and still `OggFlac` for the FLAC magic); `open()` returns `Unsupported` for Vorbis.
- [ ] **Step 2: Run, see fail.**
- [ ] **Step 3: Implement** arena, the `Format`/`probe`/`dispatch` wiring, and the `VorbisDecoder` skeleton (`new` parses only the id header for `stream_info` for now — full setup parse is Task 4–5; `decode_into` returns `Ok(0)`).
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: verify green (thumbv7em: the arena-generic decoder must compile for embedded). Commit.**

## Task 4: Setup parse — identification/comment headers + codebooks (into arena)

**DSP task — interface + spec + reference + test; implement to pass.**

**Files:** Create `src/formats/vorbis/codebook.rs`, `src/formats/vorbis/setup.rs`; extend `VorbisDecoder::new`.

**Spec:** §4.2.1–4.2.3 (common header + id + comment), §3 + §3.2.1 (codebook decode). **Reference:** stb_vorbis `start_decoder` codebook section; lewton `header.rs`. **Bug sources (verify at intermediate-value level):** canonical Huffman assignment ("lowest-valued unused codeword"), `ordered` length runs (`ilog(entries-current)`), `sparse` flag, the exactly-full-tree requirement (under/over-specified = hard error, except the single-entry special case), `lookup1_values` (`largest m: m^dim ≤ entries`), and **keeping lookup type 1 compact** (store multiplicands; compute vectors on the fly — do NOT pre-expand).

**Interface (Produces):**
```rust
// codebook.rs — stored as arena offsets, no owned Vec
pub struct Codebook { pub dims: u16, pub entries: u32, pub lookup_type: u8,
    /* offsets: codeword lengths / fast-huffman table / multiplicands */ }
impl Codebook {
    pub fn parse(br: &mut BitReader, arena: &mut Arena) -> Result<Codebook, DecodeError>;
    /// Huffman-decode one entry index from the bitstream.
    pub fn decode_scalar(&self, br: &mut BitReader, arena: &Arena) -> Result<u32, DecodeError>;
    /// VQ: fill `out` (len == dims) for `entry` (compact type-1 or explicit type-2).
    pub fn lookup_vector(&self, entry: u32, arena: &Arena, out: &mut [f32]);
}
```

- [ ] **Step 1: Failing test** — hand-build a tiny codebook (a handful of entries, ordered + sparse variants, a type-1 lookup) and assert `decode_scalar` returns the expected indices for known bits and `lookup_vector` returns the expected multiplicand-derived vector. Also: an under-specified (non-full) tree → `Malformed`.
- [ ] **Step 2: Run, see fail.**
- [ ] **Step 3: Implement** the common/id/comment header parse (id: channels@[11], rate@[12..16], blocksizes from the exponent byte; comment: skip) and codebook parse/decode into the arena. Add a `FAST_HUFFMAN_LENGTH`-style acceleration table sized by a `MAX_*`/profile const (lower on embedded).
- [ ] **Step 4: Tests green** + the header parse yields correct `stream_info`.
- [ ] **Step 5: verify green. Commit.**

## Task 5: Setup parse — floors (floor 1) + residues + mappings + modes

**DSP task.** **Files:** extend `setup.rs`, create `floor.rs`/`residue.rs` config structs.

**Spec:** §7.2.1 (floor 1 setup), §8.6.1 (residue setup), §4.2.4 (mapping + mode setup). **Reference:** lewton `header.rs`; stb_vorbis setup. **Reject floor 0** (`Unsupported`). Store all configs as arena offsets, each count bounded by a `MAX_*` const → `Unsupported` over-cap.

**Interface (Produces):** `Floor1Config`, `ResidueConfig`, `MappingConfig` (submap→floor/residue, coupling `(mag,ang)` pairs), `ModeConfig { blockflag, mapping }`, all parsed from the setup packet after codebooks, in spec order: floors → residues → mappings → modes → framing bit.

- [ ] **Step 1: Failing test** — a hand-built setup packet with 1 floor-1, 1 residue, 1 mapping (with one coupled pair), 2 modes parses into the expected config values; a floor-0 config → `Unsupported`; counts over `MAX_*` → `Unsupported`.
- [ ] **Step 2: Run, see fail.**
- [ ] **Step 3: Implement** the floor1/residue/mapping/mode setup parse into the arena.
- [ ] **Step 4: Tests green** → `VorbisDecoder::new` now fully parses a real file's 3 headers without error (add a test opening a real `tone-vorbis.ogg` header — fixture from Task 11 may be needed early; if so, generate it now via ffmpeg and commit).
- [ ] **Step 5: verify green. Commit.**

## Task 6: Per-packet decode — floor 1 curve + residue + dot product → spectrum

**DSP task.** **Files:** implement decode bodies in `floor.rs`, `residue.rs`; add the per-packet driver in `mod.rs` (up to the spectrum, before IMDCT).

**Spec:** §4.3.1–4.3.6 (packet decode: mode/window bits → floor decode → residue decode → inverse coupling → dot product). **Reference:** lewton `audio.rs`, stb_vorbis `vorbis_decode_packet_rest`. **Bug sources:** floor1 neighbor search + `render_line` integer interpolation + dB→linear; residue two-pass classification + partition/stage iteration + `begin/end` bounds; not applying residue to "unused" (all-zero floor) channels; residue type-2 concat/de-interleave.

**Interface:** internal — produce a per-channel `spectrum: [f32; blocksize/2]` (dot product of floor curve × residue) for a single (long) block, plus the per-channel "do_not_decode/unused" flags.

- [ ] **Step 1: Failing test** — drive one long-block audio packet from a real fixture (or hand-built) and assert the pre-IMDCT spectrum is finite, in range, and non-trivial; assert an all-zero-floor channel yields a zero spectrum. (Exact values validated end-to-end in Task 7 against reference PCM.)
- [ ] **Step 2: Run, see fail.**
- [ ] **Step 3: Implement** floor 1 decode + curve synthesis, residue decode (types 0/1/2), inverse coupling, dot product.
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: verify green. Commit.**

## Task 7: IMDCT + Vorbis window + overlap-add → first PCM (mono/long-block) + xtask tables

**DSP + tractable-scaffolding task.** **Files:** create `imdct.rs`, `tables.rs`; create `xtask/src/vorbis_tables.rs` + wire `gen-vorbis-tables`/`check` into `xtask/src/main.rs`; finish `decode_into`.

**Spec:** §4.3.7 (IMDCT), §4.3.8 (window + overlap-add). **Reference:** stb_vorbis `inverse_mdct` (or a naïve O(n²) IMDCT first — REQUIRED first, as the correctness oracle). Vorbis window: `sin(π/2 · sin²(π/2·(n+½)/N))`, per-side long/short halves. First audio packet emits **zero** samples (primes overlap); thereafter emit `bs(prev)/4 + bs(cur)/4` samples, carrying the right half per channel.

**xtask generator:** `gen-vorbis-tables` emits `src/formats/vorbis/vorbis_tables.rs` with the Vorbis windows for each supported block size and any IMDCT twiddles (host-side; no runtime trig). `check` re-derives and diffs (like `filterbank_tables::check`).

- [ ] **Step 1: Failing GOLDEN test** — `crates/soul-audio-core/tests/decode.rs`: decode a **mono, single-block-size** Vorbis fixture and compare to an ffmpeg `f32le`/`s16le` reference via segmental SNR (`load_s16le` + `best_align_segsnr`), require > ~60 dB, on the 16 MiB stack thread. (Generate the mono fixture + `.ref.pcm` via ffmpeg; commit them.)
- [ ] **Step 2: Run, see fail.**
- [ ] **Step 3: Implement** the naïve IMDCT + window (from generated tables) + overlap-add + first-packet priming; finish `decode_into` to emit interleaved `i32` PCM (`SampleFormat::Int { bits: 16 }`). Generate + commit the window tables via the new xtask command.
- [ ] **Step 4: Golden green** (SNR above bar). **This is the first end-to-end correct-PCM milestone.**
- [ ] **Step 5: verify green (incl. `vorbis_tables::check` and thumbv7em). Commit.**

## Task 8: Block switching (short blocks, long↔short transitions, priming)

**DSP task.** **Spec:** §4.3.8 window construction from `blockflag` + `previous/next_window_flag`; §1.3.2 block sizes. **Reference:** stb_vorbis window logic. **Bug sources:** wrong left/right half-size selection at transitions (clicks); wrong emitted sample count/offset; overlap state across a size change.

- [ ] **Step 1: Failing GOLDEN** — a fixture that actually switches blocks (transients / normal music) decodes to reference PCM within SNR. Commit fixture + `.ref.pcm`.
- [ ] **Step 2–4: Implement** short blocks + transition windows + correct overlap/emit accounting; golden green.
- [ ] **Step 5: verify green. Commit.**

## Task 9: Stereo coupling + residue type 2

**DSP task.** **Spec:** §4.3.5 (inverse coupling: (mag,ang) → L/R), §8 residue type 2 (concat all channels, decode as type-1, de-interleave). **Reference:** lewton `audio.rs` coupling.

- [ ] **Step 1: Failing GOLDEN** — a normal **coupled-stereo** Vorbis fixture (the standard A4-left/E5-right tone works, but use a real coupled encode) decodes to reference PCM within SNR, both channels correct.
- [ ] **Step 2–4: Implement** inverse coupling + residue type 2; golden green.
- [ ] **Step 5: verify green. Commit.**

## Task 10: Ogg seeking (granule bisection + overlap re-prime)

**DSP + integration task.** **Spec:** granule↔sample mapping; Ogg embedding (spec appendix). **Reference:** lewton `inside_ogg` seeking. Implement `SeekableDecoder::seek_to` on `VorbisDecoder`: `OggPackets::seek_to_granule` to land near the target page, then **decode-and-discard one packet** to rebuild overlap state before emitting. `total_frames` from `end_granule`.

- [ ] **Step 1: Failing test** — seek to `t` seconds, decode `N` samples, compare to a from-start decode aligned to `t` via SNR; assert reported position within tolerance; seek to 0 and beyond-end clamp behave.
- [ ] **Step 2–4: Implement** seek + re-prime; test green.
- [ ] **Step 5: verify green. Commit.**

## Task 11: Wire-in — probe→desktop/CLI dispatch, fixtures, goldens, profiles, qa-audio

**Integration task — complete code.** **Files:** `crates/soul-audio-desktop/src/decoder.rs`, `crates/soul-audio-cli/src/*`, `crates/soul-audio-cli/tests/e2e.rs`, `crates/soul-audio-core/tests/decode.rs`, `testdata/generate.sh`, `xtask/src/main.rs` (qa list).

- Desktop `OwnedDecoder`: add `Vorbis(VorbisDecoder<Src, alloc::boxed::Box<[u8]>>)` (desktop is std → heap arena; default 256 KiB, 512 KiB if a `SOUL_VORBIS_ROBUST` env/opt is set); `open_track` special-cases `Format::Vorbis` (probe) BEFORE the `open()` catch-all (mirror the DSF/DFF arms), constructing the arena with `vec![0u8; 256*1024].into_boxed_slice()`; delegate `Decoder`/`SeekableDecoder`/`total_frames`.
- CLI `decode`/`render`: add a `Format::Vorbis` arm constructing the same heap-arena `VorbisDecoder` so goldens run through the CLI.
- Profiles: expose the channel cap + max-blocksize + fast-huffman knobs as consts/features so the embedded profile (stereo, 4096 short-block, floor-0-reject) and desktop profile (8192, higher channel cap) both build from one codec. Document the firmware path (it constructs `VorbisDecoder<SdSrc, &'static mut [u8]>`) — firmware wiring itself is out of scope (issue #32).
- Fixtures: extend `testdata/generate.sh` to emit `tone-vorbis.ogg` (`ffmpeg -i tone.wav -c:a libvorbis -q:a 5 tone-vorbis.ogg`); commit it + the `.ref.pcm` under `crates/soul-audio-core/testdata/` (do NOT re-commit other regenerated fixtures — restore them, per the Task-2 lesson from the e2e suite).
- Goldens: `crates/soul-audio-cli/tests/e2e.rs` `real_vorbis_renders_stereo_tones` (analyze → A4-left/E5-right SFDR, the AAC pattern); confirm the core-tests goldens from Tasks 7–9 cover mono/switch/stereo. Add `tone-vorbis.ogg` to `xtask qa-audio`.

- [ ] **Step 1: Failing** CLI golden `real_vorbis_renders_stereo_tones` (fixture present).
- [ ] **Step 2: Run, see fail.**
- [ ] **Step 3: Implement** the desktop + CLI dispatch, the profile knobs, fixtures.
- [ ] **Step 4:** CLI golden + all core goldens green; `npm`-nothing here (Rust only).
- [ ] **Step 5: `cargo xtask verify` fully green (fmt, clippy, tables incl. vorbis, all tests, thumbv7em incl. the arena-generic codec, firmware build). Commit.**

---

## Self-review notes (for the executor)

- **The bit reader (Task 2) and codebook construction (Task 4) are the foundation** — if either is subtly wrong, every later golden fails inscrutably. Use the differential-vs-stb_vorbis dev tactic (compare intermediate values) the moment a golden misbehaves; don't debug from "audio sounds wrong."
- **Naïve IMDCT first (Task 7), always** — validate correctness before any optimization; keep it as the oracle. IMDCT optimization is explicitly deferred (v1.1 / issue follow-up).
- **thumbv7em must stay green every task** — the codec is arena-generic and no-alloc, so it compiles for embedded even before firmware supplies an arena. If a task breaks the cross-build, it broke the no-alloc discipline.
- **Arena over-cap = `Unsupported`, never panic** — fuzz truncated/oversized setup headers (Task 4–5).
- **Fixtures are lossy** — SNR/spectral tolerance, never bit-exact; align for first-packet priming before comparing.
- **Do not re-commit regenerated `testdata/` binaries** — only the new `tone-vorbis.ogg` + `.ref.pcm` (encoders aren't deterministic; the existing fixtures back other goldens).
- Never add AI trailers to commit messages.
