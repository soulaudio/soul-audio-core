//! MPEG-1/2/2.5 Audio Layer III **codec** — the sample-decoding half of the MP3
//! decoder (the [`Mp3Demuxer`](crate::demux::Mp3Demuxer) is the container half).
//!
//! This is a `no_std`, no-alloc, `#![forbid(unsafe_code)]` port of the Layer III
//! scalar path of [minimp3](https://github.com/lieff/minimp3) (public domain,
//! CC0). The pipeline is the classic ISO 11172-3 chain: side-info parse →
//! scalefactors → Huffman decode of quantized lines → requantization → stereo
//! (MS/intensity) → reorder (short blocks) → alias reduction → IMDCT →
//! polyphase synthesis filterbank → PCM.
//!
//! Verification is component-wise (each stage has math-checked unit tests) plus
//! an end-to-end golden vector produced by the reference C decoder; see the test
//! module in [`super::mp3`].
//!
// The float tables below are transcribed verbatim from minimp3 so they can be
// diffed against the reference; the extra digits parse to the same `f32`.
#![allow(clippy::excessive_precision)]

/// MSB-first bit reader over a byte slice, a port of minimp3's `bs_t`/`get_bits`.
///
/// Positions are tracked in **bits**. Reads that would run past `limit` yield
/// `0` and still advance the cursor (matching the reference), so a caller can
/// detect overrun via [`BitReader::pos`] exceeding [`BitReader::limit`].
pub(crate) struct BitReader<'a> {
    buf: &'a [u8],
    /// Current read position, in bits from the start of `buf`.
    pos: usize,
    /// Valid data length, in bits (may be shorter than `buf.len() * 8`).
    limit: usize,
}

impl<'a> BitReader<'a> {
    /// Reader over the whole slice (`limit = buf.len() * 8`).
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            limit: buf.len() * 8,
        }
    }

    /// Reader whose valid region is the first `bytes` of `buf`. `bytes` is
    /// clamped to the slice length so the bit `limit` never exceeds the buffer.
    pub(crate) fn with_limit(buf: &'a [u8], bytes: usize) -> Self {
        let bytes = bytes.min(buf.len());
        Self {
            buf,
            pos: 0,
            limit: bytes * 8,
        }
    }

    /// Read `n` (0..=32) bits, MSB-first, as an unsigned value. Returns `0` if
    /// the read would pass `limit` (the cursor still advances by `n`).
    pub(crate) fn get_bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let end = self.pos + n as usize;
        if end > self.limit {
            self.pos = end;
            return 0;
        }
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.pos >> 3;
            let bit = 7 - (self.pos & 7);
            let b = u32::from((self.buf[byte] >> bit) & 1);
            v = (v << 1) | b;
            self.pos += 1;
        }
        v
    }

    /// Current bit position.
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    /// Valid length in bits.
    pub(crate) fn limit(&self) -> usize {
        self.limit
    }

    /// Move the cursor to an absolute bit position.
    pub(crate) fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// The underlying byte buffer (the Huffman decoder runs its own byte cache
    /// over this and then syncs [`BitReader::pos`]).
    pub(crate) fn buf(&self) -> &'a [u8] {
        self.buf
    }
}

// --- MPEG frame header ------------------------------------------------------

/// A parsed 4-byte MPEG audio frame header. Only the Layer III fields the codec
/// needs are exposed, as a thin port of minimp3's `HDR_*` macros.
#[derive(Clone, Copy)]
pub(crate) struct Header(pub(crate) [u8; 4]);

impl Header {
    /// True for single-channel mode (`11` in the channel-mode field).
    pub(crate) fn is_mono(self) -> bool {
        (self.0[3] & 0xC0) == 0xC0
    }

    /// True when MS stereo is *unconditionally* on (joint stereo, mode-ext bit).
    pub(crate) fn is_ms_stereo(self) -> bool {
        (self.0[3] & 0xE0) == 0x60
    }

    /// True when a 16-bit CRC follows the header (protection bit is 0).
    pub(crate) fn is_crc(self) -> bool {
        (self.0[1] & 1) == 0
    }

    /// True for MPEG-1 (vs MPEG-2 / MPEG-2.5).
    pub(crate) fn is_mpeg1(self) -> bool {
        (self.0[1] & 0x08) != 0
    }

    /// Intensity-stereo enabled (joint stereo, mode-ext bit 0).
    pub(crate) fn test_i_stereo(self) -> bool {
        (self.0[3] & 0x10) != 0
    }

    /// MS-stereo enabled (joint stereo, mode-ext bit 1).
    pub(crate) fn test_ms_stereo(self) -> bool {
        (self.0[3] & 0x20) != 0
    }

    /// Layer field (`1` = Layer III, matching the 3-bit encoding `4 - layer`).
    pub(crate) fn layer(self) -> u8 {
        (self.0[1] >> 1) & 3
    }

    /// Sample-rate *index* (0..=2), before the MPEG-version family shift.
    pub(crate) fn sample_rate_idx(self) -> usize {
        ((self.0[2] >> 2) & 3) as usize
    }

    /// Combined sample-rate table index 0..=8 across MPEG-1 / 2 / 2.5.
    pub(crate) fn my_sample_rate_idx(self) -> usize {
        let bump = usize::from((self.0[1] >> 3) & 1) + usize::from((self.0[1] >> 4) & 1);
        self.sample_rate_idx() + bump * 3
    }

    /// Interleaved channel count (1 or 2).
    pub(crate) fn channels(self) -> usize {
        if self.is_mono() { 1 } else { 2 }
    }

    /// Number of Layer III granules (2 for MPEG-1, 1 otherwise).
    pub(crate) fn granules(self) -> usize {
        if self.is_mpeg1() { 2 } else { 1 }
    }
}

// --- codec state ------------------------------------------------------------

/// Reservoir byte budget: `main_data_begin` is 9 bits, so at most 511 bytes of a
/// frame's main data may live in earlier frames.
const MAX_RESERVOIR: usize = 511;
/// Upper bound on one frame's main-data payload (minimp3's
/// `MAX_L3_FRAME_PAYLOAD_BYTES`; comfortably above 320 kbps / 32 kHz).
const MAX_L3_FRAME_PAYLOAD: usize = 2304;
/// Working buffer holding `[reservoir tail] ++ [current frame main data]`.
const MAX_MAINDATA: usize = MAX_RESERVOIR + MAX_L3_FRAME_PAYLOAD;

/// Per-granule/per-channel side-info fields (port of minimp3's `L3_gr_info_t`).
#[derive(Clone, Copy, Default)]
pub(crate) struct GrInfo {
    /// Scalefactor-band width table for this granule's block type.
    pub(crate) sfbtab: &'static [u8],
    pub(crate) part_23_length: u16,
    pub(crate) big_values: u16,
    pub(crate) scalefac_compress: u16,
    pub(crate) global_gain: u8,
    pub(crate) block_type: u8,
    pub(crate) mixed_block_flag: u8,
    pub(crate) n_long_sfb: u8,
    pub(crate) n_short_sfb: u8,
    pub(crate) table_select: [u8; 3],
    pub(crate) region_count: [u8; 3],
    pub(crate) subblock_gain: [u8; 3],
    pub(crate) preflag: u8,
    pub(crate) scalefac_scale: u8,
    pub(crate) count1_table: u8,
    pub(crate) scfsi: u8,
}

/// Persistent + scratch state for Layer III decoding (port of `mp3dec_t` plus
/// minimp3's per-call `mp3dec_scratch_t`, kept resident so decoding allocates
/// and stack-spills nothing).
pub(crate) struct Mp3Codec {
    // -- persistent across frames --
    /// IMDCT overlap-add memory, per channel.
    mdct_overlap: [[f32; 9 * 32]; 2],
    /// Synthesis-filterbank delay line.
    qmf_state: [f32; 15 * 2 * 32],
    /// Bytes carried into the next frame via the bit reservoir.
    reserv_buf: [u8; MAX_RESERVOIR],
    reserv: usize,

    // -- per-frame scratch --
    /// Assembled main-data bytes (`[reservoir tail] ++ [frame main data]`).
    maindata: [u8; MAX_MAINDATA],
    /// Decoded side info (up to 2 granules × 2 channels).
    gr_info: [GrInfo; 4],
    /// Dequantized spectral lines, `[ch0 576][ch1 576]`.
    grbuf: [f32; 2 * 576],
    /// Requantization scale per scalefactor band.
    scf: [f32; 40],
    /// Synthesis working buffer (minimp3's `syn`/`lins`).
    syn: [f32; (18 + 15) * (2 * 32)],
    /// Intensity-stereo positions, per channel.
    ist_pos: [[u8; 39]; 2],

    /// Layer I/II sub-decoder (used only for non-Layer-III frames).
    l12: crate::formats::mp3_l12::Layer12,
}

impl Mp3Codec {
    /// A freshly reset codec (equivalent to `mp3dec_init` + zeroed scratch).
    pub(crate) fn new() -> Self {
        Self {
            mdct_overlap: [[0.0; 9 * 32]; 2],
            qmf_state: [0.0; 15 * 2 * 32],
            reserv_buf: [0; MAX_RESERVOIR],
            reserv: 0,
            maindata: [0; MAX_MAINDATA],
            gr_info: [GrInfo::default(); 4],
            grbuf: [0.0; 2 * 576],
            scf: [0.0; 40],
            syn: [0.0; (18 + 15) * (2 * 32)],
            ist_pos: [[0; 39]; 2],
            l12: crate::formats::mp3_l12::Layer12::new(),
        }
    }

    /// Reset all cross-frame state (bit reservoir + IMDCT/QMF history) to the
    /// post-`new` condition — required after a seek, since the reservoir and
    /// filterbank memory belong to the pre-seek position.
    pub(crate) fn reset(&mut self) {
        self.mdct_overlap = [[0.0; 9 * 32]; 2];
        self.qmf_state = [0.0; 15 * 2 * 32];
        self.reserv = 0;
        self.l12.reset();
    }

    /// Assemble this frame's main data into [`Self::maindata`]: the last
    /// `main_data_begin` bytes carried in the reservoir, then `frame_main`
    /// (the bytes after the side info in the current frame).
    ///
    /// Returns `(maindata_len, enough)`; `enough` is false when the reservoir
    /// lacks the referenced history (stream start / after an error), in which
    /// case the granule cannot be decoded but the reservoir is still updated.
    pub(crate) fn restore_reservoir(
        &mut self,
        frame_main: &[u8],
        main_data_begin: usize,
    ) -> (usize, bool) {
        let frame_bytes = frame_main.len().min(MAX_L3_FRAME_PAYLOAD);
        let bytes_have = self.reserv.min(main_data_begin);
        let start = self.reserv.saturating_sub(main_data_begin);
        self.maindata[..bytes_have].copy_from_slice(&self.reserv_buf[start..start + bytes_have]);
        self.maindata[bytes_have..bytes_have + frame_bytes]
            .copy_from_slice(&frame_main[..frame_bytes]);
        (bytes_have + frame_bytes, self.reserv >= main_data_begin)
    }

    /// Save the still-unconsumed tail of [`Self::maindata`] as the reservoir for
    /// the next frame. `pos_bits`/`limit_bits` are the final bit cursor and the
    /// valid length of the main-data [`BitReader`].
    pub(crate) fn save_reservoir(&mut self, pos_bits: usize, limit_bits: usize) {
        let mut pos = pos_bits.div_ceil(8);
        let limit_bytes = limit_bits / 8;
        let mut remains = limit_bytes.saturating_sub(pos);
        if remains > MAX_RESERVOIR {
            pos += remains - MAX_RESERVOIR;
            remains = MAX_RESERVOIR;
        }
        if remains > 0 {
            self.reserv_buf[..remains].copy_from_slice(&self.maindata[pos..pos + remains]);
        }
        self.reserv = remains;
    }
}

impl Default for Mp3Codec {
    fn default() -> Self {
        Self::new()
    }
}

// --- side info --------------------------------------------------------------

/// Layer III block type carrying three 6-sample short windows.
const SHORT_BLOCK_TYPE: u8 = 2;
/// Layer III "stop" block type (long window with a truncated tail).
const STOP_BLOCK_TYPE: u8 = 3;

/// Scalefactor-band widths for long blocks, indexed by minimp3's collapsed
/// sample-rate index. Each row is 22 band widths plus a `0` terminator.
static G_SCF_LONG: [[u8; 23]; 8] = [
    [
        6, 6, 6, 6, 6, 6, 8, 10, 12, 14, 16, 20, 24, 28, 32, 38, 46, 52, 60, 68, 58, 54, 0,
    ],
    [
        12, 12, 12, 12, 12, 12, 16, 20, 24, 28, 32, 40, 48, 56, 64, 76, 90, 2, 2, 2, 2, 2, 0,
    ],
    [
        6, 6, 6, 6, 6, 6, 8, 10, 12, 14, 16, 20, 24, 28, 32, 38, 46, 52, 60, 68, 58, 54, 0,
    ],
    [
        6, 6, 6, 6, 6, 6, 8, 10, 12, 14, 16, 18, 22, 26, 32, 38, 46, 54, 62, 70, 76, 36, 0,
    ],
    [
        6, 6, 6, 6, 6, 6, 8, 10, 12, 14, 16, 20, 24, 28, 32, 38, 46, 52, 60, 68, 58, 54, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 6, 6, 8, 8, 10, 12, 16, 20, 24, 28, 34, 42, 50, 54, 76, 158, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 6, 6, 6, 8, 10, 12, 16, 18, 22, 28, 34, 40, 46, 54, 54, 192, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 6, 6, 8, 10, 12, 16, 20, 24, 30, 38, 46, 56, 68, 84, 102, 26, 0,
    ],
];

/// Scalefactor-band widths for short blocks (39 widths + terminator). Each width
/// applies to all three windows of the band.
static G_SCF_SHORT: [[u8; 40]; 8] = [
    [
        4, 4, 4, 4, 4, 4, 4, 4, 4, 6, 6, 6, 8, 8, 8, 10, 10, 10, 12, 12, 12, 14, 14, 14, 18, 18,
        18, 24, 24, 24, 30, 30, 30, 40, 40, 40, 18, 18, 18, 0,
    ],
    [
        8, 8, 8, 8, 8, 8, 8, 8, 8, 12, 12, 12, 16, 16, 16, 20, 20, 20, 24, 24, 24, 28, 28, 28, 36,
        36, 36, 2, 2, 2, 2, 2, 2, 2, 2, 2, 26, 26, 26, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 4, 4, 4, 6, 6, 6, 6, 6, 6, 8, 8, 8, 10, 10, 10, 14, 14, 14, 18, 18, 18,
        26, 26, 26, 32, 32, 32, 42, 42, 42, 18, 18, 18, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 4, 4, 4, 6, 6, 6, 8, 8, 8, 10, 10, 10, 12, 12, 12, 14, 14, 14, 18, 18,
        18, 24, 24, 24, 32, 32, 32, 44, 44, 44, 12, 12, 12, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 4, 4, 4, 6, 6, 6, 8, 8, 8, 10, 10, 10, 12, 12, 12, 14, 14, 14, 18, 18,
        18, 24, 24, 24, 30, 30, 30, 40, 40, 40, 18, 18, 18, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 6, 6, 6, 8, 8, 8, 10, 10, 10, 12, 12, 12, 14, 14, 14,
        18, 18, 18, 22, 22, 22, 30, 30, 30, 56, 56, 56, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 6, 6, 6, 6, 6, 6, 10, 10, 10, 12, 12, 12, 14, 14, 14,
        16, 16, 16, 20, 20, 20, 26, 26, 26, 66, 66, 66, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 6, 6, 6, 8, 8, 8, 12, 12, 12, 16, 16, 16, 20, 20, 20,
        26, 26, 26, 34, 34, 34, 42, 42, 42, 12, 12, 12, 0,
    ],
];

