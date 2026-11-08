# DAP export (`.soulidx` on-card) + the blobs

Status: **design — committed; implement test-first.**
Date: 2026-11-08
Home: `soul-audio-core`, branch `feat/shared-catalog-architecture`
Master: [`ARCHITECTURE.md`](../../ARCHITECTURE.md). Builds on
[`shared-catalog-index-architecture`](2026-11-08-shared-catalog-index-architecture.md);
see also [`sources-sync-and-server`](2026-11-08-sources-sync-and-server.md).

## 1. Scope & model

We have the **import** half (`scan` + `index.sidx` bytes) and the **reader**.
Missing is **export**: writing a complete `/.soulidx/` onto a card the DAP reads,
the two extra blobs (`art.blob`, `extended.blob`), the **track→file location**
the DAP needs to actually play, and **freshness**.

**v1 model = "index the card" (locked).** The SD card is the source of truth:
the user copies audio onto it, inserts it in the desktop, and the desktop
**scans the card and writes `/.soulidx/` onto that same card**. No file copying,
no path templating, no library management. ("Sync to device" — desktop owns a
library and pushes/organizes files — is a later managed-library adapter and is
explicitly deferred.)

## 2. On-card layout (the directory format)

```
<card root>/
  …any audio tree the user made…      (untouched; album = parent folder)
  .soulidx/
    index.sidx       # header + records + order tables + album→tracks
                     #   + playlists + FOLDER TABLE + string heap
    extended.blob    # Tier-2/3 per-track TLV, read on demand
    art.blob         # baked 1-bit e-ink thumbnails, deduped per album
```

`index.sidx` offsets are **relative to that card root** via the folder table
(§4). Audio stays exactly where the user put it.

## 3. The missing piece: track → file location (REQUIRED for playback)

Today a `TrackRec` has a `folder: FolderId` (a grouping id) but **no path**, so
the DAP cannot open the file. Export must bake, and the reader must expose, the
physical location:

- **Folder table** (new `index.sidx` section): `FolderRec { path: StrRef, parent:
  FolderId }` where `path` is the folder's path **relative to the card root**
  (e.g. `Music/Bach/Goldberg`). This doubles as browse-by-folder (shared) and the
  playback locator.
- **Track filename**: add `TrackRec.file_name: StrRef` (the leaf, e.g.
  `01 - Aria.flac`). The DAP opens `card_root / folder.path / track.file_name`.
- **New ports** (`soul-audio-catalog`, no-alloc, buffer-based):
  `folder(id) -> FolderRec`, `folder_count()`, `folder_path(id, buf)` and the
  track's file name resolves via `resolve_str(track.file_name, buf)`.

This makes `.soulidx` self-sufficient: browse **and** play, no second lookup.

## 4. `art.blob` — 1-bit e-ink thumbnails (decision §8)

- **Discovery** (desktop, std): embedded art via `soul-audio-tags`
  (`read_picture_bytes`) first, else the sidecar files (`cover/folder/front/
  album/artwork` × image exts) next to the track/album folder.
- **Bake** (desktop, std): decode (JPEG/PNG/…) → downscale to the panel size →
  **1-bit dither** (Floyd–Steinberg or ordered) → pack 1 bpp. Deduped **per
  album** (and ideally per source-content hash — the single biggest size lever).
- **`art.blob` format** (little-endian): header `{ magic, version, count }` +
  a directory of `ArtEntry { off: u32, len: u32, w: u16, h: u16 }` + packed
  1-bpp bitmaps (row-major, MSB-first, rows byte-aligned). `TrackRec.art_id` /
  `AlbumRec.art_id` index the directory; `0` = none.
- **Panel size is a build parameter**, not hard-coded (e.g. a `const` or a
  `BakeConfig { art_w, art_h }`), so the exact e-ink resolution is filled in when
  the panel is final without touching the format.
- **Reader**: implement `ArtStore::art(art_id, buf)` over `art.blob` (no-alloc:
  seek the directory entry, copy the 1-bpp bytes into the caller buffer).

