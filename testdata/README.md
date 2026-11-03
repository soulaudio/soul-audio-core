# testdata — playback fixtures

Full, playable clips for **by-ear** and **render** QA of the whole pipeline
(decode → resample → normalize → output), one per supported PCM container.

Every file carries the **same 2-second stereo signal**: left = 440 Hz (A4),
right = 659.25 Hz (E5), −6 dB, with 50 ms in/out fades. So:

- each format should sound *identical* (decode correctness),
- the sample-exact formats (WAV/FLAC) played back-to-back are one continuous
  chord (gapless), and
- 44.1 kHz on a 48 kHz device exercises the resampler.

**Broadband stress fixtures** (`tests/decode.rs` `stress_*`): the 2-tone
`tone.wav` is near-silent in most bands and compresses trivially, so it never
exercises the hard codec paths. These demanding signals do.

| File | Format |
|------|--------|
| `complex.wav` | 3 s stereo stress reference — L a 50 Hz→18 kHz chirp (sweeps every band), R pink noise (high-entropy, L≠R). The lossless/MP3/Vorbis/Opus stress tests decode against this |
| `complex.flac` / `complex-alac.m4a` / `complex.wv` | lossless of `complex.wav` — must decode **bit-exact** (any deviation = a real decoder bug the tone hides) |
| `complex.mp3` / `complex-vorbis.ogg` / `complex.opus` (+ `.ref.pcm`) | lossy of `complex.wav` — SNR vs ffmpeg on broadband content |
| `chirp.wav` / `chirp-aac.m4a` (+ `.ref.pcm` float, `.fixed.pcm` fixed-point) | L=R full-band chirp → forces AAC EIGHT_SHORT transient windows without triggering PNS; cross-checked vs BOTH ffmpeg AAC decoders (independent float `aac` + fixed-point `aac_fixed`) |
| `fullscale.wav` + `fullscale.flac` / `.wv` / `-alac.m4a` | 0 dBFS independent-L/R white noise → max residual magnitude; lossless must decode **bit-exact** (guards integer predictor/entropy overflow at the range edge) |
| `antiphase.wav` + `antiphase.flac` / `.wv` | R = −L full-scale white noise (mid=0, side=max) → the FLAC mid-side / WavPack joint-stereo decorrelation extreme; bit-exact |
| `mp3-mpeg2-22k.mp3` / `mp3-mpeg25-11k.mp3` / `mp3-mono.mp3` (+ `.ref.pcm`) | MP3 MPEG-2 LSF (22 kHz) / MPEG-2.5 (11 kHz) / mono — the halved-granule + LSF-scalefactor + mono paths no 44.1 kHz MPEG-1 stereo fixture reaches; SNR vs ffmpeg |
| `complex24.wav` + `complex24.flac` / `complex24-alac.m4a` | 24-bit (hi-res) broadband → FLAC + ALAC wide-sample paths; bit-exact |
| `chirp-aac-48k.m4a` (+ `.ref.pcm`) | AAC-LC at 48 kHz — a different scalefactor-band table than the 44.1 kHz fixtures; SNR vs ffmpeg |

| File | Format |
|------|--------|
| `tone.wav`  | RIFF/WAVE PCM (the reference) |
| `tone-f32.wav` | IEEE-float WAV — re-encodes `tone.wav`; full-scale float→i32 decode reconstructs the 16-bit signal (±2). Shared float golden for CAF/AIFF float |
| `tone-aiff.aiff` | AIFF (big-endian PCM) — lossless, decodes bit-exact vs `tone.wav` |
| `tone-aifc-sowt.aifc` | AIFF-C `sowt` (little-endian PCM) — lossless, bit-exact vs `tone.wav` |
| `tone-aiff-fl32.aifc` | AIFF-C `fl32` (big-endian IEEE float) — bit-identical to `tone-f32.wav` |
| `tone-aiff-fl64.aifc` | AIFF-C `fl64` (big-endian 64-bit float) — bit-identical to `tone-f32.wav` |
| `tone-aiff-ulaw.aifc` | AIFF-C `ulaw` (G.711 µ-law) — bit-exact vs `tone-aiff-ulaw.ref.pcm` (ffmpeg s16le) |
| `tone-aiff-alaw.aifc` | AIFF-C `alaw` (G.711 A-law) — bit-exact vs `tone-aiff-alaw.ref.pcm` (ffmpeg s16le) |
| `tone.flac` | native FLAC |
| `tone-flac.mka` | FLAC in Matroska (`-c:a copy` of `tone.flac`) — bit-exact vs native FLAC |
| `tone-opus.webm` | Opus in WebM/Matroska (`-c:a copy` of `tone.opus`) — SNR vs `tone-opus.webm.ref.pcm` |
| `tone-aac.mka` | AAC-LC in Matroska (`-c:a copy` of `tone-aac.m4a`) — SNR vs `tone-aac.mka.ref.pcm` |
| `tone-vorbis.mka` | Vorbis in Matroska (`-c:a copy` of `tone-vorbis.ogg`) — bit-exact prefix vs the Ogg decode |
| `tone.wv` | WavPack lossless, stereo 16-bit (joint + cross decorrelation) — bit-exact vs `tone.wav` |
| `tone-mono.wv` | WavPack lossless, mono — bit-exact vs `tone-mono.wv.ref.pcm` |
| `tone-24.wv` | WavPack lossless, 32-bit `INT32_DATA` from a 16-bit source (`zeros=16`, 16-bit magnitude) — bit-exact vs `tone-24.wv.ref.pcm` (s32le) |
| `tone-wv24.wv` | WavPack lossless, **genuine 24-bit magnitude** (`zeros=8`, peak ≈ 2^30) — exercises the large-residual entropy path; bit-exact vs `tone-wv24.wv.ref.pcm` (s32le) |
| `tone.caf` | Apple CAF `lpcm` (big-endian PCM) — bit-exact vs `tone.wav` |
| `tone-caf-le.caf` | Apple CAF `lpcm` little-endian — bit-exact vs `tone.wav` (byte-order flag branch) |
| `tone-caf-f32.caf` | Apple CAF `lpcm` IEEE-float — bit-identical to `tone-f32.wav` (float flag branch) |
| `tone-alac.caf` | Apple CAF `alac` (Apple Lossless) — bit-exact vs `tone.wav` |
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
| `dst-slice.dff` | DST (arithmetic-coded lossless DSD) — the FFmpeg FATE slice (real music, not a tone). Full-pipeline regression-locked; DST *correctness* is in `formats/dst.rs`'s arithmetic encode↔decode unit tests (no DST encoder / ffmpeg can't decode it → no external conformance ref) |
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

