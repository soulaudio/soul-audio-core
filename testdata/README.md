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
| `tone.mp2`  | MPEG Layer II |
| `tone-layer1.mp1` | MPEG Layer I (conformance vector; ffmpeg has no L1 encoder, so not the shared tone) |
| `tone.ogg`  | FLAC-in-Ogg |
| `tone.m4a`  | ALAC (Apple Lossless) in MP4/M4A |
| `tone-aac.m4a` | AAC-LC in MP4/M4A |
| `tone.aac`  | AAC-LC, raw ADTS elementary stream |
| `tone-sbr.m4a` | HE-AAC (AAC-LC + SBR) in MP4/M4A |
| `tone-vorbis.ogg` | Vorbis I in Ogg |
| `tone.dsf`  | DSD64 (Sony DSF), 0.5 s — 1-bit, decimated to PCM for playback |
| `tone-dsd128.dsf` | DSD128 (Sony DSF), 0.5 s |
| `tone-dsd256.dsf` | DSD256 (Sony DSF), 0.1 s — decimation-rate e2e golden |
| `tone-dsd512.dsf` | DSD512 (Sony DSF), 0.1 s — decimation-rate e2e golden |
| `tone.dff`  | DSD64 (Philips DSDIFF, raw) — MSB-first/interleaved DFF + `DIIN` tags |
| `tone-dst.dff` | DST-compressed DSDIFF (FFmpeg FATE slice) — lossless-DSD decode e2e |
| `tagged.flac` | native FLAC with ID3-style tags (title/artist/album) — used by the library-scan `Some(tag)` test |

DSD is 1-bit at 2.8 MHz, so `tone.dsf` is deliberately shorter (0.5 s) to keep
it small. The CLI decimates it to multi-bit PCM (DSD64 → 352.8 kHz) and the deck
resamples that to the device rate, so it plays on any DAC. Decode is rate-agnostic
(fixed 8:1 decimation), so `tone-dsd256/512.dsf` only need to be long enough for
the decimation-rate goldens (0.1 s). `tone.dff` / `tone-dst.dff` cover the Philips
DSDIFF container (raw and DST-compressed).

### DSD real-time headroom (release)

Measured decode-vs-real-time ratio per rate (`cargo test -p soul-audio-cli
--release --test dsd_realtime -- --ignored --nocapture`; lower = more headroom):

| Rate | decode / real-time |
|------|--------------------|
| DSD64  | ~0.009× |
| DSD128 | ~0.018× |
| DSD256 | ~0.035× |
| DSD512 | ~0.068× |

Cost scales ~2× per rate doubling (byte rate doubles). Even DSD512 sustains
real-time with ~15× headroom in the **release** profile. The dev/debug profile is
roughly an order of magnitude slower, so prefer `--release` (or the audio-crate
dev opt profile) for DSD256+ playback. Numbers are machine-dependent — the test
is `#[ignore]`d and prints rather than asserts.

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

Native FLAC is additionally gated in the **core** decode suite
(`crates/soul-audio-core/tests/decode.rs::open_decodes_native_flac_bit_exact_vs_wav`)
as a bit-exact match against `tone.wav` (the lossless source). Vorbis and the
raw-DSD DSDIFF (`tone.dff`) are covered end-to-end in the desktop Tier-1 suite
(`app/src-tauri/tests/`). HE-AAC/SBR (`tone-sbr.m4a`) renders pitch-only through
the CLI e2e, like the other lossy AAC fixtures.

## Regenerating

```sh
bash testdata/generate.sh   # needs ffmpeg (libmp3lame + flac) and python3 + numpy
```

The PCM fixtures are produced by ffmpeg (independent of `soul-audio-core`), so
decoding them validates our from-scratch decoders against an external encoder.
The DSD fixtures are synthesized by `generate_dsd.py` — ffmpeg cannot *encode*
PCM→DSD, so a 2nd-order sigma-delta modulator turns the tone into a 1-bit stream
wrapped in a Sony DSF (`.dsf`, LSB-first/planar) or Philips DSDIFF (`.dff`,
MSB-first/interleaved) container (ffmpeg *can* decode both, for cross-checking):
`generate_dsd.py <out> [rate_hz] [dur_secs]` picks the container by extension.
`tone-dst.dff` is a copy of the committed DST FATE slice (DST has no open
encoder).

> Minimal decode-correctness **vectors** (DSD/DST slice, IMA-ADPCM, MPEG
> Layer I/II, VBR MP3, FLAC-in-Ogg, and a synthetic HE-AACv2/Parametric-Stereo
> `ps-synth.aac` + its ffmpeg reference) live in
> `crates/soul-audio-core/testdata/`, next to the codec tests that embed them.
> `ps-synth.aac` is our own tone encoded to HE-AACv2 by a from-source `fdk-aac`
> build (AOT 29, mono core + PS) — committable, unlike the gitignored,
> license-restricted `ps-local.*` Coding-Technologies conformance fixture.
