# `soul-audio-tags` — no-alloc metadata + cover-art crate — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `no_std`, no-alloc crate that reads tags and cover-art descriptors for every format in the library (FLAC, Opus/Ogg, MP3, WAV, DSF, M4A), replaces `lofty` in `soul-audio-service`, surfaces cover art in the desktop UI, and is reusable on the STM32 DAP.

**Architecture:** A streaming *visitor* API (`read<S: SeekSource>(src, scratch, sink)`) over the existing `embedded_io` byte-source seam. Magic-byte locators dispatch to three tag-format readers (ID3v2, VorbisComment, MP4 `ilst`). Cover art is a `{mime, offset, len}` descriptor read on demand. A feature-gated `alloc` layer collects owned `Tags`/`Vec<Picture>` for the desktop.

**Tech Stack:** Rust (edition 2024, stable), `no_std`, `embedded_io`, `heapless` (tests only), `lofty` (differential test dev-dependency only), Tauri 2 + specta (app integration).

**Design spec:** `docs/superpowers/specs/2026-09-03-metadata-tags-crate-design.md`

## Global Constraints

- **`no_std` + no-alloc** on the default build: no `String`/`Vec`/`Box`/`alloc` on the read path. Tag values are `&str` into the caller's `scratch` buffer; cover art is a descriptor. The `alloc` feature (and `std`, which enables `alloc`) gate the owned collector only.
- **Reads through `SeekSource`** (`soul_audio_core::SeekSource`, i.e. `embedded_io::Read + Seek`). Never `std::fs` in the core.
- **Integer-only core**: no `f64`. `StreamFacts { total_frames: Option<u64>, sample_rate: Option<u32>, channels: Option<u16> }`.
- **Byte-wise parsing**: `u32::from_be_bytes`/`from_le_bytes` on slices; no unaligned pointer casts.
- **Panic-free**: malformed/truncated inputs return `TagError`, never panic.
- **Edition** `2024`; the crate inherits nothing from a parent workspace beyond the root `[workspace]` membership.
- **Root gate** (`cargo xtask verify`) must stay green and must build `soul-audio-tags` on `thumbv7em-none-eabihf`.
- Commit messages **must not** contain `Co-Authored-By: Claude` or `Claude-Session` trailers (the lefthook rejects them). Pre-commit runs workspace-wide clippy — API changes commit atomically with consumers.

---

### Task 1: Crate skeleton, public model, and no-alloc/thumbv7em wiring

**Files:**
- Create: `crates/soul-audio-tags/Cargo.toml`
- Create: `crates/soul-audio-tags/src/lib.rs`
- Modify: `Cargo.toml` (root workspace `members`)
- Modify: `xtask/src/main.rs` (add the crate to the `thumbv7em-none-eabihf` build list, near line 78-110)

**Interfaces:**
- Produces: the public model every later task uses —
  `enum Field`, `enum MimeType`, `enum PictureKind`, `enum DataEncoding`,
  `struct Picture`, `struct StreamFacts`, `trait TagSink`, `enum TagError`,
  `fn read<S: SeekSource>(src: &mut S, scratch: &mut [u8], sink: &mut impl TagSink) -> Result<StreamFacts, TagError>`.

- [ ] **Step 1: Create `crates/soul-audio-tags/Cargo.toml`**

```toml
[package]
name = "soul-audio-tags"
version = "0.1.0"
edition = "2024"
description = "no_std, no-alloc audio tag + cover-art reader (ID3v2, VorbisComment, MP4 ilst)."

[features]
default = []
alloc = []
std = ["alloc"]

[dependencies]
soul-audio-core = { path = "../soul-audio-core" }
embedded-io = "0.6"

[dev-dependencies]
soul-audio-core = { path = "../soul-audio-core", features = [] }
```

- [ ] **Step 2: Add the crate to the root workspace members**

In the root `Cargo.toml`, add `"crates/soul-audio-tags"` to the `[workspace] members = [...]` list (keep alphabetical if the list is sorted).

- [ ] **Step 3: Write `crates/soul-audio-tags/src/lib.rs` with the model + a stub `read`**

```rust
//! no_std, no-alloc audio tag + cover-art reader. See the design spec.
#![cfg_attr(not(test), no_std)]   // matches soul-audio-core; std is available under `cargo test`
#![forbid(unsafe_code)]

use soul_audio_core::SeekSource;

/// Normalized tag field, modeled on Symphonia's StandardTag (player subset).
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Title, Artist, Album, AlbumArtist,
    TrackNumber, TrackTotal, DiscNumber, DiscTotal,
    Date, Genre, Composer,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MimeType { Jpeg, Png, Other }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PictureKind { FrontCover, BackCover, Artist, Other }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataEncoding { Raw, Base64 }

/// A cover-art *descriptor* — never the bytes. `Copy`, no borrows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Picture {
    pub usage: PictureKind,
    pub mime: MimeType,
    pub encoding: DataEncoding,
    pub data_offset: u64,
    pub data_len: u32,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Cheap stream facts read from the container header during the same walk.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StreamFacts {
    pub total_frames: Option<u64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
}

/// Streaming visitor. `value`/`raw_key` borrow internal scratch — copy what you keep.
pub trait TagSink {
    fn text(&mut self, field: Option<Field>, raw_key: &str, value: &str);
    fn picture(&mut self, pic: &Picture);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TagError { Io, Malformed, Unsupported }

impl From<embedded_io::ErrorKind> for TagError {
    fn from(_: embedded_io::ErrorKind) -> Self { TagError::Io }
}

/// Read tags + facts from `src`. `scratch` holds one field value at a time.
pub fn read<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let _ = (src, scratch, sink);
    Err(TagError::Unsupported) // replaced in Task 2 by locator dispatch
}
```

- [ ] **Step 4: Add the crate to `xtask verify`'s thumbv7em build list**

In `xtask/src/main.rs`, near the existing `cargo build -p soul-audio-core --target thumbv7em-none-eabihf --no-default-features` entries (~line 78-110), add an analogous entry for `soul-audio-tags`:

```rust
"soul-audio-tags",
```
(Match the exact structure of the surrounding list entries — the same `--target thumbv7em-none-eabihf --no-default-features` invocation.)

- [ ] **Step 5: Write a no-alloc smoke test**

Create `crates/soul-audio-tags/tests/no_alloc.rs`, modeled on `crates/architecture-tests/tests/no_alloc.rs`:

```rust
//! Assert `read` allocates nothing on the read path.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use soul_audio_core::SliceSource;
use soul_audio_tags::{read, Field, Picture, StreamFacts, TagSink};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 { ALLOCS.fetch_add(1, Ordering::SeqCst); unsafe { System.alloc(l) } }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) { unsafe { System.dealloc(p, l) } }
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
    let before = ALLOCS.load(Ordering::SeqCst);
    let mut src = SliceSource::new(&data);
    let _ = read(&mut src, &mut scratch, &mut sink);
    let after = ALLOCS.load(Ordering::SeqCst);
    assert_eq!(before, after, "read allocated on the read path");
    let _ = StreamFacts::default();
}
```

- [ ] **Step 6: Verify it compiles, tests pass, and it builds for thumbv7em**

Run:
```
cargo test -p soul-audio-tags
cargo build -p soul-audio-tags --target thumbv7em-none-eabihf --no-default-features
cargo xtask verify
```
Expected: tests pass; thumbv7em build `Finished`; `verify: OK`.

- [ ] **Step 7: Commit**

```
git add crates/soul-audio-tags Cargo.toml xtask/src/main.rs
git commit -m "feat(tags): crate skeleton, public model, no-alloc + thumbv7em wiring"
```

---

### Task 2: Container locator + dispatch

**Files:**
- Create: `crates/soul-audio-tags/src/locate.rs`
- Modify: `crates/soul-audio-tags/src/lib.rs` (add `mod locate;`, dispatch in `read`)

**Interfaces:**
- Produces: `enum Container { Flac, Ogg, Id3, Wav, Mp4, Dsf, Dff, Unknown }` and
  `fn sniff<S: SeekSource>(src: &mut S) -> Result<Container, TagError>` (reads the leading bytes, rewinds to 0).
- Consumes: the model from Task 1.

- [ ] **Step 1: Write the failing test** — create `crates/soul-audio-tags/src/locate.rs` with a `#[cfg(test)]` module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use soul_audio_core::SliceSource;

    fn sniff_bytes(head: &[u8]) -> Container {
        let mut buf = [0u8; 64];
        buf[..head.len()].copy_from_slice(head);
        let mut src = SliceSource::new(&buf);
        sniff(&mut src).unwrap()
    }

    #[test]
    fn sniffs_each_container_by_magic() {
        assert_eq!(sniff_bytes(b"fLaC"), Container::Flac);
        assert_eq!(sniff_bytes(b"OggS"), Container::Ogg);
        assert_eq!(sniff_bytes(b"ID3\x03"), Container::Id3);
        assert_eq!(sniff_bytes(b"RIFF\0\0\0\0WAVE"), Container::Wav);
        assert_eq!(sniff_bytes(b"\0\0\0\x20ftypM4A "), Container::Mp4);
        assert_eq!(sniff_bytes(b"DSD \0\0\0\0"), Container::Dsf);
        assert_eq!(sniff_bytes(b"FRM8\0\0\0\0\0\0\0\0DSD "), Container::Dff);
        assert_eq!(sniff_bytes(b"garbage!"), Container::Unknown);
    }
}
```

- [ ] **Step 2: Run to verify it fails** — Run: `cargo test -p soul-audio-tags locate` — Expected: FAIL (`sniff`/`Container` not found).

- [ ] **Step 3: Implement `sniff`** in `locate.rs` above the test module:

```rust
use embedded_io::{Read, Seek, SeekFrom};
use soul_audio_core::SeekSource;
use crate::TagError;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Container { Flac, Ogg, Id3, Wav, Mp4, Dsf, Dff, Unknown }

/// Read the leading bytes, classify by magic, then rewind to the start.
pub fn sniff<S: SeekSource>(src: &mut S) -> Result<Container, TagError> {
    let mut head = [0u8; 16];
    let n = read_full(src, &mut head)?;
    src.seek(SeekFrom::Start(0)).map_err(|_| TagError::Io)?;
    let h = &head[..n];
    let c = if h.starts_with(b"fLaC") { Container::Flac }
        else if h.starts_with(b"OggS") { Container::Ogg }
        else if h.starts_with(b"ID3") { Container::Id3 }
        else if h.starts_with(b"RIFF") && h.len() >= 12 && &h[8..12] == b"WAVE" { Container::Wav }
        else if h.len() >= 8 && &h[4..8] == b"ftyp" { Container::Mp4 }
        else if h.starts_with(b"DSD ") { Container::Dsf }
        else if h.starts_with(b"FRM8") && h.len() >= 16 && &h[12..16] == b"DSD " { Container::Dff }
        else { Container::Unknown };
    Ok(c)
}

