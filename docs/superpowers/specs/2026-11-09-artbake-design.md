# soul-audio-artbake: 1-bit cover art for the e-ink DAP

Status: **design — approved, pre-implementation.**
Date: 2026-11-09
Home repo: `soul-audio-core`
Parent: [`2026-11-08-shared-catalog-index-architecture`](2026-11-08-shared-catalog-index-architecture.md) §6/§8 (decided: 1-bit grayscale, baker dithers/packs, deduped per album, `ArtStore` port).

## 1. Goal

Complete the art half of the `.soulidx` sidecar: the desktop bakes cover art into
a **1-bit grayscale `art.blob`** the e-ink DAP reads through the `ArtStore` port,
deduped per album. Originals stay full-resolution on the desktop (the desktop UI
keeps using them); `art.blob` is the DAP's card art. The reserved `art_id` fields
(currently always baked `0`) start carrying real ids.

Non-goals: changing the desktop's own art display (it keeps full-res via
`cover_bytes`); the DAP firmware ArtStore wiring (the reader lands; the board
wiring is later); multi-resolution / color art.

## 2. Crate: `soul-audio-artbake` (read/write split, mirrors `soul-audio-index`)

One crate owns the `art.blob` format with the same split the index uses for
`.soulidx`:

- **default (no_std, no-alloc, `forbid(unsafe)`, builds for `thumbv7em`):** the
  **reader** — `ArtBlobReader<S: SeekSource>` impl `ArtStore`: `art(art_id, buf)`
  reads the packed 1-bit bitmap for `art_id` into a caller buffer; `dimensions()`
  returns `(w, h)`. This is what the DAP links.
- **feature `build` (std):** the **writer** + the **encoder**. `image` (with just
  the `jpeg` + `png` + `webp` + `gif` + `bmp` features — the sidecar/ embedded
  formats §5 accepts) is a `build`-only dep, so the no_std/DAP surface never sees
  it. This is what the desktop baker links.

Gate: default builds for `thumbv7em-none-eabihf --no-default-features` (added to
`xtask verify`); `build` is std-only.

## 3. `art.blob` format

Little-endian, fixed header + entry table + packed bitmaps.

```
Header (16 B):
  0  magic   "SART"
  4  version u8  (= 1)
  5  flags   u8  (reserved, 0)
  6  width   u16 (thumbnail px, e.g. 200)
  8  height  u16
  10 count   u32 (number of art entries)
  14 _pad    u16
Entry table (count × 8 B), indexed by (art_id - 1):
  0  offset  u32 (absolute byte offset of the bitmap)
  4  len     u32 (= row_bytes * height, row_bytes = ceil(width/8))
Bitmaps: packed 1-bit, MSB-first, row-major; each row padded to a byte boundary.
  bit = 1 → black (ink), 0 → white. ~row_bytes*height per image (200×200 ≈ 5000 B).
```

**`art_id` semantics:** `0` = **no art** (the sentinel; records keep `0` when a
folder has no cover). `art_id ≥ 1` indexes entry `art_id - 1`. `art(0, _)` returns
0 bytes.

## 4. Encoder (artbake `build`)

`encode_cover(bytes: &[u8], w: u16, h: u16) -> Result<Vec<u8>, ArtError>`:

1. `image::load_from_memory(bytes)` — jpeg/png/webp.
2. `.into_luma8()` — grayscale.
3. Resize to `w × h` (`FilterType::Triangle`; covers are downscaled, so a cheap
   triangle filter is enough and deterministic).
4. **Floyd-Steinberg** dither to 1-bit: threshold 128, diffuse the quantization
   error 7/16 → right, 3/16 → below-left, 5/16 → below, 1/16 → below-right.
5. Pack MSB-first, rows byte-padded → `Vec<u8>`.

Target size is a **parameter** (default **200×200**) — the final panel size is a
knob, not baked into the format beyond the header's `width`/`height`.

`ArtBlobWriter`: accepts encoded bitmaps, **dedups by content hash** of the packed
bytes (identical covers → one entry → one `art_id`, shared across an album's
tracks; album = folder-isolated so this is one id per album), lays out the header
+ table + bitmaps, and emits the `art.blob` bytes.

