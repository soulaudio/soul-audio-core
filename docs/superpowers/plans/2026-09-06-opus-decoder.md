# Opus Decoder — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A from-scratch, `no_std`, no-alloc, full **RFC 6716** Opus decoder (SILK + CELT + hybrid) in `soul-audio-core` so any `.opus` file plays. Implements Forgejo issue **#35**.

**Design spec:** `docs/superpowers/specs/2026-09-06-opus-decoder-design.md` (read it — rationale, memory model, licensing).

**Reference (implement FROM these, not by transcription):** RFC **6716** (codec — TOC, range coder, SILK, CELT, hybrid), RFC **7845** (Ogg-Opus: OpusHead/pre-skip/gain), RFC **8251** (errata + the 12 conformance vectors). Oracle: **libopus** (BSD-3; tables + structure). Study **oxideav-opus** (clean-room, MIT) for structure; cross-check **mousiki** / **rusopus**.

## How to read this plan

Two task classes (same as the shipped Vorbis plan):
- **Integration/scaffolding** (container, TOC, probe/`AnyDecoder` wiring, xtask tables, test harness): the plan gives **complete code** — transcribe + test.
- **DSP/codec** (range coder, CELT energy/allocation/PVQ/MDCT, SILK NLSF/excitation/synthesis/resampler, hybrid): the plan gives the **interface + the RFC section + the reference + the known bug-sources + the golden/unit test that defines done.** Implement from the RFC + libopus/oxideav; **the test is the contract.** Fabricated DSP code would be wrong — it is deliberately not provided.

## Global Constraints

- **No AI trailers in commits** (lefthook rejects `Co-Authored-By: Claude`/`Claude-Session`); GPG-signed.
- **`soul-audio-core` stays `no_std` + no-alloc + `#![forbid(unsafe_code)]`.** No new dependency, no `alloc`/`Vec`/`Box`/`unsafe`/`libm` in shipping code (test-only alloc OK). `cargo xtask verify` (incl. `thumbv7em-none-eabihf` + `opus_tables::check`) green at the END of every task.
- **All tables `const`/xtask-generated** (RFC-fixed — Opus has NO per-file setup, so NO arena). Bounded structures are fixed arrays sized to the RFC worst case; out-of-range → `Unsupported`/`Malformed`, never panic.
- **Lossy → tolerance goldens** (SNR vs ffmpeg + the RFC 8251 vectors under an `opus_compare`-style RMSE), never bit-exact. **Trim `pre_skip`** (48 kHz units) in alignment; decide pre/post-output-gain and match the reference.
- **Big-stack tests:** `OpusDecoder` by value + ~10-25 KB transient decode scratch → integration tests use the existing 16 MiB stack-thread helper.
- **Fixtures:** `tone.opus` (committed, CELT-only FB) = CELT golden. Add `tone-opus-voice.opus` (`-b:a 16k` mono → SILK) and `tone-opus-hybrid.opus` (`-b:a 24k` → hybrid). Commit only new fixtures (restore other regenerated `testdata/` binaries).

## File Structure

- Create `src/formats/opus/{mod,container,toc,range}.rs`, `src/formats/opus/celt/{mod,decoder,rate,pvq,mdct}.rs`, `src/formats/opus/silk/{mod,decoder,resampler}.rs`, `src/formats/opus/hybrid.rs`, `src/formats/opus/opus_tables.rs` (generated).
- Modify `src/model.rs` (`Format::Opus`), `src/probe.rs` (`OpusHead`), `src/dispatch.rs` (`AnyDecoder::Opus` + `open()`), `src/formats/mod.rs`, `src/demux/mod.rs` (reuse `ogg_packets`).
- Create `xtask/src/opus_tables.rs` + wire `gen-opus-tables`/`check` into `xtask/src/main.rs`.
- Modify `crates/soul-audio-core/tests/decode.rs` (goldens + conformance), `xtask/src/main.rs` (qa-audio). Desktop/CLI need **no** change (Opus routes through `open()` → the existing `OwnedDecoder::Pcm`/CLI paths).

---

## Phase A — container + framing + range coder

### Task 1: Ogg-Opus container + `Format::Opus` + probe + `AnyDecoder::Opus` skeleton

**Files:** create `src/formats/opus/{mod,container}.rs`; modify `src/model.rs`, `src/probe.rs`, `src/dispatch.rs`, `src/formats/mod.rs`.

