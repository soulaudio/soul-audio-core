//! FLAC decoder = [`FlacDemuxer`](crate::demux::FlacDemuxer) (container) + codec.
//!
//! The demuxer parses metadata into a real [`StreamInfo`] and positions the
//! source at the first audio frame; this module implements the FLAC **codec** —
//! an MSB-first bit reader over the raw frame bytes, frame-header parsing, the
//! four subframe kinds (CONSTANT / VERBATIM / FIXED / LPC), Rice-coded residual,
//! inter-channel decorrelation, and cross-call streaming of decoded blocks.
//!
//! Output matches the WAV codec's convention: samples keep their **true** bit
//! depth widened into `i32` (a 16-bit value stays in `[-32768, 32767]`);
//! [`StreamInfo::format`] carries the depth. No rescaling.
//!
//! Frame CRCs are consumed for positioning and, when
//! [`set_validate_crc`](FlacDecoder::set_validate_crc) is enabled, verified
//! (header CRC-8 + frame CRC-16) — a mismatch fails with `Malformed`. Off by
//! default (lenient, matching most players).

use crate::decoder::{DecodeError, Decoder, secs_to_samples};
use crate::demux::{Demuxer, FlacDemuxer};
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// Maximum decoded block size (samples per channel) the inline buffer supports.
/// 4608 covers every standard-block-size encoder (the subset limit at ≤48 kHz);
/// larger blocks are declined with [`DecodeError::Unsupported`].
const MAX_BLOCK: usize = 4608;
/// Maximum channels the inline decode buffer supports (up to 7.1 surround).
/// FLAC's inter-channel decorrelation is stereo-only, so >2 channels are always
/// coded independently and decode with no cross-channel dependency.
///
/// **RAM cost:** the `channels × MAX_BLOCK × 4`-byte plane buffer must persist
/// between `decode_into` calls, and it sizes both `FlacDecoder` and — via the
/// `Wav`/`Flac`/`Mp3` enum — [`AnyDecoder`](crate::dispatch::AnyDecoder). At the
/// default it is `8 × 4608 × 4 ≈ 147 KiB`; lower this const to trade channel
/// support for footprint on RAM-constrained targets.
const MAX_CHANNELS: usize = 8;
/// Maximum predictor order (LPC tops out at 32).
const MAX_ORDER: usize = 32;
/// Raw-byte refill buffer size for the bit reader.
const BYTE_BUF: usize = 4096;

/// The 14-bit FLAC frame sync code.
const SYNC_CODE: u32 = 0x3FFE;

/// FLAC decoder over a byte [`Source`]. Decodes frames to interleaved `i32` PCM.
pub struct FlacDecoder<S> {
    demux: FlacDemuxer<S>,
    channels: u16,
    /// Stream bit depth (from STREAMINFO); the frame's declared depth normally
    /// matches this and is used when the frame header defers to STREAMINFO.
    bits: u8,
    total_samples: Option<u64>,
    decoded_per_ch: u64,

    // --- MSB-first bit reader over the raw audio byte stream ---
    bytes: [u8; BYTE_BUF],
    byte_len: usize,
    byte_cur: usize,
    /// Right-aligned bit accumulator holding the `acc_bits` unconsumed low bits.
    acc: u64,
    acc_bits: u32,

    // --- current decoded block (channel-planar), streamed to the caller ---
    planes: [[i32; MAX_BLOCK]; MAX_CHANNELS],
    /// Interleaved samples available in the current block (`block_len × channels`).
    block_total: usize,
    /// Interleaved emit cursor within the current block.
    emit_i: usize,

    // --- optional per-frame CRC verification ---
    /// When set, mismatched header CRC-8 / frame CRC-16 fail with `Malformed`.
    validate_crc: bool,
    /// Running CRC-8 over the current frame header (poly 0x07).
    crc8: u8,
    /// Running CRC-16 over the current frame (poly 0x8005).
    crc16: u16,
}

/// FLAC frame-header CRC-8 (poly `x⁸+x²+x+1` = 0x07, init 0, MSB-first).
fn crc8_update(mut crc: u8, byte: u8) -> u8 {
    crc ^= byte;
    for _ in 0..8 {
        crc = if crc & 0x80 != 0 {
            (crc << 1) ^ 0x07
        } else {
            crc << 1
        };
    }
    crc
}

/// FLAC frame CRC-16 (poly `x¹⁶+x¹⁵+x²+1` = 0x8005, init 0, MSB-first).
fn crc16_update(mut crc: u16, byte: u8) -> u16 {
    crc ^= u16::from(byte) << 8;
    for _ in 0..8 {
        crc = if crc & 0x8000 != 0 {
            (crc << 1) ^ 0x8005
        } else {
            crc << 1
        };
    }
    crc
}

/// The fields of a parsed frame header the codec actually needs.
struct FrameHeader {
    /// Block size in samples per channel.
    block: usize,
    /// Channel-assignment code (0–7 independent, 8 L/S, 9 R/S, 10 M/S).
    ca: u32,
    /// Declared sample bit depth for this frame.
    frame_bps: u32,
}

impl<S: Source> FlacDecoder<S> {
    /// Parse the FLAC metadata and prepare to stream PCM.
    ///
    /// # Errors
    /// Propagates [`FlacDemuxer::new`] errors; [`DecodeError::Unsupported`] for a
    /// non-integer format, zero/oversize channel count, or a bit depth over 32.
    pub fn new(src: S) -> Result<Self, DecodeError> {
        let demux = FlacDemuxer::new(src)?;
        let info = demux.stream_info();
        let bits = match info.format {
            SampleFormat::Int { bits } => bits,
            _ => return Err(DecodeError::Unsupported),
        };
        if info.channels == 0 || info.channels as usize > MAX_CHANNELS || bits == 0 || bits > 32 {
            return Err(DecodeError::Unsupported);
        }
        let total_samples = demux.total_samples();
        Ok(Self {
            demux,
            channels: info.channels,
            bits,
            total_samples,
            decoded_per_ch: 0,
            bytes: [0; BYTE_BUF],
            byte_len: 0,
            byte_cur: 0,
            acc: 0,
            acc_bits: 0,
            planes: [[0; MAX_BLOCK]; MAX_CHANNELS],
            block_total: 0,
            emit_i: 0,
            validate_crc: false,
            crc8: 0,
            crc16: 0,
        })
    }

    /// Enable per-frame CRC verification. When on, a header CRC-8 or frame
    /// CRC-16 mismatch fails decoding with [`DecodeError::Malformed`] (corruption
    /// detection). Off by default — lenient decoding plays through minor errors,
    /// matching most players.
    pub fn set_validate_crc(&mut self, on: bool) {
        self.validate_crc = on;
    }

    // -- byte layer -----------------------------------------------------------

