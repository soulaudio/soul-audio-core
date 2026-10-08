# Codec E2E Completion — PR 4: Vorbis fast-Huffman (#34) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Accelerate `Codebook::decode_scalar` — today an `O(entries ×
max_length)` linear scan per codeword, the Vorbis residue hot loop (#34) — with
an stb_vorbis-style FAST_HUFFMAN prefix table, **bit-identically** (the SNR
goldens must not move) and with graceful fallback so correctness never depends on
the table.

**Architecture:** At codebook parse time, build a `2^FAST_BITS`-entry lookup
table in the arena: for each used entry whose codeword is `≤ FAST_BITS` bits, map
every table index whose MSb-first prefix matches that codeword to
`(entry, len)`. `decode_scalar` peeks `FAST_BITS` bits, bit-reverses them to the
MSb-first index, and on a hit consumes `len` bits and returns the entry; on a
miss (codeword longer than `FAST_BITS`, or near-EOF) it falls back to the
existing bit-by-bit loop unchanged. The table is **best-effort**: if the arena is
full, the book keeps `fast_off = 0` and always uses the slow path.

**Tech Stack:** Rust (edition 2024, `no_std`/no-alloc core), `cargo xtask verify`
(incl. the `vorbis-embedded-profile` re-test), ffmpeg `libvorbis` for the dense
fixture.

**Branch:** `feat/codec-e2e-vorbis-huffman`, stacked on `feat/codec-e2e-vorbis`
(PR #47).

## Global Constraints

- No AI trailers; GPG-signed commits (lefthook). Never `--no-gpg-sign`.
- `soul-audio-core` stays `no_std`/no-alloc + `#![forbid(unsafe_code)]`; no new
  deps; thumbv7em cross-build green (incl. `vorbis-embedded-profile`).
- **Bit-exactness is the invariant.** The spec worked-example codebook test
  (`decodes_the_spec_worked_example_codewords`) and the three vorbis SNR goldens
  (`open_decodes_vorbis_*_matching_ffmpeg`, ~107 dB) must pass unchanged. A fast
  path that returns one wrong symbol fails them.
- **Profile knob:** `FAST_HUFFMAN_BITS` = 10 (desktop), 0 (embedded → feature off,
  no table built, zero extra RAM).

## FAST_HUFFMAN_BITS and the arena

A `2^10`-entry table is `4 KiB` (u32/slot) per codebook. The desktop arena grows
256 → 512 KiB so real multi-codebook files fit; books that still don't fit skip
the table (slow path). Embedded (`FAST_HUFFMAN_BITS == 0`) builds no tables.

---

## Task 1: BitReader — `peek_bits` + `advance`

**Files:**
- Modify: `crates/soul-audio-core/src/formats/vorbis/bitread.rs`.

**Interfaces:**
- Produces: `BitReader::peek_bits(&self, n: u32) -> u32` (LSb-first, zero-padded
  past EOF, **non-consuming**); `BitReader::advance(&mut self, n: u32)` (bounded —
  never past `total_bits()`); `BitReader::bits_remaining(&self) -> usize`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn peek_does_not_consume_and_advance_matches_read_bits() {
    let mut r = BitReader::new(&[0xFC, 0x48]);
    let peeked = r.peek_bits(4);
    assert_eq!(peeked, 12); // same 4 bits read_bits(4) returns
    assert_eq!(r.bits_read(), 0, "peek must not consume");
    r.advance(4);
    assert_eq!(r.bits_read(), 4);
    assert_eq!(r.read_bits(3).unwrap(), 0b111);
}

#[test]
fn peek_zero_pads_past_eof() {
    let mut r = BitReader::new(&[0x0F]); // 8 bits
    // peek 12 bits: low 4 are 0xF, remaining 8 are zero-pad → 0x00F.
    assert_eq!(r.peek_bits(12), 0x00F);
    assert_eq!(r.bits_read(), 0);
}

