# testdata

Committed test fixtures live here. **Empty in the template** — decoders are
stubs, so there is nothing to decode yet.

When you implement a decoder, add:

- a small input clip, e.g. `sine_stereo_44100.wav` (keep it a few ms so the repo
  and CI stay light), and
- a golden reference, e.g. `sine_stereo_44100.pcm` — the expected decoder output
  as interleaved little-endian `i32`.

Generate the golden output **independently of `soul-audio-core`** (a short
script, a trusted tool, or by construction) so the tests validate the decoder
against an external oracle rather than against itself. Fixtures are treated as
binary — see `.gitattributes`.

The firmware can embed a clip from here via `include_bytes!` to exercise the
core on real hardware.

## DST cross-check vector

`dst-64fs44-2ch.slice.dff` — first frames of the FFmpeg FATE sample
`dst-64fs44-2ch.dff` (DSD64 stereo, DST-compressed). Our decode of the full
file matched FFmpeg's DST decoder bit-exact (raw DSD) and the FFmpeg binary
end-to-end (PCM); this slice is the embedded regression gate
(`dff.rs::dst_real_fate_slice_matches_ffmpeg`).

## IMA-ADPCM vectors

`ima-adpcm-mono.wav` / `ima-adpcm-stereo.wav` — ffmpeg-encoded IMA/DVI-ADPCM
(WAV tag 0x11). Our decode matches ffmpeg's own decode bit-exactly (golden
tests in `formats/wav.rs`).

## FLAC-in-Ogg vector

`flac-in-ogg.ogg` — ffmpeg-encoded FLAC in an Ogg container. `open()` decodes it
bit-exactly vs ffmpeg (golden test in `tests/decode.rs`).