/// Read up to `buf.len()` bytes; returns how many were read (may be < len at EOF).
pub(crate) fn read_full<S: Read>(src: &mut S, buf: &mut [u8]) -> Result<usize, TagError> {
    let mut off = 0;
    while off < buf.len() {
        match src.read(&mut buf[off..]).map_err(|_| TagError::Io)? {
            0 => break,
            n => off += n,
        }
    }
    Ok(off)
}
```

- [ ] **Step 4: Wire dispatch into `lib.rs`** — add `mod locate;` and replace the stub body of `read`:

```rust
pub fn read<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    match locate::sniff(src)? {
        // Readers land in later tasks; until then they are Unsupported.
        _ => { let _ = (scratch, sink); Err(TagError::Unsupported) }
    }
}
```

- [ ] **Step 5: Run tests** — Run: `cargo test -p soul-audio-tags locate` — Expected: PASS.

- [ ] **Step 6: Commit**

```
git add crates/soul-audio-tags/src
git commit -m "feat(tags): magic-byte container locator + dispatch"
```

---

### Task 3: VorbisComment reader + FLAC container (facts, fields, picture)

**Files:**
- Create: `crates/soul-audio-tags/src/vorbis.rs`
- Modify: `crates/soul-audio-tags/src/lib.rs` (`mod vorbis;`, dispatch `Container::Flac`)

**Interfaces:**
- Produces:
  `fn read_flac<S: SeekSource>(src, scratch, sink) -> Result<StreamFacts, TagError>` and
  `fn read_comment_vector<S: Read>(src, len, scratch, sink) -> Result<(), TagError>`
  (parses a Vorbis comment block body of `len` bytes; used again for Ogg in Task 7).
- `fn map_vorbis_key(key: &str) -> Option<Field>`.
- Consumes: model + `locate::read_full`.

**FLAC byte layout (implementer reference):** after the 4-byte `fLaC` marker, a sequence of METADATA_BLOCK: 1 header byte (`bit7` = last-block flag, `bits0..7` = block type) + 3-byte big-endian body length, then the body. Type 0 = STREAMINFO, 4 = VORBIS_COMMENT, 6 = PICTURE. STREAMINFO body: sample_rate is 20 bits starting at byte offset 10 (`(b[10]<<12)|(b[11]<<4)|(b[12]>>4)`); channels = `((b[12]>>1)&0x7)+1`; total_samples = 36 bits: `((b[13]&0xF) as u64)<<32 | be32(b[14..18])`. VORBIS_COMMENT body (little-endian lengths): u32 vendor_len, vendor bytes, u32 count, then `count` × (u32 comment_len, `KEY=VALUE` UTF-8). PICTURE body (big-endian): u32 type, u32 mime_len, mime ascii, u32 desc_len, desc, u32 width, u32 height, u32 depth, u32 colors, u32 data_len, data. The picture `data_offset` is the absolute source offset of `data`.

- [ ] **Step 1: Write the failing tests** — in `vorbis.rs` `#[cfg(test)]`, with a fixture-builder:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DataEncoding, Field, MimeType, Picture, PictureKind};
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::{vec::Vec, string::String, vec, borrow::ToOwned};

    // Minimal FLAC: fLaC + STREAMINFO(min) + VORBIS_COMMENT + PICTURE(1x1 jpeg stub).
    fn flac(comments: &[&str], with_pic: bool) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"fLaC");
        // STREAMINFO (type 0), 34-byte body, not last.
        v.push(0x00); v.extend_from_slice(&[0, 0, 34]);
        let mut si = [0u8; 34];
        // The 64 bits at si[10..18] pack: sample_rate(20) | channels-1(3) | bps-1(5) | total_samples(36).
        let sr: u32 = 44_100; let ch: u32 = 2; let bps: u32 = 16; let total: u64 = 88_200;
        si[10] = (sr >> 12) as u8;                        // sample_rate bits 19..12
        si[11] = (sr >> 4) as u8;                         // sample_rate bits 11..4
        si[12] = (((sr & 0xF) as u8) << 4)                // sample_rate bits 3..0
               | ((((ch - 1) & 0x7) as u8) << 1)          // channels-1
               | (((bps - 1) >> 4) as u8);                // bps-1 high bit
        si[13] = ((((bps - 1) & 0xF) as u8) << 4)         // bps-1 low 4 bits
               | (((total >> 32) & 0xF) as u8);           // total_samples bits 35..32
        si[14] = (total >> 24) as u8;                     // total_samples bits 31..0
        si[15] = (total >> 16) as u8;
        si[16] = (total >> 8) as u8;
        si[17] = total as u8;
        v.extend_from_slice(&si);
        // VORBIS_COMMENT (type 4)
        let mut vc = Vec::new();
        vc.extend_from_slice(&(6u32).to_le_bytes()); vc.extend_from_slice(b"vendor");
        vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments { vc.extend_from_slice(&(c.len() as u32).to_le_bytes()); vc.extend_from_slice(c.as_bytes()); }
        let last_vc = if with_pic { 0x04 } else { 0x84 };
        v.push(last_vc); v.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]); v.extend_from_slice(&vc);
        if with_pic {
            let data = b"\xFF\xD8\xFF\xEE"; // jpeg stub
            let mut pic = Vec::new();
            pic.extend_from_slice(&(3u32).to_be_bytes()); // type 3 = front cover
            let mime = b"image/jpeg";
            pic.extend_from_slice(&(mime.len() as u32).to_be_bytes()); pic.extend_from_slice(mime);
            pic.extend_from_slice(&(0u32).to_be_bytes()); // desc len
            pic.extend_from_slice(&(1u32).to_be_bytes()); // width
            pic.extend_from_slice(&(1u32).to_be_bytes()); // height
            pic.extend_from_slice(&(24u32).to_be_bytes()); // depth
            pic.extend_from_slice(&(0u32).to_be_bytes()); // colors
            pic.extend_from_slice(&(data.len() as u32).to_be_bytes()); pic.extend_from_slice(data);
            v.push(0x86); v.extend_from_slice(&(pic.len() as u32).to_be_bytes()[1..]); v.extend_from_slice(&pic);
        }
        v
    }

    #[derive(Default)]
    struct Collect { fields: Vec<(Option<Field>, String, String)>, pics: Vec<Picture> }
    impl crate::TagSink for Collect {
        fn text(&mut self, f: Option<Field>, k: &str, val: &str) { self.fields.push((f, k.to_owned(), val.to_owned())); }
        fn picture(&mut self, p: &Picture) { self.pics.push(*p); }
    }

    #[test]
    fn flac_fields_and_facts() {
        let bytes = flac(&["TITLE=Blue Train", "ARTIST=John Coltrane", "ALBUM=Blue Train"], false);
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_flac(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(88_200));
        assert!(c.fields.contains(&(Some(Field::Title), "TITLE".into(), "Blue Train".into())));
        assert!(c.fields.contains(&(Some(Field::Artist), "ARTIST".into(), "John Coltrane".into())));
    }

    #[test]
    fn flac_front_cover_descriptor() {
        let bytes = flac(&["TITLE=x"], true);
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_flac(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(c.pics.len(), 1);
        let p = c.pics[0];
        assert_eq!(p.usage, PictureKind::FrontCover);
        assert_eq!(p.mime, MimeType::Jpeg);
        assert_eq!(p.encoding, DataEncoding::Raw);
        assert_eq!(p.data_len, 4);
        assert_eq!(p.width, Some(1));
        // data_offset points at the 4 jpeg bytes:
        assert_eq!(&bytes[p.data_offset as usize..][..4], b"\xFF\xD8\xFF\xEE");
    }
}
```

- [ ] **Step 2: Run to verify it fails** — Run: `cargo test -p soul-audio-tags vorbis` — Expected: FAIL (`read_flac` not found).

- [ ] **Step 3: Implement `vorbis.rs`** — `map_vorbis_key`, `read_comment_vector`, `read_flac`, using the byte layout above. Track the absolute source offset as you read (seek/position) so `Picture.data_offset` is correct. Map keys case-insensitively: `TITLE`→Title, `ARTIST`→Artist, `ALBUM`→Album, `ALBUMARTIST`→AlbumArtist, `TRACKNUMBER`→TrackNumber, `TRACKTOTAL`/`TOTALTRACKS`→TrackTotal, `DISCNUMBER`→DiscNumber, `DATE`/`YEAR`→Date, `GENRE`→Genre, `COMPOSER`→Composer, else `None`. Split each comment on the first `=`; deliver `text(map_vorbis_key(key), key, value)`. For values larger than `scratch`, truncate at a UTF-8 boundary. Mime string `image/jpeg`→Jpeg, `image/png`→Png, else Other. Picture type 3→FrontCover, 4→BackCover, 8→Artist, else Other. `read_flac` returns `StreamFacts` from STREAMINFO and stops after the last metadata block.

- [ ] **Step 4: Dispatch `Container::Flac`** in `lib.rs` `read`:

```rust
match locate::sniff(src)? {
    locate::Container::Flac => vorbis::read_flac(src, scratch, sink),
    _ => Err(TagError::Unsupported),
}
```

- [ ] **Step 5: Run tests** — Run: `cargo test -p soul-audio-tags vorbis` — Expected: PASS (both tests).

- [ ] **Step 6: Run the full crate + thumbv7em build** — Run: `cargo test -p soul-audio-tags && cargo build -p soul-audio-tags --target thumbv7em-none-eabihf --no-default-features` — Expected: PASS + `Finished`.

- [ ] **Step 7: Commit**

```
git add crates/soul-audio-tags/src
git commit -m "feat(tags): VorbisComment reader + FLAC container (fields, facts, cover art)"
```

---

### Task 4: ID3v2 reader + MP3 container

**Files:**
- Create: `crates/soul-audio-tags/src/id3v2.rs`
- Modify: `crates/soul-audio-tags/src/lib.rs` (`mod id3v2;`, dispatch `Container::Id3`)

**Interfaces:**
- Produces: `fn read_id3v2<S: SeekSource>(src, base_offset: u64, scratch, sink) -> Result<(), TagError>`
  (parses an ID3v2 tag whose header starts at the current position; `base_offset` is that absolute offset so picture `data_offset` is correct). Reused by WAV/DSF (Task 5).
- `fn read_mp3<S: SeekSource>(src, scratch, sink) -> Result<StreamFacts, TagError>`.
- Consumes: model + `locate::read_full`.

**ID3v2 byte layout (implementer reference):** 10-byte header: `"ID3"`, `ver_major` (2/3/4), `ver_rev`, 1 flags byte (`bit7` unsync, `bit6` extended header), 4-byte **synchsafe** size (7 bits per byte: `(b0<<21)|(b1<<14)|(b2<<7)|b3`) = tag size *excluding* the header. Frames (v2.3/2.4): 4-byte frame id, 4-byte size (v2.4 synchsafe, v2.3 plain big-endian), 2 flag bytes, then body. v2.2: 3-byte id, 3-byte size, no flags. Text-frame body: 1 encoding byte (0=Latin-1, 1=UTF-16 w/ BOM, 2=UTF-16BE, 3=UTF-8) then the string. `APIC` body: encoding byte, mime (nul-terminated ascii), 1 picture-type byte, description (nul-terminated per encoding), then image bytes. Map: `TIT2`→Title, `TPE1`→Artist, `TALB`→Album, `TPE2`→AlbumArtist, `TRCK`→TrackNumber(+TrackTotal after `/`), `TPOS`→DiscNumber(+DiscTotal), `TDRC`/`TYER`→Date, `TCON`→Genre, `TCOM`→Composer (v2.2 ids: `TT2`,`TP1`,`TAL`,`TP2`,`TRK`,`TPA`,`TYE`,`TCO`,`TCM`,`PIC`).

- [ ] **Step 1: Write the failing tests** — in `id3v2.rs` `#[cfg(test)]` with a builder for a v2.3 tag holding TIT2/TPE1/TALB/TRCK + APIC:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Field, MimeType, Picture, PictureKind};
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::{vec::Vec, string::String, borrow::ToOwned};

    fn synchsafe(n: u32) -> [u8; 4] {
        [((n >> 21) & 0x7F) as u8, ((n >> 14) & 0x7F) as u8, ((n >> 7) & 0x7F) as u8, (n & 0x7F) as u8]
    }
    fn text_frame(id: &[u8; 4], s: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(id);
        let body_len = 1 + s.len() as u32; // enc byte + utf8
        b.extend_from_slice(&body_len.to_be_bytes()); // v2.3 plain size
        b.extend_from_slice(&[0, 0]); // flags
        b.push(0x03); // UTF-8
        b.extend_from_slice(s.as_bytes());
        b
    }
    fn apic(mime: &str, ptype: u8, data: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(0x03); // enc utf-8
        body.extend_from_slice(mime.as_bytes()); body.push(0); // mime + nul
        body.push(ptype);
        body.push(0); // empty description + nul
        body.extend_from_slice(data);
        let mut b = Vec::new();
        b.extend_from_slice(b"APIC");
        b.extend_from_slice(&(body.len() as u32).to_be_bytes());
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&body);
        b
    }
    fn id3v23(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        for f in frames { body.extend_from_slice(f); }
        let mut v = Vec::new();
        v.extend_from_slice(b"ID3");
        v.extend_from_slice(&[3, 0, 0]); // v2.3, no flags
        v.extend_from_slice(&synchsafe(body.len() as u32));
        v.extend_from_slice(&body);
        v
    }

    #[derive(Default)]
    struct Collect { fields: Vec<(Option<Field>, String, String)>, pics: Vec<Picture> }
    impl crate::TagSink for Collect {
        fn text(&mut self, f: Option<Field>, k: &str, val: &str) { self.fields.push((f, k.to_owned(), val.to_owned())); }
        fn picture(&mut self, p: &Picture) { self.pics.push(*p); }
    }

    #[test]
    fn id3v23_text_and_apic() {
        let tag = id3v23(&[
            text_frame(b"TIT2", "Moment's Notice"),
            text_frame(b"TPE1", "John Coltrane"),
            text_frame(b"TALB", "Blue Train"),
            text_frame(b"TRCK", "2/5"),
            apic("image/png", 3, b"\x89PNG\r\n\x1a\n"),
        ]);
        let mut src = SliceSource::new(&tag);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_id3v2(&mut src, 0, &mut scratch, &mut c).unwrap();
        assert!(c.fields.contains(&(Some(Field::Title), "TIT2".into(), "Moment's Notice".into())));
        assert!(c.fields.contains(&(Some(Field::Artist), "TPE1".into(), "John Coltrane".into())));
        assert!(c.fields.iter().any(|(f, _, v)| *f == Some(Field::TrackNumber) && v == "2"));
        assert!(c.fields.iter().any(|(f, _, v)| *f == Some(Field::TrackTotal) && v == "5"));
        assert_eq!(c.pics.len(), 1);
        assert_eq!(c.pics[0].usage, PictureKind::FrontCover);
        assert_eq!(c.pics[0].mime, MimeType::Png);
        assert_eq!(&tag[c.pics[0].data_offset as usize..][..8], b"\x89PNG\r\n\x1a\n");
    }
}
```

- [ ] **Step 2: Run to verify it fails** — Run: `cargo test -p soul-audio-tags id3v2` — Expected: FAIL.

- [ ] **Step 3: Implement `id3v2.rs`** per the byte layout. Handle v2.2/2.3/2.4 id+size widths and v2.4 synchsafe frame sizes; decode Latin-1/UTF-16(BOM)/UTF-16BE/UTF-8 text into `scratch` as UTF-8 (truncate to fit). For `TRCK`/`TPOS` split on `/` into number/total (emit two `text` calls with the respective `Field`s and the raw key). `APIC` (`PIC` in v2.2): parse mime, picture-type→`PictureKind`, skip description, compute `data_offset = base_offset + <absolute position of image bytes>`, `data_len = frame_body_end - image_start`, `encoding: Raw`. Skip unknown frames by their size. Stop at end of tag size or on a zero-id padding byte.

- [ ] **Step 4: Implement `read_mp3` + dispatch** — `read_mp3` calls `read_id3v2(src, 0, scratch, sink)` if the file starts with `ID3`, then returns `StreamFacts::default()` for now (Xing/duration is a follow-up refinement noted in the spec; leave `total_frames: None`). In `lib.rs` dispatch `Container::Id3 => id3v2::read_mp3(src, scratch, sink)`.

- [ ] **Step 5: Run tests** — Run: `cargo test -p soul-audio-tags id3v2` — Expected: PASS.

- [ ] **Step 6: Commit**

```
git add crates/soul-audio-tags/src
git commit -m "feat(tags): ID3v2 reader (2.2/2.3/2.4) + MP3 container"
```

---

### Task 5: WAV + DSF containers (reuse the ID3v2 reader)

**Files:**
- Create: `crates/soul-audio-tags/src/riff.rs` (WAV chunk walk) and `crates/soul-audio-tags/src/dsf.rs` (DSF header)
- Modify: `crates/soul-audio-tags/src/lib.rs` (`mod riff; mod dsf;`, dispatch `Wav`/`Dsf`)

**Interfaces:**
- Produces: `fn read_wav<S: SeekSource>(src, scratch, sink) -> Result<StreamFacts, TagError>`,
  `fn read_dsf<S: SeekSource>(src, scratch, sink) -> Result<StreamFacts, TagError>`.
- Consumes: `id3v2::read_id3v2`, model.

**Layouts:** *WAV/RIFF*: `RIFF`,u32 le size,`WAVE`, then chunks: 4-byte id + u32 le size + body (padded to even). `fmt ` body: u16 fmt, u16 channels, u32 sample_rate, ... `data` size ÷ block_align = total_frames. An `id3 ` (or `ID3 `) chunk body is an ID3v2 tag → `read_id3v2(src, <abs offset of chunk body>, ...)`. *DSF*: `DSD `, u64 le chunk size, u64 le total file size, u64 le **metadata pointer** at offset 20 (absolute offset of a trailing ID3v2 tag, 0 if none). Then the `fmt ` chunk magic at offset 28. **Authoritative field offsets (match `soul_audio_core::formats::dsf::DsfDecoder`, NOT the earlier draft): u32 le channel_num at offset 52, u32 le sample_rate at offset 56, u64 le sample_count at offset 64.** total_frames = sample_count; sample_rate as-is.

- [ ] **Step 0: Share the ID3v2 test builders** — in Task 4, move `synchsafe`/`text_frame`/`apic`/`id3v23` out of the `#[cfg(test)] mod tests` into a crate-internal `#[cfg(test)] pub(crate) mod testfix;` (new file `crates/soul-audio-tags/src/testfix.rs`, added as `#[cfg(test)] pub(crate) mod testfix;` in `lib.rs`). Task 4's tests and these WAV/DSF tests both `use crate::testfix::*;` — one definition, no duplication.