## 5. Cover-art discovery → shared importer (`soul-audio-import`)

Port `-ng`'s discovery into the shared importer so `bake(folder)` finds art
end-to-end (shared 1:1 with any future DAP-side import):

- **Embedded** `PICTURE` / APIC (front cover) via `soul-audio-tags`.
- **Sidecar** files: names `cover / folder / front / album / artwork` ×
  `jpg / jpeg / png / webp / gif / bmp`; resolution order track folder → immediate
  parent → largest image; skip macOS `._*`. (The name/ext constants are the
  spec §7 set; carried from the monorepo.)
- One cover **source per album** (folder). The importer attaches it to the album.

## 6. Baker integration (`soul-audio-index` `build`)

- `CatalogInput` carries a **per-album cover source** (`Vec<Option<CoverSource>>`
  aligned to `albums`; `CoverSource` = owned bytes, from embedded or a sidecar read
  by the importer).
- The bake orchestration: for each album with a cover, call
  `artbake::encode_cover` → dedup via `ArtBlobWriter` → `art.blob`; assign the
  resulting `art_id` to the **album record and all its tracks**. Art is
  **best-effort**: if `encode_cover` errors (unsupported/corrupt image), that
  album simply stays `art_id 0` — a bad cover never fails the bake.
- `soul_audio_import::export(src, dst)` writes **both** `/.soulidx/index.sidx`
  **and** `/.soulidx/art.blob`.
- `soul-audio-index["build"]` gains a dep on `soul-audio-artbake["build"]`
  (std-only, so the no_std index reader is unaffected). An architecture-test
  confirms the no_std index reader does not pull artbake/image.

## 7. MultiCatalog: `art_id` / `extended_off` source-routing

The audit follow-up (the code comment marks the spot): now that `art_id` can be
non-zero, `track()`/`album()` must re-pack it with the source — **sentinel-guarded**
like `folder.parent`/`NO_FOLDER`: pack only when `art_id != 0` (and `extended_off
!= 0`). A unit test pins that `art_id 0` stays `0` and a non-zero `art_id` round-
trips its source. (An aggregate `ArtStore` over per-source `art.blob`s is deferred
— no consumer until a multi-source DAP/server reads art.)

## 8. Testing (host-side, no board)

- **Encoder:** a known synthetic image (gradient / checker) → deterministic 1-bit
  output; a solid-black and solid-white image → all-ink / all-blank; FS dither is
  reproducible run-to-run.
- **`art.blob` round-trip:** write 2 distinct covers → read each back by `art_id`
  over a `SliceSource`; bytes + `dimensions()` match; `art(0)` → 0 bytes.
- **Dedup:** two identical covers → one entry, same `art_id`.
- **Importer discovery:** a folder with a sidecar `cover.jpg` → found; a file with
  an embedded PICTURE → found; neither → `art_id` stays 0.
- **Baker end-to-end:** bake a fixture library with one covered album → the album
  + its tracks get the same non-zero `art_id`; `art.blob` reads back the bitmap;
  an uncovered album stays `art_id 0`.
- **MultiCatalog:** `art_id`/`extended_off` pack with source when non-zero, stay 0
  when zero.
- **Gates:** artbake default builds for `thumbv7em`; `forbid(unsafe)`; the index
  no_std reader does not depend on artbake/image (layering test).

## 9. Build order (for the implementation plan)

1. `soul-audio-artbake` crate: format reader (no_std) + `ArtStore` + the `build`
   writer + encoder + their tests (standalone; no baker/importer yet).
2. Importer cover discovery → `CatalogInput` per-album cover source.
3. Baker integration: encode + dedup + `art.blob` + `art_id` assignment; `export`
   writes `art.blob`; end-to-end test.
4. MultiCatalog `art_id`/`extended_off` source-routing + test.
5. `xtask verify`: add the artbake thumbv7em gate; layering test for the no_std
   boundary.

## 10. Lessons carried

- Read/write split in one crate (default no_std reader + `build` std writer),
  exactly as `soul-audio-index` does `.soulidx` — the DAP links the reader, the
  heavy `image` dep stays behind `build`.
- `art_id 0 = no art` is a sentinel, handled like `NO_FOLDER` in the MultiCatalog
  re-packing (the audit's lesson).
