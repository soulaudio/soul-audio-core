//! DST (Direct Stream Transfer) lossless decompressor for DSDIFF / DFF.
//!
//! DST is the lossless codec used inside DSDIFF `DST ` sound-data chunks (one
//! `DSTF` chunk per coded frame). Each frame decodes to a fixed run of raw
//! **MSB-first, channel-interleaved** DSD — exactly the byte layout DFF's
//! uncompressed `DSD ` chunk carries — so the output slots straight into
//! [`crate::formats::dff`]'s `read_dsd` passthrough.
//!
//! The algorithm is ported from FFmpeg's `libavcodec/dstdec.c` (ISO/IEC
//! 14496-3 Part 3 Subpart 10): a binary arithmetic (range) decoder drives a
//! per-channel FIR predictor over the channel's recent 1-bit history; the
//! arithmetic-decoded residual XORs the prediction's sign to emit each DSD bit.
//! Filter coefficient sets and probability tables are carried per frame,
//! Golomb/Rice coded.
//!
//! Scope: strict `no_std`, no-alloc, `#[forbid(unsafe_code)]`. Fixed inline
//! buffers are sized for stereo up to DSD128; larger geometries are declined
//! with [`DecodeError::Unsupported`].

use crate::decoder::DecodeError;

/// Maximum channels the no-alloc DST state is sized for (stereo).
const MAX_CHANNELS: usize = 2;
/// Maximum distinct filter/probability elements per table (one per channel).
const MAX_ELEMENTS: usize = MAX_CHANNELS;
/// Maximum FIR filter length (taps) — 16 status bytes × 8 bits.
const MAX_FILTER_LEN: usize = 128;

// -------------------------------------------------------------------------
// Bit reader (MSB-first, matching FFmpeg get_bits semantics)
// -------------------------------------------------------------------------

/// MSB-first bit reader over a byte slice. Reads past the end yield zero bits
/// (the arithmetic decoder's final renormalisations legitimately over-read a
/// few padding bits, exactly as FFmpeg's zero-padded `GetBitContext` does).
struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute bit cursor from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Read a single bit (0 or 1); 0 once past the end.
    fn get_bit(&mut self) -> u32 {
        let byte = self.pos >> 3;
        let v = if byte < self.data.len() {
            (self.data[byte] >> (7 - (self.pos & 7))) & 1
        } else {
            0
        };
        self.pos += 1;
        u32::from(v)
    }

    /// Read `n` bits (0..=32) MSB-first into a `u32`.
    fn get_bits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.get_bit();
        }
        v
    }

    /// Read `n` bits and sign-extend from bit `n-1`.
    fn get_sbits(&mut self, n: u32) -> i32 {
        let v = self.get_bits(n) as i32;
        let shift = 32 - n;
        (v << shift) >> shift
    }
}

/// `floor(log2(x))` for `x >= 1` (FFmpeg `av_log2`).
fn av_log2(x: u32) -> u32 {
    31 - x.leading_zeros()
}

/// 8-bit reversal (FFmpeg `ff_reverse[x]`).
fn reverse8(mut b: u8) -> u8 {
    let mut r = 0u8;
    for _ in 0..8 {
        r = (r << 1) | (b & 1);
        b >>= 1;
    }
    r
}

// -------------------------------------------------------------------------
// Arithmetic (range) decoder — faithful port of FFmpeg ac_init / ac_get
// -------------------------------------------------------------------------

/// Binary arithmetic decoder state. `a` is the current range (kept in
/// `[2048, 4096)` after renormalisation); `c` is the code window.
struct ArithCoder {
    a: u32,
    c: u32,
}

impl ArithCoder {
    fn init(br: &mut BitReader) -> Self {
        Self {
            a: 4095,
            c: br.get_bits(12),
        }
    }

    /// Decode one binary symbol with probability parameter `p` (`1..=128`).
    /// Returns the decoded bit `e`.
    fn get(&mut self, br: &mut BitReader, p: u32) -> u32 {
        let k = (self.a >> 8) | ((self.a >> 7) & 1);
        let q = k * p;
        let a_q = self.a - q;

        let e = u32::from(self.c < a_q);
        if e == 1 {
            self.a = a_q;
        } else {
            self.a = q;
            self.c -= a_q;
        }

        if self.a < 2048 {
            let n = 11 - av_log2(self.a);
            self.a <<= n;
            self.c = (self.c << n) | br.get_bits(n);
        }
        e
    }
}

// -------------------------------------------------------------------------
// Golomb / Rice (JPEG-LS) coding — matches FFmpeg get_ur_golomb_jpegls
// -------------------------------------------------------------------------

/// Unsigned JPEG-LS Golomb-Rice: `value = (leading_zeros << k) + k-bit tail`.
/// The unary prefix is a run of `0`s terminated by a `1`.
fn get_ur_golomb_jpegls(br: &mut BitReader, k: u32) -> Result<u32, DecodeError> {
    let mut i = 0u32;
    while br.get_bit() == 0 {
        i += 1;
        // Runaway guard: coefficient residuals are tiny; a huge unary run means
        // corruption or a truncated frame.
        if i as usize > MAX_FILTER_LEN * 64 {
            return Err(DecodeError::Malformed);
        }
    }
    let rem = br.get_bits(k);
    Ok((i << k) + rem)
}

