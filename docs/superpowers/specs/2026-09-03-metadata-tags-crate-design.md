# Design: `soul-audio-tags` — a no-alloc, cross-format metadata + cover-art crate

**Status:** design (approved in brainstorming 2026-09-03)
**Author:** session continuation (Claude Code)
**Related:** [[desktop-player-plans]]; follows the Plan 3/4 desktop player. DSD playback correctness is a *separate* effort (see "Out of scope").

## 1. Problem & goal

The desktop player reads track metadata (title/artist/album/duration) via **`lofty`**, a `std`-only crate, in `soul-audio-service`'s `library.rs`. Two gaps:

1. **Not reusable on the STM32 DAP.** The firmware target (`thumbv7em-none-eabihf`, Cortex-M4F) is `no_std` + **no-alloc** (enforced by `architecture-tests/no_alloc.rs` at runtime and by the `thumbv7em` build at compile time). `lofty` cannot run there, so the DAP has no way to show track metadata or cover art from the same code path — risking desktop/DAP divergence.
2. **No cover art at all.** Cover art was explicitly deferred as out-of-scope in Plan 4; there is no art-read path anywhere.

**Goal:** one **`no_std`, no-alloc** crate — `soul-audio-tags` — that reads tags **and** cover art for every format in the library, is the single source of truth for both desktop and the STM32 DAP, and replaces `lofty` in the service. It exposes a `std`/`alloc` convenience layer for the desktop.

**Library reality (this machine):** FLAC ×1038, DSF ×147, MP3 ×100, WAV ×60, Opus ×11. (`.opus` is currently not even scanned — a related gap the service fix will also close.)

## 2. Prior art & rationale (researched)

- **Symphonia** (`symphonia_core::meta`), the canonical pure-Rust multimedia library, models a `Tag` as a mandatory **raw** key/value (format fidelity) plus an optional **standard** normalized enum value; cover art is a **`Visual`** with a standardized usage key, media type, and optional dimensions. Its values use `Arc` — it needs `alloc`.
- **Lofty** models `Picture { PictureType, MimeType, data }` with **owned** bytes.
- **No `no_std`/no-alloc cross-format tag library exists** in the ecosystem.

**Decision:** adopt Symphonia's proven data model (normalized field enum + cover-art descriptor with a usage key), and contribute the missing **no-alloc adaptation**: a streaming *visitor* over a `SeekSource`, with cover art as a byte-range descriptor. This gives long-term maintainability (a proven model, so consumers never special-case formats) plus the `no_std` capability neither reference library offers.

## 3. Constraints

- **`no_std` + no-alloc core.** The default build allocates nothing: no `String`/`Vec`/`Box` on the read path. Tag values are `&str` slices into a caller-provided scratch buffer; cover art is a descriptor `{ mime, offset, len }`. Verified by (a) building the crate on `thumbv7em-none-eabihf` in `xtask verify`, and (b) an allocation-counting test modeled on `architecture-tests/no_alloc.rs`.
- **Reads through `embedded_io`** via the existing `Source`/`SeekSource` seam, so it runs unchanged over a desktop `File`, an in-flash `SliceSource`, or the firmware's `SdSource` (which already `impl embedded_io::Read + Seek`).
- **Integer-only in the core** (no `f64`): `StreamFacts` returns `total_frames`/`sample_rate`/`channels`; seconds are computed by the desktop layer. Cortex-M4F has only an f32 FPU.
- **Byte-wise parsing** (`u32::from_be_bytes` on slices; no unaligned pointer casts) for Cortex-M safety/portability.
- **Panic-free / robust:** malformed or truncated tags return `TagError` or are skipped, never panic — mirroring the folder scan's existing robustness policy.
- **No churn to the root gate** beyond adding the new crate to the workspace + `xtask verify`.

## 4. Architecture

A new workspace crate `crates/soul-audio-tags/`, sibling to `soul-audio-core` (kept out of core so it never enlarges the stack-sized decoder types — see [[anydecoder-size-stack]]).

```
crates/soul-audio-tags/
  Cargo.toml         # no_std default; feature "alloc" (and "std") for the collector
  src/lib.rs         # public model: Field, Picture, TagSink, read(), StreamFacts, TagError
  src/locate.rs      # magic-byte sniff → dispatch to the right container walker
  src/id3v2.rs       # ID3v2.2/2.3/2.4 reader (MP3, WAV `id3 ` chunk, DSF)
  src/vorbis.rs      # VorbisComment reader + FLAC block walk (FLAC, Opus/Ogg)
  src/mp4.rs         # MP4 atom walk + `ilst` reader (M4A/AAC/ALAC)
  src/base64.rs      # streaming (no-alloc) base64 decoder for Ogg embedded pictures
  src/owned.rs       # #[cfg(feature = "alloc")] Tags collector + read_owned + read_picture_bytes
  tests/             # per-format fixtures + differential test vs lofty
```

### 4.1 Public data model (no-alloc core)

