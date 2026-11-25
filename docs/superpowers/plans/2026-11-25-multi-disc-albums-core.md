# Multi-Disc Albums + Side Files — Phase 1 (soul-audio-core) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `soul-audio-core` treat a multi-disc album as ONE album (flat or `CD1/CD2` sub-folders), read the disc metadata correctly (`n/total`, totals, disc subtitle, MusicBrainz release id), bake it into `.soulidx` **v8**, and make the organizer album-aware (disc-safe names, `+disc-folders` layouts, side files that travel with the album, empty-dir cleanup on Move) — so the `-ng` player (Phase 2) can group albums by id and draw disc headers.

**Architecture:** A tiny no_std/no-alloc `soul_audio_tags::disc_folder::parse` matcher is the single disc-folder rule shared by the importer and the organizer (organize must not depend on import). Tags gain `DiscSubtitle`/`MusicBrainzAlbumId` fields and a shared `n/total` splitter. The importer keys albums on `(album-root folder, title, MB release id)` and computes `disc_count`; the `.soulidx` track/album records grow append-only (85→91 B, 30→38 B, format 7→8). The organizer groups inputs into albums by the same rule, renders new disc tokens, plans side files, and prunes emptied source folders after a Move.

**Tech Stack:** Rust 2024 (MSRV 1.85), `no_std` + no-alloc kernel crates (`soul-audio-tags`, `soul-audio-catalog`, `soul-audio-index` reader), std adapters (`soul-audio-import`, `soul-audio-organize`, `soul-audio-multicatalog`), hand-rolled tag parsers, `cargo xtask ci` gate.

**Spec:** `D:\dev\soulaudio\soul-player-ng\docs\superpowers\specs\2026-11-25-multi-disc-albums-design.md` (approved). Phase 2 (`soul-player-ng`) is a separate plan.

## Spec corrections (spec vs. the code at `adcfd33`)

1. **Vorbis `TRACKTOTAL`/`TOTALTRACKS` are already mapped** (`vorbis.rs:31`). Only `DISCTOTAL`/`TOTALDISCS` are new for Vorbis. **APE maps no disc key at all** (`wavpack.rs:126-145` has no `Disc`, no totals) — this plan adds APE `Disc`, `DISCTOTAL`/`TOTALDISCS`, `TRACKTOTAL`/`TOTALTRACKS`.
2. **The baker's `intern()` does not dedupe, and an empty string yields `(heap.len(), 0)`, not `(0, 0)`** (`soul-audio-index/src/lib.rs:674`). "A 30-track disc costs one string" needs a new deduping `intern_shared` (used for the two v8 refs only; empty → canonical `(0, 0)`).
3. **`docs/ARCHITECTURE.md` has no byte-layout "format section".** The canonical layout is the `soul-audio-index` module doc (`lib.rs:12-89`). Per §10 the doc change comes first, so Task 1 edits ARCHITECTURE §2.3/§7 and the shared-catalog spec §5 (which still says `Album { …, year, … }` — stale since v5); the module-doc layout is updated with the code in Task 4.
4. **`testfix` cannot host the organize fixture writers**: it is `#[cfg(test)] pub(crate)` inside `soul-audio-tags` and unreachable from other crates. The minimal FLAC (STREAMINFO + VORBIS_COMMENT) and ID3v2.3-MP3 writers live in `crates/soul-audio-organize/tests/support/mod.rs`.
5. **The organizer never writes tags**, so a disc number *inferred from a disc folder* survives an organize run only into a `+disc-folders` layout. The organize∘import round-trip therefore asserts the **album count always**, and each album's **disc count** except for untagged-disc albums organized into a one-folder or `flat` layout (they re-import as `disc_count = 1`). Documented limitation, asserted explicitly.
6. **`plan()` has no scan-root parameter**, but "never above the scan root" needs one. Added `PlanOptions { source_root }` + `plan_with(…)`; `plan(…)` keeps its signature (cleanup then stops at each album root, inclusive). An album whose root *is* the `source_root` (loose tracks in the staging root) leaves its side files in place — otherwise every non-audio file in the staging folder would be swept into that album.
7. **Move cleanup cannot remove a folder that still holds junk** (`Thumbs.db`, `desktop.ini`, hidden files): junk is never moved and never deleted ("never a non-empty one").
8. **Ambiguity rule refined:** an album root is ambiguous when another album shares it **or another album's root is nested inside it** (an `Artist/` folder with loose tracks *and* `Artist/Album/`) — otherwise the outer album would sweep the inner album's folder.
9. **Pre-existing bug fixed in passing:** `MultiCatalog::track()` re-packs `title`/`file_name` offsets but **not `mbid.off`** (`soul-audio-multicatalog/src/lib.rs:356-376`); a remote track's MBID resolves against source 0. Task 6 re-packs `mbid`, `disc_subtitle`, `mb_album_id`.
10. **ID3 `TXXX` values are ASCII-collapsed** (`fill_ascii`, `id3v2.rs:413`) — fine for the MB release UUID, lossy for CJK text, so the ID3 disc subtitle is read from `TSST` only (not `TXXX:DISCSUBTITLE`).
11. **`\b` in the disc-folder regex is implemented as an ASCII word boundary** (`[A-Za-z0-9_]`), so `Disc 1中文` matches with subtitle `中文` (Unicode `\b` would reject it). Deliberate: the real corpus has CJK disc subtitles.
12. The organize e2e "mixed" layout is defined concretely as: one flat-tagged multi-disc album + one album in **untagged** `Disc N - Subtitle/` folders + a single-disc MP3 album, all in one source tree.

## Global Constraints

- **Kernel discipline:** `disc_folder` (tags), `TrackRec`/`AlbumRec` (catalog) and the index reader stay `no_std`, **no-alloc**, `forbid(unsafe_code)`; they cross-build for `thumbv7em-none-eabihf` (xtask gate) and pass the counting-allocator probes.
- **Disc-folder rule (verbatim):** `^(cd|disc|disk|dvd|side)[\s._-]*(\d{1,3}|[a-d])\b.*$`, case-insensitive; `a..d` → `1..4`; the remainder (trimmed of separators `\s . _ - : ( ) [ ]`) is the subtitle fallback. API: `soul_audio_tags::disc_folder::parse(name: &str) -> Option<DiscFolder<'_>>`, `DiscFolder { number: u16, subtitle: Option<&str> }`.
- **Album root:** a track's parent folder, unless that folder's name parses as a disc folder **and** the folder is strictly below the scan/source root — then the grandparent. Never climb to or above the scan root.
- **Album identity key:** `(album_root_folder, album_title, musicbrainz_album_id or "")`. Album artist is **not** in the key.
- **`disc_count = max(max disc_total tag, max disc_no seen, 1)`.** Disc number fallback: tag (if > 0) → disc-folder number → 1. Disc subtitle: tag (trimmed, non-empty) → disc-folder subtitle → `""`.
- **`.soulidx` v8:** `FORMAT_VERSION: u16 = 8` (exact-match reader). **TRACK RECORD 91 B** = v7's 85 B + `85 disc_subtitle.off u32 · 89 disc_subtitle.len u16`. **ALBUM RECORD 38 B** = v7's 30 B + `30 disc_count u16 · 32 mb_album_id.off u32 · 36 mb_album_id.len u16`. Append-only: every v7 offset is unchanged. Empty string = `StrRef { off: 0, len: 0 }`. The baker writes `disc_count.max(1)`.
- **`SCAN_CACHE_VERSION: u32 = 4`** (was 3).
- **Template tokens:** `{DiscPrefix}` = `""` when `disc_count == 1`, else `disc` zero-padded to the digit width of `disc_count` + `-` (`1-`, `01-`); `{DiscDir}` = `""` when single-disc, else `Disc N` (same padding); `{DiscTotal}` = `disc_count`; `{Disc}` unchanged. A **non-final** template segment that renders empty/whitespace is **dropped** (no `_`); a final (file-name) segment still sanitizes to `_`.
- **Presets (exact, in this order):**
  `artist-album` `{AlbumArtist}/{Album}/{DiscPrefix}{TrackNo} - {Title}` ·
  `artist-album+disc-folders` `{AlbumArtist}/{Album}/{DiscDir}/{TrackNo} - {Title}` ·
  `artist-year-album` `{AlbumArtist}/{Year} - {Album}/{DiscPrefix}{TrackNo} - {Title}` ·
  `artist-year-album+disc-folders` `{AlbumArtist}/{Year} - {Album}/{DiscDir}/{TrackNo} - {Title}` ·
  `genre-artist-album` `{Genre}/{AlbumArtist}/{Album}/{DiscPrefix}{TrackNo} - {Title}` ·
  `genre-artist-album+disc-folders` `{Genre}/{AlbumArtist}/{Album}/{DiscDir}/{TrackNo} - {Title}` ·
  `flat` `{Artist} - {Title}` (unchanged). Single-disc output of the non-flat presets is **byte-identical** to the old presets.
- **Side files:** every non-audio, non-junk file under the album root (recursive, max depth 16, symlinks not followed). Junk = name starts with `.`, or `Thumbs.db` / `desktop.ini` (case-insensitive). Side files are **never modified** (copy/rename only). `.cue` `FILE` lines are not rewritten (documented limitation).
- **Organize must not depend on import** (normal deps). A dev-dependency for the round-trip test is allowed (`tiers.rs`/`layering.rs` only inspect normal deps).
- **Commit messages:** Conventional Commits; **no** Claude/Anthropic/AI/Co-Authored-By/"Generated with" text and no standalone token `ai` (the `commit-msg` hook rejects them).
- **Verify** with `cargo xtask ci` from the worktree root before the PR. Do not run two core `cargo` builds at once (shared `~/.cargo` lock). Tags tests that touch the `alloc` layer need `--features std` (`owned.rs` and the `util` alloc tests are feature-gated).
- Work in `D:\dev\soulaudio\core-discs` on branch `feat/multi-disc-albums`.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `docs/ARCHITECTURE.md` | Committed architecture | §2.3 format v8 + album-root identity, §7 new invariant row (T1) |
| `docs/superpowers/specs/2026-11-08-shared-catalog-index-architecture.md` | Shared-layer spec | §5 domain model + folder-isolation wording (T1) |
| `crates/soul-audio-tags/src/disc_folder.rs` | **New** no_std disc-folder matcher | T1 |
| `crates/soul-audio-tags/src/lib.rs` | `Field` enum, module list | `pub mod disc_folder` (T1); `DiscSubtitle`, `MusicBrainzAlbumId` (T3) |
| `crates/soul-audio-tags/src/util.rs` | Shared key maps + parsers | `split_num_total`, `emit_text`, `parse_count` (T2); `map_disc_subtitle_key`, `map_musicbrainz_album_key` (T3) |
| `crates/soul-audio-tags/src/{vorbis,id3v2,wavpack,mp4}.rs` | Container mappers | n/total split + totals (T2); new fields (T3) |
| `crates/soul-audio-tags/src/owned.rs` | Owned `Tags` | robust count parse (T2); two new fields (T3) |
| `crates/soul-audio-tags/tests/no_alloc.rs` | Alloc probe | thread-local counter + `disc_folder::parse` probe (T1) |
| `crates/soul-audio-catalog/src/lib.rs` | `TrackRec`/`AlbumRec` | v8 fields (T4) |
| `crates/soul-audio-index/src/lib.rs` | `.soulidx` doc/reader/baker | v8 layout (T4) |
| `crates/soul-audio-index/tests/{roundtrip.rs,reader_no_alloc.rs}` + `Cargo.toml` | Format contract | v8 tests; **new** real-bytes no-alloc probe (T4) |
| `crates/soul-audio-index/benches/reader.rs` | Reader bench | literal update (T4) |
| `crates/architecture-tests/tests/no_alloc.rs` | Arch probe | header bytes → v8 (T4) |
| `crates/soul-audio-import/src/lib.rs` | Scan → catalog | album root, key, disc inference, subtitle, `disc_count`, MB id, cache v4 (T5) |
| `crates/soul-audio-import/tests/pipeline.rs` | Pipeline e2e | 8 new multi-disc tests (T5) |
| `crates/soul-audio-multicatalog/src/lib.rs` + `tests/multicatalog.rs` | Merge | re-pack v8 refs + `mbid` (T6) |
| `crates/soul-audio-organize/src/template.rs` | Renderer + presets | disc tokens, empty-segment drop, presets, `preset()` (T7) |
| `crates/soul-audio-organize/src/album.rs` | **New** album grouping | T8 |
| `crates/soul-audio-organize/src/side.rs` | **New** side-file discovery/placement | T8 |
| `crates/soul-audio-organize/src/plan.rs` | Planner | album-aware, side files, prune list, `plan_with` (T8) |
| `crates/soul-audio-organize/src/apply.rs` | Executor | prune empty dirs after Move (T9) |
| `crates/soul-audio-organize/src/lib.rs` | Exports + docs | T7/T8 |
| `crates/soul-audio-organize/Cargo.toml` | Deps | dev-dep `soul-audio-import` (T10) |
| `crates/soul-audio-organize/tests/support/mod.rs` | **New** fixture writers (FLAC/MP3), tree helpers | T8 |
| `crates/soul-audio-organize/tests/side_files.rs` | **New** planner/apply integration tests | T8/T9 |
| `crates/soul-audio-organize/tests/multi_disc_e2e.rs` | **New** e2e matrix + organize∘import | T10 |

---

## Task 1: Architecture docs + no_std disc-folder matcher

**Files:**
- Modify: `docs/ARCHITECTURE.md` (§2 item 3 at `:34-39`, §7 table at `:131-139`)
- Modify: `docs/superpowers/specs/2026-11-08-shared-catalog-index-architecture.md` (§5 at `:94-104`)
- Create: `crates/soul-audio-tags/src/disc_folder.rs`
- Modify: `crates/soul-audio-tags/src/lib.rs:18-37` (module list)
- Modify: `crates/soul-audio-tags/tests/no_alloc.rs`

**Interfaces:**
- Produces: `pub mod soul_audio_tags::disc_folder` with
  `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct DiscFolder<'a> { pub number: u16, pub subtitle: Option<&'a str> }` and
  `pub fn parse(name: &str) -> Option<DiscFolder<'_>>`. Used by T5 (import) and T8 (organize).

- [ ] **Step 1: Update the architecture docs first (change process §10)**

In `docs/ARCHITECTURE.md` §2, replace item 3 (`:34-39`) with:

```markdown
3. **One on-disk format, `.soulidx`**, for desktop cache, DAP SD card, and
   server. Fixed-width records → O(1) seek; desktop-baked pre-sorted order
   tables → zero on-device sort; string heap; in-index playlists; `extended.blob`
   (Tier-2/3) and `art.blob` (1-bit e-ink). The current format is **v8**; the
   canonical byte layout is the `soul-audio-index` module doc
   (`crates/soul-audio-index/src/lib.rs`), and the reader is exact-match on the
   version (any other version reads as an empty catalog; the desktop re-bakes,
   a remote server and its clients must match). Records only ever grow by
   **appending** fields so existing offsets never move (v8 appended a per-track
   `disc_subtitle` and a per-album `disc_count` + MusicBrainz release id).
   **Album identity** is `(album root folder, title, MusicBrainz release id)`:
   the album root is a track's folder, or its parent when that folder is a disc
   folder (`CD1`, `Disc 2 - Liyue`, …) below the scan root — one rule,
   `soul_audio_tags::disc_folder`, shared by the importer and the organizer.
   Spec:
   [`shared-catalog-index-architecture`](superpowers/specs/2026-11-08-shared-catalog-index-architecture.md),
   [`dap-export-and-blobs`](superpowers/specs/2026-11-08-dap-export-and-blobs.md);
   multi-disc albums: `soul-player-ng/docs/superpowers/specs/2026-11-25-multi-disc-albums-design.md`.
```

In §7's table, add a row after the `Desktop pipeline (scan → bake → read)` row:

```markdown
| Organize and import agree on album identity (organize ∘ import round-trip) | `soul-audio-organize/tests/multi_disc_e2e.rs` |
```

In the shared-catalog spec §5, replace the `Track { … }` / `Album { … }` bullets and the **Folder isolation** paragraph (`:94-104`) with:

```markdown
- `Track { id, title, artist, album, album_artist, genre, track_no, disc_no,
  disc_subtitle, duration, sample_rate, channels, folder, art_id, flags,
  extended, file_name, mbid, loudness }` — string fields are heap refs
  `(offset,len)` in the index; owned `&str`/`String` only on the desktop in-RAM
  model. (`disc_subtitle` new in format v8.)
- `Album { id, title, album_artist, original_date, release_date, folder, art_id,
  disc_count, mb_album_id }` (`disc_count`/`mb_album_id` new in format v8;
  `folder` is the album ROOT folder)
- `Artist { id, name, sort_name }`
- `Folder { id, path, parent }`
- `Playlist { id, name, tracks: [TrackId] }` — stored **in the index** (§6), no
  external file for v1.

**Folder isolation (carried from the monorepo):** album identity is keyed by its
**album root folder** — a track's parent folder, or the grandparent when the
parent is a disc folder (`CD1`, `Disc 2 - Liyue`; `soul_audio_tags::disc_folder`)
— plus the title and the MusicBrainz release id. Same title+artist in two
folders = two albums, never merged; `Album/CD1` + `Album/CD2` = one album.
```

- [ ] **Step 2: Commit the docs**

```bash
git add docs/ARCHITECTURE.md docs/superpowers/specs/2026-11-08-shared-catalog-index-architecture.md
git commit -m "docs(arch): soulidx v8 and album-root identity for multi-disc albums"
```

- [ ] **Step 3: Write the failing table test**

Create `crates/soul-audio-tags/src/disc_folder.rs` containing only the test module for now (the module must exist to compile; Step 5 adds the code above it):

```rust
#[cfg(test)]
mod tests {
    use super::{DiscFolder, parse};

    fn df(number: u16, subtitle: Option<&str>) -> Option<DiscFolder<'_>> {
        Some(DiscFolder { number, subtitle })
    }

    #[test]
    fn matches_disc_folder_names() {
        assert_eq!(parse("CD1"), df(1, None));
        assert_eq!(parse("cd 01"), df(1, None));
        assert_eq!(parse("Disc 2 - Liyue"), df(2, Some("Liyue")));
        assert_eq!(parse("Disc02"), df(2, None));
        assert_eq!(parse("Disk 3"), df(3, None));
        assert_eq!(parse("DVD 1"), df(1, None));
        assert_eq!(parse("Side A"), df(1, None));
        assert_eq!(parse("Side B"), df(2, None));
        assert_eq!(parse("side d"), df(4, None));
        assert_eq!(parse("Disc 1 - Mondstadt"), df(1, Some("Mondstadt")));
        assert_eq!(parse("CD.2"), df(2, None));
        assert_eq!(parse("CD_3"), df(3, None));
        assert_eq!(parse("Disc 1 (Bonus)"), df(1, Some("Bonus")));
        assert_eq!(parse("DISC 12: Finale"), df(12, Some("Finale")));
        assert_eq!(parse("CD 100"), df(100, None));
        // ASCII word boundary: a CJK subtitle glued to the number still matches.
        assert_eq!(parse("Disc 1 中文"), df(1, Some("中文")));
    }

    #[test]
    fn rejects_non_disc_folders() {
        for name in [
            "Discography",
            "CDs",
            "Disc",
            "2CD",
            "CD Collection",
            "CD1234",
            "Album",
            "",
            "Sidewinder",
            "Disco 1",
            "Disc 2_Liyue", // `_` is a word char: no boundary after the number
            " CD1",         // anchored at the start: no leading whitespace
        ] {
            assert_eq!(parse(name), None, "{name:?} must not be a disc folder");
        }
    }
}
```

And register the module in `crates/soul-audio-tags/src/lib.rs`, after `mod dff;` (`:22`):

```rust
/// Disc-folder name matcher (`CD1`, `Disc 2 - Liyue`, `Side A`) — the single
/// rule the importer and the organizer share for "this folder is one disc of an
/// album". `no_std`, no-alloc.
pub mod disc_folder;
```

- [ ] **Step 4: Run it to verify it fails**

Run: `cargo test -p soul-audio-tags disc_folder`
Expected: FAIL to compile — `cannot find function parse` / `cannot find type DiscFolder in super`.

- [ ] **Step 5: Implement the matcher**

Prepend to `crates/soul-audio-tags/src/disc_folder.rs` (above the test module):

```rust
//! Disc-folder names: `CD1`, `cd 01`, `Disc 2 - Liyue`, `Disc02`, `Disk 3`,
//! `DVD 1`, `Side A`. The rule (case-insensitive, anchored):
//!
//! ```text
//! ^(cd|disc|disk|dvd|side)[\s._-]*(\d{1,3}|[a-d])\b.*$
//! ```
//!
//! `a..d` count as discs `1..4` (vinyl sides). Whatever follows the number,
//! trimmed of separators, is a **disc subtitle** fallback (`Disc 1 - Mondstadt`
//! → `Mondstadt`). `\b` is an ASCII word boundary (`[A-Za-z0-9_]`), so a CJK
//! subtitle glued to the number still matches. `no_std`, no-alloc: the
//! subtitle borrows from `name`.

/// A parsed disc-folder name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscFolder<'a> {
    /// The disc number (`Side A` → 1, `B` → 2, …).
    pub number: u16,
    /// The text after the number (`Disc 1 - Mondstadt` → `Mondstadt`), if any.
    pub subtitle: Option<&'a str>,
}

const PREFIXES: [&str; 5] = ["disc", "disk", "side", "dvd", "cd"];

/// `[\s._-]` — the separators allowed between the prefix and the number.
fn is_sep(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | '_' | '-')
}