/// Scalefactor-band widths for mixed blocks (long head, short tail).
static G_SCF_MIXED: [[u8; 40]; 8] = [
    [
        6, 6, 6, 6, 6, 6, 6, 6, 6, 8, 8, 8, 10, 10, 10, 12, 12, 12, 14, 14, 14, 18, 18, 18, 24, 24,
        24, 30, 30, 30, 40, 40, 40, 18, 18, 18, 0, 0, 0, 0,
    ],
    [
        12, 12, 12, 4, 4, 4, 8, 8, 8, 12, 12, 12, 16, 16, 16, 20, 20, 20, 24, 24, 24, 28, 28, 28,
        36, 36, 36, 2, 2, 2, 2, 2, 2, 2, 2, 2, 26, 26, 26, 0,
    ],
    [
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 8, 8, 8, 10, 10, 10, 14, 14, 14, 18, 18, 18, 26, 26,
        26, 32, 32, 32, 42, 42, 42, 18, 18, 18, 0, 0, 0, 0,
    ],
    [
        6, 6, 6, 6, 6, 6, 6, 6, 6, 8, 8, 8, 10, 10, 10, 12, 12, 12, 14, 14, 14, 18, 18, 18, 24, 24,
        24, 32, 32, 32, 44, 44, 44, 12, 12, 12, 0, 0, 0, 0,
    ],
    [
        6, 6, 6, 6, 6, 6, 6, 6, 6, 8, 8, 8, 10, 10, 10, 12, 12, 12, 14, 14, 14, 18, 18, 18, 24, 24,
        24, 30, 30, 30, 40, 40, 40, 18, 18, 18, 0, 0, 0, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 6, 6, 4, 4, 4, 6, 6, 6, 8, 8, 8, 10, 10, 10, 12, 12, 12, 14, 14, 14, 18,
        18, 18, 22, 22, 22, 30, 30, 30, 56, 56, 56, 0, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 6, 6, 4, 4, 4, 6, 6, 6, 6, 6, 6, 10, 10, 10, 12, 12, 12, 14, 14, 14, 16,
        16, 16, 20, 20, 20, 26, 26, 26, 66, 66, 66, 0, 0,
    ],
    [
        4, 4, 4, 4, 4, 4, 6, 6, 4, 4, 4, 6, 6, 6, 8, 8, 8, 12, 12, 12, 16, 16, 16, 20, 20, 20, 26,
        26, 26, 34, 34, 34, 42, 42, 42, 12, 12, 12, 0, 0,
    ],
];

/// Parse the Layer III side info for one frame, filling `gr` (one [`GrInfo`] per
/// granule × channel) and returning `main_data_begin`. Faithful port of
/// minimp3's `L3_read_side_info`; `None` signals a malformed frame.
pub(crate) fn read_side_info(bs: &mut BitReader, gr: &mut [GrInfo], hdr: Header) -> Option<usize> {
    // minimp3's `sr_idx -= (sr_idx != 0)`: collapse indices 0 and 1.
    let sr_idx = hdr.my_sample_rate_idx().saturating_sub(1);
    let mut gr_count = hdr.channels();
    let mut scfsi: u32 = 0;
    let main_data_begin;
    if hdr.is_mpeg1() {
        gr_count *= 2;
        main_data_begin = bs.get_bits(9) as usize;
        scfsi = bs.get_bits(7 + gr_count as u32);
    } else {
        main_data_begin = (bs.get_bits(8 + gr_count as u32) >> gr_count) as usize;
    }

    let mono = hdr.is_mono();
    let mut part_23_sum: usize = 0;

    for g in gr.iter_mut().take(gr_count) {
        if mono {
            scfsi <<= 4;
        }
        g.part_23_length = bs.get_bits(12) as u16;
        part_23_sum += g.part_23_length as usize;
        g.big_values = bs.get_bits(9) as u16;
        if g.big_values > 288 {
            return None;
        }
        g.global_gain = bs.get_bits(8) as u8;
        g.scalefac_compress = bs.get_bits(if hdr.is_mpeg1() { 4 } else { 9 }) as u16;
        g.sfbtab = &G_SCF_LONG[sr_idx];
        g.n_long_sfb = 22;
        g.n_short_sfb = 0;

        let tables;
        if bs.get_bits(1) != 0 {
            g.block_type = bs.get_bits(2) as u8;
            if g.block_type == 0 {
                return None;
            }
            g.mixed_block_flag = bs.get_bits(1) as u8;
            g.region_count[0] = 7;
            g.region_count[1] = 255;
            if g.block_type == SHORT_BLOCK_TYPE {
                scfsi &= 0x0F0F;
                if g.mixed_block_flag == 0 {
                    g.region_count[0] = 8;
                    g.sfbtab = &G_SCF_SHORT[sr_idx];
                    g.n_long_sfb = 0;
                    g.n_short_sfb = 39;
                } else {
                    g.sfbtab = &G_SCF_MIXED[sr_idx];
                    g.n_long_sfb = if hdr.is_mpeg1() { 8 } else { 6 };
                    g.n_short_sfb = 30;
                }
            }
            tables = bs.get_bits(10) << 5;
            g.subblock_gain[0] = bs.get_bits(3) as u8;
            g.subblock_gain[1] = bs.get_bits(3) as u8;
            g.subblock_gain[2] = bs.get_bits(3) as u8;
        } else {
            g.block_type = 0;
            g.mixed_block_flag = 0;
            tables = bs.get_bits(15);
            g.region_count[0] = bs.get_bits(4) as u8;
            g.region_count[1] = bs.get_bits(3) as u8;
            g.region_count[2] = 255;
        }
        g.table_select[0] = (tables >> 10) as u8;
        g.table_select[1] = ((tables >> 5) & 31) as u8;
        g.table_select[2] = (tables & 31) as u8;
        g.preflag = if hdr.is_mpeg1() {
            bs.get_bits(1) as u8
        } else {
            u8::from(g.scalefac_compress >= 500)
        };
        g.scalefac_scale = bs.get_bits(1) as u8;
        g.count1_table = bs.get_bits(1) as u8;
        g.scfsi = ((scfsi >> 12) & 15) as u8;
        scfsi <<= 4;
    }

    if part_23_sum + bs.pos() > bs.limit() + main_data_begin * 8 {
        return None;
    }
    Some(main_data_begin)
}

// --- scalefactors -----------------------------------------------------------

/// `-1`: the fixed dequantizer output exponent bias (minimp3 `BITS_DEQUANTIZER_OUT`).
const BITS_DEQUANTIZER_OUT: i32 = -1;
/// Largest scalefactor exponent term (minimp3 `MAX_SCF`).
const MAX_SCF: i32 = 255 + BITS_DEQUANTIZER_OUT * 4 - 210;
/// `MAX_SCF` rounded up to a multiple of 4 (minimp3 `MAX_SCFI`).
const MAX_SCFI: i32 = (MAX_SCF + 3) & !3;

/// Multiply `y` by `2^(-exp_q2 / 4)` using only table lookups and multiplies
/// (port of minimp3's `L3_ldexp_q2`). `exp_q2` is a quarter-power exponent.
pub(crate) fn ldexp_q2(mut y: f32, mut exp_q2: i32) -> f32 {
    // g_expfrac[j] == 2^(-30 - j/4); combined with the integer `2^(30 - e/4)`
    // factor below, each step multiplies by exactly `2^(-e/4)`.
    const G_EXPFRAC: [f32; 4] = [
        9.31322575e-10,
        7.83145814e-10,
        6.58544508e-10,
        5.53767716e-10,
    ];
    loop {
        let e = (30 * 4).min(exp_q2);
        let int_pow = (1u32 << 30) >> (e >> 2);
        y *= G_EXPFRAC[(e & 3) as usize] * int_pow as f32;
        exp_q2 -= e;
        if exp_q2 <= 0 {
            break;
        }
    }
    y
}

/// Read one granule's raw scalefactors into `scf` (and intensity positions into
/// `ist_pos`), partition by partition. Port of minimp3's `L3_read_scalefactors`.
fn read_scalefactors(
    scf: &mut [u8],
    ist_pos: &mut [u8],
    scf_size: &[u8; 4],
    scf_count: &[u8],
    bs: &mut BitReader,
    mut scfsi: i32,
) {
    let mut scf_off = 0usize;
    let mut ist_off = 0usize;
    for (i, &size) in scf_size.iter().enumerate() {
        let cnt = scf_count.get(i).copied().unwrap_or(0) as usize;
        if cnt == 0 {
            break;
        }
        if scfsi & 8 != 0 {
            // Reuse the previous granule's positions for this band group.
            scf[scf_off..scf_off + cnt].copy_from_slice(&ist_pos[ist_off..ist_off + cnt]);
        } else {
            let bits = u32::from(size);
            if bits == 0 {
                for k in 0..cnt {
                    scf[scf_off + k] = 0;
                    ist_pos[ist_off + k] = 0;
                }
            } else {
                // Only meaningful when `scfsi < 0` (MPEG-2 intensity sentinel).
                let max_scf = if scfsi < 0 { (1i32 << bits) - 1 } else { -1 };
                for k in 0..cnt {
                    let s = bs.get_bits(bits) as i32;
                    ist_pos[ist_off + k] = if s == max_scf { 0xFF } else { s as u8 };
                    scf[scf_off + k] = s as u8;
                }
            }
        }
        ist_off += cnt;
        scf_off += cnt;
        scfsi *= 2;
    }
    scf[scf_off] = 0;
    scf[scf_off + 1] = 0;
    scf[scf_off + 2] = 0;
}

/// Decode a granule's scalefactors into requantization multipliers `scf`
/// (per scalefactor band). Port of minimp3's `L3_decode_scalefactors`: derives
/// the per-partition bit widths from `scalefac_compress`, reads the raw values,
/// folds in subblock gain / preflag, and scales by the global gain.
pub(crate) fn decode_scalefactors(
    hdr: Header,
    ist_pos: &mut [u8],
    bs: &mut BitReader,
    gr: &GrInfo,
    scf: &mut [f32],
    ch: usize,
) {
    // Partition layouts: [long, short, mixed] selected by block kind.
    static G_SCF_PARTITIONS: [[u8; 28]; 3] = [
        [
            6, 5, 5, 5, 6, 5, 5, 5, 6, 5, 7, 3, 11, 10, 0, 0, 7, 7, 7, 0, 6, 6, 6, 3, 8, 8, 5, 0,
        ],
        [
            8, 9, 6, 12, 6, 9, 9, 9, 6, 9, 12, 6, 15, 18, 0, 0, 6, 15, 12, 0, 6, 12, 9, 6, 6, 18,
            9, 0,
        ],
        [
            9, 9, 6, 12, 9, 9, 9, 9, 9, 9, 12, 6, 18, 18, 0, 0, 12, 12, 12, 0, 12, 9, 9, 6, 15, 12,
            9, 0,
        ],
    ];
    let part_kind = usize::from(gr.n_short_sfb != 0) + usize::from(gr.n_long_sfb == 0);
    let partition = &G_SCF_PARTITIONS[part_kind];
    let mut scf_size = [0u8; 4];
    let mut iscf = [0u8; 40];
    let scf_shift = i32::from(gr.scalefac_scale) + 1;
    let mut scfsi = i32::from(gr.scfsi);
    let mut part_off = 0usize;

    if hdr.is_mpeg1() {
        static G_SCFC_DECODE: [u8; 16] = [0, 1, 2, 3, 12, 5, 6, 7, 9, 10, 11, 13, 14, 15, 18, 19];
        let part = G_SCFC_DECODE[gr.scalefac_compress as usize & 15];
        scf_size[0] = part >> 2;
        scf_size[1] = part >> 2;
        scf_size[2] = part & 3;
        scf_size[3] = part & 3;
    } else {
        static G_MOD: [u8; 24] = [
            5, 5, 4, 4, 5, 5, 4, 1, 4, 3, 1, 1, 5, 6, 6, 1, 4, 4, 4, 1, 4, 3, 1, 1,
        ];
        let ist = usize::from(hdr.test_i_stereo() && ch != 0);
        let mut sfc = i32::from(gr.scalefac_compress >> ist);
        let mut k = ist * 3 * 4;
        while sfc >= 0 {
            let mut modprod = 1i32;
            for i in (0..4).rev() {
                scf_size[i] = (sfc / modprod % i32::from(G_MOD[k + i])) as u8;
                modprod *= i32::from(G_MOD[k + i]);
            }
            sfc -= modprod;
            k += 4;
        }
        part_off = k;
        scfsi = -16;
    }

    read_scalefactors(
        &mut iscf,
        ist_pos,
        &scf_size,
        &partition[part_off.min(partition.len())..],
        bs,
        scfsi,
    );

    if gr.n_short_sfb != 0 {
        let sh = 3 - scf_shift;
        let n_long = gr.n_long_sfb as usize;
        let mut i = 0usize;
        while i < gr.n_short_sfb as usize {
            for j in 0..3 {
                let add = (i32::from(gr.subblock_gain[j]) << sh) as u8;
                iscf[n_long + i + j] = iscf[n_long + i + j].wrapping_add(add);
            }
            i += 3;
        }
    } else if gr.preflag != 0 {
        static G_PREAMP: [u8; 10] = [1, 1, 1, 1, 2, 2, 3, 3, 3, 2];
        for (i, &amp) in G_PREAMP.iter().enumerate() {
            iscf[11 + i] = iscf[11 + i].wrapping_add(amp);
        }
    }

    let gain_exp = i32::from(gr.global_gain) + BITS_DEQUANTIZER_OUT * 4
        - 210
        - if hdr.is_ms_stereo() { 2 } else { 0 };
    let gain = ldexp_q2((1u32 << (MAX_SCFI / 4)) as f32, MAX_SCFI - gain_exp);
    let n = (gr.n_long_sfb + gr.n_short_sfb) as usize;
    for i in 0..n {
        scf[i] = ldexp_q2(gain, i32::from(iscf[i]) << scf_shift);
    }
}

// --- Huffman decode + requantization ---------------------------------------

/// `pow(x, 4/3)` requantization lookup: 16 negative entries, then 129 positive
/// entries, transcribed from minimp3's `g_pow43`.
static G_POW43: [f32; 145] = [
    0.0, -1.0, -2.519842, -4.326749, -6.349604, -8.549880, -10.902724, -13.390518, -16.000000,
    -18.720754, -21.544347, -24.463781, -27.473142, -30.567351, -33.741992, -36.993181, 0.0, 1.0,
    2.519842, 4.326749, 6.349604, 8.549880, 10.902724, 13.390518, 16.000000, 18.720754, 21.544347,
    24.463781, 27.473142, 30.567351, 33.741992, 36.993181, 40.317474, 43.711787, 47.173345,
    50.699631, 54.288352, 57.937408, 61.644865, 65.408941, 69.227979, 73.100443, 77.024898,
    81.000000, 85.024491, 89.097188, 93.216975, 97.382800, 101.593667, 105.848633, 110.146801,
    114.487321, 118.869381, 123.292209, 127.755065, 132.257246, 136.798076, 141.376907, 145.993119,
    150.646117, 155.335327, 160.060199, 164.820202, 169.614826, 174.443577, 179.305980, 184.201575,
    189.129918, 194.090580, 199.083145, 204.107210, 209.162385, 214.248292, 219.364564, 224.510845,
    229.686789, 234.892058, 240.126328, 245.389280, 250.680604, 256.000000, 261.347174, 266.721841,
    272.123723, 277.552547, 283.008049, 288.489971, 293.998060, 299.532071, 305.091761, 310.676898,
    316.287249, 321.922592, 327.582707, 333.267377, 338.976394, 344.709550, 350.466646, 356.247482,
    362.051866, 367.879608, 373.730522, 379.604427, 385.501143, 391.420496, 397.362314, 403.326427,
    409.312672, 415.320884, 421.350905, 427.402579, 433.475750, 439.570269, 445.685987, 451.822757,
    457.980436, 464.158883, 470.357960, 476.577530, 482.817459, 489.077615, 495.357868, 501.658090,
    507.978156, 514.317941, 520.677324, 527.056184, 533.454404, 539.871867, 546.308458, 552.764065,
    559.238575, 565.731879, 572.243870, 578.774440, 585.323483, 591.890898, 598.476581, 605.080431,
    611.702349, 618.342238, 625.000000, 631.675540, 638.368763, 645.079578,
];