```rust
#![no_std]

/// Normalized field, modeled on Symphonia's StandardTag (trimmed to a player's needs).
#[non_exhaustive]
pub enum Field {
    Title, Artist, Album, AlbumArtist,
    TrackNumber, TrackTotal, DiscNumber, DiscTotal,
    Date, Genre, Composer,
}

pub enum MimeType { Jpeg, Png, Other }
pub enum PictureKind { FrontCover, BackCover, Artist, Other }   // ID3v2 APIC set, normalized
pub enum DataEncoding { Raw, Base64 }                            // Ogg embeds base64

/// A cover-art *descriptor* — never the bytes.
pub struct Picture {
    pub usage: PictureKind,
    pub mime: MimeType,
    pub encoding: DataEncoding,
    pub data_offset: u64,   // location of the image bytes within the source
    pub data_len: u32,
    pub width: Option<u32>, // present when the container declares it (e.g. FLAC PICTURE)
    pub height: Option<u32>,
}

/// Cheap stream facts read from the container header during the same walk.
pub struct StreamFacts {
    pub total_frames: Option<u64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
}

/// The streaming visitor. `value`/`raw_key` borrow internal scratch — copy what you keep.
pub trait TagSink {
    fn text(&mut self, field: Option<Field>, raw_key: &str, value: &str);
    fn picture(&mut self, pic: &Picture);
}

pub enum TagError { Io, Malformed, Unsupported }

/// Read tags + facts from `src`. `scratch` holds one field value at a time.
pub fn read<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError>;
```

