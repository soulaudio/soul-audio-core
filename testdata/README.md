# testdata — playback fixtures

Full, playable clips for **by-ear** and **render** QA of the whole pipeline
(decode → resample → normalize → output), one per supported PCM container.

Every file carries the **same 2-second stereo signal**: left = 440 Hz (A4),
right = 659.25 Hz (E5), −6 dB, with 50 ms in/out fades. So:

- each format should sound *identical* (decode correctness),
- the sample-exact formats (WAV/FLAC) played back-to-back are one continuous
  chord (gapless), and
- 44.1 kHz on a 48 kHz device exercises the resampler.

| File | Format |
|------|--------|
| `tone.wav`  | RIFF/WAVE PCM (the reference) |
| `tone.flac` | native FLAC |
| `tone.mp3`  | MPEG Layer III |
| `tone.ogg`  | FLAC-in-Ogg |
| `tone.m4a`  | ALAC (Apple Lossless) in MP4/M4A |
| `tone-aac.m4a` | AAC-LC in MP4/M4A |
| `tone.aac`  | AAC-LC, raw ADTS elementary stream |
| `tone.dsf`  | DSD64 (Sony DSF), 0.5 s — 1-bit, decimated to PCM for playback |
| `tagged.flac` | native FLAC with ID3-style tags (title/artist/album) — used by the library-scan `Some(tag)` test |

DSD is 1-bit at 2.8 MHz, so `tone.dsf` is deliberately shorter (0.5 s) to keep
it small. The CLI decimates it to multi-bit PCM (DSD64 → 352.8 kHz) and the deck
resamples that to the device rate, so it plays on any DAC.

`tone-aac.m4a` is deliberately **not** named `tone.m4a` — that name is already
the lossless ALAC fixture above (same `.m4a` container, different codec), and
AAC-LC is lossy, so it can't share the "each format is identical/bit-exact"
story the rest of the table tells. It should still sound like the same chord
by ear; only the automated checks are pitch-only, not bit-exact, for this pair
(see below).

## By-ear QA (one command)

```sh
cargo xtask qa-audio
```

Plays each fixture on the default output device with what to listen for, then a
gapless WAV→FLAC queue. This is the one check tests can't make — that it actually
*sounds* right through a real device.

## Automated checks

`crates/soul-audio-cli/tests/e2e.rs` decodes these through the CLI and asserts
FLAC, Ogg-FLAC, and ALAC are **bit-exact vs `tone.wav`** (lossless), MP3 decodes
to plausible audio, and a real FLAC renders through the full pipeline. The
signal-analysis tests there additionally verify pitch/level/cleanliness and
channel routing on rendered output (including a rendered ALAC pass). AAC-LC
(`tone-aac.m4a` and `tone.aac`) is lossy, so its render tests are **pitch-only**
— FFT-assert left ≈ 440 Hz / right ≈ 659.25 Hz on a steady-state window past
the encoder's priming delay — not bit-exact equality or a tight SFDR bar.

## Regenerating

```sh
bash testdata/generate.sh   # needs ffmpeg (libmp3lame + flac) and python3 + numpy
```

The PCM fixtures are produced by ffmpeg (independent of `soul-audio-core`), so
decoding them validates our from-scratch decoders against an external encoder.
`tone.dsf` is synthesized by `generate_dsd.py` — ffmpeg cannot *encode* PCM→DSD,
so a 2nd-order sigma-delta modulator turns the tone into a 1-bit DSD64 stream
wrapped in a Sony DSF container (ffmpeg *can* decode it, for cross-checking).

> Minimal decode-correctness **vectors** (DSD/DST slice, IMA-ADPCM, MPEG
> Layer I/II, VBR MP3, FLAC-in-Ogg) live in
> `crates/soul-audio-core/testdata/`, next to the codec tests that embed them.