#[test]
fn advance_is_bounded_at_eof() {
    let mut r = BitReader::new(&[0xAB]);
    r.advance(100); // clamped
    assert_eq!(r.read_bit(), Err(DecodeError::UnexpectedEof));
}
```

- [ ] **Step 2: Run → fail (methods undefined).**

Run: `cargo test -p soul-audio-core --lib formats::vorbis::bitread::tests::peek`

- [ ] **Step 3: Implement**

```rust
    /// Peek the next `n` bits (`n <= 32`) LSb-first **without consuming** them;
    /// bits past end of stream read as `0` (a prefix table consults this, then
    /// the caller [`Self::advance`]s only the matched codeword length).
    pub fn peek_bits(&self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let mut v = 0u32;
        for i in 0..n {
            let pos = self.bit_pos + i as usize;
            let byte = self.bytes.get(pos >> 3).copied().unwrap_or(0);
            if (byte >> (pos & 7)) & 1 != 0 {
                v |= 1u32 << i;
            }
        }
        v
    }

    /// Advance the cursor by `n` bits, saturating at end of stream (so a bogus
    /// over-advance can't move `bit_pos` past the backing slice).
    pub fn advance(&mut self, n: u32) {
        self.bit_pos = (self.bit_pos + n as usize).min(self.total_bits());
    }

    /// Bits not yet consumed.
    pub fn bits_remaining(&self) -> usize {
        self.total_bits().saturating_sub(self.bit_pos)
    }
```

- [ ] **Step 4: Run → pass.** Run: `cargo test -p soul-audio-core --lib formats::vorbis::bitread`

- [ ] **Step 5: Commit**

```bash
git add crates/soul-audio-core/src/formats/vorbis/bitread.rs
git commit -m "feat(core): BitReader peek_bits/advance for Vorbis fast-Huffman (#34)"
```

---

## Task 2: codebook — FAST_HUFFMAN table + fast decode path

**Files:**
- Modify: `crates/soul-audio-core/src/formats/vorbis/mod.rs` (add
  `FAST_HUFFMAN_BITS` profile const).
- Modify: `crates/soul-audio-core/src/formats/vorbis/codebook.rs` (`fast_off`
  field; build table in `parse`; fast path in `decode_scalar`; `reverse_bits`).

**Interfaces:**
- Consumes: `FAST_HUFFMAN_BITS`, `BitReader::{peek_bits, advance,
  bits_remaining}`.
- Produces: unchanged public `decode_scalar` signature/semantics (faster).

- [ ] **Step 1: Add the profile const to `mod.rs`** (near `MAX_CHANNELS`)

```rust
/// Width of the per-codebook FAST_HUFFMAN acceleration table
/// (`2^FAST_HUFFMAN_BITS` entries) built at setup to short-circuit the
/// `decode_scalar` linear scan for codewords this long or shorter (profile
/// knob, #34). The desktop profile uses 10 (stb_vorbis's value; 4 KiB/book);
/// the embedded profile turns it **off** (`0` → no table, no extra RAM).
#[cfg(not(feature = "vorbis-embedded-profile"))]
pub(crate) const FAST_HUFFMAN_BITS: u32 = 10;
#[cfg(feature = "vorbis-embedded-profile")]
pub(crate) const FAST_HUFFMAN_BITS: u32 = 0;
```

- [ ] **Step 2: Add `fast_off` to `Codebook` + `EMPTY`**

Add field `fast_off: u32,` (0 = no table). Set `fast_off: 0` in `EMPTY` and in
the `parse` return literal (overwritten by Step 3 when a table is built).

- [ ] **Step 3: Build the table in `parse` (after `assign_codewords`)**

Insert after the `assign_codewords` call (guarded by the profile const and
best-effort on arena space):

```rust
        // FAST_HUFFMAN acceleration table (best-effort; #34). Maps the next
        // FAST_HUFFMAN_BITS stream bits (MSb-first) to (entry, len) for every
        // codeword that short; longer codewords / near-EOF fall back to the
        // scan in `decode_scalar`. Skipped entirely on the embedded profile
        // (const 0) or if the arena can't fit it (the book stays scan-only).
        let mut fast_off = 0u32;
        if FAST_HUFFMAN_BITS > 0 && max_length > 0 {
            let slots = 1usize << FAST_HUFFMAN_BITS;
            if let Ok(off) = arena.alloc(slots * 4, 4) {
                // Clear to the "no short codeword" sentinel (len == 0).
                for s in 0..slots {
                    arena.bytes_mut()[off + s * 4..off + s * 4 + 4]
                        .copy_from_slice(&0u32.to_le_bytes());
                }
                let buf = arena.bytes_mut();
                for i in 0..entries_usize {
                    let len = u32::from(buf[lengths_off + i]);
                    if len == 0 || len > FAST_HUFFMAN_BITS {
                        continue;
                    }
                    // Codeword is left-justified (bit 0 at value-bit 31); its
                    // MSb-first prefix occupies the top `len` bits of the index.
                    let cw = u32::from_le_bytes(
                        buf[codewords_off + i * 4..codewords_off + i * 4 + 4]
                            .try_into()
                            .unwrap(),
                    );
                    let prefix = cw >> (32 - FAST_HUFFMAN_BITS); // FAST_BITS-wide, MSb-first
                    let shift = FAST_HUFFMAN_BITS - len;
                    let base = (prefix >> shift) << shift; // zero the low (FAST-len) bits
                    let count = 1u32 << shift;
                    let packed = (len << 24) | (i as u32); // len in top 8, entry in low 24
                    for k in 0..count {
                        let idx = (base + k) as usize;
                        buf[off + idx * 4..off + idx * 4 + 4]
                            .copy_from_slice(&packed.to_le_bytes());
                    }
                }
                fast_off = off as u32;
            }
        }