/// `x^(4/3)` requantization power (port of minimp3's `L3_pow_43`), table-based
/// for `x < 129` and a polynomial refinement above.
fn pow_43(mut x: i32) -> f32 {
    if x < 129 {
        return G_POW43[(16 + x) as usize];
    }
    let mut mult = 256.0f32;
    if x < 1024 {
        mult = 16.0;
        x <<= 3;
    }
    let sign = (2 * x) & 64;
    let frac = ((x & 63) - sign) as f32 / ((x & !63) + sign) as f32;
    G_POW43[(16 + ((x + sign) >> 6)) as usize]
        * (1.0 + frac * ((4.0 / 3.0) + frac * (2.0 / 9.0)))
        * mult
}

/// Packed Huffman decision tree for the 32 big-value tables (minimp3 `tabs`).
static TABS: [i16; 2164] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    785, 785, 785, 785, 784, 784, 784, 784, 513, 513, 513, 513, 513, 513, 513, 513, 256, 256, 256,
    256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, -255, 1313, 1298, 1282, 785,
    785, 785, 785, 784, 784, 784, 784, 769, 769, 769, 769, 256, 256, 256, 256, 256, 256, 256, 256,
    256, 256, 256, 256, 256, 256, 256, 256, 290, 288, -255, 1313, 1298, 1282, 769, 769, 769, 769,
    529, 529, 529, 529, 529, 529, 529, 529, 528, 528, 528, 528, 528, 528, 528, 528, 512, 512, 512,
    512, 512, 512, 512, 512, 290, 288, -253, -318, -351, -367, 785, 785, 785, 785, 784, 784, 784,
    784, 769, 769, 769, 769, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256,
    256, 256, 819, 818, 547, 547, 275, 275, 275, 275, 561, 560, 515, 546, 289, 274, 288, 258, -254,
    -287, 1329, 1299, 1314, 1312, 1057, 1057, 1042, 1042, 1026, 1026, 784, 784, 784, 784, 529, 529,
    529, 529, 529, 529, 529, 529, 769, 769, 769, 769, 768, 768, 768, 768, 563, 560, 306, 306, 291,
    259, -252, -413, -477, -542, 1298, -575, 1041, 1041, 784, 784, 784, 784, 769, 769, 769, 769,
    256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, -383, -399,
    1107, 1092, 1106, 1061, 849, 849, 789, 789, 1104, 1091, 773, 773, 1076, 1075, 341, 340, 325,
    309, 834, 804, 577, 577, 532, 532, 516, 516, 832, 818, 803, 816, 561, 561, 531, 531, 515, 546,
    289, 289, 288, 258, -252, -429, -493, -559, 1057, 1057, 1042, 1042, 529, 529, 529, 529, 529,
    529, 529, 529, 784, 784, 784, 784, 769, 769, 769, 769, 512, 512, 512, 512, 512, 512, 512, 512,
    -382, 1077, -415, 1106, 1061, 1104, 849, 849, 789, 789, 1091, 1076, 1029, 1075, 834, 834, 597,
    581, 340, 340, 339, 324, 804, 833, 532, 532, 832, 772, 818, 803, 817, 787, 816, 771, 290, 290,
    290, 290, 288, 258, -253, -349, -414, -447, -463, 1329, 1299, -479, 1314, 1312, 1057, 1057,
    1042, 1042, 1026, 1026, 785, 785, 785, 785, 784, 784, 784, 784, 769, 769, 769, 769, 768, 768,
    768, 768, -319, 851, 821, -335, 836, 850, 805, 849, 341, 340, 325, 336, 533, 533, 579, 579,
    564, 564, 773, 832, 578, 548, 563, 516, 321, 276, 306, 291, 304, 259, -251, -572, -733, -830,
    -863, -879, 1041, 1041, 784, 784, 784, 784, 769, 769, 769, 769, 256, 256, 256, 256, 256, 256,
    256, 256, 256, 256, 256, 256, 256, 256, 256, 256, -511, -527, -543, 1396, 1351, 1381, 1366,
    1395, 1335, 1380, -559, 1334, 1138, 1138, 1063, 1063, 1350, 1392, 1031, 1031, 1062, 1062, 1364,
    1363, 1120, 1120, 1333, 1348, 881, 881, 881, 881, 375, 374, 359, 373, 343, 358, 341, 325, 791,
    791, 1123, 1122, -703, 1105, 1045, -719, 865, 865, 790, 790, 774, 774, 1104, 1029, 338, 293,
    323, 308, -799, -815, 833, 788, 772, 818, 803, 816, 322, 292, 307, 320, 561, 531, 515, 546,
    289, 274, 288, 258, -251, -525, -605, -685, -765, -831, -846, 1298, 1057, 1057, 1312, 1282,
    785, 785, 785, 785, 784, 784, 784, 784, 769, 769, 769, 769, 512, 512, 512, 512, 512, 512, 512,
    512, 1399, 1398, 1383, 1367, 1382, 1396, 1351, -511, 1381, 1366, 1139, 1139, 1079, 1079, 1124,
    1124, 1364, 1349, 1363, 1333, 882, 882, 882, 882, 807, 807, 807, 807, 1094, 1094, 1136, 1136,
    373, 341, 535, 535, 881, 775, 867, 822, 774, -591, 324, 338, -671, 849, 550, 550, 866, 864,
    609, 609, 293, 336, 534, 534, 789, 835, 773, -751, 834, 804, 308, 307, 833, 788, 832, 772, 562,
    562, 547, 547, 305, 275, 560, 515, 290, 290, -252, -397, -477, -557, -622, -653, -719, -735,
    -750, 1329, 1299, 1314, 1057, 1057, 1042, 1042, 1312, 1282, 1024, 1024, 785, 785, 785, 785,
    784, 784, 784, 784, 769, 769, 769, 769, -383, 1127, 1141, 1111, 1126, 1140, 1095, 1110, 869,
    869, 883, 883, 1079, 1109, 882, 882, 375, 374, 807, 868, 838, 881, 791, -463, 867, 822, 368,
    263, 852, 837, 836, -543, 610, 610, 550, 550, 352, 336, 534, 534, 865, 774, 851, 821, 850, 805,
    593, 533, 579, 564, 773, 832, 578, 578, 548, 548, 577, 577, 307, 276, 306, 291, 516, 560, 259,
    259, -250, -2107, -2507, -2764, -2909, -2974, -3007, -3023, 1041, 1041, 1040, 1040, 769, 769,
    769, 769, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, -767,
    -1052, -1213, -1277, -1358, -1405, -1469, -1535, -1550, -1582, -1614, -1647, -1662, -1694,
    -1726, -1759, -1774, -1807, -1822, -1854, -1886, 1565, -1919, -1935, -1951, -1967, 1731, 1730,
    1580, 1717, -1983, 1729, 1564, -1999, 1548, -2015, -2031, 1715, 1595, -2047, 1714, -2063, 1610,
    -2079, 1609, -2095, 1323, 1323, 1457, 1457, 1307, 1307, 1712, 1547, 1641, 1700, 1699, 1594,
    1685, 1625, 1442, 1442, 1322, 1322, -780, -973, -910, 1279, 1278, 1277, 1262, 1276, 1261, 1275,
    1215, 1260, 1229, -959, 974, 974, 989, 989, -943, 735, 478, 478, 495, 463, 506, 414, -1039,
    1003, 958, 1017, 927, 942, 987, 957, 431, 476, 1272, 1167, 1228, -1183, 1256, -1199, 895, 895,
    941, 941, 1242, 1227, 1212, 1135, 1014, 1014, 490, 489, 503, 487, 910, 1013, 985, 925, 863,
    894, 970, 955, 1012, 847, -1343, 831, 755, 755, 984, 909, 428, 366, 754, 559, -1391, 752, 486,
    457, 924, 997, 698, 698, 983, 893, 740, 740, 908, 877, 739, 739, 667, 667, 953, 938, 497, 287,
    271, 271, 683, 606, 590, 712, 726, 574, 302, 302, 738, 736, 481, 286, 526, 725, 605, 711, 636,
    724, 696, 651, 589, 681, 666, 710, 364, 467, 573, 695, 466, 466, 301, 465, 379, 379, 709, 604,
    665, 679, 316, 316, 634, 633, 436, 436, 464, 269, 424, 394, 452, 332, 438, 363, 347, 408, 393,
    448, 331, 422, 362, 407, 392, 421, 346, 406, 391, 376, 375, 359, 1441, 1306, -2367, 1290,
    -2383, 1337, -2399, -2415, 1426, 1321, -2431, 1411, 1336, -2447, -2463, -2479, 1169, 1169,
    1049, 1049, 1424, 1289, 1412, 1352, 1319, -2495, 1154, 1154, 1064, 1064, 1153, 1153, 416, 390,
    360, 404, 403, 389, 344, 374, 373, 343, 358, 372, 327, 357, 342, 311, 356, 326, 1395, 1394,
    1137, 1137, 1047, 1047, 1365, 1392, 1287, 1379, 1334, 1364, 1349, 1378, 1318, 1363, 792, 792,
    792, 792, 1152, 1152, 1032, 1032, 1121, 1121, 1046, 1046, 1120, 1120, 1030, 1030, -2895, 1106,
    1061, 1104, 849, 849, 789, 789, 1091, 1076, 1029, 1090, 1060, 1075, 833, 833, 309, 324, 532,
    532, 832, 772, 818, 803, 561, 561, 531, 560, 515, 546, 289, 274, 288, 258, -250, -1179, -1579,
    -1836, -1996, -2124, -2253, -2333, -2413, -2477, -2542, -2574, -2607, -2622, -2655, 1314, 1313,
    1298, 1312, 1282, 785, 785, 785, 785, 1040, 1040, 1025, 1025, 768, 768, 768, 768, -766, -798,
    -830, -862, -895, -911, -927, -943, -959, -975, -991, -1007, -1023, -1039, -1055, -1070, 1724,
    1647, -1103, -1119, 1631, 1767, 1662, 1738, 1708, 1723, -1135, 1780, 1615, 1779, 1599, 1677,
    1646, 1778, 1583, -1151, 1777, 1567, 1737, 1692, 1765, 1722, 1707, 1630, 1751, 1661, 1764,
    1614, 1736, 1676, 1763, 1750, 1645, 1598, 1721, 1691, 1762, 1706, 1582, 1761, 1566, -1167,
    1749, 1629, 767, 766, 751, 765, 494, 494, 735, 764, 719, 749, 734, 763, 447, 447, 748, 718,
    477, 506, 431, 491, 446, 476, 461, 505, 415, 430, 475, 445, 504, 399, 460, 489, 414, 503, 383,
    474, 429, 459, 502, 502, 746, 752, 488, 398, 501, 473, 413, 472, 486, 271, 480, 270, -1439,
    -1455, 1357, -1471, -1487, -1503, 1341, 1325, -1519, 1489, 1463, 1403, 1309, -1535, 1372, 1448,
    1418, 1476, 1356, 1462, 1387, -1551, 1475, 1340, 1447, 1402, 1386, -1567, 1068, 1068, 1474,
    1461, 455, 380, 468, 440, 395, 425, 410, 454, 364, 467, 466, 464, 453, 269, 409, 448, 268, 432,
    1371, 1473, 1432, 1417, 1308, 1460, 1355, 1446, 1459, 1431, 1083, 1083, 1401, 1416, 1458, 1445,
    1067, 1067, 1370, 1457, 1051, 1051, 1291, 1430, 1385, 1444, 1354, 1415, 1400, 1443, 1082, 1082,
    1173, 1113, 1186, 1066, 1185, 1050, -1967, 1158, 1128, 1172, 1097, 1171, 1081, -1983, 1157,
    1112, 416, 266, 375, 400, 1170, 1142, 1127, 1065, 793, 793, 1169, 1033, 1156, 1096, 1141, 1111,
    1155, 1080, 1126, 1140, 898, 898, 808, 808, 897, 897, 792, 792, 1095, 1152, 1032, 1125, 1110,
    1139, 1079, 1124, 882, 807, 838, 881, 853, 791, -2319, 867, 368, 263, 822, 852, 837, 866, 806,
    865, -2399, 851, 352, 262, 534, 534, 821, 836, 594, 594, 549, 549, 593, 593, 533, 533, 848,
    773, 579, 579, 564, 578, 548, 563, 276, 276, 577, 576, 306, 291, 516, 560, 305, 305, 275, 259,
    -251, -892, -2058, -2620, -2828, -2957, -3023, -3039, 1041, 1041, 1040, 1040, 769, 769, 769,
    769, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, 256, -511,
    -527, -543, -559, 1530, -575, -591, 1528, 1527, 1407, 1526, 1391, 1023, 1023, 1023, 1023, 1525,
    1375, 1268, 1268, 1103, 1103, 1087, 1087, 1039, 1039, 1523, -604, 815, 815, 815, 815, 510, 495,
    509, 479, 508, 463, 507, 447, 431, 505, 415, 399, -734, -782, 1262, -815, 1259, 1244, -831,
    1258, 1228, -847, -863, 1196, -879, 1253, 987, 987, 748, -767, 493, 493, 462, 477, 414, 414,
    686, 669, 478, 446, 461, 445, 474, 429, 487, 458, 412, 471, 1266, 1264, 1009, 1009, 799, 799,
    -1019, -1276, -1452, -1581, -1677, -1757, -1821, -1886, -1933, -1997, 1257, 1257, 1483, 1468,
    1512, 1422, 1497, 1406, 1467, 1496, 1421, 1510, 1134, 1134, 1225, 1225, 1466, 1451, 1374, 1405,
    1252, 1252, 1358, 1480, 1164, 1164, 1251, 1251, 1238, 1238, 1389, 1465, -1407, 1054, 1101,
    -1423, 1207, -1439, 830, 830, 1248, 1038, 1237, 1117, 1223, 1148, 1236, 1208, 411, 426, 395,
    410, 379, 269, 1193, 1222, 1132, 1235, 1221, 1116, 976, 976, 1192, 1162, 1177, 1220, 1131,
    1191, 963, 963, -1647, 961, 780, -1663, 558, 558, 994, 993, 437, 408, 393, 407, 829, 978, 813,
    797, 947, -1743, 721, 721, 377, 392, 844, 950, 828, 890, 706, 706, 812, 859, 796, 960, 948,
    843, 934, 874, 571, 571, -1919, 690, 555, 689, 421, 346, 539, 539, 944, 779, 918, 873, 932,
    842, 903, 888, 570, 570, 931, 917, 674, 674, -2575, 1562, -2591, 1609, -2607, 1654, 1322, 1322,
    1441, 1441, 1696, 1546, 1683, 1593, 1669, 1624, 1426, 1426, 1321, 1321, 1639, 1680, 1425, 1425,
    1305, 1305, 1545, 1668, 1608, 1623, 1667, 1592, 1638, 1666, 1320, 1320, 1652, 1607, 1409, 1409,
    1304, 1304, 1288, 1288, 1664, 1637, 1395, 1395, 1335, 1335, 1622, 1636, 1394, 1394, 1319, 1319,
    1606, 1621, 1392, 1392, 1137, 1137, 1137, 1137, 345, 390, 360, 375, 404, 373, 1047, -2751,
    -2767, -2783, 1062, 1121, 1046, -2799, 1077, -2815, 1106, 1061, 789, 789, 1105, 1104, 263, 355,
    310, 340, 325, 354, 352, 262, 339, 324, 1091, 1076, 1029, 1090, 1060, 1075, 833, 833, 788, 788,
    1088, 1028, 818, 818, 803, 803, 561, 561, 531, 531, 816, 771, 546, 546, 289, 274, 288, 258,
    -253, -317, -381, -446, -478, -509, 1279, 1279, -811, -1179, -1451, -1756, -1900, -2028, -2189,
    -2253, -2333, -2414, -2445, -2511, -2526, 1313, 1298, -2559, 1041, 1041, 1040, 1040, 1025,
    1025, 1024, 1024, 1022, 1007, 1021, 991, 1020, 975, 1019, 959, 687, 687, 1018, 1017, 671, 671,
    655, 655, 1016, 1015, 639, 639, 758, 758, 623, 623, 757, 607, 756, 591, 755, 575, 754, 559,
    543, 543, 1009, 783, -575, -621, -685, -749, 496, -590, 750, 749, 734, 748, 974, 989, 1003,
    958, 988, 973, 1002, 942, 987, 957, 972, 1001, 926, 986, 941, 971, 956, 1000, 910, 985, 925,
    999, 894, 970, -1071, -1087, -1102, 1390, -1135, 1436, 1509, 1451, 1374, -1151, 1405, 1358,
    1480, 1420, -1167, 1507, 1494, 1389, 1342, 1465, 1435, 1450, 1326, 1505, 1310, 1493, 1373,
    1479, 1404, 1492, 1464, 1419, 428, 443, 472, 397, 736, 526, 464, 464, 486, 457, 442, 471, 484,
    482, 1357, 1449, 1434, 1478, 1388, 1491, 1341, 1490, 1325, 1489, 1463, 1403, 1309, 1477, 1372,
    1448, 1418, 1433, 1476, 1356, 1462, 1387, -1439, 1475, 1340, 1447, 1402, 1474, 1324, 1461,
    1371, 1473, 269, 448, 1432, 1417, 1308, 1460, -1711, 1459, -1727, 1441, 1099, 1099, 1446, 1386,
    1431, 1401, -1743, 1289, 1083, 1083, 1160, 1160, 1458, 1445, 1067, 1067, 1370, 1457, 1307,
    1430, 1129, 1129, 1098, 1098, 268, 432, 267, 416, 266, 400, -1887, 1144, 1187, 1082, 1173,
    1113, 1186, 1066, 1050, 1158, 1128, 1143, 1172, 1097, 1171, 1081, 420, 391, 1157, 1112, 1170,
    1142, 1127, 1065, 1169, 1049, 1156, 1096, 1141, 1111, 1155, 1080, 1126, 1154, 1064, 1153, 1140,
    1095, 1048, -2159, 1125, 1110, 1137, -2175, 823, 823, 1139, 1138, 807, 807, 384, 264, 368, 263,
    868, 838, 853, 791, 867, 822, 852, 837, 866, 806, 865, 790, -2319, 851, 821, 836, 352, 262,
    850, 805, 849, -2399, 533, 533, 835, 820, 336, 261, 578, 548, 563, 577, 532, 532, 832, 772,
    562, 562, 547, 547, 305, 275, 560, 515, 290, 290, 288, 258,
];

