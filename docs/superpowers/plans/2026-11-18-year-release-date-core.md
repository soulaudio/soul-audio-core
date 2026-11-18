# Year / Release-Date Metadata — Phase 1 (soul-audio-core) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `soul-audio-core` extract and bake an album's *original* and *release* dates as packed `u32` values in the `.soulidx`, keyed for chronological order on the original date — so the `-ng` player (Phase 2) can show "Artist • 1973" and offer an Original/Release toggle.

**Architecture:** A new `no_std` `PackedDate(u32)` type in `soul-audio-catalog`. The custom tag readers gain a `Field::OriginalDate`; import parses both the main date tag and the new original-date tag into packed dates (additive first), then the `.soulidx` ALBUM RECORD is migrated from a single `u16 year` to two packed `u32` dates (format v4→v5), with the browse order and the multicatalog merge both keyed on the original date.

**Tech Stack:** Rust, `no_std` + no-alloc core crates (`soul-audio-catalog`, `soul-audio-index`, `soul-audio-tags`), `std` `soul-audio-import`; hand-rolled tag parsers; `cargo xtask ci` gate.

## Global Constraints

- **`PackedDate` is `no_std`, no-alloc, pure integer bit-ops — NO float, NO `libm`.** The index `album()` reader (which constructs `AlbumRec`) ships in the thumbv7em DAP binary.
- **Packing:** `value = (year << 9) | (month << 5) | day`; `day` bits 0..4, `month` bits 5..8, `year` bits 9..31; `0` = fully unknown. Sort key: unknown (`0`) → `u32::MAX` (sorts last).
- **`.soulidx` ALBUM RECORD v5 byte layout (30 bytes):** `0 id u32 · 4 title.off u32 · 8 title.len u16 · 10 album_artist u32 · 14 original_date u32 · 18 release_date u32 · 22 folder u32 · 26 art_id u32`. `ALBUM_RECORD_SIZE = 30`.
- **`FORMAT_VERSION: u16 = 5`** (was 4). **`SCAN_CACHE_VERSION: u32 = 3`** (was 2).
- **`year_at` (index) and `Head`'s order key (multicatalog) MUST both key on `original_date.sort_key()`** — identical, or the desktop merge diverges from the device-frozen order.
- **Firmware (`soul-audio-firmware`) needs NO source change** — it never reads the album record.
- **Year validity is a FIXED `1000..=2200`** (no clock), month `1..=12`, day `1..=31`, else that component is unknown.
- **Commit messages:** no Claude/Anthropic/AI/Co-Authored-By references (the commit-msg hook rejects them).
- **Verify locally** with `cargo xtask ci` from the core repo root (fmt/clippy/all tests incl. `--features build` + thumbv7em cross-builds). Do **not** run core `cargo` while another core verify runs (shared `~/.cargo` lock). Core CI gates PRs; merge via `gh pr merge --squash` after green (no `--admin`).
- Work in the local clone `D:/dev/soulaudio/soul-audio-core` on a feature branch off `main`.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/soul-audio-catalog/src/lib.rs` | Shared no_std record model | **Add** `PackedDate` type (T1); `AlbumRec` `year:u16` → two `PackedDate` (T4) |
| `crates/soul-audio-tags/src/lib.rs` | `Field` enum | **Add** `Field::OriginalDate` (T2) |
| `crates/soul-audio-tags/src/owned.rs` | Owned `Tags` snapshot + collector | **Add** `original_date` field + match arm (T2) |
| `crates/soul-audio-tags/src/{id3v2,vorbis,mp4,caf,wavpack,matroska}.rs` | Per-container mappers | **Add** original-date mapping (T2) |
| `crates/soul-audio-import/src/lib.rs` | Scan → `AlbumMeta`/`RawTrack` | **Add** parse helper + dual-date extraction + `SCAN_CACHE_VERSION` 3 (T3); drop the transitional `year` (T4) |
| `crates/soul-audio-index/src/lib.rs` | `.soulidx` baker + reader + `AlbumMeta` + order | **Migrate** record v5 + offsets + `year_at` (T4) |
| `crates/soul-audio-multicatalog/src/lib.rs` | Merge heads | **Migrate** `Head` order key to original date (T4) |

---

## Task 1: `PackedDate` type (soul-audio-catalog)

**Files:**
- Modify: `crates/soul-audio-catalog/src/lib.rs` (add the type near the top-level record types, before `AlbumRec` at `:95`)
- Test: inline `#[cfg(test)] mod tests` in the same file (the crate is `#![cfg_attr(not(test), no_std)]`, so `cargo test` builds it with `std`).

