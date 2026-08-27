//! AAC-LC decoder. The submodules provide the bitstream primitives (`bits`),
//! the Huffman codebook decode (`huffman`), the static tables (`tables`), the
//! `raw_data_block`/ICS syntax parser (`syntax`), the inverse quant + stereo +
//! TNS DSP (`dsp`), and the IMDCT filterbank (`filterbank`). This module's
//! [`AacDecoder`] is the top-level orchestration that wires them into the
//! crate's [`Decoder`] seam.
//!
//! Two container entry paths converge on one core (see the design spec §2): an
//! MP4/`.m4a` `mp4a` sample entry (via [`Mp4Reader`]) and a raw ADTS `.aac`
//! elementary stream (via [`AdtsReader`]). Both hand the same self-delimiting
//! `raw_data_block` bytes to [`decode_rdb`]; the only difference is framing —
//! ADTS headers give exact frame lengths, while MP4 access units self-delimit
//! (each `raw_data_block` ends with `ID_END` + byte-align), so — like ALAC —
//! no `stsz` table is needed.

pub(crate) mod bits;
pub(super) mod dsp;
pub(crate) mod filterbank;
mod huffman;
mod syntax;
mod tables;

use crate::decoder::{DecodeError, Decoder};
use crate::demux::adts::{AacConfig, AdtsReader};
use crate::demux::mp4::{Mp4Codec, Mp4Reader};
use crate::io::Source;
use crate::model::{SampleFormat, StreamInfo};

use filterbank::Filterbank;

/// One AAC-LC frame is 1024 samples/channel; stereo interleaved is 2048. The
/// carry buffer holds one decoded frame with headroom (the task brief's
/// `[i32; 2048*2]`), so `decode_into` can hand back partial frames across calls.
const PCM_CAP: usize = 2048 * 2;

/// Input byte buffer: one whole `raw_data_block` must fit. ADTS's 13-bit
/// `aac_frame_length` caps a frame at 8191 bytes (payload a little less), and a
/// real AAC-LC stereo access unit is far smaller; 8192 covers both the ADTS
/// `next_frame` target and the MP4 sliding-window that must always hold a
/// complete access unit before decoding it.
const AU_CAP: usize = 8192;

/// Full-scale factor turning the filterbank's ISO `2/N`-scaled float time
/// samples into 16-bit PCM. The IMDCT (`filterbank.rs`) reconstructs to the
/// encoder's coefficient units — the same inverse-quant convention ffmpeg uses
/// — which already land in the ±32768 range, so no extra gain is applied
/// (empirically the ours-vs-ffmpeg amplitude ratio is 1.000; see the Task 8
/// report). Kept as a named constant so a future scale recalibration is a
/// one-line change, exactly where the design spec says the scale lives.
const OUTPUT_GAIN: f32 = 1.0;

/// Round an `f32` time sample to the `i16` range, stored in an `i32`. Manual
/// round-half-away-from-zero (`s ± 0.5` then truncate) — no `f32::round`, which
/// is std-only — with saturation to `[-32768, 32767]`. `NaN` maps to `0` (the
/// saturating `as i32` cast), so this never produces a garbage sample.
fn round_i16(s: f32) -> i32 {
    let r = if s >= 0.0 { s + 0.5 } else { s - 0.5 };
    if r >= 32767.0 {
        32767
    } else if r <= -32768.0 {
        -32768
    } else {
        r as i32
    }
}

/// Decode one `raw_data_block` all the way to interleaved 16-bit-range PCM in
/// `pcm`, returning `(interleaved_samples_written, bytes_consumed)`. The byte
/// count (from the reader's byte-aligned end position) is what the MP4 sliding
/// window advances by; ADTS ignores it (its header already framed the block).
///
/// Pipeline order (matters — the ISO 14496-3 decoder block diagram): syntax
/// parse → per-channel `dequantize` → (CPE only) `apply_stereo` (M/S +
/// intensity, needs both channels' dequantized spectra) → per-channel
/// `apply_tns` → per-channel `imdct_and_overlap` → interleave + round. The
/// caller's [`Filterbank`] is stateful (per-channel overlap carried across
/// frames), so channel indices stay 0/1 every frame.
///
/// Never panics: `decode_raw_data_block` bounds-checks every field, and every
/// DSP stage independently re-clamps its indices (see `dsp`/`filterbank`).
fn decode_rdb(
    rdb: &[u8],
    config: AacConfig,
    sfi: u8,
    fb: &mut Filterbank,
    pcm: &mut [i32],
) -> Result<(usize, usize), DecodeError> {
    let mut r = bits::BitReader::new(rdb);
    let mut frame = syntax::decode_raw_data_block(&mut r, config)?;
    // `decode_raw_data_block` byte-aligns before returning, so this is the exact
    // access-unit length in bytes.
    let consumed = r.bit_pos().div_ceil(8);
    let n = frame.n_channels as usize;
    if n == 0 {
        // A block carrying only DSE/FIL/PCE (no audio element) — no samples, but
        // real bytes were consumed, so the MP4 window still advances.
        return Ok((0, consumed));
    }

    for ch in 0..n {
        dsp::dequantize(&mut frame.channels[ch]);
    }

    // A CPE is the only path to two channels here (two independent SCEs are out
    // of scope), so `n == 2` means M/S + intensity side info may apply. Both
    // are no-ops when the bitstream didn't signal them, and `apply_stereo`
    // re-validates the shared window layout, so this is safe for any 2-channel
    // frame.
    if n == 2 {
        let ms_mask_present = frame.ms_mask_present;
        let ms_used = frame.ms_used;
        let (a, b) = frame.channels.split_at_mut(1);
        dsp::apply_stereo(&mut a[0], &mut b[0], ms_mask_present, &ms_used);
    }

    for ch in 0..n {
        dsp::apply_tns(&mut frame.channels[ch], sfi);
    }

    let mut outs = [[0f32; 1024]; 2];
    for (ch, out) in outs.iter_mut().enumerate().take(n) {
        fb.imdct_and_overlap(ch, &frame.channels[ch], out);
    }

    let frames = 1024usize;
    for i in 0..frames {
        for ch in 0..n {
            pcm[i * n + ch] = round_i16(outs[ch][i] * OUTPUT_GAIN);
        }
    }
    Ok((frames * n, consumed))
}