- [ ] **Step 1: Write failing tests** — in `riff.rs` and `dsf.rs`, builders producing a minimal WAV (fmt+data+`id3 ` chunk with a tiny ID3v2 via `crate::testfix`) and a minimal DSF (28-byte header w/ metadata pointer + fmt chunk + trailing ID3v2 via `crate::testfix`). Assert facts (sample_rate/channels/total_frames) and that a `TIT2`/`TITLE` field is delivered.

```rust
// dsf.rs tests (sketch of the asserts; build the bytes per the layout above)
#[test]
fn dsf_facts_and_trailing_id3() {
    // header: "DSD " + sizes + metadata pointer -> ID3v2 with TIT2="X"
    // fmt chunk sets channels=2, sample_rate=2822400, sample_count=5644800
    let bytes = build_dsf_with_id3("X");
    let mut src = SliceSource::new(&bytes);
    let mut scratch = [0u8; 256];
    let mut c = Collect::default();
    let facts = read_dsf(&mut src, &mut scratch, &mut c).unwrap();
    assert_eq!(facts.sample_rate, Some(2_822_400));
    assert_eq!(facts.channels, Some(2));
    assert_eq!(facts.total_frames, Some(5_644_800));
    assert!(c.fields.iter().any(|(f, _, v)| *f == Some(Field::Title) && v == "X"));
}
```