// TAB32: 28 entries
static TAB32: [u8; 28] = [
    130, 162, 193, 209, 44, 28, 76, 140, 9, 9, 9, 9, 9, 9, 9, 9, 190, 254, 222, 238, 126, 94, 157,
    157, 109, 61, 173, 205,
];

// TAB33: 16 entries
static TAB33: [u8; 16] = [
    252, 236, 220, 204, 188, 172, 156, 140, 124, 108, 92, 76, 60, 44, 28, 12,
];

// TABINDEX: 32 entries
static TABINDEX: [i16; 32] = [
    0, 32, 64, 98, 0, 132, 180, 218, 292, 364, 426, 538, 648, 746, 0, 1126, 1460, 1460, 1460, 1460,
    1460, 1460, 1460, 1460, 1842, 1842, 1842, 1842, 1842, 1842, 1842, 1842,
];

// G_LINBITS: 32 entries
static G_LINBITS: [u8; 32] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 6, 8, 10, 13, 4, 5, 6, 7, 8, 9, 11,
    13,
];

/// Decode and requantize one granule's spectral lines into `dst` (length 576),
/// consuming the main-data bitstream up to the absolute bit position
/// `layer3gr_limit` (= granule start + `part2_3_length`). Port of minimp3's
/// `L3_huffman`.
///
/// `dst` must be pre-zeroed. `scf` holds the per-band requant multipliers from
/// [`decode_scalefactors`]. The Huffman codewords index `TABS` via `TABINDEX`;
/// escaped values pull `linbits` extra bits and go through [`pow_43`]. Every
/// shift is bounds-guarded so malformed main data cannot panic.
pub(crate) fn huffman(
    dst: &mut [f32],
    bs: &mut BitReader,
    gr: &GrInfo,
    scf: &[f32],
    layer3gr_limit: usize,
) {
    let buf = bs.buf();

    // Local big-endian bit cache (minimp3 PEEK/FLUSH/CHECK macros).
    let byte0 = bs.pos() / 8;
    let rd = |i: usize| -> u32 { u32::from(buf.get(i).copied().unwrap_or(0)) };
    let mut cache: u32 =
        ((rd(byte0) << 24) | (rd(byte0 + 1) << 16) | (rd(byte0 + 2) << 8) | rd(byte0 + 3))
            << (bs.pos() & 7);
    let mut sh: i32 = (bs.pos() & 7) as i32 - 8;
    let mut next = byte0 + 4;

    // `PEEK_BITS(n)` = top n bits; guarded so n==0 yields 0 instead of a shift
    // overflow (the reference relies on sub-table widths being >= 1).
    macro_rules! peek {
        ($n:expr) => {
            cache.checked_shr(32 - ($n as u32)).unwrap_or(0)
        };
    }
    macro_rules! flush {
        ($n:expr) => {{
            let n = $n as u32;
            cache <<= n;
            sh += n as i32;
        }};
    }
    macro_rules! check {
        () => {
            while sh >= 0 {
                cache |= rd(next).checked_shl(sh as u32).unwrap_or(0);
                next += 1;
                sh -= 8;
            }
        };
    }

    let sfb = gr.sfbtab;
    let mut sfb_idx = 0usize;
    let mut scf_idx = 0usize;
    let mut dst_idx = 0usize;
    let mut one = 0.0f32;
    let mut big_val_cnt = gr.big_values as i32;
    let mut ireg = 0usize;

    while big_val_cnt > 0 {
        let tab_num = gr.table_select[ireg] as usize;
        let mut sfb_cnt = gr.region_count[ireg] as i32;
        ireg += 1;
        let cb_base = TABINDEX[tab_num] as usize;
        let cb = |i: usize| -> i32 { i32::from(TABS.get(cb_base + i).copied().unwrap_or(0)) };
        let linbits = u32::from(G_LINBITS[tab_num]);

        loop {
            let np = i32::from(sfb[sfb_idx]) / 2;
            sfb_idx += 1;
            let pairs = big_val_cnt.min(np);
            one = scf[scf_idx];
            scf_idx += 1;

            for _ in 0..pairs {
                let mut w = 5u32;
                let mut leaf = cb(peek!(w) as usize);
                while leaf < 0 {
                    flush!(w);
                    w = (leaf & 7) as u32;
                    leaf = cb((peek!(w) as i32 - (leaf >> 3)) as usize);
                }
                flush!(leaf >> 8);

                for _ in 0..2 {
                    let lsb = leaf & 0x0F;
                    if linbits != 0 && lsb == 15 {
                        let val = lsb + peek!(linbits) as i32;
                        flush!(linbits);
                        check!();
                        let sign = if (cache as i32) < 0 { -1.0 } else { 1.0 };
                        if dst_idx < dst.len() {
                            dst[dst_idx] = one * pow_43(val) * sign;
                        }
                        flush!(1);
                    } else {
                        if dst_idx < dst.len() {
                            dst[dst_idx] =
                                G_POW43[(16 + lsb - 16 * (cache >> 31) as i32) as usize] * one;
                        }
                        flush!(if lsb != 0 { 1 } else { 0 });
                    }
                    dst_idx += 1;
                    leaf >>= 4;
                }
                check!();
            }

            big_val_cnt -= np;
            sfb_cnt -= 1;
            if !(big_val_cnt > 0 && sfb_cnt >= 0) {
                break;
            }
        }
    }

    // count1 region: quadruples of ±1 values until the granule bit budget runs out.
    let mut np = 1 - big_val_cnt;
    loop {
        let c1 = if gr.count1_table != 0 {
            &TAB33[..]
        } else {
            &TAB32[..]
        };
        let mut leaf = i32::from(c1[peek!(4u32) as usize]);
        if leaf & 8 == 0 {
            let extra = ((cache << 4)
                .checked_shr(32 - (leaf & 3) as u32)
                .unwrap_or(0)) as i32;
            let idx = (leaf >> 3) + extra;
            leaf = i32::from(c1.get(idx as usize).copied().unwrap_or(0));
        }
        flush!(leaf & 7);
        let bspos = next as i32 * 8 - 24 + sh;
        if bspos > layer3gr_limit as i32 {
            break;
        }

        macro_rules! reload {
            () => {{
                np -= 1;
                if np == 0 {
                    np = i32::from(sfb[sfb_idx]) / 2;
                    sfb_idx += 1;
                    if np == 0 {
                        break;
                    }
                    one = scf[scf_idx];
                    scf_idx += 1;
                }
            }};
        }
        macro_rules! deq {
            ($s:expr) => {
                if leaf & (128 >> $s) != 0 {
                    let v = if (cache as i32) < 0 { -one } else { one };
                    if dst_idx + $s < dst.len() {
                        dst[dst_idx + $s] = v;
                    }
                    flush!(1);
                }
            };
        }
        reload!();
        deq!(0usize);
        deq!(1usize);
        reload!();
        deq!(2usize);
        deq!(3usize);
        check!();
        dst_idx += 4;
    }

    bs.set_pos(layer3gr_limit);
}

// --- stereo processing ------------------------------------------------------

/// MS (mid/side) decode over `n` lines starting at `off`: `left = mid + side`,
/// `right = mid - side` (the `1/√2` is folded into the scalefactor gain). The
/// right channel is `off + 576` in the shared granule buffer.
pub(crate) fn midside_stereo(grbuf: &mut [f32], off: usize, n: usize) {
    for i in 0..n {
        let a = grbuf[off + i];
        let b = grbuf[off + i + 576];
        grbuf[off + i] = a + b;
        grbuf[off + i + 576] = a - b;
    }
}

/// Intensity decode of one band: pan the left channel's energy into both
/// channels by `(kl, kr)`. Port of minimp3's `L3_intensity_stereo_band`.
fn intensity_stereo_band(grbuf: &mut [f32], off: usize, n: usize, kl: f32, kr: f32) {
    for i in 0..n {
        grbuf[off + i + 576] = grbuf[off + i] * kr;
        grbuf[off + i] *= kl;
    }
}

/// Find, per short-window group (`i % 3`), the highest band with any nonzero
/// coefficient in `right`. Port of minimp3's `L3_stereo_top_band`.
fn stereo_top_band(right: &[f32], sfb: &[u8], nbands: usize, max_band: &mut [i32; 3]) {
    *max_band = [-1, -1, -1];
    let mut off = 0usize;
    for i in 0..nbands {
        let width = sfb[i] as usize;
        let mut k = 0usize;
        while k < width {
            if right[off + k] != 0.0 || right[off + k + 1] != 0.0 {
                max_band[i % 3] = i as i32;
                break;
            }
            k += 2;
        }
        off += width;
    }
}

/// Apply the per-band stereo transform: intensity where the band sits above the
/// coded top band with a valid position, else MS if enabled. Port of minimp3's
/// `L3_stereo_process`.
fn stereo_process(
    grbuf: &mut [f32],
    ist_pos: &[u8],
    sfb: &[u8],
    hdr: Header,
    max_band: &[i32; 3],
    mpeg2_sh: i32,
) {
    // Intensity pan coefficients for MPEG-1 (position 0..6, as (kl, kr) pairs).
    static G_PAN: [f32; 14] = [
        0.0, 1.0, 0.21132487, 0.78867513, 0.36602540, 0.63397460, 0.5, 0.5, 0.63397460, 0.36602540,
        0.78867513, 0.21132487, 1.0, 0.0,
    ];
    let max_pos: u32 = if hdr.is_mpeg1() { 7 } else { 64 };
    let mut off = 0usize;
    let mut i = 0usize;
    while sfb[i] != 0 {
        let ipos = u32::from(ist_pos[i]);
        let width = sfb[i] as usize;
        if i as i32 > max_band[i % 3] && ipos < max_pos {
            let s = if hdr.test_ms_stereo() {
                core::f32::consts::SQRT_2
            } else {
                1.0
            };
            let (kl, kr);
            if hdr.is_mpeg1() {
                kl = G_PAN[2 * ipos as usize];
                kr = G_PAN[2 * ipos as usize + 1];
            } else {
                let k = ldexp_q2(1.0, ((ipos as i32 + 1) >> 1) << mpeg2_sh);
                if ipos & 1 != 0 {
                    kl = k;
                    kr = 1.0;
                } else {
                    kl = 1.0;
                    kr = k;
                }
            }
            intensity_stereo_band(grbuf, off, width, kl * s, kr * s);
        } else if hdr.test_ms_stereo() {
            midside_stereo(grbuf, off, width);
        }
        off += width;
        i += 1;
    }
}

/// Orchestrate intensity stereo for one granule: locate the top coded band,
/// resolve the intensity position of the final band group, then run the
/// per-band transform. `ist_pos` is channel 1's intensity-position array and
/// `gr1_scalefac_compress` is channel 1's `scalefac_compress` (its low bit is
/// the MPEG-2 intensity shift). Port of minimp3's `L3_intensity_stereo`.
pub(crate) fn intensity_stereo(
    grbuf: &mut [f32],
    ist_pos: &mut [u8],
    gr: &GrInfo,
    hdr: Header,
    gr1_scalefac_compress: u16,
) {
    let n_sfb = (gr.n_long_sfb + gr.n_short_sfb) as usize;
    let max_blocks = if gr.n_short_sfb != 0 { 3 } else { 1 };
    let mut max_band = [0i32; 3];
    stereo_top_band(&grbuf[576..], gr.sfbtab, n_sfb, &mut max_band);
    if gr.n_long_sfb != 0 {
        let m = max_band[0].max(max_band[1]).max(max_band[2]);
        max_band = [m, m, m];
    }
    for (i, &mb) in max_band.iter().take(max_blocks).enumerate() {
        let default_pos: u8 = if hdr.is_mpeg1() { 3 } else { 0 };
        let itop = n_sfb - max_blocks + i;
        let prev = itop - max_blocks;
        ist_pos[itop] = if mb >= prev as i32 {
            default_pos
        } else {
            ist_pos[prev]
        };
    }
    stereo_process(
        grbuf,
        ist_pos,
        gr.sfbtab,
        hdr,
        &max_band,
        i32::from(gr1_scalefac_compress & 1),
    );
}

// --- reorder + alias reduction ----------------------------------------------