/// The frame-source seam: yields the next access unit's `raw_data_block` bytes
/// plus (at construction) the stream config, over either container.
enum FrameSrc<S> {
    /// MP4/`.m4a` — access units concatenated in `mdat`, framed by decode.
    Mp4(Mp4Reader<S>),
    /// Raw ADTS `.aac` — each frame's `raw_data_block` framed by its header.
    Adts(AdtsReader<S>),
}

/// A streaming AAC-LC decoder over an MP4/`.m4a` or raw ADTS [`Source`],
/// producing interleaved 16-bit-range `i32` PCM. Mono + stereo only. No
/// allocation: every buffer is fixed-size and owned by value; the [`Filterbank`]
/// overlap state and the decoded-PCM carry buffer persist across `decode_into`
/// calls.
pub struct AacDecoder<S> {
    src: FrameSrc<S>,
    config: AacConfig,
    sfi: u8,
    channels: u16,
    fb: Filterbank,
    /// Shared input buffer: ADTS's `next_frame` target, or the MP4 sliding
    /// access-unit window (`mp4_filled` valid bytes, `mp4_used` already decoded).
    inbuf: [u8; AU_CAP],
    mp4_filled: usize,
    mp4_used: usize,
    mp4_mdat_remaining: u64,
    total_samples: Option<u64>,
    /// Decoded interleaved PCM awaiting hand-off, `[pcm_pos, pcm_len)`.
    pcm: [i32; PCM_CAP],
    pcm_len: usize,
    pcm_pos: usize,
    eof: bool,
}

impl<S: Source> AacDecoder<S> {
    /// Build over a parsed MP4 reader positioned at `mdat`. Its sample entry
    /// must be `mp4a` (AAC); an `alac` reader is [`DecodeError::Unsupported`]
    /// (route it to `AlacDecoder` instead).
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a non-AAC sample entry, a non-LC object
    /// type, or a channel count outside 1..=2; [`DecodeError::Malformed`] for an
    /// unresolvable sample rate.
    pub fn new_mp4(reader: Mp4Reader<S>) -> Result<Self, DecodeError> {
        let config = match reader.codec() {
            Mp4Codec::Aac(c) => c,
            Mp4Codec::Alac(_) => return Err(DecodeError::Unsupported),
        };
        let mdat_remaining = reader.mdat_remaining();
        let total = reader.total_samples();
        Self::build(FrameSrc::Mp4(reader), config, mdat_remaining, Some(total))
    }

    /// Build over an ADTS reader (its first header already validated). ADTS is
    /// streaming, so there's no declared length ([`Self::total_frames`] is
    /// `None`).
    ///
    /// # Errors
    /// As [`Self::new_mp4`] (config validation).
    pub fn new_adts(reader: AdtsReader<S>) -> Result<Self, DecodeError> {
        let config = reader.config();
        Self::build(FrameSrc::Adts(reader), config, 0, None)
    }

