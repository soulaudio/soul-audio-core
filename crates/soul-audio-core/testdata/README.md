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

## MPEG audio (Layers I / II / III)

- `layer1-32k-stereo.mp1` + `layer1-32k-stereo.ffref16.pcm` — Layer I and its
  ffmpeg 16-bit PCM reference.
- `l2_*.mp2` — Layer II at assorted rates/modes/bitrates.
- `vbr-44100-stereo.mp3` — a VBR MP3 (Xing header) for Layer III + VBR seeking.
