# Codec Seeking Completion — Design Spec

**Date:** 2026-10-08
**Scope:** Add `SeekableDecoder` support for the three decoders that currently
return `DecodeError::Unsupported` on seek — **ALAC** (MP4), **AAC** (MP4 + raw
ADTS), and **FLAC-in-Ogg** — so every mainstream codec in the crate seeks.
Native FLAC, WAV, MP3, Vorbis, Opus, and DSD already seek; this closes the gap.

## Constraints (inherited, non-negotiable)

- `#![no_std]`, **no-alloc**, `#![forbid(unsafe_code)]`. No heap: seek indices
  must be bounded fixed arrays or **streamed/recomputed on demand**, never an
  unbounded `Vec`. (Mirrors FLAC's fixed `[(u64,u64); 512]` seektable.)
- Seeking binds to `S: SeekSource` (`Source + embedded_io::Seek`), exactly as
  FLAC/Opus/Vorbis do. A non-seekable source must not compile into a seek.
- The gate is `cargo xtask verify` (fmt, clippy `-D warnings`, generated-table
  checks, full test suite, 7× thumbv7em cross-builds, firmware link). Baseline
  is green at `78906e2`.
- No AI trailers in commits. GPG-signed. Commit atomically where a decoder API
  change and its `dispatch.rs` consumer must move together (workspace-wide
  clippy).

## Per-codec approach

### ALAC (MP4) — sample-accurate, no pre-roll

ALAC frames are **fully independent** (each packet carries its own predictor
coefficients; `Scratch` is overwritten per packet). So: map the target
per-channel sample → the MP4 sample (coded frame) that contains it → its
absolute file offset; seek the source there; reset the decoder's running
counters; decode forward and trim the first frame to the exact target. Result:
**sample-accurate**.

### AAC (MP4) — sample-accurate landing, with pre-roll

AAC frames are **not** independent: the filterbank carries a 1024-sample
overlap-add buffer + previous window shape across frames, and HE-AAC SBR / PS
carry QMF delay lines, envelope-smoothing rings, and LPC lookback. So: use the
MP4 sample table to land on a frame at least `PREROLL` frames **before** the
target; reset `Filterbank` + `SbrState` (`SbrState::reset_channels` + clear
header/active/tables); decode-and-discard the pre-roll frames to reconverge the
overlap (LC) and SBR/PS smoothing (HE-AAC); then trim to the exact target
sample. Output position is sample-accurate; the discarded pre-roll makes the
emitted audio correct (verified against the seg-SNR golden, not bit-exact —
AAC is lossy).

### AAC (ADTS) — sample-accurate via decode-and-discard from start

Raw ADTS has **no index and no declared length** (`total_frames() == None`), and
with SBR the output-samples-per-frame is rate-dependent. Rather than a fragile
header-scan that must predict SBR doubling, ADTS seeking rewinds the source to
the start (`SeekSource`), re-initializes decoder state fresh, and
decode-and-discards **output** samples until the target, trimming exactly. This
is O(target) but always correct across LC/SBR/PS, and ADTS is an index-less
streaming format where callers don't expect O(1) seeks. Backward seeks and
seek-to-0 fall out naturally.

### FLAC-in-Ogg — sample-accurate via granule bisection

In FLAC-in-Ogg the Ogg **granule position equals the per-channel sample number**
directly (no Opus pre-skip, no Vorbis overlap), and each Ogg packet is one
**independently-decodable** FLAC frame. The current `OggReader` byte-flattens the
stream and is neither granule-aware nor seekable, which is why seeking is
unsupported. Make the FLAC-in-Ogg path granule-aware and seekable by bisecting
Ogg pages on granule (reusing the proven `OggPackets::seek_to_granule` /
`landed_floor_granule` machinery that Vorbis and Opus use), landing on the page
at/before the target, resetting the FLAC codec's frame/bit state, and
decode-discarding FLAC frames to the exact target. Because FLAC frames are
independent, this is **sample-accurate** (no pre-roll).

## No-alloc MP4 sample-table strategy (the crux)

`Mp4Reader` must not store `stts`/`stsc`/`stsz`/`stco` in `Vec`s. Instead, during
the initial `moov` walk it **records the absolute file offset + byte length** of
each of the four table boxes (a handful of `u64` fields + a `co64: bool`),
tracking absolute position via a running byte counter incremented on every read.
On seek (only when `S: SeekSource`), a `sample_offset(frame_index) ->
(file_offset, samples_into_frame)` helper **re-reads those boxes streamingly** to
compute the target frame's absolute file offset:

- `stsc` (sample-to-chunk, run-length) → which chunk holds the frame + its index
  within the chunk;
- `stco`/`co64` → that chunk's **absolute** file offset;
- `stsz` → sum the byte sizes of the samples preceding the target within the
  chunk (constant-size shortcut when `stsz` sample_size != 0);
- `stts` → map a per-channel sample to a coded-frame index (uniform when all
  frame durations equal `frame_length`, the normal case).

Memory stays O(1); the cost is a few small bounded reads per seek (seeks are
rare). The MP4 module doc already anticipates this ("structured so the sample
tables … for seeking are additive").

## Dispatch + API wiring

- Add `impl<S: SeekSource> SeekableDecoder` (and `pub fn seek_to` /
  `seek_to_time`) to `AlacDecoder`, `AacDecoder`, and the FLAC-in-Ogg path.
- Replace the four `Unsupported` arms in `dispatch.rs` (`AnyDecoder::seek_to`
  `:88-89`, `seek_to_time` `:104-105`) with real forwards for `Alac`/`Aac`;
  `OggFlac` forwards to the FLAC-in-Ogg seek. For `seek_to_time`, AAC uses its
  declared output rate (MP4) / first-header rate (ADTS); ALAC uses the cookie
  rate.
- `seek_to_time` on each decoder = `seek_to(secs_to_samples(secs, rate))`.

## Testing

- **Per-decoder inline seek suites** (mirroring the FLAC/Opus/Vorbis inline
  `#[cfg(test)]` modules): round-trip-vs-from-start equality, seek-to-0
  === fresh decode, backward/repeated seeks, beyond-end clamp, `seek_to_time`.
  ALAC is lossless → **bit-exact** round-trip. AAC is lossy → **segmental-SNR**
  oracle (reuse `best_align_segsnr`/`seg_snr_at` from `tests/decode.rs`).
  FLAC-in-Ogg is lossless → bit-exact round-trip against from-start decode.
- **Fixtures already exist**: `alac.m4a`/`alac24.m4a`, `aac.m4a`/`sbr.m4a`,
  `aac.aac`/`sbr.aac`/`ps-synth.aac`, `flac-in-ogg.ogg`. No new testdata needed.
- **Update the stale assertion**: `tests/decode.rs`
  `ogg_flac_reports_duration_but_rejects_seek` asserts seek returns
  `Unsupported` — rewrite it to assert a correct round-trip once seeking lands.
- MP4 sample-table parsing gets a focused unit test against the `alac.m4a` /
  `aac.m4a` fixtures (known frame counts/sizes).

## Error handling

- Out-of-range target clamps to the stream length (never an error), matching the
  existing seekers' "closest available position" contract. ADTS (no length)
  clamps at EOF discovered during the decode-discard scan.
- Malformed sample tables / truncated boxes → `DecodeError::Unsupported` or the
  existing malformed-MP4 error; never panic. The robustness fuzz suite covers
  the decode path; seek adds its own bounds checks.

## Acceptance

Every `AnyDecoder` variant with a seekable source seeks (no `Unsupported` arms
remain except genuinely non-seekable sources); all new seek suites pass; the
updated Ogg-FLAC test asserts a real round-trip; `cargo xtask verify` is green
including all thumbv7em cross-builds; README seeking matrix + `formats/mod.rs`
docs updated to reflect full coverage.

## Scope notes / YAGNI

- Pre-roll frame counts for AAC are tuned empirically to the seg-SNR bar, not
  over-engineered.
- No new container features (no fragmented-MP4 `moof`/`tfra`, no ADTS resync
  heuristics beyond contiguous frames) — only what the existing fixtures and the
  real decode paths require.