    /// Pull the next raw audio byte, refilling from the demuxer as needed.
    /// [`DecodeError::UnexpectedEof`] if the stream ends mid-value.
    fn pull_byte(&mut self) -> Result<u8, DecodeError> {
        if self.byte_cur >= self.byte_len {
            self.byte_cur = 0;
            self.byte_len = self.demux.read_audio(&mut self.bytes)?;
            if self.byte_len == 0 {
                return Err(DecodeError::UnexpectedEof);
            }
        }
        let b = self.bytes[self.byte_cur];
        self.byte_cur += 1;
        if self.validate_crc {
            // Read-ahead is < 1 byte (acc holds ≤ 7 leftover bits), so every
            // pulled byte is frame content up to the CRC fields, which are read
            // (and snapshotted-before) explicitly.
            self.crc8 = crc8_update(self.crc8, b);
            self.crc16 = crc16_update(self.crc16, b);
        }
        Ok(b)
    }

    /// Whether at least one more bit/byte is available at a frame boundary
    /// (used to detect clean end-of-stream between frames).
    fn frame_available(&mut self) -> Result<bool, DecodeError> {
        if self.acc_bits > 0 || self.byte_cur < self.byte_len {
            return Ok(true);
        }
        self.byte_cur = 0;
        self.byte_len = self.demux.read_audio(&mut self.bytes)?;
        Ok(self.byte_len > 0)
    }

    // -- bit layer (MSB-first) -----------------------------------------------

    /// Read `n` (≤ 32) bits, MSB-first, as an unsigned value.
    fn read_bits(&mut self, n: u32) -> Result<u32, DecodeError> {
        if n == 0 {
            return Ok(0);
        }
        if n > 32 {
            return Err(DecodeError::Unsupported);
        }
        while self.acc_bits < n {
            let b = self.pull_byte()?;
            self.acc = (self.acc << 8) | u64::from(b);
            self.acc_bits += 8;
        }
        self.acc_bits -= n;
        let v = ((self.acc >> self.acc_bits) & ((1u64 << n) - 1)) as u32;
        self.acc &= (1u64 << self.acc_bits) - 1;
        Ok(v)
    }

    /// Read `n` (≤ 32) bits as a two's-complement signed value.
    fn read_signed(&mut self, n: u32) -> Result<i32, DecodeError> {
        if n == 0 {
            return Ok(0);
        }
        let u = self.read_bits(n)?;
        let shift = 32 - n;
        Ok(((u << shift) as i32) >> shift)
    }

    /// Read a unary-coded count: the number of `0` bits before the next `1`
    /// (the terminating `1` is consumed).
    fn read_unary(&mut self) -> Result<u32, DecodeError> {
        let mut count = 0;
        while self.read_bits(1)? == 0 {
            count += 1;
        }
        Ok(count)
    }

    /// Read (and discard the value of) a UTF-8-coded frame/sample number, so the
    /// bitstream is positioned past it. Returns the decoded number.
    fn read_utf8(&mut self) -> Result<u64, DecodeError> {
        let b0 = self.read_bits(8)?;
        if b0 & 0x80 == 0 {
            return Ok(u64::from(b0));
        }
        let mut len = 0u32;
        let mut m = 0x80u32;
        while b0 & m != 0 {
            len += 1;
            m >>= 1;
        }
        if !(2..=7).contains(&len) {
            return Err(DecodeError::Malformed);
        }
        let data_mask = if len >= 7 { 0 } else { 0xFFu32 >> (len + 1) };
        let mut val = u64::from(b0 & data_mask);
        for _ in 0..(len - 1) {
            let c = self.read_bits(8)?;
            if c & 0xC0 != 0x80 {
                return Err(DecodeError::Malformed);
            }
            val = (val << 6) | u64::from(c & 0x3F);
        }
        Ok(val)
    }

    /// Discard bits up to the next byte boundary (frame footer alignment).
    fn align_to_byte(&mut self) {
        let drop = self.acc_bits % 8;
        self.acc_bits -= drop;
        self.acc &= (1u64 << self.acc_bits) - 1;
    }

    // -- frame / subframe decode ---------------------------------------------

    /// Decode the next frame into `self.planes`, returning `Ok(false)` at a clean
    /// end of stream.
    fn decode_next_frame(&mut self) -> Result<bool, DecodeError> {
        if let Some(total) = self.total_samples {
            if self.decoded_per_ch >= total {
                return Ok(false);
            }
        }
        if !self.frame_available()? {
            return Ok(false);
        }

        // The frame is byte-aligned; start CRC accumulation from its first byte.
        self.crc8 = 0;
        self.crc16 = 0;
        let h = self.read_frame_header()?;
        if h.block == 0 || h.block > MAX_BLOCK {
            return Err(DecodeError::Unsupported);
        }
        let nchannels = match h.ca {
            0..=7 => (h.ca + 1) as usize,
            8..=10 => 2,
            _ => return Err(DecodeError::Unsupported),
        };
        if nchannels != self.channels as usize {
            return Err(DecodeError::Malformed);
        }

        for c in 0..nchannels {
            // The "side" channel of a stereo decorrelation carries one extra bit.
            let cbps = match h.ca {
                8 | 10 if c == 1 => h.frame_bps + 1,
                9 if c == 0 => h.frame_bps + 1,
                _ => h.frame_bps,
            };
            if cbps == 0 || cbps > 32 {
                return Err(DecodeError::Unsupported);
            }
            self.decode_subframe(c, cbps, h.block)?;
        }

        self.align_to_byte();
        let computed16 = self.crc16; // CRC-16 covers the whole frame up to here
        let stored16 = self.read_bits(16)? as u16;
        if self.validate_crc && computed16 != stored16 {
            return Err(DecodeError::Malformed);
        }

        self.apply_decorrelation(h.ca, h.block);

        // Trim the final frame to the declared total (last FLAC frame is exact,
        // but guard against a padded one).
        let mut emit = h.block;
        if let Some(total) = self.total_samples {
            let remaining = total.saturating_sub(self.decoded_per_ch);
            if emit as u64 > remaining {
                emit = remaining as usize;
            }
        }
        self.decoded_per_ch += emit as u64;
        self.block_total = emit * self.channels as usize;
        Ok(true)
    }

    /// Parse the byte-aligned frame header (sync through CRC-8).
    fn read_frame_header(&mut self) -> Result<FrameHeader, DecodeError> {
        if self.read_bits(14)? != SYNC_CODE {
            return Err(DecodeError::Malformed);
        }
        let _reserved = self.read_bits(1)?; // mandatory 0
        let _blocking = self.read_bits(1)?; // 0 = fixed, 1 = variable block size
        let bsc = self.read_bits(4)?; // block size code
        let src = self.read_bits(4)?; // sample rate code
        let ca = self.read_bits(4)?; // channel assignment
        let ssc = self.read_bits(3)?; // sample size code
        let _reserved2 = self.read_bits(1)?;
        let _num = self.read_utf8()?; // frame or sample number

        let block: usize = match bsc {
            0 => return Err(DecodeError::Malformed),
            1 => 192,
            2..=5 => 576usize << (bsc - 2),
            6 => self.read_bits(8)? as usize + 1,
            7 => self.read_bits(16)? as usize + 1,
            _ => 256usize << (bsc - 8), // 8..=15
        };

        // Consume any explicit sample-rate bytes (value unused; STREAMINFO wins).
        match src {
            12 => {
                self.read_bits(8)?;
            }
            13 | 14 => {
                self.read_bits(16)?;
            }
            15 => return Err(DecodeError::Malformed),
            _ => {}
        }

        let frame_bps = match ssc {
            0 => u32::from(self.bits),
            1 => 8,
            2 => 12,
            4 => 16,
            5 => 20,
            6 => 24,
            7 => 32,
            _ => return Err(DecodeError::Unsupported), // 3 = reserved
        };

        let computed8 = self.crc8; // CRC-8 covers the header up to (not incl.) here
        let stored8 = self.read_bits(8)? as u8;
        if self.validate_crc && computed8 != stored8 {
            return Err(DecodeError::Malformed);
        }

        Ok(FrameHeader {
            block,
            ca,
            frame_bps,
        })
    }