```

Add `fast_off,` to the returned `Codebook { … }` literal.

- [ ] **Step 4: Add `reverse_bits` + the fast path to `decode_scalar`**

At the top of `decode_scalar`, after the `max == 0` guard, before the `acc`
loop:

```rust
        // Fast path: a FAST_HUFFMAN table resolves any codeword ≤
        // FAST_HUFFMAN_BITS in one peek. Miss (sentinel 0, or not enough bits
        // left for the matched length) → fall through to the exact scan below.
        if self.fast_off != 0 {
            let fb = FAST_HUFFMAN_BITS;
            let peeked = br.peek_bits(fb); // LSb-first
            let idx = reverse_bits(peeked, fb) as usize; // MSb-first table index
            let off = self.fast_off as usize + idx * 4;
            let packed = u32::from_le_bytes(storage[off..off + 4].try_into().unwrap());
            let len = packed >> 24;
            if len != 0 && (len as usize) <= br.bits_remaining() {
                br.advance(len);
                return Ok(packed & 0x00FF_FFFF);
            }
        }
```

Add the helper (module-private, near `lookup1_values`):

```rust
/// Reverse the low `n` bits of `v` (`n <= 32`): turns the BitReader's LSb-first
/// peek into the MSb-first codeword-prefix index the FAST_HUFFMAN table is keyed
/// by (first stream bit = most-significant index bit).
fn reverse_bits(v: u32, n: u32) -> u32 {
    let mut r = 0u32;
    for i in 0..n {
        if (v >> i) & 1 != 0 {
            r |= 1u32 << (n - 1 - i);
        }
    }
    r
}
```

- [ ] **Step 5: Run the codebook unit tests (bit-exact behaviour unchanged)**

Run: `cargo test -p soul-audio-core --lib formats::vorbis::codebook`
Expected: ALL existing tests pass — crucially
`decodes_the_spec_worked_example_codewords`, the ordered/sparse decodes, and the
single-entry pseudo-nil case (whose `1`-bit still correctly reports `Malformed`:
with `FAST_HUFFMAN_BITS=10` the "1000000000" prefix has sentinel 0 → falls to the
scan → `Malformed`, unchanged). Add one directed test that a book with a
`> FAST_HUFFMAN_BITS` codeword still decodes (forces the fallback):

```rust
#[test]
fn long_codeword_falls_back_past_the_fast_table() {
    // A book whose longest codeword exceeds FAST_HUFFMAN_BITS must still decode
    // via the scan fallback. entries=2, lengths [1, 1] is too short; instead
    // build lengths that force an 11-bit codeword: [1, 2, 3, 4, 5, 6, 7, 8, 9,
    // 10, 11, 11] is a full tree with two 11-bit leaves (> FAST_BITS=10).
    let mut w = BitWriter::new();
    write_header(&mut w, 1, 12);
    w.write(0, 1); // ordered = 0
    w.write(0, 1); // sparse = 0
    for len in [1u32, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 11] {
        w.write(len - 1, 5);
    }
    w.write(0, 4); // lookup_type = 0
    let (cb, storage) = parse(&w.into_bytes()).expect("valid deep codebook");
    // The last entry's codeword is 11 ones: 1111111111 then a final 1 → entry 11.
    assert_eq!(
        decode(&cb, &storage, &[1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1]).unwrap(),
        11
    );
    // Entry 0 (codeword "0") still resolves via the fast table.
    assert_eq!(decode(&cb, &storage, &[0]).unwrap(), 0);
}
```

- [ ] **Step 6: Commit**

```bash
git add crates/soul-audio-core/src/formats/vorbis/mod.rs crates/soul-audio-core/src/formats/vorbis/codebook.rs
git commit -m "perf(core): FAST_HUFFMAN table accelerates Vorbis decode_scalar (#34)"
```

---

## Task 3: Desktop arena headroom

**Files:**
- Modify: `crates/soul-audio-desktop/src/decoder.rs` (`VORBIS_DEFAULT_ARENA`
  256→512 KiB; `VORBIS_ROBUST_ARENA` 512 KiB→1 MiB).

- [ ] **Step 1: Bump the sizes + update the doc comments**

Explain the increase: the FAST_HUFFMAN tables (4 KiB/codebook, desktop profile)
need headroom beyond the ~150–250 KiB of setup tables.

- [ ] **Step 2: Confirm a real Vorbis file still opens + the SNR path is intact**

Run: `cargo test -p soul-audio-core --test decode vorbis`
(Those tests build their own 128 KiB arenas — see Step note below.)

> **Arena note:** the core `decode.rs` vorbis goldens pass a `vec![0u8; 128 *
> 1024]` arena. With `FAST_HUFFMAN_BITS=10`, the small `tone-vorbis-*.ogg`
> fixtures have few codebooks, so their tables fit in 128 KiB and the goldens
> still pass. If any newly overflows (graceful skip → still correct, just
> scan-only), the golden is unaffected. Only the *desktop* default grows; the
> test arenas need no change. Confirm by running them.

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-desktop/src/decoder.rs
git commit -m "perf(desktop): enlarge Vorbis arena for FAST_HUFFMAN tables (#34)"
```