/// Reinterleave short-block coefficients from window-major to time order (the
/// three 6-sample windows of each band become consecutive), via `scratch`. Port
/// of minimp3's `L3_reorder`. `sfb` is the short-band width table.
pub(crate) fn reorder(grbuf: &mut [f32], scratch: &mut [f32], sfb: &[u8]) {
    let mut src = 0usize;
    let mut dst = 0usize;
    let mut si = 0usize;
    while sfb[si] != 0 {
        let len = sfb[si] as usize;
        for _ in 0..len {
            scratch[dst] = grbuf[src];
            scratch[dst + 1] = grbuf[src + len];
            scratch[dst + 2] = grbuf[src + 2 * len];
            dst += 3;
            src += 1;
        }
        si += 3;
        src += 2 * len;
    }
    grbuf[..dst].copy_from_slice(&scratch[..dst]);
}

/// Butterfly alias reduction across the `nbands` long-block subband boundaries.
/// Port of minimp3's `L3_antialias`.
pub(crate) fn antialias(grbuf: &mut [f32], nbands: usize) {
    static G_AA: [[f32; 8]; 2] = [
        [
            0.85749293, 0.88174200, 0.94962865, 0.98331459, 0.99551782, 0.99916056, 0.99989920,
            0.99999316,
        ],
        [
            0.51449576, 0.47173197, 0.31337745, 0.18191320, 0.09457419, 0.04096558, 0.01419856,
            0.00369997,
        ],
    ];
    let mut base = 0usize;
    for _ in 0..nbands {
        for i in 0..8 {
            let u = grbuf[base + 18 + i];
            let d = grbuf[base + 17 - i];
            grbuf[base + 18 + i] = u * G_AA[0][i] - d * G_AA[1][i];
            grbuf[base + 17 - i] = u * G_AA[1][i] + d * G_AA[0][i];
        }
        base += 18;
    }
}

// --- IMDCT + windowing + overlap --------------------------------------------

static G_TWID9: [f32; 18] = [
    0.73727734, 0.79335334, 0.84339145, 0.88701083, 0.92387953, 0.95371695, 0.97629601, 0.99144486,
    0.99904822, 0.67559021, 0.60876143, 0.53729961, 0.46174861, 0.38268343, 0.30070580, 0.21643961,
    0.13052619, 0.04361938,
];

static G_TWID3: [f32; 6] = [
    0.79335334, 0.92387953, 0.99144486, 0.60876143, 0.38268343, 0.13052619,
];

/// IMDCT windows: index 0 is the normal long window, index 1 the "stop" window
/// (used when a long block follows short blocks).
static G_MDCT_WINDOW: [[f32; 18]; 2] = [
    [
        0.99904822, 0.99144486, 0.97629601, 0.95371695, 0.92387953, 0.88701083, 0.84339145,
        0.79335334, 0.73727734, 0.04361938, 0.13052619, 0.21643961, 0.30070580, 0.38268343,
        0.46174861, 0.53729961, 0.60876143, 0.67559021,
    ],
    [
        1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 0.99144486, 0.92387953, 0.79335334, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.13052619, 0.38268343, 0.60876143,
    ],
];

/// In-place 9-point DCT-III kernel (minimp3's `L3_dct3_9`).
fn dct3_9(y: &mut [f32; 9]) {
    let (mut s0, s2, s4, mut s6, s8) = (y[0], y[2], y[4], y[6], y[8]);
    let t0 = s0 + s6 * 0.5;
    s0 -= s6;
    let t4 = (s4 + s2) * 0.93969262;
    let t2 = (s8 + s2) * 0.76604444;
    s6 = (s4 - s8) * 0.17364818;
    // minimp3's `s4 += s8 - s2`: the subtraction rounds before the add, which is
    // not the same in f32 as `(s4 + s8) - s2`. Preserve the reference grouping.
    let s4b = s4 + (s8 - s2);

    let s2b = s0 - s4b * 0.5;
    y[4] = s4b + s0;
    let s8b = t0 - t2 + s6;
    let s0b = t0 - t4 + t2;
    let s4c = t0 + t4 - s6;

    let (s1, s3, s5, s7) = (y[1], y[3], y[5], y[7]);
    let s3b = s3 * 0.86602540;
    let t0b = (s5 + s1) * 0.98480775;
    let t4b = (s5 - s7) * 0.34202014;
    let t2b = (s1 + s7) * 0.64278761;
    let s1b = (s1 - s5 - s7) * 0.86602540;

    let s5b = t0b - s3b - t2b;
    let s7b = t4b - s3b - t0b;
    let s3c = t4b + s3b - t2b;

    y[0] = s4c - s7b;
    y[1] = s2b + s1b;
    y[2] = s0b - s3c;
    y[3] = s8b + s5b;
    y[5] = s8b - s5b;
    y[6] = s0b + s3c;
    y[7] = s2b - s1b;
    y[8] = s4c + s7b;
}

/// 18-point IMDCT with windowing and overlap-add for `nbands` long subbands.
/// Outputs 18 samples per band into `grbuf`; carries 9 values per band in
/// `overlap` to the next granule. Port of minimp3's `L3_imdct36`.
pub(crate) fn imdct36(grbuf: &mut [f32], overlap: &mut [f32], window: &[f32; 18], nbands: usize) {
    let mut gb = 0usize;
    let mut ov = 0usize;
    for _ in 0..nbands {
        let mut co = [0.0f32; 9];
        let mut si = [0.0f32; 9];
        co[0] = -grbuf[gb];
        si[0] = grbuf[gb + 17];
        for i in 0..4 {
            si[8 - 2 * i] = grbuf[gb + 4 * i + 1] - grbuf[gb + 4 * i + 2];
            co[1 + 2 * i] = grbuf[gb + 4 * i + 1] + grbuf[gb + 4 * i + 2];
            si[7 - 2 * i] = grbuf[gb + 4 * i + 4] - grbuf[gb + 4 * i + 3];
            co[2 + 2 * i] = -(grbuf[gb + 4 * i + 3] + grbuf[gb + 4 * i + 4]);
        }
        dct3_9(&mut co);
        dct3_9(&mut si);
        si[1] = -si[1];
        si[3] = -si[3];
        si[5] = -si[5];
        si[7] = -si[7];
        for i in 0..9 {
            let ovl = overlap[ov + i];
            let sum = co[i] * G_TWID9[9 + i] + si[i] * G_TWID9[i];
            overlap[ov + i] = co[i] * G_TWID9[i] - si[i] * G_TWID9[9 + i];
            grbuf[gb + i] = ovl * window[i] - sum * window[9 + i];
            grbuf[gb + 17 - i] = ovl * window[9 + i] + sum * window[i];
        }
        gb += 18;
        ov += 9;
    }
}

/// 3-point inverse DCT kernel (minimp3's `L3_idct3`).
fn idct3(x0: f32, x1: f32, x2: f32, dst: &mut [f32]) {
    let m1 = x1 * 0.86602540;
    let a1 = x0 - x2 * 0.5;
    dst[1] = x0 + x2;
    dst[0] = a1 + m1;
    dst[2] = a1 - m1;
}

/// 6-point IMDCT (one short window), windowed and overlapped. `x` is read with a
/// stride of 3. Port of minimp3's `L3_imdct12`.
fn imdct12(x: &[f32], dst: &mut [f32], overlap: &mut [f32]) {
    let mut co = [0.0f32; 3];
    let mut si = [0.0f32; 3];
    idct3(-x[0], x[6] + x[3], x[12] + x[9], &mut co);
    idct3(x[15], x[12] - x[9], x[6] - x[3], &mut si);
    si[1] = -si[1];
    for i in 0..3 {
        let ovl = overlap[i];
        let sum = co[i] * G_TWID3[3 + i] + si[i] * G_TWID3[i];
        overlap[i] = co[i] * G_TWID3[i] - si[i] * G_TWID3[3 + i];
        dst[i] = ovl * G_TWID3[2 - i] - sum * G_TWID3[5 - i];
        dst[5 - i] = ovl * G_TWID3[5 - i] + sum * G_TWID3[2 - i];
    }
}

/// IMDCT for `nbands` short-block subbands (three 6-point windows per band).
/// Port of minimp3's `L3_imdct_short`.
pub(crate) fn imdct_short(grbuf: &mut [f32], overlap: &mut [f32], nbands: usize) {
    let mut gb = 0usize;
    let mut ov = 0usize;
    for _ in 0..nbands {
        let mut tmp = [0.0f32; 18];
        tmp.copy_from_slice(&grbuf[gb..gb + 18]);
        let mut ol = [0.0f32; 9];
        ol.copy_from_slice(&overlap[ov..ov + 9]);
        // grbuf[0..6] takes the previous overlap head.
        grbuf[gb..gb + 6].copy_from_slice(&ol[..6]);
        let (head, tail) = ol.split_at_mut(6);
        {
            let (d1, d2) = grbuf[gb + 6..gb + 18].split_at_mut(6);
            imdct12(&tmp, d1, tail);
            imdct12(&tmp[1..], d2, tail);
        }
        imdct12(&tmp[2..], head, tail);
        overlap[ov..ov + 9].copy_from_slice(&ol);
        gb += 18;
        ov += 9;
    }
}

/// Frequency inversion for odd time samples of odd subbands (prepares the
/// polyphase synthesis input). Port of minimp3's `L3_change_sign`.
pub(crate) fn change_sign(grbuf: &mut [f32]) {
    let mut base = 18usize;
    let mut b = 0;
    while b < 32 {
        let mut i = 1usize;
        while i < 18 {
            grbuf[base + i] = -grbuf[base + i];
            i += 2;
        }
        b += 2;
        base += 36;
    }
}

/// IMDCT one granule: long bands with the normal/stop window, then either the
/// short-block IMDCT or a long IMDCT for the remaining bands. Port of minimp3's
/// `L3_imdct_gr`.
pub(crate) fn imdct_gr(
    grbuf: &mut [f32],
    overlap: &mut [f32],
    block_type: u8,
    n_long_bands: usize,
) {
    if n_long_bands > 0 {
        imdct36(grbuf, overlap, &G_MDCT_WINDOW[0], n_long_bands);
    }
    let gb = 18 * n_long_bands;
    let ov = 9 * n_long_bands;
    let rest = 32 - n_long_bands;
    if block_type == SHORT_BLOCK_TYPE {
        imdct_short(&mut grbuf[gb..], &mut overlap[ov..], rest);
    } else {
        let win = if block_type == STOP_BLOCK_TYPE {
            &G_MDCT_WINDOW[1]
        } else {
            &G_MDCT_WINDOW[0]
        };
        imdct36(&mut grbuf[gb..], &mut overlap[ov..], win, rest);
    }
}

// --- synthesis polyphase filterbank -----------------------------------------

/// Round + clamp one filterbank output to the 16-bit range (as `i32`, matching
/// the crate's PCM convention). Port of minimp3's `mp3d_scale_pcm`.
fn scale_pcm(sample: f32) -> i32 {
    if sample >= 32766.5 {
        return 32767;
    }
    if sample <= -32767.5 {
        return -32768;
    }
    let mut s = (sample + 0.5) as i32; // truncates toward zero
    s -= i32::from(s < 0); // bias away from zero for negatives
    s
}

/// 32-point scaled DCT-II over the 18 time slots of one channel's `grbuf`
/// (in place). Port of minimp3's scalar `mp3d_DCT_II`.
fn dct2(grbuf: &mut [f32], n: usize) {
    for k in 0..n {
        let mut t = [[0.0f32; 8]; 4];
        for i in 0..8 {
            let x0 = grbuf[k + i * 18];
            let x1 = grbuf[k + (15 - i) * 18];
            let x2 = grbuf[k + (16 + i) * 18];
            let x3 = grbuf[k + (31 - i) * 18];
            let t0 = x0 + x3;
            let t1 = x1 + x2;
            let t2 = (x1 - x2) * G_SEC[3 * i];
            let t3 = (x0 - x3) * G_SEC[3 * i + 1];
            t[0][i] = t0 + t1;
            t[1][i] = (t0 - t1) * G_SEC[3 * i + 2];
            t[2][i] = t3 + t2;
            t[3][i] = (t3 - t2) * G_SEC[3 * i + 2];
        }
        for row in &mut t {
            let mut x0 = row[0];
            let mut x1 = row[1];
            let mut x2 = row[2];
            let mut x3 = row[3];
            let x4o = row[4];
            let x5o = row[5];
            let x6o = row[6];
            let x7o = row[7];
            let xt = x0 - x7o;
            x0 += x7o;
            let mut x7 = x1 - x6o;
            x1 += x6o;
            let mut x6 = x2 - x5o;
            x2 += x5o;
            let mut x5 = x3 - x4o;
            x3 += x4o;
            let x4 = x0 - x3;
            x0 += x3;
            x3 = x1 - x2;
            x1 += x2;
            row[0] = x0 + x1;
            row[4] = (x0 - x1) * 0.70710677;
            x5 += x6;
            x6 = (x6 + x7) * 0.70710677;
            x7 += xt;
            x3 = (x3 + x4) * 0.70710677;
            x5 -= x7 * 0.198912367;
            x7 += x5 * 0.382683432;
            x5 -= x7 * 0.198912367;
            let x0b = xt - x6;
            let xtb = xt + x6;
            row[1] = (xtb + x7) * 0.50979561;
            row[2] = (x4 + x3) * 0.54119611;
            row[3] = (x0b - x5) * 0.60134488;
            row[5] = (x0b + x5) * 0.89997619;
            row[6] = (x4 - x3) * 1.30656302;
            row[7] = (xtb - x7) * 2.56291556;
        }
        let mut yo = k;
        for i in 0..7 {
            grbuf[yo] = t[0][i];
            grbuf[yo + 18] = t[2][i] + t[3][i] + t[3][i + 1];
            grbuf[yo + 36] = t[1][i] + t[1][i + 1];
            grbuf[yo + 54] = t[2][i + 1] + t[3][i] + t[3][i + 1];
            yo += 72;
        }
        grbuf[yo] = t[0][7];
        grbuf[yo + 18] = t[2][7] + t[3][7];
        grbuf[yo + 36] = t[1][7];
        grbuf[yo + 54] = t[3][7];
    }
}

/// Compute two windowed PCM samples from a 64-strided delay-line window `z`
/// (offset `z0` into `lins`). Port of minimp3's `mp3d_synth_pair`.
fn synth_pair(pcm: &mut [i32], dst: usize, nch: usize, z: &[f32], z0: usize) {
    let mut a = (z[z0 + 14 * 64] - z[z0]) * 29.0;
    a += (z[z0 + 64] + z[z0 + 13 * 64]) * 213.0;
    a += (z[z0 + 12 * 64] - z[z0 + 2 * 64]) * 459.0;
    a += (z[z0 + 3 * 64] + z[z0 + 11 * 64]) * 2037.0;
    a += (z[z0 + 10 * 64] - z[z0 + 4 * 64]) * 5153.0;
    a += (z[z0 + 5 * 64] + z[z0 + 9 * 64]) * 6574.0;
    a += (z[z0 + 8 * 64] - z[z0 + 6 * 64]) * 37489.0;
    a += z[z0 + 7 * 64] * 75038.0;
    pcm[dst] = scale_pcm(a);

    let z1 = z0 + 2;
    let mut a = z[z1 + 14 * 64] * 104.0;
    a += z[z1 + 12 * 64] * 1567.0;
    a += z[z1 + 10 * 64] * 9727.0;
    a += z[z1 + 8 * 64] * 64019.0;
    a += z[z1 + 6 * 64] * -9975.0;
    a += z[z1 + 4 * 64] * -45.0;
    a += z[z1 + 2 * 64] * 146.0;
    a += z[z1] * -5.0;
    pcm[dst + 16 * nch] = scale_pcm(a);
}

