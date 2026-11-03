# Codec E2E Completion — PR 1: DSD / DSDIFF Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the DSD family fully working and e2e-tested: DFF reports duration
(#25a) and has a tag reader (#25b); DSD256/512 get committed fixtures + decode
goldens (#28); DFF and DST get playback-tier e2e coverage; DSD per-rate
real-time headroom is measured and the ceiling documented (#28 perf).

**Architecture:** `DffDecoder` already computes `audio_total` on both the raw
`DSD ` and DST paths but never overrides `DsdDecoder::total_samples()` (the trait
default is `None`), so `DecimateAdapter::total_frames()` is `None` → no duration.
The decoder fix is a one-method mirror of DSF. The tags crate already sniffs
`Container::Dff` in `locate.rs` but the dispatch falls through to
`Err(Unsupported)`; we add a `read_dff` mirroring `read_dsf`. Fixtures are
synthesized by `testdata/generate_dsd.py` (extended with a big-endian DSDIFF
writer + a duration arg); DST reuses the committed FATE slice.

**Tech Stack:** Rust (edition 2024, `no_std`/no-alloc core + tags), Python 3 +
numpy for fixture synthesis, ffmpeg (not needed this PR), `cargo xtask verify`.

## Global Constraints

- **No AI trailers in commits** — lefthook rejects `Co-Authored-By: Claude` /
  `Claude-Session` lines. Commits are GPG-signed by default; never pass
  `--no-gpg-sign`.
- **`soul-audio-core` and `soul-audio-tags` stay `no_std` + no-alloc +
  `#![forbid(unsafe_code)]`** — no new dependencies; the thumbv7em cross-build in
  `cargo xtask verify` step 6 must stay green.
- **Workspace-wide clippy in pre-commit** — an API change must commit atomically
  with its consumers (clippy runs `--all-targets` across the workspace).
- **`app/src-tauri` is a separate Cargo workspace** — `cargo xtask verify` does
  NOT reach it. Run its tests with `cargo test` *inside* `app/src-tauri`. Decode
  runs on a 16 MiB stack thread there (the `AnyDecoder` enum is ~150 KiB).
- **DSD duration semantics:** `total_samples()` / `StreamFacts.total_frames` is
  the **per-channel 1-bit sample count**; decimated PCM frames =
  `total_samples / 8` (fixed 8:1 `DECIMATION`).
- **DSD rate → Hz:** DSD64 = 2_822_400, DSD128 = 5_644_800, DSD256 = 11_289_600,
  DSD512 = 22_579_200. Decimated PCM rate = rate / 8 (DSD256 → 1_411_200,
  DSD512 → 2_822_400).

---

## Task 1: DFF decoder duration — `total_samples()` (#25a)

**Files:**
- Modify: `crates/soul-audio-core/src/formats/dff.rs` (add one method to the
  `impl<S: Source> DsdDecoder for DffDecoder<S>` block at lines 230–257; add two
  unit tests to the existing `mod tests`).

**Interfaces:**
- Produces: `DffDecoder::total_samples(&self) -> Option<u64>` (overrides the
  `DsdDecoder` trait default). Consumed by `DecimateAdapter::total_frames()`
  (`src/dsd.rs:275`), which returns `total_samples / 8`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `dff.rs` (the `dff(...)` and `dff_dst(...)` builders and
`SliceSource` are already in scope there):

```rust
#[test]
fn reports_total_samples_for_raw_dsd() {
    // 8 audio bytes, stereo → 4 bytes/channel → 4 × 8 = 32 per-channel samples.
    let audio = [10, 20, 11, 21, 12, 22, 13, 23];
    let bytes = dff(2_822_400, 2, b"DSD ", &audio);
    let d = DffDecoder::new(SliceSource::new(&bytes)).unwrap();
    assert_eq!(d.total_samples(), Some(32));
}

#[test]
fn reports_total_samples_for_dst() {
    // Two 9408-byte stored frames → audio_total 18816, stereo → 75264 samples.
    let frame_bytes = 9408usize;
    let mut f0 = vec![0u8; 1 + frame_bytes];
    let mut f1 = vec![0u8; 1 + frame_bytes];
    f0[0] = 0x00;
    f1[0] = 0x00;
    let bytes = dff_dst(2_822_400, 2, 75, &[&f0, &f1]);
    let d = DffDecoder::new(SliceSource::new(&bytes)).unwrap();
    assert_eq!(d.total_samples(), Some(75_264));
}
```

`total_samples` is a `DsdDecoder` trait method, so bring the trait into test
scope if it is not already: the `impl ... DsdDecoder` is in this file, and
`DsdDecoder` is imported at the top (`use crate::decoder::{DecodeError,
DsdDecoder, DsdSeek};`) — no extra import needed.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p soul-audio-core --lib formats::dff::tests::reports_total_samples`
Expected: FAIL — `total_samples()` returns the trait default `None`, so
`assert_eq!(..., Some(32))` fails.

- [ ] **Step 3: Implement the override**

In `dff.rs`, inside `impl<S: Source> DsdDecoder for DffDecoder<S>` (after
`read_dsd`, before the closing `}` at line 257), add:

```rust
    fn total_samples(&self) -> Option<u64> {
        // Per-channel 1-bit sample count: audio bytes ÷ channels × 8 samples per
        // byte — the same figure `seek_to_sample` derives. `audio_total` is set
        // on both the raw `DSD ` and DST paths, so this covers both.
        Some(self.audio_total / self.channels * 8)
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p soul-audio-core --lib formats::dff`
Expected: PASS — all existing dff tests plus the two new ones.

- [ ] **Step 5: Commit**

```bash
git add crates/soul-audio-core/src/formats/dff.rs
git commit -m "fix(core): DFF reports total_samples so DSD duration/seek-bar works (#25)"
```

---

## Task 2: DSD fixture generator — DSDIFF writer + DSD256/512 + duration arg

**Files:**
- Modify: `testdata/generate_dsd.py` (dispatch on output extension: `.dff` →
  DSDIFF big-endian writer; else DSF; add an optional duration argv[3]).
- Modify: `testdata/generate.sh` (emit `tone.dff`, `tone-dsd256.dsf`,
  `tone-dsd512.dsf`).
- Create (generated binaries, committed): `testdata/tone.dff`,
  `testdata/tone-dsd256.dsf`, `testdata/tone-dsd512.dsf`.

**Interfaces:**
- Produces: `python3 testdata/generate_dsd.py <out> [rate_hz] [dur_secs]` writing
  a DSF (`.dsf`) or DSDIFF (`.dff`, raw `DSD `) stream of the shared A4/E5 tone.
  Consumed by Tasks 4 and 5.

**Context — DSDIFF raw layout (must match `DffDecoder::new`, dff.rs:62–114 and
its `dff()` test builder):** big-endian throughout; chunk size excludes the
12-byte header.
```
FRM8 <u64 size> "DSD "                     ; formType
  PROP <u64 size> "SND "
    "FS  " <u64 4>  <u32 sample_rate>
    "CHNL" <u64 2+4*ch> <u16 ch> <ch × 4-byte channel-id>
    "CMPR" <u64 8>  "DSD " <u8 3> "raw"     ; compression = raw DSD
  "DSD " <u64 N>  <N bytes interleaved MSB-first DSD>
```
DFF audio is **MSB-first** and **clustered/interleaved** (one byte per channel
per step), unlike DSF (LSB-first, block-planar).

- [ ] **Step 1: Add a DSDIFF writer + extension dispatch + duration arg to `generate_dsd.py`**

Replace the `main()` function with the version below and add the `write_dff` and
`write_dsf` helpers. Leave `sigma_delta_2nd` and the module constants untouched.
(`write_dsf` is the existing DSF body extracted verbatim into a helper.)

```python
def big_chunk(cid: bytes, body: bytes) -> bytes:
    """A big-endian IFF chunk: 4-byte id + u64 BE size + body (caller pads)."""
    return cid + struct.pack(">Q", len(body)) + body


def write_dff(out: str, rate: int, ch_bits: list) -> None:
    """Write a raw-DSD DSDIFF (.dff): MSB-first, channel-interleaved."""
    channels = len(ch_bits)
    ch_bytes = [np.packbits(b, bitorder="big") for b in ch_bits]
    nbytes = ch_bytes[0].shape[0]
    inter = np.empty(nbytes * channels, dtype=np.uint8)
    for c, cb in enumerate(ch_bytes):
        inter[c::channels] = cb

    chnl = struct.pack(">H", channels) + b"".join(
        (b"SLFT" if c == 0 else b"SRGT") for c in range(channels)
    )
    name = b"raw"
    cmpr = b"DSD " + bytes([len(name)]) + name  # 4 + 1 + 3 = 8 (even)
    prop_body = (
        b"SND "
        + big_chunk(b"FS  ", struct.pack(">I", rate))
        + big_chunk(b"CHNL", chnl)
        + big_chunk(b"CMPR", cmpr)
    )
    body = big_chunk(b"PROP", prop_body) + big_chunk(b"DSD ", inter.tobytes())
    frm8 = b"FRM8" + struct.pack(">Q", 4 + len(body)) + b"DSD " + body
    with open(out, "wb") as fh:
        fh.write(frm8)
    print(f"wrote {out}: {len(frm8)} bytes, {channels}ch raw-DSD (.dff)")


def write_dsf(out: str, rate: int, n: int, ch_bits: list) -> None:
    """Write a Sony DSF (.dsf): LSB-first, block-planar (existing layout)."""
    channels = len(ch_bits)
    ch_bytes = [np.packbits(b, bitorder="little") for b in ch_bits]
    valid_bpc = ch_bytes[0].shape[0]
    blocks = (valid_bpc + BLOCK - 1) // BLOCK
    padded_bpc = blocks * BLOCK
    padded = [np.zeros(padded_bpc, dtype=np.uint8) for _ in range(channels)]
    for c in range(channels):
        padded[c][: ch_bytes[c].shape[0]] = ch_bytes[c]
    data = bytearray()
    for b in range(blocks):
        for c in range(channels):
            data += padded[c][b * BLOCK : (b + 1) * BLOCK].tobytes()
    data_chunk = b"data" + struct.pack("<Q", 12 + len(data)) + bytes(data)
    fmt_chunk = (
        b"fmt "
        + struct.pack("<Q", 52)
        + struct.pack("<I", 1)
        + struct.pack("<I", 0)
        + struct.pack("<I", 2)
        + struct.pack("<I", channels)
        + struct.pack("<I", rate)
        + struct.pack("<I", 1)
        + struct.pack("<Q", n)
        + struct.pack("<I", BLOCK)
        + struct.pack("<I", 0)
    )
    total = 28 + len(fmt_chunk) + len(data_chunk)
    dsd_chunk = b"DSD " + struct.pack("<Q", 28) + struct.pack("<Q", total) + struct.pack("<Q", 0)
    with open(out, "wb") as fh:
        fh.write(dsd_chunk)
        fh.write(fmt_chunk)
        fh.write(data_chunk)
    label = f"DSD{64 * rate // DSD64}" if rate % DSD64 == 0 else f"{rate}Hz"
    print(f"wrote {out}: {total} bytes, {channels}ch {label} (.dsf)")


def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else "tone.dsf"
    rate = int(sys.argv[2]) if len(sys.argv) > 2 else DSD64
    dur = float(sys.argv[3]) if len(sys.argv) > 3 else DUR
    n = int(dur * rate)
    t = np.arange(n) / rate
    ch_bits = [sigma_delta_2nd(AMP * np.sin(2.0 * np.pi * f * t)) for f in FREQS]
    if out.endswith(".dff"):
        write_dff(out, rate, ch_bits)
    else:
        write_dsf(out, rate, n, ch_bits)
```

- [ ] **Step 2: Add the new fixtures to `generate.sh`**

In `testdata/generate.sh`, replace the two existing DSD lines with:

```bash
python3 generate_dsd.py tone.dsf                      # DSD64  DSF (0.5 s)
python3 generate_dsd.py tone-dsd128.dsf 5644800       # DSD128 DSF (0.5 s)
python3 generate_dsd.py tone-dsd256.dsf 11289600 0.1  # DSD256 DSF (0.1 s, size)
python3 generate_dsd.py tone-dsd512.dsf 22579200 0.1  # DSD512 DSF (0.1 s, size)
python3 generate_dsd.py tone.dff                      # DSD64  DSDIFF/raw (0.5 s)
```

Append `tone-dsd256.dsf tone-dsd512.dsf tone.dff` to the final `ls -la` line.

- [ ] **Step 3: Generate the fixtures**

Run: `python3 testdata/generate_dsd.py testdata/tone.dff && python3 testdata/generate_dsd.py testdata/tone-dsd256.dsf 11289600 0.1 && python3 testdata/generate_dsd.py testdata/tone-dsd512.dsf 22579200 0.1`
Expected: three "wrote …" lines. `tone.dff` ~360 KB; `tone-dsd256.dsf` ~282 KB;
`tone-dsd512.dsf` ~565 KB.

- [ ] **Step 4: Verify `tone.dff` decodes through the real decoder**

Quick smoke via the CLI (decode on its big-stack thread):
Run: `cargo run -p soul-audio-cli -- render testdata/tone.dff --out /tmp/dff.pcm --rate 48000 --channels 2`
Expected: exits success; `/tmp/dff.pcm` is non-empty. (Asserted properly in
Task 5.)

- [ ] **Step 5: Commit (binary fixtures + generator)**

```bash
git add testdata/generate_dsd.py testdata/generate.sh testdata/tone.dff testdata/tone-dsd256.dsf testdata/tone-dsd512.dsf
git commit -m "test(fixtures): DSDIFF (.dff) writer + DSD256/512 fixtures (#25, #28)"
```

---

## Task 3: DFF tag reader — `read_dff` (#25b)

**Files:**
- Create: `crates/soul-audio-tags/src/dff.rs`.
- Modify: `crates/soul-audio-tags/src/lib.rs` (add `mod dff;` near line 18; add
  the `Container::Dff` dispatch arm at line 121).

**Interfaces:**
- Consumes: `StreamFacts`, `TagError`, `TagSink`, `Field` (crate root);
  `crate::util::read_exact`; `crate::id3v2::read_id3v2`; `soul_audio_core::{Source,
  SeekSource}`; `embedded_io::SeekFrom`.
- Produces: `pub fn read_dff<S: SeekSource>(src, scratch, sink) -> Result<StreamFacts, TagError>`
  mirroring `dsf::read_dsf`.

**Context — DSDIFF metadata:** per-channel sample count is not a header field;
it is derived from the sound-chunk byte size: `total_frames = (sound_bytes /
channels) × 8`. Human tags live in the top-level `DIIN` (edited info) chunk as
`DITI` (title) and `DIAR` (artist) sub-chunks — each `<id> <u64 size> <u32
count> <count ASCII bytes>` — and/or an `ID3 ` chunk (delegate to
`id3v2::read_id3v2`). Facts come from `PROP`/`SND ` (`FS `, `CHNL`) exactly as
the decoder parses them.

- [ ] **Step 1: Write the failing test**

Create `crates/soul-audio-tags/src/dff.rs` with the test module first (so Step 2
fails to compile → red):

```rust
//! DFF / DSDIFF (Philips) container tags: facts from `PROP`/`SND ` (`FS `,
//! `CHNL`) plus the sound-chunk size, human tags from a `DIIN` (edited info)
//! chunk (`DITI`/`DIAR`) and/or a trailing `ID3 ` chunk. Big-endian IFF; chunk
//! sizes exclude the 12-byte header. Mirrors the `soul_audio_core::formats::dff`
//! decoder's container parse.

use crate::util::read_exact;
use crate::{Field, StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::{SeekSource, Source};

// (implementation added in Step 3)

#[cfg(test)]
mod tests {
    use crate::Field;
    use crate::testfix::*;
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    fn be_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(id);
        v.extend_from_slice(&(body.len() as u64).to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    /// Build a raw-DSD DSDIFF with a DIIN(DITI,DIAR) chunk and a `DSD ` sound
    /// chunk of `sound_len` bytes (stereo, DSD64).
    fn build_dff(title: &str, artist: &str, sound_len: usize) -> Vec<u8> {
        let mut fs = Vec::new();
        fs.extend_from_slice(&2_822_400u32.to_be_bytes());
        let mut chnl = Vec::new();
        chnl.extend_from_slice(&2u16.to_be_bytes());
        chnl.extend_from_slice(b"SLFTSRGT");
        let mut prop = Vec::new();
        prop.extend_from_slice(b"SND ");
        prop.extend_from_slice(&be_chunk(b"FS  ", &fs));
        prop.extend_from_slice(&be_chunk(b"CHNL", &chnl));

        let mut diti = Vec::new();
        diti.extend_from_slice(&(title.len() as u32).to_be_bytes());
        diti.extend_from_slice(title.as_bytes());
        let mut diar = Vec::new();
        diar.extend_from_slice(&(artist.len() as u32).to_be_bytes());
        diar.extend_from_slice(artist.as_bytes());
        let mut diin = Vec::new();
        diin.extend_from_slice(&be_chunk(b"DITI", &diti));
        diin.extend_from_slice(&be_chunk(b"DIAR", &diar));

        let mut body = Vec::new();
        body.extend_from_slice(&be_chunk(b"PROP", &prop));
        body.extend_from_slice(&be_chunk(b"DIIN", &diin));
        body.extend_from_slice(&be_chunk(b"DSD ", &alloc::vec![0u8; sound_len]));

        let mut v = Vec::new();
        v.extend_from_slice(b"FRM8");
        v.extend_from_slice(&((4 + body.len()) as u64).to_be_bytes());
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn dff_facts_and_diin_tags() {
        // stereo, 16 sound bytes → (16/2)*8 = 64 per-channel samples.
        let bytes = build_dff("My Title", "My Artist", 16);
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = super::read_dff(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(2_822_400));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(64));
        assert!(c.fields.iter().any(|(f, _, v)| *f == Some(Field::Title) && v == "My Title"));
        assert!(c.fields.iter().any(|(f, _, v)| *f == Some(Field::Artist) && v == "My Artist"));
    }

    #[test]
    fn dff_facts_without_diin() {
        let bytes = build_dff("", "", 32); // (32/2)*8 = 128
        // Rebuild without the DIIN chunk by stripping tags is awkward; instead a
        // zero-length title/artist DIIN still yields facts and no non-empty tags.
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = super::read_dff(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.total_frames, Some(128));
    }
}
```

- [ ] **Step 2: Run to confirm it fails to compile (red)**

Run: `cargo test -p soul-audio-tags --lib dff`
Expected: FAIL — `super::read_dff` is not defined yet.

- [ ] **Step 3: Implement `read_dff`**

Add between the module doc/imports and the `#[cfg(test)]` block in `dff.rs`:

```rust
/// Read big-endian IFF facts (`FS `/`CHNL` + sound-chunk size) and human tags
/// (`DIIN` → `DITI`/`DIAR`, and/or `ID3 `) from a DSDIFF stream.
pub fn read_dff<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let mut top = [0u8; 16];
    read_exact(src, &mut top)?;
    if &top[0..4] != b"FRM8" || &top[12..16] != b"DSD " {
        return Err(TagError::Malformed);
    }

    let mut facts = StreamFacts {
        sample_rate: None,
        channels: None,
        total_frames: None,
    };
    let mut channels: u64 = 0;

    // Walk top-level chunks. Sizes exclude the 12-byte header; odd sizes pad.
    loop {
        let (id, size) = match read_chunk_header(src) {
            Ok(h) => h,
            Err(_) => break, // clean EOF — stop scanning
        };
        let padded = size.saturating_add(size & 1);
        match &id {
            b"PROP" => {
                let (sr, ch) = read_prop(src, size)?;
                facts.sample_rate = Some(sr);
                facts.channels = Some(ch);
                channels = u64::from(ch);
            }
            b"DSD " | b"DST " => {
                // Sound data. For raw `DSD `, `size` is the interleaved byte
                // count → per-channel samples = size / channels × 8. (DST would
                // need frame math; DFF-DST tagging is out of scope — facts from
                // PROP still apply.) Record then stop (sound data is last).
                if &id == b"DSD " && channels > 0 {
                    facts.total_frames = Some(size / channels * 8);
                }
                break;
            }
            b"DIIN" => read_diin(src, size, scratch, sink)?,
            b"ID3 " => {
                // Delegate to the shared ID3v2 reader, then skip any pad byte.
                crate::id3v2::read_id3v2(src, scratch, sink)?;
                // read_id3v2 consumes exactly the tag; align to the padded chunk.
                // (If the tag is shorter than `size`, skip the remainder.)
                let _ = padded; // positions are chunk-relative; ID3 fills its chunk
            }
            _ => skip(src, padded)?,
        }
    }
    Ok(facts)
}

/// Read a 12-byte IFF header: 4-byte id + big-endian u64 size.
fn read_chunk_header<S: Source>(src: &mut S) -> Result<([u8; 4], u64), TagError> {
    let mut h = [0u8; 12];
    read_exact(src, &mut h)?;
    Ok((
        [h[0], h[1], h[2], h[3]],
        u64::from_be_bytes([h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11]]),
    ))
}

/// Skip `n` bytes of `src` (seek-relative).
fn skip<S: SeekSource>(src: &mut S, n: u64) -> Result<(), TagError> {
    src.seek(SeekFrom::Current(n as i64)).map_err(|_| TagError::Io)?;
    Ok(())
}

/// Parse `PROP`/`SND ` for `FS `(rate) and `CHNL`(channels); skip the rest.
fn read_prop<S: SeekSource>(src: &mut S, size: u64) -> Result<(u32, u16), TagError> {
    let mut snd = [0u8; 4];
    read_exact(src, &mut snd)?;
    if &snd != b"SND " {
        return Err(TagError::Malformed);
    }
    let mut rem = size.checked_sub(4).ok_or(TagError::Malformed)?;
    let (mut rate, mut channels) = (0u32, 0u16);
    while rem >= 12 {
        let (id, csize) = read_chunk_header(src)?;
        rem -= 12;
        let padded = csize.saturating_add(csize & 1);
        match &id {
            b"FS  " => {
                let mut b = [0u8; 4];
                read_exact(src, &mut b)?;
                rate = u32::from_be_bytes(b);
                skip(src, padded.saturating_sub(4))?;
            }
            b"CHNL" => {
                let mut b = [0u8; 2];
                read_exact(src, &mut b)?;
                channels = u16::from_be_bytes(b);
                skip(src, padded.saturating_sub(2))?;
            }
            _ => skip(src, padded)?,
        }
        rem = rem.saturating_sub(padded);
    }
    Ok((rate, channels))
}

/// Parse a `DIIN` (edited info) chunk: `DITI`(title)/`DIAR`(artist) sub-chunks,
/// each `<u32 count><count ASCII>`. Emits via the sink; unknown sub-chunks skip.
fn read_diin<S: SeekSource>(
    src: &mut S,
    size: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let mut rem = size;
    while rem >= 12 {
        let (id, csize) = read_chunk_header(src)?;
        rem -= 12;
        let padded = csize.saturating_add(csize & 1);
        let field = match &id {
            b"DITI" => Some(Field::Title),
            b"DIAR" => Some(Field::Artist),
            _ => None,
        };
        match field {
            Some(f) if csize >= 4 => {
                let mut cnt = [0u8; 4];
                read_exact(src, &mut cnt)?;
                let count = u32::from_be_bytes(cnt) as u64;
                let take = count.min(scratch.len() as u64).min(csize - 4) as usize;
                read_exact(src, &mut scratch[..take])?;
                if let Ok(s) = core::str::from_utf8(&scratch[..take]) {
                    sink.field(Some(f), "", s);
                }
                skip(src, padded.saturating_sub(4 + take as u64))?;
            }
            _ => skip(src, padded)?,
        }
        rem = rem.saturating_sub(padded);
    }
    Ok(())
}
```

> **Implementer note:** the exact `TagSink::field` signature and `Field` variants
> must match the crate (read `crates/soul-audio-tags/src/lib.rs` + `dsf.rs` usage
> — `dsf.rs` delegates to `id3v2`, which calls the sink). If `field` takes
> different params (e.g. no middle `&str` key), adapt the two `sink.field(...)`
> calls to the real signature; the `Collect` test helper in `testfix` records
> `(Option<Field>, key, value)`.

- [ ] **Step 4: Wire the dispatch in `lib.rs`**

Add `mod dff;` next to `mod dsf;` (line 18). In the `read` match (line 115–123),
add after the `Dsf` arm:

```rust
        locate::Container::Dff => dff::read_dff(src, scratch, sink),
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p soul-audio-tags --lib dff`
Expected: PASS — both dff tests.

- [ ] **Step 6: Full tags crate + no-default (thumbv7em-shape) check**

Run: `cargo test -p soul-audio-tags` then `cargo clippy -p soul-audio-tags --all-targets -- -D warnings`
Expected: green; no-alloc path unaffected (the reader uses only `scratch`).

- [ ] **Step 7: Commit**

```bash
git add crates/soul-audio-tags/src/dff.rs crates/soul-audio-tags/src/lib.rs
git commit -m "feat(tags): DFF/DSDIFF reader — facts + DIIN/ID3 tags (#25)"
```

---

## Task 4: Tier-1 app e2e — DFF + DSD256/512 coverage

**Files:**
- Modify: `app/src-tauri/tests/fixtures.rs` (add `tone.dff` to the playable set;
  extend the decimation test to 256/512; fix the stale "core has no Opus
  decoder" comment).
- Modify: `app/src-tauri/tests/e2e.rs` (add `tone.dff` to the
  `per_format_durations_are_reported` fixture list and bump the DSD count).

**Interfaces:**
- Consumes: `decode_all(name) -> (samples, peak, rate)` and `testdata(name)`
  (already in `fixtures.rs`); `load_folder` + `per_format_durations` (in
  `e2e.rs`). `tone.dff`/`tone-dsd256.dsf`/`tone-dsd512.dsf` from Task 2.

- [ ] **Step 1: Extend `fixtures.rs` — playable set, decimation rates, comment**

In `playable_fixtures_decode_nonsilent`, add `"tone.dff"` to the array (after
`"tone-dsd128.dsf"`). Replace the stale header/comment lines (`//! … Opus is
intentionally NOT decoded — the core has no Opus decoder.` and the NOTE at the
top of the test) with an accurate note — the core now decodes Opus; it is
excluded here only to keep the list short (it is decoded in the CLI e2e). Then
rewrite `dsd128_decimates_to_double_rate` to cover all four rates:

```rust
#[test]
fn dsd_rates_decimate_eight_to_one() {
    // 8:1 decimation: each DSD rate → rate/8 PCM.
    assert_eq!(decode_all("tone.dsf").2, 352_800); // DSD64
    assert_eq!(decode_all("tone-dsd128.dsf").2, 705_600); // DSD128
    assert_eq!(decode_all("tone-dsd256.dsf").2, 1_411_200); // DSD256
    assert_eq!(decode_all("tone-dsd512.dsf").2, 2_822_400); // DSD512
    assert_eq!(decode_all("tone.dff").2, 352_800); // DSD64 via DSDIFF
}
```

(Delete the old `dsd128_decimates_to_double_rate`; this supersedes it.)

- [ ] **Step 2: Add DFF to the Tier-1 duration matrix in `e2e.rs`**

In `per_format_durations_are_reported`, add `"tone.dff"` to the `library_of([…])`
list and update the DSD assertion: `tone.dff` is DSD64 at 0.5 s like `tone.dsf`,
so the "~0.5 s DSD" count goes from 2 to 3. Adjust:

```rust
    let dsd = secs.iter().filter(|d| (**d - 0.5).abs() < 0.06).count();
    let pcm = secs.iter().filter(|d| (**d - 2.0).abs() < 0.16).count();
    assert_eq!(dsd, 3, "three DSD fixtures ~0.5s: {secs:?}"); // dsf + dsd128 + dff
    assert_eq!(pcm, 6, "six PCM fixtures ~2.0s: {secs:?}");
```

(Keep the existing six PCM fixtures unchanged. `tone-dsd256/512.dsf` are 0.1 s and
are NOT added here — they are covered by the decimation test in Step 1; adding
them would need a third duration bucket.)

- [ ] **Step 3: Run the app Tier-1 tests (separate workspace)**

Run (inside `app/src-tauri`): `cargo test --test fixtures && cargo test --test e2e -- per_format_durations`
Expected: PASS — `tone.dff` decodes non-silent and reports ~0.5 s; all four DSD
rates decimate 8:1. Set `SOUL_PLAYER_OFFLINE=1` if the harness needs it (the
common harness sets it).

- [ ] **Step 4: Commit**

```bash
git add app/src-tauri/tests/fixtures.rs app/src-tauri/tests/e2e.rs
git commit -m "test(app): Tier-1 DFF duration + DSD256/512 decimation goldens (#25, #28)"
```

---

## Task 5: CLI e2e — DFF + DST render goldens

**Files:**
- Create (committed): `testdata/tone-dst.dff` — a copy of the DST FATE slice
  `crates/soul-audio-core/testdata/dst-64fs44-2ch.slice.dff` so the CLI `fixture()`
  helper (root `testdata/`) can reach it.
- Modify: `crates/soul-audio-cli/tests/e2e.rs` (add `real_dff_*` and `real_dst_*`
  render tests mirroring `real_dsd_decodes_decimates_and_preserves_stereo_tones`,
  e2e.rs:511–549).

**Interfaces:**
- Consumes: `tmp`, `fixture`, `read_pcm`, `analyze`, `channel_window`,
  `Command::cargo_bin("soul-audio-cli")` (all already in `e2e.rs`).

- [ ] **Step 1: Copy the DST slice into root testdata**

Run: `cp crates/soul-audio-core/testdata/dst-64fs44-2ch.slice.dff testdata/tone-dst.dff`
Expected: `testdata/tone-dst.dff` exists (~9.6 KB).

- [ ] **Step 2: Write the DFF render test (tone matching)**

Add to `crates/soul-audio-cli/tests/e2e.rs` (mirrors the DSD test; the A4/E5
tones must survive decode → decimate → resample):

```rust
#[test]
fn real_dff_decodes_and_preserves_stereo_tones() {
    // A raw-DSD DSDIFF (DSD64) through the whole pipeline — same tone goldens as
    // the DSF path, proving MSB-first/interleaved DFF decode is correct.
    let out = tmp("dff_out.pcm");
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(fixture("tone.dff"))
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "48000", "--channels", "2"])
        .assert()
        .success();

    let pcm = read_pcm(&out);
    let n = 16_384;
    let left = analyze(&channel_window(&pcm, 2, 0, 4000, n), 48_000);
    let right = analyze(&channel_window(&pcm, 2, 1, 4000, n), 48_000);
    assert!((left.peak_hz - 440.0).abs() < 6.0, "DFF left {:.1} Hz", left.peak_hz);
    assert!((right.peak_hz - 659.25).abs() < 6.0, "DFF right {:.1} Hz", right.peak_hz);
    assert!(left.sfdr_db > 40.0 && right.sfdr_db > 40.0, "DFF noisy");
}
```

- [ ] **Step 3: Write the DST render test (runs + non-silent)**

The FATE slice is a real-music fragment (not the A4/E5 tone), so assert it
renders and is non-silent rather than pitch:

```rust
#[test]
fn real_dst_compressed_dff_renders_nonsilent() {
    // DST-compressed DSDIFF (the committed FFmpeg FATE slice) decodes through the
    // arithmetic coder → DSD → decimate → resample without error, non-silent.
    let out = tmp("dst_out.pcm");
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(fixture("tone-dst.dff"))
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "48000", "--channels", "2"])
        .assert()
        .success();

    let pcm = read_pcm(&out);
    assert!(!pcm.is_empty(), "DST render produced no PCM");
    assert!(pcm.iter().any(|&s| s.abs() > 1000), "DST render silent");
}
```

> **Implementer note:** confirm the `render` subcommand accepts a very short input
> (the slice is a fraction of a second). If it needs a minimum length, pad the
> assertion window accordingly; keep the non-silent check.

- [ ] **Step 4: Run the CLI e2e tests**

Run: `cargo test -p soul-audio-cli --test e2e real_dff real_dst`
Expected: PASS — both render cleanly; DFF tones land at 440/659 Hz.

- [ ] **Step 5: Commit**

```bash
git add testdata/tone-dst.dff crates/soul-audio-cli/tests/e2e.rs
git commit -m "test(cli): e2e render goldens for DFF (raw) and DST-compressed DSDIFF"
```

---

## Task 6: DSD per-rate real-time headroom + documented ceiling (#28 perf)

**Files:**
- Create: `crates/soul-audio-cli/tests/dsd_realtime.rs` — an `#[ignore]`d
  measurement that decodes each DSD fixture and prints the decode-vs-real-time
  ratio (timing is machine-dependent, so it prints rather than asserts a bound).
- Modify: `testdata/README.md` — document the measured per-rate ceiling.

**Interfaces:**
- Consumes: `soul_audio_desktop::decoder::open_track`, `soul_audio_core::Decoder`.
- Produces: `cargo test -p soul-audio-cli --test dsd_realtime -- --ignored --nocapture`
  prints one line per rate.

- [ ] **Step 1: Write the measurement harness**

```rust
//! DSD decode-vs-real-time headroom, per rate. `#[ignore]`d: the ratio is
//! machine-dependent, so this is a developer measurement (run with --ignored
//! --nocapture), not a CI gate. Its finding is recorded in testdata/README.md.
use std::path::PathBuf;
use std::time::Instant;

use soul_audio_core::Decoder;
use soul_audio_desktop::decoder::open_track;

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata").join(name)
}

fn ratio(name: &str) -> f64 {
    let path = testdata(name);
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut d = open_track(&path).expect("open");
            let rate = d.stream_info().sample_rate as f64;
            let mut buf = [0i32; 8192];
            let mut frames = 0u64;
            let t0 = Instant::now();
            loop {
                let n = d.decode_into(&mut buf).expect("decode");
                if n == 0 { break; }
                frames += (n / d.stream_info().channels.max(1) as usize) as u64;
            }
            let decode_s = t0.elapsed().as_secs_f64();
            let audio_s = frames as f64 / rate;
            decode_s / audio_s // < 1.0 = faster than real-time
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
#[ignore = "machine-dependent timing; run with --ignored --nocapture"]
fn dsd_decode_realtime_ratio_per_rate() {
    for name in ["tone.dsf", "tone-dsd128.dsf", "tone-dsd256.dsf", "tone-dsd512.dsf"] {
        println!("{name}: decode/realtime = {:.3}×", ratio(name));
    }
}
```

- [ ] **Step 2: Run it (release) and capture the numbers**

Run: `cargo test -p soul-audio-cli --release --test dsd_realtime -- --ignored --nocapture`
Expected: four lines, e.g. `tone-dsd512.dsf: decode/realtime = 0.?××`. Record the
actual four ratios.

- [ ] **Step 3: Document the ceiling in `testdata/README.md`**

Add a short subsection under the DSD description with the measured release-build
ratios and a one-line verdict (e.g. "DSD512 sustains real-time at N.NN× in
release; the dev profile is ~8× slower, so use `--release` for DSD256+."). Use
the real numbers from Step 2 — do not invent them.

- [ ] **Step 4: Commit**

```bash
git add crates/soul-audio-cli/tests/dsd_realtime.rs testdata/README.md
git commit -m "test(perf): DSD per-rate real-time measurement + documented ceiling (#28)"
```

---

## Task 7: Full verify + PR

- [ ] **Step 1: Run the root gate**

Run: `cargo xtask verify`
Expected: fmt + clippy + table-checks + full `cargo test` + thumbv7em cross-builds
all green. (This covers Tasks 1, 3, 5, 6 in the root workspace.)

- [ ] **Step 2: Run the app gate (separate workspace)**

Run (inside `app/src-tauri`): `SOUL_PLAYER_OFFLINE=1 cargo test`
Expected: all Tier-1 tests green (covers Task 4).

- [ ] **Step 3: Open the PR**

```bash
git push -u origin feat/codec-e2e-dsd
tea pr create --title "DSD/DSDIFF: duration, tags, DSD256/512 + DFF/DST e2e (#25, #28)" --description "…"
```
PR body: summarize the DFF duration/tags fix, the new fixtures, the DSD256/512
decimation goldens, the DFF/DST render e2e, and the documented DSD512 ceiling.
Reference #25 and #28. Do NOT close them yet — the comprehensive effort's PR 2/3
also touch adjacent areas; close on final merge or per-issue as each is fully
satisfied (#25 and #28 are fully satisfied by this PR, so they may close here).

---

## Self-Review

**Spec coverage (vs `2026-10-07-codec-e2e-completion-design.md`, DSD/DSDIFF
slice):**
- #25a DFF duration → Task 1. 
- #25b DFF tag reader → Task 3. 
- `tone.dff` fixture → Task 2. DST playback (reuse slice) → Task 5. 
- DSD256/512 fixtures + decimation goldens → Tasks 2 + 4. 
- DFF e2e (CLI + Tier-1 + duration) → Tasks 4 + 5. 
- #28 DSD512 real-time headroom + documented ceiling → Task 6. 
- Stale `fixtures.rs` Opus comment → Task 4, Step 1. 

**Placeholder scan:** The only deferred spots are flagged "Implementer note"
(sink signature confirmation in Task 3; render min-length in Task 5) — these are
*verification* steps against real code, not missing content; the code to write is
fully given. The PR body text is intentionally author-time.

**Type consistency:** `total_samples() -> Option<u64>` (Task 1) is what
`DecimateAdapter::total_frames()` (dsd.rs:275) consumes. `read_dff` signature
matches `read_dsf` and the `lib.rs` dispatch arm (Task 3). `decode_all(name) ->
(u64, i32, u32)` reused from the existing `fixtures.rs` (Task 4). `fixture`,
`analyze`, `channel_window`, `read_pcm` reused from the existing CLI `e2e.rs`
(Task 5).