**Interfaces:**
- Produces: `pub struct PackedDate(pub u32)` with `const fn new(year: u16, month: u8, day: u8) -> PackedDate`, `const fn from_year(year: u16) -> PackedDate`, `const fn year(self) -> u16`, `const fn month(self) -> u8`, `const fn day(self) -> u8`, `const fn is_known(self) -> bool`, `const fn sort_key(self) -> u32`, and `const UNKNOWN: PackedDate`. Derives `Clone, Copy, Debug, PartialEq, Eq, Default` (NOT `Ord` — callers must sort via `sort_key()`).

- [ ] **Step 1: Write the failing test**

Add to `crates/soul-audio-catalog/src/lib.rs`:

```rust
#[cfg(test)]
mod packed_date_tests {
    use super::PackedDate;

    #[test]
    fn packs_and_unpacks_components() {
        let d = PackedDate::new(1973, 8, 7);
        assert_eq!(d.year(), 1973);
        assert_eq!(d.month(), 8);
        assert_eq!(d.day(), 7);
        assert!(d.is_known());
    }

    #[test]
    fn year_only_has_zero_month_and_day() {
        let d = PackedDate::from_year(2015);
        assert_eq!(d.year(), 2015);
        assert_eq!(d.month(), 0);
        assert_eq!(d.day(), 0);
        assert!(d.is_known());
    }

    #[test]
    fn out_of_range_month_or_day_is_dropped_to_unknown_component() {
        let d = PackedDate::new(2001, 13, 40);
        assert_eq!(d.year(), 2001);
        assert_eq!(d.month(), 0);
        assert_eq!(d.day(), 0);
    }

    #[test]
    fn zero_is_fully_unknown_and_sorts_last() {
        assert_eq!(PackedDate::UNKNOWN, PackedDate::default());
        assert!(!PackedDate::UNKNOWN.is_known());
        assert_eq!(PackedDate::UNKNOWN.sort_key(), u32::MAX);
    }

    #[test]
    fn sort_key_is_chronological() {
        // Earlier date → smaller sort key; same year, later month → larger.
        assert!(PackedDate::from_year(1973).sort_key() < PackedDate::from_year(2015).sort_key());
        assert!(PackedDate::new(1973, 1, 1).sort_key() < PackedDate::new(1973, 12, 1).sort_key());
        // Any known date sorts before unknown.
        assert!(PackedDate::from_year(2200).sort_key() < PackedDate::UNKNOWN.sort_key());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p soul-audio-catalog packed_date`
Expected: FAIL — `cannot find type PackedDate`.

- [ ] **Step 3: Write minimal implementation**

Add above `AlbumRec` in `crates/soul-audio-catalog/src/lib.rs`:

```rust
/// A packed calendar date for album metadata: `(year << 9) | (month << 5) | day`.
/// `day` occupies bits 0..4, `month` bits 5..8, `year` bits 9..31. A bare year
/// keeps `month`/`day` = 0 ("precision unknown"); `0` overall is a fully-unknown
/// date. Pure integer bit-ops only — `no_std`, no-alloc, no float (this type is
/// read by the DAP's on-device index reader).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PackedDate(pub u32);

impl PackedDate {
    /// A fully-unknown date.
    pub const UNKNOWN: PackedDate = PackedDate(0);

    /// Pack components. `month` outside `1..=12` or `day` outside `1..=31`
    /// collapses that component to `0` (unknown); the year is stored as given.
    pub const fn new(year: u16, month: u8, day: u8) -> PackedDate {
        let y = year as u32;
        let m = if month >= 1 && month <= 12 { month as u32 } else { 0 };
        let d = if day >= 1 && day <= 31 { day as u32 } else { 0 };
        PackedDate((y << 9) | (m << 5) | d)
    }

    /// A year-only date (month/day unknown).
    pub const fn from_year(year: u16) -> PackedDate {
        PackedDate::new(year, 0, 0)
    }

    pub const fn year(self) -> u16 {
        (self.0 >> 9) as u16
    }
    pub const fn month(self) -> u8 {
        ((self.0 >> 5) & 0xF) as u8
    }
    pub const fn day(self) -> u8 {
        (self.0 & 0x1F) as u8
    }

    /// Whether any component is known (i.e. not the fully-unknown `0`).
    pub const fn is_known(self) -> bool {
        self.0 != 0
    }

    /// Ascending chronological sort key with unknown (`0`) forced LAST.
    pub const fn sort_key(self) -> u32 {
        if self.0 == 0 { u32::MAX } else { self.0 }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p soul-audio-catalog packed_date`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/soul-audio-catalog/src/lib.rs
