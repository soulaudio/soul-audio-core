# soul-audio-core testdata — decoder golden vectors

Small, committed inputs (and some golden PCM references) that the codec unit
tests decode via `include_bytes!`. Each was produced **independently of
`soul-audio-core`** (ffmpeg, or by construction) so the tests validate our
decoders against an external oracle, not against themselves. All are treated as
binary (see the repo `.gitattributes`).

Full playable clips for by-ear/render QA live in the workspace-root `testdata/`,
not here — these are minimal decode-correctness vectors.

## DSD / DST

- `dst-64fs44-2ch.slice.dff` — first frames of the FFmpeg FATE sample
  `dst-64fs44-2ch.dff` (DSD64 stereo, DST-compressed). Our decode of the full
  file matched FFmpeg's DST decoder bit-exact (raw DSD) and end-to-end (PCM);
  this slice is the embedded regression gate
  (`formats/dff.rs::dst_real_fate_slice_matches_ffmpeg`).

## WAV (IMA-ADPCM)

- `ima-adpcm-mono.wav` / `ima-adpcm-stereo.wav` — ffmpeg-encoded IMA/DVI-ADPCM
  (WAV tag `0x11`). Our decode matches ffmpeg's own decode bit-exactly (golden
  tests in `formats/wav.rs`).

## FLAC-in-Ogg

- `flac-in-ogg.ogg` — ffmpeg-encoded FLAC in an Ogg container. `open()` decodes
  it bit-exactly vs ffmpeg (golden test in `tests/decode.rs`).

## ALAC

