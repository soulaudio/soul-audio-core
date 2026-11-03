# ALAC (Apple Lossless) + MP4 container — decoder design

**Date:** 2026-08-26
**Status:** Approved (design review)
**Depends on:** the decode seam (`Decoder`, `AnyDecoder`, `probe`/`Format`), the
`demux/` layer, and the ffmpeg golden-vector test pattern in
`crates/soul-audio-core/testdata/`.

## 1. Goal

Add a from-scratch, `no_std`, no-alloc **ALAC** (Apple Lossless) decoder to
`soul-audio-core`, plus a **reusable MP4/M4A container reader** it rides on.
ALAC completes the lossless story for the DAP, and the MP4 demuxer is the
groundwork that lets AAC-in-`.m4a` drop in later with no new container work.

## 2. Why ALAC needs a container (and the v1 constraint)

ALAC has **no raw/self-describing stream form**: the codec config (the "magic
cookie" — frame length, bit depth, channels, sample rate) lives only inside an
MP4 box (`stsd/alac`). So MP4 parsing is mandatory.

MP4 separates the **index** (`moov`: sample tables) from the **audio** (`mdat`).
To keep decode a forward `BufSource` stream (like every other codec) and
no-alloc, **v1 requires a faststart file** (`moov` before `mdat`) and relies on
ALAC packets being **self-delimiting** (see §4), so no per-frame size table is
needed to decode. Non-faststart files (or seeking) need random access and are
deferred (§6).

## 3. Seam integration

- **`probe`** (`probe.rs`): detect `ftyp` at bytes 4..8 → new `Format::Mp4`.
- **`model::Format`**: add `Mp4`.
- **`open::<S: BufSource>`** (`dispatch.rs`): for `Format::Mp4`, construct the MP4
  reader, read `stsd`; if the codec is `alac` → `AnyDecoder::Alac(AlacDecoder)`;
  any other codec (e.g. `mp4a`) → `DecodeError::Unsupported`. No new public API —
  ALAC rides the existing `open()` path.
- **`AnyDecoder<S>`**: add `Alac(AlacDecoder<S>)`; delegate `stream_info` /
  `decode_into` like the other variants.
- **Seeking**: `AnyDecoder`'s `seek_to` returns `DecodeError::Unsupported` for the
  ALAC variant in v1 — exactly as the streaming Ogg-FLAC variant does today.
  `total_frames` returns the per-channel duration from the container.

## 4. ALAC decoder — `formats/alac.rs`

A streaming decoder over the MP4 reader's `mdat` bitstream. Per **packet**:

- Read 3-bit **element tags** in a loop: channel elements (mono `SCE`-style /
  stereo `CPE`-style pairs) carry the audio; the **`END` tag (0x7)** terminates
  the packet, after which the reader **byte-aligns**. This makes each packet
  self-delimiting, so packets decode back-to-back straight out of `mdat` — **no
  `stsz` size table needed for decode.** Decode stops when `mdat` (or the
  duration) is consumed.
- Per channel within a packet: an **adaptive FIR predictor** (sign-sign LMS
  coefficient update, warm-up leader samples) over **adaptive Rice/Golomb**
  residuals (history-driven `k`), then the **uncompressed-frame fallback** (the
  encoder emits verbatim samples for incompressible frames — a flag in the
  packet).
- **Inter-channel decorrelation** for stereo: undo the `interlacing shift` +
  `interlacing weight` mixing to recover L/R.
- **Bit depths** 16 and 24 (20/32 handled if trivial); output interleaved `i32`
  with `SampleFormat::Int { bits }` from the cookie — native-scale samples, like
  WAV/FLAC. The deck normalizes downstream.
- **Channels**: mono and stereo in v1; >2 channels (ALAC's channel-layout
  element ordering) deferred.

**Buffers (no-alloc):** a fixed packet-decode buffer sized by the cookie's
`maxFrameBytes` bound and `frameLength × MAX_CH`, embedded by value like FLAC's
inline block buffer. No heap.

**Verification-first risk:** the self-delimiting property (§4, first bullet) is
the one assumption to confirm. The **first implementation task decodes the
golden fixture and checks packet framing + bit-exactness**; if packets do *not*
self-delimit cleanly, the fallback is to size packets from `stsz` supplied by the
demuxer (which would bind ALAC to `SeekSource` — a scoped change, not a redesign).

## 5. MP4 container — `demux/mp4.rs`

A generic box-tree reader (the reusable piece):

- Walk boxes by `(size, fourcc)` headers, descending
  `moov→trak→mdia→minf→stbl→stsd`. Handle 32-bit and 64-bit (`size==1`) box
  sizes; ignore unknown boxes by skipping `size` bytes.
- Extract: the `stsd` **sample entry** (`alac` fourcc → its magic cookie;
  `mp4a`/other → report the codec so `open` can decline), `mdat` (offset + size),
  and duration/timescale (`mdhd`).
- Structured so `stsz`/`stco`/`stsc`/`stts` parsing (for seeking) and the `mp4a`
  entry (for AAC) are additive — no traversal rewrite.
- v1 assumes **faststart** (`moov` seen before `mdat` while reading forward);
  if `mdat` is reached before the cookie, error `Unsupported`.

## 6. Scope

**In:** `demux/mp4.rs` (box tree → cookie + `mdat` + duration, faststart);
`formats/alac.rs` (ALAC mono/stereo, 16/24-bit, adaptive predictor + Rice +
inter-channel + uncompressed fallback); seam wiring (`Format::Mp4`,
`AnyDecoder::Alac`, `open` routing); `total_frames`; golden + unit tests; a
playable `.m4a` in the by-ear `testdata/` + `qa-audio`.

**Out (deferred):** seeking (needs the sample tables + `SeekSource`);
non-faststart files; >2 channels; AAC (`mp4a`) decode; `.caf`/raw ALAC.

## 7. Testing

- **Golden (bit-exact):** `crates/soul-audio-core/testdata/alac.m4a` from
  `ffmpeg -i tone.wav -c:a alac -movflags +faststart alac.m4a`. ALAC is lossless,
  so `open()`-decode of it must equal the reference PCM bit-for-bit (a `decode.rs`
  golden test, mirroring `flac-in-ogg`).
- **Unit:** box-tree parsing (nested boxes, 64-bit size, unknown-box skip, cookie
  extraction, faststart-violation error); the adaptive Rice decoder; the
  predictor; the stereo decorrelation; the uncompressed-frame path.
- **Playback:** a short playable `.m4a` in the workspace `testdata/`; `qa-audio`
  plays it; an e2e test renders it and checks the tones (like the DSD test).

## 8. Notes / risks

- **Self-delimiting packets** (§4) — verified first; `stsz` fallback exists.
- **RAM:** the inline packet buffer (`maxFrameBytes` + `frameLength × MAX_CH`) is
  bounded and small vs FLAC's block buffer; fine for the DAP.
- **Faststart:** ffmpeg's `+faststart` produces it; the constraint is documented
  and enforced with a clear error, not a silent failure.