- [ ] **Step 2: Run to verify they fail** — Run: `cargo test -p soul-audio-tags riff dsf` — Expected: FAIL.

- [ ] **Step 3: Implement `riff.rs` and `dsf.rs`** per the layouts; for the ID3 sub-tag, seek to the tag start and call `id3v2::read_id3v2(src, tag_abs_offset, scratch, sink)`. Return facts from `fmt`/`data` (WAV) and the DSF header/fmt.

- [ ] **Step 4: Dispatch** in `lib.rs`: `Container::Wav => riff::read_wav(...)`, `Container::Dsf => dsf::read_dsf(...)`.

- [ ] **Step 5: Run tests** — Run: `cargo test -p soul-audio-tags` — Expected: PASS (all so far).

- [ ] **Step 6: Commit**

```
git add crates/soul-audio-tags/src
git commit -m "feat(tags): WAV and DSF containers over the ID3v2 reader"
```

---

### Task 6: MP4 `ilst` reader (M4A/AAC/ALAC)

**Files:**
- Create: `crates/soul-audio-tags/src/mp4.rs`
- Modify: `crates/soul-audio-tags/src/lib.rs` (`mod mp4;`, dispatch `Mp4`)

**Interfaces:**
- Produces: `fn read_mp4<S: SeekSource>(src, scratch, sink) -> Result<StreamFacts, TagError>`.