    /// Decode one subframe (channel `ci`, `bps` bits per sample) into
    /// `self.planes[ci][..block]`.
    fn decode_subframe(&mut self, ci: usize, bps: u32, block: usize) -> Result<(), DecodeError> {
        if self.read_bits(1)? != 0 {
            return Err(DecodeError::Malformed); // mandatory leading 0
        }
        let stype = self.read_bits(6)?;
        let wasted = if self.read_bits(1)? == 1 {
            self.read_unary()? + 1
        } else {
            0
        };
        if wasted >= bps {
            return Err(DecodeError::Malformed);
        }
        let sbps = bps - wasted;

        match stype {
            0 => self.decode_constant(ci, sbps, block)?,
            1 => self.decode_verbatim(ci, sbps, block)?,
            t if t & 0b111_000 == 0b001_000 => {
                let order = (t & 0b000_111) as usize;
                if order > 4 {
                    return Err(DecodeError::Malformed);
                }
                self.read_warmup(ci, sbps, order)?;
                self.decode_residual(ci, order, block)?;
                Self::restore_fixed(&mut self.planes[ci], order, block);
            }
            t if t & 0b100_000 != 0 => {
                let order = (t & 0b011_111) as usize + 1;
                self.read_warmup(ci, sbps, order)?;
                let prec_m1 = self.read_bits(4)?;
                if prec_m1 == 0b1111 {
                    return Err(DecodeError::Malformed); // invalid precision
                }
                let precision = prec_m1 + 1;
                let shift = self.read_signed(5)?;
                if shift < 0 {
                    return Err(DecodeError::Unsupported);
                }
                let mut coefs = [0i32; MAX_ORDER];
                for slot in coefs.iter_mut().take(order) {
                    *slot = self.read_signed(precision)?;
                }
                self.decode_residual(ci, order, block)?;
                Self::restore_lpc(&mut self.planes[ci], &coefs, order, block, shift as u32);
            }
            _ => return Err(DecodeError::Unsupported), // reserved subframe type
        }

        if wasted > 0 {
            for slot in self.planes[ci][..block].iter_mut() {
                *slot <<= wasted;
            }
        }
        Ok(())
    }

    /// CONSTANT subframe: one value repeated across the block.
    fn decode_constant(&mut self, ci: usize, bps: u32, block: usize) -> Result<(), DecodeError> {
        let v = self.read_signed(bps)?;
        self.planes[ci][..block].fill(v);
        Ok(())
    }

    /// VERBATIM subframe: raw sample values.
    fn decode_verbatim(&mut self, ci: usize, bps: u32, block: usize) -> Result<(), DecodeError> {
        let mut i = 0;
        while i < block {
            let v = self.read_signed(bps)?;
            self.planes[ci][i] = v;
            i += 1;
        }
        Ok(())
    }

    /// Read `order` warm-up samples into the head of the plane.
    fn read_warmup(&mut self, ci: usize, bps: u32, order: usize) -> Result<(), DecodeError> {
        let mut i = 0;
        while i < order {
            let v = self.read_signed(bps)?;
            self.planes[ci][i] = v;
            i += 1;
        }
        Ok(())
    }

    /// Decode the Rice-coded residual into `self.planes[ci][order..block]`.
    fn decode_residual(
        &mut self,
        ci: usize,
        order: usize,
        block: usize,
    ) -> Result<(), DecodeError> {
        let method = self.read_bits(2)?;
        if method >= 2 {
            return Err(DecodeError::Unsupported); // reserved coding method
        }
        let param_bits = if method == 0 { 4 } else { 5 };
        let escape = if method == 0 { 0xF } else { 0x1F };

        let po = self.read_bits(4)? as usize;
        let partitions = 1usize << po;
        if block % partitions != 0 {
            return Err(DecodeError::Malformed);
        }
        let part_len = block >> po;
        if part_len < order {
            return Err(DecodeError::Malformed);
        }

        let mut idx = order;
        for p in 0..partitions {
            let count = if p == 0 { part_len - order } else { part_len };
            let param = self.read_bits(param_bits)?;
            if param == escape {
                let n = self.read_bits(5)?;
                for _ in 0..count {
                    let v = self.read_signed(n)?;
                    self.planes[ci][idx] = v;
                    idx += 1;
                }
            } else {
                let k = param;
                for _ in 0..count {
                    let q = self.read_unary()?;
                    let r = self.read_bits(k)?;
                    let uval = (u64::from(q) << k) | u64::from(r);
                    let v = ((uval >> 1) as i64) ^ -((uval & 1) as i64);
                    self.planes[ci][idx] = v as i32;
                    idx += 1;
                }
            }
        }
        Ok(())
    }

    /// Restore a FIXED subframe in place (residual → samples), using the standard
    /// fixed polynomial predictor of the given order.
    fn restore_fixed(plane: &mut [i32; MAX_BLOCK], order: usize, block: usize) {
        match order {
            0 => {}
            1 => {
                for i in 1..block {
                    plane[i] += plane[i - 1];
                }
            }
            2 => {
                for i in 2..block {
                    let p = 2 * i64::from(plane[i - 1]) - i64::from(plane[i - 2]);
                    plane[i] = (i64::from(plane[i]) + p) as i32;
                }
            }
            3 => {
                for i in 3..block {
                    let p = 3 * i64::from(plane[i - 1]) - 3 * i64::from(plane[i - 2])
                        + i64::from(plane[i - 3]);
                    plane[i] = (i64::from(plane[i]) + p) as i32;
                }
            }
            _ => {
                for i in 4..block {
                    let p = 4 * i64::from(plane[i - 1]) - 6 * i64::from(plane[i - 2])
                        + 4 * i64::from(plane[i - 3])
                        - i64::from(plane[i - 4]);
                    plane[i] = (i64::from(plane[i]) + p) as i32;
                }
            }
        }
    }

    /// Restore an LPC subframe in place: `s[i] = residual[i] + (Σ c[j]·s[i-1-j]) >> shift`.
    fn restore_lpc(
        plane: &mut [i32; MAX_BLOCK],
        coefs: &[i32; MAX_ORDER],
        order: usize,
        block: usize,
        shift: u32,
    ) {
        for i in order..block {
            let mut acc: i64 = 0;
            for (j, &c) in coefs.iter().take(order).enumerate() {
                acc += i64::from(c) * i64::from(plane[i - 1 - j]);
            }
            plane[i] = (i64::from(plane[i]) + (acc >> shift)) as i32;
        }
    }