git commit -m "feat(catalog): add no_std PackedDate(u32) for album dates"
```

---

## Task 2: `Field::OriginalDate` + tag readers (soul-audio-tags)

**Files:**
- Modify: `crates/soul-audio-tags/src/lib.rs:50` (enum), `src/owned.rs:39` (field) + `:101` (collector arm)
- Modify mappers: `src/id3v2.rs:245`, `src/vorbis.rs:35`, `src/mp4.rs:330`, `src/caf.rs:155`, `src/wavpack.rs:135`, `src/matroska.rs:294`
- Test: the per-container `#[cfg(test)] mod tests` already in each parser file, plus `src/owned.rs` tests.

**Interfaces:**
- Produces: `Field::OriginalDate` variant; `Tags { …, pub original_date: Option<String>, … }`. Each container mapper now returns `Some(Field::OriginalDate)` for its original-release-date tag.

- [ ] **Step 1: Write the failing test (ID3v2, the representative container)**

In `crates/soul-audio-tags/src/id3v2.rs`'s test module, add a test that an ID3v2.4 frame set with both `TDRC` (release) and `TDOR` (original) emits both fields. Follow the existing multi-field ReplayGain fixture pattern at `id3v2.rs:1022` (same `Collect` sink + `id3v24`/text-frame helpers in `testfix.rs`). The assertion shape:

```rust
#[test]
fn id3v2_emits_both_date_and_original_date() {
    // Build an ID3v2.4 tag with TDRC=2015 (release) and TDOR=1973 (original),
    // using the same frame-builder helpers the ReplayGain test at :1022 uses.
    let collected = collect_frames(&[("TDRC", "2015"), ("TDOR", "1973")]); // see :1022 for the exact helper
    assert!(collected.iter().any(|(f, v)| *f == Some(Field::Date) && v == "2015"));
    assert!(collected.iter().any(|(f, v)| *f == Some(Field::OriginalDate) && v == "1973"));
}
```

