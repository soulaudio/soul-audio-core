# AAC-LC Decoder Implementation Plan (Phase 1 of HE-AACv2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> **Algorithm bible:** every decoder task must read `.superpowers/sdd/aac-reference.md` (source-verified AAC-LC reference: bit layouts, tables, formulas, pipeline order, and the exact FFmpeg/faad2 source URLs to transcribe the big tables from). It is the single source of algorithm truth; this plan gives structure, interfaces, and the trickiest verbatim code.

**Goal:** A from-scratch, `no_std`, no-alloc AAC-LC decoder for `soul-audio-core`, reading MP4/.m4a (`mp4a`/`esds`) and raw ADTS (.aac), wired into `open()`/`AnyDecoder`, output interleaved `i32` 16-bit PCM.

**Architecture:** Two container front-ends (MP4 via the existing `demux/mp4.rs`; a new `demux/adts.rs`) feed one shared `AacDecoder` over a small frame-source seam. The decoder: bitstream syntax → Huffman spectral decode → inverse-quant + scalefactors → M/S + intensity stereo → TNS → IMDCT filterbank + overlap-add → PCM. All fixed by-value buffers.

**Tech Stack:** Rust 1.85/edition 2024, `#![no_std]` + no-alloc + `#![forbid(unsafe_code)]`. Tests use ffmpeg golden fixtures; because AAC is **lossy**, the golden gate is a **tolerance/segmental-SNR** comparison vs ffmpeg's decode (not bit-exact), plus a tone-FFT absolute check.

## Global Constraints