## 5. `extended.blob` — Tier-2/3 metadata (TLV)

- Per track, a **TLV list**: `(tag: u8, len: u16, bytes)` repeated, pointed to by
  `TrackRec.extended_off` (0 = none). Tags enumerate Tier-2 fields (ReplayGain
  track/album gain+peak, Bpm, Lyrics, Publisher, Copyright, Grouping, Isrc,
  MusicBrainz recording/release/artist ids) and a Tier-3 raw passthrough
  `(raw_key_strref, value_strref)`.
- **Reader**: an on-demand iterator `extended(track_id)` that parses one TLV into
  a caller buffer when a detail view opens — no alloc, not touched during browse.

## 6. Freshness

- **Header gains**: `generation: u32` (bumped each full bake) and
  `source_root_hash` (cheap hash of the tree shape — dir names + mtimes).
- **Per track**: `mtime: u32` (already planned) so a changed file is detectable.
- **v1 = full re-bake** (simplest; matches "index the card"). The DAP, on mount,
  compares a quick card signature to the header; mismatch → **"needs re-index"**
  state (re-insert in desktop), never an on-device scan.
- **Incremental re-bake** (only changed dirs, via a `scanned_directories`-style
  mtime cache, carried from the monorepo) is **deferred** — the fields above
  reserve the seam.

## 7. Crate / module shape (vertical slices, each gated)

| Crate | Gate | Role |
| --- | --- | --- |
| `soul-audio-index` (read) | no_std/no-alloc/`thumbv7em` | read `index.sidx` + `extended.blob` + `art.blob`; `ArtStore`, folder ports |
| `soul-audio-index` (`build`) | std | write all three byte sections |
| `soul-audio-artbake` *(new)* | std | decode + downscale + 1-bit dither → art bytes (keeps the `image` dep isolated) |
| `soul-audio-import` (std) | std | scan the card → assemble full input (incl. file names, folder paths, discovered art) → call artbake + index::build → **write `/.soulidx/` to the card root** |

`soul-audio-import` gains `export(card_root, &BakeConfig) -> io::Result<()>` that
writes the three files. The reader stays no-alloc; `artbake`'s `image`/dither dep
never reaches the DAP (layering test enforces: firmware deps exclude `import`,
`artbake`, and the `build` feature).

## 8. Test-first plan

**Arch/constraint gates (unchanged discipline):** `thumbv7em` build of catalog +
index(read) still compiles with the new folder/art/extended read paths; no-alloc
probe extended to cover `folder`, `art`, and one `extended` TLV read; layering
adds `soul-audio-artbake` (std, above the shared layer, not a firmware dep).

**The new contract — on-DISK round-trip (not just a slice):**
1. Build a temp "card" dir with 2–3 tagged files in nested folders + a
   `cover.jpg`.
2. `import::export(card, &cfg)` writes `card/.soulidx/`.
3. Open `card/.soulidx/index.sidx` through a **`std::fs::File`-backed
   `SeekSource`** (via `embedded-io-adapters`) — the real on-disk path, closest
   to the DAP's SD read — and assert: track fields, **folder path + file name
   reconstruct the real file path**, browse order, an `extended` TLV value, and
   that `ArtStore::art` returns a 1-bpp bitmap of the configured size.
4. Independently open `art.blob`/`extended.blob` and assert their headers.

This proves the **whole desktop→card→DAP loop** on the host with no board.

## 9. Locked decisions / deferred

**Locked:** index-the-card for v1; audio files untouched; track locatable via
folder-path + file-name; 1-bit e-ink `art.blob`, panel size a build param; full
re-bake with a generation counter; blobs in `/.soulidx/`.

**Deferred (seams reserved):** sync-to-device / managed-library; incremental
re-bake; enrichment-backend-sourced extended data; multi-user. All are new
adapters/fields, not changes to the reader or the shared model.
