# Opus Decoder — Design

**Status:** Approved design (2026-09-06). Implements Forgejo issue **#35** (Opus decoding — split from #24; the Vorbis half shipped in `1ae52d3`).

**Goal:** A from-scratch, `no_std`, no-alloc, full **RFC 6716** Opus decoder (SILK + CELT + hybrid) in `soul-audio-core` so **any** `.ogg`/Opus file plays — the user has real `.opus` music (a ~139 kbps CELT album) that currently scans but fails to play, and the full decoder also covers low-bitrate/voice content.

**Non-goal:** Opus encoding; multistream/surround beyond the channel cap; DRED/deep-PLC extensions.

---

## Why this shape (grounded in research)

Opus is **architecturally simpler for our no-alloc target than Vorbis was**, but a **bigger build** (two sub-codecs + a range coder). Two facts drive the design:

1. **No per-file setup → pure fixed-capacity, no arena.** Unlike Vorbis (whose setup header defines a *file-variable* number of codebooks, which forced the caller-provided arena), **every Opus table is fixed by RFC 6716** (SILK NLSF/LTP codebooks, CELT band layout + allocation tables, MDCT twiddles, range-coder ICDFs). All are `const`/xtask-generated. So the decoder is **self-contained, fixed-cap, no-alloc, no-arena** — it works on the DAP with zero caller storage, and it slots in as a **normal `AnyDecoder` variant** (not consumer-dispatched like Vorbis). By-value state is only **~30-40 KB** (well under FLAC's ~147 KB), so it fits the existing big-stack-thread pattern.

2. **Three specs.** RFC **6716** = the codec bitstream (TOC, range coder, SILK, CELT, hybrid). RFC **7845** = Ogg-Opus encapsulation (`OpusHead`/`OpusTags`, **pre-skip**, output gain, channel mapping). RFC **8251** = decoder errata + the current **conformance test vectors**. All three are needed: 6716 is the core, 7845 the container glue, 8251 the errata + test oracle.

Everything else carries over from the shipped Vorbis decoder: the packet-preserving Ogg demuxer (`demux/ogg_packets.rs`), the xtask const-table pattern, the SNR-vs-ffmpeg golden harness, and the `Decoder`/`SeekableDecoder` traits.

---

## Global Constraints

- **No AI trailers in commits** (lefthook rejects `Co-Authored-By: Claude`/`Claude-Session`); GPG-signed.
- **`soul-audio-core` stays strictly `no_std` + no-alloc + `#![forbid(unsafe_code)]`.** No new dependency (only `embedded-io`), no `alloc`, no `unsafe`. Default build + `cargo xtask verify` (incl. `thumbv7em-none-eabihf`) green at the end of every task.
- **No runtime transcendentals / `libm`.** All tables (range-coder ICDFs, NLSF/LTP/pitch codebooks, CELT eBands/allocation, MDCT twiddles/windows) are precomputed host-side by an `xtask gen-opus-tables` command and checked in (the established `resample_tables`/`vorbis_tables` pattern) — with a `check` gate.
- **Lossy codec, tolerance-based conformance.** Opus is intentionally NOT bit-exact; conformance is `opus_compare` (weighted spectral RMSE). Goldens use SNR/spectral tolerance vs an ffmpeg reference, AND the 12 official RFC 8251 vectors as a hard gate.
- **Fixed-capacity, reject-over-cap.** Bounded structures (channels, frame sizes, LPC order, bands) are fixed arrays sized to the RFC worst case; anything out of range → `DecodeError::Unsupported`/`Malformed`, never panic.
- **Fixture naming:** `tone.opus` (already committed, CELT-only fullband) is the CELT fixture; add `tone-opus-voice.opus` (low-bitrate SILK) and `tone-opus-hybrid.opus` (hybrid) for the other modes.

## Licensing note (important)

- **libopus is BSD-3-Clause** — its tables and structure may be read/ported with attribution; the RFC 6716 Appendix A also embeds the reference C.
- Prefer studying **oxideav-opus** (a clean-room, RFC-only pure-Rust decoder, MIT) for *structure* to keep the implementation clean-room where practical, and take **tables** from libopus/RFC (BSD, attributed). Cross-check behavior against **mousiki** (no_std+alloc port) and **rusopus/`opus-decoder`** (passes the 12 vectors). Keep any copied constant tables' provenance noted.

---

## Architecture

### Dispatch: a normal `AnyDecoder` variant (no arena)

Because Opus is fixed-cap with no caller storage, it is added as `AnyDecoder::Opus(OpusDecoder<S>)` and wired into `open()` — **no consumer special-casing** (unlike Vorbis/DSD). Desktop `OwnedDecoder::Pcm(AnyDecoder)` and the CLI both route through `open()`, so they get Opus for free once `probe`/`open` handle it.

- `model.rs`: add `Format::Opus`.
- `probe.rs`: in the `b"OggS"` arm, detect `OpusHead` (the id header magic) → `Format::Opus`.
- `dispatch.rs`: `open()` → `Format::Opus => OpusDecoder::new(...).map(AnyDecoder::Opus)`; add `AnyDecoder::Opus` to every trait arm (`stream_info`/`decode_into`/`seek_to`/`total_frames`).
- The by-value `OpusDecoder<S>` (~30-40 KB) is smaller than the FLAC variant, so it does not grow `AnyDecoder`'s max size.

### Module layout (`src/formats/opus/`)

- `mod.rs` — `OpusDecoder<S>`, the packet loop, `Decoder`/`SeekableDecoder` impls, pre-skip/end-trim/gain.
- `container.rs` — RFC 7845 `OpusHead`/`OpusTags` parse (channels, pre-skip, output gain, mapping family); reuse `demux/ogg_packets.rs` for pages/packets/granule.
- `toc.rs` — TOC byte → (mode, bandwidth, frame size, stereo); code 0/1/2/3 frame packing (incl. code 3 VBR/padding).
- `range.rs` — the `ec_dec` range/entropy decoder (arithmetic front + backward raw-bit reader). **Foundation — everything reads from it.**
- `celt/` — `decoder.rs` (energy, allocation, PVQ shape, anti-collapse, denormalize, IMDCT+overlap, stereo, downsample), `rate.rs` (deterministic bit allocation), `pvq.rs` (CWRS combinatorics + band split), `mdct.rs` (Opus MDCT/overlap + the 120-sample window), `tables` (generated).
- `silk/` — `decoder.rs` (frame type/gains, NLSF VQ + stabilization, NLSF→LPC, LSF interp, LTP/pitch, shell-coded excitation, LTP+LPC synthesis, stereo unmix), `resampler.rs` (8/12/16 kHz → output), `tables` (generated).
- `hybrid.rs` — SILK low band + CELT high band (`start` band offset) + delay compensation + sum.
- `opus_tables.rs` — `include!`d generated tables; `xtask/src/opus_tables.rs` generator + `check`.

### The decode pipeline (per packet)

Container (RFC 7845) → TOC/framing (§3) → for each frame: **range decoder** (§4.1) drives → **SILK** (§4.2, if mode is SILK/hybrid) and/or **CELT** (§4.3, if CELT/hybrid) from the *same* range coder → **hybrid combine** (§?, if hybrid) → resample/decimate to output rate → **pre-skip trim** (first `pre_skip` samples, 48 kHz units) + **end-trim** (final granule) + **output gain**. Ogg granule is always 48 kHz.

### Memory

- `OpusDecoder<S>` holds fixed-size per-channel state: CELT `decode_mem` (`DECODE_BUFFER_SIZE 2048` + `overlap 120` per channel ≈ 17 KB stereo), energy history, SILK LPC/LTP/resampler delay lines (~3-6 KB stereo). Total **~30-40 KB by value** (worst case stereo FB) — under the FLAC variant, so no `AnyDecoder` size regression; decode on the existing big-stack thread.
- **Transient decode scratch** (libopus's VLA `freq[]`/`X[]` ~7.7 KB each, SILK synth buffers) becomes fixed-size stack arrays sized for the 960-sample / 1920-MDCT worst case (~10-25 KB stack during a decode call). Size the decode thread stack for it.
- Channel cap: 2 (stereo) for the first cut; a higher cap is a const/feature (matching the Vorbis profile approach). Channel mapping family 0/1.

---

## Build order (phased milestones — each golden-tested)

Mirrors the Vorbis cadence. **CELT is the first-correct-PCM milestone and plays the user's real music** (their `.opus` files are CELT-only fullband); SILK and hybrid follow.

**Phase A — container + framing + range coder (foundation)**
1. RFC 7845 `OpusHead`/`OpusTags` parse + pre-skip/gain/mapping plumbing (reuse the Ogg demuxer); `Format::Opus`/probe/`AnyDecoder` skeleton (decode_into stub).
2. TOC byte + code 0/1/2/3 frame packing (incl. code 3 VBR/padding).
3. **Range decoder `ec_dec`** — arithmetic path + the backward raw-bit reader; unit-tested against known vectors. *Must be bit-exact before anything downstream.*

**Phase B — CELT → first correct PCM (plays real music)**
4. CELT energy decode (coarse Laplace + fine) + the eBands/energy-prediction tables.
5. **CELT bit allocation** (`compute_allocation`/`rate.c`) — deterministic, table-driven, must derive the exact per-band budget. *Top bug risk.*
6. **PVQ / CWRS** band-shape decode (unit-pulse combinatorics + band splitting). *Second bug risk.*
7. CELT anti-collapse + denormalize (shape × energy).
8. CELT **IMDCT + overlap-add** (120-sample window) + transient short blocks + stereo (mid/side, intensity) + downsample → **first correct PCM golden vs ffmpeg** on `tone.opus` (CELT). This is the milestone that plays the user's music.

**Phase C — SILK**
9. SILK frame type/gains + **NLSF VQ (2-stage) + stabilization** + NLSF→LPC + LSF interpolation.
10. SILK LTP/pitch + **shell-coded excitation** (pulse counts/locations/LSBs/signs) + gains.
11. SILK LTP+LPC synthesis + stereo unmix + **resampler** (8/12/16 kHz → output) → **SILK golden** vs ffmpeg on a low-bitrate voice fixture.

**Phase D — hybrid + integration**
12. **Hybrid combine** — SILK low band + CELT high band (`start`-band offset) + delay compensation + sum → **hybrid golden**.
13. Integration hardening: pre-skip trimming, end-trim via granule, output gain, `SeekableDecoder::seek_to` (granule/48 kHz + re-prime overlap/resampler/pre-skip), PLC/comfort-noise for lost packets; wire `AnyDecoder`/probe/CLI/desktop; add the **12 RFC 8251 conformance vectors** as a gate; qa-audio.

Each phase's tasks are split further at plan time (Phase B alone ≈ a whole Vorbis). Expect **~22-28 tasks total**.

---

## Testing

- **SNR-vs-ffmpeg goldens** per mode: `tone.opus` (CELT), a low-bitrate mono voice clip (`-b:a 16k` → SILK), a hybrid clip (`-b:a 24k`), stereo + mono. Decode with ffmpeg/opusdec to `.ref.pcm`; **align by trimming `pre_skip`** (48 kHz units) off our output before comparing; decide **pre- vs post-output-gain** and match the reference (opusdec applies gain — either apply gain in our decoder and compare post-gain, or generate the reference with gain disabled). Big-stack thread.
- **RFC 8251 conformance vectors** (`opus_testvectors-rfc8251.tar.gz`, 12 vectors) — the hard gate; pass under an `opus_compare`-style tolerance (RMSE), exercising mode switching / stereo / PLC / edge cases the tone clips won't. (Vectors are committed or fetched by the test; decide at plan time — committing 12 vectors is sizable, so consider an `#[ignore]`d corpus mode + a couple of representative committed vectors.)
- **Sub-component unit tests** (high leverage, since a full-frame SNR failure barely localizes on a range-coded stream): `ec_dec` vs known sequences, CWRS enumeration, NLSF→LPC, the bit-allocation output — each vs libopus reference values, before integration.

## Risks / hard parts (flagged)

- **CELT bit allocation** — deterministic and must match the encoder's derived per-band budget *exactly*; a 1-bit mismatch desyncs the whole frame. The single most error-prone piece.
- **PVQ / CWRS** — exact integer combinatorics (V(n,k) enumeration) with band splitting; unusual, off-by-one-prone; needs isolated tests.
- **SILK NLSF stage-2 VQ + stabilization** and **shell-coded excitation** — fiddly.
- **SILK resampler + hybrid delay compensation** — the classic "audio right but shifted/comb-filtered" bug.
- **Range coder backward raw-bit read** — subtle; if wrong, misattributes to CELT/SILK.
- **Conformance-vector size** — 12 vectors + refs are sizable to commit; decide corpus-vs-committed at plan time.

## Out of scope (follow-ups)

- Opus encoding; DRED / deep-PLC / neural extensions.
- >2 channels / surround mapping family beyond a raised cap.
- Fast/SIMD transforms (naïve-correct first, like Vorbis).