**Layout:** atoms = u32 be size + 4-byte type + body (size includes the 8-byte header; size 1 = 64-bit size follows; size 0 = to EOF). Walk `moov`→`udta`→`meta` (NOTE: `meta` has a 4-byte version/flags prefix before its children)→`ilst`. Each `ilst` entry: an atom whose type is the key (`©nam`,`©ART`,`©alb`,`aART`,`trkn`,`disk`,`©day`,`©gen`,`covr`,`©wrt`) containing a `data` atom: u32 size,`data`,u32 type-indicator,u32 locale, then payload. type-indicator 1 = UTF-8 text; for `trkn`/`disk` the payload is binary `00 00 <track be16> <total be16>`; for `covr` type-indicator 13 = JPEG, 14 = PNG. Facts from `moov/mvhd`: version 0 → u32 timescale (off 12), u32 duration (off 16); version 1 → u32 timescale (off 20), u64 duration (off 24). total_frames = duration; sample_rate = timescale (movie timescale; adequate for the library list).

- [ ] **Step 1: Write the failing test** — build a minimal MP4 with `ftyp` + `moov`(`mvhd`+`udta/meta/ilst` with `©nam`,`©ART`,`trkn`,`covr`) and assert Title/Artist, TrackNumber/TrackTotal, a FrontCover Jpeg descriptor, and facts.

- [ ] **Step 2: Run to verify it fails** — Run: `cargo test -p soul-audio-tags mp4` — Expected: FAIL.

- [ ] **Step 3: Implement `mp4.rs`** — a recursive/iterative atom walker bounded by each atom's size; descend only into container atoms on the `moov/udta/meta/ilst` path; map `ilst` keys→`Field`; for `covr` emit a `Picture` (type-indicator→mime, `usage: FrontCover`, `encoding: Raw`, `data_offset`/`data_len` at the payload); for `trkn`/`disk` emit number+total. Guard every read against the atom bound (no panic on truncation).

- [ ] **Step 4: Dispatch** `Container::Mp4 => mp4::read_mp4(...)`.

- [ ] **Step 5: Run tests** — Run: `cargo test -p soul-audio-tags mp4` — Expected: PASS.

- [ ] **Step 6: Commit**

```
git add crates/soul-audio-tags/src
git commit -m "feat(tags): MP4 ilst reader (M4A/AAC/ALAC)"
```

---

### Task 7: Ogg/Opus container + streaming base64 decoder

**Files:**
- Create: `crates/soul-audio-tags/src/base64.rs`, `crates/soul-audio-tags/src/ogg.rs`
- Modify: `crates/soul-audio-tags/src/lib.rs` (`mod base64; mod ogg;`, dispatch `Ogg`)

**Interfaces:**
- Produces: `fn read_ogg<S: SeekSource>(src, scratch, sink) -> Result<StreamFacts, TagError>`;
  `fn decode_into(input: &[u8], out: &mut [u8]) -> Result<usize, TagError>` (base64, no-alloc).
- Consumes: `vorbis::{read_comment_vector, map_vorbis_key}`.

**Layout:** Ogg page: `OggS`, version, header_type, u64 granule, u32 serial, u32 seq, u8 seg_count, seg_count lacing bytes; page body = sum(lacing). Reassemble the first two packets: packet 1 identifies codec (`OpusHead` → Opus, `\x01vorbis` → Vorbis); packet 2 is the comment header (`OpusTags` then a Vorbis comment vector for Opus; `\x03vorbis` + vector for Vorbis). Parse the vector via `read_comment_vector`. A `METADATA_BLOCK_PICTURE` comment value is base64 of the FLAC PICTURE structure (Task 3 layout) → emit `Picture { encoding: Base64, data_offset = <offset of base64 text>, data_len = <base64 length> }` (the consumer base64-decodes then parses; for v1 the descriptor marks it Base64 and points at the base64 text — desktop `read_picture_bytes` handles decode). Facts: sample_rate from `OpusHead` (input rate, offset 12, u32 le) or Vorbis id header; total_frames from the last page granule (optional; `None` acceptable for v1 if not cheaply available).

- [ ] **Step 1: Write failing tests** — (a) base64: `decode_into(b"aGVsbG8=", out)` → `"hello"`; round-trip a few vectors incl. padding. (b) Ogg/Opus: build a 2-page Opus stream (`OpusHead`+`OpusTags` with `TITLE=...`) and assert the Title field + sample_rate.

- [ ] **Step 2: Run to verify they fail** — Run: `cargo test -p soul-audio-tags base64 ogg` — Expected: FAIL.

- [ ] **Step 3: Implement `base64.rs`** (standard alphabet, `=` padding, reject invalid chars with `TagError::Malformed`) and `ogg.rs` (page walker + packet reassembly into `scratch` or a bounded buffer, then `read_comment_vector`).

- [ ] **Step 4: Dispatch** `Container::Ogg => ogg::read_ogg(...)`.

- [ ] **Step 5: Run tests** — Run: `cargo test -p soul-audio-tags && cargo build -p soul-audio-tags --target thumbv7em-none-eabihf --no-default-features` — Expected: PASS + `Finished`.

- [ ] **Step 6: Commit**

```
git add crates/soul-audio-tags/src
git commit -m "feat(tags): Ogg/Opus container + no-alloc streaming base64 decoder"
```

---

### Task 8: `std`/`alloc` convenience layer

**Files:**
- Create: `crates/soul-audio-tags/src/owned.rs`
- Modify: `crates/soul-audio-tags/src/lib.rs` (`#[cfg(feature = "alloc")] mod owned;` + re-export)
- Modify: `crates/soul-audio-tags/Cargo.toml` (nothing new; `alloc`/`std` features already declared)

**Interfaces:**
- Produces (under `feature = "alloc"`):
  `struct Tags { title, artist, album, album_artist: Option<String>, track, track_total, disc, disc_total: Option<u32>, date, genre, composer: Option<String>, pictures: Vec<Picture> }`,
  `fn read_owned<S: SeekSource>(src) -> Result<(Tags, StreamFacts), TagError>`,
  `fn read_picture_bytes<S: SeekSource>(src, p: &Picture) -> Result<Vec<u8>, TagError>`.

- [ ] **Step 1: Write the failing test** (gated `#[cfg(feature = "alloc")]`) — over the Task 3 FLAC fixture, assert `read_owned` returns `Tags { title: Some("Blue Train"), artist: Some("John Coltrane"), .. }` and that `read_picture_bytes` on the front-cover `Picture` returns the 4 stub jpeg bytes.