/// An ASCII `\w` byte — what a `\b` after the number must NOT be followed by.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Parse a folder name as a disc folder. `None` when it is not one.
pub fn parse(name: &str) -> Option<DiscFolder<'_>> {
    let bytes = name.as_bytes();
    let plen = PREFIXES
        .iter()
        .find(|p| bytes.len() >= p.len() && bytes[..p.len()].eq_ignore_ascii_case(p.as_bytes()))?
        .len();
    // The matched prefix is ASCII, so `plen` is a char boundary.
    let rest = name[plen..].trim_start_matches(is_sep);
    let rb = rest.as_bytes();
    let (number, used) = if rb.first().is_some_and(u8::is_ascii_digit) {
        let n = rb.iter().take_while(|b| b.is_ascii_digit()).count();
        if n > 3 {
            return None; // \d{1,3} then \b can never match a 4+ digit run
        }
        let mut v: u16 = 0;
        for &b in &rb[..n] {
            v = v * 10 + u16::from(b - b'0');
        }
        (v, n)
    } else {
        match rb.first().map(u8::to_ascii_lowercase) {
            Some(c @ b'a'..=b'd') => (u16::from(c - b'a') + 1, 1),
            _ => return None,
        }
    };
    if rb.get(used).is_some_and(|&b| is_word_byte(b)) {
        return None; // no word boundary after the number/letter
    }
    let tail = rest[used..]
        .trim_matches(|c: char| is_sep(c) || matches!(c, ':' | '(' | ')' | '[' | ']'));
    Some(DiscFolder {
        number,
        subtitle: (!tail.is_empty()).then_some(tail),
    })
}
```

- [ ] **Step 6: Run the table test**

Run: `cargo test -p soul-audio-tags disc_folder`
Expected: PASS (2 tests).

- [ ] **Step 7: Add the no-alloc probe**

Replace `crates/soul-audio-tags/tests/no_alloc.rs` entirely (the global `AtomicUsize` counter is racy across libtest threads — the same flake `architecture-tests/no_alloc.rs` documents — so count per thread):

```rust
//! Assert `read` and `disc_folder::parse` allocate nothing.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use soul_audio_core::SliceSource;
use soul_audio_tags::{Field, Picture, StreamFacts, TagSink, disc_folder, read};

thread_local! {
    // Per-thread, const-initialised: libtest's other threads can't perturb it
    // and first access never allocates.
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get().wrapping_add(1)));
        // SAFETY: forwarding to the system allocator with the same layout.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: forwarding to the system allocator with the same ptr/layout.
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static GA: Counting = Counting;

struct Null;
impl TagSink for Null {
    fn text(&mut self, _f: Option<Field>, _k: &str, _v: &str) {}
    fn picture(&mut self, _p: &Picture) {}
}

#[test]
fn read_does_not_allocate() {
    let data = [0u8; 64]; // not a real container; read returns Unsupported without allocating
    let mut scratch = [0u8; 256];
    let mut sink = Null;
    let before = ALLOCS.with(Cell::get);
    let mut src = SliceSource::new(&data);
    let _ = read(&mut src, &mut scratch, &mut sink);
    let after = ALLOCS.with(Cell::get);
    assert_eq!(before, after, "read allocated on the read path");
    let _ = StreamFacts::default();
}

#[test]
fn disc_folder_parse_does_not_allocate() {
    let before = ALLOCS.with(Cell::get);
    let hit = disc_folder::parse("Disc 2 - Liyue");
    let miss = disc_folder::parse("Discography");
    let after = ALLOCS.with(Cell::get);
    assert_eq!(before, after, "disc_folder::parse allocated");
    assert_eq!(hit.map(|d| (d.number, d.subtitle)), Some((2, Some("Liyue"))));
    assert_eq!(miss, None);
}
```

- [ ] **Step 8: Run the probes + the embedded build**

Run: `cargo test -p soul-audio-tags --test no_alloc && cargo build -p soul-audio-tags --target thumbv7em-none-eabihf --no-default-features`
Expected: 2 tests PASS; the thumbv7em build succeeds (the matcher is `core`-only).

- [ ] **Step 9: Commit**

```bash
git add crates/soul-audio-tags/src/disc_folder.rs crates/soul-audio-tags/src/lib.rs crates/soul-audio-tags/tests/no_alloc.rs
git commit -m "feat(tags): no_std disc-folder name matcher"
```

---

## Task 2: Split `n/total` numbers + map disc/track totals

**Files:**
- Modify: `crates/soul-audio-tags/src/util.rs` (new helpers after `map_musicbrainz_recording_key`, `:40`)
- Modify: `crates/soul-audio-tags/src/vorbis.rs:33-36` (`map_vorbis_key`), `:124` (emit)
- Modify: `crates/soul-audio-tags/src/id3v2.rs:299-332` (`handle_num_total`), `:639-656` (call sites)
- Modify: `crates/soul-audio-tags/src/wavpack.rs:116` (emit), `:126-145` (`map_key`)
- Modify: `crates/soul-audio-tags/src/owned.rs:119-130` (count parse) + import of `parse_count`
- Test: inline test modules of the same files

**Interfaces:**
- Produces (crate-private): `util::split_num_total(s: &str) -> (&str, Option<&str>)`; `util::emit_text(sink: &mut impl TagSink, field: Option<Field>, raw_key: &str, value: &str)` — for `TrackNumber`/`DiscNumber` it emits the number and (when present) the total as `TrackTotal`/`DiscTotal`; `#[cfg(feature = "alloc")] util::parse_count(s: &str) -> Option<u32>`.
- Behaviour: Vorbis/Ogg/APE `DISCNUMBER=1/3` now yields `disc = Some(1), disc_total = Some(3)` (was `None` → importer disc 1).

- [ ] **Step 1: Write the failing tests**

Append to `crates/soul-audio-tags/src/util.rs` (a NON-alloc-gated module, so it runs without features):

```rust
#[cfg(test)]
mod num_total_tests {
    use super::split_num_total;

    #[test]
    fn splits_number_and_total() {
        assert_eq!(split_num_total("1/3"), ("1", Some("3")));
        assert_eq!(split_num_total(" 2 / 4 "), ("2", Some("4")));
        assert_eq!(split_num_total("05/"), ("05", None));
        assert_eq!(split_num_total("7"), ("7", None));
        assert_eq!(split_num_total(" 01 "), ("01", None));
    }
}
```

Append to the `tests` module of `crates/soul-audio-tags/src/vorbis.rs`:

```rust
    #[test]
    fn vorbis_splits_n_over_total_and_maps_disc_totals() {
        let body = comment_body(&["DISCNUMBER=1/3", "TRACKNUMBER=05/12", "TOTALDISCS=3"]);
        let mut src = SliceSource::new(&body);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_comment_vector(&mut src, body.len() as u32, &mut scratch, &mut c).unwrap();
        let got: Vec<(Field, &str)> = c
            .fields
            .iter()
            .filter_map(|(f, _, v)| f.map(|f| (f, v.as_str())))
            .collect();
        assert_eq!(
            got,
            vec![
                (Field::DiscNumber, "1"),
                (Field::DiscTotal, "3"),
                (Field::TrackNumber, "05"),
                (Field::TrackTotal, "12"),
                (Field::DiscTotal, "3"),
            ]
        );
    }

    #[test]
    fn vorbis_disc_total_keys_map() {
        assert_eq!(map_vorbis_key("DISCTOTAL"), Some(Field::DiscTotal));
        assert_eq!(map_vorbis_key("totaldiscs"), Some(Field::DiscTotal));
        assert_eq!(map_vorbis_key("TRACKTOTAL"), Some(Field::TrackTotal));
    }
```

Append to the `tests` module of `crates/soul-audio-tags/src/wavpack.rs`:

```rust
    #[test]
    fn apev2_disc_and_track_split_n_over_total() {
        let mut v = Vec::new();
        v.extend_from_slice(&wvpk_header(1000, 9, false));
        v.extend_from_slice(&[0u8; 32]);
        v.extend_from_slice(&apev2(&[
            ("Track", "3/9"),
            ("Disc", "2/2"),
            ("TOTALDISCS", "2"),
        ]));
        let mut src = SliceSource::new(&v);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_wavpack(&mut src, &mut scratch, &mut c).unwrap();
        let got: Vec<(Field, &str)> = c
            .fields
            .iter()
            .filter_map(|(f, _, v)| f.map(|f| (f, v.as_str())))
            .collect();
        assert_eq!(
            got,
            vec![
                (Field::TrackNumber, "3"),
                (Field::TrackTotal, "9"),
                (Field::DiscNumber, "2"),
                (Field::DiscTotal, "2"),
                (Field::DiscTotal, "2"),
            ]
        );
    }
```

Append to the `tests` module of `crates/soul-audio-tags/src/id3v2.rs`:

```rust
    #[test]
    fn id3_tpos_splits_disc_and_total() {
        let tag = id3v23(&[text_frame(b"TPOS", "2/3")]);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_id3v2(&mut SliceSource::new(&tag), &mut scratch, &mut c).unwrap();
        let got: Vec<(Field, &str)> = c
            .fields
            .iter()
            .filter_map(|(f, _, v)| f.map(|f| (f, v.as_str())))
            .collect();
        assert_eq!(got, vec![(Field::DiscNumber, "2"), (Field::DiscTotal, "3")]);
    }
```

Append to the `tests` module of `crates/soul-audio-tags/src/owned.rs`:

```rust
    #[test]
    fn read_owned_splits_vorbis_disc_and_track_n_over_total() {
        // Regression: `DISCNUMBER=2/3` used to fail `parse()` and become `None`
        // (the importer then defaulted it to disc 1).
        let bytes = flac_fixture(&["DISCNUMBER=2/3", "TRACKNUMBER=7/12"]);
        let (tags, _) = read_owned(&mut SliceSource::new(&bytes)).unwrap();
        assert_eq!((tags.disc, tags.disc_total), (Some(2), Some(3)));
        assert_eq!((tags.track, tags.track_total), (Some(7), Some(12)));
    }

    #[test]
    fn read_owned_reads_totaldiscs_and_tolerates_padding() {
        let bytes = flac_fixture(&["DISCNUMBER= 01 ", "TOTALDISCS=4"]);
        let (tags, _) = read_owned(&mut SliceSource::new(&bytes)).unwrap();
        assert_eq!((tags.disc, tags.disc_total), (Some(1), Some(4)));
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p soul-audio-tags --features std`
Expected: FAIL to compile (`split_num_total` not found); after a stub, the Vorbis/APE/owned tests fail because `DISCNUMBER=1/3` is emitted unsplit and `DISCTOTAL`/APE `Disc` are unmapped.

- [ ] **Step 3: Implement the shared helpers**

In `crates/soul-audio-tags/src/util.rs`, change the first import line to `use crate::{Field, TagError, TagSink, locate};` and add after `map_musicbrainz_recording_key`:

```rust
/// Split an `n/total` number tag (`"1/3"`, `" 05 / 12 "`) into its trimmed
/// number and, when non-empty, its total. A bare number has no total. ID3
/// `TRCK`/`TPOS`, VorbisComment and APE `TRACKNUMBER`/`DISCNUMBER` all use this
/// form.
pub(crate) fn split_num_total(s: &str) -> (&str, Option<&str>) {
    match s.split_once('/') {
        Some((n, t)) => (n.trim(), Some(t.trim()).filter(|t| !t.is_empty())),
        None => (s.trim(), None),
    }
}

/// Hand one text value to `sink`, splitting a `TrackNumber`/`DiscNumber` value
/// of the form `n/total` into the number plus a `TrackTotal`/`DiscTotal` call
/// (the total only when present). Every other field passes through verbatim.
pub(crate) fn emit_text(
    sink: &mut impl TagSink,
    field: Option<Field>,
    raw_key: &str,
    value: &str,
) {
    let total = match field {
        Some(Field::TrackNumber) => Field::TrackTotal,
        Some(Field::DiscNumber) => Field::DiscTotal,
        _ => {
            sink.text(field, raw_key, value);
            return;
        }
    };
    let (number, total_value) = split_num_total(value);
    sink.text(field, raw_key, number);
    if let Some(t) = total_value {
        sink.text(Some(total), raw_key, t);
    }
}

/// Parse a track/disc number or total, tolerating whitespace and a stray
/// `n/total` form (`"2/3"` → 2). `None` on garbage. `alloc`-gated (only the
/// owned snapshot parses values; see [`parse_gain_db`]).
#[cfg(feature = "alloc")]
pub(crate) fn parse_count(s: &str) -> Option<u32> {
    split_num_total(s).0.parse().ok()
}
```

- [ ] **Step 4: Wire Vorbis (and so Ogg, which reuses `read_comment_vector`)**

In `crates/soul-audio-tags/src/vorbis.rs` `map_vorbis_key`, after the `DISCNUMBER` branch (`:33-34`) add:

```rust
    } else if key.eq_ignore_ascii_case("DISCTOTAL") || key.eq_ignore_ascii_case("TOTALDISCS") {
        Some(Field::DiscTotal)
```

and in `read_comment_vector` replace `sink.text(map_vorbis_key(key), key, value);` (`:124`) with:

```rust
                crate::util::emit_text(sink, map_vorbis_key(key), key, value);
```

- [ ] **Step 5: Wire ID3 through the shared helper**

In `crates/soul-audio-tags/src/id3v2.rs`, replace `handle_num_total` (`:299-332`) with:

```rust
/// `TRCK`/`TPOS` (and v2.2 `TRK`/`TPA`): "n" or "n/total" → one or two
/// `sink.text` calls (number, then total if present) via the shared
/// [`crate::util::emit_text`] split.
fn handle_num_total<S: embedded_io::Read>(
    src: &mut S,
    raw_len: usize,
    raw_key: &str,
    number_field: Field,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    if raw_len == 0 {
        return Ok(());
    }
    let mut enc = [0u8; 1];
    read_exact(src, &mut enc)?;
    let s = decode_text(src, enc[0], raw_len - 1, scratch)?;
    crate::util::emit_text(sink, Some(number_field), raw_key, s);
    Ok(())
}
```

and in `handle_frame` (`:639-656`) drop the now-unused total argument:

```rust
        "TRCK" | "TRK" => handle_num_total(src, raw_len, id, Field::TrackNumber, scratch, sink),
        "TPOS" | "TPA" => handle_num_total(src, raw_len, id, Field::DiscNumber, scratch, sink),
```

- [ ] **Step 6: Wire APE (WavPack)**

In `crates/soul-audio-tags/src/wavpack.rs` replace `sink.text(Some(field), raw, s);` (`:116`) with `crate::util::emit_text(sink, Some(field), raw, s);`, and in `map_key` after the `Track` branch add:

```rust
    } else if key.eq_ignore_ascii_case(b"Disc") {
        Some(Field::DiscNumber)
    } else if key.eq_ignore_ascii_case(b"DISCTOTAL") || key.eq_ignore_ascii_case(b"TOTALDISCS") {
        Some(Field::DiscTotal)
    } else if key.eq_ignore_ascii_case(b"TRACKTOTAL") || key.eq_ignore_ascii_case(b"TOTALTRACKS") {
        Some(Field::TrackTotal)
```

- [ ] **Step 7: Make the owned parse robust**

In `crates/soul-audio-tags/src/owned.rs` change the util import to
`use crate::util::{parse_count, parse_gain_db, parse_peak, parse_r128_gain_db, read_exact};` and replace the four count arms (`:119-130`) with:

```rust
            Field::TrackNumber if self.tags.track.is_none() => {
                self.tags.track = parse_count(value);
            }
            Field::TrackTotal if self.tags.track_total.is_none() => {
                self.tags.track_total = parse_count(value);
            }
            Field::DiscNumber if self.tags.disc.is_none() => {
                self.tags.disc = parse_count(value);
            }
            Field::DiscTotal if self.tags.disc_total.is_none() => {
                self.tags.disc_total = parse_count(value);
            }
```

- [ ] **Step 8: Run the tag suite**

Run: `cargo test -p soul-audio-tags --features std`
Expected: all PASS, including the 6 new tests and the existing `TRCK 2/5` assertion in `id3v23_text_and_apic`.

- [ ] **Step 9: Commit**

```bash
git add crates/soul-audio-tags/src/
git commit -m "fix(tags): split n/total disc and track numbers; map disc totals"
```

---

## Task 3: `DiscSubtitle` + `MusicBrainzAlbumId` fields

**Files:**
- Modify: `crates/soul-audio-tags/src/lib.rs:50` (after `DiscTotal`), `:85` (after `MusicBrainzRecordingId`)
- Modify: `crates/soul-audio-tags/src/util.rs` (two key maps)
- Modify: `crates/soul-audio-tags/src/vorbis.rs:46-49`, `id3v2.rs:21-23,248,492`, `mp4.rs:36,341-343`, `wavpack.rs` `map_key`
- Modify: `crates/soul-audio-tags/src/owned.rs:56` (fields), `:114` (arms)

**Interfaces:**
- Produces: `Field::DiscSubtitle`, `Field::MusicBrainzAlbumId`; `Tags { …, pub disc_subtitle: Option<String>, pub musicbrainz_album_id: Option<String> }` (first value wins). Consumed by T5 (`extract_raw`) and T8 (organize grouping).

- [ ] **Step 1: Write the failing tests**

Append to `vorbis.rs` tests:

```rust
    #[test]
    fn vorbis_disc_subtitle_and_mb_album_id() {
        let body = comment_body(&[
            "DISCSUBTITLE=Mondstadt",
            "MUSICBRAINZ_ALBUMID=0b1e5b0a-1111-4222-8333-444455556666",
        ]);
        let mut src = SliceSource::new(&body);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_comment_vector(&mut src, body.len() as u32, &mut scratch, &mut c).unwrap();
        let got: Vec<(Field, &str)> = c
            .fields
            .iter()
            .filter_map(|(f, _, v)| f.map(|f| (f, v.as_str())))
            .collect();
        assert_eq!(
            got,
            vec![
                (Field::DiscSubtitle, "Mondstadt"),
                (Field::MusicBrainzAlbumId, "0b1e5b0a-1111-4222-8333-444455556666"),
            ]
        );
    }
```

Append to `id3v2.rs` tests:

```rust
    #[test]
    fn id3_tsst_and_txxx_musicbrainz_album_id() {
        let mbid = "0b1e5b0a-1111-4222-8333-444455556666";
        let tag = id3v24(&[text_frame_v24(b"TSST", "璃月 Liyue")]);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_id3v2(&mut SliceSource::new(&tag), &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::DiscSubtitle) && v == "璃月 Liyue"),
            "TSST -> DiscSubtitle (full UTF-8): {:?}",
            c.fields
        );

        let tag = id3v23(&[txxx_frame("MusicBrainz Album Id", mbid)]);
        let mut c = Collect::default();
        read_id3v2(&mut SliceSource::new(&tag), &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::MusicBrainzAlbumId) && v == mbid),
            "TXXX:MusicBrainz Album Id -> MusicBrainzAlbumId: {:?}",
            c.fields
        );
    }
```

Append to `mp4.rs` tests:

```rust
    #[test]
    fn mp4_freeform_disc_subtitle_and_mb_album_id() {
        let mbid = "0b1e5b0a-1111-4222-8333-444455556666";
        let entries = ilst(&[
            freeform_entry("com.apple.iTunes", "DISCSUBTITLE", "Inazuma"),
            freeform_entry("com.apple.iTunes", "MusicBrainz Album Id", mbid),
        ]);
        let moov_body = {
            let mut v = Vec::new();
            v.extend_from_slice(&mvhd_v0(44_100, 1000));
            v.extend_from_slice(&udta(&meta(&entries)));
            v
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&atom(b"ftyp", b"M4A \0\0\0\0M4A mp42isom"));
        bytes.extend_from_slice(&atom(b"moov", &moov_body));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_mp4(&mut src, &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::DiscSubtitle) && v == "Inazuma"),
            "{:?}",
            c.fields
        );
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::MusicBrainzAlbumId) && v == mbid),
            "{:?}",
            c.fields
        );
    }
```

Append to `wavpack.rs` tests:

```rust
    #[test]
    fn apev2_disc_subtitle_and_mb_album_id() {
        let mut v = Vec::new();
        v.extend_from_slice(&wvpk_header(1000, 9, false));
        v.extend_from_slice(&[0u8; 32]);
        v.extend_from_slice(&apev2(&[
            ("DiscSubtitle", "Sumeru"),
            ("MUSICBRAINZ_ALBUMID", "0b1e5b0a-1111-4222-8333-444455556666"),
        ]));
        let mut src = SliceSource::new(&v);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_wavpack(&mut src, &mut scratch, &mut c).unwrap();
        let got: Vec<(Field, &str)> = c
            .fields
            .iter()
            .filter_map(|(f, _, v)| f.map(|f| (f, v.as_str())))
            .collect();
        assert_eq!(
            got,
            vec![
                (Field::DiscSubtitle, "Sumeru"),
                (Field::MusicBrainzAlbumId, "0b1e5b0a-1111-4222-8333-444455556666"),
            ]
        );
    }
```

Append to `owned.rs` tests:

```rust
    #[test]
    fn read_owned_collects_disc_subtitle_and_mb_album_id() {
        let bytes = flac_fixture(&[
            "DISCSUBTITLE=Mondstadt",
            "DISCSUBTITLE=ignored second value",
            "MUSICBRAINZ_ALBUMID=0b1e5b0a-1111-4222-8333-444455556666",
        ]);
        let (tags, _) = read_owned(&mut SliceSource::new(&bytes)).unwrap();
        assert_eq!(tags.disc_subtitle.as_deref(), Some("Mondstadt"));
        assert_eq!(
            tags.musicbrainz_album_id.as_deref(),
            Some("0b1e5b0a-1111-4222-8333-444455556666")
        );
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p soul-audio-tags --features std`
Expected: FAIL to compile — no variant `Field::DiscSubtitle` / `Field::MusicBrainzAlbumId`, no field `disc_subtitle`.

- [ ] **Step 3: Add the variants**

In `crates/soul-audio-tags/src/lib.rs`, after `DiscTotal,` (`:50`):

```rust
    /// Per-disc subtitle ("Mondstadt"): Vorbis/APE `DISCSUBTITLE`, ID3v2.4
    /// `TSST`, MP4 freeform `----:com.apple.iTunes:DISCSUBTITLE`.
    DiscSubtitle,
```

and after `MusicBrainzRecordingId,` (`:85`):

```rust
    /// The MusicBrainz **release** MBID (a UUID) — splits two releases of one
    /// title dumped in one folder. Vorbis/APE `MUSICBRAINZ_ALBUMID`, ID3
    /// `TXXX:MusicBrainz Album Id`, MP4 `----:com.apple.iTunes:MusicBrainz Album Id`.
    MusicBrainzAlbumId,
```

- [ ] **Step 4: Add the shared key maps**

In `crates/soul-audio-tags/src/util.rs`, after `map_musicbrainz_recording_key`:

```rust
/// `DISCSUBTITLE` (VorbisComment / APE `DiscSubtitle` / MP4 freeform name),
/// case-insensitive.
pub(crate) fn map_disc_subtitle_key(key: &str) -> Option<Field> {
    key.trim()
        .eq_ignore_ascii_case("DISCSUBTITLE")
        .then_some(Field::DiscSubtitle)
}

/// The MusicBrainz **release** id key across containers: VorbisComment/APE
/// `MUSICBRAINZ_ALBUMID`, ID3 `TXXX` / MP4 freeform `MusicBrainz Album Id`.
pub(crate) fn map_musicbrainz_album_key(key: &str) -> Option<Field> {
    let k = key.trim();
    (k.eq_ignore_ascii_case("MUSICBRAINZ_ALBUMID") || k.eq_ignore_ascii_case("MusicBrainz Album Id"))
        .then_some(Field::MusicBrainzAlbumId)
}
```

- [ ] **Step 5: Wire the containers**

- `vorbis.rs` — the fallback chain at the end of `map_vorbis_key` (`:46-49`) becomes:
  ```rust
          crate::util::map_replaygain_key(key)
              .or_else(|| crate::util::map_musicbrainz_recording_key(key))
              .or_else(|| crate::util::map_disc_subtitle_key(key))
              .or_else(|| crate::util::map_musicbrainz_album_key(key))
  ```
- `id3v2.rs` — extend the util import (`:21-23`) with `map_musicbrainz_album_key`; in `map_frame_id` after `"TCOM" | "TCM" => …` (`:248`) add `"TSST" => Some(Field::DiscSubtitle),`; in `handle_txxx` (`:492`):
  ```rust
      let field = map_replaygain_key(desc)
          .or_else(|| map_musicbrainz_recording_key(desc))
          .or_else(|| map_musicbrainz_album_key(desc));
  ```
- `mp4.rs` — import line (`:36`) becomes
  `use crate::util::{map_disc_subtitle_key, map_musicbrainz_album_key, map_musicbrainz_recording_key, map_replaygain_key, read_exact, utf8_prefix};`
  and the freeform chain (`:341-343`):
  ```rust
      let Some(field) = map_replaygain_key(name)
          .or_else(|| map_musicbrainz_recording_key(name))
          .or_else(|| map_original_date_key(name))
          .or_else(|| map_disc_subtitle_key(name))
          .or_else(|| map_musicbrainz_album_key(name))
  ```
- `wavpack.rs` `map_key` — before the final `else { None }`:
  ```rust
      } else if key.eq_ignore_ascii_case(b"DiscSubtitle") {
          Some(Field::DiscSubtitle)
      } else if key.eq_ignore_ascii_case(b"MUSICBRAINZ_ALBUMID") {
          Some(Field::MusicBrainzAlbumId)
  ```

- [ ] **Step 6: Owned snapshot**

In `owned.rs` `struct Tags`, after `pub musicbrainz_recording_id: Option<String>,` (`:56`):

```rust
    /// Per-disc subtitle ("Mondstadt"), first value wins. `None` when untagged.
    pub disc_subtitle: Option<String>,
    /// MusicBrainz **release** MBID, first value wins — part of the album
    /// identity key (two releases of one title in one folder stay apart).
    pub musicbrainz_album_id: Option<String>,
```

and in `OwnedTags::text`, after the `Field::MusicBrainzRecordingId => { … }` arm:

```rust
            Field::DiscSubtitle => {
                self.tags.disc_subtitle.get_or_insert_with(|| value.into());
            }
            Field::MusicBrainzAlbumId => {
                self.tags
                    .musicbrainz_album_id
                    .get_or_insert_with(|| value.into());
            }
```

- [ ] **Step 7: Run the suite**

Run: `cargo test -p soul-audio-tags --features std && cargo build -p soul-audio-tags --target thumbv7em-none-eabihf --no-default-features`
Expected: all PASS; embedded build OK.

- [ ] **Step 8: Commit**

```bash
git add crates/soul-audio-tags/src/
git commit -m "feat(tags): read disc subtitle and MusicBrainz release id"
```

---

## Task 4: `.soulidx` v8 — `TrackRec.disc_subtitle`, `AlbumRec.disc_count` + `mb_album_id`

**Files:**
- Modify: `crates/soul-audio-catalog/src/lib.rs:62-95` (`TrackRec`), `:170-185` (`AlbumRec`)
- Modify: `crates/soul-audio-index/src/lib.rs` — module doc layout (`:37-50`), `FORMAT_VERSION` (`:106`), `RECORD_SIZE`/`ALBUM_RECORD_SIZE` (`:173-174`), reader `track()` (`:376-418`) / `album()` (`:420-441`), baker `TrackInput`/`AlbumMeta` (`:600-643`), `intern` (`:674`), heap build (`:744-788`), record writers (`:972-1025`)
- Modify: `crates/soul-audio-index/tests/roundtrip.rs` (fixture literals, v8 tests, the `FORMAT_VERSION == 7` assertion at `:651-655`)
- Create: `crates/soul-audio-index/tests/reader_no_alloc.rs`; Modify: `crates/soul-audio-index/Cargo.toml` (`[[test]]` entry)
- Modify (literals only): `crates/soul-audio-index/benches/reader.rs:111-140`, `crates/soul-audio-multicatalog/tests/multicatalog.rs` (9 sites), `crates/soul-audio-import/src/lib.rs:294-300,466-482`
- Modify: `crates/architecture-tests/tests/no_alloc.rs:75` (header bytes)

**Interfaces:**
- Consumes: nothing from T1–T3.
- Produces:
  - `TrackRec { …, pub disc_subtitle: StrRef }` (read at offsets 85/89).
  - `AlbumRec { …, pub disc_count: u16, pub mb_album_id: StrRef }` (read at 30 and 32/36).
  - `build::TrackInput { …, pub disc_subtitle: String }`; `build::AlbumMeta { …, pub disc_count: u16, pub mb_album_id: String }`.
  - `FORMAT_VERSION == 8`.

- [ ] **Step 0: Capture the reader bench baseline (v7)**

Run (before touching code): `cargo bench -p soul-audio-index --features build --bench reader > ../bench-v7.txt 2>&1`
Expected: the "DAP I/O model" table (reads / cold blocks / modeled ms per scenario). Keep the file (outside the worktree) for Task 11.

- [ ] **Step 1: Write the failing v8 round-trip tests**

First update every existing literal so the test files compile once the fields exist. From the worktree root:

```bash
perl -0pi -e 's/^(\s*)mbid: (.+),\n/$1mbid: $2,\n$1disc_subtitle: String::new(),\n/mg' \
  crates/soul-audio-index/tests/roundtrip.rs crates/soul-audio-index/benches/reader.rs \
  crates/soul-audio-multicatalog/tests/multicatalog.rs
perl -0pi -e 's/^(\s*)release_date: (.+),\n/$1release_date: $2,\n$1disc_count: 1,\n$1mb_album_id: String::new(),\n/mg' \
  crates/soul-audio-index/tests/roundtrip.rs crates/soul-audio-index/benches/reader.rs \
  crates/soul-audio-multicatalog/tests/multicatalog.rs
git diff --stat
```

Expected: `roundtrip.rs` 10 `TrackInput` + 8 `AlbumMeta` sites, `multicatalog.rs` 4 + 5, `reader.rs` 1 + 1 (every `TrackInput` literal in these files has a `mbid: …,` line and every `AlbumMeta` literal a `release_date: …,` line; nothing else matches `^\s*mbid: ` / `^\s*release_date: `).

In `roundtrip.rs`, change the version assertion in `loudness_fields_round_trip` (`:651-655`) to:

```rust
    assert_eq!(
        soul_audio_index::FORMAT_VERSION,
        8,
        "v8: disc subtitle / disc count / MB release id appended"
    );
```

and append these tests to `roundtrip.rs`:

```rust
/// v8: the per-disc subtitle (track) and disc count + MusicBrainz release id
/// (album) round-trip; a subtitle shared by several tracks is ONE heap string;
/// every pre-v8 field still reads back from its unchanged offset.
#[test]
fn v8_disc_fields_round_trip_and_share_heap_strings() {
    let mbid = "0b1e5b0a-1111-4222-8333-444455556666";
    let mut input = fixture();
    input.tracks[0].disc_subtitle = "Mondstadt".into();
    input.tracks[1].disc_subtitle = "Mondstadt".into();
    input.tracks[0].mbid = "rec-mbid".into();
    input.albums[0].disc_count = 2;
    input.albums[0].mb_album_id = mbid.into();
    let bytes = write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 64];

    let t0 = rdr.track(0).unwrap();
    let t1 = rdr.track(1).unwrap();
    assert_eq!(resolve(&rdr, t0.disc_subtitle, &mut buf), "Mondstadt");
    assert_eq!(
        t0.disc_subtitle, t1.disc_subtitle,
        "one heap string per distinct subtitle"
    );
    let a = rdr.album(0).unwrap();
    assert_eq!(a.disc_count, 2);
    assert_eq!(resolve(&rdr, a.mb_album_id, &mut buf), mbid);

    // Append-only: v7 fields at their old offsets are untouched.
    assert_eq!(resolve(&rdr, t0.title, &mut buf), "Aria");
    assert_eq!(resolve(&rdr, t0.file_name, &mut buf), "01 - Aria.flac");
    assert_eq!(resolve(&rdr, t0.mbid, &mut buf), "rec-mbid");
    assert_eq!((t0.track_no, t0.disc_no, t0.duration_ms), (1, 1, 299_520));
    assert_eq!(t0.loudness, None);
    assert_eq!(resolve(&rdr, a.title, &mut buf), "Open Goldberg Variations");
    assert_eq!(a.original_date, PackedDate::new(1973, 8, 7));
    assert_eq!(a.release_date, PackedDate::from_year(2015));
    assert_eq!((a.folder, a.art_id), (1, 0));
}

/// Untagged v8 fields are the canonical empty ref `(0, 0)`, and a `0` disc
/// count is floored to 1 by the baker.
#[test]
fn v8_untagged_disc_fields_are_empty_and_disc_count_is_at_least_one() {
    let mut input = fixture();
    input.albums[0].disc_count = 0;
    let bytes = write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let t0 = rdr.track(0).unwrap();
    assert_eq!(t0.disc_subtitle, soul_audio_catalog::StrRef::default());
    let a = rdr.album(0).unwrap();
    assert_eq!(a.disc_count, 1);
    assert_eq!(a.mb_album_id, soul_audio_catalog::StrRef::default());
}

/// The header's section offsets prove the v8 record strides: 91 B per track,
/// 38 B per album.
#[test]
fn v8_record_strides() {
    let bytes = write_index(&fixture()); // 2 tracks, 1 album
    let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap()) as usize;
    let (records_off, albums_off, artists_off) = (u32_at(32), u32_at(36), u32_at(40));
    assert_eq!(albums_off - records_off, 2 * 91, "TRACK RECORD is 91 B");
    assert_eq!(artists_off - albums_off, 38, "ALBUM RECORD is 38 B");
}

/// The reader is exact-match on the version: a v7 file reads as an empty
/// catalog (the desktop re-bakes), never as garbage.
#[test]
fn a_v7_file_reads_as_empty() {
    let mut bytes = write_index(&fixture());
    bytes[4..6].copy_from_slice(&7u16.to_le_bytes());
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    assert_eq!(rdr.track_count(), 0);
    assert_eq!(rdr.album_count(), 0);
    assert!(rdr.track(0).is_err());
}
```

Create `crates/soul-audio-index/tests/reader_no_alloc.rs`:

```rust
//! The v8 reader path over REAL baked bytes allocates nothing. The
//! architecture-tests probe only sees an empty header, so this one exercises the
//! new disc fields end to end (bake first, then measure the reads).
#![cfg(feature = "build")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use soul_audio_catalog::{Catalog, NO_FOLDER, PackedDate};
use soul_audio_core::SliceSource;
use soul_audio_index::IndexReader;
use soul_audio_index::build::{AlbumMeta, CatalogInput, FolderInput, TrackInput, write_index};

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get().wrapping_add(1)));
        // SAFETY: forwarding to the system allocator with the same layout.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: forwarding to the system allocator with the same ptr/layout.
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static GA: Counting = Counting;

#[test]
fn v8_reader_reads_disc_fields_without_allocating() {
    let input = CatalogInput {
        tracks: vec![TrackInput {
            title: "Dawn".into(),
            artist: 0,
            album: 0,
            album_artist: 0,
            genre: 0,
            folder: 1,
            track_no: 1,
            disc_no: 2,
            duration_ms: 1000,
            sample_rate: 48_000,
            channels: 2,
            file_name: "01 Dawn.flac".into(),
            mbid: String::new(),
            disc_subtitle: "Mondstadt".into(),
            loudness: None,
        }],
        artists: vec!["HOYO-MiX".into()],
        artist_sorts: Vec::new(),
        genres: vec!["Game".into()],
        albums: vec![AlbumMeta {
            title: "Suite".into(),
            album_artist: 0,
            original_date: PackedDate::UNKNOWN,
            release_date: PackedDate::UNKNOWN,
            folder: 1,
            disc_count: 2,
            mb_album_id: "0b1e5b0a-1111-4222-8333-444455556666".into(),
        }],
        folders: vec![
            FolderInput {
                path: String::new(),
                parent: NO_FOLDER,
            },
            FolderInput {
                path: "Suite".into(),
                parent: 0,
            },
        ],
        playlists: Vec::new(),
        covers: Vec::new(),
    };
    let bytes = write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 64];

    let before = ALLOCS.with(Cell::get);
    let t = rdr.track(0).unwrap();
    let n = rdr.resolve_str(t.disc_subtitle, &mut buf).unwrap();
    let a = rdr.album(t.album).unwrap();
    let m = rdr.resolve_str(a.mb_album_id, &mut buf).unwrap();
    let first = rdr.album_track(0, 0).unwrap();
    let after = ALLOCS.with(Cell::get);

    assert_eq!(before, after, "the v8 reader path allocated");
    assert_eq!((n, a.disc_count, m, first), (9, 2, 36, 0));
}
```

Register it in `crates/soul-audio-index/Cargo.toml`, after the `roundtrip` `[[test]]` block:

```toml
# Counting-allocator probe over real baked v8 bytes (needs the baker).
[[test]]
name = "reader_no_alloc"
required-features = ["build"]
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p soul-audio-index --features build`
Expected: FAIL to compile — `TrackInput` has no field `disc_subtitle`, `AlbumMeta` no `disc_count`/`mb_album_id`, `TrackRec` no `disc_subtitle`.

- [ ] **Step 3: Catalog records**

In `crates/soul-audio-catalog/src/lib.rs`, add to `TrackRec` after `pub loudness: Option<LoudnessRec>,`:

```rust
    /// Per-disc subtitle ("Mondstadt"), a heap ref — the tag, else the
    /// `Disc N - Subtitle` folder's text; empty `StrRef` when neither. Tracks of
    /// one disc share one heap string. (New in format v8.)
    pub disc_subtitle: StrRef,
```

and to `AlbumRec` after `pub art_id: u32,`:

```rust
    /// Number of discs: `max(max disc-total tag, max disc number seen, 1)`.
    /// `0` only in a `Default` value; a baked album is always ≥ 1. (New in
    /// format v8.)
    pub disc_count: u16,
    /// MusicBrainz **release** MBID, a heap ref (empty when untagged) — part of
    /// the album identity key. (New in format v8.)
    pub mb_album_id: StrRef,
```

- [ ] **Step 4: Index constants, doc, reader**

In `crates/soul-audio-index/src/lib.rs`:

1. Module doc — replace the TRACK RECORD and ALBUM RECORD blocks (`:37-50`) with:
   ```text
   //! TRACK RECORD (91 B, fixed; `record[i]` at records_off + i*91):
   //!    0 id u32 · 4 title.off u32 · 8 title.len u16 · 10 artist u32 ·
   //!   14 album u32 · 18 album_artist u32 · 22 genre u32 · 26 folder u32 ·
   //!   30 track_no u16 · 32 disc_no u16 · 34 duration_ms u32 · 38 sample_rate u32 ·
   //!   42 channels u8 · 43 art_id u32 · 47 flags u16 · 49 extended_off u32 ·
   //!   53 file_name.off u32 · 57 file_name.len u16 · 59 mbid.off u32 ·
   //!   63 mbid.len u16 · 65 loudness: integrated_lufs f32 · 69 lra f32 ·
   //!   73 true_peak_dbtp f32 · 77 plr f32 · 81 dr f32 ·
   //!   85 disc_subtitle.off u32 · 89 disc_subtitle.len u16
   //!   (mbid new in v4; loudness new in v6, valid iff flags & FLAG_HAS_LOUDNESS;
   //!    disc_subtitle new in v8 — appended, `(0,0)` = none, one heap string per
   //!    distinct value)
   //!
   //! ALBUM RECORD (38 B, fixed; indexed by dense album id):
   //!    0 id u32 · 4 title.off u32 · 8 title.len u16 · 10 album_artist u32 ·
   //!   14 original_date u32 · 18 release_date u32 · 22 folder u32 · 26 art_id u32 ·
   //!   30 disc_count u16 · 32 mb_album_id.off u32 · 36 mb_album_id.len u16
   //!   (original_date/release_date are PackedDate, v5; disc_count/mb_album_id
   //!    new in v8 — appended; `folder` is the album ROOT folder, so the tracks of
   //!    `Album/CD1` + `Album/CD2` share one album)
   ```
2. `:106` → `pub const FORMAT_VERSION: u16 = 8;`
3. Replace `:173-174` with:
   ```rust
   const RECORD_SIZE: usize = 91; // 65 (v5) + 5×f32 loudness (v6) + disc_subtitle StrRef (v8)
   const ALBUM_RECORD_SIZE: usize = 38; // 30 (v5) + disc_count u16 + mb_album_id StrRef (v8)
   /// v8 appended-field offsets. The compile-time checks pin them to the record
   /// ends, so an append that forgets to grow a record (or an insert that shifts
   /// them) fails to build.
   const TRACK_DISC_SUBTITLE_OFF: usize = 85;
   const ALBUM_DISC_COUNT_OFF: usize = 30;
   const ALBUM_MB_ID_OFF: usize = 32;
   const _: () = assert!(TRACK_DISC_SUBTITLE_OFF + 6 == RECORD_SIZE);
   const _: () = assert!(ALBUM_DISC_COUNT_OFF + 2 == ALBUM_MB_ID_OFF);
   const _: () = assert!(ALBUM_MB_ID_OFF + 6 == ALBUM_RECORD_SIZE);
   ```
4. Reader `track()` — after the `loudness: …,` initializer add:
   ```rust
               disc_subtitle: StrRef {
                   off: le_u32(&r, TRACK_DISC_SUBTITLE_OFF),
                   len: le_u16(&r, TRACK_DISC_SUBTITLE_OFF + 4),
               },
   ```
5. Reader `album()` — after `art_id: le_u32(&r, 26),` add:
   ```rust
               disc_count: le_u16(&r, ALBUM_DISC_COUNT_OFF),
               mb_album_id: StrRef {
                   off: le_u32(&r, ALBUM_MB_ID_OFF),
                   len: le_u16(&r, ALBUM_MB_ID_OFF + 4),
               },
   ```

- [ ] **Step 5: Baker inputs + shared interning + record writers**

In the `build` module:

1. Imports (`:593`): `use alloc::{collections::BTreeMap, string::String, vec, vec::Vec};`
2. `TrackInput` — after `pub mbid: String,`:
   ```rust
       /// Per-disc subtitle ("Mondstadt"), or empty. Interned once per distinct
       /// value. (v8)
       pub disc_subtitle: String,
   ```
3. `AlbumMeta` — after `pub folder: u32,`:
   ```rust
       /// Number of discs (`max(disc totals, disc numbers, 1)`); the baker floors
       /// `0` to `1`. (v8)
       pub disc_count: u16,
       /// MusicBrainz release MBID, or empty. (v8)
       pub mb_album_id: String,
   ```
4. After `fn intern` (`:674-678`) add:
   ```rust
       /// Intern `s` once per distinct value — every track of a disc repeats its
       /// subtitle, so they share one heap string. The empty string is the
       /// canonical `(0, 0)` ref (it never touches the heap).
       fn intern_shared(
           heap: &mut Vec<u8>,
           memo: &mut BTreeMap<String, (u32, u16)>,
           s: &str,
       ) -> (u32, u16) {
           if s.is_empty() {
               return (0, 0);
           }
           if let Some(&r) = memo.get(s) {
               return r;
           }
           let r = intern(heap, s);
           memo.insert(String::from(s), r);
           r
       }
   ```
5. In `write_index_with_art`, right after the `playlist_names` block (`:782-786`) — appended last so every pre-v8 string keeps its heap offset:
   ```rust
           // v8: per-disc subtitles + MB release ids, deduplicated (a 30-track
           // disc costs one string); empty → (0, 0).
           let mut shared: BTreeMap<String, (u32, u16)> = BTreeMap::new();
           let track_subtitles: Vec<(u32, u16)> = tracks
               .iter()
               .map(|t| intern_shared(&mut heap, &mut shared, &t.disc_subtitle))
               .collect();
           let album_mb_ids: Vec<(u32, u16)> = input
               .albums
               .iter()
               .map(|a| intern_shared(&mut heap, &mut shared, &a.mb_album_id))
               .collect();
   ```
6. Track record loop (`--- Track records ---`) — make its first statement `let rec_start = out.len();` and append after the five loudness `f32` writes:
   ```rust
               let (soff, slen) = track_subtitles[i];
               out.extend_from_slice(&soff.to_le_bytes()); // disc_subtitle.off @85 (v8)
               out.extend_from_slice(&slen.to_le_bytes()); // disc_subtitle.len @89 (v8)
               debug_assert_eq!(out.len() - rec_start, RECORD_SIZE);
   ```
7. Album record loop — first statement `let rec_start = out.len();`, then after the `art_id` write:
   ```rust
               out.extend_from_slice(&meta.disc_count.max(1).to_le_bytes()); // @30 (v8)
               let (moff, mlen) = album_mb_ids[a];
               out.extend_from_slice(&moff.to_le_bytes()); // mb_album_id.off @32 (v8)
               out.extend_from_slice(&mlen.to_le_bytes()); // mb_album_id.len @36 (v8)
               debug_assert_eq!(out.len() - rec_start, ALBUM_RECORD_SIZE);
   ```

- [ ] **Step 6: Keep the importer compiling (real values come in Task 5)**

In `crates/soul-audio-import/src/lib.rs`, in `Interner::album`'s `self.albums.push(AlbumMeta { … })` (`:294-300`) add after `folder,`:

```rust
            disc_count: 1,
            mb_album_id: String::new(),
```

and in `build_catalog`'s `tracks.push(TrackInput { … })` (`:466-482`) add after `mbid: r.mbid.clone(),`:

```rust
            disc_subtitle: String::new(),
```

