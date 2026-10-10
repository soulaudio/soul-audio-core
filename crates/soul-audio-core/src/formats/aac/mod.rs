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
pub(crate) mod ps;
pub(crate) mod sbr;
mod syntax;
mod tables;

use crate::decoder::{DecodeError, Decoder, SeekableDecoder};
use crate::demux::adts::{AacConfig, AdtsReader};
use crate::demux::mp4::{Mp4Codec, Mp4Reader};
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

use filterbank::Filterbank;

/// One AAC-LC frame is 1024 samples/channel; an SBR (or SBR+PS) frame is
/// **2048** samples/channel (2× rate, `sbr-reference.md` §0). The carry buffer
/// holds exactly one decoded stereo frame — `2048 · 2 ch` interleaved i32, the
/// largest a single `decode_rdb` can produce (SBR/PS 2×-rate stereo; the LC
/// path is half that) — which `decode_into` drains across calls. Sized to one
/// frame (not two): `decode_rdb` overwrites `pcm` per frame and never
/// accumulates, so a second frame's headroom was pure slack. Keeping
/// `AacDecoder` compact matters because it is the largest [`AnyDecoder`]
/// variant, embedded by value on the (default-stack) construction path.
/// Maximum channels this decoder reconstructs: 7.1 (`channel_configuration` 7).
/// Covers mono/stereo (1/2), 5.1 (6), and 7.1 (8) — every standard AAC layout.
/// The large per-channel [`syntax::DecodedFrame`] scales with this but stays a
/// transient on the (big-stack) decode path, never embedded in the persistent
/// `AacDecoder`/[`AnyDecoder`](crate::AnyDecoder) value.
pub(crate) const MAX_CHANNELS: usize = 8;

