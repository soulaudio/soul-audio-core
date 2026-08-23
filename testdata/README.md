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
