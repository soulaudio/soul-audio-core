# Shared catalog + index architecture

Status: **design — committed; implementation is test-first (see §9).**
Date: 2026-11-08
Home repo: `soul-audio-core`
Master: [`ARCHITECTURE.md`](../../ARCHITECTURE.md) — this spec is the depth behind
its §2 decisions and §3 tiers. See also
[`dap-export-and-blobs`](2026-11-08-dap-export-and-blobs.md) and
[`sources-sync-and-server`](2026-11-08-sources-sync-and-server.md).

## 1. Goal

One set of music-domain logic — the **catalog** (tracks/albums/artists/folders/
playlists), its **metadata model**, and the on-disk **index format** — shared
**1:1** between the desktop app and the e-ink DAP (STM32H743, Embassy). Only
**navigation and rendering** are per-platform. Nothing that can be shared is
duplicated; nothing platform-specific leaks into the shared layer.

Non-goals / deferred (but their seams are reserved, see §7): multi-user, a
desktop SQLite store, an enrichment backend, managed-library import, full-text
search, playlist file export.

## 2. The organizing principle: the constraint is the arbiter

We do **not** decide "shared vs platform" by taste. We let hard build
constraints decide, and we encode them as tests that run in CI:

> If code compiles under **`no_std` + no-alloc + `forbid(unsafe_code)` +
> `thumbv7em-none-eabihf`**, it belongs in the shared layer. If it cannot, it is
> *by definition* platform-specific and lives in an adapter.

The target triple is the judge. This is why the architecture tests are written
**first** (§9): they are the specification of the boundaries, and implementation
is pulled toward green.

## 3. Architecture: hexagonal (ports & adapters)

The shared no_std core defines the **model**, the **ports** (traits it needs),
and the **format**. It never imports an implementation. Platforms supply
**adapters**; they are swappable without touching the core or each other.

```
      ┌──────── shared no_std core: model + ports + format ────────┐
      │ model:  Track / Album / Artist / Folder / Playlist (Tier-1) │
      │ ports:  trait Catalog       (pull/query, seek-based)         │
      │         trait ArtStore      (art bytes for an id)            │
      │ format: .soulidx codec  (read = no-alloc; write = `std`)     │
      └──────────────────────────────────────────────────────────────┘
          ▲ implements                         ▲ implements
   Desktop adapters (std)              DAP adapters (no_std, e-ink)
   • import: scan + hash + bake         • SoulidxCatalog (reads .soulidx, no-alloc)
   • InMemoryCatalog / browse           • 1-bit art.blob reader
   • keeps HIGH-RES original art         • (art baking is a desktop step)
   • LATER: Sqlite / search / sync       • firmware I/O (SD source, I²S sink)
     — all just NEW adapters
```

The playback runtime (`soul-audio-control::Player` + `soul-audio-queue`) is
already shared no_std and sync; this adds the **data/browse** half with the same
discipline.

### Why this gives extensibility for free

Every deferred capability is a **new adapter behind an existing port**, added
later, touching neither the shared core nor the DAP:

- Multi-user / sync → a `Catalog` adapter (or decorator) + extra fields in a
  *desktop* store. Shared model stays single-user-shaped; ids are stable.
- Desktop SQLite / search index → an alternate `Catalog` adapter, desktop-only.
- Enrichment backend → a source that writes Tier-2/Tier-3 overlays (§6) the
  baker merges; readers are unaffected.

## 4. Vertical crates (all in `soul-audio-core`), each with its own hard gate

| Crate | Hard gate (CI) | Owns |
| --- | --- | --- |
| `soul-audio-catalog` | `no_std` + `alloc`; normal deps ⊆ {core, embedded-io} | domain model + ports + grouping / natural-sort / folder-isolation logic |
| `soul-audio-index` *(default = read)* | `no_std`, **no-alloc**, `forbid(unsafe)`, builds for `thumbv7em` | `.soulidx` **reader**: seek records, walk baked order tables, copy strings to caller buffers, playlist section, TLV-on-demand, art bytes |
| `soul-audio-index` *(feature `build`)* | `std`/`alloc` | the **baker**: lay out records, pre-sort order tables, string heap, extended + art blobs. Mirrors `soul-audio-tags`' read/write split. |
| `soul-audio-import` | `std` | walk `std::fs`, read tags, hash, incremental scan, produce catalog → drive the baker |
| desktop glue (existing app) | `std` | actor, cpal, webview DTOs |

The **fork dies**: today's two `soul-audio-service` copies (core's + the
`soul-player-ng` fork) and `-ng`'s forked architecture test collapse into these
crates; `-ng` consumes them. The MP1/MP2 tests added to `-ng`'s `library.rs`
move into `import`/`catalog`.

## 5. Domain model (minimal, Tier-1)

Integer ids only — **not** UUID strings (the monorepo's INTEGER-vs-TEXT id
mismatch is a cautionary tale). The canonical id of a record is its position in
the index record table.

- `Track { id, title, artist, album, album_artist, track_no, disc_no, duration,
  sample_rate, channels, folder, art_id, flags, extended }` — string fields are
  heap refs `(offset,len)` in the index; owned `&str`/`String` only on the
  desktop in-RAM model.
- `Album { id, title, album_artist, year, folder, art_id }`
- `Artist { id, name, sort_name }`
- `Folder { id, path, parent }`
- `Playlist { id, name, tracks: [TrackId] }` — stored **in the index** (§6), no
  external file for v1.

**Folder isolation (carried from the monorepo):** album identity is keyed by its
parent folder — same title+artist in two folders = two albums, never merged.

## 6. The `.soulidx` on-SD format (the shared contract)