- [ ] **Step 2: Run to verify it fails** — Run: `cargo test -p soul-audio-tags --features alloc owned` — Expected: FAIL.

- [ ] **Step 3: Implement `owned.rs`** — an `OwnedTags: TagSink` that maps each `Field` into the struct (parsing `Track/DiscNumber`/`Total` values to `u32`) and pushes `Picture`s; `read_owned` allocates a 256-byte scratch, runs `read`, returns `(Tags, facts)`. `read_picture_bytes` seeks to `p.data_offset`, reads `p.data_len` bytes; if `p.encoding == Base64`, base64-decode (using `base64::decode_into` into a `Vec` sized `len*3/4`) then, for Ogg `METADATA_BLOCK_PICTURE`, parse the inner FLAC PICTURE to return just the image bytes. Add `extern crate alloc;` at the crate root under the feature.

- [ ] **Step 4: Run tests** — Run: `cargo test -p soul-audio-tags --features alloc` — Expected: PASS. Also confirm the default build is still no-alloc: `cargo build -p soul-audio-tags --target thumbv7em-none-eabihf --no-default-features` → `Finished`.

- [ ] **Step 5: Commit**

```
git add crates/soul-audio-tags/src crates/soul-audio-tags/Cargo.toml
git commit -m "feat(tags): std/alloc convenience layer (Tags collector, owned picture bytes)"
```

---

### Task 9: Differential test against lofty

**Files:**
- Create: `crates/soul-audio-tags/tests/differential.rs`
- Create: `crates/soul-audio-tags/testdata/` (a few tiny tagged fixtures per format, committed)
- Modify: `crates/soul-audio-tags/Cargo.toml` (`[dev-dependencies] lofty = "0.22"`)

**Interfaces:** consumes `read_owned` (feature `std`).

- [ ] **Step 1: Add committed fixtures** — one tiny tagged file per format under `testdata/` (`a.flac`, `a.mp3`, `a.dsf`, `a.m4a`, `a.ogg`, `a.wav`), each with known title/artist/album (document the expected values in a `testdata/README.md`).

- [ ] **Step 2: Write the differential test** (`#[cfg(feature = "std")]`) — for each fixture, read with `lofty::read_from_path` and with our `read_owned` (over `std::fs::File`, which is `Read + Seek`), and assert the normalized title/artist/album/track match. Skip a field only where lofty returns `None`.

```rust
#[cfg(feature = "std")]
#[test]
fn matches_lofty_on_fixtures() {
    for name in ["a.flac", "a.mp3", "a.dsf", "a.m4a", "a.ogg", "a.wav"] {
        let path = concat_testdata(name);
        let lof = lofty_fields(&path);           // (title, artist, album) via lofty
        let mut f = std::fs::File::open(&path).unwrap();
        let (tags, _facts) = soul_audio_tags::read_owned(&mut f).unwrap();
        assert_eq!(tags.title, lof.0, "{name} title");
        assert_eq!(tags.artist, lof.1, "{name} artist");
        assert_eq!(tags.album, lof.2, "{name} album");
    }
}
```

- [ ] **Step 3: Run it** — Run: `cargo test -p soul-audio-tags --features std --test differential` — Expected: PASS. Fix parser discrepancies until green.

- [ ] **Step 4: (Optional local corpus)** — document in the test how to point it at `D:\content\music` via an env var to run parity over the real 1300-file library locally (not in CI).

- [ ] **Step 5: Commit**

```
git add crates/soul-audio-tags/tests crates/soul-audio-tags/testdata crates/soul-audio-tags/Cargo.toml
git commit -m "test(tags): differential parity test vs lofty on committed fixtures"
```

---

### Task 10: Service integration — replace lofty in the scan

**Files:**
- Modify: `crates/soul-audio-service/Cargo.toml` (drop `lofty`, add `soul-audio-tags = { path = "../soul-audio-tags", features = ["std"] }`)
- Modify: `crates/soul-audio-service/src/library.rs` (`read_tags` + `SUPPORTED` + `Track`)
- Modify: `crates/soul-audio-service/src/types.rs` (`TrackView` gains `has_art: bool`)

**Interfaces:**
- Consumes: `soul_audio_tags::{read_owned, read_picture_bytes, Tags, StreamFacts, Picture}`.
- Produces: `Track { ..., has_art: bool }`, `TrackView { ..., has_art: bool }`, and a `Library::picture_of(id) -> Option<(&Path, Picture)>` helper for the app command (Task 11).

- [ ] **Step 1: Write/adjust failing tests** — extend `library.rs` tests: `scan_reads_tags_from_a_tagged_file` should still pass using the new reader (keep the committed `testdata/tagged.flac`); add `scan_sets_has_art_when_cover_present` over a fixture with embedded art (copy `crates/soul-audio-tags/testdata/a.flac` if it has art, or add one). Assert `views()[0].has_art == true` and title/artist/album read.

- [ ] **Step 2: Run to verify the new assertion fails** — Run: `cargo test -p soul-audio-service library` — Expected: FAIL on `has_art` (field missing / not set).

- [ ] **Step 3: Rewrite `read_tags`** to use the tags crate:

```rust
// library.rs
use soul_audio_tags::{read_owned, StreamFacts};

fn read_tags(path: &Path) -> (Option<String>, Option<String>, Option<String>, f64, bool) {
    let Ok(mut f) = std::fs::File::open(path) else { return (None, None, None, 0.0, false); };
    match read_owned(&mut f) {
        Ok((tags, facts)) => {
            let dur = duration_secs(&facts);
            let has_art = !tags.pictures.is_empty();
            (tags.title, tags.artist, tags.album, dur, has_art)
        }
        Err(_) => (None, None, None, 0.0, false),
    }
}

fn duration_secs(f: &StreamFacts) -> f64 {
    match (f.total_frames, f.sample_rate) {
        (Some(n), Some(r)) if r > 0 => n as f64 / r as f64,
        _ => 0.0,
    }
}
```
Add `"opus"` to `SUPPORTED`. Add `has_art: bool` to `Track` and set it from `read_tags`. Store the front-cover `Picture` on `Track` too (e.g. `art: Option<Picture>`) so Task 11 can fetch bytes without re-scanning tags — or add `Library::picture_of(id)` that re-reads on demand (simpler; re-reading one file is cheap). Choose the re-read approach: implement `Library::path_of` is already there; Task 11 re-reads.