/// Signed Golomb used by DST tables: magnitude then a sign bit when non-zero.
fn get_sr_golomb_dst(br: &mut BitReader, k: u32) -> Result<i32, DecodeError> {
    let v = get_ur_golomb_jpegls(br, k)? as i32;
    if v != 0 && br.get_bit() == 1 {
        Ok(-v)
    } else {
        Ok(v)
    }
}

// -------------------------------------------------------------------------
// Frame header tables (filter coefficient sets + probability tables)
// -------------------------------------------------------------------------

/// Predictor coefficients for delta-coded filter coefficient sets (10.12).
const FSETS_CODE_PRED: [[i32; 3]; 3] = [[-8, 0, 0], [-16, 8, 0], [-9, -5, 6]];
/// Predictor coefficients for delta-coded probability tables (10.13).
const PROBS_CODE_PRED: [[i32; 3]; 3] = [[-8, 0, 0], [-16, 8, 0], [-24, 24, -8]];

/// A per-frame table of `elements` coefficient vectors (filter sets or
/// probability tables). Coefficients are stored widened to `i32`.
struct Table {
    elements: u32,
    length: [u32; MAX_ELEMENTS],
    coeff: [[i32; MAX_FILTER_LEN]; MAX_ELEMENTS],
}

impl Table {
    fn zeroed() -> Self {
        Self {
            elements: 0,
            length: [0; MAX_ELEMENTS],
            coeff: [[0; MAX_FILTER_LEN]; MAX_ELEMENTS],
        }
    }
}

/// Read the channel→element map (10.7-10.9) and the resulting `elements` count.
fn read_map(
    br: &mut BitReader,
    elements: &mut u32,
    map: &mut [u32; MAX_CHANNELS],
    channels: usize,
) -> Result<(), DecodeError> {
    *elements = 1;
    map[0] = 0;
    if br.get_bit() == 0 {
        for slot in map.iter_mut().take(channels).skip(1) {
            let bits = av_log2(*elements) + 1;
            *slot = br.get_bits(bits);
            if *slot == *elements {
                *elements += 1;
                if *elements as usize > MAX_ELEMENTS {
                    return Err(DecodeError::Unsupported);
                }
            } else if *slot > *elements {
                return Err(DecodeError::Malformed);
            }
        }
    } else {
        map.fill(0);
    }
    Ok(())
}

/// Read `n` raw coefficients (`coeff_bits` each, signed or not) plus `offset`.
fn read_uncoded_coeff(
    br: &mut BitReader,
    dst: &mut [i32],
    n: usize,
    coeff_bits: u32,
    is_signed: bool,
    offset: i32,
) {
    for d in dst.iter_mut().take(n) {
        *d = if is_signed {
            br.get_sbits(coeff_bits)
        } else {
            br.get_bits(coeff_bits) as i32
        } + offset;
    }
}

/// Read one table (either raw or delta/Golomb coded), per FFmpeg `read_table`.
fn read_table(
    br: &mut BitReader,
    t: &mut Table,
    code_pred: &[[i32; 3]; 3],
    length_bits: u32,
    coeff_bits: u32,
    is_signed: bool,
    offset: i32,
) -> Result<(), DecodeError> {
    for i in 0..t.elements as usize {
        t.length[i] = br.get_bits(length_bits) + 1;
        let len = t.length[i] as usize;
        if len > MAX_FILTER_LEN {
            return Err(DecodeError::Malformed);
        }
        if br.get_bit() == 0 {
            read_uncoded_coeff(br, &mut t.coeff[i], len, coeff_bits, is_signed, offset);
        } else {
            let method = br.get_bits(2) as usize;
            if method == 3 {
                return Err(DecodeError::Malformed);
            }
            read_uncoded_coeff(
                br,
                &mut t.coeff[i],
                method + 1,
                coeff_bits,
                is_signed,
                offset,
            );
            let lsb_size = br.get_bits(3);
            for j in (method + 1)..len {
                let mut x: i32 = 0;
                for (k, &pc) in code_pred[method].iter().enumerate().take(method + 1) {
                    x = x.wrapping_add(pc.wrapping_mul(t.coeff[i][j - k - 1]));
                }
                let mut c = get_sr_golomb_dst(br, lsb_size)?;
                if x >= 0 {
                    c -= (x + 4) / 8;
                } else {
                    c += (-x + 3) / 8;
                }
                if !is_signed && (c < offset || c >= offset + (1 << coeff_bits)) {
                    return Err(DecodeError::Malformed);
                }
                t.coeff[i][j] = c;
            }
        }
    }
    Ok(())
}

// -------------------------------------------------------------------------
// Prediction (FIR over 1-bit channel history)
// -------------------------------------------------------------------------

/// Filter partial-sum table: `filter[element][byte][history_byte]` is the dot
/// product of 8 coefficients against the ±1-mapped bits of one history byte.
type FilterTable = [[[i16; 256]; 16]; MAX_ELEMENTS];

/// Precompute the FIR partial-sum table from the filter coefficient sets.
fn build_filter(filter: &mut FilterTable, fsets: &Table) -> Result<(), DecodeError> {
    for (i, felem) in filter.iter_mut().enumerate().take(fsets.elements as usize) {
        let length = fsets.length[i] as i32;
        for (j, sub) in felem.iter_mut().enumerate() {
            let total = (length - (j as i32) * 8).clamp(0, 8) as usize;
            for (k, slot) in sub.iter_mut().enumerate() {
                let mut v: i64 = 0;
                for l in 0..total {
                    v += (((k >> l) & 1) as i64 * 2 - 1) * i64::from(fsets.coeff[i][j * 8 + l]);
                }
                if v < i64::from(i16::MIN) || v > i64::from(i16::MAX) {
                    return Err(DecodeError::Malformed);
                }
                *slot = v as i16;
            }
        }
    }
    Ok(())
}