A sidecar directory at the library/SD root, little-endian, versioned magic
header. Desktop bakes it; the DAP (and desktop browse) read it.

```
/.soulidx/
  index.sidx     # header · core record table · order tables · playlist table · string heap
  extended.blob  # Tier-2/Tier-3 per-track TLV, fetched on demand
  art.blob       # baked art, deduplicated per album
```

- **Core record table** — fixed-width records (~48–64 B) → `record[i]` is a
  direct O(1) seek, O(log n) binary search. Holds Tier-1 fields + `art_id` +
  `extended_offset`.
- **Order tables** — `u32` record-id arrays, **pre-sorted by the desktop** per
  browse axis (by album, by artist→album→track, by title, by folder). Browsing =
  walk the array; jump-to-letter = binary search. **No on-device sort, no
  alloc.** This is how "same browse on both devices" is guaranteed: the desktop
  computes the order once; both sides read the frozen answer.
- **Playlist table** — playlists live here (decision §8): name heap-ref + a run
  of `u32` track-ids. No external playlist file for v1.
- **String heap** — UTF-8 blob; records index in by `(offset,len)`; the no-alloc
  reader copies into a small fixed stack buffer on demand.
- **Metadata tiers:** Tier-1 Core (the ~11 browse fields, in the fixed record),
  Tier-2 Extended (ReplayGain / Bpm / Lyrics / Publisher / ISRC / MusicBrainz
  ids … as a TLV list in `extended.blob`, parsed into a caller buffer only when a
  detail view opens), Tier-3 raw passthrough `(raw_key, value)`.
- **Art (decision §8): 1-bit grayscale for the e-ink panel.** Originals are kept
  full-resolution on the desktop and read via the `ArtStore` port; the **baker**
  dithers/packs a 1-bit grayscale thumbnail into `art.blob`, deduped per album.
  The baker is a desktop step; the *seam* (art referenced by `art_id`, bytes via
  an adapter) exists now, the 1-bit encoder is filled when the panel is final.
- **Freshness:** a generation counter in the header + per-track `mtime` (and
  content hash on the desktop) so the DAP can detect a card edited outside the
  importer and show a "needs re-index" state. (Exact scheme pinned during impl.)

## 7. Directory format (v1)

- **Watched folders**, source files never modified (from the monorepo's model;
  "managed library" deferred). Any tree allowed; album = parent folder.
- **Cover-art discovery** (from the monorepo, verbatim): names
  `cover / folder / front / album / artwork` × `jpg/jpeg/png/webp/gif/bmp`;
  resolution order = track folder → immediate parent → largest image (skip
  macOS `._*`). Also honour `-ng`'s `Scans/Front-*.jpg` sibling convention.
- **Index location:** `/.soulidx/` at the library/SD root (desktop bakes onto the
  card; desktop may also cache its own copy). Not a per-track sidecar.

## 8. Locked decisions

1. **No SQLite in the shared layer.** Re-enters later only as an optional
   desktop `Catalog` adapter — zero impact on core or the DAP.
2. **Playlists live in the binary index** (`.soulidx` playlist table), not an
   external `.m3u8` file, for v1.
3. **E-ink art is baked to 1-bit grayscale**; originals kept full-res on desktop.
4. **Home is `soul-audio-core`**, heavily vertically sliced (§4), architecture-
   tested.
5. **The `Catalog` port is pull/query**, seek-based (`album(i)`,
   `tracks_of(a)`, `folder_children(f)`) — it must never hand back owned
   collections, so it survives the DAP's no-alloc gate and paged SD reads.
6. **Multi-user deferred**, model stays single-user-shaped but id-stable so it
   can be layered on later.

## 9. Test-first plan (architecture tests before any implementation)

Written and **passing on empty skeletons** before logic exists:

**A. Constraint gates (the "judge"):**
- `soul-audio-catalog`, `soul-audio-index` (default) **build for
  `thumbv7em-none-eabihf --no-default-features`** (xtask `check-embedded` gate).
- No-alloc runtime probe (counting `GlobalAlloc`, per `architecture-tests/
  no_alloc.rs`) around the index **read** path.
- `forbid(unsafe_code)` on catalog + index-read; workspace lints.

**B. Layering (`architecture-tests`, via `cargo_metadata`):**
- `soul-audio-catalog` normal deps ⊆ {`soul-audio-core`, `embedded-io`}.
- `soul-audio-index` (read) does **not** depend on `std`/`import`.
- The DAP crate depends on `soul-audio-index` (read) but **never** on
  `soul-audio-import` or the `build` feature.
- Ports live in `catalog`; adapters never appear in core's or catalog's normal
  deps.

**C. Contract (the real TDD driver):** a **host-side `.soulidx` round-trip** —
the `build` feature writes an index from a fixture catalog; the no-alloc reader
consumes it over a `SliceSource`; asserts per-axis browse order, Tier-1/2/3
field values, extended-on-demand, playlist round-trip, and art bytes. **No board
needed.** This test is written red first; implementation makes it green.

**Build order:** arch tests + skeletons (B, A) → `.soulidx` codec to pass the
round-trip (C) → catalog model + pull/query ports → desktop `import`/bake → DAP
`SoulidxCatalog` reader → wire `-ng` to consume, retire the fork.

## 10. Lessons carried / avoided (from the monorepo)

- **Carry:** folder-isolation, `mtime`+content-hash freshness (`scanned_
  directories` pattern), cover-art name/ext constants, the `{AlbumArtist}/{Year}
  - {Album}/{TrackNo} - {Title}` template (for a future managed mode).
- **Avoid:** UUID-string ids (use compact integer record ids); carrying SQLite
  or multi-user schema into the shared/embedded layer.