**Interfaces (Produces):**
```rust
// container.rs — RFC 7845
pub struct OpusHead { pub channels: u8, pub pre_skip: u16, pub output_gain_q8: i16, pub mapping_family: u8 }
impl OpusHead { pub fn parse(id_packet: &[u8]) -> Result<OpusHead, DecodeError>; } // "OpusHead", version 1, ch@9, pre_skip@10(LE u16), gain@16(LE i16), family@18

// mod.rs — skeleton; decode filled later
pub struct OpusDecoder<S> { /* OggPackets<S>, OpusHead, per-frame state (later) */ }
impl<S: Source> OpusDecoder<S> {
    pub fn new(src: S) -> Result<Self, DecodeError>; // read OpusHead(pkt1)+OpusTags(pkt2, skip); stream_info = 48000/channels/Int{16}
}
impl<S> Decoder for OpusDecoder<S> { /* stream_info; decode_into -> Ok(0) until Task 8 */ }
impl<S: SeekSource> SeekableDecoder for OpusDecoder<S> { /* seek_to Unsupported until Task 13; total_frames from end granule - pre_skip */ }
```
- `model.rs`: add `Format::Opus`. `probe.rs`: in the `b"OggS"` arm, detect `OpusHead` in the BOS page → `Some(Format::Opus)` (before the `None`). `dispatch.rs` `open()`: `Format::Opus => OpusDecoder::new(src).map(AnyDecoder::Opus)`, and add `AnyDecoder::Opus(OpusDecoder<S>)` to the enum + all trait arms (`stream_info`/`decode_into`/`seek_to`/`total_frames`). Reuse `demux::ogg_packets::OggPackets` for pages/packets/granule (Opus granule is 48 kHz).

- [ ] **Step 1: Failing tests** — `OpusHead::parse` on a hand-built id packet yields channels/pre_skip/gain; `probe` returns `Format::Opus` for an `OpusHead` BOS page (and still `OggFlac`/`Vorbis` for theirs); `open()` builds an `AnyDecoder::Opus` whose `stream_info` = 48000/ch/Int{16}. Also open the real `testdata/tone.opus` and read its header.
- [ ] **Step 2: Run, see fail.**
- [ ] **Step 3: Implement** container parse + wiring + skeleton (`decode_into` → `Ok(0)`).
- [ ] **Step 4: Tests green.**
- [ ] **Step 5: `cargo xtask verify` green (thumbv7em: `OpusDecoder<S>` compiles for embedded). Commit.**

### Task 2: TOC byte + frame packing (codes 0/1/2/3)

**Files:** create `src/formats/opus/toc.rs`.

**Spec:** RFC 6716 §3.1 (TOC), §3.2 (frame packing). Well-defined byte bookkeeping.

**Interfaces (Produces):**
```rust
pub enum Mode { SilkOnly, Hybrid, CeltOnly }
pub struct Config { pub mode: Mode, pub bandwidth: Bandwidth, pub frame_size_48k: u16, pub stereo: bool }
pub fn parse_toc(b0: u8) -> Config; // config=b0>>3 -> table; stereo=(b0>>2)&1
/// Split a packet into frames (borrowed subslices). Handles code 0/1/2/3 (incl.
/// code-3 VBR/CBR + padding). Returns up to MAX_FRAMES (48) frame slices + the Config.
pub fn frames<'a>(packet: &'a [u8], out: &mut [&'a [u8]; 48]) -> Result<(Config, usize), DecodeError>;
```
Config table (§3.1): 0-11 SILK (NB/MB/WB, 10/20/40/60 ms), 12-15 Hybrid (SWB/FB, 10/20 ms), 16-31 CELT (NB/WB/SWB/FB, 2.5/5/10/20 ms).

- [ ] **Step 1: Failing tests** — the 32 configs map to the right (mode, bandwidth, frame size); code 0 (1 frame), code 1 (2 equal), code 2 (length-prefixed), code 3 CBR (M equal), code 3 VBR (M-1 self-delimited lengths) + padding each split correctly (hand-built packets); >120 ms total or malformed → `Malformed`.
- [ ] **Step 2-4: Implement** `parse_toc` + `frames`; tests green.
- [ ] **Step 5: verify green. Commit.**

### Task 3: Range decoder (`ec_dec`)

**DSP task — must be bit-exact.** **Files:** create `src/formats/opus/range.rs`.