- [ ] **Step 4: Add `has_art` to `TrackView`** (`types.rs`) and set it in `Library::views()`.

- [ ] **Step 5: Run tests** — Run: `cargo test -p soul-audio-service` — Expected: PASS. Then `cargo xtask verify` — Expected: `verify: OK` (lofty removed from the default build).

- [ ] **Step 6: Regenerate the app's TS bindings** (TrackView changed) — Run: `cd app/src-tauri && SOUL_PLAYER_CODEGEN=1 cargo run` (the debug codegen path writes `bindings.ts` and exits). Verify `app/src/lib/bindings.ts` now has `has_art` on `TrackView`.

- [ ] **Step 7: Commit**

```
git add crates/soul-audio-service app/src/lib/bindings.ts
git commit -m "feat(service): read tags+art via soul-audio-tags, drop lofty; TrackView.has_art"
```

---

### Task 11: App integration — `track_art` command + now-playing cover

**Files:**
- Modify: `app/src-tauri/src/commands.rs` (new `track_art` command)
- Modify: `app/src-tauri/src/ipc.rs` (register the command in the specta builder)
- Modify: `app/src/lib/ipc.ts`, `app/src/lib/bindings.ts` (regenerated), `app/src/views/PlayerBar.svelte`

**Interfaces:**
- Produces: `#[tauri::command] fn track_art(state, id: u32) -> Result<Option<TrackArt>, String>` where `TrackArt { mime: String, bytes: Vec<u8> }` (specta `Type`).

- [ ] **Step 1: Add the `TrackArt` type + command** in `commands.rs`:

```rust
#[derive(serde::Serialize, specta::Type)]
pub struct TrackArt { pub mime: String, pub bytes: Vec<u8> }

/// Read the front-cover art for a track id, if any.
#[tauri::command]
#[specta::specta]
pub fn track_art(state: State<'_, PlayerState>, id: u32) -> Result<Option<TrackArt>, String> {
    let Some(lib) = state.library() else { return Ok(None); };
    let Some(path) = lib.path_of(id).map(|p| p.to_owned()) else { return Ok(None); };
    match soul_audio_tags::read_owned(&mut std::fs::File::open(&path).map_err(|e| e.to_string())?) {
        Ok((tags, _)) => {
            let Some(pic) = tags.pictures.iter().find(|p| matches!(p.usage, soul_audio_tags::PictureKind::FrontCover))
                .or_else(|| tags.pictures.first()) else { return Ok(None); };
            let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
            let bytes = soul_audio_tags::read_picture_bytes(&mut f, pic).map_err(|_| "read art".to_string())?;
            let mime = match pic.mime { soul_audio_tags::MimeType::Png => "image/png", _ => "image/jpeg" }.to_string();
            Ok(Some(TrackArt { mime, bytes }))
        }
        Err(_) => Ok(None),
    }
}
```
Add `soul-audio-tags = { path = "../../crates/soul-audio-tags", features = ["std"] }` to `app/src-tauri/Cargo.toml`.

- [ ] **Step 2: Register + regenerate bindings** — add `track_art` to the `collect_commands!`/builder in `ipc.rs`; run `cd app/src-tauri && SOUL_PLAYER_CODEGEN=1 cargo run`; confirm `commands.trackArt` + `TrackArt` appear in `app/src/lib/bindings.ts`.

- [ ] **Step 3: Frontend wrapper** — in `app/src/lib/ipc.ts`:

```ts
import { commands, type TrackArt } from "./bindings";
export async function trackArt(id: number): Promise<TrackArt | null> {
  const r = await commands.trackArt(id);
  return r.status === "error" ? null : r.data;
}
```

- [ ] **Step 4: Show the cover in `PlayerBar.svelte`** — when `player.nowPlayingId` changes and the current track's `has_art` is true, call `trackArt(id)`, build a Blob URL (`URL.createObjectURL(new Blob([new Uint8Array(bytes)], { type: mime }))`), and render it in the now-playing area; revoke the previous URL on change; show a neutral placeholder when `has_art` is false or the fetch returns null.

- [ ] **Step 5: Verify (manual device QA)** — Run the app (`npm run tauri dev`), open the DSF folder `D:\content\music\City Pop\Hiroshi Sato\Orient` and a FLAC album; confirm title/artist/album now populate, duration shows, and the now-playing cover renders. Run `svelte-check` (`cd app && npm run check`) → clean. Run `cargo clippy` in `app/src-tauri` → clean.

- [ ] **Step 6: Commit**

```
git add app/src-tauri app/src crates/soul-audio-tags
git commit -m "feat(app): track_art command + now-playing cover art in PlayerBar"
```

---

## Self-Review

**Spec coverage:**
- no-alloc crate + model → Task 1. ✓
- Locator/dispatch → Task 2. ✓
- Three tag readers (VorbisComment/ID3v2/MP4) → Tasks 3, 4, 6. ✓
- All library formats (FLAC/MP3/DSF/WAV/M4A/Opus) → Tasks 3-7. ✓
- Cover art descriptors + on-demand bytes → Tasks 3/4/6/7 (descriptors) + Task 8 (`read_picture_bytes`). ✓
- Integer `StreamFacts` incl. duration → Tasks 3/5/6 (MP3 duration deferred, per spec §4/§10). ✓
- Streaming base64 → Task 7. ✓
- `std` collector + drop lofty → Tasks 8, 10. ✓
- `.opus` scanning + `has_art` + `track_art` + PlayerBar cover → Tasks 10, 11. ✓
- Firmware reuse (thumbv7em in gate, no-alloc test) → Task 1 (wiring), re-checked in Tasks 3/7/8. ✓
- Differential test vs lofty → Task 9. ✓
- Robustness (never panic) → implementation notes in every reader task; covered by malformed-input guards.

**Notes / deliberate deferrals (from the spec's "out of scope"):** MP3 Xing duration is left `None` in Task 4 (approximation noted); DFF text chunks, RIFF `INFO`, per-row thumbnails, DAP screen wiring, and DSD playback are not in this plan.

**Type consistency:** `read` signature, `TagSink::{text,picture}`, `Picture`/`StreamFacts`/`Field` names are used identically across Tasks 1-11. `read_id3v2` takes `base_offset` (Task 4) and is called with the chunk/tag absolute offset in Task 5. `read_comment_vector`/`map_vorbis_key` (Task 3) are reused in Task 7. `read_owned`/`read_picture_bytes` (Task 8) are consumed in Tasks 10-11. `has_art` is added in Task 10 and consumed in Task 11.