- [ ] **Step 7: Architecture probe header**

In `crates/architecture-tests/tests/no_alloc.rs:75` replace the stale stub header:

```rust
    let bytes = *b"SIDX\x08\x00"; // magic + format version (v8), truncated -> empty catalog
```

- [ ] **Step 8: Run everything that bakes or reads**

Run: `cargo fmt --all && cargo test -p soul-audio-catalog && cargo test -p soul-audio-index --features build && cargo test -p soul-audio-multicatalog && cargo test -p soul-audio-import && cargo test -p architecture-tests`
Expected: all PASS — the 4 new round-trip tests, `reader_no_alloc`, the `FORMAT_VERSION == 8` assertion; `import/tests/incremental.rs` byte-identity still holds (both bake paths share the baker).

- [ ] **Step 9: Embedded build**

Run: `cargo build -p soul-audio-index --target thumbv7em-none-eabihf --no-default-features && cargo build -p soul-audio-catalog --target thumbv7em-none-eabihf --no-default-features`
Expected: OK (the reader stays `core`-only).

- [ ] **Step 10: Commit**

```bash
git add crates/soul-audio-catalog/ crates/soul-audio-index/ crates/soul-audio-multicatalog/tests/ crates/soul-audio-import/src/lib.rs crates/architecture-tests/tests/no_alloc.rs
git commit -m "feat(index): soulidx v8 with disc subtitle, disc count and MB release id"
```

---

## Task 5: Import — album-root identity, disc inference, subtitle fallback, `disc_count`, MB id

**Files:**
- Modify: `crates/soul-audio-import/src/lib.rs` — crate doc (`:1-7`), imports (`:16-18`), `Interner.album_ids` (`:200-202`), `Interner::album` (`:274-305`), new `album_root` (after `normalize_rel`, `:353`), `RawTrack` (`:366-386`), `extract_raw` (`:391-439`), `build_catalog` (`:445-498`), `SCAN_CACHE_VERSION` (`:554`)
- Test: `crates/soul-audio-import/tests/pipeline.rs`

**Interfaces:**
- Consumes: `soul_audio_tags::disc_folder::{parse, DiscFolder}` (T1); split `Tags.disc`/`disc_total` (T2); `Tags.disc_subtitle`/`musicbrainz_album_id` (T3); `TrackInput.disc_subtitle`, `AlbumMeta.{disc_count, mb_album_id}` (T4).
- Produces: `CatalogInput` where `albums[i].folder` is the **album root** folder and `disc_count`/`mb_album_id` are filled; `tracks[j].disc_no` is the effective disc and `tracks[j].disc_subtitle` the tag-or-folder subtitle; `pub const SCAN_CACHE_VERSION: u32 = 4`. Private: `fn album_root<'a>(track: &'a Path, root: &'a Path) -> (&'a Path, Option<DiscFolder<'a>>)`.

- [ ] **Step 1: Write the failing pipeline tests**

Append to `crates/soul-audio-import/tests/pipeline.rs`:

```rust
// ── Multi-disc albums (spec 2026-11-25) ─────────────────────────────────────

/// A fresh, uniquely-named temp library root.
fn fresh(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("soul_import_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Write a minimal tagged FLAC at `root/rel`, creating parent folders.
fn put(root: &Path, rel: &str, comments: &[&str]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, flac_with_comments(comments)).unwrap();
}

#[test]
fn flat_multi_disc_album_is_one_album_with_disc_count() {
    let root = fresh("md_flat");
    put(&root, "Box/101.flac", &["ALBUM=Box", "TITLE=d1t1", "TRACKNUMBER=1", "DISCNUMBER=1/2"]);
    put(&root, "Box/102.flac", &["ALBUM=Box", "TITLE=d1t2", "TRACKNUMBER=2", "DISCNUMBER=1/2"]);
    put(&root, "Box/201.flac", &["ALBUM=Box", "TITLE=d2t1", "TRACKNUMBER=1", "DISCNUMBER=2/2"]);

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.albums[0].disc_count, 2);
    let discs: Vec<u16> = input.tracks.iter().map(|t| t.disc_no).collect();
    assert_eq!(discs, vec![1, 1, 2], "DISCNUMBER=n/total splits (was silently disc 1)");

    // Baked v8: disc count on the album record, album run in (disc, track) order.
    let bytes = soul_audio_index::build::write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    assert_eq!(rdr.album(0).unwrap().disc_count, 2);
    let mut buf = [0u8; 128];
    let run: Vec<String> = (0..rdr.album_track_count(0))
        .map(|i| {
            let t = rdr.track(rdr.album_track(0, i).unwrap()).unwrap();
            resolve(&rdr, t.title, &mut buf).to_owned()
        })
        .collect();
    assert_eq!(run, ["d1t1", "d1t2", "d2t1"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn disc_subfolders_fold_into_the_album_root() {
    let root = fresh("md_cd");
    put(&root, "Box/CD1/01.flac", &["ALBUM=Box", "TITLE=a", "TRACKNUMBER=1", "DISCNUMBER=1"]);
    put(&root, "Box/CD2/01.flac", &["ALBUM=Box", "TITLE=b", "TRACKNUMBER=1", "DISCNUMBER=2"]);

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1, "CD1 + CD2 = one album");
    assert_eq!(input.albums[0].disc_count, 2);
    let path = |f: u32| input.folders[f as usize].path.clone();
    assert_eq!(path(input.albums[0].folder), "Box", "album folder = album root");
    assert_eq!(path(input.tracks[0].folder), "Box/CD1", "track folder = its real location");
    assert_eq!(path(input.tracks[1].folder), "Box/CD2");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn untagged_disc_folders_infer_disc_numbers() {
    let root = fresh("md_infer");
    put(&root, "Box/Disc 1/01.flac", &["ALBUM=Box", "TITLE=a", "TRACKNUMBER=1"]);
    put(&root, "Box/Disc 2/01.flac", &["ALBUM=Box", "TITLE=b", "TRACKNUMBER=1"]);

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.albums[0].disc_count, 2);
    let discs: Vec<u16> = input.tracks.iter().map(|t| t.disc_no).collect();
    assert_eq!(discs, vec![1, 2], "disc number from the folder when untagged");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn disc_folder_subtitle_is_a_fallback_for_the_tag() {
    let root = fresh("md_sub");
    put(&root, "Suite/Disc 1 - Mondstadt/01.flac", &["ALBUM=Suite", "TITLE=a", "TRACKNUMBER=1"]);
    put(
        &root,
        "Suite/Disc 2 - Liyue/01.flac",
        &["ALBUM=Suite", "TITLE=b", "TRACKNUMBER=1", "DISCSUBTITLE=Liyue Harbor"],
    );

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.tracks[0].disc_subtitle, "Mondstadt", "folder fallback");
    assert_eq!(input.tracks[1].disc_subtitle, "Liyue Harbor", "the tag wins");
    let bytes = soul_audio_index::build::write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 128];
    let t0 = rdr.track(0).unwrap();
    assert_eq!(resolve(&rdr, t0.disc_subtitle, &mut buf), "Mondstadt");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn same_title_in_two_folders_is_two_albums() {
    let root = fresh("md_iso");
    put(&root, "A/01.flac", &["ALBUM=Greatest Hits", "TITLE=a"]);
    put(&root, "B/01.flac", &["ALBUM=Greatest Hits", "TITLE=b"]);
    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 2, "folder isolation holds");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn two_musicbrainz_releases_in_one_folder_are_two_albums() {
    let root = fresh("md_mb");
    let (a, b) = (
        "aaaaaaaa-0000-4000-8000-000000000001",
        "bbbbbbbb-0000-4000-8000-000000000002",
    );
    let ma = format!("MUSICBRAINZ_ALBUMID={a}");
    let mb = format!("MUSICBRAINZ_ALBUMID={b}");
    put(&root, "Dump/01.flac", &["ALBUM=Footprints", "TITLE=x", ma.as_str()]);
    put(&root, "Dump/02.flac", &["ALBUM=Footprints", "TITLE=y", mb.as_str()]);

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 2, "two releases of one title stay apart");
    assert_eq!(input.albums[0].mb_album_id, a);
    let bytes = soul_audio_index::build::write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 128];
    assert_eq!(resolve(&rdr, rdr.album(1).unwrap().mb_album_id, &mut buf), b);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn various_artists_without_album_artist_is_one_album() {
    let root = fresh("md_va");
    put(&root, "VA/01.flac", &["ALBUM=Hits", "ARTIST=Alpha", "TITLE=a"]);
    put(&root, "VA/02.flac", &["ALBUM=Hits", "ARTIST=Beta", "TITLE=b"]);
    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1, "album artist is not part of the key");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cover_in_album_root_serves_disc_folder_tracks() {
    let root = fresh("md_cover");
    put(&root, "Box/CD1/01.flac", &["ALBUM=Box", "TITLE=a", "DISCNUMBER=1"]);
    put(&root, "Box/CD2/01.flac", &["ALBUM=Box", "TITLE=b", "DISCNUMBER=2"]);
    let cover = b"album-root cover bytes".to_vec();
    std::fs::write(root.join("Box/cover.jpg"), &cover).unwrap();

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.covers.len(), 1);
    assert_eq!(input.covers[0].as_deref(), Some(cover.as_slice()));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_disc_named_scan_root_is_never_climbed() {
    let outer = fresh("md_cdroot");
    let root = outer.join("CD1");
    std::fs::create_dir_all(&root).unwrap();
    put(&root, "01.flac", &["ALBUM=X", "TITLE=a"]);
    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(
        input.folders[input.albums[0].folder as usize].path, "",
        "the album root never leaves the scan root"
    );
    assert_eq!(input.tracks[0].disc_no, 1);
    let _ = std::fs::remove_dir_all(&outer);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p soul-audio-import --test pipeline`
Expected: FAIL — `disc_subfolders_fold_into_the_album_root` (2 albums), `untagged_disc_folders_infer_disc_numbers`, `disc_folder_subtitle_is_a_fallback_for_the_tag`, `two_musicbrainz_releases_in_one_folder_are_two_albums`, `cover_in_album_root_serves_disc_folder_tracks` (2 covers), and every `disc_count == 2` assertion (still 1).

- [ ] **Step 3: Album-root helper + imports**

Imports (`:16-18`): add `use soul_audio_tags::disc_folder::{self, DiscFolder};`.
Crate doc (`:3`): replace "with **folder-isolation** (album identity = parent folder)" with "with **folder-isolation** (album identity = album root folder + title + MusicBrainz release id; a `CD1`/`Disc 2` folder folds into its parent)".
After `normalize_rel` add:

```rust
/// A track's **album root** and the disc folder it sits in, if any. The album
/// root is the track's parent folder, unless that folder's name is a disc folder
/// (`CD1`, `Disc 2 - Liyue`; [`disc_folder::parse`]) strictly below the scan
/// `root` — then it is the grandparent, so `Album/CD1` + `Album/CD2` are one
/// album. Never climbs to or above `root`. `soul-audio-organize` applies the
/// same rule with the same matcher (it may not depend on this crate).
fn album_root<'a>(track: &'a Path, root: &'a Path) -> (&'a Path, Option<DiscFolder<'a>>) {
    let parent = track.parent().unwrap_or(root);
    if parent != root
        && parent.starts_with(root)
        && let Some(df) = parent
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(disc_folder::parse)
        && let Some(grandparent) = parent.parent()
    {
        return (grandparent, Some(df));
    }
    (parent, None)
}
```

- [ ] **Step 4: `RawTrack` + `extract_raw`**

In `RawTrack`, replace `disc_no: u16,` with:

```rust
    /// The tagged disc number, `None` when untagged (then the disc folder's
    /// number, else 1, is used at intern time — that needs the scan root).
    disc_no: Option<u16>,
    /// The tagged disc total (`DISCTOTAL`/`TOTALDISCS`/`n/total`), 0 when untagged.
    disc_total: u16,
    /// The tagged disc subtitle (trimmed), empty when untagged.
    disc_subtitle: String,
    /// The MusicBrainz release id (trimmed), empty when untagged — album key part.
    mb_album_id: String,
```

In `extract_raw`'s `RawTrack { … }` literal, replace `disc_no: tags.disc.unwrap_or(1) as u16,` with:

```rust
        disc_no: tags.disc.filter(|&d| d > 0).map(|d| d as u16),
        disc_total: tags.disc_total.unwrap_or(0) as u16,
        disc_subtitle: tags
            .disc_subtitle
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_owned(),
        mb_album_id: tags
            .musicbrainz_album_id
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_owned(),
```

(These borrow `tags`; keep the existing `mbid: tags.musicbrainz_recording_id.unwrap_or_default()` move as the literal's last field.)

- [ ] **Step 5: `Interner` key, `album`, `note_disc`**

Replace the album-key comment + field (`:200-202`) with:

```rust
    // album key = (album-root folder id, title, MB release id) → folder-isolation
    // that folds disc folders and splits releases dumped into one folder.
    album_ids: HashMap<(u32, String, String), u32>,
```

Replace `Interner::album` (`:274-305`, including the `disc_count: 1, mb_album_id: String::new()` added in Task 4) with:

```rust
    /// Intern an album under folder-isolation, keyed on its ALBUM ROOT folder,
    /// title and MusicBrainz release id; fills its metadata on first sight
    /// (first track wins), along with `dir` (the album root on disk) and
    /// `track_path` (a representative track, for embedded cover-art discovery).
    /// `disc_count` starts at 1 and grows via [`Interner::note_disc`].
    #[allow(clippy::too_many_arguments)] // album identity + both dates + cover-discovery inputs
    fn album(
        &mut self,
        title: &str,
        folder: u32,
        mb_album_id: &str,
        album_artist: u32,
        original_date: soul_audio_catalog::PackedDate,
        release_date: soul_audio_catalog::PackedDate,
        dir: &Path,
        track_path: &Path,
    ) -> u32 {
        let key = (folder, title.to_owned(), mb_album_id.to_owned());
        if let Some(&id) = self.album_ids.get(&key) {
            return id;
        }
        let id = self.albums.len() as u32;
        self.albums.push(AlbumMeta {
            title: title.to_owned(),
            album_artist,
            original_date,
            release_date,
            folder,
            disc_count: 1,
            mb_album_id: mb_album_id.to_owned(),
        });
        self.album_dirs.push(dir.to_owned());
        self.album_tracks.push(track_path.to_owned());
        self.album_ids.insert(key, id);
        id
    }

    /// Fold one track's disc facts into its album:
    /// `disc_count = max(max disc_total tag, max disc_no seen, 1)`.
    fn note_disc(&mut self, album: u32, disc_no: u16, disc_total: u16) {
        let a = &mut self.albums[album as usize];
        a.disc_count = a.disc_count.max(disc_no).max(disc_total).max(1);
    }
```

- [ ] **Step 6: `build_catalog`**

Replace the loop (from `for r in raw {` through the closing `}` after `tracks.push(TrackInput { … });`, `:448-483`) with:

```rust
    for r in raw {
        // The track's real folder (its playback locator) …
        let folder = interner.folder(r.path.parent().unwrap_or(root), root);
        // … vs. its album root (a disc folder folds into its parent).
        let (album_dir, disc_dir) = album_root(&r.path, root);
        let album_folder = interner.folder(album_dir, root);
        let artist = interner.artist(&r.artist_name, r.artist_sort.as_deref());
        let album_artist = interner.artist(
            &r.album_artist_name,
            r.album_artist_sort.as_deref().or(r.artist_sort.as_deref()),
        );
        let album = interner.album(
            &r.album_name,
            album_folder,
            &r.mb_album_id,
            album_artist,
            r.original_date,
            r.release_date,
            album_dir,
            &r.path,
        );
        // Disc: tag → disc-folder number → 1. Subtitle: tag → folder text → "".
        let disc_no = r.disc_no.or(disc_dir.map(|d| d.number)).unwrap_or(1);
        let disc_subtitle = if r.disc_subtitle.is_empty() {
            disc_dir.and_then(|d| d.subtitle).unwrap_or("").to_owned()
        } else {
            r.disc_subtitle.clone()
        };
        interner.note_disc(album, disc_no, r.disc_total);
        let genre = interner.genre(&r.genre_name);
        tracks.push(TrackInput {
            title: r.title.clone(),
            artist,
            album,
            album_artist,
            genre,
            folder,
            track_no: r.track_no,
            disc_no,
            duration_ms: r.duration_ms,
            sample_rate: r.sample_rate,
            channels: r.channels,
            file_name: r.file_name.clone(),
            mbid: r.mbid.clone(),
            disc_subtitle,
            // Loudness is a deferred Tier-2 analysis pass (full decode), not the metadata scan.
            loudness: None,
        });
    }
```

(The cover loop after it is unchanged: `album_dirs` now holds album roots, so a `Disc01/` track finds `Box/cover.jpg` directly. Interning `album_dir` adds no folder ids: the album root is an ancestor of the track folder and is already interned, so existing ids/bytes are unchanged.)

- [ ] **Step 7: Bump the scan cache**

`:554` → `pub const SCAN_CACHE_VERSION: u32 = 4;` and append to its doc comment: "v4 (multi-disc albums): `RawTrack` gained `disc_total`, `disc_subtitle`, `mb_album_id`; `disc_no` became `Option`."

- [ ] **Step 8: Run the import suite**

Run: `cargo test -p soul-audio-import && cargo test -p soul-audio-import --features serde`
Expected: all PASS — the 9 new pipeline tests and the existing pipeline/export/incremental/playback tests (no disc folders there, so their ids and baked bytes are unchanged), plus the serde compile check.

- [ ] **Step 9: Commit**

```bash
git add crates/soul-audio-import/
git commit -m "feat(import): album-root identity with disc inference, disc count and MB release id"
```

---

## Task 6: MultiCatalog — re-pack the v8 string refs (+ the `mbid` fix)

**Files:**
- Modify: `crates/soul-audio-multicatalog/src/lib.rs:356-389` (`track()`, `album()`)
- Test: `crates/soul-audio-multicatalog/tests/multicatalog.rs` (append)

**Interfaces:**
- Consumes: `TrackRec.disc_subtitle`, `AlbumRec.{disc_count, mb_album_id}` (T4).
- Produces: every `StrRef` the aggregate returns (`title`, `file_name`, `mbid`, `disc_subtitle`, album `title`, `mb_album_id`) carries its source in the high bits, so `resolve_str` routes it. `Head`/merge order unchanged (keys on album id + `(disc, track)`).

- [ ] **Step 1: Write the failing test**

Append to `crates/soul-audio-multicatalog/tests/multicatalog.rs`:

```rust
/// Multi-disc albums on two sources: each album run stays in (disc, track)
/// order through the aggregate, and the v8 per-disc subtitle, the album's disc
/// count + MusicBrainz release id, and the track MBID resolve against the OWNING
/// source (their StrRef offsets are re-packed like every other string).
#[test]
fn multi_disc_v8_fields_route_per_source() {
    fn source(prefix: &str, subs: [&str; 2], mb: &str) -> CatalogInput {
        let t = |title: String, disc: u16, track: u16, sub: &str| TrackInput {
            mbid: format!("{title}-mbid"),
            title,
            artist: 0,
            album: 0,
            album_artist: 0,
            genre: 0,
            folder: 1,
            track_no: track,
            disc_no: disc,
            duration_ms: 0,
            sample_rate: 44_100,
            channels: 2,
            file_name: format!("{disc}-{track}.flac"),
            disc_subtitle: sub.into(),
            loudness: None,
        };
        CatalogInput {
            // Deliberately out of (disc, track) order.
            tracks: vec![
                t(format!("{prefix}_d2t1"), 2, 1, subs[1]),
                t(format!("{prefix}_d1t2"), 1, 2, subs[0]),
                t(format!("{prefix}_d1t1"), 1, 1, subs[0]),
            ],
            artists: vec![format!("{prefix}_artist")],
            artist_sorts: Vec::new(),
            genres: vec!["Game".into()],
            albums: vec![AlbumMeta {
                title: "Jade Moon".into(),
                album_artist: 0,
                original_date: PackedDate::UNKNOWN,
                release_date: PackedDate::UNKNOWN,
                folder: 1,
                disc_count: 2,
                mb_album_id: mb.into(),
            }],
            folders: vec![
                FolderInput {
                    path: String::new(),
                    parent: NO_FOLDER,
                },
                FolderInput {
                    path: format!("{prefix}/Jade Moon"),
                    parent: 0,
                },
            ],
            playlists: Vec::new(),
            covers: Vec::new(),
        }
    }
    // Different string lengths per source, so an un-re-packed offset would land
    // on the wrong heap (wrong text, or a resolve error).
    let cases = [
        (
            "local",
            ["Mondstadt", "Liyue"],
            "aaaaaaaa-0000-4000-8000-000000000001",
        ),
        (
            "remote_server_copy",
            ["Inazuma Archipelago", "Sumeru Rainforest"],
            "bbbbbbbb-0000-4000-8000-000000000002",
        ),
    ];
    let multi = MultiCatalog::new(
        cases
            .iter()
            .map(|(p, subs, mb)| boxed_member(&source(p, *subs, mb)))
            .collect(),
    );
    assert_eq!(multi.album_count(), 2);
    for (s, (prefix, subs, mb)) in cases.iter().enumerate() {
        let album = pack(s as u32, 0);
        let rec = multi.album(album).unwrap();
        assert_eq!(rec.disc_count, 2);
        assert_eq!(resolve(&multi, rec.mb_album_id), *mb);
        let run: Vec<(String, String, String)> = (0..multi.album_track_count(album))
            .map(|i| {
                let t = multi.track(multi.album_track(album, i).unwrap()).unwrap();
                (
                    resolve(&multi, t.title),
                    resolve(&multi, t.disc_subtitle),
                    resolve(&multi, t.mbid),
                )
            })
            .collect();
        let row = |name: String, sub: &str| (name.clone(), sub.to_owned(), format!("{name}-mbid"));
        assert_eq!(
            run,
            vec![
                row(format!("{prefix}_d1t1"), subs[0]),
                row(format!("{prefix}_d1t2"), subs[0]),
                row(format!("{prefix}_d2t1"), subs[1]),
            ],
            "source {s}"
        );
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p soul-audio-multicatalog multi_disc_v8_fields_route_per_source`
Expected: FAIL — for source 1 the subtitle / MB id / MBID resolve against source 0's heap (wrong text, or `resolve`'s `expect("resolve_str")` panics on `Corrupt`).

- [ ] **Step 3: Re-pack the refs**