/// Scaling constants for `mp3d_DCT_II` (three per sub-butterfly).
static G_SEC: [f32; 24] = [
    10.19000816,
    0.50060302,
    0.50241929,
    3.40760851,
    0.50547093,
    0.52249861,
    2.05778098,
    0.51544732,
    0.56694406,
    1.48416460,
    0.53104258,
    0.64682180,
    1.16943991,
    0.55310392,
    0.78815460,
    0.97256821,
    0.58293498,
    1.06067765,
    0.83934963,
    0.62250412,
    1.72244716,
    0.74453628,
    0.67480832,
    5.10114861,
];

/// Polyphase synthesis window coefficients (15 rows of 16).
static G_WIN: [f32; 240] = [
    -1.0, 26.0, -31.0, 208.0, 218.0, 401.0, -519.0, 2063.0, 2000.0, 4788.0, -5517.0, 7134.0,
    5959.0, 35640.0, -39336.0, 74992.0, -1.0, 24.0, -35.0, 202.0, 222.0, 347.0, -581.0, 2080.0,
    1952.0, 4425.0, -5879.0, 7640.0, 5288.0, 33791.0, -41176.0, 74856.0, -1.0, 21.0, -38.0, 196.0,
    225.0, 294.0, -645.0, 2087.0, 1893.0, 4063.0, -6237.0, 8092.0, 4561.0, 31947.0, -43006.0,
    74630.0, -1.0, 19.0, -41.0, 190.0, 227.0, 244.0, -711.0, 2085.0, 1822.0, 3705.0, -6589.0,
    8492.0, 3776.0, 30112.0, -44821.0, 74313.0, -1.0, 17.0, -45.0, 183.0, 228.0, 197.0, -779.0,
    2075.0, 1739.0, 3351.0, -6935.0, 8840.0, 2935.0, 28289.0, -46617.0, 73908.0, -1.0, 16.0, -49.0,
    176.0, 228.0, 153.0, -848.0, 2057.0, 1644.0, 3004.0, -7271.0, 9139.0, 2037.0, 26482.0,
    -48390.0, 73415.0, -2.0, 14.0, -53.0, 169.0, 227.0, 111.0, -919.0, 2032.0, 1535.0, 2663.0,
    -7597.0, 9389.0, 1082.0, 24694.0, -50137.0, 72835.0, -2.0, 13.0, -58.0, 161.0, 224.0, 72.0,
    -991.0, 2001.0, 1414.0, 2330.0, -7910.0, 9592.0, 70.0, 22929.0, -51853.0, 72169.0, -2.0, 11.0,
    -63.0, 154.0, 221.0, 36.0, -1064.0, 1962.0, 1280.0, 2006.0, -8209.0, 9750.0, -998.0, 21189.0,
    -53534.0, 71420.0, -2.0, 10.0, -68.0, 147.0, 215.0, 2.0, -1137.0, 1919.0, 1131.0, 1692.0,
    -8491.0, 9863.0, -2122.0, 19478.0, -55178.0, 70590.0, -3.0, 9.0, -73.0, 139.0, 208.0, -29.0,
    -1210.0, 1870.0, 970.0, 1388.0, -8755.0, 9935.0, -3300.0, 17799.0, -56778.0, 69679.0, -3.0,
    8.0, -79.0, 132.0, 200.0, -57.0, -1283.0, 1817.0, 794.0, 1095.0, -8998.0, 9966.0, -4533.0,
    16155.0, -58333.0, 68692.0, -4.0, 7.0, -85.0, 125.0, 189.0, -83.0, -1356.0, 1759.0, 605.0,
    814.0, -9219.0, 9959.0, -5818.0, 14548.0, -59838.0, 67629.0, -4.0, 7.0, -91.0, 117.0, 177.0,
    -106.0, -1428.0, 1698.0, 402.0, 545.0, -9416.0, 9916.0, -7154.0, 12980.0, -61289.0, 66494.0,
    -5.0, 6.0, -97.0, 111.0, 163.0, -127.0, -1498.0, 1634.0, 185.0, 288.0, -9585.0, 9838.0,
    -8540.0, 11455.0, -62684.0, 65290.0,
];

/// One polyphase synthesis step over a pair of subbands, writing 64 interleaved
/// PCM frames. `xl` is the band offset into `grbuf` (right channel is `+576`),
/// `lin` the base offset of this call's delay-line window in `lins`, and `dst`
/// the PCM output offset. Port of minimp3's scalar `mp3d_synth`.
#[allow(clippy::too_many_arguments)]
fn synth(
    grbuf: &[f32],
    xl: usize,
    nch: usize,
    lins: &mut [f32],
    lin: usize,
    pcm: &mut [i32],
    dst: usize,
) {
    let xr = xl + 576 * (nch - 1);
    let dstl = dst;
    let dstr = dst + (nch - 1);
    let zbase = lin + 15 * 64;

    lins[zbase + 4 * 15] = grbuf[xl + 18 * 16];
    lins[zbase + 4 * 15 + 1] = grbuf[xr + 18 * 16];
    lins[zbase + 4 * 15 + 2] = grbuf[xl];
    lins[zbase + 4 * 15 + 3] = grbuf[xr];
    lins[zbase + 4 * 31] = grbuf[xl + 1 + 18 * 16];
    lins[zbase + 4 * 31 + 1] = grbuf[xr + 1 + 18 * 16];
    lins[zbase + 4 * 31 + 2] = grbuf[xl + 1];
    lins[zbase + 4 * 31 + 3] = grbuf[xr + 1];

    synth_pair(pcm, dstr, nch, lins, lin + 4 * 15 + 1);
    synth_pair(pcm, dstr + 32 * nch, nch, lins, lin + 4 * 15 + 64 + 1);
    synth_pair(pcm, dstl, nch, lins, lin + 4 * 15);
    synth_pair(pcm, dstl + 32 * nch, nch, lins, lin + 4 * 15 + 64);

    let mut w = 0usize;
    let zb = zbase as isize;
    let mut i: i32 = 14;
    while i >= 0 {
        let iu = i as usize;
        lins[zbase + 4 * iu] = grbuf[xl + 18 * (31 - iu)];
        lins[zbase + 4 * iu + 1] = grbuf[xr + 18 * (31 - iu)];
        lins[zbase + 4 * iu + 2] = grbuf[xl + 1 + 18 * (31 - iu)];
        lins[zbase + 4 * iu + 3] = grbuf[xr + 1 + 18 * (31 - iu)];
        lins[zbase + 4 * (iu + 16)] = grbuf[xl + 1 + 18 * (1 + iu)];
        lins[zbase + 4 * (iu + 16) + 1] = grbuf[xr + 1 + 18 * (1 + iu)];
        lins[(zb + 4 * (i as isize - 16) + 2) as usize] = grbuf[xl + 18 * (1 + iu)];
        lins[(zb + 4 * (i as isize - 16) + 3) as usize] = grbuf[xr + 18 * (1 + iu)];

        let mut a = [0.0f32; 4];
        let mut b = [0.0f32; 4];
        for k in 0..8i32 {
            let w0 = G_WIN[w];
            let w1 = G_WIN[w + 1];
            w += 2;
            let vz = (zb + 4 * i as isize - k as isize * 64) as usize;
            let vy = (zb + 4 * i as isize - (15 - k) as isize * 64) as usize;
            for j in 0..4 {
                let z = lins[vz + j];
                let y = lins[vy + j];
                if k == 0 {
                    b[j] = z * w1 + y * w0;
                    a[j] = z * w0 - y * w1;
                } else if k % 2 == 1 {
                    b[j] += z * w1 + y * w0;
                    a[j] += y * w1 - z * w0;
                } else {
                    b[j] += z * w1 + y * w0;
                    a[j] += z * w0 - y * w1;
                }
            }
        }

        pcm[dstr + (15 - iu) * nch] = scale_pcm(a[1]);
        pcm[dstr + (17 + iu) * nch] = scale_pcm(b[1]);
        pcm[dstl + (15 - iu) * nch] = scale_pcm(a[0]);
        pcm[dstl + (17 + iu) * nch] = scale_pcm(b[0]);
        pcm[dstr + (47 - iu) * nch] = scale_pcm(a[3]);
        pcm[dstr + (49 + iu) * nch] = scale_pcm(b[3]);
        pcm[dstl + (47 - iu) * nch] = scale_pcm(a[2]);
        pcm[dstl + (49 + iu) * nch] = scale_pcm(b[2]);

        i -= 1;
    }
}

/// Synthesize one granule (`nbands` × 18 subband samples → 576 PCM frames per
/// channel) into `pcm` at `pcm_off`, updating the persistent filterbank delay
/// line `qmf_state`. `lins` is scratch. Port of minimp3's `mp3d_synth_granule`.
#[allow(clippy::too_many_arguments)]
fn synth_granule(
    qmf_state: &mut [f32],
    grbuf: &mut [f32],
    nbands: usize,
    nch: usize,
    pcm: &mut [i32],
    pcm_off: usize,
    lins: &mut [f32],
) {
    for ch in 0..nch {
        dct2(&mut grbuf[576 * ch..], nbands);
    }
    lins[..15 * 64].copy_from_slice(&qmf_state[..15 * 64]);
    let mut i = 0usize;
    while i < nbands {
        synth(grbuf, i, nch, lins, i * 64, pcm, pcm_off + 32 * nch * i);
        i += 2;
    }
    if nch == 1 {
        // minimp3 default: only even delay-line entries are refreshed for mono.
        let mut k = 0usize;
        while k < 15 * 64 {
            qmf_state[k] = lins[nbands * 64 + k];
            k += 2;
        }
    } else {
        qmf_state[..15 * 64].copy_from_slice(&lins[nbands * 64..nbands * 64 + 15 * 64]);
    }
}

// --- granule + frame orchestration ------------------------------------------

/// Decode one granule (both channels): scalefactors + Huffman per channel, then
/// stereo processing, reorder, alias reduction, IMDCT, and frequency inversion.
/// Port of minimp3's `L3_decode`. `gr` is this granule's per-channel side info,
/// `bs` the shared main-data reader (advanced across granules).
#[allow(clippy::too_many_arguments)]
fn l3_decode(
    bs: &mut BitReader,
    hdr: Header,
    gr: &[GrInfo],
    grbuf: &mut [f32],
    scf: &mut [f32],
    ist_pos: &mut [[u8; 39]; 2],
    overlap: &mut [[f32; 9 * 32]; 2],
    scratch: &mut [f32],
    nch: usize,
) {
    for ch in 0..nch {
        let layer3gr_limit = bs.pos() + gr[ch].part_23_length as usize;
        decode_scalefactors(hdr, &mut ist_pos[ch], bs, &gr[ch], scf, ch);
        huffman(
            &mut grbuf[576 * ch..576 * ch + 576],
            bs,
            &gr[ch],
            scf,
            layer3gr_limit,
        );
    }

    if hdr.test_i_stereo() {
        let gr1_sfc = gr[1].scalefac_compress;
        intensity_stereo(grbuf, &mut ist_pos[1], &gr[0], hdr, gr1_sfc);
    } else if hdr.is_ms_stereo() {
        midside_stereo(grbuf, 0, 576);
    }

    let sr2 = hdr.my_sample_rate_idx() == 2;
    for ch in 0..nch {
        let n_long_bands: usize =
            (if gr[ch].mixed_block_flag != 0 { 2 } else { 0 }) << usize::from(sr2);
        // -1 in the reference (no antialias) maps to 0 bands here.
        let aa_bands = if gr[ch].n_short_sfb != 0 {
            n_long_bands.saturating_sub(1)
        } else {
            31
        };
        if gr[ch].n_short_sfb != 0 {
            let n_long_sfb = gr[ch].n_long_sfb as usize;
            reorder(
                &mut grbuf[576 * ch + n_long_bands * 18..576 * ch + 576],
                scratch,
                &gr[ch].sfbtab[n_long_sfb..],
            );
        }
        antialias(&mut grbuf[576 * ch..576 * ch + 576], aa_bands);
        imdct_gr(
            &mut grbuf[576 * ch..576 * ch + 576],
            &mut overlap[ch],
            gr[ch].block_type,
            n_long_bands,
        );
        change_sign(&mut grbuf[576 * ch..576 * ch + 576]);
    }
}

impl Mp3Codec {
    /// Decode one MPEG Layer III frame (`frame` = 4-byte header + optional CRC +
    /// side info + main data) into interleaved 16-bit PCM (`i32`) at the start of
    /// `pcm`. Returns the number of interleaved samples written, or `None` if the
    /// frame is malformed / unsupported. A return of `Some(0)` means the frame was
    /// consumed (reservoir updated) but produced no audio (insufficient reservoir
    /// history, e.g. at stream start or after a seek).
    ///
    /// Port of the Layer III path of minimp3's `mp3dec_decode_frame`.
    pub(crate) fn decode_frame(&mut self, frame: &[u8], pcm: &mut [i32]) -> Option<usize> {
        if frame.len() < 4 {
            return None;
        }
        let hdr = Header([frame[0], frame[1], frame[2], frame[3]]);
        // Layer bits: 1 = Layer III (handled here), 2 = Layer II, 3 = Layer I
        // (both routed to the self-contained subband decoder).
        if hdr.layer() != 1 {
            return self.l12.decode_frame(frame, pcm);
        }
        let nch = hdr.channels();
        let granules = hdr.granules();

        // Bit reader over the frame after the 4-byte header; skip the CRC word.
        let mut bs_frame = BitReader::new(&frame[4..]);
        if hdr.is_crc() {
            bs_frame.get_bits(16);
        }

        let main_data_begin = read_side_info(&mut bs_frame, &mut self.gr_info, hdr)?;
        if bs_frame.pos() > bs_frame.limit() {
            return None;
        }

        // Current frame's main data = the bytes after the side info.
        let md_start = 4 + bs_frame.pos() / 8;
        let frame_main = frame.get(md_start..)?;
        let (maindata_len, enough) = self.restore_reservoir(frame_main, main_data_begin);

        let mut produced = 0usize;
        if enough {
            let mut bs = BitReader::with_limit(&self.maindata, maindata_len);
            for igr in 0..granules {
                self.grbuf[..2 * 576].fill(0.0);
                let base = igr * nch;
                l3_decode(
                    &mut bs,
                    hdr,
                    &self.gr_info[base..base + nch],
                    &mut self.grbuf,
                    &mut self.scf,
                    &mut self.ist_pos,
                    &mut self.mdct_overlap,
                    &mut self.syn,
                    nch,
                );
                synth_granule(
                    &mut self.qmf_state,
                    &mut self.grbuf,
                    18,
                    nch,
                    pcm,
                    igr * 576 * nch,
                    &mut self.syn,
                );
            }
            produced = granules * 576 * nch;
            let (fp, fl) = (bs.pos(), bs.limit());
            self.save_reservoir(fp, fl);
        } else {
            // Not enough reservoir to decode, but still update it for the next frame.
            self.save_reservoir(0, maindata_len * 8);
        }
        Some(produced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MSB-first bit writer for hand-building bitstreams in tests.
    pub(super) struct BitWriter {
        pub bytes: Vec<u8>,
        cur: u8,
        nbits: u8,
    }

    impl BitWriter {
        pub(super) fn new() -> Self {
            Self {
                bytes: Vec::new(),
                cur: 0,
                nbits: 0,
            }
        }

        /// Write the low `n` bits of `val`, MSB-first.
        pub(super) fn bits(&mut self, val: u32, n: u32) {
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

        /// Flush to a byte boundary (zero-padded) and return the bytes.
        pub(super) fn into_bytes(mut self) -> Vec<u8> {
            if self.nbits > 0 {
                let pad = 8 - self.nbits;
                self.bits(0, u32::from(pad));
            }
            self.bytes
        }
    }

    #[test]
    fn reads_single_bits_msb_first() {
        // 0b1011_0010
        let mut r = BitReader::new(&[0b1011_0010]);
        assert_eq!(r.get_bits(1), 1);
        assert_eq!(r.get_bits(1), 0);
        assert_eq!(r.get_bits(1), 1);
        assert_eq!(r.get_bits(1), 1);
        assert_eq!(r.get_bits(4), 0b0010);
        assert_eq!(r.pos(), 8);
    }

    #[test]
    fn reads_multibit_fields_across_byte_boundaries() {
        // Bytes 0xAB, 0xCD, 0xEF = 1010_1011 1100_1101 1110_1111.
        let data = [0xAB, 0xCD, 0xEF];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get_bits(12), 0xABC); // first 12 bits
        assert_eq!(r.get_bits(12), 0xDEF); // next 12 bits
        assert_eq!(r.pos(), 24);
    }

    #[test]
    fn reads_full_24_and_8_bit_words() {
        let data = [0x12, 0x34, 0x56, 0x78];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get_bits(24), 0x123456);
        assert_eq!(r.get_bits(8), 0x78);
    }

    #[test]
    fn zero_width_read_is_noop() {
        let mut r = BitReader::new(&[0xFF]);
        assert_eq!(r.get_bits(0), 0);
        assert_eq!(r.pos(), 0);
    }

    #[test]
    fn overrun_returns_zero_but_advances() {
        let mut r = BitReader::with_limit(&[0xFF, 0xFF], 1); // limit = 8 bits
        assert_eq!(r.get_bits(4), 0xF);
        // Next read crosses the limit: yields 0 but the cursor still moves.
        assert_eq!(r.get_bits(8), 0);
        assert_eq!(r.pos(), 12);
        assert!(r.pos() > r.limit());
    }

    #[test]
    fn round_trips_a_written_bitstream() {
        let mut w = BitWriter::new();
        w.bits(0x1FF, 9); // main_data_begin-like 9-bit field
        w.bits(0x5, 3);
        w.bits(0xABCD, 16);
        w.bits(0, 1);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.get_bits(9), 0x1FF);
        assert_eq!(r.get_bits(3), 0x5);
        assert_eq!(r.get_bits(16), 0xABCD);
        assert_eq!(r.get_bits(1), 0);
    }