- `field: Option<Field>` → `None` means an unrecognized key; `raw_key` is still delivered so nothing is lost (fidelity, per Symphonia's raw+standard split).
- `Picture` has **no borrows** (all enums/offsets/`Option`s), so it is `Copy` and the `alloc` layer stores it directly — there is no separate owned picture type.
- Values longer than `scratch` are **truncated at a UTF-8 boundary**; the `text` signature stays clean and this is a documented convention (not a per-call flag). 256 B is the recommended scratch size; titles rarely exceed it.

### 4.2 Parsers

**Container locators** (`locate.rs`) sniff magic bytes and seek to where tags live, then hand off to a tag-format reader:

| Container | Sniff | Tag location | Reader |
|-----------|-------|--------------|--------|
| FLAC | `fLaC` | METADATA_BLOCK chain (VORBIS_COMMENT #4, PICTURE #6, STREAMINFO #0 for facts) | `vorbis` |
| Ogg/Opus | `OggS` | 2nd packet (`OpusTags`/`vorbis` comment); granule for duration | `vorbis` |
| MP3 | `ID3`/frame sync | ID3v2 at offset 0; Xing/Info for duration | `id3v2` |
| WAV | `RIFF…WAVE` | `id3 ` chunk (ID3v2) and/or `LIST/INFO`; `fmt `/`data` for facts | `id3v2` |
| MP4 | `…ftyp` | `moov/udta/meta/ilst`; `mvhd` for duration | `mp4` |
| DSF | `DSD ` | header's metadata pointer → ID3v2; header sample count for facts | `id3v2` |
| DFF | `FRM8…DSD ` | (v1: facts only; proprietary text chunks deferred) | — |

**Tag-format readers** (three cover everything):
- **`id3v2`** — 2.2/2.3/2.4, unsynchronization, extended header skip; frames `TIT2`/`TPE1`/`TALB`/`TPE2`/`TRCK`/`TPOS`/`TDRC`(+`TYER`)/`TCON`/`APIC`. `TRCK`/`TPOS` split `n/total` into `TrackNumber`/`TrackTotal`, `DiscNumber`/`DiscTotal`. `APIC` → `Picture{ encoding: Raw }`.
- **`vorbis`** — `KEY=VALUE` comment vector: `TITLE`/`ARTIST`/`ALBUM`/`ALBUMARTIST`/`TRACKNUMBER`/`TRACKTOTAL`/`DISCNUMBER`/`DATE`/`GENRE`/`COMPOSER`. Cover art: FLAC native PICTURE block (`Raw`), or `METADATA_BLOCK_PICTURE` in a comment (`Base64`, decoded by `base64.rs` on demand).
- **`mp4`** — `ilst` atoms `©nam/©ART/©alb/aART/trkn/disk/©day/©gen/covr`; `covr` → `Picture{ encoding: Raw }` (JPEG/PNG per data-atom type).

### 4.3 Cover art

The reader emits a `Picture` **descriptor** per embedded image (front, back, artist…); it never buffers image bytes. Consumers read the bytes on demand from `data_offset`/`data_len`:
- **Desktop:** `read_picture_bytes(src, &PictureRef) -> Vec<u8>` (in `owned.rs`) reads and, for `Base64`, decodes into a `Vec<u8>`; loaded **only** when the UI needs to show art — never during the whole-library scan.
- **STM32 DAP:** stream the range from SD directly to the panel / hardware JPEG codec (STM32H7) in chunks; `Base64` uses `base64.rs`'s streaming decoder. No heap, no multi-MB buffer.

### 4.4 Duration

From container headers, cheaply, during the same walk: FLAC STREAMINFO (`total_samples`), MP4 `mvhd` (`duration`/`timescale`), DSF header (sample count), WAV (`data` size ÷ block align), Ogg (last-page granule). **MP3 is the soft spot:** use the Xing/Info/VBRI header when present, else estimate from bitrate × file size — documented as an approximation. This keeps the library scan from opening every decoder just for a duration.

## 5. `std`/`alloc` convenience layer (`owned.rs`, feature-gated)

```rust
pub struct Tags {
    pub title: Option<String>, pub artist: Option<String>, pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track: Option<u32>, pub track_total: Option<u32>,
    pub disc: Option<u32>, pub date: Option<String>, pub genre: Option<String>,
    pub composer: Option<String>,
    pub pictures: Vec<Picture>,   // descriptors only (offset/len/mime/usage/encoding)
}
pub fn read_owned<S: SeekSource>(src: &mut S) -> Result<(Tags, StreamFacts), TagError>;
pub fn read_picture_bytes<S: SeekSource>(src: &mut S, p: &Picture) -> Result<Vec<u8>, TagError>;
```

`read_owned` allocates the scratch buffer, runs `read` with an `OwnedTags` visitor that pushes into `Tags`, and returns it. This is what the service uses.

## 6. Integration

- **`soul-audio-service` (`library.rs`):** `read_tags` swaps `lofty` → `soul_audio_tags::read_owned` over the `File`. Duration from `StreamFacts.total_frames`/`sample_rate` (fallback: open the decoder only if a format returned no facts). Add `.opus` to the supported extensions. `Track`/`TrackView` gain `has_art: bool` (true when `read_owned` yielded ≥1 picture) so the UI knows art exists without loading it during the scan.
- **App (Tauri):** new command `track_art(id: u32) -> Option<TrackArt>` where `TrackArt { mime: String, bytes: Vec<u8> }` (specta-typed). It resolves `id → path`, re-reads the front-cover `Picture`, reads the bytes, returns them. Called **lazily** by the frontend for the now-playing track.
- **Frontend:** `PlayerBar.svelte` shows the now-playing cover (from `track_art`, as a Blob/data URL) with a neutral placeholder when `has_art` is false. Per-row thumbnails are a later enhancement, not v1.
- **`xtask verify`:** add `soul-audio-tags` to the `thumbv7em-none-eabihf` build list.

## 7. Firmware reuse guarantees (explicit)

1. **Input:** `read<S: SeekSource>` runs over the firmware's `SdSource` (already `impl embedded_io::Read + Seek`) exactly as decoders do via `open(source)`. No new plumbing.
2. **Compile:** the crate is built on `thumbv7em-none-eabihf` in `xtask verify` every commit.
3. **No-alloc:** default build has no `alloc`; an allocation-counting test asserts zero runtime allocations on the read path.
4. **Integer facts:** no `f64` in the core.
5. **Streaming art:** cover art is a byte range the firmware streams to the panel / hardware JPEG codec; base64 (Ogg) decodes chunk-wise. No multi-MB buffer.

Wiring the DAP's actual screen (display driver, layout) is **separate firmware work**, out of this spec; this spec makes the crate DAP-ready and proves it compiles/links no-alloc for the target.

## 8. Testing

- **Per-reader unit tests** with hand-built minimal tag blobs (following the `library.rs` `wav()` fixture-builder style) — deterministic, in-repo.
- **Committed tiny fixtures** per format (FLAC/MP3/DSF/M4A/Ogg/WAV) with known title/artist/album + a small embedded picture; assert normalized `Field`s and the `Picture` descriptor (offset/len/mime).
- **Differential test** (`std`-only, `lofty` dev-dependency): walk a corpus and assert our normalized fields equal lofty's for title/artist/album/track/etc. Run locally against the real library; a small committed subset runs in CI. This is the parity gate before `lofty` is dropped.
- **No-alloc test** modeled on `architecture-tests/no_alloc.rs`, wrapping a full `read()` in the measured region.
- **Robustness:** truncated/garbage inputs per format return `TagError`/skip, never panic.

## 9. Scope

**In v1:** the three tag readers (ID3v2, VorbisComment, MP4 ilst) covering FLAC, Opus/Ogg, MP3, WAV, DSF, M4A/AAC/ALAC; cover-art descriptors + on-demand bytes; integer stream facts incl. duration; the `std` collector; service swap off `lofty`; `.opus` scanning; `has_art` + `track_art` + now-playing cover in `PlayerBar`; firmware-ready (compiles thumbv7em, no-alloc verified).

**Out of scope (separate efforts):** DSD playback correctness (the decimation/decoder bug — its own debugging pass); DFF proprietary text chunks; tag *writing*; per-row art thumbnails; wiring the DAP screen; RIFF `INFO` (WAV uses its `id3 ` chunk in v1).

## 10. Risks

- **Parser parity with `lofty`** — mitigated by the differential test before dropping `lofty`. Keep `lofty` as a `git`-history fallback until the differential test is green on the full local corpus.
- **MP3 duration accuracy** — Xing/Info covers most; the bitrate estimate is a documented approximation, acceptable for a library list.
- **ID3v2 breadth** (unsync, v2.2 3-char frame ids, footers) — covered by targeted fixtures for each quirk.
