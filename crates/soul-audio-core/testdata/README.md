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

## AAC-LC

- `aac.m4a` / `aac.m4a.ref.pcm` — an ffmpeg-encoded AAC-LC stream in an
  MP4/M4A container (`esds`/`mp4a`), plus ffmpeg's own decoded PCM reference.
- `aac.aac` / `aac.aac.ref.pcm` — the same encode, framed as a raw ADTS
  elementary stream instead, plus its own ffmpeg PCM reference.

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

## MPEG audio (Layers I / II / III)

- `layer1-32k-stereo.mp1` + `layer1-32k-stereo.ffref16.pcm` — Layer I and its
  ffmpeg 16-bit PCM reference.
- `l2_*.mp2` — Layer II at assorted rates/modes/bitrates.
- `vbr-44100-stereo.mp3` — a VBR MP3 (Xing header) for Layer III + VBR seeking.