    /// Undo inter-channel decorrelation for the stereo assignment modes.
    fn apply_decorrelation(&mut self, ca: u32, block: usize) {
        match ca {
            8 => {
                // Left/side: ch0 = left, ch1 = side (= left − right) ⇒ right = left − side.
                for i in 0..block {
                    let l = i64::from(self.planes[0][i]);
                    let s = i64::from(self.planes[1][i]);
                    self.planes[1][i] = (l - s) as i32;
                }
            }
            9 => {
                // Right/side: ch0 = side (= left − right), ch1 = right ⇒ left = right + side.
                for i in 0..block {
                    let s = i64::from(self.planes[0][i]);
                    let r = i64::from(self.planes[1][i]);
                    self.planes[0][i] = (r + s) as i32;
                }
            }
            10 => {
                // Mid/side: ch0 = mid = (left+right)>>1, ch1 = side = left − right.
                for i in 0..block {
                    let side = i64::from(self.planes[1][i]);
                    // Recover the LSB dropped by the mid's floor via the side's parity.
                    let mid = (i64::from(self.planes[0][i]) << 1) | (side & 1);
                    self.planes[0][i] = ((mid + side) >> 1) as i32;
                    self.planes[1][i] = ((mid - side) >> 1) as i32;
                }
            }
            _ => {} // independent channels: nothing to undo
        }
    }
}

impl<S: Source> FlacDecoder<S> {
    /// Current playback position, in per-channel samples from the start (the
    /// index of the next sample `decode_into` will return).
    #[must_use]
    pub fn position(&self) -> u64 {
        let ch = self.channels as u64;
        let unemitted = ((self.block_total - self.emit_i) as u64) / ch;
        self.decoded_per_ch - unemitted
    }

    /// Total per-channel frames from STREAMINFO, if declared (0 = unknown → `None`).
    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        self.total_samples
    }
}

impl<S: SeekSource> FlacDecoder<S> {
    /// Seek to per-channel `sample` (sample-accurate). Uses the SEEKTABLE to jump
    /// near, then decodes forward to the exact sample. Returns the sample landed
    /// on (clamped to the stream length when known).
    ///
    /// # Errors
    /// [`DecodeError::Io`] on a source-seek failure; propagates decode errors
    /// from the forward pass.
    pub fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        let target = self.total_samples.map_or(sample, |t| sample.min(t));

        // Jump to the best SEEKTABLE point at or before the target (or the start).
        let (base_sample, byte_off) = self.demux.seek_point_before(target);
        self.demux.seek_audio(byte_off)?;

        // Full reset — drop every byte/bit/plane of the pre-seek position so no
        // stale sample can leak into the post-seek stream.
        self.byte_len = 0;
        self.byte_cur = 0;
        self.acc = 0;
        self.acc_bits = 0;
        self.block_total = 0;
        self.emit_i = 0;
        self.decoded_per_ch = base_sample;