/// Interleaved PCM scratch for one decoded frame: the larger of the LC
/// multichannel write (1024 samples × `MAX_CHANNELS`) and the ≤2-channel SBR
/// write (2048 × 2 at the doubled HE-AAC rate). `decode_rdb` overwrites `pcm`
/// per frame and never accumulates.
const PCM_CAP: usize = 1024 * MAX_CHANNELS;

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
/// parse → per-channel `dequantize` → per-CPE `apply_stereo` (M/S + intensity,
/// needs both of a pair's dequantized spectra; a 5.1/7.1 block has several
/// CPEs) → per-channel `apply_tns` → per-channel `imdct_and_overlap` →
/// interleave + round. The caller's [`Filterbank`] is stateful (per-channel
/// overlap carried across frames), so a channel keeps the same index every
/// frame.
///
/// Never panics: `decode_raw_data_block` bounds-checks every field, and every
/// DSP stage independently re-clamps its indices (see `dsp`/`filterbank`).
#[allow(clippy::too_many_arguments)]
fn decode_rdb(
    rdb: &[u8],
    config: AacConfig,
    sfi: u8,
    fb: &mut Filterbank,
    sbr: &mut sbr::SbrState,
    sbr_out: &mut sbr::SbrOutput,
    output_rate: u32,
    pcm: &mut [i32],
) -> Result<(usize, usize, bool), DecodeError> {
    let mut r = bits::BitReader::new(rdb);
    let mut frame = syntax::decode_raw_data_block(&mut r, config)?;
    // `decode_raw_data_block` byte-aligns before returning, so this is the exact
    // access-unit length in bytes.
    let consumed = r.bit_pos().div_ceil(8);
    let n = frame.n_channels as usize;
    if n == 0 {
        // A block carrying only DSE/FIL/PCE (no audio element) — no samples, but
        // real bytes were consumed, so the MP4 window still advances.
        return Ok((0, consumed, false));
    }

    for ch in 0..n {
        dsp::dequantize(&mut frame.channels[ch]);
    }

    // M/S + intensity stereo, applied per CPE (each carries its own side info —
    // a 5.1/7.1 block has several CPEs). Both are no-ops when the bitstream
    // didn't signal them, and `apply_stereo` re-validates the shared window
    // layout, so this is safe for any pair. `ms_used` is a `Copy` bool matrix,
    // so copying it out releases the `frame.stereo` borrow before the disjoint
    // `frame.channels` split.
    for si in 0..frame.n_stereo as usize {
        let ch0 = frame.stereo[si].ch0 as usize;
        let ms_mask_present = frame.stereo[si].ms_mask_present;
        let ms_used = frame.stereo[si].ms_used;
        let (a, b) = frame.channels.split_at_mut(ch0 + 1);
        dsp::apply_stereo(&mut a[ch0], &mut b[0], ms_mask_present, &ms_used);
    }

    for ch in 0..n {
        dsp::apply_tns(&mut frame.channels[ch], sfi);
    }

    // LC core PCM for every channel (also the SBR analysis QMF's input).
    let mut outs = [[0f32; 1024]; MAX_CHANNELS];
    for (ch, out) in outs.iter_mut().enumerate().take(n) {
        fb.imdct_and_overlap(ch, &frame.channels[ch], out);
    }

    // SBR (HE-AAC): a fill element in this block carried EXT_SBR_DATA. Run the
    // §10 drive over the core PCM; when it produces output, emit **2048**
    // interleaved samples/channel (2× rate) instead of 1024. Mono/stereo only
    // — the SBR drive and its QMF scratch are sized for ≤2 channels, so a
    // (rare) multichannel HE-AAC stream decodes its LC core without SBR rather
    // than misindex. The LC-only path below is byte-identical when no SBR fill
    // element was present.
    if let Some(loc) = frame.sbr.filter(|_| n <= 2) {
        // SBR consumes the ≤2 core channels; hand it a fixed 2-element view of
        // the wider multichannel `outs` (always in range — `outs` is length
        // `MAX_CHANNELS` ≥ 2, and this arm only runs when `n <= 2`).
        let core: &[[f32; 1024]; 2] = outs[..2]
            .try_into()
            .expect("outs has >= 2 channels for the SBR path");
        let applied = sbr::decode_fill_sbr(
            rdb,
            loc.start_bit,
            loc.end_bit,
            loc.crc,
            loc.id_cpe,
            sbr,
            core,
            n,
            output_rate,
            sbr_out,
        )?;
        if applied {
            // R6: Parametric Stereo may have upmixed a mono core to stereo, so
            // interleave by the channel count SBR actually produced
            // (`sbr_out.channels`), not the core element count `n`. The caller
            // reads `sbr_out.channels` after this returns to promote
            // `AacDecoder.channels` 1->2 (mirroring the `sbr_confirmed` rate
            // promotion). This 2x-rate stereo write of 2048·2 samples is the
            // largest a frame produces and exactly fills `PCM_CAP` (= 2048·2).
            let out_ch = sbr_out.channels.clamp(1, 2);
            let frames = 2048usize;
            for i in 0..frames {
                for ch in 0..out_ch {
                    pcm[i * out_ch + ch] = round_i16(sbr_out.pcm[ch][i] * OUTPUT_GAIN);
                }
            }
            return Ok((frames * out_ch, consumed, true));
        }
    }

    let frames = 1024usize;
    for i in 0..frames {
        for ch in 0..n {
            pcm[i * n + ch] = round_i16(outs[ch][i] * OUTPUT_GAIN);
        }
    }
    Ok((frames * n, consumed, false))
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
/// producing interleaved 16-bit-range `i32` PCM. Mono through 7.1 (up to
/// [`MAX_CHANNELS`]); HE-AAC (SBR/PS) is mono/stereo only. No allocation: every
/// buffer is fixed-size and owned by value; the [`Filterbank`] overlap state
/// and the decoded-PCM carry buffer persist across `decode_into` calls.
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
    total_samples: Option<u64>,
    /// SBR (HE-AAC) decode state — always present, starts inactive (pure
    /// upsample-only, `kx=32 M=0`). Costs nothing on an LC-only stream: the
    /// drive only runs when a fill element carries `EXT_SBR_DATA`.
    sbr: sbr::SbrState,
    /// 2×-rate PCM scratch the SBR drive fills (reused each frame).
    sbr_out: sbr::SbrOutput,
    /// SBR operating/output rate (= 2×core, or the explicit `ext_sample_rate`).
    output_rate: u32,
    /// `true` once SBR output has been produced (explicit ASC signal, or the
    /// first `EXT_SBR_DATA` frame) — flips `stream_info().sample_rate` to 2×.
    sbr_confirmed: bool,
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
        // A fragmented stream carries no `mdhd` total; report length unknown
        // (like ADTS) and let decode run until the `mdat` fragments drain.
        let total = if reader.is_fragmented() {
            None
        } else {
            Some(reader.total_samples())
        };
        Self::build(FrameSrc::Mp4(reader), config, total)
    }

    /// Build over an ADTS reader (its first header already validated). ADTS is
    /// streaming, so there's no declared length ([`Self::total_frames`] is
    /// `None`).
    ///
    /// # Errors
    /// As [`Self::new_mp4`] (config validation).
    pub fn new_adts(reader: AdtsReader<S>) -> Result<Self, DecodeError> {
        let config = reader.config();
        Self::build(FrameSrc::Adts(reader), config, None)
    }

    fn build(
        src: FrameSrc<S>,
        config: AacConfig,
        total_samples: Option<u64>,
    ) -> Result<Self, DecodeError> {
        if config.object_type != 2 {
            return Err(DecodeError::Unsupported); // AAC-LC only
        }
        if config.channels == 0 || config.channels as usize > MAX_CHANNELS {
            return Err(DecodeError::Unsupported); // mono … 7.1 (channel_configuration 1-7)
        }
        // Validate the sample rate up front (also the `sfi` the DSP stages need)
        // so a bad ASC/header fails at construction, not mid-decode.
        let sfi = tables::resolve_sfi(config.sample_rate)?;
        // SBR output rate: the explicit ASC extension rate if signalled, else
        // 2×core (the dual-rate default). Reported by `stream_info` only once
        // SBR is confirmed (up front for explicit ASC, after the first
        // EXT_SBR_DATA frame for implicit ADTS).
        let output_rate = config
            .ext_sample_rate
            .unwrap_or_else(|| config.sample_rate.saturating_mul(2));
        Ok(Self {
            src,
            config,
            sfi,
            channels: u16::from(config.channels),
            fb: Filterbank::new(),
            inbuf: [0; AU_CAP],
            mp4_filled: 0,
            mp4_used: 0,
            total_samples,
            sbr: sbr::SbrState::default(),
            sbr_out: sbr::SbrOutput::default(),
            output_rate,
            sbr_confirmed: config.sbr_signalled,
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

    /// Gapless encoder delay (leading priming, per-channel samples) from the
    /// MP4 `elst` edit list; `0` for raw ADTS (no container edit list) or an MP4
    /// without one. The decoder emits the full decode — the [`Deck`] trims this
    /// many samples off the front for gapless playback.
    ///
    /// [`Deck`]: crate::deck::Deck
    fn mp4_encoder_delay(&self) -> u64 {
        match &self.src {
            FrameSrc::Mp4(r) => r.encoder_delay(),
            FrameSrc::Adts(_) => 0,
        }
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
        // Refill from the mdat stream (up to capacity). `read_audio` bounds the
        // read to the current `mdat` and, for a fragmented file, transparently
        // crosses into the next fragment's `mdat`; it returns 0 at true EOF.
        while self.mp4_filled < AU_CAP {
            let start = self.mp4_filled;
            let n = {
                let reader = match &mut self.src {
                    FrameSrc::Mp4(r) => r,
                    FrameSrc::Adts(_) => unreachable!("dispatched by decode_next_rdb"),
                };
                reader.read_audio(&mut self.inbuf[start..])?
            };
            if n == 0 {
                break;
            }
            self.mp4_filled += n;
        }
        if self.mp4_filled == 0 {
            return Ok(None); // mdat drained
        }
        let (samples, consumed, sbr_applied) = decode_rdb(
            &self.inbuf[..self.mp4_filled],
            self.config,
            self.sfi,
            &mut self.fb,
            &mut self.sbr,
            &mut self.sbr_out,
            self.output_rate,
            &mut self.pcm,
        )?;
        self.mp4_used = consumed;
        if sbr_applied {
            self.sbr_confirmed = true;
            self.promote_channels_if_ps();
        }
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
        let (samples, _consumed, sbr_applied) = decode_rdb(
            &self.inbuf[..len],
            self.config,
            self.sfi,
            &mut self.fb,
            &mut self.sbr,
            &mut self.sbr_out,
            self.output_rate,
            &mut self.pcm,
        )?;
        if sbr_applied {
            self.sbr_confirmed = true;
            self.promote_channels_if_ps();
        }
        Ok(Some(samples))
    }

    /// Promote the reported channel count 1->2 the first time Parametric Stereo
    /// upmixes a mono core to stereo (R6), mirroring the `sbr_confirmed`
    /// sample-rate promotion. `SbrOutput.channels` is set to 2 by the SBR drive
    /// whenever PS ran; from then on `stream_info().channels` reports 2 and
    /// `decode_rdb` interleaves every frame as stereo.
    fn promote_channels_if_ps(&mut self) {
        if self.sbr_out.channels == 2 && self.channels < 2 {
            self.channels = 2;
        }
    }
}

impl<S: SeekSource> AacDecoder<S> {
    /// Seek to per-channel output `sample` (clamped). AAC frames are **not**
    /// independent — the filterbank carries a 1024-sample MDCT overlap, and
    /// HE-AAC SBR/PS carry QMF delay lines + delta-time-coded envelopes — so a
    /// seek resets all decode state and **pre-rolls** (decode-and-discards) a
    /// run of frames before the target to reconverge before emitting. MP4 lands
    /// via the sample table (Task 1); ADTS (no index) decode-discards from the
    /// start. The landing is trimmed to the exact output sample.
    ///
    /// # Errors
    /// Propagates the sample-table / IO / decode errors of the pre-roll.
    pub fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        match &self.src {
            FrameSrc::Mp4(_) => self.seek_to_mp4(sample),
            FrameSrc::Adts(_) => self.seek_to_adts(sample),
        }
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to`]). Uses the
    /// reported output rate (2× core once SBR is confirmed).
    ///
    /// # Errors
    /// As [`Self::seek_to`].
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        let rate = if self.sbr_confirmed {
            self.output_rate
        } else {
            self.config.sample_rate
        };
        self.seek_to(crate::decoder::secs_to_samples(secs, rate))
    }

    /// Output samples per access unit: 1024 core samples, doubled once dual-rate
    /// SBR is confirmed (output rate = 2× core).
    fn output_samples_per_au(&self) -> u64 {
        if self.sbr_confirmed && self.config.sample_rate != 0 {
            1024 * u64::from((self.output_rate / self.config.sample_rate).max(1))
        } else {
            1024
        }
    }

    /// Reset all cross-frame decode state to a clean post-seek baseline. Keeps
    /// the monotonic `sbr_confirmed`/`channels`/`output_rate` promotions (the
    /// pre-roll re-establishes the SBR/PS state they describe).
    fn reset_decode_state(&mut self) {
        self.fb = Filterbank::new();
        self.sbr = sbr::SbrState::default();
        self.sbr_out = sbr::SbrOutput::default();
        self.pcm_len = 0;
        self.pcm_pos = 0;
        self.eof = false;
        self.mp4_filled = 0;
        self.mp4_used = 0;
    }

    fn mp4_reader(&mut self) -> &mut Mp4Reader<S> {
        match &mut self.src {
            FrameSrc::Mp4(r) => r,
            FrameSrc::Adts(_) => unreachable!("mp4 seek path"),
        }
    }

    fn seek_to_mp4(&mut self, sample: u64) -> Result<u64, DecodeError> {
        // Pre-roll frames to reconverge filterbank overlap (1 needed) and SBR
        // QMF delay lines + delta-time envelopes (several); a generous fixed
        // run covers both. For tiny targets this clamps to frame 0 (= decode
        // from start, which is exact).
        const PREROLL: u32 = 12;
        let out_per_au = self.output_samples_per_au();
        let num_frames = self.mp4_reader().coded_frame_count()?;
        if num_frames == 0 {
            return Ok(0);
        }
        let max_out = u64::from(num_frames) * out_per_au;
        let target = sample.min(max_out);
        let au_index = u32::try_from(target / out_per_au)
            .unwrap_or(num_frames)
            .min(num_frames - 1);
        // Pure AAC-LC: a short pre-roll restores the MDCT overlap → land near the
        // target (efficient, bit-exact). HE-AAC: SBR is initialized by a header
        // typically sent only in frame 0, and envelopes are delta-time coded, so
        // a mid-stream landing can't reconstruct the high band — decode from the
        // start instead (correct + bit-exact; O(target), acceptable on the rarer
        // HE-AAC seek). (Implicit-SBR MP4, where `sbr_confirmed` is still false
        // here, uses the pre-roll path — no such fixture ships; documented.)
        let land = if self.sbr_confirmed {
            0
        } else {
            au_index.saturating_sub(PREROLL)
        };
        let within_out = target - u64::from(au_index) * out_per_au;

        let offset = self.mp4_reader().frame_file_offset(land)?;
        let end_offset = self.mp4_reader().frame_file_offset(num_frames)?;
        self.mp4_reader()
            .seek_audio(offset, end_offset.saturating_sub(offset))?;
        self.reset_decode_state();

        // Decode-and-discard whole frames [land, au_index).
        let mut cur = land;
        while cur < au_index {
            if !self.fill_next_frame()? {
                return Ok(target); // EOF during pre-roll — clamp
            }
            self.pcm_pos = self.pcm_len;
            cur += 1;
        }
        // Land in au_index's frame and trim to the exact output sample.
        if within_out > 0 && self.fill_next_frame()? {
            let drop = (within_out * u64::from(self.channels.max(1))).min(self.pcm_len as u64);
            self.pcm_pos = drop as usize;
        }
        Ok(target)
    }

    fn seek_to_adts(&mut self, sample: u64) -> Result<u64, DecodeError> {
        // ADTS has no index/length: rewind, reset, and decode-discard output
        // samples until the target (O(target), but correct across LC/SBR/PS).
        match &mut self.src {
            FrameSrc::Adts(r) => r.rewind()?,
            FrameSrc::Mp4(_) => unreachable!("adts seek path"),
        }
        self.reset_decode_state();
        let mut passed = 0u64; // per-channel output samples discarded so far
        while passed < sample {
            if !self.fill_next_frame()? {
                return Ok(passed); // EOF → clamp to the true length
            }
            let ch = u64::from(self.channels.max(1));
            let frame_pc = self.pcm_len as u64 / ch;
            if passed + frame_pc <= sample {
                self.pcm_pos = self.pcm_len; // discard the whole frame
                passed += frame_pc;
            } else {
                self.pcm_pos = ((sample - passed) * ch) as usize;
                passed = sample;
            }
        }
        Ok(sample)
    }
}

impl<S: SeekSource> SeekableDecoder for AacDecoder<S> {
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        AacDecoder::seek_to(self, sample)
    }
    fn total_frames(&self) -> Option<u64> {
        AacDecoder::total_frames(self)
    }
}

impl<S: Source> Decoder for AacDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            // SBR doubles the output rate (dual-rate HE-AAC). Report 2× once
            // SBR is confirmed — up front for an explicit ASC signal, or after
            // the first EXT_SBR_DATA frame for implicit ADTS; the core rate
            // otherwise (pure AAC-LC, byte-identical to before).
            sample_rate: if self.sbr_confirmed {
                self.output_rate
            } else {
                self.config.sample_rate
            },
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

    fn encoder_delay(&self) -> u64 {
        self.mp4_encoder_delay()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::adts::AdtsReader;
    use crate::io::SliceSource;

    /// Decode a whole stream through `AacDecoder` on a generous stack (the SBR
    /// state + per-frame DSP scratch are large — see MEMORY.md / the CLI's own
    /// big-stack loader), returning the interleaved PCM and final stream rate.
    fn decode_all_adts(bytes: &'static [u8]) -> (Vec<i32>, u32, u16) {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let reader = AdtsReader::new(SliceSource::new(bytes)).expect("adts header");
                let mut dec = AacDecoder::new_adts(reader).expect("aac decoder");
                let mut out = Vec::new();
                let mut buf = [0i32; 4096];
                loop {
                    let n = dec.decode_into(&mut buf).expect("decode");
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                let info = dec.stream_info();
                (out, info.sample_rate, info.channels)
            })
            .expect("spawn")
            .join()
            .expect("join")
    }

    /// Decode a whole stream through the public `open` dispatch on a big stack.
    fn decode_all_open(bytes: &'static [u8]) -> (Vec<i32>, u32, u16) {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let mut dec =
                    crate::dispatch::open(SliceSource::new(bytes)).expect("probe + construct");
                let rate_up_front = dec.stream_info().sample_rate;
                let mut out = Vec::new();
                let mut buf = [0i32; 4096];
                loop {
                    let n = dec.decode_into(&mut buf).expect("decode");
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                let info = dec.stream_info();
                assert_eq!(
                    rate_up_front, info.sample_rate,
                    "explicit-SBR rate must be reported up front, not only after decode"
                );
                (out, info.sample_rate, info.channels)
            })
            .expect("spawn")
            .join()
            .expect("join")
    }

    /// Explicit HE-AAC in MP4 (`sbr.m4a`): the ASC advertises SBR (AOT 5), so
    /// the decoder reports the 2× output rate (44100) *up front* — before any
    /// frame is decoded — and produces 2048 samples/channel of non-silent audio.
    #[test]
    fn sbr_m4a_explicit_decodes_2x_non_silent() {
        let m4a = include_bytes!("../../../testdata/sbr.m4a");
        let (pcm, rate, channels) = decode_all_open(m4a);
        assert_eq!(channels, 2);
        assert_eq!(rate, 44_100, "explicit SBR core 22050 → output 44100");
        assert!(!pcm.is_empty());
        assert_eq!(
            pcm.len() % (2048 * 2),
            0,
            "output length {} is not a whole number of 2048/ch stereo frames",
            pcm.len()
        );
        assert!(
            pcm.iter().any(|&s| s != 0),
            "explicit SBR output all-silent"
        );
    }

    /// Smoke test (task brief): the HE-AAC ADTS fixture decodes end-to-end with
    /// no panic, SBR activates (output rate doubles to 2×core = 44100), every
    /// frame yields 2048 samples/channel, and the reconstructed audio is
    /// non-silent and finite. Structural only — Task 12 calibrates SNR.
    #[test]
    fn sbr_adts_smoke_decodes_2x_non_silent() {
        let aac = include_bytes!("../../../testdata/sbr.aac");
        let (pcm, rate, channels) = decode_all_adts(aac);

        // SBR discovered implicitly from the fill element → 2× output rate.
        assert_eq!(channels, 2, "sbr.aac is stereo (CPE)");
        assert_eq!(rate, 44_100, "SBR must double the 22050 core to 44100");

        // Each SBR frame is 2048 samples/channel = 4096 interleaved (stereo);
        // the whole decode is a whole number of such frames.
        assert!(!pcm.is_empty(), "no audio decoded");
        assert_eq!(
            pcm.len() % (2048 * 2),
            0,
            "output length {} is not a whole number of 2048/ch stereo frames",
            pcm.len()
        );
        let frames = pcm.len() / (2048 * 2);
        assert!(frames >= 10, "decoded only {frames} SBR frames, want >= 10");

        // Non-silent + finite (i32 samples in the i16 output range, no NaN —
        // `round_i16` maps NaN to 0 and saturates, so any out-of-range value
        // would be a real bug).
        let mut any_nonzero = false;
        let mut peak = 0i32;
        for &s in &pcm {
            assert!(
                (-32768..=32767).contains(&s),
                "sample {s} outside i16 range (NaN/overflow leaked through)"
            );
            if s != 0 {
                any_nonzero = true;
            }
            peak = peak.max(s.abs());
        }
        assert!(any_nonzero, "SBR decode produced all-silent output");
        assert!(
            peak > 8,
            "SBR decode output implausibly quiet (peak {peak})"
        );
    }

    /// Backward-compatibility: a pure AAC-LC ADTS stream (no `EXT_SBR_DATA`
    /// fill element) decodes exactly as before — SBR never activates, the rate
    /// stays the core rate (NOT doubled), and every frame stays 1024
    /// samples/channel. This is the "SBR is purely additive; the LC-only path
    /// stays byte-identical" invariant: `decode_fil` now inspects the extension
    /// type, but consumes the same `count·8` bits as before for a non-SBR
    /// extension and leaves `frame.sbr == None`, so the shared LC `decode_rdb`
    /// path is unchanged. (The semantic byte-for-byte match vs ffmpeg is the
    /// `tests/decode.rs` segmental-SNR gate; this pins the structural shape.)
    #[test]
    fn lc_only_adts_stays_unchanged_no_spurious_sbr() {
        let aac = include_bytes!("../../../testdata/aac.aac");
        let (pcm, rate, channels) = decode_all_adts(aac);
        assert_eq!(channels, 2);
        // If SBR had spuriously activated, the rate would double to 88200 and
        // the length would be 2048/ch/frame.
        assert_eq!(rate, 44_100, "LC stream must NOT double its rate (no SBR)");
        assert_eq!(
            pcm.len(),
            47_104,
            "LC decode length changed — 23 frames × 1024/ch × 2ch expected"
        );
        assert!(pcm.iter().any(|&s| s != 0), "LC decode was all-silent");
    }
}

#[cfg(test)]
mod seek_tests {
    use super::AacDecoder;
    use crate::decoder::Decoder;
    use crate::demux::adts::AdtsReader;
    use crate::demux::mp4::Mp4Reader;
    use crate::io::{SliceSource, Source};

    /// AAC decoders (esp. HE-AAC's ~250 KiB SBR state) overflow the default
    /// test-thread stack; run the whole decode on a generous one (as the
    /// integration suite and the CLI loader do).
    fn on_big_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
        std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(16 * 1024 * 1024)
                .spawn_scoped(s, f)
                .unwrap()
                .join()
                .unwrap()
        })
    }

    fn drain<S: Source>(d: &mut AacDecoder<S>) -> Vec<i32> {
        let mut out = Vec::new();
        let mut buf = [0i32; 4096];
        loop {
            let n = d.decode_into(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    fn full_mp4(bytes: &'static [u8]) -> Vec<i32> {
        on_big_stack(|| {
            let mut d =
                AacDecoder::new_mp4(Mp4Reader::new(SliceSource::new(bytes)).unwrap()).unwrap();
            drain(&mut d)
        })
    }

    fn seek_tail_mp4(bytes: &'static [u8], target: u64) -> (u64, Vec<i32>) {
        on_big_stack(move || {
            let mut d =
                AacDecoder::new_mp4(Mp4Reader::new(SliceSource::new(bytes)).unwrap()).unwrap();
            let landed = d.seek_to(target).unwrap();
            (landed, drain(&mut d))
        })
    }

    fn full_adts(bytes: &'static [u8]) -> Vec<i32> {
        on_big_stack(|| {
            let mut d =
                AacDecoder::new_adts(AdtsReader::new(SliceSource::new(bytes)).unwrap()).unwrap();
            drain(&mut d)
        })
    }

    fn seek_tail_adts(bytes: &'static [u8], target: u64) -> (u64, Vec<i32>) {
        on_big_stack(move || {
            let mut d =
                AacDecoder::new_adts(AdtsReader::new(SliceSource::new(bytes)).unwrap()).unwrap();
            let landed = d.seek_to(target).unwrap();
            (landed, drain(&mut d))
        })
    }

    /// AAC-LC: a 12-frame pre-roll fully restores the 1-frame MDCT overlap, so
    /// the post-seek tail is **bit-identical** to decoding from the start and
    /// dropping the head — at frame boundaries and interiors alike.
    #[test]
    fn aac_mp4_lc_seek_bit_exact() {
        const CH: usize = 2;
        let full = full_mp4(include_bytes!("../../../testdata/aac.m4a"));
        let pc = (full.len() / CH) as u64;
        for &t in &[0u64, 1, 1024, 1500, pc / 3, pc / 2, (3 * pc) / 4, pc - 1] {
            let (landed, tail) = seek_tail_mp4(include_bytes!("../../../testdata/aac.m4a"), t);
            assert_eq!(landed, t, "LC seek landing");
            let from = t as usize * CH;
            assert_eq!(tail.len(), full.len() - from, "LC tail length at {t}");
            assert_eq!(tail, &full[from..], "LC tail bit-exact at {t}");
        }
    }

    #[test]
    fn aac_mp4_lc_seek_to_zero_matches_fresh() {
        let full = full_mp4(include_bytes!("../../../testdata/aac.m4a"));
        let (_, tail) = seek_tail_mp4(include_bytes!("../../../testdata/aac.m4a"), 0);
        assert_eq!(tail, full, "LC seek-to-0 == fresh decode");
    }

    #[test]
    fn aac_mp4_lc_seek_beyond_end_clamps() {
        const CH: usize = 2;
        let full = full_mp4(include_bytes!("../../../testdata/aac.m4a"));
        let pc = (full.len() / CH) as u64;
        let (landed, _) = seek_tail_mp4(include_bytes!("../../../testdata/aac.m4a"), 10_000_000);
        assert!(
            landed <= pc + 1024,
            "clamps near the declared length: {landed}"
        );
    }

    /// HE-AAC (SBR, dual-rate): seeking decodes from the start so the frame-0
    /// SBR header + delta-time envelopes are always seen, making the tail
    /// bit-identical to the from-start decode (the output position is
    /// sample-accurate at the dual rate).
    #[test]
    fn aac_mp4_sbr_seek_bit_exact() {
        const CH: usize = 2;
        let full = full_mp4(include_bytes!("../../../testdata/sbr.m4a"));
        let pc = (full.len() / CH) as u64;
        for &t in &[pc / 4, pc / 2, (3 * pc) / 4] {
            let (landed, tail) = seek_tail_mp4(include_bytes!("../../../testdata/sbr.m4a"), t);
            assert_eq!(landed, t, "SBR seek landing (sample-accurate output pos)");
            let from = t as usize * CH;
            assert_eq!(tail.len(), full.len() - from, "SBR tail length at {t}");
            assert_eq!(tail, &full[from..], "HE-AAC SBR tail bit-exact at {t}");
        }
    }

    /// ADTS LC decode-discards from the start (no index) → bit-exact tail.
    #[test]
    fn aac_adts_lc_seek_bit_exact() {
        const CH: usize = 2;
        let full = full_adts(include_bytes!("../../../testdata/aac.aac"));
        let pc = (full.len() / CH) as u64;
        for &t in &[0u64, 1024, pc / 2, pc - 1] {
            let (landed, tail) = seek_tail_adts(include_bytes!("../../../testdata/aac.aac"), t);
            assert_eq!(landed, t, "ADTS seek landing");
            let from = t as usize * CH;
            assert_eq!(tail, &full[from..], "ADTS LC tail bit-exact at {t}");
        }
    }

    /// ADTS HE-AAC decode-discards from the start too → also bit-exact (no
    /// reconvergence gap, unlike the sample-table pre-roll path).
    #[test]
    fn aac_adts_sbr_seek_bit_exact() {
        const CH: usize = 2;
        let full = full_adts(include_bytes!("../../../testdata/sbr.aac"));
        let pc = (full.len() / CH) as u64;
        for &t in &[pc / 4, pc / 2] {
            let (landed, tail) = seek_tail_adts(include_bytes!("../../../testdata/sbr.aac"), t);
            assert_eq!(landed, t, "ADTS SBR seek landing");
            let from = t as usize * CH;
            assert_eq!(tail, &full[from..], "ADTS SBR tail bit-exact at {t}");
        }
    }
}