---

## Task 4: Dense synthetic fixture + perf measurement

**Files:**
- Modify: `testdata/generate.sh` (dense broadband Vorbis encode).
- Create (committed): `testdata/tone-vorbis-dense.ogg`.
- Create: `crates/soul-audio-cli/tests/vorbis_realtime.rs` (ignored measurement).

- [ ] **Step 1: Generate a dense fixture**

A pure tone has a light residue payload (misleadingly fast). Synthesize a
broadband, high-bitrate stereo signal (band-limited noise) so the residue hot
loop is actually exercised:

```bash
# Dense Vorbis: broadband noise, high quality → heavy residue payload, the #34
# decode hot path. Synthetic (no copyright); 2 s stereo.
ffmpeg -y -hide_banner -loglevel error -f lavfi -i "anoisesrc=d=2:c=pink:r=44100:a=0.5,aformat=channel_layouts=stereo" -c:a libvorbis -q:a 8 tone-vorbis-dense.ogg
```

Append `tone-vorbis-dense.ogg` to the final `ls -la` line.

- [ ] **Step 2: Write the perf measurement (ignored, prints ratio)**

```rust
//! Vorbis decode-vs-real-time on a dense (broadband) fixture — the #34 residue
//! hot path. `#[ignore]`d (machine-dependent); run:
//!   cargo test -p soul-audio-cli --release --test vorbis_realtime -- --ignored --nocapture
use std::path::PathBuf;
use std::time::Instant;
use soul_audio_core::Decoder;
use soul_audio_desktop::open_track;

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata").join(name)
}