In `crates/soul-audio-multicatalog/src/lib.rs` `track()`, after `r.file_name.off = pack(s, r.file_name.off);`:

```rust
        r.mbid.off = pack(s, r.mbid.off);
        r.disc_subtitle.off = pack(s, r.disc_subtitle.off); // v8
```

and in `album()`, after `r.title.off = pack(s, r.title.off);`:

```rust
        r.mb_album_id.off = pack(s, r.mb_album_id.off); // v8
```

(An empty ref `(0, 0)` becomes `(s << 28, 0)`: `resolve_str` routes it to source `s` and reads zero bytes — still empty.)

- [ ] **Step 4: Run the suite**

Run: `cargo test -p soul-audio-multicatalog`
Expected: all PASS, including `merged_order_equals_union_bake`, `multi_disc_ordering_across_the_merge`, `routing` and the new test.

- [ ] **Step 5: Commit**

```bash
git add crates/soul-audio-multicatalog/
git commit -m "fix(multicatalog): re-pack mbid and v8 disc string refs per source"
```

---

## Task 7: Organize — disc tokens, empty-segment drop, disc-safe presets

**Files:**
- Modify: `crates/soul-audio-organize/src/template.rs` (module doc `:1-13`, `render` `:42-58`, `substitute` `:63-86`, `token_value` `:89-107`, `presets` `:133-147`, tests `:149-273`)
- Modify: `crates/soul-audio-organize/src/lib.rs:28` (export `preset`)

**Interfaces:**
- Produces: tokens `{DiscPrefix}`, `{DiscDir}`, `{DiscTotal}`; `pub fn preset(name: &str) -> Option<&'static str>`; the 7-entry `presets()` (Global Constraints); crate-private `fn render_inner(&self, tags: &Tags, src: &Path, neutral_disc_dir: bool) -> PathBuf`, `pub(crate) fn disc_label(disc: u32, disc_count: u32) -> String`, `pub(crate) fn disc_dir_name(disc: u32, disc_count: u32) -> String` (used by T8).
- Disc context comes from the tags: `disc_count = max(disc_total, disc, 1)` (missing disc → 1). T8's planner sets `tags.disc`/`tags.disc_total` to the album's effective values before rendering.

- [ ] **Step 1: Write the failing tests**

In `template.rs`'s test module change the import to `use super::{Template, preset, presets};` and append:

```rust
    fn disc_tags(disc: u32, total: Option<u32>) -> Tags {
        Tags {
            title: Some("T".into()),
            album: Some("Alb".into()),
            track: Some(5),
            disc: Some(disc),
            disc_total: total,
            ..Default::default()
        }
    }

    #[test]
    fn disc_prefix_is_empty_for_a_single_disc_album() {
        let t = Template::new("{DiscPrefix}{TrackNo} - {Title}");
        assert_eq!(
            t.render(&disc_tags(1, Some(1)), Path::new("x.flac")),
            expect(&["05 - T.flac"])
        );
        let untagged = Tags {
            title: Some("T".into()),
            track: Some(5),
            ..Default::default()
        };
        assert_eq!(
            t.render(&untagged, Path::new("x.flac")),
            expect(&["05 - T.flac"]),
            "untagged = single disc"
        );
    }

    #[test]
    fn disc_prefix_pads_to_the_disc_count_width() {
        let t = Template::new("{DiscPrefix}{TrackNo}");
        assert_eq!(t.render(&disc_tags(2, Some(3)), Path::new("x.flac")), expect(&["2-05.flac"]));
        assert_eq!(t.render(&disc_tags(3, Some(12)), Path::new("x.flac")), expect(&["03-05.flac"]));
        // No total tag: the count is inferred from the disc number itself.
        assert_eq!(t.render(&disc_tags(2, None), Path::new("x.flac")), expect(&["2-05.flac"]));
    }

    #[test]
    fn disc_dir_segment_is_dropped_for_a_single_disc_album() {
        let t = Template::new("{Album}/{DiscDir}/{TrackNo} - {Title}");
        assert_eq!(
            t.render(&disc_tags(1, Some(1)), Path::new("x.flac")),
            expect(&["Alb", "05 - T.flac"])
        );
        assert_eq!(
            t.render(&disc_tags(2, Some(2)), Path::new("x.flac")),
            expect(&["Alb", "Disc 2", "05 - T.flac"])
        );
        assert_eq!(
            t.render(&disc_tags(2, Some(10)), Path::new("x.flac")),
            expect(&["Alb", "Disc 02", "05 - T.flac"])
        );
    }

    #[test]
    fn disc_total_token_is_the_disc_count() {
        let t = Template::new("{Disc} of {DiscTotal}");
        assert_eq!(t.render(&disc_tags(2, Some(3)), Path::new("x.flac")), expect(&["2 of 3.flac"]));
        assert_eq!(t.render(&Tags::default(), Path::new("x.flac")), expect(&["1 of 1.flac"]));
    }

    #[test]
    fn empty_directory_segments_are_dropped_but_an_empty_file_name_is_not() {
        let tags = Tags {
            album: Some("Alb".into()),
            title: Some("T".into()),
            ..Default::default()
        };
        // `{Year}` with no date renders empty → the directory level disappears.
        assert_eq!(
            Template::new("{Year}/{Album}/{Title}").render(&tags, Path::new("x.flac")),
            expect(&["Alb", "T.flac"])
        );
        // The final (file-name) segment still sanitizes to `_`.
        assert_eq!(
            Template::new("{Album}/{Year}").render(&tags, Path::new("x.flac")),
            expect(&["Alb", "_.flac"])
        );
    }

    #[test]
    fn presets_are_byte_identical_to_the_old_ones_for_single_disc_albums() {
        let old = [
            ("artist-album", "{AlbumArtist}/{Album}/{TrackNo} - {Title}"),
            ("artist-year-album", "{AlbumArtist}/{Year} - {Album}/{TrackNo} - {Title}"),
            ("genre-artist-album", "{Genre}/{AlbumArtist}/{Album}/{TrackNo} - {Title}"),
            ("flat", "{Artist} - {Title}"),
        ];
        let single = Tags {
            title: Some("Hells Bells".into()),
            artist: Some("AC/DC".into()),
            album: Some("Back in Black".into()),
            date: Some("1980".into()),
            genre: Some("Rock".into()),
            track: Some(1),
            disc: Some(1),
            disc_total: Some(1),
            ..Default::default()
        };
        let src = Path::new("x.flac");
        for (name, old_tpl) in old {
            let want = Template::new(old_tpl).render(&single, src);
            assert_eq!(Template::new(preset(name).unwrap()).render(&single, src), want, "{name}");
            if name != "flat" {
                let df = preset(&format!("{name}+disc-folders")).unwrap();
                assert_eq!(Template::new(df).render(&single, src), want, "{name}+disc-folders");
            }
        }
    }

    #[test]
    fn multi_disc_presets_render_both_layouts() {
        let tags = Tags {
            title: Some("Intro".into()),
            album_artist: Some("HOYO-MiX".into()),
            album: Some("Box".into()),
            track: Some(1),
            disc: Some(2),
            disc_total: Some(2),
            ..Default::default()
        };
        let src = Path::new("x.flac");
        assert_eq!(
            Template::new(preset("artist-album").unwrap()).render(&tags, src),
            expect(&["HOYO-MiX", "Box", "2-01 - Intro.flac"])
        );
        assert_eq!(
            Template::new(preset("artist-album+disc-folders").unwrap()).render(&tags, src),
            expect(&["HOYO-MiX", "Box", "Disc 2", "01 - Intro.flac"])
        );
        assert_eq!(preset("no-such-preset"), None);
    }
```

and replace `presets_catalog_is_exact` with:

```rust
    #[test]
    fn presets_catalog_is_exact() {
        assert_eq!(
            presets(),
            &[
                ("artist-album", "{AlbumArtist}/{Album}/{DiscPrefix}{TrackNo} - {Title}"),
                ("artist-album+disc-folders", "{AlbumArtist}/{Album}/{DiscDir}/{TrackNo} - {Title}"),
                (
                    "artist-year-album",
                    "{AlbumArtist}/{Year} - {Album}/{DiscPrefix}{TrackNo} - {Title}"
                ),
                (
                    "artist-year-album+disc-folders",
                    "{AlbumArtist}/{Year} - {Album}/{DiscDir}/{TrackNo} - {Title}"
                ),
                (
                    "genre-artist-album",
                    "{Genre}/{AlbumArtist}/{Album}/{DiscPrefix}{TrackNo} - {Title}"
                ),
                (
                    "genre-artist-album+disc-folders",
                    "{Genre}/{AlbumArtist}/{Album}/{DiscDir}/{TrackNo} - {Title}"
                ),
                ("flat", "{Artist} - {Title}"),
            ]
        );
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p soul-audio-organize template`
Expected: FAIL to compile (`preset` not found); then the disc-token tests fail (unknown tokens render empty; empty segments become `_`).

- [ ] **Step 3: Implement**

In `template.rs`:

1. Module doc — replace the `# Tokens` paragraph with:
   ```rust
   //! # Tokens
   //! The supported tokens are, case-sensitive and exact: `{AlbumArtist}`,
   //! `{Artist}`, `{Album}`, `{Year}`, `{Disc}`, `{DiscPrefix}`, `{DiscDir}`,
   //! `{DiscTotal}`, `{TrackNo}`, `{Title}`, `{Genre}`. An **unknown** `{...}`
   //! token renders as the empty string.
   //!
   //! # Disc tokens
   //! The album's disc count is `max(disc_total, disc, 1)` from the tags (the
   //! planner sets both to the album's effective values). `{DiscPrefix}` is `""`
   //! for a single-disc album, else the disc number zero-padded to the width of
   //! the disc count plus `-` (`1-`, `01-` for ≥ 10 discs) — so
   //! `{DiscPrefix}{TrackNo}` is `1-05`. `{DiscDir}` is `""` for a single-disc
   //! album, else `Disc N` (same padding). `{DiscTotal}` is the disc count.
   //!
   //! # Empty segments
   //! A **directory** segment that renders empty (or whitespace) — `{DiscDir}` of
   //! a single-disc album, `{Year}` without a date — is dropped, not turned into
   //! `_`, so `{Album}/{DiscDir}/{TrackNo}` gives `Album/05` for one disc and
   //! `Album/Disc 1/05` for several. The file-name segment still becomes `_`.
   ```
2. Replace `render` with:
   ```rust
       pub fn render(&self, tags: &Tags, src: &Path) -> PathBuf {
           self.render_inner(tags, src, false)
       }

       /// [`render`](Self::render), optionally forcing `{DiscDir}` empty (the
       /// planner uses that to find an album's own folder).
       fn render_inner(&self, tags: &Tags, src: &Path, neutral_disc_dir: bool) -> PathBuf {
           let mut path = PathBuf::new();
           let mut segments = self.raw.split('/').peekable();
           while let Some(template_segment) = segments.next() {
               let substituted = substitute(template_segment, tags, src, neutral_disc_dir);
               let is_file_name = segments.peek().is_none();
               if !is_file_name && substituted.trim().is_empty() {
                   continue; // an empty directory level is dropped, not `_`
               }
               path.push(sanitize_segment(&substituted));
           }
           // Preserve the source extension (original case) by appending it to the
           // final component — `set_extension` would mis-handle a name with dots.
           if let Some(ext) = src.extension() {
               let mut os = path.into_os_string();
               os.push(".");
               os.push(ext);
               path = PathBuf::from(os);
           }
           path
       }
   ```
3. `substitute` gains the flag: signature `fn substitute(segment: &str, tags: &Tags, src: &Path, neutral_disc_dir: bool) -> String`, and its call becomes `token_value(&after[..close], tags, src, neutral_disc_dir)`.
4. `token_value` gains the flag (`fn token_value(name: &str, tags: &Tags, src: &Path, neutral_disc_dir: bool) -> Option<String>`) and three arms after `"Disc" => …`:
   ```rust
           "DiscPrefix" => {
               let (disc, count) = disc_and_count(tags);
               if count > 1 {
                   format!("{}-", disc_label(disc, count))
               } else {
                   String::new()
               }
           }
           "DiscDir" => {
               let (disc, count) = disc_and_count(tags);
               if count > 1 && !neutral_disc_dir {
                   disc_dir_name(disc, count)
               } else {
                   String::new()
               }
           }
           "DiscTotal" => disc_and_count(tags).1.to_string(),
   ```
5. Add after `token_value`:
   ```rust
   /// `(disc, disc_count)` for the disc tokens: the disc number (missing or 0 →
   /// 1) and `max(disc_total, disc, 1)`.
   fn disc_and_count(tags: &Tags) -> (u32, u32) {
       let disc = tags.disc.filter(|&d| d > 0).unwrap_or(1);
       (disc, tags.disc_total.unwrap_or(0).max(disc))
   }

   /// `disc` zero-padded to the digit width of `disc_count` (`2` of 3 → `"2"`,
   /// of 12 → `"02"`).
   pub(crate) fn disc_label(disc: u32, disc_count: u32) -> String {
       let width = disc_count.max(disc).to_string().len();
       format!("{disc:0width$}")
   }

   /// The `{DiscDir}` folder name: `Disc N`, padded like [`disc_label`].
   pub(crate) fn disc_dir_name(disc: u32, disc_count: u32) -> String {
       format!("Disc {}", disc_label(disc, disc_count))
   }
   ```
6. Replace `presets` and add `preset`:
   ```rust
   /// Built-in named templates: `(name, template)`. Every album preset is
   /// disc-safe (`{DiscPrefix}` — identical output for single-disc albums) and
   /// has a `+disc-folders` twin that puts each disc in a `Disc N/` folder.
   pub fn presets() -> &'static [(&'static str, &'static str)] {
       &[
           ("artist-album", "{AlbumArtist}/{Album}/{DiscPrefix}{TrackNo} - {Title}"),
           ("artist-album+disc-folders", "{AlbumArtist}/{Album}/{DiscDir}/{TrackNo} - {Title}"),
           (
               "artist-year-album",
               "{AlbumArtist}/{Year} - {Album}/{DiscPrefix}{TrackNo} - {Title}",
           ),
           (
               "artist-year-album+disc-folders",
               "{AlbumArtist}/{Year} - {Album}/{DiscDir}/{TrackNo} - {Title}",
           ),
           (
               "genre-artist-album",
               "{Genre}/{AlbumArtist}/{Album}/{DiscPrefix}{TrackNo} - {Title}",
           ),
           (
               "genre-artist-album+disc-folders",
               "{Genre}/{AlbumArtist}/{Album}/{DiscDir}/{TrackNo} - {Title}",
           ),
           ("flat", "{Artist} - {Title}"),
       ]
   }

   /// Look up a built-in template by name (e.g. `"artist-album+disc-folders"`).
   pub fn preset(name: &str) -> Option<&'static str> {
       presets().iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
   }
   ```

In `crates/soul-audio-organize/src/lib.rs:28`: `pub use template::{Template, preset, presets};`

- [ ] **Step 4: Run the organize suite**

Run: `cargo test -p soul-audio-organize`
Expected: all PASS — the new template tests and every existing plan/apply/roundtrip test (they use `presets()[0]` = `artist-album`, whose single-disc output is unchanged).

- [ ] **Step 5: Commit**

```bash
git add crates/soul-audio-organize/src/template.rs crates/soul-audio-organize/src/lib.rs
git commit -m "feat(organize): disc tokens, empty-segment drop and disc-safe presets"
```

---

## Task 8: Organize — album-aware planning + side files

**Files:**
- Create: `crates/soul-audio-organize/src/album.rs`, `crates/soul-audio-organize/src/side.rs`
- Modify: `crates/soul-audio-organize/src/plan.rs` (everything above `#[cfg(test)]`, `:1-140`)
- Modify: `crates/soul-audio-organize/src/template.rs` (two crate-private helpers)
- Modify: `crates/soul-audio-organize/src/lib.rs` (modules, exports, crate doc)
- Create: `crates/soul-audio-organize/tests/support/mod.rs`, `crates/soul-audio-organize/tests/side_files.rs`

**Interfaces:**
- Consumes: `soul_audio_tags::disc_folder::parse` (T1), `Tags.musicbrainz_album_id` + split disc tags (T2/T3), `Template` disc tokens, `disc_label`, `disc_dir_name`, `render_inner` (T7).
- Produces (public):
  - `pub enum EntryKind { Audio, SideFile }`; `PlanEntry { src, dst, action, pub kind: EntryKind }`.
  - `pub enum SideFilesLeft { SharedAlbumRoot, NoAlbumFolder, AtSourceRoot }`; `pub struct LeftInPlace { pub album_root: PathBuf, pub reason: SideFilesLeft, pub files: usize }`.
  - `OrganizePlan { dest_root, entries, pub side_files_left: Vec<LeftInPlace>, pub prune_dirs: Vec<PathBuf> }`.
  - `#[derive(Clone, Debug, Default, PartialEq, Eq)] pub struct PlanOptions { pub source_root: Option<PathBuf> }`.
  - `pub fn plan_with(files: &[PathBuf], dest_root: &Path, template: &Template, strategy: Strategy, opts: &PlanOptions) -> OrganizePlan`; `plan(…)` = `plan_with(…, &PlanOptions::default())`.
  - Entry order: audio in input order, then side files album by album (sorted per album).
- Produces (crate-private): `album::{group, album_root, RootIndex, TrackInfo, AlbumGroup}`, `side::{collect, place, AlbumPlacement, is_audio, is_junk}`, `Template::{render_album_dir, has_disc_dir}`.

- [ ] **Step 1: Test support module (real tag writers)**

Create `crates/soul-audio-organize/tests/support/mod.rs`:

```rust
//! Shared helpers for the organize integration tests: unique temp dirs, tiny
//! REAL tag writers (FLAC STREAMINFO + VORBIS_COMMENT; ID3v2.3 MP3) and tree
//! listing. (`soul-audio-tags`' `testfix` is crate-private, so these live here.)
#![allow(dead_code)] // each test binary uses a subset

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh, unique temp dir (manual, per repo convention — no `tempfile`).
pub fn unique_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("organize-it-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `root` joined with a `/`-separated relative path, component by component
/// (no mixed separators on Windows).
pub fn at(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
    }
    p
}

/// Write `bytes` at `root/rel`, creating parent folders. Returns the path.
pub fn put(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let p = at(root, rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, bytes).unwrap();
    p
}

/// `path` relative to `base`, `/`-separated.
pub fn rel(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or_else(|_| panic!("{path:?} is not under {base:?}"))
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Every file under `root` (hidden ones included), relative and `/`-separated.
pub fn tree(root: &Path) -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(rel(root, &p));
            }
        }
    }
    let mut out = BTreeSet::new();
    if root.exists() {
        walk(root, root, &mut out);
    }
    out
}

/// The `.flac`/`.mp3` files under `root` (not hidden), sorted — the organizer's
/// input list.
pub fn audio_files(root: &Path) -> Vec<PathBuf> {
    tree(root)
        .into_iter()
        .filter(|r| {
            let name = r.rsplit('/').next().unwrap_or(r);
            !name.starts_with('.') && (r.ends_with(".flac") || r.ends_with(".mp3"))
        })
        .map(|r| at(root, &r))
        .collect()
}

/// A minimal real FLAC: `fLaC` + STREAMINFO (44.1 kHz stereo, 2 s) + a LAST
/// VORBIS_COMMENT block with `comments` (`KEY=VALUE`). No audio frames — enough
/// for every tag reader in the workspace.
pub fn flac(comments: &[&str]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"fLaC");
    v.push(0x00); // STREAMINFO, not last
    v.extend_from_slice(&[0, 0, 34]);
    let mut si = [0u8; 34];
    let (sr, ch, bps, total): (u32, u32, u32, u64) = (44_100, 2, 16, 88_200);
    si[10] = (sr >> 12) as u8;
    si[11] = (sr >> 4) as u8;
    si[12] = (((sr & 0xF) as u8) << 4) | ((((ch - 1) & 0x7) as u8) << 1) | (((bps - 1) >> 4) as u8);
    si[13] = ((((bps - 1) & 0xF) as u8) << 4) | (((total >> 32) & 0xF) as u8);
    si[14] = (total >> 24) as u8;
    si[15] = (total >> 16) as u8;
    si[16] = (total >> 8) as u8;
    si[17] = total as u8;
    v.extend_from_slice(&si);
    let mut vc = Vec::new();
    vc.extend_from_slice(&6u32.to_le_bytes());
    vc.extend_from_slice(b"vendor");
    vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
        vc.extend_from_slice(c.as_bytes());
    }
    v.push(0x84); // VORBIS_COMMENT, last block
    v.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]);
    v.extend_from_slice(&vc);
    v
}

/// A minimal real MP3: an ID3v2.3 tag of UTF-8 text frames `(id, value)` (e.g.
/// `("TIT2", "Solo")`, `("TRCK", "1/1")`) followed by 64 zero bytes standing in
/// for audio (no MPEG sync, so the facts scan finds nothing).
pub fn mp3(frames: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, value) in frames {
        assert_eq!(id.len(), 4, "ID3v2.3 frame ids are 4 chars");
        body.extend_from_slice(id.as_bytes());
        body.extend_from_slice(&(1 + value.len() as u32).to_be_bytes());
        body.extend_from_slice(&[0, 0]); // flags
        body.push(0x03); // UTF-8
        body.extend_from_slice(value.as_bytes());
    }
    let n = body.len() as u32;
    let mut v = b"ID3".to_vec();
    v.extend_from_slice(&[3, 0, 0]); // v2.3, no flags
    v.extend_from_slice(&[
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]);
    v.extend_from_slice(&body);
    v.extend_from_slice(&[0u8; 64]);
    v
}
```

- [ ] **Step 2: Write the failing planner tests**

Create `crates/soul-audio-organize/tests/side_files.rs`:

```rust
//! Planner + executor integration tests for multi-disc albums and side files,
//! over real tagged FLAC fixtures written into temp dirs.

mod support;

use std::collections::BTreeSet;
use std::path::Path;

use soul_audio_organize::{
    Action, EntryKind, LeftInPlace, OrganizePlan, PlanOptions, SideFilesLeft, Strategy, Template,
    apply, plan, plan_with, preset,
};
use support::{audio_files, flac, put, rel, tree, unique_dir};

fn tpl(name: &str) -> Template {
    Template::new(preset(name).expect("preset"))
}

/// A tagged FLAC track of `album` by HOYO-MiX (`disc` = `DISCNUMBER` value).
fn track(album: &str, title: &str, n: u32, disc: Option<&str>) -> Vec<u8> {
    let mut c = vec![
        format!("ALBUM={album}"),
        format!("TITLE={title}"),
        "ARTIST=HOYO-MiX".to_owned(),
        "ALBUMARTIST=HOYO-MiX".to_owned(),
        format!("TRACKNUMBER={n}"),
    ];
    if let Some(d) = disc {
        c.push(format!("DISCNUMBER={d}"));
    }
    let refs: Vec<&str> = c.iter().map(String::as_str).collect();
    flac(&refs)
}

/// The plan's destinations of one kind, relative to `dest`, `/`-separated.
fn dsts(p: &OrganizePlan, dest: &Path, kind: EntryKind) -> BTreeSet<String> {
    p.entries
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| rel(dest, &e.dst))
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn opts(src: &Path) -> PlanOptions {
    PlanOptions {
        source_root: Some(src.to_path_buf()),
    }
}

#[test]
fn disc_two_track_one_no_longer_collides_with_disc_one_track_one() {
    let (src, dest) = (unique_dir("collide-src"), unique_dir("collide-dest"));
    put(&src, "Box/101 Intro.flac", &track("Box", "Intro", 1, Some("1/2")));
    put(&src, "Box/201 Intro.flac", &track("Box", "Intro", 1, Some("2/2")));
    let p = plan_with(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Move, &opts(&src));
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&["HOYO-MiX/Box/1-01 - Intro.flac", "HOYO-MiX/Box/2-01 - Intro.flac"])
    );
    assert!(
        p.entries.iter().all(|e| e.action == Action::Move),
        "no SkipDuplicate data loss: {:#?}",
        p.entries
    );
}

#[test]
fn single_disc_album_names_are_unchanged() {
    let (src, dest) = (unique_dir("single-src"), unique_dir("single-dest"));
    put(&src, "Single/01.flac", &track("Single", "Solo", 1, None));
    let p = plan(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Copy);
    assert_eq!(
        dsts(&p, &dest, EntryKind::Audio),
        set(&["HOYO-MiX/Single/01 - Solo.flac"])
    );
}

#[test]
fn side_files_travel_with_the_album_and_junk_stays() {
    let (src, dest) = (unique_dir("side-src"), unique_dir("side-dest"));
    put(&src, "Box/101 Intro.flac", &track("Box", "Intro", 1, Some("1/2")));
    put(&src, "Box/201 Reprise.flac", &track("Box", "Reprise", 1, Some("2/2")));
    for (f, b) in [
        ("Box/Box.log", &b"EAC log"[..]),
        ("Box/Box.cue", b"FILE \"101 Intro.flac\" WAVE"),
        ("Box/cover.jpg", b"jpg"),
        ("Box/Scans/Front.jpg", b"front"),
        ("Box/info.txt", b"notes"),
        ("Box/Thumbs.db", b"junk"),
        ("Box/desktop.ini", b"junk"),
        ("Box/.DS_Store", b"junk"),
        ("Box/._101 Intro.flac", b"appledouble"),
    ] {
        put(&src, f, b);
    }
    let p = plan_with(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Copy, &opts(&src));
    assert_eq!(
        dsts(&p, &dest, EntryKind::SideFile),
        set(&[
            "HOYO-MiX/Box/Box.cue",
            "HOYO-MiX/Box/Box.log",
            "HOYO-MiX/Box/Scans/Front.jpg",
            "HOYO-MiX/Box/cover.jpg",
            "HOYO-MiX/Box/info.txt",
        ])
    );
    assert!(p.side_files_left.is_empty());
    // Audio first (input order), then the side files.
    let first_side = p
        .entries
        .iter()
        .position(|e| e.kind == EntryKind::SideFile)
        .unwrap();
    assert_eq!(first_side, 2);
    assert!(p.entries[first_side..].iter().all(|e| e.kind == EntryKind::SideFile));
}

#[test]
fn lyrics_follow_their_track_and_take_its_new_name() {
    let (src, dest) = (unique_dir("lrc-src"), unique_dir("lrc-dest"));
    put(&src, "Box/101 Intro.flac", &track("Box", "Intro", 1, Some("1/2")));
    put(&src, "Box/201 Reprise.flac", &track("Box", "Reprise", 1, Some("2/2")));
    put(&src, "Box/101 Intro.lrc", b"[00:00.00]Intro");
    put(&src, "Box/201 Reprise.LRC", b"[00:00.00]Reprise");
    let files = audio_files(&src);
    let one = plan_with(&files, &dest, &tpl("artist-album"), Strategy::Copy, &opts(&src));
    assert_eq!(
        dsts(&one, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Box/1-01 - Intro.lrc", "HOYO-MiX/Box/2-01 - Reprise.LRC"])
    );
    let df = plan_with(&files, &dest, &tpl("artist-album+disc-folders"), Strategy::Copy, &opts(&src));
    assert_eq!(
        dsts(&df, &dest, EntryKind::SideFile),
        set(&["HOYO-MiX/Box/Disc 1/01 - Intro.lrc", "HOYO-MiX/Box/Disc 2/01 - Reprise.LRC"])
    );
}

#[test]
fn disc_folder_side_files_get_a_disc_prefix_only_when_they_collide() {
    let (src, dest) = (unique_dir("prefix-src"), unique_dir("prefix-dest"));
    put(&src, "Box/CD1/01 Intro.flac", &track("Box", "Intro", 1, Some("1")));
    put(&src, "Box/CD2/01 Reprise.flac", &track("Box", "Reprise", 1, Some("2")));
    put(&src, "Box/CD1/Box.log", b"disc 1 log");
    put(&src, "Box/CD2/Box.log", b"disc 2 log!");
    put(&src, "Box/CD2/notes.txt", b"notes");
    let files = audio_files(&src);
    let one = plan_with(&files, &dest, &tpl("artist-album"), Strategy::Copy, &opts(&src));
    assert_eq!(
        dsts(&one, &dest, EntryKind::SideFile),
        set(&[
            "HOYO-MiX/Box/Disc 1 - Box.log",
            "HOYO-MiX/Box/Disc 2 - Box.log",
            "HOYO-MiX/Box/notes.txt",
        ])
    );
    let df = plan_with(&files, &dest, &tpl("artist-album+disc-folders"), Strategy::Copy, &opts(&src));
    assert_eq!(
        dsts(&df, &dest, EntryKind::SideFile),
        set(&[
            "HOYO-MiX/Box/Disc 1/Box.log",
            "HOYO-MiX/Box/Disc 2/Box.log",
            "HOYO-MiX/Box/Disc 2/notes.txt",
        ])
    );
}

#[test]
fn two_albums_in_one_folder_leave_side_files_in_place() {
    let (src, dest) = (unique_dir("shared-src"), unique_dir("shared-dest"));
    put(&src, "Dump/01.flac", &track("A", "a", 1, None));
    put(&src, "Dump/02.flac", &track("B", "b", 1, None));
    put(&src, "Dump/cover.jpg", b"whose cover?");
    let p = plan_with(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Copy, &opts(&src));
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert_eq!(
        p.side_files_left,
        vec![LeftInPlace {
            album_root: src.join("Dump"),
            reason: SideFilesLeft::SharedAlbumRoot,
            files: 1,
        }]
    );
}

#[test]
fn a_nested_album_makes_the_outer_root_ambiguous() {
    let (src, dest) = (unique_dir("nested-src"), unique_dir("nested-dest"));
    put(&src, "Artist/01.flac", &track("Loose", "l", 1, None));
    put(&src, "Artist/Y/01.flac", &track("Y", "y", 1, None));
    put(&src, "Artist/artist.jpg", b"artist photo");
    put(&src, "Artist/Y/cover.jpg", b"y cover");
    let p = plan_with(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Copy, &opts(&src));
    assert_eq!(dsts(&p, &dest, EntryKind::SideFile), set(&["HOYO-MiX/Y/cover.jpg"]));
    assert_eq!(
        p.side_files_left,
        vec![LeftInPlace {
            album_root: src.join("Artist"),
            reason: SideFilesLeft::SharedAlbumRoot,
            files: 2,
        }]
    );
}

#[test]
fn loose_tracks_in_the_source_root_never_sweep_it() {
    let (src, dest) = (unique_dir("loose-src"), unique_dir("loose-dest"));
    put(&src, "01.flac", &track("Loose", "l", 1, None));
    put(&src, "tax-return.pdf", b"not album material");
    let p = plan_with(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Copy, &opts(&src));
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert_eq!(
        p.side_files_left,
        vec![LeftInPlace {
            album_root: src.clone(),
            reason: SideFilesLeft::AtSourceRoot,
            files: 1,
        }]
    );
}

#[test]
fn flat_layout_leaves_side_files_in_place() {
    let (src, dest) = (unique_dir("flat-src"), unique_dir("flat-dest"));
    put(&src, "Box/101 Intro.flac", &track("Box", "Intro", 1, Some("1/2")));
    put(&src, "Box/cover.jpg", b"jpg");
    let p = plan_with(&audio_files(&src), &dest, &tpl("flat"), Strategy::Copy, &opts(&src));
    assert!(dsts(&p, &dest, EntryKind::SideFile).is_empty());
    assert_eq!(
        p.side_files_left,
        vec![LeftInPlace {
            album_root: src.join("Box"),
            reason: SideFilesLeft::NoAlbumFolder,
            files: 1,
        }]
    );
}

#[test]
fn a_side_file_never_overwrites_a_different_file() {
    let (src, dest) = (unique_dir("conflict-src"), unique_dir("conflict-dest"));
    put(&src, "Single/01.flac", &track("Single", "Solo", 1, None));
    put(&src, "Single/cover.jpg", b"new cover");
    let theirs = put(&dest, "HOYO-MiX/Single/cover.jpg", b"someone else's, longer cover");
    let p = plan_with(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Copy, &opts(&src));
    let cover = p.entries.iter().find(|e| e.kind == EntryKind::SideFile).unwrap();
    assert_eq!(cover.action, Action::SkipConflict);
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty());
    assert_eq!(std::fs::read(&theirs).unwrap(), b"someone else's, longer cover");
}

#[test]
fn copy_rerun_is_all_skip_duplicate_and_side_files_are_byte_identical() {
    let (src, dest) = (unique_dir("rerun-src"), unique_dir("rerun-dest"));
    put(&src, "Box/CD1/01 Intro.flac", &track("Box", "Intro", 1, Some("1")));
    put(&src, "Box/CD2/01 Reprise.flac", &track("Box", "Reprise", 1, Some("2")));
    put(&src, "Box/CD1/Box.log", b"Log checksum 0xDEADBEEF");
    put(&src, "Box/cover.jpg", b"jpg");
    let files = audio_files(&src);
    let p = plan_with(&files, &dest, &tpl("artist-album"), Strategy::Copy, &opts(&src));
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    for e in &p.entries {
        assert_eq!(std::fs::read(&e.src).unwrap(), std::fs::read(&e.dst).unwrap(), "{:?}", e.dst);
    }
    let again = plan_with(&files, &dest, &tpl("artist-album"), Strategy::Copy, &opts(&src));
    assert_eq!(again.entries.len(), p.entries.len());
    assert!(
        again.entries.iter().all(|e| e.action == Action::SkipDuplicate),
        "{:#?}",
        again.entries
    );
    assert!(tree(&dest).contains("HOYO-MiX/Box/Box.log"));
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p soul-audio-organize --test side_files`
Expected: FAIL to compile — `EntryKind`, `LeftInPlace`, `PlanOptions`, `SideFilesLeft`, `plan_with` are not exported; `PlanEntry` has no `kind`.

- [ ] **Step 4: `album.rs` (grouping)**

Create `crates/soul-audio-organize/src/album.rs`:

```rust
//! Album grouping for the planner — the importer's identity rule (album root
//! folder + album title + MusicBrainz release id; a disc folder folds into its
//! parent via `soul_audio_tags::disc_folder`), re-stated here because organize
//! must not depend on import. An organize run and a later re-import therefore
//! agree on what an album is (`tests/multi_disc_e2e.rs` asserts it).

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::path::{Path, PathBuf};

use soul_audio_tags::{Tags, disc_folder};

use crate::tags::read_tags;

/// One input track after grouping.
pub(crate) struct TrackInfo {
    /// The file's tags with `disc` set to the effective disc number (tag, else
    /// the disc folder's number, else 1) and `disc_total` set to the album's
    /// `disc_count` — so the disc tokens render with the album's context.
    pub(crate) tags: Tags,
    /// Index into the album list returned alongside.
    pub(crate) album: usize,
}

/// One album: its source root, disc count, and member tracks (input indices).
pub(crate) struct AlbumGroup {
    pub(crate) root: PathBuf,
    pub(crate) disc_count: u32,
    pub(crate) tracks: Vec<usize>,
}

/// `(album root, disc-folder number)` for `src`: its parent folder, or the
/// grandparent when the parent's name is a disc folder strictly below
/// `source_root` (when given). Mirrors `soul-audio-import`'s `album_root`.
pub(crate) fn album_root(src: &Path, source_root: Option<&Path>) -> (PathBuf, Option<u32>) {
    let parent = src.parent().unwrap_or(Path::new(""));
    let below_root = source_root.is_none_or(|r| parent != r && parent.starts_with(r));
    if below_root
        && let Some(df) = parent
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(disc_folder::parse)
        && let Some(grandparent) = parent.parent()
    {
        return (grandparent.to_path_buf(), Some(u32::from(df.number)));
    }
    (parent.to_path_buf(), None)
}

/// Read every input's tags and group them into albums (first-seen order).
pub(crate) fn group(
    files: &[PathBuf],
    source_root: Option<&Path>,
) -> (Vec<TrackInfo>, Vec<AlbumGroup>) {
    let mut albums: Vec<AlbumGroup> = Vec::new();
    let mut ids: HashMap<(PathBuf, String, String), usize> = HashMap::new();
    let mut infos: Vec<TrackInfo> = Vec::with_capacity(files.len());
    for (i, src) in files.iter().enumerate() {
        let mut tags = read_tags(src);
        let (root, folder_disc) = album_root(src, source_root);
        let title = tags
            .album
            .clone()
            .unwrap_or_else(|| "Unknown Album".to_owned());
        let mb = tags
            .musicbrainz_album_id
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .to_owned();
        let key = (root.clone(), title, mb);
        let album = match ids.get(&key) {
            Some(&a) => a,
            None => {
                albums.push(AlbumGroup {
                    root,
                    disc_count: 1,
                    tracks: Vec::new(),
                });
                ids.insert(key, albums.len() - 1);
                albums.len() - 1
            }
        };
        let disc = tags.disc.filter(|&d| d > 0).or(folder_disc).unwrap_or(1);
        let group = &mut albums[album];
        group.disc_count = group
            .disc_count
            .max(disc)
            .max(tags.disc_total.unwrap_or(0));
        group.tracks.push(i);
        tags.disc = Some(disc);
        infos.push(TrackInfo { tags, album });
    }
    for info in &mut infos {
        info.tags.disc_total = Some(albums[info.album].disc_count);
    }
    (infos, albums)
}

/// Album roots, for the side-file rule "the album root holds exactly one album".
pub(crate) struct RootIndex {
    counts: BTreeMap<PathBuf, usize>,
}

impl RootIndex {
    pub(crate) fn new(albums: &[AlbumGroup]) -> Self {
        let mut counts = BTreeMap::new();
        for a in albums {
            *counts.entry(a.root.clone()).or_insert(0) += 1;
        }
        Self { counts }
    }

    /// Whether `root` is ambiguous: another album shares it, or another album's
    /// root is nested inside it. `Path` orders component-wise, so a nested root
    /// sorts immediately after `root`.
    pub(crate) fn is_shared(&self, root: &Path) -> bool {
        if self.counts.get(root).is_some_and(|&n| n > 1) {
            return true;
        }
        self.counts
            .range::<Path, _>((Bound::Excluded(root), Bound::Unbounded))
            .next()
            .is_some_and(|(next, _)| next.starts_with(root))
    }
}

#[cfg(test)]
mod tests {
    use super::{AlbumGroup, RootIndex, album_root};
    use std::path::{Path, PathBuf};

    #[test]
    fn disc_folder_parent_climbs_to_the_album_root() {
        let lib = Some(Path::new("lib"));
        assert_eq!(
            album_root(Path::new("lib/Box/CD2/01.flac"), lib),
            (PathBuf::from("lib/Box"), Some(2))
        );
        assert_eq!(
            album_root(Path::new("lib/Box/01.flac"), lib),
            (PathBuf::from("lib/Box"), None)
        );
        assert_eq!(
            album_root(Path::new("x/CD1/01.flac"), None).0,
            PathBuf::from("x"),
            "no source root: the rule still applies"
        );
    }

    #[test]
    fn never_climbs_out_of_the_source_root() {
        assert_eq!(
            album_root(Path::new("CD1/01.flac"), Some(Path::new("CD1"))),
            (PathBuf::from("CD1"), None)
        );
    }

    #[test]
    fn shared_and_nested_roots_are_ambiguous() {
        let g = |r: &str| AlbumGroup {
            root: PathBuf::from(r),
            disc_count: 1,
            tracks: Vec::new(),
        };
        let idx = RootIndex::new(&[
            g("a/Dump"),
            g("a/Dump"),
            g("a/Artist"),
            g("a/Artist/Album"),
            g("a/Artist Two"),
        ]);
        assert!(idx.is_shared(Path::new("a/Dump")));
        assert!(idx.is_shared(Path::new("a/Artist")), "a nested album root");
        assert!(!idx.is_shared(Path::new("a/Artist/Album")));
        assert!(!idx.is_shared(Path::new("a/Artist Two")));
    }
}
```

- [ ] **Step 5: `side.rs` (discovery + placement)**

Create `crates/soul-audio-organize/src/side.rs`:

```rust
//! Side files: the non-audio files that travel with an album — rip logs and cue
//! sheets, covers, `Scans/`, lyrics, `.txt`/`.nfo`. Discovery walks the album
//! root; placement decides each file's destination in the album's new folder.
//! Side files are never opened or modified, only copied/moved (a `.log`'s
//! checksum must stay valid). `.cue` `FILE` lines are NOT rewritten when tracks
//! are renamed (documented limitation).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use soul_audio_tags::disc_folder;

use crate::template::{disc_dir_name, disc_label};

/// Audio extensions (lower-case): `soul-audio-import`'s `SUPPORTED` +
/// `KNOWN_AUDIO_WE_DO_NOT_DECODE`, kept in sync by hand (organize must not
/// depend on import). A file with one of these is never a side file.
pub(crate) const AUDIO_EXTS: &[&str] = &[
    "wav", "flac", "mp3", "mp2", "mp1", "m4a", "aac", "ogg", "oga", "opus", "aiff", "aif", "dsf",
    "dff", "mka", "webm", "wv", "caf", "wma", "ape", "mpc", "tta", "shn", "ra", "m4b", "m4p",
    "aifc", "au", "ac3", "dts", "amr",
];

/// Recursion guard for a pathological tree.
const MAX_DEPTH: usize = 16;

pub(crate) fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTS.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

/// Hidden (`.`-prefixed: `.DS_Store`, AppleDouble `._x`) or OS junk.
pub(crate) fn is_junk(name: &str) -> bool {
    name.starts_with('.')
        || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
}

/// Every side file under `album_root`, recursively, sorted. Skips junk/hidden
/// entries, audio files, symlinks, and the `skip` directory (the destination
/// root, when it sits inside a source album root).
pub(crate) fn collect(album_root: &Path, skip: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, skip: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > MAX_DEPTH || dir == skip {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if is_junk(name) {
                continue;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                walk(&path, skip, depth + 1, out);
            } else if ft.is_file() && !is_audio(&path) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(album_root, skip, 0, &mut out);
    out.sort();
    out
}

/// Where one album's side files go.
pub(crate) struct AlbumPlacement<'a> {
    /// The album's source root.
    pub(crate) root: &'a Path,
    /// The album's destination folder (absolute).
    pub(crate) dir: &'a Path,
    /// The destination lays discs out as folders (`{DiscDir}` and > 1 disc).
    pub(crate) disc_dirs: bool,
    pub(crate) disc_count: u32,
    /// Source track path without extension → that track's planned destination.
    pub(crate) stems: &'a HashMap<PathBuf, PathBuf>,
}

/// Plan a destination for each side file, in `side`'s order:
/// 1. same stem as a track in the same folder (`01 Liyue.lrc`) → the track's
///    new name with the side file's own extension (keeps lyrics attached);
/// 2. inside a disc folder → `<dir>/Disc N/<rest>` with per-disc folders, else
///    `<dir>/<rest>`, prefixed `Disc N - ` only when that destination collides
///    (with another side file or with a `taken` path);
/// 3. anything else → `<dir>/<path relative to the album root>`.
pub(crate) fn place(
    p: &AlbumPlacement<'_>,
    side: &[PathBuf],
    taken: &HashSet<PathBuf>,
) -> Vec<(PathBuf, PathBuf)> {
    // Pass 1: candidate destinations (+ the disc number for rule-2 flat files).
    let mut cands: Vec<(PathBuf, PathBuf, Option<u32>)> = Vec::with_capacity(side.len());
    for src in side {
        if let Some(ext) = src.extension()
            && let Some(track_dst) = p.stems.get(&src.with_extension(""))
        {
            cands.push((src.clone(), track_dst.with_extension(ext), None));
            continue;
        }
        let rel = src.strip_prefix(p.root).unwrap_or(src);
        let mut comps = rel.components();
        let first = comps.next();
        let rest = comps.as_path();
        let disc = match first {
            Some(c) if !rest.as_os_str().is_empty() => c
                .as_os_str()
                .to_str()
                .and_then(disc_folder::parse)
                .map(|d| u32::from(d.number)),
            _ => None,
        };
        let cand = match disc {
            Some(n) if p.disc_dirs => (
                src.clone(),
                p.dir.join(disc_dir_name(n, p.disc_count)).join(rest),
                None,
            ),
            Some(n) => (src.clone(), p.dir.join(rest), Some(n)),
            None => (src.clone(), p.dir.join(rel), None),
        };
        cands.push(cand);
    }
    // Pass 2: a disc-folder file whose flat destination collides gets `Disc N - `.
    let mut count: HashMap<PathBuf, usize> = HashMap::new();
    for (_, dst, _) in &cands {
        *count.entry(dst.clone()).or_insert(0) += 1;
    }
    cands
        .into_iter()
        .map(|(src, dst, disc)| {
            let collides = count.get(&dst).is_some_and(|&n| n > 1) || taken.contains(&dst);
            match disc {
                Some(n) if collides => {
                    let name = dst
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let renamed =
                        dst.with_file_name(format!("Disc {} - {name}", disc_label(n, p.disc_count)));
                    (src, renamed)
                }
                _ => (src, dst),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{AlbumPlacement, is_audio, is_junk, place};
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    #[test]
    fn audio_and_junk_classification() {
        assert!(is_audio(Path::new("a/01.FLAC")));
        assert!(is_audio(Path::new("a/01.wma")), "undecodable audio is still audio");
        assert!(!is_audio(Path::new("a/Box.cue")));
        assert!(is_junk("Thumbs.db") && is_junk("DESKTOP.INI") && is_junk(".DS_Store"));
        assert!(!is_junk("cover.jpg"));
    }

    #[test]
    fn placement_rules_in_both_layouts() {
        let stems = HashMap::from([(
            PathBuf::from("s/Box/CD1/01 Intro"),
            PathBuf::from("d/Box/1-01 - Intro.flac"),
        )]);
        let side: Vec<PathBuf> = [
            "s/Box/CD1/01 Intro.lrc",
            "s/Box/CD1/Box.log",
            "s/Box/CD2/Box.log",
            "s/Box/CD2/notes.txt",
            "s/Box/Scans/Front.jpg",
            "s/Box/cover.jpg",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        let flat = AlbumPlacement {
            root: Path::new("s/Box"),
            dir: Path::new("d/Box"),
            disc_dirs: false,
            disc_count: 2,
            stems: &stems,
        };
        let got: Vec<PathBuf> = place(&flat, &side, &HashSet::new())
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        assert_eq!(
            got,
            [
                "d/Box/1-01 - Intro.lrc",
                "d/Box/Disc 1 - Box.log",
                "d/Box/Disc 2 - Box.log",
                "d/Box/notes.txt",
                "d/Box/Scans/Front.jpg",
                "d/Box/cover.jpg",
            ]
            .map(PathBuf::from)
        );
        let folders = AlbumPlacement {
            disc_dirs: true,
            ..flat
        };
        let got: Vec<PathBuf> = place(&folders, &side, &HashSet::new())
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        assert_eq!(
            got,
            [
                "d/Box/1-01 - Intro.lrc",
                "d/Box/Disc 1/Box.log",
                "d/Box/Disc 2/Box.log",
                "d/Box/Disc 2/notes.txt",
                "d/Box/Scans/Front.jpg",
                "d/Box/cover.jpg",
            ]
            .map(PathBuf::from)
        );
    }
}
```