**Spec:** RFC 6716 §4.1 (range decoder). **Reference:** libopus `celt/entdec.c`; oxideav range decoder. **Bug source:** `ec_dec_bits` reads **raw bits from the END of the packet backward** while the arithmetic stream advances from the front — they meet in the middle; getting this wrong makes every downstream bug look like a range-coder bug.

**Interfaces (Produces):**
```rust
pub struct RangeDecoder<'a> { /* buf, front byte pos, val, rng, end byte pos, raw bit accumulator */ }
impl<'a> RangeDecoder<'a> {
    pub fn new(buf: &'a [u8]) -> Self;                        // rng=128, val=127-(b0>>1)
    pub fn decode(&mut self, ft: u32) -> u32;                // ec_decode (freq)
    pub fn dec_update(&mut self, fl: u32, fh: u32, ft: u32);  // ec_dec_update + renorm
    pub fn decode_bit_logp(&mut self, logp: u32) -> u32;      // ec_dec_bit_logp
    pub fn decode_icdf(&mut self, icdf: &[u8], ftb: u32) -> u32; // ec_dec_icdf
    pub fn dec_bits(&mut self, bits: u32) -> u32;             // ec_dec_bits (BACKWARD raw bits)
    pub fn dec_uint(&mut self, ft: u32) -> u32;               // ec_dec_uint
    pub fn tell(&self) -> u32; pub fn tell_frac(&self) -> u32;
}
```

- [ ] **Step 1: Failing tests** — encode a known symbol sequence by hand (or use RFC §4.1 examples / a libopus-derived vector) and assert `decode`/`dec_update`/`decode_icdf`/`dec_bits`/`dec_uint` reproduce the exact symbols; confirm the front arithmetic and the backward raw-bit reader coexist without collision on a small buffer.
- [ ] **Step 2-4: Implement** the range decoder from §4.1; tests green.
- [ ] **Step 5: verify green. Commit.** *Do not proceed to CELT/SILK until this is provably correct.*

---

## Phase B — CELT → first correct PCM (plays real music)

### Task 4: CELT static tables + `xtask gen-opus-tables`

**Scaffolding task.** **Files:** create `xtask/src/opus_tables.rs` + wire `gen-opus-tables`/`check` into `xtask/src/main.rs` (mirror `gen-vorbis-tables`); create `src/formats/opus/celt/mdct.rs` + `src/formats/opus/opus_tables.rs` (generated).

Generate host-side (from libopus/RFC constants, no runtime trig): CELT `eBands` (21-band layout @ 48 kHz + the per-frame-size band tables), the **allocation tables** (`cache.bits`, `cache.index`, `band_allocation`, `log2Frac`, etc.), `e_prob_model`/coarse-energy prediction tables, the **MDCT twiddles** for N ∈ {120,240,480,960} and the **120-sample overlap window**, and the range-coder Laplace/ICDF constants CELT needs.

- [ ] **Step 1:** `gen-opus-tables` emits `src/formats/opus/opus_tables.rs`; `opus_tables::check` re-derives and diffs (like `vorbis_tables::check`).
- [ ] **Step 2:** run the generator, commit the generated file; `check` wired into `verify`.
- [ ] **Step 3:** a unit test spot-checks a couple of known table values (e.g. `eBands` boundaries) vs the RFC.
- [ ] **Step 4: verify green (incl. `opus_tables::check` + thumbv7em). Commit.**

### Task 5: CELT energy decode (coarse + fine)

**DSP task.** **Spec:** RFC 6716 §4.3.2-4.3.3. **Reference:** libopus `celt/quant_bands.c` (`unquant_coarse_energy`/`unquant_fine_energy`). **Interface:** `unquant_energy(rd, config, bands, intra, out_energy: &mut [f32; MAX_BANDS*2], ...)` — coarse via the Laplace model with 2-D (inter-band + inter-frame) prediction, then fine bits.

- [ ] Failing test: decode the energy envelope for a real `tone.opus` CELT frame; assert finite/in-range and stable across frames (exact values validated at Task 8). RED → implement → GREEN → verify. Commit.

### Task 6: CELT bit allocation

**DSP task — TOP RISK.** **Spec:** RFC 6716 §4.3.3 (allocation). **Reference:** libopus `celt/rate.c` (`compute_allocation`, `interp_bits2pulses`). Deterministic, table-driven — the decoder must derive the **exact** per-band pulse/fine-bit budget the encoder used; a 1-bit mismatch desyncs the whole frame. **Interface:** `compute_allocation(config, offsets, trim, ... ) -> (pulses: [i32; MAX_BANDS], fine_bits: [i32; MAX_BANDS], ...)`.