#[test]
#[ignore = "machine-dependent timing; run with --ignored --nocapture"]
fn vorbis_dense_decode_realtime_ratio() {
    let path = testdata("tone-vorbis-dense.ogg");
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut d = open_track(&path).expect("open");
            let rate = d.stream_info().sample_rate as f64;
            let ch = d.stream_info().channels.max(1) as usize;
            let mut buf = [0i32; 8192];
            let mut frames = 0u64;
            let t0 = Instant::now();
            loop {
                let n = d.decode_into(&mut buf).expect("decode");
                if n == 0 { break; }
                frames += (n / ch) as u64;
            }
            let ratio = t0.elapsed().as_secs_f64() / (frames as f64 / rate);
            println!("tone-vorbis-dense.ogg: decode/realtime = {ratio:.3}x");
        })
        .unwrap()
        .join()
        .unwrap();
}
```

- [ ] **Step 3: Measure (release) and sanity-check < 1×**

Run: `cargo test -p soul-audio-cli --release --test vorbis_realtime -- --ignored --nocapture`
Expected: a ratio comfortably `< 1.0` (record it). This is the #34 acceptance
("decode-vs-real-time margin stays comfortably < 1× on a dense fixture").

- [ ] **Step 4: Commit**

```bash
git add testdata/generate.sh testdata/tone-vorbis-dense.ogg crates/soul-audio-cli/tests/vorbis_realtime.rs
git commit -m "test(perf): dense Vorbis fixture + decode-vs-real-time measurement (#34)"
```

---

## Task 5: Goldens-unchanged proof + verify + review + PR

- [ ] **Step 1: The bit-exactness gate — SNR goldens unchanged**

Run: `cargo test -p soul-audio-core --test decode vorbis`
Then the embedded profile (FAST_HUFFMAN off path): `cargo test -p soul-audio-core
--features vorbis-embedded-profile --test decode vorbis`
Expected: both green — the three `open_decodes_vorbis_*_matching_ffmpeg` SNR
goldens + the seek/granule tests pass identically with the table ON (desktop) and
OFF (embedded). This is the proof the fast path is bit-identical to the scan.

- [ ] **Step 2: Root verify (incl. embedded-profile re-test + thumbv7em)**

Run: `cargo xtask verify`
Expected: `verify: OK`.

- [ ] **Step 3: Review the diff** (code-review skill); fix findings. Focus: the
  table-build index math (`base`/`shift`/`prefix`), `reverse_bits`, the
  `bits_remaining` EOF guard, and the `packed` entry/len encoding (entries are
  24-bit, so `len << 24 | entry` is lossless).

- [ ] **Step 4: Push + open the stacked PR**

```bash
git push -u origin feat/codec-e2e-vorbis-huffman
tea pr create --base feat/codec-e2e-vorbis --head feat/codec-e2e-vorbis-huffman --title "perf: Vorbis FAST_HUFFMAN decode acceleration (#34)" --description "…"
```
PR body: the table design, graceful skip, profile gating (off on embedded),
arena bump, dense-fixture perf number, and the goldens-unchanged proof.

---

## Self-Review

**Spec coverage:** #34 FAST_HUFFMAN table (Task 2), profile-const sized / off on
embedded (Task 2 Step 1), built at setup (Task 2 Step 3), dense-fixture perf
margin < 1× (Task 4), SNR goldens unchanged (Task 5 Step 1).

**Risk:** bit-exactness. Mitigations: graceful fallback (table miss → exact
scan), the spec worked-example test, the long-codeword fallback test, and the
~107 dB SNR goldens run both table-on and table-off. Any symbol error fails them.

**Type consistency:** `fast_off: u32` matches the other arena-offset fields;
`FAST_HUFFMAN_BITS: u32`; `packed = (len << 24) | entry` with `entry < 2^24`
(codebook_entries is a 24-bit field) is lossless and matches the decode unpack.