- [ ] **Step 6: Template helpers for the planner**

In `template.rs` `impl Template`, after `render`:

```rust
    /// The folder a track renders into with `{DiscDir}` forced empty — relative
    /// to the destination root. The planner takes the common ancestor of these
    /// over an album's tracks as the album's own folder (where side files go).
    pub(crate) fn render_album_dir(&self, tags: &Tags, src: &Path) -> PathBuf {
        self.render_inner(tags, src, true)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    /// Whether the template lays discs out as folders (`{DiscDir}`).
    pub(crate) fn has_disc_dir(&self) -> bool {
        self.raw.contains("{DiscDir}")
    }
```

- [ ] **Step 7: Rewrite the planner**

Replace everything in `crates/soul-audio-organize/src/plan.rs` above `#[cfg(test)]` with:

```rust
//! Planning: group the inputs into albums (the importer's identity rule), read
//! each file's tags, compute its destination via the [`Template`], add each
//! album's side files, and classify every [`Action`] — **without** mutating the
//! filesystem. `plan` only *reads* tags, *lists* album folders and *stats* any
//! existing destination; it never creates directories or copies bytes.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::album::{self, AlbumGroup, RootIndex, TrackInfo};
use crate::side::{self, AlbumPlacement};
use crate::{Strategy, Template};

/// What [`apply`](crate::apply) will do with one source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Copy the source to the destination, keeping the original.
    Copy,
    /// Move the source to the destination, removing the original.
    Move,
    /// Skip: the destination is already claimed by an **earlier** entry in this
    /// batch, or already present on disk at the **same byte length** (treated as
    /// already-organized — see the size heuristic on [`plan`]).
    SkipDuplicate,
    /// Skip: a **different** file already occupies the destination (same path,
    /// different byte length). We never overwrite.
    SkipConflict,
}

/// What kind of file a [`PlanEntry`] places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// An input audio file, renamed per the template.
    Audio,
    /// A non-audio file travelling with its album (`.log`, `.cue`, covers,
    /// `Scans/…`, lyrics, `.txt`/`.nfo`). Never modified — only copied/moved,
    /// renamed only when it shares a track's stem (it follows that track).
    SideFile,
}

/// One planned source → destination placement with its classified [`Action`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEntry {
    /// The source file to copy/move.
    pub src: PathBuf,
    /// Its computed destination.
    pub dst: PathBuf,
    /// How [`apply`](crate::apply) will treat this entry.
    pub action: Action,
    /// Audio input or album side file.
    pub kind: EntryKind,
}

/// Why an album's side files were left where they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideFilesLeft {
    /// The source album root holds more than one album (another album shares
    /// the folder, or sits in a sub-folder of it): ownership is ambiguous.
    SharedAlbumRoot,
    /// The template gives the album no folder of its own (e.g. `flat`, or its
    /// tracks render under different folders).
    NoAlbumFolder,
    /// The album's tracks sit loose in the source root itself — its other files
    /// are not album material to sweep along.
    AtSourceRoot,
}

/// An album root whose side files were **not** planned (reported, never guessed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftInPlace {
    /// The source album root.
    pub album_root: PathBuf,
    /// Why.
    pub reason: SideFilesLeft,
    /// How many side files stay there.
    pub files: usize,
}

/// A computed, not-yet-executed organize plan.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct OrganizePlan {
    /// The root every entry's destination lives under.
    pub dest_root: PathBuf,
    /// Audio entries in input order, then side-file entries album by album.
    pub entries: Vec<PlanEntry>,
    /// Album roots whose side files stay in place, with the reason (each root at
    /// most once).
    pub side_files_left: Vec<LeftInPlace>,
    /// [`Strategy::Move`] only: source folders [`apply`](crate::apply) removes
    /// **if empty** after the moves, deepest first. Never the source root or
    /// above it; without a source root, never above a moved file's album root.
    /// Empty for Copy.
    pub prune_dirs: Vec<PathBuf>,
}

/// Optional planning context.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanOptions {
    /// The folder the inputs were collected from (the scan/staging root). Album
    /// roots never climb to or above it; an album whose tracks sit loose in it
    /// keeps its side files in place; Move cleanup may prune emptied folders up
    /// to — never including — it. `None`: cleanup stops at each album root
    /// (inclusive).
    pub source_root: Option<PathBuf>,
}

/// [`plan_with`] with default [`PlanOptions`] (no source root).
pub fn plan(
    files: &[PathBuf],
    dest_root: &Path,
    template: &Template,
    strategy: Strategy,
) -> OrganizePlan {
    plan_with(files, dest_root, template, strategy, &PlanOptions::default())
}

/// Compute an [`OrganizePlan`] for `files` against `dest_root`.
///
/// Inputs are first grouped into **albums** exactly as the importer does
/// (album root folder — a `CD1`/`Disc 2 - …` folder folds into its parent —
/// plus album title plus MusicBrainz release id), so every track renders with
/// its album's disc context (`{DiscPrefix}`, `{DiscDir}`, `{DiscTotal}`; a disc
/// number missing from the tags comes from the disc folder). Each audio file's
/// destination is `dest_root.join(template.render(..))`.
///
/// **Side files**: for every album root that holds exactly one album, each
/// non-audio, non-junk file under it (recursively) is planned too — see
/// [`EntryKind::SideFile`]; otherwise the root is reported in
/// [`OrganizePlan::side_files_left`].
///
/// Each entry's [`Action`] is classified as:
/// - **[`SkipDuplicate`](Action::SkipDuplicate)** if an *earlier* entry in this
///   batch already claims the identical destination, **or** the destination
///   already exists on disk with the **same byte length** as the source.
/// - **[`SkipConflict`](Action::SkipConflict)** if the destination already exists
///   with a *different* byte length (a distinct file we must not overwrite).
/// - otherwise **[`Copy`](Action::Copy)** or **[`Move`](Action::Move)** per
///   `strategy`.
///
/// # Duplicate heuristic
/// The already-exists check compares **sizes only**, not a content hash; the
/// existing destination is **never overwritten**.
///
/// `plan_with` performs **no** filesystem mutation.
pub fn plan_with(
    files: &[PathBuf],
    dest_root: &Path,
    template: &Template,
    strategy: Strategy,
    opts: &PlanOptions,
) -> OrganizePlan {
    let source_root = opts.source_root.as_deref();
    let (tracks, albums) = album::group(files, source_root);
    let mut entries: Vec<PlanEntry> = Vec::with_capacity(files.len());
    // Parallel to `entries`: each entry's source album root (the Move-cleanup bound).
    let mut entry_roots: Vec<&Path> = Vec::with_capacity(files.len());
    // Destinations an earlier Copy/Move entry will write — later entries mapping
    // to the same path are duplicates of that pending write.
    let mut claimed: HashSet<PathBuf> = HashSet::new();

    // 1. Audio, in input order.
    let mut audio_dst: Vec<PathBuf> = Vec::with_capacity(files.len());
    for (src, info) in files.iter().zip(&tracks) {
        let dst = dest_root.join(template.render(&info.tags, src));
        audio_dst.push(dst.clone());
        push(&mut entries, &mut claimed, src, dst, strategy, EntryKind::Audio);
        entry_roots.push(&albums[info.album].root);
    }

    // 2. Side files, album by album; each album root is handled once.
    let roots = RootIndex::new(&albums);
    let mut side_files_left: Vec<LeftInPlace> = Vec::new();
    let mut seen_roots: HashSet<&Path> = HashSet::new();
    for album in &albums {
        if !seen_roots.insert(album.root.as_path()) {
            continue;
        }
        let side = side::collect(&album.root, dest_root);
        if side.is_empty() {
            continue;
        }
        let reason = if source_root == Some(album.root.as_path()) {
            Some(SideFilesLeft::AtSourceRoot)
        } else if roots.is_shared(&album.root) {
            Some(SideFilesLeft::SharedAlbumRoot)
        } else {
            None
        };
        let rel_dir = match reason {
            None => album_dest_dir(template, files, &tracks, album),
            Some(_) => None,
        };
        let Some(rel_dir) = rel_dir else {
            side_files_left.push(LeftInPlace {
                album_root: album.root.clone(),
                reason: reason.unwrap_or(SideFilesLeft::NoAlbumFolder),
                files: side.len(),
            });
            continue;
        };
        let album_dir = dest_root.join(rel_dir);
        let stems: HashMap<PathBuf, PathBuf> = album
            .tracks
            .iter()
            .filter(|&&i| files[i].extension().is_some())
            .map(|&i| (files[i].with_extension(""), audio_dst[i].clone()))
            .collect();
        let placement = AlbumPlacement {
            root: &album.root,
            dir: &album_dir,
            disc_dirs: template.has_disc_dir() && album.disc_count > 1,
            disc_count: album.disc_count,
            stems: &stems,
        };
        for (src, dst) in side::place(&placement, &side, &claimed) {
            push(&mut entries, &mut claimed, &src, dst, strategy, EntryKind::SideFile);
            entry_roots.push(&album.root);
        }
    }

    let prune_dirs = match strategy {
        Strategy::Move => prune_candidates(&entries, &entry_roots, source_root),
        Strategy::Copy => Vec::new(),
    };
    OrganizePlan {
        dest_root: dest_root.to_path_buf(),
        entries,
        side_files_left,
        prune_dirs,
    }
}

/// Classify and append one entry, claiming its destination when it will write.
fn push(
    entries: &mut Vec<PlanEntry>,
    claimed: &mut HashSet<PathBuf>,
    src: &Path,
    dst: PathBuf,
    strategy: Strategy,
    kind: EntryKind,
) {
    let action = classify(src, &dst, strategy, claimed);
    if matches!(action, Action::Copy | Action::Move) {
        claimed.insert(dst.clone());
    }
    entries.push(PlanEntry {
        src: src.to_path_buf(),
        dst,
        action,
        kind,
    });
}

/// The album's own destination folder, relative to the destination root: the
/// common ancestor of its tracks' rendered folders with `{DiscDir}` neutralized.
/// `None` when that is the destination root itself (e.g. `flat`, or tracks of
/// one album rendering under different artist folders).
fn album_dest_dir(
    template: &Template,
    files: &[PathBuf],
    tracks: &[TrackInfo],
    album: &AlbumGroup,
) -> Option<PathBuf> {
    let mut common: Option<PathBuf> = None;
    for &i in &album.tracks {
        let dir = template.render_album_dir(&tracks[i].tags, &files[i]);
        common = Some(match common {
            None => dir,
            Some(c) => c
                .components()
                .zip(dir.components())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a)
                .collect(),
        });
    }
    common.filter(|c| c.components().next().is_some())
}

/// Every source folder a Move may empty: for each moved entry, its parent and
/// upward — up to and including the album root, or (with a source root above
/// the album root) up to but excluding the source root. Never the source root
/// or an ancestor of it. Deepest first, deterministic.
fn prune_candidates(
    entries: &[PlanEntry],
    roots: &[&Path],
    source_root: Option<&Path>,
) -> Vec<PathBuf> {
    let mut set: BTreeSet<PathBuf> = BTreeSet::new();
    for (e, root) in entries.iter().zip(roots) {
        if e.action != Action::Move {
            continue;
        }
        let limit = match source_root {
            Some(sr) if root.starts_with(sr) => Some(sr),
            _ => root.parent(),
        };
        let Some(limit) = limit else {
            continue;
        };
        let mut dir = e.src.parent();
        while let Some(d) = dir {
            if d == limit || !d.starts_with(limit) {
                break;
            }
            if source_root.is_some_and(|sr| sr.starts_with(d)) {
                break;
            }
            set.insert(d.to_path_buf());
            dir = d.parent();
        }
    }
    let mut dirs: Vec<PathBuf> = set.into_iter().collect();
    dirs.sort_by_key(|d| Reverse(d.components().count()));
    dirs
}

/// Classify one source against the destinations already claimed this batch and
/// whatever currently sits on disk at `dst`.
fn classify(src: &Path, dst: &Path, strategy: Strategy, claimed: &HashSet<PathBuf>) -> Action {
    if claimed.contains(dst) {
        return Action::SkipDuplicate;
    }
    if let Ok(dst_meta) = std::fs::metadata(dst) {
        // Something already lives at `dst`. Equal size = treat as already
        // organized (SkipDuplicate); different size = a distinct file we refuse
        // to clobber (SkipConflict). Size-only, not a content hash.
        let src_len = std::fs::metadata(src).map(|m| m.len()).ok();
        return if src_len == Some(dst_meta.len()) {
            Action::SkipDuplicate
        } else {
            Action::SkipConflict
        };
    }
    match strategy {
        Strategy::Copy => Action::Copy,
        Strategy::Move => Action::Move,
    }
}
```

- [ ] **Step 8: Wire the crate**

In `crates/soul-audio-organize/src/lib.rs`: module list becomes

```rust
mod album;
mod apply;
mod plan;
mod sanitize;
mod side;
mod tags;
mod template;
```

exports:

```rust
pub use apply::{Progress, Summary, apply};
pub use plan::{
    Action, EntryKind, LeftInPlace, OrganizePlan, PlanEntry, PlanOptions, SideFilesLeft, plan,
    plan_with,
};
pub use template::{Template, preset, presets};
```

and replace the crate doc's "Task A (this layer) is **pure logic** … added on top later." paragraph with:

```rust
//! Layers: [`Template`] (pure rendering, incl. the multi-disc tokens and the
//! [`presets`]), [`plan`]/[`plan_with`] (album-aware planning: groups inputs into
//! albums with the importer's identity rule, plans each album's side files —
//! logs, cues, covers, scans, lyrics — and never mutates), and [`apply`] (the
//! only mutating call: copy/move, never overwrite, prune emptied source folders
//! after a Move).
```

- [ ] **Step 9: Run the organize suite**

Run: `cargo test -p soul-audio-organize`
Expected: all PASS — the new `album`/`side` unit tests, the 11 `side_files` tests, and the pre-existing plan/apply/roundtrip tests (their source dirs hold only audio, so they plan no side files).

- [ ] **Step 10: Commit**

```bash
git add crates/soul-audio-organize/
git commit -m "feat(organize): album-aware planning with side files that travel with the album"
```

---

## Task 9: Organize — prune emptied source folders after a Move

**Files:**
- Modify: `crates/soul-audio-organize/src/apply.rs` (`Summary` `:24-36`, `apply` `:51-66`)
- Test: `crates/soul-audio-organize/tests/side_files.rs` (append)

**Interfaces:**
- Consumes: `OrganizePlan.prune_dirs` (T8).
- Produces: `Summary { …, pub pruned_dirs: usize }`; after a run that moved at least one file, `apply` calls `std::fs::remove_dir` (empty-only) on each `prune_dirs` entry in order and counts successes. Errors (non-empty, missing) are ignored — never reported as failures.

- [ ] **Step 1: Write the failing tests**

In `crates/soul-audio-organize/tests/side_files.rs` extend the support import to `use support::{at, audio_files, flac, put, rel, tree, unique_dir};` and append:

```rust
#[test]
fn move_prunes_emptied_folders_bottom_up_but_never_the_source_root() {
    let (src, dest) = (unique_dir("prune-src"), unique_dir("prune-dest"));
    put(&src, "Box/CD1/01 Intro.flac", &track("Box", "Intro", 1, Some("1")));
    put(&src, "Box/CD2/01 Reprise.flac", &track("Box", "Reprise", 1, Some("2")));
    put(&src, "Box/CD1/Box.log", b"log");
    put(&src, "Box/cover.jpg", b"jpg");
    put(&src, "Box/Scans/Front.jpg", b"front");
    let p = plan_with(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Move, &opts(&src));
    let order: Vec<String> = p.prune_dirs.iter().map(|d| rel(&src, d)).collect();
    assert_eq!(order, ["Box/CD1", "Box/CD2", "Box/Scans", "Box"], "deepest first");
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty(), "{:?}", s.failed);
    assert_eq!(s.pruned_dirs, 4);
    assert!(src.exists(), "the source root itself is never removed");
    assert!(tree(&src).is_empty());
    assert!(!src.join("Box").exists());
}

#[test]
fn move_keeps_a_folder_that_still_holds_junk() {
    let (src, dest) = (unique_dir("junk-src"), unique_dir("junk-dest"));
    put(&src, "Box/101 Intro.flac", &track("Box", "Intro", 1, None));
    put(&src, "Box/Scans/Front.jpg", b"front");
    put(&src, "Box/Thumbs.db", b"junk");
    let p = plan_with(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Move, &opts(&src));
    let s = apply(&p, |_| {}).unwrap();
    assert_eq!(s.pruned_dirs, 1, "only Scans/ emptied");
    assert_eq!(tree(&src), set(&["Box/Thumbs.db"]));
}

#[test]
fn move_with_a_source_root_prunes_emptied_parents_below_it() {
    let (src, dest) = (unique_dir("parents-src"), unique_dir("parents-dest"));
    put(&src, "Artist/Box/01.flac", &track("Box", "a", 1, None));
    let p = plan_with(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Move, &opts(&src));
    let order: Vec<String> = p.prune_dirs.iter().map(|d| rel(&src, d)).collect();
    assert_eq!(order, ["Artist/Box", "Artist"]);
    let s = apply(&p, |_| {}).unwrap();
    assert_eq!(s.pruned_dirs, 2);
    assert!(src.exists() && !src.join("Artist").exists());
}

#[test]
fn move_without_a_source_root_stops_at_the_album_root() {
    let (src, dest) = (unique_dir("bound-src"), unique_dir("bound-dest"));
    put(&src, "Lib/Artist/Box/01.flac", &track("Box", "a", 1, None));
    let p = plan(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Move);
    assert_eq!(p.prune_dirs, vec![at(&src, "Lib/Artist/Box")]);
    let s = apply(&p, |_| {}).unwrap();
    assert_eq!(s.pruned_dirs, 1);
    assert!(at(&src, "Lib/Artist").exists(), "never above the album root");
}

#[test]
fn copy_never_prunes() {
    let (src, dest) = (unique_dir("copyprune-src"), unique_dir("copyprune-dest"));
    put(&src, "Box/01.flac", &track("Box", "a", 1, None));
    let before = tree(&src);
    let p = plan_with(&audio_files(&src), &dest, &tpl("artist-album"), Strategy::Copy, &opts(&src));
    assert!(p.prune_dirs.is_empty());
    let s = apply(&p, |_| {}).unwrap();
    assert_eq!(s.pruned_dirs, 0);
    assert_eq!(tree(&src), before);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p soul-audio-organize --test side_files move_ copy_never`
Expected: FAIL to compile — `Summary` has no field `pruned_dirs`.

- [ ] **Step 3: Implement**

In `crates/soul-audio-organize/src/apply.rs`:

1. Module doc: append "After a run that moved files, it removes the plan's [`prune_dirs`](crate::OrganizePlan::prune_dirs) that are now empty (bottom-up; a non-empty folder is never touched)."
2. `Summary` — after `pub failed: …,`:
   ```rust
       /// Source folders removed after a Move because they were left empty
       /// ([`OrganizePlan::prune_dirs`](crate::OrganizePlan::prune_dirs)). Not
       /// part of the per-entry tally.
       pub pruned_dirs: usize,
   ```
3. In `apply`, between the entry loop and `Ok(summary)`:
   ```rust
       // Move cleanup: `remove_dir` only ever removes an EMPTY folder, so a folder
       // still holding a file (junk, an ambiguous album's side files, an
       // unplanned file) survives. Errors are expected and ignored.
       if summary.moved > 0 {
           for dir in &plan.prune_dirs {
               if std::fs::remove_dir(dir).is_ok() {
                   summary.pruned_dirs += 1;
               }
           }
       }
   ```

- [ ] **Step 4: Run the organize suite**

Run: `cargo test -p soul-audio-organize`
Expected: all PASS (16 `side_files` tests + unit + existing).

- [ ] **Step 5: Commit**

```bash
git add crates/soul-audio-organize/src/apply.rs crates/soul-audio-organize/tests/side_files.rs
git commit -m "feat(organize): prune emptied source folders after a move"
```

---