    fn build(
        src: FrameSrc<S>,
        config: AacConfig,
        mdat_remaining: u64,
        total_samples: Option<u64>,
    ) -> Result<Self, DecodeError> {
        if config.object_type != 2 {
            return Err(DecodeError::Unsupported); // AAC-LC only
        }
        if config.channels == 0 || config.channels > 2 {
            return Err(DecodeError::Unsupported); // mono + stereo only
        }
        // Validate the sample rate up front (also the `sfi` the DSP stages need)
        // so a bad ASC/header fails at construction, not mid-decode.
        let sfi = tables::resolve_sfi(config.sample_rate)?;
        Ok(Self {
            src,
            config,
            sfi,
            channels: u16::from(config.channels),
            fb: Filterbank::new(),
            inbuf: [0; AU_CAP],
            mp4_filled: 0,
            mp4_used: 0,
            mp4_mdat_remaining: mdat_remaining,
            total_samples,
            pcm: [0; PCM_CAP],
            pcm_len: 0,
            pcm_pos: 0,
            eof: false,
        })
    }

    /// Total per-channel frames from the container's `mdhd` (MP4), or `None`
    /// for streaming ADTS.
    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        self.total_samples
    }

    /// Decode the next access unit into `self.pcm`, looping past any audio-less
    /// blocks. `Ok(false)` at end of stream.
    fn fill_next_frame(&mut self) -> Result<bool, DecodeError> {
        loop {
            let samples = match self.decode_next_rdb()? {
                Some(s) => s,
                None => return Ok(false),
            };
            if samples > 0 {
                self.pcm_len = samples;
                self.pcm_pos = 0;
                return Ok(true);
            }
            // Zero-sample block (DSE/FIL/PCE only) — keep decoding.
        }
    }

    /// Decode one `raw_data_block`, dispatching on the container. `Ok(None)` at
    /// end of stream; `Ok(Some(samples))` otherwise (`samples` may be 0).
    fn decode_next_rdb(&mut self) -> Result<Option<usize>, DecodeError> {
        match &self.src {
            FrameSrc::Mp4(_) => self.decode_next_rdb_mp4(),
            FrameSrc::Adts(_) => self.decode_next_rdb_adts(),
        }
    }

    /// MP4 path: slide out the previously-decoded bytes, refill the window from
    /// `mdat`, decode the leading access unit, and record its byte length.
    fn decode_next_rdb_mp4(&mut self) -> Result<Option<usize>, DecodeError> {
        // Drop the previous frame's bytes to the front.
        if self.mp4_used > 0 {
            if self.mp4_used < self.mp4_filled {
                self.inbuf.copy_within(self.mp4_used..self.mp4_filled, 0);
                self.mp4_filled -= self.mp4_used;
            } else {
                self.mp4_filled = 0;
            }
            self.mp4_used = 0;
        }
        // Refill from the mdat stream (bounded by capacity and what's left).
        while self.mp4_filled < AU_CAP && self.mp4_mdat_remaining > 0 {
            let start = self.mp4_filled;
            let want = ((AU_CAP - start) as u64).min(self.mp4_mdat_remaining) as usize;
            let end = start + want;
            let n = {
                let reader = match &mut self.src {
                    FrameSrc::Mp4(r) => r,
                    FrameSrc::Adts(_) => unreachable!("dispatched by decode_next_rdb"),
                };
                reader
                    .src_mut()
                    .read(&mut self.inbuf[start..end])
                    .map_err(|_| DecodeError::Io)?
            };
            if n == 0 {
                break;
            }
            self.mp4_filled += n;
            self.mp4_mdat_remaining -= n as u64;
        }
        if self.mp4_filled == 0 {
            return Ok(None); // mdat drained
        }
        let (samples, consumed) = decode_rdb(
            &self.inbuf[..self.mp4_filled],
            self.config,
            self.sfi,
            &mut self.fb,
            &mut self.pcm,
        )?;
        self.mp4_used = consumed;
        Ok(Some(samples))
    }

    /// ADTS path: the reader frames each `raw_data_block` from its header.
    fn decode_next_rdb_adts(&mut self) -> Result<Option<usize>, DecodeError> {
        let len = {
            let reader = match &mut self.src {
                FrameSrc::Adts(r) => r,
                FrameSrc::Mp4(_) => unreachable!("dispatched by decode_next_rdb"),
            };
            match reader.next_frame(&mut self.inbuf)? {
                Some(rdb) => rdb.len(),
                None => return Ok(None),
            }
        };
        let (samples, _consumed) = decode_rdb(
            &self.inbuf[..len],
            self.config,
            self.sfi,
            &mut self.fb,
            &mut self.pcm,
        )?;
        Ok(Some(samples))
    }
}

impl<S: Source> Decoder for AacDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.config.sample_rate,
            channels: self.channels,
            format: SampleFormat::Int { bits: 16 },
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let mut written = 0;
        while written < out.len() {
            if self.pcm_pos >= self.pcm_len {
                if self.eof {
                    break;
                }
                if !self.fill_next_frame()? {
                    self.eof = true;
                    break;
                }
            }
            let n = (out.len() - written).min(self.pcm_len - self.pcm_pos);
            out[written..written + n].copy_from_slice(&self.pcm[self.pcm_pos..self.pcm_pos + n]);
            self.pcm_pos += n;
            written += n;
        }
        Ok(written)
    }
}