/// The FIR prediction for one output bit: sum of the 16 status-byte partial
/// sums, truncated to `i16` (matching FFmpeg's `int16_t predict`).
fn compute_predict(filter: &FilterTable, felem: usize, status: &[u8; 16]) -> i16 {
    let sum: i32 = filter[felem]
        .iter()
        .zip(status.iter())
        .map(|(tbl, &s)| i32::from(tbl[s as usize]))
        .sum();
    sum as i16
}

/// Select the arithmetic-coder probability for one bit from the prediction
/// magnitude (or the uniform value 128 during filter warm-up).
fn choose_prob(
    predict: i16,
    half: bool,
    i: usize,
    fset_len: u32,
    probs: &Table,
    pelem: usize,
) -> u32 {
    if !half || i >= fset_len as usize {
        let index = (i32::from(predict).unsigned_abs() >> 3) as usize;
        let idx = index.min(probs.length[pelem] as usize - 1);
        probs.coeff[pelem][idx] as u32
    } else {
        128
    }
}

/// Shift a new bit `v` into the 128-bit per-channel history (newest at bit 0).
fn update_status(status: &mut [u8; 16], v: u32) {
    let lo = u64::from_le_bytes(status[0..8].try_into().unwrap());
    let hi = u64::from_le_bytes(status[8..16].try_into().unwrap());
    let new_hi = (hi << 1) | (lo >> 63);
    let new_lo = (lo << 1) | u64::from(v);
    status[0..8].copy_from_slice(&new_lo.to_le_bytes());
    status[8..16].copy_from_slice(&new_hi.to_le_bytes());
}

/// Initial arithmetic probability for the discarded `DST_X_BIT` symbol.
fn prob_dst_x_bit(c: i32) -> u32 {
    u32::from(reverse8((c & 127) as u8) >> 1) + 1
}

// -------------------------------------------------------------------------
// Frame decoder
// -------------------------------------------------------------------------

/// DSD bytes per channel in the largest supported frame (DSD128 @ 75 fps:
/// `5_644_800 / 75 / 8`).
const MAX_FRAME_BYTES_PER_CH: usize = 9408;
/// Largest decoded interleaved-DSD frame (stereo DSD128).
const MAX_FRAME_BYTES: usize = MAX_FRAME_BYTES_PER_CH * MAX_CHANNELS;

/// A no-alloc DST frame decoder holding all per-stream scratch.
///
/// One [`DstDecoder`] decodes every `DSTF` frame of a stream; construct it with
/// the channel count and the number of 1-bit samples per channel per frame
/// (`dsd_rate_hz / frame_rate`). Larger-than-stereo-DSD128 geometries are
/// declined at construction with [`DecodeError::Unsupported`].
pub struct DstDecoder {
    channels: usize,
    samples_per_frame: usize,
    frame_bytes: usize,
    fsets: Table,
    probs: Table,
    filter: FilterTable,
    status: [[u8; 16]; MAX_CHANNELS],
    out: [u8; MAX_FRAME_BYTES],
}