## Task 10: Organize e2e matrix + organize ∘ import round-trip

**Files:**
- Modify: `crates/soul-audio-organize/Cargo.toml` (dev-dependency)
- Create: `crates/soul-audio-organize/tests/multi_disc_e2e.rs`

**Interfaces:**
- Consumes: everything above; `soul_audio_import::scan(&Path) -> io::Result<CatalogInput>` (only `.albums[i].{title, disc_count}` are read).
- Produces: the spec's core e2e matrix — `{FlatTagged, DiscSubfolders, Mixed}` × the 7 presets × `{Copy, Move}`.

- [ ] **Step 1: Add the dev-dependency**

In `crates/soul-audio-organize/Cargo.toml` append:

```toml
[dev-dependencies]
# organize ∘ import round-trip (tests/multi_disc_e2e.rs). Dev-only: the organizer
# itself must not depend on the importer (both are std adapters; tiers.rs and
# layering.rs only inspect normal dependencies).
soul-audio-import = { path = "../soul-audio-import" }
```

- [ ] **Step 2: Write the matrix**

Create `crates/soul-audio-organize/tests/multi_disc_e2e.rs`:

```rust
//! The organize e2e matrix (multi-disc spec, "Testing"): three source layouts ×
//! every preset (one-folder + `+disc-folders` album presets, and `flat`) ×
//! Copy/Move. Each case asserts the exact destination tree (audio + side-file
//! placement + collision prefixes), that nothing was modified, the source left
//! intact (Copy) or pruned (Move), re-run idempotence, and finally that
//! re-importing the destination yields the same albums (organize ∘ import).

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use soul_audio_organize::{Action, PlanOptions, Strategy, Template, apply, plan_with, preset};
use support::{audio_files, flac, mp3, put, tree, unique_dir};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Multi-disc album with disc tags only (`DISCNUMBER=n/2`), all in one folder.
    FlatTagged,
    /// Multi-disc album in `CD1/`/`CD2/` sub-folders, disc-tagged.
    DiscSubfolders,
    /// FlatTagged's album + an album in UNTAGGED `Disc N - Subtitle/` folders.
    Mixed,
}

const PRESETS: [&str; 7] = [
    "artist-album",
    "artist-album+disc-folders",
    "artist-year-album",
    "artist-year-album+disc-folders",
    "genre-artist-album",
    "genre-artist-album+disc-folders",
    "flat",
];

fn track(album: &str, title: &str, n: u32, disc: Option<&str>) -> Vec<u8> {
    let mut c = vec![
        format!("ALBUM={album}"),
        format!("TITLE={title}"),
        "ARTIST=HOYO-MiX".to_owned(),
        "ALBUMARTIST=HOYO-MiX".to_owned(),
        "DATE=2020".to_owned(),
        "GENRE=Game".to_owned(),
        format!("TRACKNUMBER={n}"),
    ];
    if let Some(d) = disc {
        c.push(format!("DISCNUMBER={d}"));
    }
    let refs: Vec<&str> = c.iter().map(String::as_str).collect();
    flac(&refs)
}

/// A single-disc MP3 album (exercises the ID3 path end to end).
fn single(src: &Path) {
    put(
        src,
        "Single/01 Solo.mp3",
        &mp3(&[
            ("TIT2", "Solo"),
            ("TPE1", "HOYO-MiX"),
            ("TPE2", "HOYO-MiX"),
            ("TALB", "Single"),
            ("TRCK", "1/1"),
            ("TDRC", "2020"),
            ("TCON", "Game"),
        ]),
    );
    put(src, "Single/cover.png", b"png-bytes");
}

/// The flat-tagged two-disc `Box` (with junk that must stay behind).
fn flat_box(src: &Path) {
    put(src, "Box/101 Intro.flac", &track("Box", "Intro", 1, Some("1/2")));
    put(src, "Box/102 Theme.flac", &track("Box", "Theme", 2, Some("1/2")));
    put(src, "Box/201 Reprise.flac", &track("Box", "Reprise", 1, Some("2/2")));
    put(src, "Box/101 Intro.lrc", b"[00:00.00]Intro");
    put(src, "Box/Box.log", b"EAC log, checksum 1234");
    put(src, "Box/Box.cue", b"FILE \"101 Intro.flac\" WAVE");
    put(src, "Box/cover.jpg", b"jpg-bytes");
    put(src, "Box/Scans/Front.jpg", b"front-scan");
    put(src, "Box/info.txt", b"notes");
    put(src, "Box/Thumbs.db", b"junk");
    put(src, "Box/.DS_Store", b"junk");
}

fn build_source(layout: Layout, src: &Path) {
    match layout {
        Layout::FlatTagged => flat_box(src),
        Layout::DiscSubfolders => {
            put(src, "Box/CD1/01 Intro.flac", &track("Box", "Intro", 1, Some("1")));
            put(src, "Box/CD1/02 Theme.flac", &track("Box", "Theme", 2, Some("1")));
            put(src, "Box/CD2/01 Reprise.flac", &track("Box", "Reprise", 1, Some("2")));
            put(src, "Box/CD1/01 Intro.lrc", b"[00:00.00]Intro");
            put(src, "Box/CD1/Box.log", b"disc 1 log");
            put(src, "Box/CD2/Box.log", b"disc 2 log!");
            put(src, "Box/cover.jpg", b"jpg-bytes");
            put(src, "Box/Scans/Front.jpg", b"front-scan");
            put(src, "Box/info.nfo", b"nfo");
        }
        Layout::Mixed => {
            flat_box(src);
            put(src, "Suite/Disc 1 - Mondstadt/01 Dawn.flac", &track("Suite", "Dawn", 1, None));
            put(src, "Suite/Disc 1 - Mondstadt/02 Dusk.flac", &track("Suite", "Dusk", 2, None));
            put(src, "Suite/Disc 2 - Liyue/01 Night.flac", &track("Suite", "Night", 1, None));
            put(src, "Suite/Disc 1 - Mondstadt/Suite.cue", b"cue 1");
            put(src, "Suite/Disc 2 - Liyue/Suite.cue", b"cue 2");
            put(src, "Suite/Disc 2 - Liyue/01 Night.lrc", b"[00:00.00]Night");
            put(src, "Suite/cover.jpg", b"suite-jpg");
        }
    }
    single(src);
}

fn albums_of(layout: Layout) -> &'static [&'static str] {
    match layout {
        Layout::Mixed => &["Box", "Suite", "Single"],
        _ => &["Box", "Single"],
    }
}

/// Expected files inside one album's destination folder.
fn album_files(layout: Layout, album: &str, df: bool) -> Vec<&'static str> {
    match (album, layout, df) {
        ("Single", _, _) => vec!["01 - Solo.mp3", "cover.png"],
        ("Box", Layout::DiscSubfolders, false) => vec![
            "1-01 - Intro.flac",
            "1-02 - Theme.flac",
            "2-01 - Reprise.flac",
            "1-01 - Intro.lrc",
            "Disc 1 - Box.log",
            "Disc 2 - Box.log",
            "cover.jpg",
            "Scans/Front.jpg",
            "info.nfo",
        ],
        ("Box", Layout::DiscSubfolders, true) => vec![
            "Disc 1/01 - Intro.flac",
            "Disc 1/02 - Theme.flac",
            "Disc 2/01 - Reprise.flac",
            "Disc 1/01 - Intro.lrc",
            "Disc 1/Box.log",
            "Disc 2/Box.log",
            "cover.jpg",
            "Scans/Front.jpg",
            "info.nfo",
        ],
        ("Box", _, false) => vec![
            "1-01 - Intro.flac",
            "1-02 - Theme.flac",
            "2-01 - Reprise.flac",
            "1-01 - Intro.lrc",
            "Box.log",
            "Box.cue",
            "cover.jpg",
            "Scans/Front.jpg",
            "info.txt",
        ],
        ("Box", _, true) => vec![
            "Disc 1/01 - Intro.flac",
            "Disc 1/02 - Theme.flac",
            "Disc 2/01 - Reprise.flac",
            "Disc 1/01 - Intro.lrc",
            "Box.log",
            "Box.cue",
            "cover.jpg",
            "Scans/Front.jpg",
            "info.txt",
        ],
        ("Suite", _, false) => vec![
            "1-01 - Dawn.flac",
            "1-02 - Dusk.flac",
            "2-01 - Night.flac",
            "2-01 - Night.lrc",
            "Disc 1 - Suite.cue",
            "Disc 2 - Suite.cue",
            "cover.jpg",
        ],
        ("Suite", _, true) => vec![
            "Disc 1/01 - Dawn.flac",
            "Disc 1/02 - Dusk.flac",
            "Disc 2/01 - Night.flac",
            "Disc 2/01 - Night.lrc",
            "Disc 1/Suite.cue",
            "Disc 2/Suite.cue",
            "cover.jpg",
        ],
        other => panic!("no expectation for {other:?}"),
    }
}

fn album_dir(preset_name: &str, album: &str) -> String {
    match preset_name.trim_end_matches("+disc-folders") {
        "artist-album" => format!("HOYO-MiX/{album}"),
        "artist-year-album" => format!("HOYO-MiX/2020 - {album}"),
        "genre-artist-album" => format!("Game/HOYO-MiX/{album}"),
        other => panic!("no album folder for {other}"),
    }
}

fn expected_dest(layout: Layout, preset_name: &str) -> BTreeSet<String> {
    if preset_name == "flat" {
        // `{Artist} - {Title}`: audio only — side files have no album folder.
        let mut titles = vec!["Intro.flac", "Theme.flac", "Reprise.flac", "Solo.mp3"];
        if layout == Layout::Mixed {
            titles.extend(["Dawn.flac", "Dusk.flac", "Night.flac"]);
        }
        return titles.into_iter().map(|t| format!("HOYO-MiX - {t}")).collect();
    }
    let df = preset_name.ends_with("+disc-folders");
    albums_of(layout)
        .iter()
        .flat_map(|a| {
            let dir = album_dir(preset_name, a);
            album_files(layout, a, df)
                .into_iter()
                .map(move |f| format!("{dir}/{f}"))
        })
        .collect()
}

fn is_audio(rel: &str) -> bool {
    rel.ends_with(".flac") || rel.ends_with(".mp3")
}

fn is_junk(rel: &str) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.starts_with('.') || name == "Thumbs.db"
}

/// `title → disc_count` of a scanned library.
fn disc_counts(root: &Path) -> BTreeMap<String, u16> {
    soul_audio_import::scan(root)
        .expect("scan")
        .albums
        .iter()
        .map(|a| (a.title.clone(), a.disc_count))
        .collect()
}

fn run_case(layout: Layout, preset_name: &str, strategy: Strategy) {
    let tag = format!("{layout:?}-{}-{strategy:?}", preset_name.replace('+', "_"));
    let src = unique_dir(&format!("{tag}-src"));
    let dest = unique_dir(&format!("{tag}-dest"));
    build_source(layout, &src);
    let initial = tree(&src);
    let before = disc_counts(&src);
    let files = audio_files(&src);
    let template = Template::new(preset(preset_name).expect("preset"));
    let opts = PlanOptions {
        source_root: Some(src.clone()),
    };

    // Plan: every entry is a fresh write.
    let p = plan_with(&files, &dest, &template, strategy, &opts);
    let want = match strategy {
        Strategy::Copy => Action::Copy,
        Strategy::Move => Action::Move,
    };
    assert!(
        p.entries.iter().all(|e| e.action == want),
        "{tag}: every entry {want:?}: {:#?}",
        p.entries
    );
    let snapshot: Vec<(std::path::PathBuf, Vec<u8>)> = p
        .entries
        .iter()
        .map(|e| (e.dst.clone(), std::fs::read(&e.src).unwrap()))
        .collect();

    // Apply.
    let summary = apply(&p, |_| {}).expect("apply");
    assert!(summary.failed.is_empty(), "{tag}: {:?}", summary.failed);
    assert_eq!(summary.copied + summary.moved, p.entries.len(), "{tag}");

    // 1. The exact destination tree.
    assert_eq!(tree(&dest), expected_dest(layout, preset_name), "{tag}: destination tree");
    // 2. Nothing was modified on the way (side files and audio, byte for byte).
    for (dst, bytes) in &snapshot {
        assert_eq!(&std::fs::read(dst).unwrap(), bytes, "{tag}: {dst:?} modified");
    }
    // 3. The source: untouched (Copy) / only what must stay (Move).
    match strategy {
        Strategy::Copy => assert_eq!(tree(&src), initial, "{tag}: Copy leaves the source intact"),
        Strategy::Move => {
            let left: BTreeSet<String> = if preset_name == "flat" {
                initial.iter().filter(|r| !is_audio(r)).cloned().collect()
            } else {
                initial.iter().filter(|r| is_junk(r)).cloned().collect()
            };
            assert_eq!(tree(&src), left, "{tag}: Move leaves only what it must");
            assert!(src.exists(), "{tag}: the source root is never pruned");
            if preset_name != "flat" {
                assert!(!src.join("Single").exists(), "{tag}: emptied album folder pruned");
                assert!(!src.join("Box").join("Scans").exists(), "{tag}: Scans/ pruned");
                assert_eq!(
                    src.join("Box").exists(),
                    layout != Layout::DiscSubfolders,
                    "{tag}: Box/ survives only when it still holds junk"
                );
            }
        }
    }
    // 4. Idempotent re-run.
    match strategy {
        Strategy::Copy => {
            let again = plan_with(&files, &dest, &template, strategy, &opts);
            assert_eq!(again.entries.len(), p.entries.len(), "{tag}");
            assert!(
                again.entries.iter().all(|e| e.action == Action::SkipDuplicate),
                "{tag}: re-run is all SkipDuplicate: {:#?}",
                again.entries
            );
            assert_eq!(apply(&again, |_| {}).unwrap().copied, 0, "{tag}");
        }
        Strategy::Move => {
            let s = apply(&p, |_| {}).unwrap();
            assert_eq!((s.moved, s.skipped), (0, p.entries.len()), "{tag}: re-apply moves nothing");
            assert!(s.failed.is_empty(), "{tag}");
        }
    }
    // 5. organize ∘ import: same albums, same disc counts — except that an
    // untagged album's folder-inferred disc numbers survive only in a
    // `+disc-folders` layout (the organizer never writes tags).
    let mut want_counts = before.clone();
    if layout == Layout::Mixed && !preset_name.ends_with("+disc-folders") {
        want_counts.insert("Suite".to_owned(), 1);
    }
    assert_eq!(disc_counts(&dest), want_counts, "{tag}: re-import");
    assert_eq!(before.len(), albums_of(layout).len(), "{tag}: source albums");

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dest);
}

fn run_all(layout: Layout, strategy: Strategy) {
    for p in PRESETS {
        run_case(layout, p, strategy);
    }
}

#[test]
fn flat_tagged_copy() {
    run_all(Layout::FlatTagged, Strategy::Copy);
}
#[test]
fn flat_tagged_move() {
    run_all(Layout::FlatTagged, Strategy::Move);
}
#[test]
fn disc_subfolders_copy() {
    run_all(Layout::DiscSubfolders, Strategy::Copy);
}
#[test]
fn disc_subfolders_move() {
    run_all(Layout::DiscSubfolders, Strategy::Move);
}
#[test]
fn mixed_copy() {
    run_all(Layout::Mixed, Strategy::Copy);
}
#[test]
fn mixed_move() {
    run_all(Layout::Mixed, Strategy::Move);
}

/// Existing destination files are never overwritten: a different-size file is
/// a SkipConflict and survives byte for byte; a same-size one is a duplicate.
#[test]
fn existing_destination_files_are_never_overwritten() {
    let src = unique_dir("noclobber-src");
    let dest = unique_dir("noclobber-dest");
    flat_box(&src);
    let theirs = put(&dest, "HOYO-MiX/Box/cover.jpg", b"someone else's, longer cover");
    put(&dest, "HOYO-MiX/Box/1-01 - Intro.flac", &track("Box", "Intro", 1, Some("1/2")));
    let p = plan_with(
        &audio_files(&src),
        &dest,
        &Template::new(preset("artist-album").unwrap()),
        Strategy::Copy,
        &PlanOptions {
            source_root: Some(src.clone()),
        },
    );
    let action = |rel: &str| {
        let want = support::at(&dest, rel);
        p.entries.iter().find(|e| e.dst == want).unwrap().action
    };
    assert_eq!(action("HOYO-MiX/Box/cover.jpg"), Action::SkipConflict);
    assert_eq!(action("HOYO-MiX/Box/1-01 - Intro.flac"), Action::SkipDuplicate);
    let s = apply(&p, |_| {}).unwrap();
    assert!(s.failed.is_empty());
    assert_eq!(std::fs::read(&theirs).unwrap(), b"someone else's, longer cover");
}
```

- [ ] **Step 3: Run the matrix**

Run: `cargo test -p soul-audio-organize --test multi_disc_e2e`
Expected: 7 tests PASS (42 matrix cases + the no-clobber case). If a tree assertion fails, the message names the case (`{layout}-{preset}-{strategy}`) and diffs the sets.

- [ ] **Step 4: Architecture gates still green**

Run: `cargo test -p architecture-tests`
Expected: PASS — the new edge is a dev-dependency (`tiers.rs`/`layering.rs` filter `DependencyKind::Normal`).

- [ ] **Step 5: Commit**

```bash
git add crates/soul-audio-organize/Cargo.toml crates/soul-audio-organize/tests/multi_disc_e2e.rs Cargo.lock
git commit -m "test(organize): multi-disc e2e matrix and organize-import round-trip"
```

---

## Task 11: Full gate + reader bench check

**Files:** none (verification only; fix-ups go in the task whose code they touch).

- [ ] **Step 1: Full local CI gate**

Run: `cargo xtask ci`
Expected: fmt clean, clippy `-D warnings` clean (default features and `--features build` for index/artbake), every host test green (incl. `soul-audio-index --features build`, `soul-audio-tags` with workspace-unified `std`), and the thumbv7em cross-builds (core, tags, catalog-via-firmware, index reader, firmware) succeed.

- [ ] **Step 2: Reader bench within budget**

Run: `cargo bench -p soul-audio-index --features build --bench reader > ../bench-v8.txt 2>&1` and compare each scenario row with `../bench-v7.txt` (Task 4 Step 0).
Expected: the "reads" and "cold blocks" columns grow by **≤ 10 %** (the track record grew 85 → 91 B = +7 %; album records are read rarely). A larger regression is a blocker — investigate before the PR.

- [ ] **Step 3: Branch review**

Run: `git log --oneline origin/main..HEAD` and `git diff --stat origin/main..HEAD`
Expected: the T1–T10 commits (docs → tags → index → import → multicatalog → organize), no stray files.

- [ ] **Step 4: PR**

Push `feat/multi-disc-albums` and open the PR against `main`. PR body must call out: **`.soulidx` format 7 → 8** (desktop re-bakes automatically; a remote server and its clients must be upgraded together — an old reader shows an empty remote library), **`SCAN_CACHE_VERSION` 3 → 4** (one cold re-scan), the **API changes** for the `-ng` consumer (`PlanEntry.kind`, `OrganizePlan.{side_files_left, prune_dirs}`, `Summary.pruned_dirs`, `plan_with`/`PlanOptions`, `preset()`, 3 new presets, empty directory segments are now dropped), and the documented limitations (`.cue` `FILE` lines not rewritten; folder-inferred discs survive organize only into `+disc-folders`; junk keeps a folder from being pruned). Wait for CI green; merge with `gh pr merge --squash --delete-branch` (no `--admin`). Record the squashed `main` SHA for Phase 2.

---

## Self-Review

**Spec coverage (core scope):**
- §1 tags — `n/total` split shared via `util::emit_text`/`split_num_total` (T2); `DISCTOTAL`/`TOTALDISCS`/`TRACKTOTAL`/`TOTALTRACKS` (Vorbis + APE, T2); `disc_subtitle` (Vorbis/APE `DISCSUBTITLE`, ID3 `TSST`, MP4 freeform) and `musicbrainz_album_id` (Vorbis/APE, ID3 `TXXX`, MP4 freeform) + `Field` variants + owned arms (T3). Compilation flag / release-group id: out of scope ✓.
- §2 import — album root (disc folder → grandparent), disc-number and subtitle fallbacks, key `(root folder, title, MB id)` without album artist, `disc_count`, MB id, album folder/cover from the root, `RawTrack` fields, `SCAN_CACHE_VERSION` 4 (T5).
- §3 `.soulidx` v8 — track 91 B / album 38 B append-only, interned (deduped) subtitle, catalog fields, version 8 exact-match, compile-time offset guards + roundtrip, no-alloc probe over real bytes, architecture doc first (T1, T4); multicatalog routing (T6).
- §4 organizer — album-aware plan via the shared `disc_folder` matcher (T8), `{DiscPrefix}`/`{DiscDir}`/`{DiscTotal}`, empty-segment drop, disc-safe presets + `+disc-folders` twins (T7), side files incl. stem-following rename, disc-folder placement, collision prefix, ambiguity rule, never-modified, flat carries none (T8), Move cleanup (T9).
- Testing — tag tests (T2/T3), matcher table (T1), import pipeline 8 scenarios + scan-root guard (T5), index roundtrip/version/v7-empty/no-alloc/bench (T4/T11), multicatalog (T6), organize e2e matrix + organize∘import (T10).
- Phase 2 items (§5–§6, ng tests, manual acceptance) are intentionally excluded.

**Placeholder scan:** no TBD/TODO; every code step carries complete code; the two `perl` one-liners are exact, with the expected per-file site counts.

**Type consistency:** `DiscFolder { number: u16, subtitle: Option<&str> }` (T1) → import uses `d.number` as `u16` (T5), organize converts with `u32::from` (T8). `disc_label`/`disc_dir_name(u32, u32)` (T7) ← `AlbumGroup.disc_count: u32` and `AlbumPlacement.disc_count: u32` (T8). `TrackInput.disc_subtitle: String`, `AlbumMeta.{disc_count: u16, mb_album_id: String}` (T4) ← import (T5), multicatalog test (T6). `TrackRec.disc_subtitle`/`AlbumRec.{disc_count, mb_album_id}` (T4) ← multicatalog re-pack (T6). `PlanOptions`/`plan_with`/`EntryKind`/`LeftInPlace`/`SideFilesLeft::{SharedAlbumRoot, NoAlbumFolder, AtSourceRoot}`/`OrganizePlan.prune_dirs` (T8) ← apply (T9) and tests (T8–T10). `Summary.pruned_dirs` (T9) ← T9/T10 tests.
