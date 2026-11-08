# Metadata expansion + DAP on-SD index — design exploration

> **Status: EXPLORATORY — a documented possibility, not a committed spec.**
> Nothing here is scheduled for implementation. It captures a brainstorming
> discussion (2026-11-03) so the ideas aren't lost. If/when any part is picked
> up, it gets its own `docs/superpowers/specs/` spec → plan → implementation
> cycle. Treat every "would / could" below as a proposal, not a decision.

## Why this note exists

Two wishes surfaced:

1. **Richer metadata.** Today `soul-audio-tags` reads a deliberate "player
   subset" of fields (Title, Artist, Album, AlbumArtist, Track/Total,
   Disc/Total, Date, Genre, Composer) plus cover art and stream facts. We'd
   like the option of Comment, Lyrics, ReplayGain, BPM, Publisher, Copyright,
   MusicBrainz IDs, Grouping, etc. — *without* bloating the lean core model.
2. **A big library on the DAP.** The embedded player (STM32H743, Embassy,
   `no_std`/no-alloc/`forbid(unsafe_code)`) should browse **thousands of files
   on an SD card**. That raises the real question: where does all that metadata
   live, given ~1 MB of internal SRAM and no external SDRAM on a stock
   Nucleo-class board?

A third force shapes both: **a future backend/service will enrich libraries
(MusicBrainz, etc.)**. That's "later later," but it means the eventual source of
truth for rich metadata is a library database the backend owns — files on disk
become just one input. The model below is built to stay forward-compatible with
that without designing the enrichment service now.

## Where metadata stands today (baseline)

- `crates/soul-audio-tags` — `no_std`/no-alloc streaming `TagSink` reader + an
  `alloc`-gated owned `Tags` snapshot. Dialects: ID3v2 (2.2/2.3/2.4),
  VorbisComment, MP4 `ilst`, RIFF `INFO`. Containers wired: FLAC, Ogg, MP3,
  WAV, DSF, DFF, ADTS, MP4 (8).
- Cover art as a `Picture` *descriptor* (offset/len/mime/usage/dims, never bytes
  on the no-alloc path); bytes materialized on demand, incl. Ogg
  `METADATA_BLOCK_PICTURE` base64 reassembly.
- `StreamFacts { total_frames, sample_rate, channels }` → duration. Per-format
  length logic: FLAC STREAMINFO, MP3 Xing/Info/VBRI (+CBR estimate), Ogg
  last-page granule (Vorbis & Opus), ADTS frame-scan, WAV/DSF/DFF headers, MP4.
- Proven by a differential test against `lofty` 0.22; drives the desktop app
  (now-playing art, library rows, durations).

### Known gaps the note touches

- Metadata coverage trails *decode* coverage by exactly the #75–#90 container
  additions: **no tag reader for AIFF/AIFF-C, Matroska/WebM, Apple CAF, or
  WavPack**, and **no APEv2** dialect.
- Read-only — no tag writing.
- The service's `read_tags` doc-comment still calls MP3/Opus duration a "KNOWN,
  ACCEPTED REGRESSION → 0.0", but the tags crate now computes it. **Verify the
  wiring still populates `total_frames` for those, then delete the stale note.**

## Part 1 — A tiered metadata model ("keep it minimal, extras somehow")

The instinct to "keep it minimal and treat the rest as additional metadata" maps
cleanly onto **three tiers**. The streaming `TagSink` already hands the sink
*every* tag with its `raw_key`, so extended data is already flowing past us — it
just isn't captured. The tiers decide what gets *normalized* and what gets
*stored where*.

- **Tier 1 — Core (`Field`).** The existing ~11 fields. These are what a
  *browser* needs: title/artist/album/albumartist/track/disc/date/genre/
  composer. Small, typed, always present in the fast path. **Unchanged.**
- **Tier 2 — Extended (`ExtendedField`).** A normalized enum for well-known
  extras that have real cross-format encodings worth unifying:
  Comment, Lyrics (unsynced), Bpm, ReplayGainTrackGain/TrackPeak/AlbumGain/
  AlbumPeak, Publisher, Copyright, Grouping, Isrc,
  MusicBrainzTrackId/ReleaseId/ArtistId/ReleaseGroupId. Normalization matters
  here because the same datum is `TXXX:REPLAYGAIN_TRACK_GAIN` (ID3),
  `REPLAYGAIN_TRACK_GAIN` (Vorbis), and `----:com.apple.iTunes:…` (MP4).
