# Vorbis I Decoder — Design

**Status:** Approved design (2026-09-05). Implements the Vorbis half of Forgejo issue **#24** ("Opus & Vorbis audio decoding in soul-audio-core"). Opus is a separate, later effort.

**Goal:** A from-scratch Vorbis I decoder in `soul-audio-core` so the player can **decode and play** `.ogg`/Vorbis files (today they scan + show a tag-derived duration but fail to play), with working **seek**, on both desktop (heap) and the embedded DAP (`thumbv7em`, no heap).

**Non-goal:** Opus, floor 0, Vorbis multichannel beyond the configured cap, stream chaining/concatenation.

---

## Why this shape (grounded in research)

Vorbis is unusual: its **setup header** (codebooks, floors, residues, mappings, modes) is **file-variable** — the file declares, at runtime, how many of each and how big. The only spec ceilings are enormous and useless for static sizing (256 codebooks, 16.7M entries each). Every other codec in this crate (FLAC/AAC/ALAC/MP3) has structures the spec bounds tightly and small, so fixed arrays sufficed. Vorbis genuinely needs runtime-sized storage — and this crate has no heap.

Real embedded Vorbis decoders all converge on the same answer (Rockbox/Tremor, stb_vorbis, the STM32F103 lowmem port): **a per-stream bump allocator over a fixed region the platform provides**, reset per track, `free` = no-op, with **graceful rejection** when a file's setup overflows the region. Xiph's own budget guidance: ~150 KB typical, 250 KB "use low-mem below this", ~500 KB rare worst case. Rockbox gives its codec ~900 KB; stb_vorbis's `alloc_buffer` lets the *caller* choose SRAM vs PSRAM.

We adopt exactly that: an **arena-generic, no-alloc codec** whose storage the caller supplies.

---

## Global Constraints

- **No AI trailers in commits** (lefthook rejects `Co-Authored-By: Claude` / `Claude-Session`); commits are GPG-signed.
- **`soul-audio-core` stays strictly `no_std` + no-alloc + `#![forbid(unsafe_code)]`.** The Vorbis codec must add **no** `alloc` dependency and **no** `unsafe`, and the default build + the `thumbv7em-none-eabihf` cross-build in `cargo xtask verify` must stay green. Core's only dependency remains `embedded-io` (enforced by `architecture-tests`).
- **No runtime transcendentals / no `libm`.** All trig/window/twiddle tables are precomputed host-side via an `xtask` generator and checked in (the established pattern: `resample_tables`, `filterbank_tables`, `sbr_tables`, `ps_tables`).
- **Lossy codec:** goldens use SNR/spectral tolerance vs an ffmpeg reference, never bit-exact (mirrors the AAC golden pattern, not the FLAC lossless one).
- **Fixture naming:** `tone.ogg` is already the FLAC-in-Ogg fixture; the Vorbis fixture is **`tone-vorbis.ogg`** (same A4-left/E5-right 2 s signal).

---

## Architecture

### The arena seam (the crux)

```rust
// core, no-alloc. `A` is caller-owned storage; the codec bump-allocates within it
// and stores OFFSETS (never references) into it — no self-reference, no unsafe.
pub struct VorbisDecoder<S, A>
where
    S: Source,                       // the crate's embedded-io Source ladder
    A: AsMut<[u8]> + AsRef<[u8]>,     // core names no concrete storage → stays no-alloc
{ /* Ogg demux state, parsed setup as offsets into the arena, per-frame DSP state */ }
```

- **Bump allocator over `A`**, reset per stream, `free` = no-op — the Rockbox `codeclib` model. Setup structures (codebook tables, floor/residue configs, mode/mapping tables) are laid down in the arena at open; each is addressed by an offset + length, so the struct holds no borrows into itself.
- **Platform supplies `A`:**
  - **Desktop / CLI:** a heap `Box<[u8]>` (256 KB default). Because the arena sits behind a `Box` pointer, it does **not** bloat any by-value enum or the decode-thread stack.
  - **Firmware (DAP):** a `&'static mut [u8]` in internal SRAM (H7 DTCM) or external PSRAM/SDRAM, sized to the board.