- [ ] Failing test: for a real CELT frame, assert the allocation matches a libopus-derived reference (per-band pulse counts) — this is the one place to compare intermediate values against libopus directly. RED → implement → GREEN → verify. Commit.

### Task 7: PVQ / CWRS band-shape decode

**DSP task — second risk.** **Spec:** RFC 6716 §4.3.4. **Reference:** libopus `celt/cwrs.c` (`decode_pulses`, `cwrsi`/`icwrs`, `V(n,k)`) + `celt/vq.c` (`alg_unquant`, band split). Exact integer combinatorics (unit-pulse vector enumeration) + wide-band splitting + stereo (mid/side, intensity/`dual_stereo`). **Interface:** `decode_band_shape(rd, band, K, N, ...) -> normalized shape into X[]`.

- [ ] Failing test: CWRS enumeration unit tests (known `(N,K) → index → vector` round-trips) + a real-frame band-shape decode is finite/unit-norm. RED → implement → GREEN → verify. Commit.

### Task 8: CELT synthesis → FIRST CORRECT PCM (golden)

**DSP task — milestone.** **Files:** `celt/decoder.rs`, `celt/mdct.rs`; finish `decode_into` for CELT-only packets. **Spec:** §4.3.5-4.3.7 (anti-collapse, denormalize, inverse MDCT + overlap-add, transient short blocks, de-emphasis, downsample). **Reference:** libopus `celt/celt_decoder.c` (`celt_decode_with_ec`), `celt/mdct.c`.