    // -- header parsing -------------------------------------------------------

    #[test]
    fn parses_mpeg1_stereo_no_crc_header() {
        // 0xFF 0xFB: sync + MPEG-1 + Layer III + protection=1 (no CRC).
        // 0x90: bitrate idx 9, sample-rate idx 0, no padding.
        // 0x00: channel mode 00 = stereo.
        let h = Header([0xFF, 0xFB, 0x90, 0x00]);
        assert!(h.is_mpeg1());
        assert!(!h.is_crc());
        assert!(!h.is_mono());
        assert_eq!(h.channels(), 2);
        assert_eq!(h.layer(), 1); // Layer III encodes as 1
        assert_eq!(h.sample_rate_idx(), 0);
        assert_eq!(h.my_sample_rate_idx(), 6); // MPEG-1 base offset
        assert_eq!(h.granules(), 2);
    }

    #[test]
    fn parses_mono_crc_and_joint_stereo_flags() {
        // 0xFA: protection=0 -> CRC present. Mono channel mode (0xC0).
        let mono = Header([0xFF, 0xFA, 0x90, 0xC0]);
        assert!(mono.is_crc());
        assert!(mono.is_mono());
        assert_eq!(mono.channels(), 1);

        // Joint stereo, mode-ext 11 (0x70): both MS and intensity on.
        let joint = Header([0xFF, 0xFB, 0x90, 0x70]);
        assert!(joint.is_ms_stereo());
        assert!(joint.test_ms_stereo());
        assert!(joint.test_i_stereo());
        assert!(!joint.is_mono());

        // Joint stereo, mode-ext 10 (0x60): MS on, intensity off.
        let ms_only = Header([0xFF, 0xFB, 0x90, 0x60]);
        assert!(ms_only.test_ms_stereo());
        assert!(!ms_only.test_i_stereo());
    }

    #[test]
    fn parses_mpeg2_and_mpeg25_families() {
        // MPEG-2: h[1] bit4=1, bit3=0 -> 0xF3 (1111_0011).
        let m2 = Header([0xFF, 0xF3, 0x90, 0x00]);
        assert!(!m2.is_mpeg1());
        assert_eq!(m2.my_sample_rate_idx(), 3);
        assert_eq!(m2.granules(), 1);

        // MPEG-2.5: h[1] bit4=0, bit3=0 -> 0xE3 (1110_0011).
        let m25 = Header([0xFF, 0xE3, 0x90, 0x00]);
        assert!(!m25.is_mpeg1());
        assert_eq!(m25.my_sample_rate_idx(), 0);
    }

    // -- bit reservoir --------------------------------------------------------