(Use the file's actual existing test helper names; `collect_frames`/`collect` is whatever the ReplayGain test at `:1022` calls — copy that harness verbatim.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p soul-audio-tags id3v2_emits_both`
Expected: FAIL — `Field::OriginalDate` does not exist (compile error), then once the enum exists, FAIL because the mapper doesn't emit it.

- [ ] **Step 3: Add the enum variant**

In `crates/soul-audio-tags/src/lib.rs`, immediately after the `Date` variant (`:50`):

```rust
    /// The main release/recording date (ID3 `TDRC`/`TYER`, Vorbis `DATE`, MP4 `©day`).
    Date,
    /// The ORIGINAL release date of a reissue/remaster (ID3 `TDOR`/`TORY`,
    /// Vorbis `ORIGINALDATE`/`ORIGINALYEAR`, MP4 freeform `ORIGINALDATE`). The
    /// audiophile-canonical "year" prefers this, falling back to `Date`.
    OriginalDate,
```

- [ ] **Step 4: Add the owned-collector field + arm**

In `crates/soul-audio-tags/src/owned.rs`, add to `struct Tags` right after `pub date: Option<String>,` (`:39`):

```rust
    pub date: Option<String>,
    /// The original-release date tag, if present (first value wins). Distinct
    /// from `date` (the reissue/release date); the importer prefers it.
    pub original_date: Option<String>,
```

And in the collector's `match field` (right after the `Field::Date => { … }` arm at `:99-101`):

```rust
            Field::OriginalDate => {
                self.tags.original_date.get_or_insert_with(|| value.into());
            }
```

- [ ] **Step 5: Add each container mapper (one arm each; mirror the existing `Field::Date` arm in the same function)**

- `src/id3v2.rs` — after `"TDRC" | "TYER" | "TYE" => Some(Field::Date),` (`:245`):
  ```rust
          "TDOR" | "TORY" => Some(Field::OriginalDate),
  ```
- `src/vorbis.rs` — in `map_vorbis_key`, beside the `DATE`/`YEAR` → `Field::Date` branch (`:35`), add a branch (use the same `eq_ignore_ascii_case` style the function already uses):
  ```rust
          } else if key.eq_ignore_ascii_case("ORIGINALDATE")
              || key.eq_ignore_ascii_case("ORIGINALYEAR")
          {
              Some(Field::OriginalDate)
  ```
- `src/mp4.rs` — in the freeform `handle_freeform_entry` name→field decision (`:330`, currently `map_replaygain_key(name).or_else(|| map_musicbrainz_recording_key(name))`), extend the chain so an `ORIGINALDATE`/`ORIGINALYEAR` freeform name (case-insensitive) maps to `Field::OriginalDate`. Add a small local matcher next to the existing `map_*` helpers:
  ```rust
  fn map_original_date_key(name: &str) -> Option<Field> {
      if name.eq_ignore_ascii_case("ORIGINALDATE") || name.eq_ignore_ascii_case("ORIGINALYEAR") {
          Some(Field::OriginalDate)
      } else {
          None
      }
  }
  ```
  and at `:330`: `map_replaygain_key(name).or_else(|| map_musicbrainz_recording_key(name)).or_else(|| map_original_date_key(name))`.
- `src/caf.rs` — in `map_key`, beside the `year`/`date` → `Field::Date` branch (`:155`):
  ```rust
      } else if key.eq_ignore_ascii_case(b"original date") || key.eq_ignore_ascii_case(b"original year") {
          Some(Field::OriginalDate)
  ```
- `src/wavpack.rs` (APE) — in `map_key`, beside `Year`/`Date` → `Field::Date` (`:135`):
  ```rust
      } else if key.eq_ignore_ascii_case("Original Date") || key.eq_ignore_ascii_case("Original Year") {
          Some(Field::OriginalDate)
  ```
- `src/matroska.rs` — in `map_name`, beside `DATE`/`DATE_RELEASED` → `Field::Date` (`:294`):
  ```rust
      "DATE_RECORDED" => Some(Field::OriginalDate),
  ```

(Also add any newly-required match arm for `Field::OriginalDate` in the no-op fall-through group at `owned.rs:152-161` if the compiler flags a non-exhaustive match — the real arm from Step 4 should satisfy it; remove `OriginalDate` from any catch-all list.)

- [ ] **Step 6: Run the ID3v2 test + add one Vorbis test, verify pass**

Run: `cargo test -p soul-audio-tags`
Expected: `id3v2_emits_both_date_and_original_date` PASS. Add a parallel Vorbis test (`DATE=2015` + `ORIGINALDATE=1973` comment vector, mirroring the file's existing vorbis test fixture) asserting both fields emit; PASS. Confirm the whole `soul-audio-tags` suite stays green (the new variant must not break the exhaustive `owned.rs` match).

- [ ] **Step 7: Add an owned-snapshot test**

In `crates/soul-audio-tags/src/owned.rs` tests (near the ReplayGain snapshot test at `:391`), assert a `read_owned` over an ID3v2 fixture with both dates yields `Tags { date: Some("2015"), original_date: Some("1973"), .. }`.

- [ ] **Step 8: Commit**

```bash
git add crates/soul-audio-tags/src/
git commit -m "feat(tags): read original-release date into Field::OriginalDate across containers"
```

---

## Task 3: Dual-date extraction in import (additive) + scan-cache bump

**Files:**
- Modify: `crates/soul-audio-import/src/lib.rs` — new `parse_date` helper; `extract_raw` (`:286-315`); `RawTrack` (`:254`, add two fields, keep `year`); `Interner::album` (`:179-203`, add two params); `AlbumMeta` construction (via the baker's input — but `AlbumMeta` itself is defined in `soul-audio-index`; see note); `SCAN_CACHE_VERSION` (`:429`).
- Test: inline tests in `crates/soul-audio-import/src/lib.rs`.

> **Note (transitional, additive):** This task does NOT change the on-disk format. `AlbumMeta` (defined in `soul-audio-index/src/lib.rs:515`) gains two `PackedDate` fields **alongside** the existing `year: u16`; the baker still writes `year` (Task 4 flips the format). So in this task: add `original_date`/`release_date` to `AlbumMeta` (index), to `RawTrack` (import), and thread them through `Interner::album`, while leaving `year` in place everywhere. Everything compiles and all existing tests stay green.

**Interfaces:**
- Consumes: `PackedDate` (Task 1), `Tags.original_date` (Task 2).
- Produces: `fn parse_date(s: Option<&str>) -> PackedDate`; `RawTrack { …, year: u16, original_date: PackedDate, release_date: PackedDate }`; `AlbumMeta { …, year: u16, original_date: PackedDate, release_date: PackedDate }`; `Interner::album(…, year: u16, original_date: PackedDate, release_date: PackedDate, …)`.

- [ ] **Step 1: Write the failing test for `parse_date`**

Add to `crates/soul-audio-import/src/lib.rs` tests:

```rust
#[test]
fn parse_date_takes_leading_iso_components_with_guards() {
    use soul_audio_catalog::PackedDate;
    assert_eq!(parse_date(Some("2012-05-29")), PackedDate::new(2012, 5, 29));
    assert_eq!(parse_date(Some("1973")), PackedDate::from_year(1973));
    assert_eq!(parse_date(Some("1973-08")), PackedDate::new(1973, 8, 0));
    assert_eq!(parse_date(Some("not a date")), PackedDate::UNKNOWN);
    assert_eq!(parse_date(None), PackedDate::UNKNOWN);
    assert_eq!(parse_date(Some("0000")), PackedDate::UNKNOWN); // year < 1000
    assert_eq!(parse_date(Some("9999")), PackedDate::UNKNOWN); // year > 2200
    assert_eq!(parse_date(Some("2001-13-40")), PackedDate::from_year(2001)); // bad m/d drop
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p soul-audio-import parse_date`
Expected: FAIL — `cannot find function parse_date`.

- [ ] **Step 3: Implement `parse_date`**

Add near the existing year parse in `crates/soul-audio-import/src/lib.rs`:

```rust
/// Parse a tag date string (leading ISO `YYYY[-MM[-DD]]`) into a `PackedDate`.
/// Year is accepted only in a fixed plausible range `1000..=2200` (no clock, so
/// parsing is deterministic); an out-of-range year yields `UNKNOWN`. Month/day
/// are optional and individually dropped to "unknown" when absent or malformed.
fn parse_date(s: Option<&str>) -> soul_audio_catalog::PackedDate {
    use soul_audio_catalog::PackedDate;
    let Some(s) = s else { return PackedDate::UNKNOWN };
    let year = match s.get(0..4).and_then(|y| y.parse::<u16>().ok()) {
        Some(y) if (1000..=2200).contains(&y) => y,
        _ => return PackedDate::UNKNOWN,
    };
    let month = s.get(5..7).and_then(|m| m.parse::<u8>().ok()).unwrap_or(0);
    let day = s.get(8..10).and_then(|d| d.parse::<u8>().ok()).unwrap_or(0);
    PackedDate::new(year, month, day) // new() drops out-of-range month/day
}
```

- [ ] **Step 4: Run `parse_date` test, verify pass**

Run: `cargo test -p soul-audio-import parse_date`
Expected: PASS.

- [ ] **Step 5: Thread both dates through `RawTrack`, `extract_raw`, `Interner::album`, `AlbumMeta` (additive)**

1. `RawTrack` (`:254`): after `year: u16,` add
   ```rust
       original_date: soul_audio_catalog::PackedDate,
       release_date: soul_audio_catalog::PackedDate,
   ```
2. `extract_raw` (after the existing `let year = …` block `:287-292`): add
   ```rust
       let release_date = parse_date(tags.date.as_deref());
       let original_date = parse_date(tags.original_date.as_deref());
   ```
   and in the `RawTrack { … }` literal (`:299+`), after `year,` add `original_date,` and `release_date,`.
3. `AlbumMeta` in `soul-audio-index/src/lib.rs:515-520`: after `pub year: u16,` add
   ```rust
       pub original_date: soul_audio_catalog::PackedDate,
       pub release_date: soul_audio_catalog::PackedDate,
   ```
   (Confirm `soul-audio-index`'s `build` module already depends on `soul-audio-catalog`; it does via `AlbumRec`.)
4. `Interner::album` (`:179-203`): add params `original_date: soul_audio_catalog::PackedDate, release_date: soul_audio_catalog::PackedDate` after `year: u16`, and in the `self.albums.push(AlbumMeta { … })` add `original_date,` and `release_date,`.
5. The `Interner::album(...)` call site in `build_catalog` (`:340`): pass `r.original_date, r.release_date` after `r.year`.

- [ ] **Step 6: Bump `SCAN_CACHE_VERSION`**

In `crates/soul-audio-import/src/lib.rs:429`: `pub const SCAN_CACHE_VERSION: u32 = 3;` (was `2`) — `RawTrack` changed shape, so warm caches must be discarded.

- [ ] **Step 7: Add an extraction test**

```rust
#[test]
fn extract_raw_captures_distinct_original_and_release_dates() {
    use soul_audio_catalog::PackedDate;
    // Build a Tags with date=2015 (reissue) and original_date=1973, run extract_raw
    // (reuse the file's existing extract_raw test harness / Tags builder).
    let raw = /* extract_raw over Tags { date: Some("2015".into()),
                 original_date: Some("1973-08-07".into()), .. } */;
    assert_eq!(raw.release_date, PackedDate::from_year(2015));
    assert_eq!(raw.original_date, PackedDate::new(1973, 8, 7));
}
```

- [ ] **Step 8: Run import suite + confirm no regressions**

Run: `cargo test -p soul-audio-import`
Expected: new tests PASS; existing tests (incl. `tests/incremental.rs` byte-identity at `:97`) stay green — the baker still writes `year`, so baked bytes are unchanged this task.

- [ ] **Step 9: Commit**

```bash
git add crates/soul-audio-import/src/lib.rs crates/soul-audio-index/src/lib.rs
git commit -m "feat(import): extract original + release dates per album; bump scan cache to v3"
```

---

## Task 4: `.soulidx` format v5 + dual-date `AlbumRec` + original-keyed order

**Files:**
- Modify: `crates/soul-audio-index/src/lib.rs` — `FORMAT_VERSION` (`:106`), `ALBUM_RECORD_SIZE` (`:110`), doc layout (`:48-50`), baker (`:852-863`), reader `album()` (`:341-359`), `year_at` (`:714-723`), drop `AlbumMeta.year` (`:518`)
- Modify: `crates/soul-audio-catalog/src/lib.rs:95-102` — `AlbumRec` `year:u16` → two `PackedDate`
- Modify: `crates/soul-audio-multicatalog/src/lib.rs` — `Head.year`/population/order (`:193-197`, `:255`, `:284`)
- Modify: `crates/soul-audio-import/src/lib.rs` — drop the now-unused `RawTrack.year`/`Interner::album(year)`/`AlbumMeta.year` threading (make `original_date`/`release_date` the only album dates)
- Tests: `crates/soul-audio-index/tests/roundtrip.rs`, `crates/soul-audio-multicatalog/tests/multicatalog.rs`, `crates/soul-audio-index/benches/reader.rs:112`

**Interfaces:**
- Consumes: `PackedDate` (T1), `AlbumMeta.original_date/release_date` (T3).
- Produces: `AlbumRec { …, pub original_date: PackedDate, pub release_date: PackedDate, pub folder, pub art_id }` (no `year`); `fn album()` reads them at offsets 14/18 with `folder`@22/`art_id`@26; `year_at(albums, i) -> u32` returning `albums[i].original_date.sort_key()`.

- [ ] **Step 1: Write the failing roundtrip test**

In `crates/soul-audio-index/tests/roundtrip.rs`, change the album-year assertion (`:160`) and add a two-date read-back. Replace `assert_eq!(album.year, 2012);` with:

```rust
use soul_audio_catalog::PackedDate;
assert_eq!(album.original_date, PackedDate::new(1973, 8, 7));
assert_eq!(album.release_date, PackedDate::from_year(2015));
```

and set the album's input dates where the test builds `AlbumMeta` (`:57-60` and the other construction sites) to `original_date: PackedDate::new(1973, 8, 7), release_date: PackedDate::from_year(2015)` (replacing `year: 2012`). Add an assertion that `reader.format_version()` (or the header check) is `5` if such an accessor exists; otherwise assert a freshly baked index loads and an old-v4 byte buffer is rejected by `IndexReader::new`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p soul-audio-index --features build roundtrip`
Expected: FAIL — `AlbumRec` has no field `original_date` (compile error), and `AlbumMeta { year: … }` literals no longer match.

- [ ] **Step 3: Migrate `AlbumRec` (catalog)**

`crates/soul-audio-catalog/src/lib.rs:95-102` — replace `pub year: u16,` with:

```rust
pub struct AlbumRec {
    pub id: AlbumId,
    pub title: StrRef,
    pub album_artist: ArtistId,
    pub original_date: PackedDate,
    pub release_date: PackedDate,
    pub folder: FolderId,
    pub art_id: u32,
}
```

(`PackedDate` derives `Copy`/`Default`/`Eq`, so `AlbumRec`'s derives stay valid.)

- [ ] **Step 4: Migrate the index constants, doc, baker, reader, `year_at`, `AlbumMeta`**

`crates/soul-audio-index/src/lib.rs`:
1. `:106` `pub const FORMAT_VERSION: u16 = 5;`
2. `:110` `const ALBUM_RECORD_SIZE: usize = 30;`
3. Doc layout (`:48-50`) — replace the `ALBUM RECORD (24 B …)` block with:
   ```
   //! ALBUM RECORD (30 B, fixed; indexed by dense album id):
   //!    0 id u32 · 4 title.off u32 · 8 title.len u16 · 10 album_artist u32 ·
   //!   14 original_date u32 · 18 release_date u32 · 22 folder u32 · 26 art_id u32
   //!   (original_date/release_date are PackedDate; year u16 replaced in format v5)
   ```
4. Baker (`:859`) — replace `out.extend_from_slice(&meta.year.to_le_bytes());` with:
   ```rust
   out.extend_from_slice(&meta.original_date.0.to_le_bytes());
   out.extend_from_slice(&meta.release_date.0.to_le_bytes());
   ```
   (The following `meta.folder` and `art_id` writes are unchanged in order; the `debug_assert_eq!(out.len(), artists_off)` at `:864` now expects the 30-byte stride via `ALBUM_RECORD_SIZE`.)
5. Reader `album()` (`:352-358`) — replace the `year`/`folder`/`art_id` reads with the shifted offsets:
   ```rust
   album_artist: le_u32(&r, 10),
   original_date: PackedDate(le_u32(&r, 14)),
   release_date: PackedDate(le_u32(&r, 18)),
   folder: le_u32(&r, 22),
   art_id: le_u32(&r, 26),
   ```
   (Add `use soul_audio_catalog::PackedDate;` where needed.)
6. `AlbumMeta` (`:515-520`) — remove `pub year: u16,` (the two `PackedDate` fields added in Task 3 remain).
7. `year_at` (`:714-723`) — key on the original date:
   ```rust
   // Album ORIGINAL-release date for chronological ordering within an artist
   // (the audiophile default). Unknown (0) sorts LAST (u32::MAX). MUST match the
   // multicatalog merge key or the desktop merge diverges from the device order.
   fn year_at(albums: &[AlbumMeta], i: u32) -> u32 {
       albums
           .get(i as usize)
           .map_or(u32::MAX, |a| a.original_date.sort_key())
   }
   ```
   (Its two call sites — `:733` in `artist_album_track` and the genre-order site `:766` — now compare `u32`; no change needed beyond the return type.)

- [ ] **Step 5: Migrate multicatalog `Head`**

`crates/soul-audio-multicatalog/src/lib.rs` — change the `Head.year: u16` field (`:284`) to `original_sort: u32`, populate it from `album.original_date.sort_key()` (`:193-197`/`:255`) instead of `album.year`, and ensure the derived `Ord` uses it exactly where it used `year`. Keep the semantics identical to the index `year_at` (original-date key, unknown→`u32::MAX`). Update the design doc comment (`:178-180`) to say "original date" instead of "year".

- [ ] **Step 6: Drop the transitional `year` in import**

`crates/soul-audio-import/src/lib.rs` — remove `RawTrack.year` (`:254`), the `let year = …` block (`:287-292`), the `year` param from `Interner::album` and its call site (`:340`), and the `year` in the `AlbumMeta { … }` push. `original_date`/`release_date` are now the only album dates. (The `template.rs` `{Year}` token still parses `tags.date` directly — leave it; it does not use `RawTrack.year`.)

- [ ] **Step 7: Update the remaining test/bench literals**

- `crates/soul-audio-multicatalog/tests/multicatalog.rs` — every `AlbumMeta { …, year: N, … }` → `original_date: PackedDate::from_year(N), release_date: PackedDate::from_year(N)` (`:156-159`, `:201-204`, `:424`/`:444-447`, `:510-513`, `:586-589`); the year-ordering assertions (around `:418`) now assert the order produced by the original-date key — since each test album uses `from_year(N)` for both, the existing ordering expectations hold unchanged.
- `crates/soul-audio-index/benches/reader.rs:112` — `year: 1960 + …` → `original_date: PackedDate::from_year(1960 + …), release_date: PackedDate::from_year(1960 + …)`.
- Any other `AlbumMeta { year: … }` / `album.year` the compiler flags → convert the same way.

- [ ] **Step 8: Run the full core suite + no-alloc + byte-identity guards**

Run: `cargo test -p soul-audio-index --features build && cargo test -p soul-audio-catalog && cargo test -p soul-audio-multicatalog && cargo test -p soul-audio-import && cargo test -p architecture-tests`
Expected: all PASS. Specifically: `roundtrip` reads back both dates; `architecture-tests/no_alloc.rs` (reader path) stays non-allocating; `import/incremental.rs:97` byte-identity holds (both bake paths share the new baker).

- [ ] **Step 9: Full local CI gate**

Run: `cargo xtask ci`
Expected: fmt + clippy clean, all tests green, thumbv7em cross-build of `soul-audio-firmware` compiles (the new `album()` reader is no_std/no-alloc; firmware source unchanged).

- [ ] **Step 10: Commit**

```bash
git add crates/soul-audio-index/ crates/soul-audio-catalog/ crates/soul-audio-multicatalog/ crates/soul-audio-import/
git commit -m "feat(index): .soulidx v5 — dual packed album dates, original-keyed order"
```

---

## After all tasks

- Open a PR against `soul-audio-core` `main`; wait for the `gate` + CodeQL + Security checks green; merge with `gh pr merge --squash --delete-branch` (no `--admin`).
- Record the squashed `main` commit SHA — Phase 2 (`-ng`) consumes it via `cargo update -p soul-audio-core --precise <SHA>` across the rune + host lockfiles (and a root-lock reconciliation), per the design spec.

---

## Self-Review

**Spec coverage:** PackedDate (T1 ✓); Field::OriginalDate + per-container readers + owned (T2 ✓); import dual-date + SCAN_CACHE_VERSION 3 (T3 ✓); index v5 record/offsets/version + AlbumRec + year_at + multicatalog original-key (T4 ✓); all test updates + no_alloc/byte-identity guards (T2/T3/T4 ✓); firmware no-change (noted, verified in T4 Step 9). Phase-2/-ng consumption, Settings toggle, display — out of this plan by design (separate Phase 2 plan).

**Placeholder scan:** Two test steps (T2 Step 1, T3 Step 7) reference the file's existing fixture/harness helpers rather than inlining byte-fixture builders, because those helpers (`testfix.rs` frame builders, the `extract_raw` Tags builder) are established patterns the implementer copies from the cited `file:line` (ReplayGain test at `id3v2.rs:1022` / `owned.rs:391`; the existing `extract_raw` tests). This is a deliberate "copy the concrete neighboring pattern" instruction, not a vague placeholder — the assertion shape and inputs are fully specified.

**Type consistency:** `PackedDate` signatures (`new`/`from_year`/`year`/`sort_key`/`UNKNOWN`) are identical across T1 definition and T3/T4 uses. `AlbumMeta` gains `original_date`/`release_date: PackedDate` in T3 and drops `year` in T4 (transitional `year` documented). `AlbumRec` mirrors (`year:u16` → two `PackedDate`) in T4. `year_at` return type changes `u16` → `u32` (sort_key) in T4; both call sites compare `u32`. Baker writes `meta.*.0` (the raw `u32`); reader wraps `PackedDate(le_u32(..))` — consistent with the `.0` public field.
