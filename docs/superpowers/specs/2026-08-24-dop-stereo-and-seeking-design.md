# soul-audio-core — Stereo DoP fix + WAV/DSD seeking

**Date:** 2026-08-24
**Status:** Approved — implementing
**Builds on:** demuxer slice 1 (WavDemuxer, DsfDecoder, DffDecoder, DopAdapter).

## 1. Research grounding (two agent reports; citations below)

- **DoP Open Standard v1.1** (dCS), FFmpeg `dsddec`/DoP encoder, MPD `Dop.cxx`,
  XMOS lib_xua: the DoP word is `marker<<16 | dsd_byte_t0<<8 | dsd_byte_t1`
  (earlier byte high); the marker `0x05↔0xFA` **toggles once per DoP frame** and
  **every channel of a frame shares the same marker**; channels are ordinary
  interleaved PCM frames at `DSD_rate/16`. Phase is deterministic:
  `marker = (frame_index & 1) ? 0xFA : 0x05`.
- **Seeking** (dr_wav, miniaudio, Symphonia, FFmpeg `dsfdec.c`): WAV PCM seek is
  pure arithmetic (`data_start + frame*blockAlign`), sample-accurate. DSD seek is
  byte/block-granular (bit-accuracy is meaningless at ≥2.8 MHz and would force
  bit-shifting the packed stream). Gate seeking on the `Seek` capability at the
  type level.

## 2. Bug fix: `DopAdapter` stereo framing

Current `decode_into` toggles `marker ^= 0xFF` per emitted `i32` → in stereo, the
two channels of one time-frame get **different** markers (DoP violation → DAC
pop). Fix:

- Capture `channels` (`C`) from `inner.stream_info()` at construction; hold a
  `frame_index: u64` and a `group: [u8; 2*MAX_DOP_CHANNELS]`.
- Per DoP frame: read `2*C` normalized MSB-first bytes (`t0` row then `t1` row).
  For channel `ch`: `word = marker<<16 | group[ch]<<8 | group[C+ch]`, emit `L,R,…`
  interleaved. `marker = (frame_index & 1) ? 0xFA : 0x05`. Advance one frame
  (all channels), `frame_index += 1`. An incomplete trailing group ends the stream.
- `MAX_DOP_CHANNELS = 8` (group ≤ 16 bytes). `channels == 0 || > MAX` →
  `Unsupported` at decode.
- Mono is the `C = 1` case → identical to today's output (regression-safe).

## 3. DSD seeking

New trait (segregated, like the DSD seam):
```rust
pub trait DsdSeek {
    /// Seek to per-channel DSD sample `s` (byte/block granular). Returns the
    /// sample actually landed on.
    fn seek_to_sample(&mut self, s: u64) -> Result<u64, DecodeError>;
}
```

**`impl<S: SeekSource> DsdSeek for DffDecoder<S>`** — interleaved, trivial:
```
group = min(s, total_samples) / 8
byte  = sound_data_start + group * channels
seek(byte); audio_bytes_remaining = audio_total - group*channels
return group * 8
```

**`impl<S: SeekSource> DsdSeek for DsfDecoder<S>`** — planar, must reload a row:
```
s        = min(s, total_samples)
row_r    = s / (4096*8)
byte_in  = (s/8) % 4096
seek(data_start + row_r * channels*4096)
audio_bytes_remaining   = audio_total   - row_r*channels*4096
valid_bytes_per_channel = valid_total   - row_r*4096          // valid_total = total_samples.div_ceil(8)
row_len = 0; emit_i = 0                                        // force fresh load_row()
if byte_in > 0 && load_row(): emit_i = min(byte_in*channels, row_len*channels)
return row_r*4096*8 + (emit_i/channels)*8
```

Both need fields cached at parse time: `data_start`/`sound_data_start`,
`audio_total`, `total_samples` (DSF `sample_count`; DFF `audio_total/channels*8`),
`channels`. DSF also keeps `valid_total`.

**`DopAdapter::seek_to_frame`** (marker phase re-derivation):
```rust
impl<D: DsdDecoder + DsdSeek> DopAdapter<D> {
    fn seek_to_frame(&mut self, dop_frame: u64) -> Result<u64, DecodeError> {
        let landed = self.inner.seek_to_sample(dop_frame * 16)?; // 16 DSD bits/DoP frame
        self.frame_index = landed / 16;   // deterministic marker phase
        self.ch_pos = 0;                  // drop any partial frame
        Ok(self.frame_index)
    }
}
```

## 4. WAV seeking

Already implemented and correct (`WavDemuxer::seek_to` / `WavDecoder::seek_to`).
Add a stronger decode-after-seek round-trip test. Time→frame stays a caller
conversion (`frame = secs * sample_rate`) to keep core float-free.

## 5. Non-seekable

Unchanged: seek methods live in `impl<S: SeekSource>` / `D: DsdSeek` blocks, so a
plain `Source`/non-seekable DSD decoder simply cannot name them (compile-time).

## 6. Testing

- **DoP stereo:** fake stereo `DsdDecoder` → assert L/R DoP words share the frame
  marker and carry the right per-channel `t0`/`t1` bytes; marker toggles per frame,
  not per channel. Mono regression unchanged.
- **DFF seek:** build DFF, `seek_to_sample`, read → bytes match the target group.
- **DSF seek:** build multi-row DSF, seek across a block-row boundary and within a
  row → normalized output matches; padding/valid bookkeeping correct.
- **DoP seek phase:** seek to an odd DoP frame → first emitted word uses `0xFA`.
- **WAV:** decode-after-seek round trip.
- Architecture (`layering`/`no_alloc`) + `thumbv7em` gate unchanged.

## 7. Out of scope

Stereo DoP for >8 channels; time-based (float) seek API; forward-only skip
emulation for non-seekable sources; DSF sub-byte (bit) seek; FLAC/MP3 (slice 2).

## Citations
DoP Open Standard v1.1 (dsd-guide.com); FFmpeg `libavformat/dsfdec.c`,
`libavcodec/dsddec.c`; MPD `src/pcm/Dop.cxx`; dr_wav / miniaudio seek; Symphonia
`FormatReader`/`SeekMode`; Sony DSF & Philips DSDIFF specs.
