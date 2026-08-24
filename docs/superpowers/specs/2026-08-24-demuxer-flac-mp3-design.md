# soul-audio-core — Demuxer Slice 2: FLAC + MP3 demuxers

**Date:** 2026-08-24
**Status:** Approved — implementing
**Builds on:** demuxer slice 1 (Packet/Demuxer, WAV + DSD). Research: the earlier
WAV+FLAC and MP3 agent reports.
**Scope:** container parsing for FLAC and MP3 → real `StreamInfo` + packet
iteration. The **codecs** (FLAC decompression, MP3 decode) remain out of scope —
`decode_into` stays `Unsupported`, but `stream_info()` becomes real, and `open()`
surfaces correct parameters for these formats.

## 1. FLAC demuxer (`demux/flac.rs`)

Parse the native FLAC metadata (big-endian, bit-packed):
- `fLaC` marker. Tolerate a leading **ID3v2** tag (skip via the syncsafe size);
  reject **Ogg-FLAC** (`OggS`) as `Unsupported`.
- Metadata block chain: header byte `b` → `last = b & 0x80`, `type = b & 0x7F`;
  then `length: u24 BE`. Parse **STREAMINFO** (type 0, 34 bytes); skip the rest by
  `length` until the last-flagged block. `audio_start` = position after it.
- **STREAMINFO** (272 bits): `min/max_block u16`, `min/max_frame u24`, then a
  64-bit packed field → `sample_rate = v>>44` (20 bits), `channels = ((v>>41)&7)+1`,
  `bits = ((v>>36)&0x1F)+1`, `total_samples = v & 0xF_FFFF_FFFF` (36 bits), then md5.
- `StreamInfo { sample_rate, channels, format: Int { bits } }`.
- **Packets:** FLAC frame length is not in the frame header (found by decoding or
  the next sync), so the demuxer hands the codec **raw audio bytes** in chunks
  (`next_packet` fills scratch from `audio_start` onward; `dur = 0` = unknown).
  Precise frame delimiting + SEEKTABLE seeking is codec-era work (deferred).

## 2. MP3 demuxer (`demux/mp3.rs`)

- Skip a leading **ID3v2** tag (10-byte header + syncsafe 28-bit size; footer if
  flagged). (ID3v1 at EOF is ignored — bounded by frame parsing.)
- **Find + validate a frame header** (32 bits BE): `0xFF Ex` sync; version bits
  ≠ reserved; layer ≠ reserved; bitrate index ∈ 1..14; samplerate index ≠ 3.
  Resync byte-by-byte on a bad candidate.
- Parse: MPEG **version** (2.5/2/1), **layer** (III here), bitrate & samplerate via
  the standard tables, padding, channel mode (`11` = mono → 1 ch, else 2).
- **Frame length (the version-dependent bug):**
  `frame_len = coef * bitrate_bps / sample_rate + padding`, `coef = 144` (MPEG-1
  Layer III) or `72` (MPEG-2/2.5 Layer III); samples/frame `spf = 1152` (MPEG-1)
  or `576` (MPEG-2/2.5).
- `StreamInfo { sample_rate, channels, format: Int { bits: 16 } }` (MP3 decodes to
  16-bit PCM).
- **Packets:** one MPEG frame = one packet (`frame_len` bytes, header included),
  `ts += spf`. Xing/VBRI TOC + seeking are deferred (need the codec/seek).

## 3. Decoders (`formats/flac.rs`, `formats/mp3.rs`)

`FlacDecoder<S>` / `Mp3Decoder<S>` now **hold their demuxer**: `stream_info()`
delegates (real), `decode_into` returns `Unsupported` (codec deferred). `open()`
already routes to `AnyDecoder::Flac`/`Mp3`; those now report real params.

## 4. Testing

- **FLAC:** hand-built `fLaC` + STREAMINFO → assert `sample_rate`/`channels`/`bits`;
  ID3v2-prefixed FLAC parses; `OggS` → `Unsupported`; `next_packet` yields the
  audio bytes after the metadata chain.
- **MP3:** hand-built frame headers for MPEG-1 L3 and MPEG-2 L3 → assert
  `sample_rate`/`channels` and the **frame_len** math (144 vs 72 coefficient);
  ID3v2 skip; `next_packet` returns whole frames with correct `dur = spf`;
  resync past leading garbage.
- `open()` over FLAC/MP3 bytes → correct `stream_info()`; `decode_into` still
  `Unsupported`.
- Architecture (`layering`/`no_alloc`) + `thumbv7em` gate unchanged.

## 5. Out of scope

FLAC/MP3 **codec decode**; FLAC frame delimiting + SEEKTABLE seek; MP3 Xing/VBRI
TOC + seek; free-format MP3; MPEG Layers I/II.