impl DstDecoder {
    /// Create a decoder for `channels` channels and `samples_per_frame` 1-bit
    /// samples per channel per frame.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for more than stereo, a non-multiple-of-8
    /// frame length, or a frame larger than stereo DSD128.
    pub fn new(channels: usize, samples_per_frame: usize) -> Result<Self, DecodeError> {
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(DecodeError::Unsupported);
        }
        if samples_per_frame == 0 || samples_per_frame % 8 != 0 {
            return Err(DecodeError::Unsupported);
        }
        let frame_bytes = samples_per_frame / 8 * channels;
        if frame_bytes > MAX_FRAME_BYTES {
            return Err(DecodeError::Unsupported);
        }
        Ok(Self {
            channels,
            samples_per_frame,
            frame_bytes,
            fsets: Table::zeroed(),
            probs: Table::zeroed(),
            filter: [[[0; 256]; 16]; MAX_ELEMENTS],
            status: [[0; 16]; MAX_CHANNELS],
            out: [0; MAX_FRAME_BYTES],
        })
    }

    /// Interleaved DSD bytes produced per decoded frame.
    #[must_use]
    pub fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    /// The most recently decoded frame (`frame_bytes()` bytes). Contents are
    /// meaningful only after a successful [`Self::decode_frame`].
    #[must_use]
    pub fn last_frame(&self) -> &[u8] {
        &self.out[..self.frame_bytes]
    }

    /// Decode one `DSTF` frame (`input`) into interleaved MSB-first DSD.
    ///
    /// Returns the decoded frame slice (`frame_bytes()` long), borrowing the
    /// decoder's internal buffer until the next call.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a corrupt frame; [`DecodeError::Unsupported`]
    /// for a valid frame using a feature outside the supported profile (e.g.
    /// per-segment segmentation).
    pub fn decode_frame(&mut self, input: &[u8]) -> Result<&[u8], DecodeError> {
        let fb = self.frame_bytes;
        if input.len() <= 1 {
            return Err(DecodeError::Malformed);
        }
        let mut br = BitReader::new(input);

        // Uncompressed bypass frame: header byte then raw DSD.
        if br.get_bit() == 0 {
            br.get_bit(); // reserved
            if br.get_bits(6) != 0 {
                return Err(DecodeError::Malformed);
            }
            let n = (input.len() - 1).min(fb);
            self.out[..n].copy_from_slice(&input[1..1 + n]);
            self.out[n..fb].fill(0);
            return Ok(&self.out[..fb]);
        }

        // Segmentation (10.4-10.6): only fully-shared segmentation is supported.
        if br.get_bit() == 0 || br.get_bit() == 0 || br.get_bit() == 0 {
            return Err(DecodeError::Unsupported);
        }

        // Mapping (10.7-10.9).
        let same_map = br.get_bit() == 1;
        let mut map_felem = [0u32; MAX_CHANNELS];
        let mut map_pelem = [0u32; MAX_CHANNELS];
        read_map(
            &mut br,
            &mut self.fsets.elements,
            &mut map_felem,
            self.channels,
        )?;
        if same_map {
            self.probs.elements = self.fsets.elements;
            map_pelem = map_felem;
        } else {
            read_map(
                &mut br,
                &mut self.probs.elements,
                &mut map_pelem,
                self.channels,
            )?;
        }

        // Half probability (10.10).
        let mut half = [false; MAX_CHANNELS];
        for h in half.iter_mut().take(self.channels) {
            *h = br.get_bit() == 1;
        }

        // Filter coefficient sets (10.12) and probability tables (10.13).
        read_table(&mut br, &mut self.fsets, &FSETS_CODE_PRED, 7, 9, true, 0)?;
        read_table(&mut br, &mut self.probs, &PROBS_CODE_PRED, 6, 7, false, 1)?;

        // Arithmetic coded data (10.11).
        if br.get_bit() == 1 {
            return Err(DecodeError::Malformed);
        }
        let mut ac = ArithCoder::init(&mut br);

        build_filter(&mut self.filter, &self.fsets)?;

        for st in self.status.iter_mut() {
            *st = [0xAA; 16];
        }
        self.out[..fb].fill(0);

        // Initial DST_X_BIT symbol — consumed to advance the coder, discarded.
        let _ = ac.get(&mut br, prob_dst_x_bit(self.fsets.coeff[0][0]));

        let channels = self.channels;
        for i in 0..self.samples_per_frame {
            for ch in 0..channels {
                let felem = map_felem[ch] as usize;
                let predict = compute_predict(&self.filter, felem, &self.status[ch]);
                let prob = choose_prob(
                    predict,
                    half[ch],
                    i,
                    self.fsets.length[felem],
                    &self.probs,
                    map_pelem[ch] as usize,
                );
                let residual = ac.get(&mut br, prob);
                let sgn = u32::from(predict < 0);
                let v = sgn ^ residual;
                self.out[(i >> 3) * channels + ch] |= (v as u8) << (7 - (i & 7));
                update_status(&mut self.status[ch], v);
            }
        }
        Ok(&self.out[..fb])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    use alloc::vec::Vec;

    /// MSB-first bit writer collecting one entry per bit (test-only).
    struct BitWriter {
        bits: Vec<u8>,
    }

    impl BitWriter {
        fn new() -> Self {
            Self { bits: Vec::new() }
        }
        fn put_bit(&mut self, b: u32) {
            self.bits.push((b & 1) as u8);
        }
        fn put_bits(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                self.put_bit((v >> i) & 1);
            }
        }
        /// Append an already-serialised bit sequence.
        fn append_bits(&mut self, bits: &[u8]) {
            self.bits.extend_from_slice(bits);
        }
        /// Pack to bytes, MSB-first, zero-padding the final byte.
        fn into_bytes(self) -> Vec<u8> {
            let mut out = alloc::vec![0u8; self.bits.len().div_ceil(8)];
            for (i, &b) in self.bits.iter().enumerate() {
                if b != 0 {
                    out[i >> 3] |= 1 << (7 - (i & 7));
                }
            }
            out
        }
    }

    /// Arithmetic (range) *encoder* — the exact inverse of [`ArithCoder`]
    /// (test-only oracle). Maintains the interval low bound and range `a`,
    /// emitting the code's bits MSB-first with explicit carry ripple.
    struct ArithEncoder {
        a: u32,
        low: u32,
        bits: Vec<u8>,
    }

    impl ArithEncoder {
        fn new() -> Self {
            Self {
                a: 4095,
                low: 0,
                bits: Vec::new(),
            }
        }

        /// Ripple a carry (+1) back into the already-emitted bits.
        fn carry(&mut self) {
            let mut idx = self.bits.len();
            loop {
                assert!(idx != 0, "arith encoder carry overflow");
                idx -= 1;
                if self.bits[idx] == 0 {
                    self.bits[idx] = 1;
                    break;
                }
                self.bits[idx] = 0;
            }
        }

        /// Encode one binary symbol `e` with probability parameter `p`.
        fn encode(&mut self, p: u32, e: u32) {
            let k = (self.a >> 8) | ((self.a >> 7) & 1);
            let q = k * p;
            let a_q = self.a - q;

            if e == 1 {
                self.a = a_q;
            } else {
                self.low += a_q;
                if self.low >= 4096 {
                    self.low -= 4096;
                    self.carry();
                }
                self.a = q;
            }

            while self.a < 2048 {
                self.bits.push(((self.low >> 11) & 1) as u8);
                self.low = (self.low << 1) & 0xFFF;
                self.a <<= 1;
            }
        }

        /// Flush the final 12-bit window, returning the full bit sequence.
        fn finish(mut self) -> Vec<u8> {
            for shift in (0..12).rev() {
                self.bits.push(((self.low >> shift) & 1) as u8);
            }
            self.bits
        }
    }

    /// Deterministic xorshift PRNG for reproducible test vectors.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u32) -> u32 {
            (self.next() % u64::from(n)) as u32
        }
    }

    #[test]
    fn bit_reader_writer_roundtrip() {
        let mut w = BitWriter::new();
        let spec = [(5u32, 3u32), (0, 12), (255, 8), (1, 1), (0, 1), (4095, 12)];
        for &(v, n) in &spec {
            w.put_bits(v, n);
        }
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        for &(v, n) in &spec {
            assert_eq!(r.get_bits(n), v);
        }
    }

    #[test]
    fn av_log2_matches_floor_log2() {
        assert_eq!(av_log2(1), 0);
        assert_eq!(av_log2(2), 1);
        assert_eq!(av_log2(3), 1);
        assert_eq!(av_log2(8), 3);
        assert_eq!(av_log2(4095), 11);
        assert_eq!(av_log2(2048), 11);
    }

    #[test]
    fn reverse8_is_bit_reversal() {
        assert_eq!(reverse8(0b0000_0001), 0b1000_0000);
        assert_eq!(reverse8(0x78), 0x1E);
        assert_eq!(reverse8(0xFF), 0xFF);
        assert_eq!(reverse8(0x00), 0x00);
    }

    /// The core proof for the arithmetic coder: encode a pseudo-random symbol
    /// sequence with per-symbol probabilities, then decode it through the REAL
    /// [`ArithCoder`] and assert the identical sequence returns.
    #[test]
    fn arith_encode_decode_symmetry() {
        for seed in [1u64, 0xDEAD_BEEF, 0x1234_5678_9ABC_DEF0, 42] {
            let mut rng = Rng(seed);
            let n = 4000;
            let mut probs = Vec::with_capacity(n);
            let mut syms = Vec::with_capacity(n);
            for _ in 0..n {
                probs.push(rng.below(128) + 1); // p in 1..=128
                syms.push(rng.below(2)); // e in {0,1}
            }

            let mut enc = ArithEncoder::new();
            for i in 0..n {
                enc.encode(probs[i], syms[i]);
            }
            let bits = enc.finish();
            let mut w = BitWriter::new();
            w.append_bits(&bits);
            // Pad so the decoder's trailing renorm reads never run dry.
            w.put_bits(0, 32);
            let bytes = w.into_bytes();

            let mut r = BitReader::new(&bytes);
            let mut dec = ArithCoder::init(&mut r);
            for i in 0..n {
                let e = dec.get(&mut r, probs[i]);
                assert_eq!(e, syms[i], "seed {seed}, symbol {i}");
            }
        }
    }

    /// Probability edge values (all p = 1, all p = 128) must round-trip too.
    #[test]
    fn arith_symmetry_probability_extremes() {
        for &p in &[1u32, 128, 64] {
            let mut rng = Rng(u64::from(p) * 7 + 1);
            let n = 2000;
            let syms: Vec<u32> = (0..n).map(|_| rng.below(2)).collect();

            let mut enc = ArithEncoder::new();
            for &s in &syms {
                enc.encode(p, s);
            }
            let mut w = BitWriter::new();
            w.append_bits(&enc.finish());
            w.put_bits(0, 32);
            let bytes = w.into_bytes();

            let mut r = BitReader::new(&bytes);
            let mut dec = ArithCoder::init(&mut r);
            for (i, &s) in syms.iter().enumerate() {
                assert_eq!(dec.get(&mut r, p), s, "p {p}, symbol {i}");
            }
        }
    }

    // ---------------------------------------------------------------------
    // Golomb writer (inverse of get_ur_golomb_jpegls / get_sr_golomb_dst)
    // ---------------------------------------------------------------------

    fn put_ur_golomb_jpegls(w: &mut BitWriter, v: u32, k: u32) {
        let q = v >> k;
        for _ in 0..q {
            w.put_bit(0);
        }
        w.put_bit(1);
        w.put_bits(v & ((1 << k) - 1), k);
    }

    fn put_sr_golomb_dst(w: &mut BitWriter, v: i32, k: u32) {
        put_ur_golomb_jpegls(w, v.unsigned_abs(), k);
        if v != 0 {
            w.put_bit(u32::from(v < 0));
        }
    }

    #[test]
    fn golomb_write_read_symmetry() {
        for k in 0u32..=7 {
            let mut w = BitWriter::new();
            let vals: [i32; 11] = [0, 1, -1, 2, -2, 7, -7, 33, -33, 128, -128];
            for &v in &vals {
                put_sr_golomb_dst(&mut w, v, k);
            }
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            for &v in &vals {
                assert_eq!(get_sr_golomb_dst(&mut r, k).unwrap(), v, "k {k}, v {v}");
            }
        }
    }

    // ---------------------------------------------------------------------
    // Table + full-frame encoder (test-only DST oracle)
    // ---------------------------------------------------------------------

    fn write_uncoded_coeff(
        w: &mut BitWriter,
        coeff: &[i32],
        n: usize,
        coeff_bits: u32,
        offset: i32,
    ) {
        let mask = (1u32 << coeff_bits) - 1;
        for &c in coeff.iter().take(n) {
            w.put_bits((c - offset) as u32 & mask, coeff_bits);
        }
    }

    fn write_table(
        w: &mut BitWriter,
        t: &Table,
        code_pred: &[[i32; 3]; 3],
        length_bits: u32,
        coeff_bits: u32,
        offset: i32,
        coded: bool,
    ) {
        for i in 0..t.elements as usize {
            let len = t.length[i] as usize;
            w.put_bits(t.length[i] - 1, length_bits);
            if !coded {
                w.put_bit(0);
                write_uncoded_coeff(w, &t.coeff[i], len, coeff_bits, offset);
            } else {
                w.put_bit(1);
                let method = 1usize; // 2 predictor taps -> exercises the Golomb path
                w.put_bits(method as u32, 2);
                write_uncoded_coeff(w, &t.coeff[i], method + 1, coeff_bits, offset);
                let lsb_size = 4u32;
                w.put_bits(lsb_size, 3);
                for j in (method + 1)..len {
                    let mut x: i32 = 0;
                    for (k, &pc) in code_pred[method].iter().enumerate().take(method + 1) {
                        x = x.wrapping_add(pc.wrapping_mul(t.coeff[i][j - k - 1]));
                    }
                    let c = if x >= 0 {
                        t.coeff[i][j] + (x + 4) / 8
                    } else {
                        t.coeff[i][j] - (-x + 3) / 8
                    };
                    put_sr_golomb_dst(w, c, lsb_size);
                }
            }
        }
    }

    fn write_map(w: &mut BitWriter, elements: u32, map: &[u32; MAX_CHANNELS], channels: usize) {
        if elements == 1 {
            w.put_bit(1);
        } else {
            w.put_bit(0);
            let mut cur = 1u32;
            for &m in map.iter().take(channels).skip(1) {
                let bits = av_log2(cur) + 1;
                w.put_bits(m, bits);
                if m == cur {
                    cur += 1;
                }
            }
        }
    }

    /// Read one interleaved DSD bit at (sample `i`, channel `ch`).
    fn dsd_bit(input: &[u8], channels: usize, i: usize, ch: usize) -> u32 {
        u32::from((input[(i >> 3) * channels + ch] >> (7 - (i & 7))) & 1)
    }

    /// Encode a full DST frame from a known interleaved-DSD `input` using the
    /// given filter/probability model — the exact inverse of `decode_frame`.
    #[allow(clippy::too_many_arguments)]
    fn encode_frame(
        channels: usize,
        samples_per_frame: usize,
        input: &[u8],
        fsets: &Table,
        probs: &Table,
        map_felem: &[u32; MAX_CHANNELS],
        map_pelem: &[u32; MAX_CHANNELS],
        half: &[bool; MAX_CHANNELS],
        coded_tables: bool,
    ) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put_bit(1); // coded frame
        w.put_bit(1);
        w.put_bit(1);
        w.put_bit(1); // fully-shared segmentation
        let same_map = map_felem == map_pelem && fsets.elements == probs.elements;
        w.put_bit(u32::from(same_map));
        write_map(&mut w, fsets.elements, map_felem, channels);
        if !same_map {
            write_map(&mut w, probs.elements, map_pelem, channels);
        }
        for &h in half.iter().take(channels) {
            w.put_bit(u32::from(h));
        }
        write_table(&mut w, fsets, &FSETS_CODE_PRED, 7, 9, 0, coded_tables);
        write_table(&mut w, probs, &PROBS_CODE_PRED, 6, 7, 1, coded_tables);
        w.put_bit(0); // reserved

        let mut filter: FilterTable = [[[0; 256]; 16]; MAX_ELEMENTS];
        build_filter(&mut filter, fsets).unwrap();
        let mut status = [[0xAAu8; 16]; MAX_CHANNELS];
        let mut enc = ArithEncoder::new();
        enc.encode(prob_dst_x_bit(fsets.coeff[0][0]), 0);

        for i in 0..samples_per_frame {
            for ch in 0..channels {
                let felem = map_felem[ch] as usize;
                let predict = compute_predict(&filter, felem, &status[ch]);
                let prob = choose_prob(
                    predict,
                    half[ch],
                    i,
                    fsets.length[felem],
                    probs,
                    map_pelem[ch] as usize,
                );
                let v = dsd_bit(input, channels, i, ch);
                let residual = v ^ u32::from(predict < 0);
                enc.encode(prob, residual);
                update_status(&mut status[ch], v);
            }
        }
        w.append_bits(&enc.finish());
        w.put_bits(0, 32); // trailing zero padding for the decoder's final renorm
        w.into_bytes()
    }

    /// Build a simple stereo filter/probability model (single element each).
    fn simple_model() -> (Table, Table) {
        let mut fsets = Table::zeroed();
        fsets.elements = 1;
        fsets.length[0] = 6;
        fsets.coeff[0][..6].copy_from_slice(&[64, -32, 16, -8, 4, -2]);
        let mut probs = Table::zeroed();
        probs.elements = 1;
        probs.length[0] = 8;
        probs.coeff[0][..8].copy_from_slice(&[128, 120, 96, 64, 40, 20, 8, 2]);
        (fsets, probs)
    }

    fn patterns(len: usize, seed: u64) -> [Vec<u8>; 4] {
        let mut rng = Rng(seed);
        let random: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
        [
            alloc::vec![0x00; len],
            alloc::vec![0xFF; len],
            alloc::vec![0xAA; len],
            random,
        ]
    }

    #[test]
    fn dst_frame_roundtrip_uncoded_tables() {
        let channels = 2;
        let spf = 512; // bits per channel
        let fb = spf / 8 * channels;
        let (fsets, probs) = simple_model();
        let map = [0u32; MAX_CHANNELS];
        let half = [false; MAX_CHANNELS];

        for input in patterns(fb, 0xABCD) {
            let frame = encode_frame(
                channels, spf, &input, &fsets, &probs, &map, &map, &half, false,
            );
            let mut dec = DstDecoder::new(channels, spf).unwrap();
            let out = dec.decode_frame(&frame).unwrap();
            assert_eq!(out, &input[..], "uncoded-table frame mismatch");
        }
    }

    #[test]
    fn dst_frame_roundtrip_coded_tables() {
        // `coded_tables = true` drives read_table's predictive Golomb branch.
        let channels = 2;
        let spf = 256;
        let fb = spf / 8 * channels;
        let (fsets, probs) = simple_model();
        let map = [0u32; MAX_CHANNELS];
        let half = [false; MAX_CHANNELS];

        for input in patterns(fb, 0x1357) {
            let frame = encode_frame(
                channels, spf, &input, &fsets, &probs, &map, &map, &half, true,
            );
            let mut dec = DstDecoder::new(channels, spf).unwrap();
            let out = dec.decode_frame(&frame).unwrap();
            assert_eq!(out, &input[..], "coded-table frame mismatch");
        }
    }

    #[test]
    fn dst_frame_roundtrip_half_prob() {
        // half_prob per channel forces the uniform-probability warm-up branch.
        let channels = 2;
        let spf = 256;
        let fb = spf / 8 * channels;
        let (fsets, probs) = simple_model();
        let map = [0u32; MAX_CHANNELS];
        let half = [true; MAX_CHANNELS];

        for input in patterns(fb, 0x2468) {
            let frame = encode_frame(
                channels, spf, &input, &fsets, &probs, &map, &map, &half, false,
            );
            let mut dec = DstDecoder::new(channels, spf).unwrap();
            let out = dec.decode_frame(&frame).unwrap();
            assert_eq!(out, &input[..], "half-prob frame mismatch");
        }
    }

    #[test]
    fn dst_frame_roundtrip_two_elements() {
        // Distinct filter/probability element per channel (map [0, 1]).
        let channels = 2;
        let spf = 256;
        let fb = spf / 8 * channels;
        let mut fsets = Table::zeroed();
        fsets.elements = 2;
        fsets.length[0] = 4;
        fsets.coeff[0][..4].copy_from_slice(&[80, -40, 20, -10]);
        fsets.length[1] = 5;
        fsets.coeff[1][..5].copy_from_slice(&[-64, 48, -24, 12, -6]);
        let mut probs = Table::zeroed();
        probs.elements = 2;
        probs.length[0] = 6;
        probs.coeff[0][..6].copy_from_slice(&[128, 100, 70, 40, 16, 4]);
        probs.length[1] = 4;
        probs.coeff[1][..4].copy_from_slice(&[120, 80, 30, 6]);
        let map = [0u32, 1];
        let half = [false; MAX_CHANNELS];

        for input in patterns(fb, 0x9E37) {
            let frame = encode_frame(
                channels, spf, &input, &fsets, &probs, &map, &map, &half, false,
            );
            let mut dec = DstDecoder::new(channels, spf).unwrap();
            let out = dec.decode_frame(&frame).unwrap();
            assert_eq!(out, &input[..], "two-element frame mismatch");
        }
    }

    #[test]
    fn dst_frame_roundtrip_distinct_maps() {
        // fsets map [0, 1] (two filter elements) but probs map [0, 0] (one
        // probability element) -> same_map = false, driving decode_frame's
        // separate-probability-map branch.
        let channels = 2;
        let spf = 256;
        let fb = spf / 8 * channels;
        let mut fsets = Table::zeroed();
        fsets.elements = 2;
        fsets.length[0] = 4;
        fsets.coeff[0][..4].copy_from_slice(&[72, -36, 18, -9]);
        fsets.length[1] = 3;
        fsets.coeff[1][..3].copy_from_slice(&[-56, 40, -20]);
        let mut probs = Table::zeroed();
        probs.elements = 1;
        probs.length[0] = 6;
        probs.coeff[0][..6].copy_from_slice(&[128, 96, 64, 32, 12, 3]);
        let map_felem = [0u32, 1];
        let map_pelem = [0u32, 0];
        let half = [false; MAX_CHANNELS];

        for input in patterns(fb, 0x0BADF00D) {
            let frame = encode_frame(
                channels, spf, &input, &fsets, &probs, &map_felem, &map_pelem, &half, false,
            );
            let mut dec = DstDecoder::new(channels, spf).unwrap();
            let out = dec.decode_frame(&frame).unwrap();
            assert_eq!(out, &input[..], "distinct-map frame mismatch");
        }
    }

    #[test]
    fn dst_uncompressed_bypass_frame() {
        // A stored (uncompressed) frame: header byte 0x00 then raw DSD.
        let channels = 2;
        let spf = 128;
        let fb = spf / 8 * channels;
        let mut rng = Rng(0x5A5A);
        let raw: Vec<u8> = (0..fb).map(|_| rng.below(256) as u8).collect();
        let mut frame = alloc::vec![0u8; 1 + fb];
        frame[1..].copy_from_slice(&raw);

        let mut dec = DstDecoder::new(channels, spf).unwrap();
        let out = dec.decode_frame(&frame).unwrap();
        assert_eq!(out, &raw[..]);
    }

    #[test]
    fn dst_decoder_rejects_unsupported_geometry() {
        assert_eq!(
            DstDecoder::new(3, 512).err(),
            Some(DecodeError::Unsupported)
        );
        assert_eq!(
            DstDecoder::new(0, 512).err(),
            Some(DecodeError::Unsupported)
        );
        assert_eq!(
            DstDecoder::new(2, 500).err(),
            Some(DecodeError::Unsupported)
        );
        // Exactly stereo DSD128 is the largest accepted frame.
        assert_eq!(DstDecoder::new(2, MAX_FRAME_BYTES_PER_CH * 8).err(), None);
        // One step larger is declined.
        assert_eq!(
            DstDecoder::new(2, 2 * MAX_FRAME_BYTES_PER_CH * 8).err(),
            Some(DecodeError::Unsupported)
        );
    }

    // ---------------------------------------------------------------------
    // End-to-end: real coded frames through the DFF container
    // ---------------------------------------------------------------------

    /// Assemble a DSDIFF file whose sound data is a `DST ` list (PROP with
    /// `CMPR "DST "`, an `FRTE`, and one `DSTF` per supplied frame payload).
    fn build_dst_dff(freq: u32, channels: u16, frame_rate: u16, frames: &[Vec<u8>]) -> Vec<u8> {
        let mut prop = Vec::new();
        prop.extend_from_slice(b"SND ");
        prop.extend_from_slice(b"FS  ");
        prop.extend_from_slice(&4u64.to_be_bytes());
        prop.extend_from_slice(&freq.to_be_bytes());
        let chnl_body = 2 + channels as usize * 4;
        prop.extend_from_slice(b"CHNL");
        prop.extend_from_slice(&(chnl_body as u64).to_be_bytes());
        prop.extend_from_slice(&channels.to_be_bytes());
        for _ in 0..channels {
            prop.extend_from_slice(b"SLFT");
        }
        prop.extend_from_slice(b"CMPR");
        prop.extend_from_slice(&8u64.to_be_bytes());
        prop.extend_from_slice(b"DST ");
        prop.push(3);
        prop.extend_from_slice(b"DST");

        let mut dst = Vec::new();
        dst.extend_from_slice(b"FRTE");
        dst.extend_from_slice(&6u64.to_be_bytes());
        dst.extend_from_slice(&(frames.len() as u32).to_be_bytes());
        dst.extend_from_slice(&frame_rate.to_be_bytes());
        for f in frames {
            dst.extend_from_slice(b"DSTF");
            dst.extend_from_slice(&(f.len() as u64).to_be_bytes());
            dst.extend_from_slice(f);
            if f.len() % 2 == 1 {
                dst.push(0);
            }
        }

        let mut body = Vec::new();
        body.extend_from_slice(b"PROP");
        body.extend_from_slice(&(prop.len() as u64).to_be_bytes());
        body.extend_from_slice(&prop);
        body.extend_from_slice(b"DST ");
        body.extend_from_slice(&(dst.len() as u64).to_be_bytes());
        body.extend_from_slice(&dst);

        let mut v = Vec::new();
        v.extend_from_slice(b"FRM8");
        v.extend_from_slice(&(4u64 + body.len() as u64).to_be_bytes());
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&body);
        v
    }

    /// The full pipeline: encode several patterns as real arithmetic-coded DSTF
    /// frames, wrap them in a DFF `DST ` container, and decode the whole thing
    /// through `DffDecoder::read_dsd` — asserting bit-exact interleaved DSD.
    #[test]
    fn dst_dff_end_to_end_through_container() {
        use crate::decoder::DsdDecoder;
        use crate::formats::dff::DffDecoder;
        use crate::io::SliceSource;

        let freq = 2_822_400u32;
        let frame_rate = 11_025u16; // spf = 256 bits/ch, frame_bytes = 64
        let channels = 2usize;
        let spf = (freq / u32::from(frame_rate)) as usize;
        let fb = spf / 8 * channels;
        let (fsets, probs) = simple_model();
        let map = [0u32; MAX_CHANNELS];
        let half = [false; MAX_CHANNELS];

        let inputs = patterns(fb, 0x00C0_FFEE);
        let frames: Vec<Vec<u8>> = inputs
            .iter()
            .map(|inp| encode_frame(channels, spf, inp, &fsets, &probs, &map, &map, &half, true))
            .collect();

        let file = build_dst_dff(freq, channels as u16, frame_rate, &frames);
        let mut d = DffDecoder::new(SliceSource::new(&file)).unwrap();
        assert_eq!(d.stream_info().channels, 2);

        let total = fb * inputs.len();
        let mut out = alloc::vec![0u8; total];
        let mut got = 0;
        loop {
            let n = d.read_dsd(&mut out[got..]).unwrap();
            if n == 0 {
                break;
            }
            got += n;
        }
        assert_eq!(got, total);

        let mut expect = Vec::new();
        for inp in &inputs {
            expect.extend_from_slice(inp);
        }
        assert_eq!(out, expect, "container round-trip mismatch");
    }
}