- [ ] **Step 1: Failing GOLDEN** in `tests/decode.rs`: decode `testdata/tone.opus` (CELT-only FB), compare to an ffmpeg `s16le` reference via segmental SNR (trim `pre_skip`), require a high bar (aim near-conformance; if low, it's a real bug — allocation/PVQ/MDCT/energy — not a bar to lower). Big-stack thread. Generate + commit the `.ref.pcm`.
- [ ] **Step 2-4: Implement** anti-collapse + denormalize + IMDCT + overlap-add (120 window) + transient/short-block + stereo + downsample; finish `decode_into` (interleaved `i32`, 16-bit). **First end-to-end correct PCM — plays real music.**
- [ ] **Step 5: verify green (incl. opus_tables::check + thumbv7em). Commit.**

---

## Phase C — SILK

### Task 9: SILK headers + NLSF (VQ + stabilization + NLSF→LPC + LSF interp)

**DSP task.** **Files:** `silk/decoder.rs` + SILK tables (extend `gen-opus-tables`). **Spec:** RFC 6716 §4.2.7 (header/gains), §4.2.7.5 (NLSF stage-1/2 VQ + stabilization + NLSF→LPC), §4.2.7.5.7 (LSF interpolation). **Reference:** libopus `silk/decode_*`, `silk/NLSF2A.c`, `silk/NLSF_stabilize.c`. **Bug source:** stage-2 backward-predicted residual + stabilization; the NLSF→LPC conversion.

- [ ] Failing test: SILK header/gains + NLSF decode for a real low-bitrate voice frame → LPC coefficients are stable/in-range; NLSF→LPC vs a libopus reference for a known NLSF vector. RED → implement → GREEN → verify. Commit.

### Task 10: SILK LTP/pitch + shell-coded excitation

**DSP task.** **Spec:** §4.2.7.6 (LTP: pitch lags + 5-tap filter + scaling), §4.2.7.8 (excitation: pulse counts, shell/combinatorial pulse locations, LSBs, signs, seed). **Reference:** libopus `silk/decode_pulses.c`, `silk/shell_coder.c`. **Bug source:** the shell (combinatorial split) pulse-location decode.

- [ ] Failing test: excitation decode for a real frame produces the expected pulse pattern (vs libopus reference for a known frame); LTP taps/lag in range. RED → implement → GREEN → verify. Commit.

### Task 11: SILK synthesis + stereo unmix + resampler → SILK golden

**DSP task.** **Files:** `silk/resampler.rs`. **Spec:** §4.2.7.9 (LTP+LPC synthesis), §4.2.7.10 (stereo unmix), §4.2.9 (resampler to output rate). **Reference:** libopus `silk/decode_core.c`, `silk/resampler*.c`. **Bug source:** the resampler delay lines.

- [ ] **Step 1: Failing GOLDEN**: decode a low-bitrate mono voice fixture (`tone-opus-voice.opus`, `-b:a 16k`) vs ffmpeg reference SNR (trim pre_skip). Generate + commit the fixture + `.ref.pcm`.
- [ ] **Step 2-4: Implement** synthesis + stereo unmix + resampler; golden green.
- [ ] **Step 5: verify green. Commit.**

---

## Phase D — hybrid + integration

### Task 12: Hybrid combine → hybrid golden

**DSP task.** **Spec:** §? hybrid (SILK low band + CELT high band, `start`-band offset 17, delay compensation, sum). **Reference:** libopus `src/opus_decoder.c` (`opus_decode_frame` hybrid path). **Bug source:** the SILK→48 kHz upsample + delay alignment (comb-filter/shift bug).

- [ ] **Step 1: Failing GOLDEN**: `tone-opus-hybrid.opus` (`-b:a 24k`) vs ffmpeg reference SNR. Generate + commit fixture + ref.
- [ ] **Step 2-4: Implement** the hybrid combine (both sub-codecs from the same range coder, then align + sum); golden green.
- [ ] **Step 5: verify green. Commit.**

### Task 13: Integration — pre-skip/end-trim/gain + seek + PLC + qa-audio

**Integration task.** **Files:** `mod.rs`, `tests/decode.rs`, `xtask/src/main.rs`.
- **Pre-skip:** discard the first `pre_skip` samples (48 kHz units) at stream start; **end-trim** the last block to the final granule; **output gain** (`10^(gain_Q7.8/(20·256))`). Exact-length assertion (like the Vorbis granule end-trim).
- **Seek:** `SeekableDecoder::seek_to` — granule bisection (48 kHz units) via `OggPackets::seek_to_granule`, then re-prime (decode-and-discard, reset overlap/resampler/pre-skip state). `total_frames` = end_granule − pre_skip.
- **PLC:** lost/missing packet → CELT pitch-based extrapolation / comfort noise (§4.4) — at minimum, emit silence/held state without desync.
- **qa-audio:** add `tone.opus` to the `xtask qa-audio` list. Desktop/CLI need no change (Opus routes through `open()`), but confirm `OwnedDecoder::Pcm` + the CLI decode `.opus` end-to-end (add a CLI golden `real_opus_renders_stereo_tones` via `analyze`).

- [ ] Failing tests: seek round-trip SNR + exact-length after pre-skip/end-trim; seek-to-0; beyond-end clamp; CLI golden. RED → implement → GREEN → verify. Commit.

### Task 14: RFC 8251 conformance vectors

**Testing task.** **Files:** `tests/decode.rs` (or a new `tests/opus_conformance.rs`).
- Add the **12 RFC 8251 vectors** (`opus_testvectors-rfc8251.tar.gz`) as a gate under an `opus_compare`-style RMSE tolerance. Committing all 12 + refs is sizable — **commit 2-3 representative vectors** (covering mode-switching + stereo + PLC) and provide an `#[ignore]`d full-corpus mode that reads the rest from a local path (documented in the test), mirroring the tags crate's corpus pattern.
- Implement the `opus_compare` RMSE metric (or reuse the segmental-SNR helper with the vectors' expected tolerance).

- [ ] Failing test: the committed representative vectors decode within tolerance. RED → implement → GREEN → `cargo xtask verify` fully green. Commit.

---

## Self-review notes (for the executor)

- **Range coder (Task 3) is the foundation** — do not build CELT/SILK on a shaky `ec_dec`. The backward raw-bit read is the classic trap.
- **CELT allocation (Task 6) and PVQ/CWRS (Task 7) are the top bug sources** — compare intermediate values against libopus directly the moment Task 8's golden misbehaves; don't debug from "audio sounds wrong."
- **Naïve-correct MDCT first** (like Vorbis); optimization is a follow-up.
- **thumbv7em + `opus_tables::check` stay green every task** — the decoder is fixed-cap no-alloc, so it compiles for embedded from the start.
- **Pre-skip alignment** in every golden (trim `pre_skip` before comparing), and decide pre/post-gain once and match the reference.
- **Do not commit regenerated `testdata/` binaries** — only the new `.opus`/`.ref.pcm` fixtures.
- Never add AI trailers to commit messages.