## Cross-format metadata (`meta.*`)

The same 1.5 s tone muxed into every container the tag reader supports, each
carrying identical `TITLE=Aria` / `ARTIST=Kimiko Ishizaka` / `ALBUM=Open
Goldberg Variations` (self-contained lavfi source in `generate.sh`).

| File | What it exercises |
|------|-------------------|
| `meta.wav` | RIFF `LIST`/`INFO` tags (how ffmpeg tags WAV) |
| `meta.flac` / `meta.ogg` / `meta.opus` | FLAC / Vorbis / Opus comment headers |
| `meta.mp3` | ID3v2 |
| `meta-aac.m4a` / `meta-alac.m4a` | MP4 `ilst`, with **`moov` after `mdat`** (ffmpeg default — no `+faststart`) |
| `meta-oggflac.oga` | FLAC-in-Ogg — STREAMINFO `total_samples = 0`, duration recovered from the Ogg granule |
| `meta.aiff` | AIFF `NAME` chunk (title only; ffmpeg writes no artist/album for AIFF) |
| `meta.aac` | raw ADTS — no tag structure, facts only |

`crates/soul-audio-tags/tests/cross_format_metadata.rs` reads each via
`read_owned` and asserts consistent title/artist/album and a ~1.5 s duration.
`crates/soul-audio-core/tests/decode.rs` additionally decodes the two
moov-after-mdat `.m4a`s through `open_seekable` (ALAC bit-exact vs `meta.wav`) —
the regression gate for the player's "AAC/ALAC doesn't start playing". Both are
`std`-gated e2e tests (the tags one needs `--features std`), outside the default
`cargo xtask verify` set.

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
`dst-slice.dff` is the committed DST FATE slice (`crates/soul-audio-core/testdata/
dst-64fs44-2ch.slice.dff`), copied here for the CLI e2e. DST has no open encoder,
so there is no tone/round-trip fixture for it.

> Minimal decode-correctness **vectors** (DSD/DST slice, IMA-ADPCM, MPEG
> Layer I/II, VBR MP3, FLAC-in-Ogg, and a synthetic HE-AACv2/Parametric-Stereo
> `ps-synth.aac` + its ffmpeg reference) live in
> `crates/soul-audio-core/testdata/`, next to the codec tests that embed them.
> `ps-synth.aac` is our own tone encoded to HE-AACv2 by a from-source `fdk-aac`
> build (AOT 29, mono core + PS) — committable, unlike the gitignored,
> license-restricted `ps-local.*` Coding-Technologies conformance fixture.

## ISO MPEG audio conformance (local-only)

`tests/mp3_conformance.rs` validates the MPEG audio decoder against the
authoritative **ISO/IEC 11172-4 / 13818-4 conformance vectors** (`.bit` +
reference `.pcm`) — a decoder-independent check beyond the ffmpeg SNR
cross-checks. The vectors are ISO-copyright, so (like `ps-local.*`) they are
**not committed**: they live in a gitignored `testdata/mp3-conformance-local/`
and the test skips when absent. Enable with `bash testdata/fetch-mp3-conformance.sh`.
Measured: **Layer III (MPEG-1 & MPEG-2) is ISO full-accuracy** (max |Δ| ≤ 1 LSB,
113–117 dB PSNR); Layer I/II track the ISO reference only to ~42–64 dB, the
by-design limited accuracy of the kjmp2 decoder our Layer I/II is a bit-exact
port of.

## Real-music fixtures (CC0)

`realmusic.wav` is a 4-second, CD-quality (44.1 kHz/16-bit stereo) excerpt of the
**Open Goldberg Variations** — Kimiko Ishizaka playing J.S. Bach's Goldberg
Variations BWV 988 (the Aria), released **CC0 / public domain** by the Open
Goldberg project (opengoldbergvariations.org; source: archive.org
`OpenGoldbergVariations`). CC0 means it is freely committable. It is the one test
signal with genuine musical structure — note onsets (transients → short blocks),
harmonic partials, pedal/decay tails, real stereo imaging and dynamics — that the
synthetic tone/chirp/noise fixtures cannot provide. The codec variants
(`realmusic.flac` / `-alac.m4a` / `.wv` / `.mp3` / `-aac.m4a` / `-vorbis.ogg` /
`.opus`, + lossy `.ref.pcm`) are derived from it: lossless decode bit-exact, lossy
SNR vs ffmpeg (`tests/decode.rs` `realmusic_*`). Native-DSD real music is not
committed — no CC0 native-DSD recording exists (2L/NativeDSD/Blue Coast are
commercial); the DSD decode path is covered by the synthetic `tone.dsf`/`.dff`
and the `dst-slice.dff` conformance slice.