- `#![no_std]`, no heap allocation in non-test code (fixed by-value buffers only), `#![forbid(unsafe_code)]`; no std-only float ops in non-test code (use `libm` if a transcendental is needed — check whether the crate already depends on `libm`; the resampler generated its tables at build time via `xtask`, so precompute transcendentals into `const` tables or use `libm`).
- Decoders read via `Source`/`BufSource`; output interleaved `i32` via `Decoder` (`stream_info()` + `decode_into(&mut [i32]) -> Result<usize, DecodeError>`; `Ok(0)` = EOS).
- **Never panic / never index OOB on malformed input** (firmware invariant): every field read from the bitstream (element counts, max_sfb, codebook indices, section lengths, TNS order/length, pulse offsets, window params) is bounds-checked → `Err`, never a panic/OOB. This is a hard gate (final review will fuzz it).
- Scope IN: AAC-LC (AOT 2), MP4 `mp4a`/`esds` + ADTS, mono+stereo, all four window sequences, TNS, M/S + intensity, decode + `total_frames`, 16-bit output, `seek`→`Unsupported`. OUT: SBR, PS, seeking, >2ch/PCE layouts, gain-control, LD/LTP/Main/SSR, PNS (LC streams don't use it; treat NOISE cb as an error or zero — see Task 4).
- Follow the existing `demux/<c>.rs` + `formats/<c>.rs` pattern. New modules under `formats/aac/`.
- Read `.superpowers/sdd/aac-reference.md` before any decoder task.

---

## File structure

- `crates/soul-audio-core/src/demux/adts.rs` (NEW) — ADTS frame reader (probe + header parse + frame iteration).
- `crates/soul-audio-core/src/demux/mp4.rs` (MODIFY) — add a `mp4a`/`esds`→ASC branch to `parse_stsd`; expose the ASC + codec.
- `crates/soul-audio-core/src/formats/aac/mod.rs` (NEW) — `AacDecoder`, the frame-source seam, top-level orchestration + `Decoder` impl.
- `crates/soul-audio-core/src/formats/aac/bits.rs` (NEW) — MSB-first bit reader (AAC-local; do NOT touch ALAC's).
- `crates/soul-audio-core/src/formats/aac/huffman.rs` (NEW) — codebook tables + spectral/scalefactor decode.
- `crates/soul-audio-core/src/formats/aac/tables.rs` (NEW) — swb offsets, num_swb, sample-rate table, window tables (or generated).
- `crates/soul-audio-core/src/formats/aac/syntax.rs` (NEW) — `raw_data_block`/element/ICS parse → quantized coeffs + params.
- `crates/soul-audio-core/src/formats/aac/dsp.rs` (NEW) — dequant + scalefactors + pulse + stereo + TNS.
- `crates/soul-audio-core/src/formats/aac/filterbank.rs` (NEW) — FFT + IMDCT + windows + overlap-add.
- `crates/soul-audio-core/src/{model.rs,probe.rs,dispatch.rs}` (MODIFY) — `Format::Adts`, probe, `AnyDecoder::Aac`, `open` routing.
- `crates/soul-audio-core/testdata/` — `aac.m4a`, `aac.m4a.ref.pcm`, `aac.aac`, `aac.aac.ref.pcm`.
- `crates/soul-audio-core/tests/decode.rs` + module unit tests; `testdata/tone.m4a`(AAC)/`.aac` for playback; `xtask qa_audio`; cli e2e.

---

## Task 0: Golden fixtures (AAC-LC MP4 + ADTS + ffmpeg reference PCM)

**Files:** Create `crates/soul-audio-core/testdata/{aac.m4a,aac.m4a.ref.pcm,aac.aac,aac.aac.ref.pcm}`.

- [ ] **Step 1: Generate** (from repo root; write the intermediate WAV to an absolute path — Git Bash `/tmp` isn't visible to native ffmpeg here):
```bash
python - <<'PY'
import struct,math
rate=44100; n=int(rate*0.5); d=bytearray()
for i in range(n):
    for f in (440.0,659.25): d+=struct.pack('<h',int(0.4*32767*math.sin(2*math.pi*f*i/rate)))
hdr=b'RIFF'+struct.pack('<I',36+len(d))+b'WAVE'+b'fmt '+struct.pack('<IHHIIHH',16,1,2,rate,rate*4,4,16)+b'data'+struct.pack('<I',len(d))
open('aac_src.wav','wb').write(hdr+d)
PY
ffmpeg -y -hide_banner -loglevel error -i aac_src.wav -c:a aac -b:a 128k -movflags +faststart crates/soul-audio-core/testdata/aac.m4a
ffmpeg -y -hide_banner -loglevel error -i aac_src.wav -c:a aac -b:a 128k -f adts crates/soul-audio-core/testdata/aac.aac
ffmpeg -y -hide_banner -loglevel error -i crates/soul-audio-core/testdata/aac.m4a -f s16le crates/soul-audio-core/testdata/aac.m4a.ref.pcm
ffmpeg -y -hide_banner -loglevel error -i crates/soul-audio-core/testdata/aac.aac -f s16le crates/soul-audio-core/testdata/aac.aac.ref.pcm
```
- [ ] **Step 2: Verify** the refs are 16-bit stereo interleaved (each ≈ `0.5s·44100·2ch·2B` plus AAC encoder delay; sizes need not match each other exactly — AAC has priming/padding). `ffprobe crates/soul-audio-core/testdata/aac.m4a` shows `aac (LC)`. `.gitattributes` already has `*.m4a`/`*.aac`? Add `*.aac binary` if missing (`*.m4a`/`*.pcm` already present).
- [ ] **Step 3: Commit** `git add crates/soul-audio-core/testdata/aac.* .gitattributes && git commit -m "test(aac): AAC-LC MP4 + ADTS fixtures + ffmpeg reference PCM"`.

> Note: AAC decoders emit encoder-delay priming samples (typically 1024 or 2048 zero-ish samples at the start) and the total length differs from the source by the priming/padding. The conformance test (Task 8) aligns by trimming the leading priming and compares the overlapping region — see Task 8.

---

## Task 1: ADTS demuxer + probe + `Format::Adts`

**Files:** Create `demux/adts.rs`; MODIFY `demux/mod.rs` (`pub mod adts;`), `model.rs` (`Format::Adts`), `probe.rs`.
**Interfaces produced:** `pub struct AdtsReader<S>{..}` with `new(src) -> Result<Self, DecodeError>` (reads/validates the first header, exposes config), `pub fn config(&self) -> AacConfig` (`{ object_type: u8, sample_rate: u32, channels: u8 }`), `pub fn next_frame<'a>(&mut self, buf: &'a mut [u8]) -> Result<Option<&'a [u8]>, DecodeError>` (reads the next `aac_frame_length` frame's raw_data_block bytes — i.e. skip the 7/9-byte header — into `buf`, returns `None` at EOS). Reuse the sample-rate table from `tables.rs` (Task 4) — for Task 1 inline the 13-entry table locally and dedupe later, OR create `tables.rs` here with just the SR table.

- [ ] **Step 1: Failing test** — probe + header parse of the fixture:
```rust
#[cfg(test)]
mod tests {
    use super::*; use crate::io::SliceSource;
    const AAC: &[u8] = include_bytes!("../../testdata/aac.aac");
    #[test]
    fn probes_and_parses_adts_header() {
        assert_eq!(crate::probe::probe(&mut SliceSource::new(AAC)).unwrap(), crate::model::Format::Adts);
        let r = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        let c = r.config();
        assert_eq!(c.object_type, 2); // LC (profile 01 + 1)
        assert_eq!(c.sample_rate, 44_100);
        assert_eq!(c.channels, 2);
    }
    #[test]
    fn iterates_frames() {
        let mut r = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        let mut buf = [0u8; 8192]; let mut frames = 0;
        while r.next_frame(&mut buf).unwrap().is_some() { frames += 1; }
        assert!(frames > 10); // ~0.5s / (1024/44100) ≈ 21 frames
    }
}
```
- [ ] **Step 2:** Run `cargo test -p soul-audio-core --lib demux::adts` → FAIL (undefined).
- [ ] **Step 3: Implement** per reference §1. `probe`: in `identify(head)`, before the fallback, `if head.len()>=2 && head[0]==0xFF && (head[1]&0xF0)==0xF0 && (head[1]&0x06)==0 { return Some(Format::Adts) }` (layer==00 disambiguates MP3). `Format::Adts` added to `model.rs`. `AdtsReader`: parse the 7-byte fixed+variable header (protection_absent bit → +2 CRC bytes), extract profile→object_type(=profile+1), sampling_frequency_index→Hz (13-entry table), channel_configuration, aac_frame_length; `next_frame` reads the header, then `aac_frame_length - header_len` bytes of raw_data_block into `buf` (bounds-check `header_len ≤ aac_frame_length ≤ buf.len()`; error `Malformed` otherwise). Every field bounds-checked; a bad syncword / SFI≥13 / channels 0 → `Malformed`/`Unsupported`.
- [ ] **Step 4:** Run the tests → PASS.
- [ ] **Step 5:** `cargo build -p soul-audio-core --target thumbv7em-none-eabihf --no-default-features` + `cargo clippy -p soul-audio-core --all-targets -- -D warnings` clean.
- [ ] **Step 6: Commit** `feat(demux): ADTS reader + probe`.

---

## Task 2: MP4 `mp4a`/`esds` → AudioSpecificConfig

**Files:** MODIFY `demux/mp4.rs`.
**Interfaces:** extend `parse_stsd` to recognize a `mp4a` sample entry, parse the nested `esds` box (reference §2) → the ASC (audioObjectType, samplingFrequencyIndex→Hz, channelConfiguration). Expose a codec enum so `open` can branch: e.g. change `Mp4Reader` to hold either the ALAC cookie **or** an `AacConfig{object_type,sample_rate,channels}`; add `pub fn codec(&self) -> Mp4Codec` where `enum Mp4Codec { Alac(AlacConfig), Aac(AacConfig) }`. Keep the existing ALAC path working unchanged.

- [ ] **Step 1: Failing test** (add to `mp4.rs` tests):
```rust
#[test]
fn parses_mp4a_esds_asc() {
    let m4a = include_bytes!("../../testdata/aac.m4a");
    let r = Mp4Reader::new(crate::io::SliceSource::new(m4a)).unwrap();
    match r.codec() {
        Mp4Codec::Aac(c) => { assert_eq!(c.object_type,2); assert_eq!(c.sample_rate,44_100); assert_eq!(c.channels,2); }
        _ => panic!("expected AAC"),
    }
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** the `mp4a` branch. The `mp4a` sample entry has the same 8+28-byte AudioSampleEntry preamble as `alac`, then an `esds` box (not an inner `alac` box). Parse the `esds` FullBox (skip 4 version/flags), then the descriptor chain with **expandable lengths** (reference §2) to reach the DecoderSpecificInfo (tag 0x05) bytes = the ASC; parse the ASC bits. Bounds-check every descriptor length against the remaining box bytes → `Malformed`. Keep `Format::Mp4` routing in `open` (Task 8) branching on `codec()`.
- [ ] **Step 4:** Run → PASS; existing `parses_cookie_and_locates_mdat` (ALAC) still passes.
- [ ] **Step 5:** thumbv7em + clippy clean.
- [ ] **Step 6: Commit** `feat(demux): parse mp4a/esds AudioSpecificConfig`.

---

## Task 3: Bit reader + Huffman codebooks + spectral/scalefactor decode

**Files:** Create `formats/aac/bits.rs`, `formats/aac/huffman.rs`; `formats/aac/mod.rs` (`pub(crate) mod bits; mod huffman;`); `formats/mod.rs` (`pub mod aac;`).
**Interfaces:** `bits::BitReader<'a>` (MSB-first: `read(n)`, `read_bit`, `byte_align`, `bit_pos`, `bits_left`). `huffman::decode_spectral(r, cb) -> [i32; 4]` (returns 2 or 4 signed coeffs per reference §6, applying sign bits + ESC for cb 11) and `huffman::decode_scalefactor(r) -> i32` (index 0..120).

- [ ] **Step 1: Failing tests** — decode known codewords (anchors from reference §6):
```rust
#[cfg(test)]
mod tests {
    use super::*; use crate::formats::aac::bits::BitReader;
    #[test]
    fn cb1_zero_codeword() {
        // cb1 index 40 = code 0x000 len 1 → quad (0,0,0,0)
        let mut r = BitReader::new(&[0b0000_0000]);
        assert_eq!(decode_spectral(&mut r, 1), [0,0,0,0]);
        assert_eq!(r.bit_pos(), 1);
    }
    #[test]
    fn scalefactor_zero_codeword() {
        // index 60 = code 0x00000 len 1 → returns 60 (caller subtracts 60)
        let mut r = BitReader::new(&[0b0000_0000]);
        assert_eq!(decode_scalefactor(&mut r), 60);
    }
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement.**
  - `bits.rs`: a straightforward MSB-first reader over a byte slice (you may mirror ALAC's `BitReader` in `formats/alac.rs` — read it for the exact idiom — but keep a separate copy here; do not modify ALAC).
  - `huffman.rs`: **fetch the (code,len) tables** for spectral codebooks 1..11 and the scalefactor book from FFmpeg `libavcodec/aactab.c` (`ff_aac_spectral_codes`/`_bits`, `ff_aac_scalefactor_code`/`_bits`) — the reference doc gives verbatim cb1, cb11, and the scalefactor book to self-check against; transcribe cb2..cb10 from the same source. Store as `const` arrays. Build a decode that reads MSB-first and matches (code,len) (a simple linear/binary match over the ≤289-entry book is fine at these sizes and no-alloc; or a small lookup). Then unpack index→coeffs per §6 (4-dim M=3 signed/unsigned; 2-dim; sign bits for unsigned; ESC for cb 11). **Bounds:** an unmatched codeword (should be impossible for valid input) → `Malformed`, not an infinite read.
- [ ] **Step 4:** Run → PASS. Add a couple more codeword tests for cb5 (`0x000` len1 → `[0,0]`) and one nonzero cb11 ESC value if you can hand-derive it.
- [ ] **Step 5:** thumbv7em + clippy clean.
- [ ] **Step 6: Commit** `feat(aac): bit reader + Huffman codebooks`.

---

## Task 4: Syntax — tables + `raw_data_block`/ICS parse → quantized coeffs

**Files:** Create `formats/aac/tables.rs`, `formats/aac/syntax.rs`.
**Interfaces:** `tables.rs`: `SAMPLE_RATES: [u32;13]`, `NUM_SWB_1024/128: [u8;12]`, `swb_offset_1024(sfi)->&[u16]`, `swb_offset_128(sfi)->&[u16]`, `MAX_TNS_SFB` (§9). `syntax.rs`: `struct Ics { window_sequence, window_shape, max_sfb, num_window_groups, window_group_length[8], num_windows, sfb_cb[8][MAX_SFB], scale_factors[8][MAX_SFB], coeffs:[i32;1024], tns:TnsData, .. }` and `fn decode_ics(r, &mut Ics, config, common_window)`, plus `fn decode_element(r, ..)`/`fn decode_raw_data_block(r, config) -> DecodedFrame{ channels:[ChannelData; 2], n_channels }` (each `ChannelData` = the per-channel `Ics` with quantized `coeffs` filled). Consumes `bits`, `huffman`. Parses (does NOT yet dequant/IMDCT).

- [ ] **Step 1: Failing test** — decode the first frame's structure from the ADTS fixture's first raw_data_block:
```rust
#[test]
fn decodes_first_frame_structure() {
    // Feed the first ADTS frame's raw_data_block bytes; expect a CPE (stereo), long window,
    // sane max_sfb, and 1024 quantized coeffs per channel (mostly small ints).
    let rdb = /* first frame via AdtsReader::next_frame */;
    let mut r = BitReader::new(rdb);
    let f = decode_raw_data_block(&mut r, /*config 44100,2ch*/).unwrap();
    assert_eq!(f.n_channels, 2);
    assert_eq!(f.channels[0].window_sequence, 0); // ONLY_LONG typical for a steady tone
    assert!(f.channels[0].max_sfb > 0 && f.channels[0].max_sfb <= 51);
}
```
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** per reference §3–§6 (transcribe the swb tables from faad2 `specrec.c`; self-check each table's last entry == 1024/128). `decode_raw_data_block`: element-tag loop; SCE/LFE + CPE; for CPE with common_window read ics_info + ms_mask + ms_used; per channel `decode_ics` → global_gain, section_data, scale_factor_data, pulse/tns present flags + data, spectral_data → fill `coeffs[1024]`. **Bounds-check everything**: `max_sfb ≤ num_swb`, section lengths sum to max_sfb, codebook 0..11 (12 illegal → Malformed; 13/14/15 handled), pulse offsets in range, tns order ≤ cap, spectral line index < 1024. Treat NOISE cb (PNS, 13) as `Unsupported` (not used by conformant LC) OR zero the band — choose Unsupported for v1 and document.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** thumbv7em + clippy clean.
- [ ] **Step 6: Commit** `feat(aac): raw_data_block + ICS syntax parse`.

---

## Task 5: Dequant + scalefactors + pulse + de-interleave

**Files:** Create `formats/aac/dsp.rs`.
**Interfaces:** `dsp::dequantize(ics: &mut Ics)` — turn the quantized `coeffs:[i32;1024]` into dequantized `spec:[f32;1024]` (add a `spec` field to the per-channel data) per reference §7: `sign(q)·|q|^(4/3)` × `2^(0.25·(sf−100))` per sfb; pulses already added to the int coeffs in Task 4 (do them there, before dequant); short-block group de-interleave into per-window ascending order.

- [ ] **Step 1: Failing test** — a hand-built ICS: one long window, sfb 0 has coeffs `[3,-3,0,...]` with scalefactor 100 → dequant `[3^(4/3)·2^0, -(3^(4/3)), 0]` = `[±4.3267, 0]`:
```rust
#[test]
fn dequant_applies_power_and_scalefactor() {
    let mut ics = Ics::long_test(); // helper: 1 window, max_sfb 1, swb [0,4], coeffs[0..4]=[3,-3,0,0], sf[0]=100
    dequantize(&mut ics);
    let s = &ics.spec;
    assert!((s[0] - 3f32.powf(4.0/3.0)).abs() < 1e-3);
    assert!((s[1] + 3f32.powf(4.0/3.0)).abs() < 1e-3);
    assert_eq!(s[2], 0.0);
}
```
- [ ] **Step 2–4:** FAIL → implement (`libm::powf` or `iq_table` const; `2^(0.25·(sf−100))` via `exp=sf>>2, frac=sf&3`, `pow2sf[exp]·pow2frac[frac]` const tables) → PASS.
- [ ] **Step 5:** thumbv7em + clippy clean. **Step 6: Commit** `feat(aac): inverse quantization + scalefactors`.

---

## Task 6: Stereo (M/S + intensity) + TNS

**Files:** MODIFY `formats/aac/dsp.rs`.
**Interfaces:** `dsp::apply_stereo(l:&mut Ics, r:&mut Ics, ms_mask_present, ms_used)` (§8: M/S `l=m+s,r=m−s` per band; intensity `r=±l·0.5^(0.25·is_pos)`). `dsp::apply_tns(ics:&mut Ics, sfi)` (§9: decode PARCOR from the 4 coef tables → LPC step-up → per-window AR filter over the TNS bands, honoring direction). Order per pipeline: stereo BEFORE tns; tns per channel.

- [ ] **Step 1: Failing tests** — (a) M/S on a band: `l=[1,2], r=[3,4]` → `l=[4,6], r=[-2,-2]`. (b) TNS AR filter: order-1 LPC `a=[1, 0.5]` on `x=[1,0,0,0]` gives `y=[1,-0.5,0.25,-0.125]` (`y[n]=x[n]-0.5·y[n-1]`). (c) intensity: `is_pos=0` → scale `1.0`, `r=±l`.
- [ ] **Step 2–4:** FAIL → implement (embed faad2's 4 TNS coef tables `tns_coef_{0_3,0_4,1_3,1_4}` from reference §9; step-up recursion; AR filter with per-filter zeroed state) → PASS.
- [ ] **Step 5:** thumbv7em + clippy clean. **Step 6: Commit** `feat(aac): M/S + intensity stereo + TNS`.

---

## Task 7: Filterbank — FFT + IMDCT + windows + overlap-add

**Files:** Create `formats/aac/filterbank.rs`.
**Interfaces:** `filterbank::Filterbank` holding per-channel `overlap:[[f32;1024];2]` + `prev_window_shape:[u8;2]`; `fn imdct_and_overlap(&mut self, ch:usize, ics:&Ics, out:&mut [f32;1024])` — runs the IMDCT of `ics.spec` per the window_sequence, windows (sine/KBD, current vs previous shape), overlap-adds with the carried state, writes 1024 PCM-domain samples, and updates the overlap state. Internal: a no-alloc complex FFT (radix-2/4) of size 512 and 64, and `const` window tables (sine + KBD, long+short) generated by an `xtask` step or precomputed.

This is the hardest task. Follow reference §10 EXACTLY (it has the four verbatim overlap-add loop bodies and the fast-IMDCT twiddle/reorder). Recommended sub-steps:

- [ ] **Step 1: FFT unit test** — implement a fixed complex FFT (size 512/64) and test it against a naive DFT for a random vector (max abs error < 1e-3).
- [ ] **Step 2: IMDCT unit test** — implement `imdct(spec[N/2]) -> time[N]` via the N/4 FFT + pre/post twiddle + reorder (reference §10). Test: an all-zero spectrum → all-zero output; a single-bin spectrum → the expected cosine (compare to the direct IMDCT sum for N=256 at a few `n`). Watch the `√N` twiddle scale vs the textbook `2/N` — pick ONE convention and make the reconstruction test define "correct" (the golden test in Task 8 is the real gate; internal consistency here + Task 8 conformance nails the scale).
- [ ] **Step 3: Windows** — generate sine + KBD (alpha 4 long / 6 short) tables (reference §10; `libm` `i0`/Bessel for KBD, or precompute via `xtask` like the resampler tables). Unit-test window symmetry + endpoint values.
- [ ] **Step 4: Overlap-add** — implement all four window sequences' overlap-add EXACTLY from reference §10 (ONLY_LONG, LONG_START, LONG_STOP, EIGHT_SHORT — the EIGHT_SHORT layout at offset 448 with 50% adjacent-short overlap is the fiddly one). Carry `overlap` + `prev_window_shape` per channel.
- [ ] **Step 5: Integrate + test** — a synthesized single long-window frame → round-trips to a recognizable time signal (sanity); the real correctness gate is Task 8's conformance test.
- [ ] **Step 6:** thumbv7em + clippy clean. **Step 7: Commit** `feat(aac): IMDCT filterbank + overlap-add`.

---

## Task 8: Top-level `AacDecoder` + seam wiring + conformance golden

**Files:** Create `formats/aac/mod.rs` (`AacDecoder`); MODIFY `dispatch.rs` (`AnyDecoder::Aac`, `open` routing for `Format::Mp4`→AAC and `Format::Adts`), `probe.rs`/`model.rs` already done.
**Interfaces:** `AacDecoder<S>` over the frame-source seam: `enum FrameSrc<S> { Mp4(Mp4Reader<S>), Adts(AdtsReader<S>) }` yielding raw_data_block bytes + the config. `AacDecoder::new_mp4(Mp4Reader)`/`new_adts(AdtsReader)` or a single `new(FrameSrc)`. `impl Decoder`: `stream_info` = `{sample_rate, channels, Int{bits:16}}`; `decode_into` decodes frames on demand (each frame → 1024 PCM samples/channel via syntax→dsp→filterbank, interleaved, rounded to i16 range), buffering across `decode_into` calls; `total_frames` from MP4 `mdhd` (ADTS: `None`). `seek_to`→`Unsupported`.

- [ ] **Step 1: Conformance golden test** (`formats::aac` or `tests/decode.rs`): decode `aac.m4a` fully; load `aac.m4a.ref.pcm` (ffmpeg's decode, s16le). **AAC is lossy** — assert a tolerance, not equality:
```rust
#[test]
fn aac_mp4_matches_ffmpeg_within_tolerance() {
    let ours = decode_all("aac.m4a");          // Vec<i32>
    let refr = load_s16le("aac.m4a.ref.pcm");   // Vec<i32>
    // Align: AAC encoder priming delays the signal; find the best small offset
    // (search 0..=3072 samples) that minimizes error, then compare the overlap.
    let (off, segsnr) = best_align_segsnr(&ours, &refr);
    assert!(segsnr > 30.0, "AAC decode SNR vs ffmpeg too low: {segsnr:.1} dB (offset {off})");
}
```
Implement `best_align_segsnr`: for each candidate priming offset, compute segmental SNR = `10·log10( Σref² / Σ(ref−ours)² )` over the overlapping region; take the max. **Calibrate the threshold from the measured value** (two conformant decoders should give a high SNR; if the measured value is, say, 55 dB, set the bar at 40 — with margin, like the ALAC SFDR bar). Document the measured number in a comment. Repeat for the ADTS fixture (`aac.aac`).
- [ ] **Step 2:** Run → FAIL (AacDecoder undefined).
- [ ] **Step 3: Implement** the top-level orchestration (wire syntax → dsp(dequant, stereo, tns) → filterbank per channel, interleave, round to i16 into `i32`), the frame-source seam, and the seam wiring: `probe` `Format::Adts`→AAC; `open` `Format::Mp4` branches on `Mp4Reader::codec()` (Alac→ALAC, Aac→AAC), `Format::Adts`→AAC. `AnyDecoder::Aac` arms in `Decoder`/`total_frames`/`seek_to`/`seek_to_time`. **The `AnyDecoder` `Decoder` impl bound** may need `S: Source` vs `BufSource` — match how ALAC was resolved (relaxed to `Source`); verify the whole workspace compiles.
- [ ] **Step 4:** Iterate against the conformance test until the segmental SNR clears the bar for BOTH fixtures. If early samples align but the signal drifts, suspect the overlap-add / window-shape carry (Task 7) or the scalefactor/dequant scale; if it's garbage, suspect the syntax bit alignment or the IMDCT twiddle. This is the integration gate — expect iteration.
- [ ] **Step 5:** `cargo test -p soul-audio-core`, `cargo clippy --workspace --all-targets -- -D warnings`, thumbv7em build — all clean.
- [ ] **Step 6: Commit** `feat(core): AAC-LC decoder — route MP4/ADTS through open + AnyDecoder`.

---

## Task 9: Playback fixture, e2e, qa-audio, fuzz seed, docs

**Files:** `testdata/tone.m4a` (AAC) + `testdata/tone.aac`; MODIFY `testdata/generate.sh`, `testdata/README.md`, `crates/soul-audio-core/testdata/README.md`, `xtask/src/main.rs` (`qa_audio`), `crates/soul-audio-cli/tests/e2e.rs`, `crates/soul-audio-core/tests/robustness.rs`.

- [ ] **Step 1:** `generate.sh`: add `ffmpeg -y -i tone.wav -c:a aac -b:a 192k -movflags +faststart tone.m4a` and `... -f adts tone.aac`; run it (revert any non-reproducible `tone.ogg`/others it rewrites, commit only the new AAC files + intended edits).
- [ ] **Step 2:** e2e (`tests/e2e.rs`): `real_aac_m4a_renders_stereo_tones` and `real_aac_adts_renders_stereo_tones` — render `tone.m4a`/`tone.aac` at 48000/2, FFT-assert left≈440 Hz, right≈659.25 Hz within 6 Hz (reuse `analyze`/`channel_window`; lossy, so pitch-only, no lossless-equality test).
- [ ] **Step 3:** `qa_audio` fixture list: add `("AAC (.m4a)", "tone.m4a", "AAC-LC — should sound like the WAV")` and `("AAC (ADTS)", "tone.aac", "raw AAC stream")`.
- [ ] **Step 4:** robustness fuzz: add `aac.m4a` + `aac.aac` seeds and the `ftyp`/ADTS-sync magics so the AAC path joins the standing no-panic sweep.
- [ ] **Step 5:** docs: `tone.m4a`/`tone.aac` rows + an AAC section in both READMEs.
- [ ] **Step 6:** Run `cargo test -p soul-audio-cli --test e2e`, full `cargo test`, clippy, fmt — clean. **Step 7: Commit** `test(aac): playback fixtures, e2e, qa-audio, fuzz seed`.

---

## Self-review notes (for the executor)

- **Spec coverage:** T0 fixtures; T1 ADTS+probe; T2 mp4a/esds; T3 Huffman; T4 syntax; T5 dequant; T6 stereo+TNS; T7 filterbank; T8 integration+seam+lossy golden; T9 playback/e2e/fuzz. All spec sections covered.
- **Lossy golden:** the segmental-SNR-vs-ffmpeg with priming alignment is the primary correctness gate (Task 8) — bit-exact is impossible; the tone-FFT (Task 9) is the absolute backstop.
- **Hardest tasks:** T7 (filterbank/IMDCT/overlap-add) and T8 (integration) — expect iteration; the reference §10 has the exact overlap-add loops.
- **Firmware invariant:** every bitstream-driven index/count/length is bounds-checked → `Err`, verified by the T9 fuzz seed + the final whole-branch review.
- **Type consistency:** `AacConfig{object_type,sample_rate,channels}`, `Ics`, `Mp4Codec`, `Format::Adts`, `AnyDecoder::Aac`, `FrameSrc` used consistently across tasks.