- **Tier 3 — Raw passthrough.** Any key we didn't model, delivered as
  `(raw_key, value)`. Captures everything for completeness and for the future
  enrichment pipeline, at zero modeling cost and no normalization promise.

### Why a tier split rather than one big enum

- **Flat big `Field`** (just add 20+ variants): simplest code, but the "core"
  identity is lost, the owned struct grows 20+ `Option<String>`, and — critically
  — the DAP's fast browse record would widen with fields it never shows while
  scrolling. Rejected.
- **Core enum + raw-only for everything else**: minimal code, but throws away
  cross-format normalization for ReplayGain/MBIDs exactly where it's most
  valuable. Rejected.
- **Three tiers** (recommended): core stays lean and is all the browse index
  needs; extended is normalized but lives off the fast path; raw catches the
  long tail and feeds enrichment. Matches the "minimal + additional" intuition
  directly.

Owned snapshot sketch (desktop / `alloc` side only; the no-alloc streaming path
is unchanged):

```rust
pub struct Tags {
    // Tier 1 — unchanged
    pub title: Option<String>, pub artist: Option<String>, /* … */
    // Tier 2 — normalized extras (all optional)
    pub comment: Option<String>,
    pub lyrics: Option<String>,
    pub bpm: Option<u16>,
    pub replaygain: Option<ReplayGain>,      // {track_gain, track_peak, album_gain, album_peak}
    pub publisher: Option<String>, pub copyright: Option<String>,
    pub grouping: Option<String>, pub isrc: Option<String>,
    pub musicbrainz: MusicBrainzIds,         // track/release/artist/release_group
    // Tier 3 — long tail
    pub other: Vec<(String, String)>,
    pub pictures: Vec<Picture>,
}
```

### Format-reader work implied

Per dialect, map the new keys into Tier 2 / Tier 3: ID3v2 `COMM`/`USLT`/`TBPM`/
`TPUB`/`TCOP`/`TIT1`/`TXXX:*`/`UFID`; VorbisComment `COMMENT`/`LYRICS`/`BPM`/
`REPLAYGAIN_*`/`MUSICBRAINZ_*`; MP4 `©cmt`/`©lyr`/`tmpo`/`----:*`. **Adjacent,
optional:** adding the missing *containers* (AIFF/Matroska/CAF/WavPack) and the
**APEv2** dialect would make metadata coverage == decode coverage — a natural
companion but a separable scope.

## Part 2 — Storing it on the DAP (the on-SD index)

**Constraint recap:** thousands of files, ~1 MB SRAM, no SDRAM, FAT on SD via
`embedded-sdmmc`. You cannot (a) re-parse files to browse, nor (b) hold the
library's metadata in RAM.

**Decision from the discussion:** the **desktop app is the indexer**; the **DAP
is a pure reader**. The desktop already has the scanner + `soul-audio-tags` +
(later) the enrichment backend. During *import to the DAP* it prepares the
folder structure, bakes art thumbnails, and writes a compact index. The firmware
never parses a tag.

> **Reframe worth keeping:** text metadata is *small*. A fat record (all tiers +
> MusicBrainz GUIDs + a comment) is ~0.5–2 KB; 10k tracks ≈ 5–20 MB — trivial on
> an SD card. The only large cost is **cover art**.

### Proposed on-SD layout

```
/.soulidx/
  index.sidx      # header + core record table + order tables + string heap
  extended.blob   # tier-2/tier-3 values, per-track, fetched on demand
  art.blob        # baked thumbnails, deduplicated per album
```

`index.sidx` sections (all **little-endian** — Cortex-M and x86 agree, no
byte-swap either side; versioned magic header for forward-compat):

1. **Header** — magic, format version, counts, byte-offsets of each section.
2. **Core record table** — fixed-size records (≈48–64 B): string refs
   `(heap_offset, len)` for title/artist/album/albumartist; track/disc u16;
   duration; sample_rate/channels; flags; `extended_offset`; `art_id`. Fixed
   size ⇒ `record[i]` is a direct seek, so random access is O(1) and binary
   search is O(log n) seeks.