- **Graceful over-cap:** a file whose setup exceeds the arena returns `DecodeError::Unsupported` (a `SetupTooLarge`-style condition mapped onto the existing `#[non_exhaustive]` `DecodeError`). Never panic, never overrun.
- **Two logical regions:** a *persistent setup arena* (lives for the stream) and *per-frame DSP scratch* (IMDCT + overlap-add working buffers, reused every frame). The setup arena is `A`; per-frame scratch can be fixed-size arrays sized by the block-size cap (they're spec-bounded once channels + max blocksize are capped).

### Dispatch: per-platform, NOT in `AnyDecoder`

Vorbis's "needs a caller-provided scratch region" resource model differs from the self-contained by-value codecs in core's `AnyDecoder`. Rather than thread an arena type-param through `AnyDecoder` → `Deck`/`Engine` (an ugly ripple that would force every WAV decode to carry the param), **Vorbis is dispatched by each std/embedded consumer**:

- **`soul-audio-core`** provides `VorbisDecoder<S, A>` + the new Ogg packet demuxer + a `probe` that recognizes Vorbis, but **does not** add a `Vorbis` variant to `AnyDecoder`. `open()` returns `DecodeError::Unsupported` for Vorbis (the codec is constructed directly by consumers with their arena).
- **`soul-audio-desktop`:** `OwnedDecoder` gains a `Vorbis(VorbisDecoder<Src, Box<[u8]>>)` variant; `open_track` constructs it with a heap arena when the probe says Vorbis (mirrors how DSF/DFF are already special-cased there).
- **`soul-audio-cli`:** its `decode`/`render` format dispatch gains a Vorbis arm (heap arena) so the golden tests run through the CLI like every other format.
- **Firmware:** constructs `VorbisDecoder<SdSrc, &'static mut [u8]>` when it wants Vorbis (later; see profiles).

### New Ogg packet demuxer (core)

Core's existing `OggReader` (`demux/ogg.rs`) byte-flattens Ogg into a native-FLAC stream and **discards packet boundaries** — unusable for Vorbis (a packet codec). The tags crate's `PageWalker`/`PacketReader` is the correct shape but lives in `soul-audio-tags` (core may not depend on it — layering forbids it). So we **port a packet-preserving Ogg demuxer down into core** (`demux/ogg_packets.rs` or extend `demux/ogg.rs`):

- Page header parse + 255-entry lacing table; packet reassembly across pages/segments (a segment `< 255` terminates a packet); **granule-position** capture per page (needed for `total_frames` and seek).
- A `Source`-based streaming path for decode, plus a `SeekSource` path for granule bisection.
- Byte-for-byte reuse the tags `PageWalker` logic as the template.

### DSP: self-contained IMDCT + Vorbis window

The AAC IMDCT (`formats/aac/filterbank.rs`) is private and fixed to AAC block sizes (2048/256) and windows (sine/KBD) — not reusable for Vorbis (64–8192 blocks, Vorbis's own window). Build a **self-contained Vorbis IMDCT**:

- **Naïve O(n²) IMDCT first** (correct, easy to validate), behind a feature/flag as a differential oracle; optimize to a split-radix/FFT IMDCT later.
- **Vorbis window** `sin(π/2·sin²(π/2·(n+½)/N))` with per-side long/short halves selected by the `blockflag` + `previous/next_window_flag` bits, precomputed host-side per supported block size.
- **Overlap-add state** carried per channel across packets; the first audio packet emits no PCM (priming); emit `blocksize(prev)/4 + blocksize(cur)/4` samples per packet thereafter.

### Codec scope

- **Floor 1 only** (floor 0 is dead in practice — reject it as `Unsupported`).
- **Residue types 0/1/2**, **stereo coupling** (inverse mag/angle), **modes** (long/short), **mappings**.
- Codebooks: canonical Huffman construction from the length list (ordered + sparse), VQ lookup types 0/1/2. **Keep lattice VQ (type 1) compact** — store only the multiplicand list and compute vectors on the fly (divide+modulo per element); do **not** pre-expand (the dominant RAM knob per stb/Tremor).
- **Bit reader is LSb-first** (Vorbis convention), with `ilog`, signed reads, and `float32_unpack` (mantissa/exponent, not IEEE-754).

### Profiles

Both shipped from the start:
- **Desktop profile:** full 8192 blocks, channels up to a higher cap (e.g. 8), 256 KB arena default (512 KB "accept-anything" option).
- **Embedded profile:** **stereo cap**, optional **4096 short-block** cap (rejects long-block-8192 files; ~halves the IMDCT/PCM working set), floor-0 rejected, arena a fixed static region. Channel cap + max-blocksize are compile-time knobs (const generics or features) so both profiles come from one codec.

### Seeking (in v1)

- **Ogg-granule bisection:** binary-search Ogg pages by `granulepos` (last sample of the page) to land near the target sample.
- **Re-prime:** after seeking, decode-and-discard one packet to rebuild the overlap-add state before emitting audio (a hard requirement — emitting immediately after a seek produces a wrong first block).
- Exposed via `SeekableDecoder::seek_to` on the consumer decoder (desktop `OwnedDecoder`), so the app's seek bar works for Vorbis.

---

## Build order (phased — each phase a golden-tested milestone)

Layer the Ogg demuxer separately from the codec so packet decode is unit-testable with hand-fed packets.

1. **Ogg packet demuxer in core** — page/lacing parse, cross-page packet reassembly, granule capture, `Source` + `SeekSource` paths. Unit tests with hand-built pages (tags `ogg_page` helpers are a model).
2. **Bit reader + primitives** — LSb-first `read_bits`, signed, `ilog`, `float32_unpack`. Unit-tested against spec examples. *Foundation — must be provably correct before anything else.*
3. **Setup-header parse into the arena** — codebooks (Huffman build + compact VQ), floor 1, residues, mappings, modes; the bump-arena + graceful over-cap. Milestone: parse a real file's three headers, dump the structure, reject an over-cap fixture cleanly.
4. **Single long-block audio packet → spectrum** — floor 1 decode + residue decode + dot product, one channel, one long block.
5. **Naïve IMDCT + window + overlap-add (mono, all-long-block)** — **smallest correct-PCM milestone**: a mono/simple file decodes to PCM matching an ffmpeg reference within SNR tolerance.
6. **Block switching** — short blocks + long↔short window transitions + persistent overlap + first-packet priming.
7. **Stereo coupling + residue type 2** — inverse mag/angle coupling; a normal coupled-stereo file decodes correctly.
8. **Ogg seeking** — granule bisection + re-prime; seek round-trip test.
9. **Wire-in + profiles + fixtures** — `probe` detects Vorbis; desktop `OwnedDecoder::Vorbis` (heap arena) + CLI dispatch arm; embedded profile knobs; `testdata/tone-vorbis.ogg`; CLI + core goldens; add to `xtask qa-audio`.
10. **(Deferred to v1.1) IMDCT optimization** — split-radix/FFT IMDCT, keep naïve as the differential oracle.

The `no_std`/thumbv7em cross-build must stay green at every phase (the codec compiles for embedded even before the firmware wires an arena — it's arena-generic).

---

## Testing

- **CLI golden** (`crates/soul-audio-cli/tests/e2e.rs`): `real_vorbis_renders_stereo_tones` — decode `tone-vorbis.ogg`, `analyze` (Hann FFT → peak_hz + SFDR) to assert A4-left / E5-right land on the right channels (the AAC pattern, since Vorbis is lossy).
- **Core golden** (`crates/soul-audio-core/tests/decode.rs`): segmental-SNR vs an ffmpeg `*.ref.pcm` (`load_s16le` + `best_align_segsnr`), plus a non-silent/in-range backstop. Big-stack thread for the by-value decoder.
- **Differential debugging** (dev aid, highest leverage): compare intermediate values (codebook entries, floor curves, residue vectors, pre-IMDCT spectra) against stb_vorbis (public domain) — how lewton was built. Not a committed test; a scratch tool.
- **Robustness:** truncated/corrupt setup headers must return `Unsupported`/`Malformed`, not panic — especially the arena over-cap path.
- **Seek test:** seek to `t`, decode, compare against a from-start decode aligned to `t` (SNR), and assert position lands within tolerance.
- **Determinism / alignment:** account for first-packet priming when aligning to the reference.

## References (borrow algorithms/tables; mind licenses)

- Xiph **Vorbis I spec** — `https://xiph.org/vorbis/doc/Vorbis_I_spec.html` (ground truth; implement from this).
- **stb_vorbis** (public domain / MIT) — arena (`alloc_buffer`) model, compact-VQ (`DIVIDES_IN_CODEBOOK`), fast-Huffman, IMDCT, bounding constants. Best fixed-footprint reference.
- **Tremor `lowmem`** (BSD) — integer/low-memory codebook handling; embedded reference.
- **lewton** (MIT/Apache, pure-Rust, `#![forbid(unsafe_code)]`) — cleanest Rust structure/control-flow model.
- **Symphonia** `symphonia-codec-vorbis` (MPL — do not vendor code, cross-check only).

## Risks / open items

- **Arena sizing:** 256 KB default may reject rare huge-setup files; 512 KB option + graceful rejection mitigate. Calibrate against a real `.ogg` corpus during Phase 3.
- **IMDCT correctness before speed:** validate the naïve IMDCT against reference PCM before any optimization; keep it as the oracle.
- **Codebook Huffman construction** and **lattice-VQ reconstruction** are the top bug sources — differential-test them at the intermediate-value level (Phase 3–4).
- **`AnyDecoder` exclusion:** Vorbis being dispatched per-consumer (not in `AnyDecoder`) means three small dispatch sites (desktop, CLI, firmware) instead of one — accepted, to avoid the arena type-param ripple through the engine/deck.
- **Firmware silicon:** arbitrary-file Vorbis needs an F7/H7-class part (≥320 KB SRAM) or PSRAM; the embedded 4096/stereo profile + rejection make smaller parts degrade gracefully rather than fail hard.

## Out of scope (tracked separately)

- **Opus** decoding (the other half of #24 — its own design/plan).
- Floor 0, multichannel beyond the cap, Ogg stream chaining.
- IMDCT SIMD/fixed-point for no-FPU parts (tremor-style) — future, if a target needs it.
