# AAC-LC decoder (Phase 1 of HE-AACv2) — design

**Date:** 2026-08-26
**Status:** Approved (design review)
**Depends on:** the decode seam (`Decoder`, `AnyDecoder`, `probe`/`Format`), the
`demux/` layer, the reusable MP4 box reader (`demux/mp4.rs`), and the ffmpeg
golden-vector + FFT signal-analysis test patterns.

## 0. Phasing

The goal is **HE-AACv2** (AAC-LC + SBR + PS), which is too large for one
spec/plan. It is decomposed into three sequential, independently-useful phases,
each its own spec → plan → PR:

1. **AAC-LC** (this spec) — the core codec; plays virtually all AAC music files.
2. **SBR** (later) — Spectral Band Replication → HE-AAC; wraps the LC output with
   a QMF filterbank + high-frequency reconstruction.
3. **PS** (later) — Parametric Stereo → HE-AACv2; operates in SBR's QMF domain.

SBR/PS depend on LC (and each other) and are out of scope here.

## 1. Goal

A from-scratch, `no_std`, no-alloc **AAC-LC** decoder for `soul-audio-core`,
reading from **MP4/.m4a** (via the existing MP4 reader) and **raw ADTS (.aac)**,
wired into the `open()`/`AnyDecoder` seam. Output: interleaved `i32` PCM at
16-bit.

## 2. Seam & containers — two entry paths, one shared core

- **MP4/.m4a**: extend `demux/mp4.rs` `parse_stsd` with a **`mp4a`** branch → read
  the **`esds`** box → the 2-byte **AudioSpecificConfig** (ASC): `audioObjectType`
  (must be 2 = AAC-LC), `samplingFrequencyIndex`, `channelConfiguration`. `open()`
  already receives `Format::Mp4`; it inspects the sample-entry codec and routes
  `alac`→ALAC, `mp4a`(AAC-LC)→AAC, anything else → `Unsupported`. AAC access units
  in `mdat` **self-delimit** (each `raw_data_block` ends with the `ID_END`
  element + byte-align), so — like ALAC — no `stsz` table is needed to decode
  (faststart only, `moov` before `mdat`).
- **ADTS (.aac)**: new **`Format::Adts`**. `probe` detects the ADTS syncword
  (`byte[0]==0xFF && (byte[1]&0xF0)==0xF0 && (byte[1]&0x06)==0x00`); the
  `layer==00` bits **disambiguate ADTS from MPEG audio/MP3** (whose layer is
  non-zero). A small `demux/adts.rs` parses each 7/9-byte ADTS header
  (`profile`→object type, `sampling_frequency_index`, `channel_configuration`,
  `aac_frame_length`) and yields the frame's `raw_data_block` bytes; config comes
  from the header (no ASC).
- **Shared core**: one `AacDecoder` decodes `raw_data_block`s. A small internal
  **frame-source seam** abstracts "give me the next access-unit bytes + the
  stream config": one impl over the MP4 reader (ASC + `mdat`), one over ADTS
  frames. New `AnyDecoder::Aac(..)` variant; `Decoder` output is interleaved
  `i32`, `SampleFormat::Int { bits: 16 }`; `seek_to` → `Unsupported` (v1);
  `total_frames` from the container where known (MP4 `mdhd`; ADTS = `None`,
  streaming).

## 3. The AAC-LC decoder — focused units (`formats/aac/`)

Each becomes a plan task with its own test. All `no_std`, no-alloc — fixed
by-value buffers (1024-coefficient spectra, per-channel overlap buffers, a
fixed-size FFT). MSB-first bitstream (the ALAC `BitReader` semantics apply; share
or adapt it).

- **Huffman codebooks** (`formats/aac/huffman.rs` + generated tables): the 11
  spectral codebooks + the scalefactor codebook, as checked-in `const` tables
  (an `xtask` generator or transcribed from the ISO tables, with a table-check
  gate like the resampler tables).
- **Syntax** (`formats/aac/syntax.rs`): `raw_data_block` → element tags
  (`SCE=0/CPE=1/CCE=2/LFE=3/DSE=4/PCE=5/FIL=6/END=7`) → `ics_info` (window
  sequence: `ONLY_LONG/LONG_START/EIGHT_SHORT/LONG_STOP`, window shape, max_sfb,
  scalefactor grouping), `section_data` (per-band codebook), `scale_factor_data`
  (Huffman differential), `pulse_data`, `tns_data`, and `spectral_data`
  (Huffman-decoded quantized coefficients + sign/escape).