3. **Order tables** — arrays of `u32` record-ids pre-sorted by the desktop for
   each browse axis (by artist→album→track, by album, by title). Browsing an
   axis = walk its array; jump-to-letter = binary search in it. No on-device
   sorting, no allocation.
4. **String heap** — UTF-8 blob; records point in by `(offset, len)`. The reader
   copies a value into a small fixed stack buffer on demand (same pattern as the
   tags crate's `scratch`).

**RAM story:** the reader holds the header + one page (a few KB) at a time.
Scrolling reads the next slice of an order table + the referenced records;
opening a detail view seeks into `extended.blob` and parses a tiny TLV list
(`field-tag, len, bytes`) into a caller buffer. Nothing scales with library
size. This is a new crate, mirroring the tags split:

- `soul-audio-index::write` — `alloc`/`std`, desktop-only (builds the sections).
- `soul-audio-index::read` — `no_std`, no-alloc, `forbid(unsafe_code)`, firmware.

### Cover art (dual strategy, from the discussion)

- **Default (desktop player / general reading):** art comes from the *original
  file* (embedded descriptor, or external `Front.jpg` fallback) — exactly what
  the app does today.
- **DAP import:** the desktop **bakes a small thumbnail** (e.g. ~128–256 px,
  small JPEG or RGB565) into `art.blob`; the core record's `art_id` points at
  it. No image decoding on the MCU.
- **Dedup by album** (and ideally by content hash): ~10k tracks ≈ hundreds of
  albums, so per-album thumbnails are a few MB total instead of hundreds of MB
  if baked per track. This is the single most important sizing lever.

## Part 3 — "Properly e2e test it"

- **Reader (Tier 1–3):** extend the `lofty` differential test to the new fields;
  per-dialect fixtures (ID3/Vorbis/MP4, + APEv2 if that scope is taken); real
  committed files. Same proven pattern as today.
- **Index round-trip (no hardware):** the highest-value new test — desktop
  `write` builds an index for a fixture library; the firmware `read` (no-alloc)
  reads it back over a `SliceSource` on the host; assert browse order per axis,
  field values across tiers, extended-on-demand fetch, and thumbnail bytes. This
  mirrors the existing render-to-file philosophy: exercise the embedded code path
  in CI without a board.
- **Firmware integration:** host-side tests of the reader over byte slices now;
  on-device validation deferred until a board exists (consistent with the
  current compile-only firmware status).

## Decomposition (if ever built)

1. **Metadata model expansion** in `soul-audio-tags` — tiers + readers + e2e.
   Self-contained; benefits the desktop player immediately; foundation for the
   rest. *(Optionally: +AIFF/Matroska/CAF/WavPack +APEv2 to reach decode parity.)*
2. **`soul-audio-index`** — new crate (`write` alloc + `read` no_std), the on-SD
   format, and the host round-trip e2e.
3. **Desktop importer + DAP browse UI** — folder prep, thumbnail baking, on-device
   browser. DAP UI validation waits on hardware.

Each is its own spec → plan cycle. #1 is the natural first slice; the index format
here is a *target* to validate against when #2 is taken up.

## Open questions / to verify before any of this is real

- **Enrichment precedence:** when the backend has better metadata than the file,
  who wins, and is the override stored as a Tier-3 overlay or merged at index
  build? (Deferred with the backend, but the index format should leave room.)
- **Duration doc-comment** in `soul-audio-service/src/library.rs` — confirm
  MP3/Opus `total_frames` now flows through, then delete the stale "regression".
- **Thumbnail format** — small JPEG (needs an MCU JPEG decoder path) vs a
  pre-decoded framebuffer format like RGB565 (zero decode, larger, tied to one
  screen size). Decide with the actual display.
- **Index freshness** — how the DAP detects a card changed outside the importer
  (mtime/generation counter in the header; graceful "needs re-index" state).
- **Field set** — is the Tier-2 list above the right one, or do we want
  more/less (e.g. mood, conductor, original-date)?