    #[test]
    fn reservoir_assembles_main_data_across_frames() {
        let mut c = Mp3Codec::new();

        // Frame A: nothing in the reservoir, main_data_begin = 0.
        let a = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let (len_a, enough_a) = c.restore_reservoir(&a, 0);
        assert_eq!(len_a, 8);
        assert!(enough_a);
        assert_eq!(&c.maindata[..8], &a);

        // Suppose decoding consumed 5 bytes (40 bits) of an 8-byte main-data
        // buffer; the trailing 3 bytes become the reservoir.
        c.save_reservoir(40, len_a * 8);
        assert_eq!(c.reserv, 3);
        assert_eq!(&c.reserv_buf[..3], &[6, 7, 8]);

        // Frame B references 2 bytes of history: the assembled main data must be
        // the reservoir tail [7, 8] followed by this frame's bytes.
        let b = [9u8, 10, 11, 12];
        let (len_b, enough_b) = c.restore_reservoir(&b, 2);
        assert_eq!(len_b, 6);
        assert!(enough_b);
        assert_eq!(&c.maindata[..6], &[7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn reservoir_reports_underflow_but_still_assembles() {
        let mut c = Mp3Codec::new();
        c.reserv = 1;
        c.reserv_buf[0] = 0xAA;
        // Frame wants 5 bytes of history but only 1 exists.
        let (len, enough) = c.restore_reservoir(&[0xBB, 0xCC], 5);
        assert!(!enough);
        assert_eq!(len, 1 + 2);
        assert_eq!(&c.maindata[..3], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn reservoir_save_clamps_to_max_bytes() {
        let mut c = Mp3Codec::new();
        // Fill more than the reservoir cap and mark it all unconsumed.
        for (i, b) in c.maindata.iter_mut().enumerate().take(MAX_RESERVOIR + 20) {
            *b = (i & 0xFF) as u8;
        }
        c.save_reservoir(0, (MAX_RESERVOIR + 20) * 8);
        assert_eq!(c.reserv, MAX_RESERVOIR);
        // The saved window is the *tail* (dropped the first 20 bytes).
        assert_eq!(c.reserv_buf[0], 20);
    }

    // -- side info ------------------------------------------------------------

    #[test]
    fn reads_mpeg1_mono_side_info() {
        let mut w = BitWriter::new();
        w.bits(100, 9); // main_data_begin
        w.bits(0, 5); // private_bits
        w.bits(0xA, 4); // scfsi[ch0] -> granule 1's scfsi

        // Granule 0: a long block.
        w.bits(200, 12); // part2_3_length
        w.bits(50, 9); // big_values
        w.bits(150, 8); // global_gain
        w.bits(5, 4); // scalefac_compress
        w.bits(0, 1); // window_switching = 0
        w.bits(10, 5); // table_select[0]
        w.bits(11, 5); // table_select[1]
        w.bits(12, 5); // table_select[2]
        w.bits(7, 4); // region0_count
        w.bits(3, 3); // region1_count
        w.bits(1, 1); // preflag
        w.bits(0, 1); // scalefac_scale
        w.bits(1, 1); // count1table_select

        // Granule 1: a short block (window switching).
        w.bits(180, 12); // part2_3_length
        w.bits(40, 9); // big_values
        w.bits(140, 8); // global_gain
        w.bits(6, 4); // scalefac_compress
        w.bits(1, 1); // window_switching = 1
        w.bits(2, 2); // block_type = SHORT
        w.bits(0, 1); // mixed_block_flag = 0
        w.bits(8, 5); // table_select[0]
        w.bits(9, 5); // table_select[1]
        w.bits(1, 3); // subblock_gain[0]
        w.bits(2, 3); // subblock_gain[1]
        w.bits(3, 3); // subblock_gain[2]
        w.bits(0, 1); // preflag
        w.bits(1, 1); // scalefac_scale
        w.bits(0, 1); // count1table_select

        let bytes = w.into_bytes();
        let hdr = Header([0xFF, 0xFB, 0x90, 0xC0]); // MPEG-1, 44.1k, mono
        let mut bs = BitReader::new(&bytes);
        let mut gr = [GrInfo::default(); 4];
        let mdb = read_side_info(&mut bs, &mut gr, hdr).expect("valid side info");
        assert_eq!(mdb, 100);

        // Granule 0 (long).
        let g0 = &gr[0];
        assert_eq!(g0.part_23_length, 200);
        assert_eq!(g0.big_values, 50);
        assert_eq!(g0.global_gain, 150);
        assert_eq!(g0.scalefac_compress, 5);
        assert_eq!(g0.block_type, 0);
        assert_eq!(g0.n_long_sfb, 22);
        assert_eq!(g0.n_short_sfb, 0);
        assert_eq!(g0.table_select, [10, 11, 12]);
        assert_eq!(g0.region_count[0], 7);
        assert_eq!(g0.region_count[1], 3);
        assert_eq!(g0.preflag, 1);
        assert_eq!(g0.scalefac_scale, 0);
        assert_eq!(g0.count1_table, 1);
        assert_eq!(g0.scfsi, 0); // granule 0 never reuses
        // sr_idx = my_sample_rate(6) - 1 = 5.
        assert_eq!(g0.sfbtab, &G_SCF_LONG[5][..]);

        // Granule 1 (short).
        let g1 = &gr[1];
        assert_eq!(g1.part_23_length, 180);
        assert_eq!(g1.big_values, 40);
        assert_eq!(g1.global_gain, 140);
        assert_eq!(g1.scalefac_compress, 6);
        assert_eq!(g1.block_type, SHORT_BLOCK_TYPE);
        assert_eq!(g1.mixed_block_flag, 0);
        assert_eq!(g1.n_long_sfb, 0);
        assert_eq!(g1.n_short_sfb, 39);
        assert_eq!(g1.region_count[0], 8);
        assert_eq!(g1.subblock_gain, [1, 2, 3]);
        assert_eq!(g1.table_select[0], 8);
        assert_eq!(g1.table_select[1], 9);
        assert_eq!(g1.preflag, 0);
        assert_eq!(g1.scalefac_scale, 1);
        assert_eq!(g1.count1_table, 0);
        // A short block forces scfsi to 0 (`scfsi &= 0x0F0F`), so no reuse.
        assert_eq!(g1.scfsi, 0);
        assert_eq!(g1.sfbtab, &G_SCF_SHORT[5][..]);
    }

    #[test]
    fn reads_mpeg1_stereo_scfsi_split_per_channel() {
        // Minimal stereo side info: focus on main_data_begin and the per-channel
        // scfsi routing to granule 1. All granules use simple long blocks.
        let mut w = BitWriter::new();
        w.bits(0, 9); // main_data_begin = 0
        w.bits(0, 3); // private_bits (stereo)
        w.bits(0x5, 4); // scfsi[ch0]
        w.bits(0xC, 4); // scfsi[ch1]
        // Four granule/channel blocks (g0c0, g0c1, g1c0, g1c1), all long.
        for _ in 0..4 {
            w.bits(0, 12); // part2_3_length
            w.bits(0, 9); // big_values
            w.bits(0, 8); // global_gain
            w.bits(0, 4); // scalefac_compress
            w.bits(0, 1); // window_switching = 0
            w.bits(0, 15); // table_select x3
            w.bits(0, 4); // region0_count
            w.bits(0, 3); // region1_count
            w.bits(0, 1); // preflag
            w.bits(0, 1); // scalefac_scale
            w.bits(0, 1); // count1table_select
        }
        let bytes = w.into_bytes();
        let hdr = Header([0xFF, 0xFB, 0x90, 0x00]); // MPEG-1, stereo
        let mut bs = BitReader::new(&bytes);
        let mut gr = [GrInfo::default(); 4];
        let mdb = read_side_info(&mut bs, &mut gr, hdr).expect("valid side info");
        assert_eq!(mdb, 0);
        // Granule 0 channels never reuse.
        assert_eq!(gr[0].scfsi, 0);
        assert_eq!(gr[1].scfsi, 0);
        // Granule 1 channels carry each channel's scfsi field.
        assert_eq!(gr[2].scfsi, 0x5);
        assert_eq!(gr[3].scfsi, 0xC);
    }

    #[test]
    fn side_info_rejects_oversized_big_values() {
        let mut w = BitWriter::new();
        w.bits(0, 9);
        w.bits(0, 5);
        w.bits(0, 4);
        w.bits(0, 12); // part2_3_length
        w.bits(400, 9); // big_values > 288 -> malformed
        let bytes = w.into_bytes();
        let hdr = Header([0xFF, 0xFB, 0x90, 0xC0]);
        let mut bs = BitReader::new(&bytes);
        let mut gr = [GrInfo::default(); 4];
        assert!(read_side_info(&mut bs, &mut gr, hdr).is_none());
    }

    // -- scalefactors ---------------------------------------------------------

    #[test]
    fn ldexp_q2_is_power_of_two_quarters() {
        // ldexp_q2(y, e) == y * 2^(-e/4).
        assert!((ldexp_q2(1.0, 0) - 1.0).abs() < 1e-6);
        assert!((ldexp_q2(1.0, 4) - 0.5).abs() < 1e-6);
        assert!((ldexp_q2(1.0, 8) - 0.25).abs() < 1e-6);
        assert!((ldexp_q2(1.0, 2) - 2f32.powf(-0.5)).abs() < 1e-6);
        assert!((ldexp_q2(2048.0, 44) - 1.0).abs() < 1e-5); // 2048 * 2^-11 = 1
        // Large exponent (exercises the multi-step loop).
        assert!((ldexp_q2(1.0, 200) - 2f32.powi(-50)).abs() < 1e-12);
    }

    /// A minimal long-block granule for scalefactor tests.
    fn long_gr(scalefac_compress: u16, global_gain: u8, scalefac_scale: u8) -> GrInfo {
        GrInfo {
            sfbtab: &G_SCF_LONG[5],
            scalefac_compress,
            global_gain,
            scalefac_scale,
            n_long_sfb: 22,
            n_short_sfb: 0,
            ..GrInfo::default()
        }
    }

    #[test]
    fn scalefactors_all_zero_compress_yields_uniform_gain() {
        // scalefac_compress 0 -> all scf_size 0 -> every iscf is 0 -> every scf
        // equals `gain`. global_gain 214 makes gain exactly 1.0.
        let hdr = Header([0xFF, 0xFB, 0x90, 0xC0]); // MPEG-1 mono, MS off
        let gr = long_gr(0, 214, 0);
        let bytes = [0u8; 8];
        let mut bs = BitReader::new(&bytes);
        let mut scf = [0.0f32; 40];
        let mut ist = [0u8; 39];
        decode_scalefactors(hdr, &mut ist, &mut bs, &gr, &mut scf, 0);
        for s in &scf[..22] {
            assert!((s - 1.0).abs() < 1e-5, "expected uniform gain 1.0, got {s}");
        }
    }

    #[test]
    fn scalefactors_read_and_scale_per_band() {
        // scalefac_compress 1 -> scf_size = [0, 0, 1, 1]. Long partitions are
        // [6, 5, 5, 5]: bands 0..10 are 0-bit (iscf 0), bands 11..15 and 16..20
        // read 1 bit each. global_gain 214 & scale 0 -> gain 1.0, so
        // scf[i] = 2^(-iscf[i]/2).
        let mut w = BitWriter::new();
        for &b in &[1u32, 0, 1, 0, 1] {
            w.bits(b, 1); // partition 2 (bands 11..15)
        }
        for &b in &[0u32, 1, 0, 1, 0] {
            w.bits(b, 1); // partition 3 (bands 16..20)
        }
        let bytes = w.into_bytes();

        let hdr = Header([0xFF, 0xFB, 0x90, 0xC0]);
        let gr = long_gr(1, 214, 0);
        let mut bs = BitReader::new(&bytes);
        let mut scf = [0.0f32; 40];
        let mut ist = [0u8; 39];
        decode_scalefactors(hdr, &mut ist, &mut bs, &gr, &mut scf, 0);

        let half = 2f32.powf(-0.5);
        assert!((scf[0] - 1.0).abs() < 1e-5); // iscf 0
        assert!((scf[10] - 1.0).abs() < 1e-5); // iscf 0
        assert!((scf[11] - half).abs() < 1e-5); // iscf 1
        assert!((scf[12] - 1.0).abs() < 1e-5); // iscf 0
        assert!((scf[13] - half).abs() < 1e-5); // iscf 1
        assert!((scf[16] - 1.0).abs() < 1e-5); // iscf 0
        assert!((scf[17] - half).abs() < 1e-5); // iscf 1
    }

    // -- Huffman + requantization ---------------------------------------------

    #[test]
    fn pow_43_matches_definition() {
        // pow_43(x) == x^(4/3) for the table region and the extended region.
        for &x in &[0i32, 1, 2, 5, 100, 128] {
            let want = (x as f32).powf(4.0 / 3.0);
            assert!(
                (pow_43(x) - want).abs() < 1e-2,
                "pow_43({x})={} want {want}",
                pow_43(x)
            );
        }
        // Extended (x >= 129) uses the polynomial refinement.
        for &x in &[130i32, 200, 500, 1023, 2000, 8191] {
            let want = (x as f32).powf(4.0 / 3.0);
            let got = pow_43(x);
            assert!(
                (got - want).abs() / want < 1e-3,
                "pow_43({x})={got} want {want}"
            );
        }
    }

    #[test]
    fn huffman_decodes_table1_pairs() {
        // ISO Huffman table 1 codewords for value pairs (x, y in {0,1}):
        //   (0,0)="1", (1,0)="01", (0,1)="001", (1,1)="000".
        // Each nonzero value is followed by a sign bit (1 = negative).
        let mut w = BitWriter::new();
        w.bits(0b1, 1); // pair0 (0,0)
        w.bits(0b01, 2); // pair1 (1,0) ...
        w.bits(0, 1); //   x sign = +
        w.bits(0b001, 3); // pair2 (0,1) ...
        w.bits(1, 1); //   y sign = -
        w.bits(0b000, 3); // pair3 (1,1) ...
        w.bits(1, 1); //   x sign = -
        w.bits(0, 1); //   y sign = +
        let part_23 = 13; // total bits above
        let mut bytes = w.into_bytes();
        bytes.resize(bytes.len() + 4, 0); // zero padding for the cache look-ahead

        let gr = GrInfo {
            sfbtab: &G_SCF_LONG[5],
            big_values: 4,
            part_23_length: part_23,
            table_select: [1, 0, 0],
            region_count: [7, 255, 255],
            n_long_sfb: 22,
            ..GrInfo::default()
        };
        let scf = [1.0f32; 40];
        let mut dst = [0.0f32; 576];
        let mut bs = BitReader::new(&bytes);
        let limit = gr.part_23_length as usize;
        huffman(&mut dst, &mut bs, &gr, &scf, limit);

        // Magnitudes are g_pow43[16 + value] = value (0 or 1); signs as encoded.
        assert_eq!(&dst[..8], &[0.0, 0.0, 1.0, 0.0, 0.0, -1.0, -1.0, 1.0]);
        // Nothing spilled into the count1 region.
        assert!(dst[8..].iter().all(|&v| v == 0.0));
        // The bit cursor is parked at the granule limit.
        assert_eq!(bs.pos(), part_23 as usize);
    }

    #[test]
    fn huffman_applies_scalefactor_multiplier() {
        // Same single nonzero pair but with a non-unit scalefactor: the decoded
        // magnitude scales by `one` (= scf[band]).
        let mut w = BitWriter::new();
        w.bits(0b01, 2); // (1,0)
        w.bits(0, 1); // x sign +
        let mut bytes = w.into_bytes();
        bytes.resize(bytes.len() + 4, 0);

        let gr = GrInfo {
            sfbtab: &G_SCF_LONG[5],
            big_values: 1,
            part_23_length: 3,
            table_select: [1, 0, 0],
            region_count: [7, 255, 255],
            n_long_sfb: 22,
            ..GrInfo::default()
        };
        let mut scf = [0.0f32; 40];
        scf[0] = 2.5;
        let mut dst = [0.0f32; 576];
        let mut bs = BitReader::new(&bytes);
        let limit = gr.part_23_length as usize;
        huffman(&mut dst, &mut bs, &gr, &scf, limit);
        assert!((dst[0] - 2.5).abs() < 1e-6); // x=1 * scf 2.5
        assert_eq!(dst[1], 0.0);
    }

    // -- synthesis ------------------------------------------------------------

    #[test]
    fn scale_pcm_rounds_and_clamps() {
        assert_eq!(scale_pcm(0.0), 0);
        assert_eq!(scale_pcm(100.4), 100);
        assert_eq!(scale_pcm(100.6), 101);
        assert_eq!(scale_pcm(-100.4), -100);
        assert_eq!(scale_pcm(-100.6), -101); // away from zero
        assert_eq!(scale_pcm(40000.0), 32767); // clamp high
        assert_eq!(scale_pcm(-40000.0), -32768); // clamp low
        assert_eq!(scale_pcm(32766.5), 32767); // boundary
        assert_eq!(scale_pcm(-32767.5), -32768); // boundary
    }

    #[test]
    fn synth_granule_zero_input_is_silent() {
        for nch in 1..=2usize {
            let mut qmf = [0.0f32; 15 * 64];
            let mut grbuf = [0.0f32; 2 * 576];
            let mut lins = [0.0f32; (18 + 15) * 64];
            let mut pcm = [123i32; 1152 * 2]; // pre-fill to catch stray writes
            synth_granule(&mut qmf, &mut grbuf, 18, nch, &mut pcm, 0, &mut lins);
            assert!(
                pcm[..576 * nch].iter().all(|&v| v == 0),
                "nch={nch}: expected silence"
            );
        }
    }

    // -- IMDCT ----------------------------------------------------------------

    #[test]
    fn imdct36_matches_direct_windowed_definition() {
        // First granule (overlap = 0): the long IMDCT output equals the windowed
        // raw IMDCT, grbuf[i] = w[i] * sum_k X[k] cos((pi/72)(2i+19)(2k+1)),
        // with the normal window w[i] = sin(pi/36 (i + 0.5)).
        use std::f64::consts::PI;
        let mut x = [0.0f32; 18];
        x[0] = 1.0;
        x[1] = -0.5;
        x[5] = 0.25;
        x[11] = 0.9;
        x[17] = 0.7;

        let mut grbuf = [0.0f32; 18];
        grbuf.copy_from_slice(&x);
        let mut overlap = [0.0f32; 9];
        imdct36(&mut grbuf, &mut overlap, &G_MDCT_WINDOW[0], 1);

        for (i, &got) in grbuf.iter().enumerate() {
            let mut raw = 0.0f64;
            for (k, &xk) in x.iter().enumerate() {
                let a = PI / 72.0 * ((2 * i + 19) as f64) * ((2 * k + 1) as f64);
                raw += f64::from(xk) * a.cos();
            }
            let w = (PI / 36.0 * (i as f64 + 0.5)).sin();
            let expect = (raw * w) as f32;
            assert!(
                (got - expect).abs() < 2e-3,
                "i={i}: got {got} want {expect}"
            );
        }
    }

    #[test]
    fn imdct36_overlap_carries_second_half_to_next_block() {
        // The overlap saved by block 1 must equal the windowed *second half* of
        // block 1's raw IMDCT, which is what block 2 overlap-adds. Verify by
        // running two blocks and checking block 2's output includes it.
        use std::f64::consts::PI;
        let x1 = {
            let mut v = [0.0f32; 18];
            v[0] = 0.8;
            v[3] = -0.4;
            v[9] = 0.5;
            v
        };
        // Block 1.
        let mut grbuf = x1;
        let mut overlap = [0.0f32; 9];
        imdct36(&mut grbuf, &mut overlap, &G_MDCT_WINDOW[0], 1);
        // Reference: windowed second half of block-1 raw IMDCT (samples 18..36).
        let mut ref_overlap_add = [0.0f64; 18];
        for (n, slot) in ref_overlap_add.iter_mut().enumerate() {
            let i = 18 + n;
            let mut raw = 0.0f64;
            for (k, &xk) in x1.iter().enumerate() {
                let a = PI / 72.0 * ((2 * i + 19) as f64) * ((2 * k + 1) as f64);
                raw += f64::from(xk) * a.cos();
            }
            let w = (PI / 36.0 * (i as f64 + 0.5)).sin();
            *slot = raw * w;
        }
        // Block 2 with zero input: output is purely the carried overlap.
        let mut grbuf2 = [0.0f32; 18];
        imdct36(&mut grbuf2, &mut overlap, &G_MDCT_WINDOW[0], 1);
        for (n, &r) in ref_overlap_add.iter().enumerate() {
            assert!(
                (f64::from(grbuf2[n]) - r).abs() < 3e-3,
                "overlap n={n}: got {} want {r}",
                grbuf2[n]
            );
        }
    }

    #[test]
    fn change_sign_negates_odd_lines_of_odd_bands() {
        let mut grbuf = [1.0f32; 2 * 576];
        change_sign(&mut grbuf);
        // Band 0 untouched.
        assert!(grbuf[..18].iter().all(|&v| v == 1.0));
        // Band 1 (offset 18): odd indices negated, even untouched.
        assert_eq!(grbuf[18], 1.0);
        assert_eq!(grbuf[19], -1.0);
        assert_eq!(grbuf[20], 1.0);
        // Band 2 (offset 36) untouched (even band).
        assert!(grbuf[36..54].iter().all(|&v| v == 1.0));
    }

    // -- reorder + antialias --------------------------------------------------

    #[test]
    fn reorder_interleaves_three_short_windows() {
        // One band of width 2: windows stored [w0[0],w0[1], w1[0],w1[1], w2..],
        // reordered to time order [w0[0],w1[0],w2[0], w0[1],w1[1],w2[1]].
        let mut grbuf = [0.0f32; 576];
        grbuf[..6].copy_from_slice(&[10.0, 11.0, 20.0, 21.0, 30.0, 31.0]);
        let mut scratch = [0.0f32; 64];
        let sfb = [2u8, 0, 0, 0];
        reorder(&mut grbuf, &mut scratch, &sfb);
        assert_eq!(&grbuf[..6], &[10.0, 20.0, 30.0, 11.0, 21.0, 31.0]);
    }

    #[test]
    fn antialias_butterfly_first_coefficient() {
        // A unit impulse at grbuf[18] mixes into grbuf[17]/grbuf[18] by the first
        // alias coefficients (only i=0 has nonzero inputs).
        let mut grbuf = [0.0f32; 2 * 576];
        grbuf[18] = 1.0;
        antialias(&mut grbuf, 1);
        assert!((grbuf[18] - 0.85749293).abs() < 1e-6); // u*c0
        assert!((grbuf[17] - 0.51449576).abs() < 1e-6); // u*c1
        // Untouched neighbours.
        assert_eq!(grbuf[16], 0.0);
        assert_eq!(grbuf[19], 0.0);
    }

    // -- stereo ---------------------------------------------------------------

    #[test]
    fn midside_stereo_sum_and_difference() {
        // Stored mid/side -> left = mid + side, right = mid - side.
        let mid = [100.0f32, -50.0, 5.0, 3.0];
        let side = [98.0f32, -60.0, 2.0, -3.0];
        let mut grbuf = [0.0f32; 2 * 576];
        grbuf[..4].copy_from_slice(&mid);
        grbuf[576..580].copy_from_slice(&side);
        midside_stereo(&mut grbuf, 0, 4);
        for i in 0..4 {
            assert!((grbuf[i] - (mid[i] + side[i])).abs() < 1e-4);
            assert!((grbuf[576 + i] - (mid[i] - side[i])).abs() < 1e-4);
        }
    }

    #[test]
    fn intensity_stereo_pans_mpeg1_band() {
        // MPEG-1 joint stereo, intensity only (mode-ext 01 -> h[3] = 0x50).
        let hdr = Header([0xFF, 0xFB, 0x90, 0x50]);
        assert!(hdr.test_i_stereo() && !hdr.test_ms_stereo());

        let mut grbuf = [0.0f32; 2 * 576];
        let left0 = [10.0f32, 20.0, 30.0, 40.0];
        grbuf[..4].copy_from_slice(&left0); // band 0 (width 4 in G_SCF_LONG[5])
        // Right channel all zero -> top coded band is -1, so every band is
        // eligible for intensity.

        let gr = GrInfo {
            sfbtab: &G_SCF_LONG[5],
            n_long_sfb: 22,
            ..GrInfo::default()
        };
        // ipos 2 on band 0; all other bands "illegal" (>= max_pos 7) so untouched.
        let mut ist_pos = [7u8; 39];
        ist_pos[0] = 2;

        intensity_stereo(&mut grbuf, &mut ist_pos, &gr, hdr, 0);

        // g_pan[2*2] = 0.36602540 (kl), g_pan[5] = 0.63397460 (kr).
        let kl = 0.36602540f32;
        let kr = 0.63397460f32;
        for i in 0..4 {
            assert!((grbuf[i] - left0[i] * kl).abs() < 1e-4, "left band0 pan");
            assert!(
                (grbuf[576 + i] - left0[i] * kr).abs() < 1e-4,
                "right band0 pan"
            );
        }
        // Band 1 (indices 4..8) untouched (ipos 7 is illegal).
        assert!(grbuf[4..8].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn scalefactors_preflag_adds_high_band_preamp() {
        // With preflag set and scalefac_compress 0 (iscf all 0), bands 11..20 get
        // the preamp table added, so scf[11] = 2^(-preamp/2) with scale 0.
        let hdr = Header([0xFF, 0xFB, 0x90, 0xC0]);
        let mut gr = long_gr(0, 214, 0);
        gr.preflag = 1;
        let bytes = [0u8; 8];
        let mut bs = BitReader::new(&bytes);
        let mut scf = [0.0f32; 40];
        let mut ist = [0u8; 39];
        decode_scalefactors(hdr, &mut ist, &mut bs, &gr, &mut scf, 0);
        // g_preamp = {1,1,1,1,2,2,3,3,3,2} at bands 11..20; scf = 2^(-amp/2).
        assert!((scf[10] - 1.0).abs() < 1e-5); // no preamp below band 11
        assert!((scf[11] - 2f32.powf(-0.5)).abs() < 1e-5); // amp 1
        assert!((scf[15] - 0.5).abs() < 1e-5); // amp 2 -> 2^-1
        assert!((scf[17] - 2f32.powf(-1.5)).abs() < 1e-5); // amp 3
    }
}