        // Decode-and-discard whole frames until the frame containing `target`.
        loop {
            let frame_start = self.decoded_per_ch;
            if !self.decode_next_frame()? {
                break; // reached end of stream (target == total): next read is 0
            }
            if target < self.decoded_per_ch {
                // This frame covers `target`; position the emit cursor at it.
                self.emit_i = ((target - frame_start) as usize) * self.channels as usize;
                break;
            }
            // Otherwise discard the whole frame and continue.
            self.emit_i = self.block_total;
        }
        Ok(target)
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to`]). Returns the
    /// per-channel sample landed on.
    ///
    /// # Errors
    /// Propagates [`Self::seek_to`] errors.
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        self.seek_to(secs_to_samples(secs, self.demux.stream_info().sample_rate))
    }
}

impl<S: Source> Decoder for FlacDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        self.demux.stream_info()
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let ch = self.channels as usize;
        let mut w = 0;
        while w < out.len() {
            if self.emit_i >= self.block_total {
                if !self.decode_next_frame()? {
                    break;
                }
                self.emit_i = 0;
                if self.block_total == 0 {
                    break;
                }
            }
            let frame = self.emit_i / ch;
            let c = self.emit_i % ch;
            out[w] = self.planes[c][frame];
            self.emit_i += 1;
            w += 1;
        }
        Ok(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// MSB-first bit writer for hand-building FLAC frames in tests.
    struct BitWriter {
        bytes: Vec<u8>,
        cur: u8,
        nbits: u8,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                cur: 0,
                nbits: 0,
            }
        }

        /// Write the low `n` bits of `val`, MSB-first.
        fn bits(&mut self, val: u64, n: u32) {
            for i in (0..n).rev() {
                let bit = ((val >> i) & 1) as u8;
                self.cur = (self.cur << 1) | bit;
                self.nbits += 1;
                if self.nbits == 8 {
                    self.bytes.push(self.cur);
                    self.cur = 0;
                    self.nbits = 0;
                }
            }
        }

        /// Write a two's-complement signed value in `n` bits.
        fn signed(&mut self, val: i32, n: u32) {
            let mask = if n >= 32 { u64::MAX } else { (1u64 << n) - 1 };
            self.bits((val as i64 as u64) & mask, n);
        }

        /// Unary code: `q` zeros then a terminating `1`.
        fn unary(&mut self, q: u32) {
            for _ in 0..q {
                self.bits(0, 1);
            }
            self.bits(1, 1);
        }

        /// Rice-code one residual with parameter `k`.
        fn rice(&mut self, v: i32, k: u32) {
            let u = ((v << 1) ^ (v >> 31)) as u32; // zigzag fold
            self.unary(u >> k);
            if k > 0 {
                self.bits(u64::from(u & ((1 << k) - 1)), k);
            }
        }

        /// Pad with zero bits to the next byte boundary.
        fn align(&mut self) {
            if self.nbits > 0 {
                self.bits(0, u32::from(8 - self.nbits));
            }
        }

        fn into_bytes(mut self) -> Vec<u8> {
            self.align();
            self.bytes
        }
    }

    /// Write a byte-aligned frame header. `bsc = 7` ⇒ an explicit 16-bit block
    /// size follows, so any block length is expressible. `src`/`ssc` are 0 (defer
    /// to STREAMINFO). CRC-8 is written as 0 (the decoder does not validate it).
    fn write_header(bw: &mut BitWriter, block: usize, ca: u32) {
        bw.bits(u64::from(SYNC_CODE), 14);
        bw.bits(0, 1); // reserved
        bw.bits(0, 1); // fixed block size
        bw.bits(7, 4); // block size code = 7 (16-bit explicit)
        bw.bits(0, 4); // sample rate code = 0 (STREAMINFO)
        bw.bits(u64::from(ca), 4);
        bw.bits(0, 3); // sample size code = 0 (STREAMINFO)
        bw.bits(0, 1); // reserved
        bw.bits(0, 8); // UTF-8 frame number = 0
        bw.bits((block - 1) as u64, 16); // explicit block size
        bw.bits(0, 8); // CRC-8 (unchecked)
    }

    /// Append a frame footer (align + a zeroed CRC-16).
    fn write_footer(bw: &mut BitWriter) {
        bw.align();
        bw.bits(0, 16); // CRC-16 (unchecked)
    }

    /// Build a 34-byte STREAMINFO body carrying the given parameters.
    fn streaminfo(rate: u32, channels: u16, bits: u8, total: u64) -> [u8; 34] {
        let mut si = [0u8; 34];
        let v: u64 = (u64::from(rate) << 44)
            | ((u64::from(channels) - 1) << 41)
            | ((u64::from(bits) - 1) << 36)
            | (total & 0xF_FFFF_FFFF);
        si[10..18].copy_from_slice(&v.to_be_bytes());
        si
    }

    /// Wrap raw frame `audio` bytes in a minimal valid FLAC container.
    fn flac_file(rate: u32, channels: u16, bits: u8, total: u64, audio: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"fLaC");
        v.push(0x80); // last metadata block + STREAMINFO
        v.extend_from_slice(&[0, 0, 34]);
        v.extend_from_slice(&streaminfo(rate, channels, bits, total));
        v.extend_from_slice(audio);
        v
    }

    // -- CONSTANT -------------------------------------------------------------

    #[test]
    fn constant_subframe_mono() {
        let block = 8usize;
        let k = -12345i32;
        let mut bw = BitWriter::new();
        write_header(&mut bw, block, 0);
        bw.bits(0, 1); // subframe: leading 0
        bw.bits(0, 6); // type CONSTANT
        bw.bits(0, 1); // no wasted bits
        bw.signed(k, 16);
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, block as u64, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 16];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, block);
        assert!(out[..n].iter().all(|&s| s == k));
        assert_eq!(dec.decode_into(&mut out).unwrap(), 0); // EOF
    }

    // -- VERBATIM -------------------------------------------------------------

    #[test]
    fn verbatim_subframe_mono() {
        let samples = [0i32, 1, -1, 32767, -32768, 100, -100, 7];
        let mut bw = BitWriter::new();
        write_header(&mut bw, samples.len(), 0);
        bw.bits(0, 1);
        bw.bits(1, 6); // type VERBATIM
        bw.bits(0, 1);
        for &s in &samples {
            bw.signed(s, 16);
        }
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, samples.len() as u64, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 16];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &samples);
    }

    // -- FIXED ----------------------------------------------------------------

    /// Emit a FIXED subframe (single Rice partition, parameter `k`).
    fn fixed_subframe(bw: &mut BitWriter, order: usize, warmup: &[i32], resid: &[i32], k: u32) {
        bw.bits(0, 1);
        bw.bits(0b001_000 | order as u64, 6); // FIXED, order in low 3 bits
        bw.bits(0, 1); // no wasted bits
        for &w in warmup {
            bw.signed(w, 16);
        }
        bw.bits(0, 2); // residual method 0 (4-bit Rice)
        bw.bits(0, 4); // partition order 0
        bw.bits(u64::from(k), 4);
        for &r in resid {
            bw.rice(r, k);
        }
    }

    #[test]
    fn fixed_order0_is_raw_residual() {
        // Order 0: samples equal the residual directly.
        let resid = [5i32, -3, 20, -1, 0, 7];
        let mut bw = BitWriter::new();
        write_header(&mut bw, resid.len(), 0);
        fixed_subframe(&mut bw, 0, &[], &resid, 2);
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, resid.len() as u64, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &resid);
    }

    #[test]
    fn fixed_order1_restores_running_sum() {
        // s[0] = warm-up; s[i] = s[i-1] + resid[i].
        let warm = 10i32;
        let resid = [1i32, 2, -1, 3, -2];
        let expected = [10i32, 11, 13, 12, 15, 13];
        let mut bw = BitWriter::new();
        write_header(&mut bw, expected.len(), 0);
        fixed_subframe(&mut bw, 1, &[warm], &resid, 0);
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, expected.len() as u64, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &expected);
    }

    #[test]
    fn fixed_order2_restores_second_difference() {
        // s[i] = 2·s[i-1] − s[i-2] + resid[i]. Compute the expectation directly.
        let warm = [100i32, 102];
        let resid = [1i32, -2, 4, 0];
        let mut expected = [0i32; 6];
        expected[0] = warm[0];
        expected[1] = warm[1];
        for i in 2..6 {
            let pred = 2 * expected[i - 1] - expected[i - 2];
            expected[i] = pred + resid[i - 2];
        }

        let mut bw = BitWriter::new();
        write_header(&mut bw, 6, 0);
        fixed_subframe(&mut bw, 2, &warm, &resid, 3);
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, 6, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &expected);
    }

    #[test]
    fn residual_escape_code_reads_raw() {
        // Order-0 FIXED with an escape partition: residuals stored as raw n-bit.
        let resid = [3i32, -4, 63, -64];
        let mut bw = BitWriter::new();
        write_header(&mut bw, resid.len(), 0);
        bw.bits(0, 1);
        bw.bits(0b001_000, 6); // FIXED order 0
        bw.bits(0, 1);
        bw.bits(0, 2); // method 0
        bw.bits(0, 4); // partition order 0
        bw.bits(0xF, 4); // escape parameter
        bw.bits(7, 5); // 7 raw bits per residual
        for &r in &resid {
            bw.signed(r, 7);
        }
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, resid.len() as u64, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &resid);
    }

    #[test]
    fn fixed_order3_and_order4_restore() {
        // Exercise the two highest FIXED orders against a reference restoration
        // written with the standard fixed-predictor coefficients.
        for order in 3..=4usize {
            let warm = [5i32, 9, 2, 7][..order].to_vec();
            let resid = [1i32, -2, 3, -1, 2];
            let block = order + resid.len();
            let mut expected = vec![0i32; block];
            expected[..order].copy_from_slice(&warm);
            for i in order..block {
                let p = match order {
                    3 => 3 * expected[i - 1] - 3 * expected[i - 2] + expected[i - 3],
                    _ => {
                        4 * expected[i - 1] - 6 * expected[i - 2] + 4 * expected[i - 3]
                            - expected[i - 4]
                    }
                };
                expected[i] = p + resid[i - order];
            }

            let mut bw = BitWriter::new();
            write_header(&mut bw, block, 0);
            fixed_subframe(&mut bw, order, &warm, &resid, 3);
            write_footer(&mut bw);

            let bytes = flac_file(44_100, 1, 16, block as u64, &bw.into_bytes());
            let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
            let mut out = [0i32; 16];
            let n = dec.decode_into(&mut out).unwrap();
            assert_eq!(&out[..n], &expected[..]);
        }
    }

    #[test]
    fn rice_method1_five_bit_parameters() {
        // Residual coding method 1 uses 5-bit Rice parameters (vs method 0's 4-bit).
        let warm = 20i32;
        let resid = [3i32, -4, 5, -1, 2];
        let mut expected = [0i32; 6];
        expected[0] = warm;
        for i in 1..6 {
            expected[i] = expected[i - 1] + resid[i - 1];
        }
        let k = 3u32;

        let mut bw = BitWriter::new();
        write_header(&mut bw, 6, 0);
        bw.bits(0, 1);
        bw.bits(0b001_000 | 1, 6); // FIXED order 1
        bw.bits(0, 1);
        bw.signed(warm, 16);
        bw.bits(1, 2); // residual method 1 (5-bit parameters)
        bw.bits(0, 4); // partition order 0
        bw.bits(u64::from(k), 5); // 5-bit Rice parameter
        for &r in &resid {
            bw.rice(r, k);
        }
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, 6, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &expected);
    }

    #[test]
    fn multi_partition_residual() {
        // Order-1 FIXED, partition order 1 (two partitions) over an 8-sample block.
        let warm = 50i32;
        let resid = [2i32, -1, 3, 0, 1, -2, 4]; // 7 residuals (8 − order 1)
        let mut expected = [0i32; 8];
        expected[0] = warm;
        for i in 1..8 {
            expected[i] = expected[i - 1] + resid[i - 1];
        }

        let mut bw = BitWriter::new();
        write_header(&mut bw, 8, 0);
        bw.bits(0, 1);
        bw.bits(0b001_000 | 1, 6); // FIXED order 1
        bw.bits(0, 1);
        bw.signed(warm, 16);
        bw.bits(0, 2); // method 0
        bw.bits(1, 4); // partition order 1 → 2 partitions of 4
        // partition 0: 3 residuals (4 − order), k = 2
        bw.bits(2, 4);
        for &r in &resid[..3] {
            bw.rice(r, 2);
        }
        // partition 1: 4 residuals, k = 3
        bw.bits(3, 4);
        for &r in &resid[3..] {
            bw.rice(r, 3);
        }
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, 8, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &expected);
    }

    // -- LPC ------------------------------------------------------------------

    #[test]
    fn lpc_order1_restores() {
        // coef 256 at shift 8 ⇒ prediction = s[i-1] exactly.
        let warm = 1000i32;
        let resid = [1i32, 2, -3, 4];
        let expected = [1000i32, 1001, 1003, 1000, 1004];
        let mut bw = BitWriter::new();
        write_header(&mut bw, expected.len(), 0);
        bw.bits(0, 1);
        bw.bits(0b100_000, 6); // LPC order 1 (order-1 encoded)
        bw.bits(0, 1); // no wasted bits
        bw.signed(warm, 16);
        bw.bits(11, 4); // precision − 1 = 11 ⇒ 12-bit coefficients
        bw.bits(8, 5); // shift = 8
        bw.signed(256, 12); // coefficient
        bw.bits(0, 2); // residual method 0
        bw.bits(0, 4); // partition order 0
        bw.bits(0, 4); // Rice parameter k = 0
        for &r in &resid {
            bw.rice(r, 0);
        }
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, expected.len() as u64, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &expected);
    }

    #[test]
    fn lpc_order2_nontrivial_coefficients() {
        // Two coefficients with a real shift; expectation computed by the same
        // integer predictor the decoder uses.
        let warm = [200i32, 210];
        let coefs = [384i32, -128]; // Q7-ish
        let shift = 7u32;
        let resid = [3i32, -5, 2, 6, -1];
        let order = 2usize;
        let block = order + resid.len();
        let mut expected = [0i32; 7];
        expected[0] = warm[0];
        expected[1] = warm[1];
        for i in order..block {
            let mut acc: i64 = 0;
            for (j, &c) in coefs.iter().enumerate() {
                acc += i64::from(c) * i64::from(expected[i - 1 - j]);
            }
            expected[i] = (acc >> shift) as i32 + resid[i - order];
        }

        let mut bw = BitWriter::new();
        write_header(&mut bw, block, 0);
        bw.bits(0, 1);
        bw.bits(0b100_000 | (order as u64 - 1), 6); // LPC order 2
        bw.bits(0, 1);
        for &w in &warm {
            bw.signed(w, 16);
        }
        bw.bits(11, 4); // 12-bit coefficients
        bw.bits(u64::from(shift), 5);
        for &c in &coefs {
            bw.signed(c, 12);
        }
        bw.bits(0, 2);
        bw.bits(0, 4);
        bw.bits(1, 4); // k = 1
        for &r in &resid {
            bw.rice(r, 1);
        }
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, block as u64, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, block);
        assert_eq!(&out[..block], &expected[..block]);
    }

    // -- wasted bits ----------------------------------------------------------

    #[test]
    fn wasted_bits_are_shifted_back() {
        // CONSTANT subframe with 3 wasted bits: value read then left-shifted by 3.
        let base = -101i32;
        let block = 4usize;
        let mut bw = BitWriter::new();
        write_header(&mut bw, block, 0);
        bw.bits(0, 1);
        bw.bits(0, 6); // CONSTANT
        bw.bits(1, 1); // wasted-bits flag
        bw.unary(2); // 2 zeros + 1 ⇒ wasted = 3
        bw.signed(base, 13); // 16 − 3 effective bits
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, block as u64, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert!(out[..n].iter().all(|&s| s == base << 3));
    }

    // -- stereo decorrelation -------------------------------------------------

    /// Append two VERBATIM subframes carrying `ch0`/`ch1` at the given bit depths.
    fn verbatim_stereo(bw: &mut BitWriter, ch0: &[i32], bps0: u32, ch1: &[i32], bps1: u32) {
        bw.bits(0, 1);
        bw.bits(1, 6);
        bw.bits(0, 1);
        for &s in ch0 {
            bw.signed(s, bps0);
        }
        bw.bits(0, 1);
        bw.bits(1, 6);
        bw.bits(0, 1);
        for &s in ch1 {
            bw.signed(s, bps1);
        }
    }

    #[test]
    fn stereo_mid_side() {
        let left = [100i32, -50, 5, 3];
        let right = [98i32, -60, 2, -3];
        let mut mid = [0i32; 4];
        let mut side = [0i32; 4];
        for i in 0..4 {
            mid[i] = (left[i] + right[i]) >> 1;
            side[i] = left[i] - right[i];
        }

        let mut bw = BitWriter::new();
        write_header(&mut bw, 4, 10); // channel assignment 10 = mid/side
        verbatim_stereo(&mut bw, &mid, 16, &side, 17); // side carries an extra bit
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 2, 16, 4, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 8);
        // Interleaved L,R,L,R,...
        let expected = [
            left[0], right[0], left[1], right[1], left[2], right[2], left[3], right[3],
        ];
        assert_eq!(&out[..n], &expected);
    }

    #[test]
    fn stereo_left_side() {
        let left = [100i32, -50, 5];
        let right = [98i32, -60, 2];
        let mut side = [0i32; 3];
        for i in 0..3 {
            side[i] = left[i] - right[i];
        }

        let mut bw = BitWriter::new();
        write_header(&mut bw, 3, 8); // channel assignment 8 = left/side
        verbatim_stereo(&mut bw, &left, 16, &side, 17);
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 2, 16, 3, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        let expected = [left[0], right[0], left[1], right[1], left[2], right[2]];
        assert_eq!(&out[..n], &expected);
    }

    #[test]
    fn stereo_right_side() {
        let left = [400i32, -200, 33];
        let right = [398i32, -260, 30];
        let mut side = [0i32; 3];
        for i in 0..3 {
            side[i] = left[i] - right[i];
        }

        let mut bw = BitWriter::new();
        write_header(&mut bw, 3, 9); // channel assignment 9 = right/side
        verbatim_stereo(&mut bw, &side, 17, &right, 16); // ch0 = side, ch1 = right
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 2, 16, 3, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        let expected = [left[0], right[0], left[1], right[1], left[2], right[2]];
        assert_eq!(&out[..n], &expected);
    }

    #[test]
    fn independent_stereo() {
        let ch0 = [1i32, 2, 3];
        let ch1 = [-1i32, -2, -3];
        let mut bw = BitWriter::new();
        write_header(&mut bw, 3, 1); // channel assignment 1 = 2 independent channels
        verbatim_stereo(&mut bw, &ch0, 16, &ch1, 16);
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 2, 16, 3, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &[1, -1, 2, -2, 3, -3]);
    }

    // -- streaming ------------------------------------------------------------

    #[test]
    fn streams_a_block_across_small_calls() {
        let block = 8usize;
        let k = 777i32;
        let mut bw = BitWriter::new();
        write_header(&mut bw, block, 0);
        bw.bits(0, 1);
        bw.bits(0, 6); // CONSTANT
        bw.bits(0, 1);
        bw.signed(k, 16);
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, block as u64, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 3];
        let mut total = 0;
        loop {
            let n = dec.decode_into(&mut out).unwrap();
            if n == 0 {
                break;
            }
            assert!(out[..n].iter().all(|&s| s == k));
            total += n;
        }
        assert_eq!(total, block);
    }

    #[test]
    fn decodes_two_frames_until_eof_without_declared_total() {
        // total = 0 in STREAMINFO ⇒ decode until the source is exhausted.
        let mut bw = BitWriter::new();
        write_header(&mut bw, 4, 0);
        bw.bits(0, 1);
        bw.bits(0, 6);
        bw.bits(0, 1);
        bw.signed(11, 16); // frame 1: constant 11
        write_footer(&mut bw);
        write_header(&mut bw, 4, 0);
        bw.bits(0, 1);
        bw.bits(0, 6);
        bw.bits(0, 1);
        bw.signed(22, 16); // frame 2: constant 22
        write_footer(&mut bw);

        let bytes = flac_file(44_100, 1, 16, 0, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 16];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 8);
        assert_eq!(&out[..8], &[11, 11, 11, 11, 22, 22, 22, 22]);
        assert_eq!(dec.decode_into(&mut out).unwrap(), 0);
    }

    // -- end to end -----------------------------------------------------------

    #[test]
    fn full_tiny_flac_end_to_end() {
        // Build a complete FLAC (fLaC + STREAMINFO + one CONSTANT frame) and decode
        // it through the public API.
        let block = 6usize;
        let k = 2500i32;
        let mut bw = BitWriter::new();
        write_header(&mut bw, block, 0);
        bw.bits(0, 1);
        bw.bits(0, 6);
        bw.bits(0, 1);
        bw.signed(k, 16);
        write_footer(&mut bw);

        let bytes = flac_file(48_000, 1, 16, block as u64, &bw.into_bytes());
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(dec.stream_info().sample_rate, 48_000);
        assert_eq!(dec.stream_info().channels, 1);
        assert_eq!(dec.stream_info().format, SampleFormat::Int { bits: 16 });
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, block);
        assert!(out[..n].iter().all(|&s| s == k));
    }

    // -- seeking --------------------------------------------------------------

    /// Encode one VERBATIM mono frame carrying `samples` (16-bit).
    fn verbatim_frame(samples: &[i32]) -> Vec<u8> {
        let mut bw = BitWriter::new();
        write_header(&mut bw, samples.len(), 0);
        bw.bits(0, 1); // subframe leading 0
        bw.bits(1, 6); // VERBATIM
        bw.bits(0, 1); // no wasted bits
        for &s in samples {
            bw.signed(s, 16);
        }
        write_footer(&mut bw);
        bw.into_bytes()
    }

    /// A multi-frame mono FLAC of a counting ramp (`sample[i] == i`), split into
    /// `block`-sized VERBATIM frames, optionally carrying a SEEKTABLE with one
    /// point per frame. Returns `(bytes, ramp)`.
    fn ramp_flac(total: usize, block: usize, with_seektable: bool) -> (Vec<u8>, Vec<i32>) {
        let ramp: Vec<i32> = (0..total as i32).collect();
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut points: Vec<(u64, u64, u16)> = Vec::new(); // (sample, byte_off, count)
        let mut off = 0u64;
        let mut s = 0usize;
        while s < total {
            let n = block.min(total - s);
            let fr = verbatim_frame(&ramp[s..s + n]);
            points.push((s as u64, off, n as u16));
            off += fr.len() as u64;
            frames.push(fr);
            s += n;
        }

        let mut v = Vec::new();
        v.extend_from_slice(b"fLaC");
        v.push(if with_seektable { 0x00 } else { 0x80 }); // STREAMINFO (last iff no seektable)
        v.extend_from_slice(&[0, 0, 34]);
        v.extend_from_slice(&streaminfo(44_100, 1, 16, total as u64));
        if with_seektable {
            let body = points.len() * 18;
            v.push(0x83); // last + SEEKTABLE (type 3)
            v.extend_from_slice(&[(body >> 16) as u8, (body >> 8) as u8, body as u8]);
            for (sn, bo, fs) in &points {
                v.extend_from_slice(&sn.to_be_bytes());
                v.extend_from_slice(&bo.to_be_bytes());
                v.extend_from_slice(&fs.to_be_bytes());
            }
        }
        for fr in &frames {
            v.extend_from_slice(fr);
        }
        (v, ramp)
    }

    fn read_n(dec: &mut FlacDecoder<SliceSource<'_>>, n: usize) -> Vec<i32> {
        let mut out = vec![0i32; n];
        let got = dec.decode_into(&mut out).unwrap();
        out.truncate(got);
        out
    }

    fn drain(dec: &mut FlacDecoder<SliceSource<'_>>) -> Vec<i32> {
        let mut all = Vec::new();
        let mut buf = [0i32; 16];
        loop {
            let n = dec.decode_into(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buf[..n]);
        }
        all
    }

    #[test]
    fn seek_round_trip_with_seektable() {
        let (bytes, ramp) = ramp_flac(40, 8, true); // 5 frames
        for &n in &[0u64, 1, 8, 15, 24, 39] {
            let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
            assert_eq!(dec.seek_to(n).unwrap(), n);
            let got = read_n(&mut dec, 6);
            assert_eq!(
                got,
                &ramp[n as usize..(n as usize + got.len())],
                "seek({n})"
            );
        }
    }

    #[test]
    fn seek_round_trip_without_seektable() {
        // No SEEKTABLE → decode-forward fallback from the stream start.
        let (bytes, ramp) = ramp_flac(40, 8, false);
        for &n in &[0u64, 3, 8, 20, 39] {
            let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
            assert_eq!(dec.seek_to(n).unwrap(), n);
            let got = read_n(&mut dec, 5);
            assert_eq!(
                got,
                &ramp[n as usize..(n as usize + got.len())],
                "seek({n})"
            );
        }
    }

    #[test]
    fn seek_exhaustive_per_sample() {
        let (bytes, ramp) = ramp_flac(24, 4, true);
        for n in 0..24u64 {
            let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
            assert_eq!(dec.seek_to(n).unwrap(), n);
            assert_eq!(read_n(&mut dec, 1), &ramp[n as usize..n as usize + 1]);
        }
    }

    #[test]
    fn seek_to_zero_after_partial_matches_fresh() {
        let (bytes, ramp) = ramp_flac(40, 8, true);
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let _ = read_n(&mut dec, 13); // partial decode leaves internal state
        assert_eq!(dec.seek_to(0).unwrap(), 0);
        assert_eq!(drain(&mut dec), ramp); // no stale samples after seek(0)
    }

    #[test]
    fn seek_backward_and_repeated() {
        let (bytes, ramp) = ramp_flac(48, 8, true);
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let _ = read_n(&mut dec, 30);
        assert_eq!(dec.seek_to(4).unwrap(), 4); // backward
        assert_eq!(read_n(&mut dec, 3), &ramp[4..7]);
        dec.seek_to(40).unwrap(); // repeated seeks, only the last counts
        dec.seek_to(12).unwrap();
        assert_eq!(dec.seek_to(20).unwrap(), 20);
        assert_eq!(read_n(&mut dec, 2), &ramp[20..22]);
    }

    #[test]
    fn seek_beyond_end_reads_zero() {
        let (bytes, _) = ramp_flac(40, 8, true);
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(dec.seek_to(40).unwrap(), 40); // exact end → clamp
        assert_eq!(dec.position(), 40);
        assert_eq!(dec.decode_into(&mut [0i32; 8]).unwrap(), 0);
        assert_eq!(dec.seek_to(10_000).unwrap(), 40); // far beyond → clamp
        assert_eq!(dec.decode_into(&mut [0i32; 8]).unwrap(), 0);
    }

    #[test]
    fn seek_to_time_converts_seconds() {
        let (bytes, ramp) = ramp_flac(40, 8, true); // 44_100 Hz
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        // 0.0004 s × 44_100 = 17.64 → sample 18.
        assert_eq!(dec.seek_to_time(0.0004).unwrap(), 18);
        assert_eq!(read_n(&mut dec, 1), &ramp[18..19]);
    }

    #[test]
    fn position_tracks_seek_and_reads() {
        let (bytes, _) = ramp_flac(40, 8, true);
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(dec.position(), 0);
        dec.seek_to(17).unwrap();
        assert_eq!(dec.position(), 17); // before any read
        let _ = read_n(&mut dec, 5);
        assert_eq!(dec.position(), 22); // after reading 5
    }

    // -- CRC validation -------------------------------------------------------

    /// A VERBATIM mono frame with correct CRC-8 (header) and CRC-16 (frame).
    fn frame_with_valid_crc(samples: &[i16]) -> Vec<u8> {
        let mut bw = BitWriter::new();
        write_header(&mut bw, samples.len(), 0);
        bw.bits(0, 1);
        bw.bits(1, 6); // VERBATIM
        bw.bits(0, 1);
        for &s in samples {
            bw.signed(i32::from(s), 16);
        }
        write_footer(&mut bw);
        let mut frame = bw.into_bytes();
        // Header CRC-8 covers bytes[0..7] (sync..num..block16), stored at byte 7.
        let mut c8 = 0u8;
        for &b in &frame[0..7] {
            c8 = crc8_update(c8, b);
        }
        frame[7] = c8;
        // Frame CRC-16 covers everything but the trailing 2 CRC bytes.
        let len = frame.len();
        let mut c16 = 0u16;
        for &b in &frame[0..len - 2] {
            c16 = crc16_update(c16, b);
        }
        frame[len - 2..len].copy_from_slice(&c16.to_be_bytes());
        frame
    }

    /// Offset of the frame within a `flac_file` container: fLaC(4) + block
    /// header(4) + STREAMINFO(34).
    const FRAME_OFF: usize = 42;

    #[test]
    fn crc_valid_frame_passes_validation() {
        let samples = [10i16, -20, 30, -40];
        let frame = frame_with_valid_crc(&samples);
        let bytes = flac_file(44_100, 1, 16, samples.len() as u64, &frame);
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        dec.set_validate_crc(true);
        let mut out = [0i32; 8];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 4);
        assert_eq!(&out[..4], &[10, -20, 30, -40]);
    }

    #[test]
    fn crc8_mismatch_is_rejected_when_validating() {
        let samples = [10i16, -20, 30, -40];
        let frame = frame_with_valid_crc(&samples);
        let mut bytes = flac_file(44_100, 1, 16, samples.len() as u64, &frame);
        bytes[FRAME_OFF + 7] ^= 0xFF; // corrupt the stored header CRC-8
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        dec.set_validate_crc(true);
        assert_eq!(dec.decode_into(&mut [0i32; 8]), Err(DecodeError::Malformed));
    }

    #[test]
    fn crc16_mismatch_is_rejected_when_validating() {
        let samples = [10i16, -20, 30, -40];
        let frame = frame_with_valid_crc(&samples);
        let mut bytes = flac_file(44_100, 1, 16, samples.len() as u64, &frame);
        bytes[FRAME_OFF + 9] ^= 0xFF; // corrupt a subframe data byte (after header)
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        dec.set_validate_crc(true);
        assert_eq!(dec.decode_into(&mut [0i32; 8]), Err(DecodeError::Malformed));
    }

    #[test]
    fn crc_off_by_default_is_lenient() {
        let samples = [10i16, -20, 30, -40];
        let frame = frame_with_valid_crc(&samples);
        let mut bytes = flac_file(44_100, 1, 16, samples.len() as u64, &frame);
        bytes[FRAME_OFF + 7] ^= 0xFF; // corrupt the stored CRC-8
        // Default (validation off): the corrupt CRC is ignored, decode proceeds.
        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 4);
    }

    // -- multichannel ---------------------------------------------------------

    #[test]
    fn decodes_4_independent_channels() {
        // FLAC >2 channels are always independent (decorrelation is stereo-only).
        // Channel assignment 3 ⇒ 4 channels. sample[ch][i] = ch*100 + i.
        let block = 5usize;
        let channels = 4u16;
        let mut bw = BitWriter::new();
        write_header(&mut bw, block, 3); // ca = 3 → 4 independent channels
        for c in 0..channels as i32 {
            bw.bits(0, 1); // subframe leading 0
            bw.bits(1, 6); // VERBATIM
            bw.bits(0, 1); // no wasted bits
            for i in 0..block as i32 {
                bw.signed(c * 100 + i, 16);
            }
        }
        write_footer(&mut bw);
        let bytes = flac_file(96_000, channels, 16, block as u64, &bw.into_bytes());

        let mut dec = FlacDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(dec.stream_info().channels, 4);
        let mut out = [0i32; 32];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, block * channels as usize);
        // Interleaved: frame i → [ch0, ch1, ch2, ch3].
        for i in 0..block {
            for c in 0..channels as usize {
                assert_eq!(out[i * 4 + c], (c as i32) * 100 + i as i32);
            }
        }
    }
}