- `alac.m4a` / `alac.ref.pcm` — an ffmpeg-encoded ALAC (Apple Lossless) stream
  in an MP4/M4A container, plus ffmpeg's own decoded PCM reference. Our ALAC
  decoder (via `open()`'s MP4 `moov`/`mdat` parsing) matches ffmpeg's decode
  bit-exactly (golden test in the ALAC decoder/format tests).
- `alac-nonfaststart.m4a` — a stream-copy remux of `alac.m4a`
  (`ffmpeg -i alac.m4a -c copy`, ffmpeg's default muxer layout) with the box
  order **non-faststart**: `ftyp, free, mdat, moov` (`moov` last). The audio is
  byte-identical to `alac.m4a`, so it decodes bit-exact to `alac.ref.pcm` — but
  only via `open_seekable()` (the forward-only `open()` returns `Unsupported`
  because `moov` follows `mdat`). Golden in `tests/decode.rs`.

## AAC-LC

- `aac.m4a` / `aac.m4a.ref.pcm` — an ffmpeg-encoded AAC-LC stream in an
  MP4/M4A container (`esds`/`mp4a`), plus ffmpeg's own decoded PCM reference.
- `aac.aac` / `aac.aac.ref.pcm` — the same encode, framed as a raw ADTS
  elementary stream instead, plus its own ffmpeg PCM reference.
- `aac-id3.aac` — `aac.aac` with a 42-byte ID3v2.3 tag prepended (by
  construction, not ffmpeg; a `TIT2`="Aria" frame + padding). Taggers add ID3 to
  raw `.aac` streams too, so the probe must skip the tag and classify by the
  first real ADTS frame sync — routing it to the AAC decoder, not MP3. Audio is
  byte-identical to `aac.aac` (reuses `aac.aac.ref.pcm` / compares equal to
  `decode_all(aac.aac)`; no new reference). Golden in `tests/decode.rs`.
- `aac-nonfaststart.m4a` — a stream-copy remux of `aac.m4a`
  (`ffmpeg -i aac.m4a -c copy`) with the **non-faststart** box order
  (`ftyp, free, mdat, moov`). Same audio as `aac.m4a`, so it is SNR-compared to
  `aac.m4a.ref.pcm`, and decodes only via `open_seekable()` (forward-only
  `open()` is `Unsupported`). Golden in `tests/decode.rs`.

AAC-LC is lossy, so unlike the ALAC/FLAC goldens above, our decode is checked
against ffmpeg's own decode by **segmental SNR with a tolerance**, not
bit-exact equality — ~55 dB vs ffmpeg (golden tests in the AAC decoder/format
tests). Frame 0 of both streams uses PNS (perceptual noise substitution) on
some scalefactor bands; this decoder renders those bands silent rather than
synthesizing noise, so the golden's alignment/tolerance accounts for that first
frame specifically (later frames don't use PNS in this encode). The workspace-
root `tone-aac.m4a` / `tone.aac` playback fixtures (same signal, same lossy
codec) exercise the pitch-only render path end to end.

## AAC SBR (HE-AAC)

- `sbr.m4a` / `sbr.m4a.ref.pcm` — a **HE-AAC (AAC-LC + SBR)** stream in an
  MP4/M4A container, plus ffmpeg's own decoded PCM reference.
- `sbr.aac` / `sbr.aac.ref.pcm` — the same content framed as raw ADTS, plus its
  reference.

**Self-encoded** (unlike a downloaded conformance vector) via the Windows
MediaFoundation encoder, which — unlike ffmpeg's native `aac` encoder — emits
SBR: `ffmpeg -i <src>.wav -c:a aac_mf -b:a 24k sbr.m4a` (and `-f adts` →
`sbr.aac`). The source is a **broadband** 200 Hz→16 kHz log chirp + shaped noise
(seed 1), deliberately *not* the low-frequency LC tone, so there is real energy
above the SBR crossover for SBR's high-band reconstruction to act on.

The two fixtures cover **both SBR signalling paths**: `sbr.m4a`'s `esds`
advertises `profile=HE-AAC` (explicit, AudioObjectType 5), while `sbr.aac`'s
ADTS header carries an AAC-LC profile at the **22050 Hz** core rate
(`sampling_frequency_index=7`) and SBR is discovered **implicitly** from the
fill element at decode time — both decode to **44100 Hz** output (2× the core),
the structural signature of active SBR. HE-AAC is lossy (SBR reconstruction is
LPC-predicted, not exact), so the golden is a **segmental-SNR tolerance** vs
ffmpeg's own decode (calibrated from the measured value), backed by a structural
sanity gate (2× output length, non-silent, finite).

## AAC Parametric Stereo (HE-AACv2, PS) — local-only fixture

- `ps-local.m4a` / `ps-local.ref.pcm` — a **HE-AACv2 (AAC-LC + SBR + Parametric
  Stereo)** conformance clip and ffmpeg's own s16le stereo decode of it.

Unlike every other fixture in this file, these are **license-restricted**
(sourced from the Coding Technologies aacPlus DecoderCheck suite) and
**gitignored — never committed** (see the root `.gitignore`,
`/crates/soul-audio-core/testdata/ps-local.*`). They exist only in local dev
checkouts and are used purely for local, human-triggered verification: the
`tests/decode.rs` PS conformance tests (`ps_local_matches_ffmpeg_per_channel`,
per-channel segmental SNR vs ffmpeg; `ps_local_output_is_stereo_and_channels_differ`,
a structural stereo gate) read them at runtime via `std::fs::read` and skip
(no-op pass) when the files are absent — the case for CI and every clean
checkout. There is no local encoder that produces HE-AACv2/PS (unlike the
self-encoded SBR fixtures above, MediaFoundation's `aac_mf` does not emit PS),
so this fixture cannot be regenerated or replaced and must never be sourced
into the repo.

**Measured conformance** (local, at the shared priming offset of 384
per-channel frames): per-channel segmental SNR **L = 33.55 dB, R = 34.53 dB**
vs the ffmpeg reference — high fidelity, because SBR is already near-bit-exact
here and PS layers table-driven f32 DSP (hybrid filterbank → decorrelation →
2×2 complex mixing) on top. `ps_local_matches_ffmpeg_per_channel` de-interleaves
L/R and scores **each channel independently** at that shared offset, requiring
**both** to clear `PS_SNR_BAR = 28 dB` (a ~5.5 dB margin below the lower channel,
the same calibrate-from-measured discipline as the AAC-LC 55.3→45 dB and SBR
~104→90 dB gates). The structural gate observes stereo width
`RMS(L−R)/RMS(L) ≈ 0.80` (ffmpeg ref ≈ 0.808), which a mono-duplicated /
PS-silently-disabled regression would collapse to ~0.

**Committed, CI-enforced PS coverage** instead comes from the per-unit
synthetic-input DSP tests — the hybrid filterbank, decorrelation, and the Type
A/Type B mixing matrices + IPD/OPD phase rotation (incl. the NEGATE_IPD_MASK
sign path), all checked against faad2/FFmpeg reference closed forms — plus the
robustness sweep (`tests/robustness.rs`): its bit-flips/truncations over the
committed `sbr.*` fixtures' SBR `bs_extension` region drive the PS
`parse_extensions`/`parse_frame` `[NOPANIC]` fields to `Err`/disable-PS, never
a panic/OOB/hang. The local `ps-local.m4a`, when present, is additionally seeded
into that sweep as a genuine valid-baseline PS input (never committed —
`std::fs::read`, never `include_bytes!`).

## MPEG audio (Layers I / II / III)

- `layer1-32k-stereo.mp1` + `layer1-32k-stereo.ffref16.pcm` — Layer I and its
  ffmpeg 16-bit PCM reference.
- `l2_*.mp2` — Layer II at assorted rates/modes/bitrates.
- `vbr-44100-stereo.mp3` — a VBR MP3 (Xing header) for Layer III + VBR seeking.

## Opus (adaptive mode switching)

- `mode-switch-mono.opus` + `mode-switch-mono.ref.pcm` — a mono Ogg-Opus stream
  that adaptively switches decode mode mid-stream, plus **libopus's** own s16le
  decode of it. It is the golden for issue #37 (routing the Ogg `open()` decode
  through the CELT redundancy / mode-transition logic).

  **Self-encoded** (like the SBR fixtures, not a downloaded vector): a 3 s source
  of alternating low tones (SILK-friendly) and mid-range triads with a light
  high-frequency component (music-like → CELT/Hybrid) is encoded
  `ffmpeg -i src.wav -c:a libopus -b:a 24k -application audio -frame_duration 20`.
  libopus's adaptive encoder then spans **all three Opus modes** with five
  mid-stream transitions (Hybrid→SILK→CELT→SILK→Hybrid→CELT, verified in the
  golden by parsing every packet's TOC byte). The reference is the **libopus**
  decode `ffmpeg -c:a libopus -i mode-switch-mono.opus -f s16le` — the reference
  decoder itself, not ffmpeg's *native* Opus decoder, so it uses libopus's own
  SILK resampler (the one this crate transcribes) and the golden's achievable SNR
  is the CELT float-approximation ceiling across every mode, making it a tight
  gate on the transition frames specifically (the pre-#37 Ogg path scored 19 dB
  here; the unified core scores 41 dB — bar 30 dB). Opus is lossy, so this is a
  segmental-SNR-with-tolerance golden, not a bit-exact one; the definitive
  bit-exact validation is the committed RFC 8251 vectors (`opus_tv*.bit`,
  `tests/opus_conformance.rs`).