- **Dequant + scalefactors** (`formats/aac/dequant.rs`): inverse quantization
  (`sign(x)·|x|^(4/3)`, via a lookup for the common range) then per-band
  scalefactor gain (`2^(0.25·(sf-100))`), pulse addition.
- **Stereo tools** (`formats/aac/stereo.rs`): **M/S** (per-band sum/difference)
  and **intensity** stereo, applied to a CPE's two channels.
- **TNS** (`formats/aac/tns.rs`): decode per-window LPC coefficients and apply the
  inverse all-pole filter across spectral coefficients (up to 3 filters/window).
- **Filterbank** (`formats/aac/imdct.rs`): the DSP core — 2048-point (long) and
  256-point (short) **IMDCT** computed as an N/4-point complex FFT with pre/post
  rotation (fixed 512-pt and 64-pt, radix-2/4, no-alloc), then **KBD/sine
  windowing** and **overlap-add** across all four window sequences (including the
  eight-short grouping), with per-channel overlap state carried between frames.
- **Top-level** (`formats/aac/mod.rs`, `AacDecoder`): orchestrates
  syntax→dequant→stereo→TNS→filterbank per element, holds per-channel overlap
  state, and interleaves the two channels to `i32` PCM.

## 4. Testing — AAC is LOSSY (no bit-exact golden)

- **Conformance vs ffmpeg (primary gate):** decode a committed AAC-LC fixture and
  compare against ffmpeg's decode of the same file with an **error tolerance**,
  not equality. Metric: RMS of the sample difference and segmental SNR must be
  within a threshold (two spec-conformant decoders differ only by IMDCT/windowing
  float rounding, so they track closely — target segmental SNR well above ~40 dB;
  the exact threshold is calibrated against the measured value, like the ALAC
  SFDR bar). Applied to both an MP4 and an ADTS fixture.
- **Absolute signal analysis (sanity gate):** an A4-left/E5-right stereo tone
  encoded to AAC-LC → decoded → FFT-assert correct per-channel pitch, level, and
  low distortion (SFDR), reusing the existing `analyze()` helper. Independent of
  ffmpeg; catches gross errors (wrong pitch, garbage, channel swap).
- **Malformed-input fuzz + no-panic invariant:** an AAC fixture seed + the ADTS/
  `mp4a` magics join the standing robustness sweep; every field read from the
  bitstream (element counts, sfb, codebook indices, TNS order, window params) is
  bounds-checked → `Err`, never a panic/OOB — the same firmware invariant ALAC
  upholds.
- **Unit tests** per module: Huffman decode of known codewords, dequant values,
  a known IMDCT vector (impulse/tone in → expected time-domain out), TNS filter,
  M/S and intensity, and ADTS/`esds` header parsing.
- **Playback:** a playable `.m4a`/`.aac` in the workspace `testdata/` + `qa-audio`
  + an e2e render/tone check.

## 5. Scope

**In (Phase 1):** AAC-LC (object type 2); MP4 `mp4a`/`esds` + raw ADTS; mono +
stereo; all four window sequences; TNS; M/S + intensity stereo; decode +
`total_frames`; 16-bit `i32` output; `seek`→`Unsupported`.

**Out:** SBR (Phase 2), PS (Phase 3); seeking; >2-channel and PCE-defined
layouts; the gain-control tool; AAC-LD/LTP/Main/SSR; error resilience.

## 6. Notes / risks

- **IMDCT/FFT** is the hardest DSP unit (like the resampler) — a no-alloc fixed
  FFT of size 512/64 with the IMDCT twiddle rotation; verify against a known
  vector before wiring the full path.
- **Lossy golden**: the tolerance threshold is calibrated from the measured
  ours-vs-ffmpeg delta (documented in the test), not guessed; the tone FFT test
  is the absolute backstop.
- **ADTS/MP3 probe collision** is resolved by the `layer` bits; a probe unit test
  covers both an ADTS header and an MP3 header to lock it in.
- **RAM**: fixed per-channel spectra (1024 f32) + overlap (1024 f32) + FFT
  scratch; ~tens of KB/channel — fine for the DAP; larger than ALAC but bounded.
- **MP4 self-delimiting access units** (§2) is verified first (decode the fixture
  and confirm frame framing + conformance), exactly as ALAC's self-delimiting was;
  the fallback is sizing access units from `stsz` supplied by the MP4 reader.
  ADTS is always explicitly length-delimited (`aac_frame_length`), so it needs no
  such assumption.
