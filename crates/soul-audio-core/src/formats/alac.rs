//! ALAC (Apple Lossless) decoder — decodes an `mdat` ALAC bitstream (from the
//! MP4 reader) to interleaved `i32`. Algorithm transcribed from Apple's
//! reference (`macosforge/alac`, Apache-2.0): `ALACDecoder.cpp`, `ag_dec.c`,
//! `dp_dec.c`, `matrix_dec.c`.

/// MSB-first bit reader over a byte slice. Reads past the end return zero-padded
/// bits (a truncated final frame is tolerated).
// `read_long`/`advance`/`set_bit_pos`/`peek32` are not yet called outside this
// module's own tests — the codec (Task 3) consumes them.
#[allow(dead_code)]
struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

#[allow(dead_code)]
impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn byte(&self, i: usize) -> u32 {
        self.data.get(i).copied().map_or(0, u32::from)
    }

    /// Read `n` bits (n ≤ 16), MSB-first, right-aligned.
    fn read(&mut self, n: u32) -> u32 {
        let byte = self.bit_pos >> 3;
        let bit = (self.bit_pos & 7) as u32;
        let window = (self.byte(byte) << 16) | (self.byte(byte + 1) << 8) | self.byte(byte + 2);
        let v = ((window << bit) & 0x00FF_FFFF) >> (24 - n);
        self.bit_pos += n as usize;
        v
    }

    /// Read `n` bits (n ≤ 32).
    fn read_long(&mut self, n: u32) -> u32 {
        if n <= 16 {
            self.read(n)
        } else {
            let hi = self.read(16);
            let lo = self.read(n - 16);
            (hi << (n - 16)) | lo
        }
    }

    fn read_one(&mut self) -> u32 {
        let v = (self.byte(self.bit_pos >> 3) >> (7 - (self.bit_pos & 7) as u32)) & 1;
        self.bit_pos += 1;
        v
    }

    fn byte_align(&mut self) {
        let rem = self.bit_pos & 7;
        if rem != 0 {
            self.bit_pos += 8 - rem;
        }
    }

    fn advance(&mut self, n: usize) {
        self.bit_pos += n;
    }
    fn bit_pos(&self) -> usize {
        self.bit_pos
    }
    fn set_bit_pos(&mut self, p: usize) {
        self.bit_pos = p;
    }

    /// Big-endian 32-bit window at the current position, left-aligned (top bit =
    /// next bit to read). Used by the entropy stage which computes prefixes.
    fn peek32(&self) -> u32 {
        let byte = self.bit_pos >> 3;
        let bit = (self.bit_pos & 7) as u32;
        let w = (self.byte(byte) << 24)
            | (self.byte(byte + 1) << 16)
            | (self.byte(byte + 2) << 8)
            | self.byte(byte + 3);
        if bit == 0 {
            w
        } else {
            (w << bit) | (self.byte(byte + 4) >> (8 - bit))
        }
    }
}

// ===========================================================================
// ALAC codec core.
//
// Adaptive Golomb/Rice residual decode + adaptive FIR predictor + stereo
// un-mix + the AAC-style element/packet loop. The entropy (`dyn_get_32bit`,
// `dyn_get_16bit`, `dyn_decomp`), predictor (`unpc_block`) and un-mix
// arithmetic are transcribed 1:1 from Apple's reference codec
// (`ag_dec.c`, `dp_dec.c`, `matrix_dec.c`, `ALACDecoder.cpp`, Apache-2.0). All
// 32-bit wrap-around the reference relies on is reproduced with `wrapping_*`
// (sample reconstruction) or `i64` intermediates (the predictor sum).
// ===========================================================================

use crate::decoder::{DecodeError, Decoder, SeekableDecoder};
use crate::demux::mp4::Mp4Reader;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

const MAX_CHANNELS: usize = 2; // v1: mono + stereo
const MAX_FRAME: usize = 4096; // frameLength cap
const QBSHIFT: u32 = 9;
const QB: i32 = 1 << QBSHIFT; // 512
const MMULSHIFT: u32 = 2;
const MDENSHIFT: u32 = QBSHIFT - MMULSHIFT - 1; // 6
const MOFF: i32 = 1 << (MDENSHIFT - 2); // 16
const BITOFF: i32 = 24;
const MAX_PREFIX: u32 = 9;
const MAX_DATATYPE_BITS_16: u32 = 16;
const N_MAX_MEAN_CLAMP: u32 = 0xFFFF;

/// Input byte buffer capacity: one full ALAC packet must fit. The worst case is
/// a stereo escape (verbatim) frame at the widest supported depth (24-bit ->
/// 3 bytes/sample): `MAX_FRAME * channels * 3` bytes of samples plus a little
/// element-header overhead. Sizing for 3 bytes/sample keeps 20/24-bit verbatim
/// frames from overflowing the buffer (a 2-byte cap silently desynced them).
const PACKET_CAP: usize = MAX_FRAME * MAX_CHANNELS * 3 + 1024;

/// `lead(x)` == number of leading zero bits (`lead(0) == 32`, matching the
/// reference's `lead()`).
#[inline]
fn lead(x: u32) -> u32 {
    x.leading_zeros()
}

/// `lg3a(x) = 31 - lead(x+3)` ~= floor(log2(x+3)).
#[inline]
fn lg3a(x: u32) -> u32 {
    31 - lead(x.wrapping_add(3))
}

/// Branchless sign: `-1`, `0`, or `+1` (Apple's `sign_of_int`).
#[inline]
fn sign_of(x: i32) -> i32 {
    (((x.wrapping_neg() as u32) >> 31) as i32) | (x >> 31)
}

/// Read one Golomb value with parameter `m = (1<<k)-1`, quotient escape at
/// prefix >= `MAX_PREFIX` (-> literal of `maxbits`).
fn dyn_get_32bit(r: &mut BitReader, k: u32, maxbits: u32) -> u32 {
    let stream = r.peek32();
    let prefix = lead(!stream);
    if prefix >= MAX_PREFIX {
        r.advance(MAX_PREFIX as usize);
        return r.read_long(maxbits);
    }
    let m = (1u32 << k) - 1;
    r.advance((prefix + 1) as usize); // 'prefix' ones + terminating zero
    if k != 1 {
        let v = (stream << (prefix + 1)) >> (32 - k); // next k bits
        r.advance((k - 1) as usize);
        let mut result = prefix * m;
        if v >= 2 {
            result += v - 1;
            r.advance(1);
        }
        result
    } else {
        prefix
    }
}

/// Zero-run reader (16-bit variant).
fn dyn_get_16bit(r: &mut BitReader, mz: u32, k: u32) -> u32 {
    let stream = r.peek32();
    let prefix = lead(!stream);
    if prefix >= MAX_PREFIX {
        r.advance(MAX_PREFIX as usize);
        return r.read_long(MAX_DATATYPE_BITS_16);
    }
    r.advance((prefix + 1) as usize);
    let v = (stream << (prefix + 1)) >> (32 - k);
    if v < 2 {
        r.advance((k - 1) as usize);
        prefix * mz
    } else {
        r.advance(k as usize);
        prefix * mz + v - 1
    }
}

/// `dyn_decomp`: decode `num` residuals into `pc`. `chan_bits` is the literal
/// escape width; `mb0`/`pb`/`kb` are the adaptive-Golomb parameters.
#[allow(clippy::needless_range_loop)]
fn dyn_decomp(
    r: &mut BitReader,
    pc: &mut [i32],
    num: usize,
    chan_bits: u32,
    mb0: u32,
    pb: u32,
    kb: u32,
) {
    let wb = (1u32 << kb) - 1;
    let mut mb = mb0;
    let mut zmode = 0u32;
    let mut c = 0usize;
    while c < num {
        let mut k = lg3a(mb >> QBSHIFT);
        k = k.min(kb);
        let n = dyn_get_32bit(r, k, chan_bits);

        let ndecode = n + zmode;
        let del = if ndecode & 1 != 0 {
            -(((ndecode + 1) >> 1) as i32)
        } else {
            ((ndecode + 1) >> 1) as i32
        };
        pc[c] = del;
        c += 1;

        // mb = pb*(n+zmode) + mb - ((pb*mb) >> QBSHIFT)
        mb = pb
            .wrapping_mul(n + zmode)
            .wrapping_add(mb)
            .wrapping_sub(pb.wrapping_mul(mb) >> QBSHIFT);
        if n > N_MAX_MEAN_CLAMP {
            mb = N_MAX_MEAN_CLAMP;
        }
        zmode = 0;

        if (mb << MMULSHIFT) < QB as u32 && c < num {
            zmode = 1;
            let kz = (lead(mb) as i32 - BITOFF + ((mb as i32 + MOFF) >> MDENSHIFT)) as u32;
            let mz = ((1u32 << kz) - 1) & wb;
            let run = dyn_get_16bit(r, mz, kz) as usize;
            for _ in 0..run {
                if c >= num {
                    break;
                }
                pc[c] = 0;
                c += 1;
            }
            if run >= 65535 {
                zmode = 0;
            }
            mb = 0;
        }
    }
}

/// Order-31 long-term integrator, run in place on the residuals when the
/// sub-frame predictor `mode != 0` (Apple's `unpc_block(.., .., 31, .., 0)`).
#[allow(clippy::needless_range_loop)]
fn dc31_inplace(buf: &mut [i32], num: usize, chan_bits: u32) {
    let chanshift = 32 - chan_bits;
    let mut prev = buf[0];
    for j in 1..num {
        let del = buf[j].wrapping_add(prev);
        prev = (del << chanshift) >> chanshift;
        buf[j] = prev;
    }
}

/// `unpc_block`: reconstruct `out[0..num]` from residuals `pc1` using
/// `numactive` adaptive FIR coefficients (`coefs`), sign-sign LMS adapted.
#[allow(clippy::needless_range_loop)]
fn unpc_block(
    pc1: &[i32],
    out: &mut [i32],
    num: usize,
    coefs: &mut [i32],
    numactive: usize,
    chan_bits: u32,
    denshift: u32,
) {
    let chanshift = 32 - chan_bits;
    let denhalf = 1i32 << (denshift.max(1) - 1);

    out[0] = pc1[0];
    if numactive == 0 {
        out[..num].copy_from_slice(&pc1[..num]);
        return;
    }
    if numactive == 31 {
        // order-31 long-term integrator
        let mut prev = out[0];
        for j in 1..num {
            let del = pc1[j].wrapping_add(prev);
            prev = (del << chanshift) >> chanshift;
            out[j] = prev;
        }
        return;
    }

    // warm-up: first `numactive` samples via a first-order integrator
    for j in 1..=numactive {
        let del = pc1[j].wrapping_add(out[j - 1]);
        out[j] = (del << chanshift) >> chanshift;
    }
    let lim = numactive + 1;
    for j in lim..num {
        let top = out[j - lim]; // out[j - numactive - 1]
        let mut sum1 = 0i64;
        for k in 0..numactive {
            sum1 += i64::from(coefs[k]) * i64::from(out[j - 1 - k] - top);
        }
        let mut del0 = pc1[j];
        let sg = sign_of(pc1[j]);
        let del = pc1[j]
            .wrapping_add(top)
            .wrapping_add(((sum1 + i64::from(denhalf)) >> denshift) as i32);
        out[j] = (del << chanshift) >> chanshift;

        // sign-sign LMS coefficient adaptation
        match sg.cmp(&0) {
            core::cmp::Ordering::Greater => {
                for k in (0..numactive).rev() {
                    let dd = top - out[j - 1 - k];
                    let sgn = sign_of(dd);
                    coefs[k] -= sgn;
                    del0 -= (numactive - k) as i32 * ((sgn * dd) >> denshift);
                    if del0 <= 0 {
                        break;
                    }
                }
            }
            core::cmp::Ordering::Less => {
                for k in (0..numactive).rev() {
                    let dd = top - out[j - 1 - k];
                    let sgn = sign_of(dd);
                    coefs[k] += sgn;
                    del0 -= (numactive - k) as i32 * ((-sgn * dd) >> denshift);
                    if del0 >= 0 {
                        break;
                    }
                }
            }
            core::cmp::Ordering::Equal => {}
        }
    }
}

/// Immutable per-stream ALAC parameters, threaded into the packet decode.
#[derive(Clone, Copy)]
struct AlacCfg {
    bit_depth: u32,
    channels: usize,
    pb: u32,
    mb: u32,
    kb: u32,
    frame_length: usize,
}

/// Fixed-size decode scratch (no allocation): the two predictor output buffers,
/// the residual buffer, the wasted-bits ("shift") buffer, and the interleaved
/// output buffer handed to the caller.
struct Scratch {
    mix_u: [i32; MAX_FRAME],
    mix_v: [i32; MAX_FRAME],
    resid: [i32; MAX_FRAME],
    shift: [u16; MAX_FRAME * MAX_CHANNELS],
    out: [i32; MAX_FRAME * MAX_CHANNELS],
}

/// Decode one SCE/LFE (`elem_ch == 1`) or CPE (`elem_ch == 2`) element into the
/// interleaved output buffer at `channel_index`.
#[allow(clippy::needless_range_loop)]
fn decode_channel_element(
    r: &mut BitReader,
    s: &mut Scratch,
    cfg: AlacCfg,
    channel_index: usize,
    num_samples: &mut usize,
    elem_ch: usize,
) -> Result<(), DecodeError> {
    let _instance = r.read(4);
    if r.read(12) != 0 {
        return Err(DecodeError::Malformed);
    }
    let header = r.read(4);
    let partial = header >> 3;
    let bytes_shifted = (header >> 1) & 0x3;
    if bytes_shifted == 3 {
        return Err(DecodeError::Malformed);
    }
    let escape = header & 0x1;
    let shift = bytes_shifted * 8;
    if shift >= cfg.bit_depth {
        // `chan_bits` below (`bit_depth - shift`) would be zero (or, absent
        // this guard, underflow): a wasted-bits shift that consumes the
        // entire sample width is malformed — it would later drive
        // `chanshift = 32 - chan_bits` to 32, a shift-amount overflow.
        return Err(DecodeError::Malformed);
    }

    if partial != 0 {
        let hi = r.read(16);
        let lo = r.read(16);
        *num_samples = ((hi << 16) | lo) as usize;
    }
    let num = *num_samples;
    if num > MAX_FRAME {
        return Err(DecodeError::Unsupported);
    }

    // CPE runs the predictor one bit wider (the mid/side residual gains a bit).
    let mut chan_bits = cfg.bit_depth - shift;
    if elem_ch == 2 {
        chan_bits += 1;
    }
    if chan_bits > 32 {
        return Err(DecodeError::Malformed);
    }

    let mut mix_bits = 0u32;
    let mut mix_res = 0i32;

    if escape == 0 {
        // compressed frame
        mix_bits = r.read(8);
        if mix_bits >= 32 {
            // Used as a shift amount in the un-mix (`>> mix_bits`); >= 32 is
            // a shift-amount overflow.
            return Err(DecodeError::Malformed);
        }
        mix_res = i32::from(r.read(8) as u8 as i8);

        let mut modes = [0u32; MAX_CHANNELS];
        let mut denshifts = [0u32; MAX_CHANNELS];
        let mut nums = [0usize; MAX_CHANNELS];
        let mut pbfactors = [0u32; MAX_CHANNELS];
        let mut coefs = [[0i32; 32]; MAX_CHANNELS];

        for e in 0..elem_ch {
            let hb = r.read(8);
            modes[e] = hb >> 4;
            denshifts[e] = hb & 0xF;
            let hb = r.read(8);
            pbfactors[e] = hb >> 5;
            nums[e] = (hb & 0x1F) as usize;
            for slot in coefs[e][..nums[e]].iter_mut() {
                *slot = i32::from(r.read(16) as u16 as i16);
            }
        }

        // The wasted-bits ("shift") buffer precedes the residuals; note its
        // start, skip it, decode residuals, then come back and read it.
        let shift_pos = r.bit_pos();
        if bytes_shifted != 0 {
            r.advance(shift as usize * elem_ch * num);
        }

        for e in 0..elem_ch {
            let pb_eff = (cfg.pb * pbfactors[e]) / 4;
            dyn_decomp(r, &mut s.resid, num, chan_bits, cfg.mb, pb_eff, cfg.kb);
            if modes[e] != 0 {
                dc31_inplace(&mut s.resid, num, chan_bits);
            }
            if e == 0 {
                unpc_block(
                    &s.resid,
                    &mut s.mix_u,
                    num,
                    &mut coefs[0][..nums[0]],
                    nums[0],
                    chan_bits,
                    denshifts[0],
                );
            } else {
                unpc_block(
                    &s.resid,
                    &mut s.mix_v,
                    num,
                    &mut coefs[1][..nums[1]],
                    nums[1],
                    chan_bits,
                    denshifts[1],
                );
            }
        }

        if bytes_shifted != 0 {
            let save = r.bit_pos();
            r.set_bit_pos(shift_pos);
            for slot in s.shift[..num * elem_ch].iter_mut() {
                *slot = r.read(shift) as u16;
            }
            r.set_bit_pos(save);
        }
    } else {
        // uncompressed / verbatim frame: samples stored directly (interleaved
        // for stereo). Escape implies no wasted-bits shift.
        chan_bits = cfg.bit_depth;
        let sh = 32 - chan_bits;
        if chan_bits <= 16 {
            for i in 0..num {
                s.mix_u[i] = ((r.read(chan_bits) << sh) as i32) >> sh;
                if elem_ch == 2 {
                    s.mix_v[i] = ((r.read(chan_bits) << sh) as i32) >> sh;
                }
            }
        } else {
            let extra = chan_bits - 16;
            for i in 0..num {
                let hi = ((r.read(16) << 16) as i32) >> sh;
                s.mix_u[i] = hi | r.read(extra) as i32;
                if elem_ch == 2 {
                    let hi = ((r.read(16) << 16) as i32) >> sh;
                    s.mix_v[i] = hi | r.read(extra) as i32;
                }
            }
        }
    }

    // ---- un-mix + reassemble wasted bits + write interleaved output ----
    let stride = cfg.channels;
    let base = channel_index;
    let is16 = cfg.bit_depth == 16;
    let apply_shift = escape == 0 && bytes_shifted != 0;

    if elem_ch == 1 {
        for i in 0..num {
            let mut val = s.mix_u[i];
            if apply_shift {
                val = (val << shift) | i32::from(s.shift[i]);
            }
            s.out[i * stride + base] = if is16 { i32::from(val as i16) } else { val };
        }
    } else {
        for i in 0..num {
            let u = s.mix_u[i];
            let v = s.mix_v[i];
            let (mut l, mut rr) = if mix_res != 0 {
                let l = u
                    .wrapping_add(v)
                    .wrapping_sub(mix_res.wrapping_mul(v) >> mix_bits);
                (l, l.wrapping_sub(v))
            } else {
                (u, v)
            };
            if apply_shift {
                l = (l << shift) | i32::from(s.shift[2 * i]);
                rr = (rr << shift) | i32::from(s.shift[2 * i + 1]);
            }
            if is16 {
                s.out[i * stride + base] = i32::from(l as i16);
                s.out[i * stride + base + 1] = i32::from(rr as i16);
            } else {
                s.out[i * stride + base] = l;
                s.out[i * stride + base + 1] = rr;
            }
        }
    }
    Ok(())
}

/// Decode one ALAC packet from `data` into `s.out`. Returns
/// `(bytes_consumed, frames)` where `bytes_consumed` is the byte-aligned packet
/// length (used to advance the sliding input buffer) and `frames` is the
/// per-channel sample count produced.
fn decode_packet(
    data: &[u8],
    s: &mut Scratch,
    cfg: AlacCfg,
) -> Result<(usize, usize), DecodeError> {
    let mut r = BitReader::new(data);
    let mut num_samples = cfg.frame_length;
    let mut channel_index = 0usize;
    let mut guard = 0u32;
    loop {
        let tag = r.read(3);
        match tag {
            0 | 3 => {
                // SCE / LFE — mono element
                if channel_index + 1 > cfg.channels {
                    return Err(DecodeError::Malformed);
                }
                decode_channel_element(&mut r, s, cfg, channel_index, &mut num_samples, 1)?;
                channel_index += 1;
            }
            1 => {
                // CPE — stereo element
                if channel_index + 2 > cfg.channels {
                    return Err(DecodeError::Malformed);
                }
                decode_channel_element(&mut r, s, cfg, channel_index, &mut num_samples, 2)?;
                channel_index += 2;
            }
            4 => {
                // DSE — data stream element, skip
                let _instance = r.read(4);
                let align = r.read_one();
                let mut count = r.read(8);
                if count == 255 {
                    count += r.read(8);
                }
                if align != 0 {
                    r.byte_align();
                }
                r.advance(count as usize * 8);
            }
            6 => {
                // FIL — fill element, skip
                let mut count = i64::from(r.read(4));
                if count == 15 {
                    count += i64::from(r.read(8)) - 1;
                }
                if count > 0 {
                    r.advance(count as usize * 8);
                }
            }
            7 => {
                // END — byte-align the frame and stop
                r.byte_align();
                break;
            }
            _ => return Err(DecodeError::Unsupported), // CCE / PCE
        }
        guard += 1;
        if guard > 64 {
            return Err(DecodeError::Malformed);
        }
    }
    Ok((r.bit_pos() / 8, num_samples))
}

/// A streaming ALAC (Apple Lossless) decoder over an MP4/M4A [`Source`].
///
/// Decodes the `mdat` ALAC packets produced by [`Mp4Reader`] to interleaved
/// `i32` PCM. v1 supports mono and stereo at 16/20/24-bit; 16- and 24-bit are
/// gated bit-exact by golden fixtures (20-bit shares the same width-generic
/// path but has no fixture yet). No allocation: all buffers are fixed-size and
/// owned by value.
pub struct AlacDecoder<S> {
    reader: Mp4Reader<S>,
    scratch: Scratch,
    inbuf: [u8; PACKET_CAP],
    filled: usize,
    used: usize,
    samples_left: u64,
    total_samples: u64,
    out_len: usize,
    out_pos: usize,
    eof: bool,
    cfg: AlacCfg,
    sample_rate: u32,
}

impl<S: Source> AlacDecoder<S> {
    /// Parse the MP4 header and prepare to stream ALAC packets.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for >2 channels or a frame length beyond the
    /// fixed buffer; container errors propagate from [`Mp4Reader`].
    pub fn new(src: S) -> Result<Self, DecodeError> {
        Self::from_reader(Mp4Reader::new(src)?)
    }

    /// Construct from an already-parsed [`Mp4Reader`] (its header walked to
    /// `mdat`). Lets [`crate::dispatch::open`] build the reader once, inspect
    /// [`Mp4Reader::codec`], and route an `alac` sample entry here without
    /// re-reading the container. `new` is this over a fresh `Mp4Reader`.
    ///
    /// # Errors
    /// As [`Self::new`]: [`DecodeError::Unsupported`] for >2 channels, an
    /// out-of-range frame length, or a sample entry that wasn't ALAC.
    pub fn from_reader(reader: Mp4Reader<S>) -> Result<Self, DecodeError> {
        let c = *reader.config();
        if c.channels == 0 || c.channels as usize > MAX_CHANNELS {
            return Err(DecodeError::Unsupported);
        }
        if c.frame_length == 0 || c.frame_length as usize > MAX_FRAME {
            return Err(DecodeError::Unsupported);
        }
        // The cookie advertises the largest packet the stream can emit. If it
        // exceeds the fixed input buffer, a single `read` can't hold a whole
        // packet — the tail would zero-pad and silently desync. Reject it as a
        // clean error rather than decode corrupt output.
        if c.max_frame_bytes as usize > PACKET_CAP {
            return Err(DecodeError::Unsupported);
        }
        // `bit_depth` drives every `chan_bits`/`chanshift` computation in the
        // decode path; 16/20/24 are the only widths this decoder's shift and
        // reader-width (`BitReader::read` assumes n <= 16, so `extra =
        // bit_depth - 16` must stay <= 16) invariants were verified against.
        // 32-bit stereo in particular is non-functional here (`chan_bits =
        // 32 + 1 = 33` underflows `32 - chan_bits`), so it's out of scope —
        // reject it here rather than let it corrupt or panic mid-decode.
        if !matches!(c.bit_depth, 16 | 20 | 24) {
            return Err(DecodeError::Unsupported);
        }
        // `kb` seeds every element's adaptive Golomb parameter (`k =
        // lg3a(..).min(kb)`) and sizes the Rice modulus `(1 << kb) - 1`; 0
        // and >=32 both drive a `1u32 << kb` / `>> (32 - kb)` shift-amount
        // overflow.
        if c.kb == 0 || c.kb > 31 {
            return Err(DecodeError::Unsupported);
        }
        let cfg = AlacCfg {
            bit_depth: u32::from(c.bit_depth),
            channels: c.channels as usize,
            pb: u32::from(c.pb),
            mb: u32::from(c.mb),
            kb: u32::from(c.kb),
            frame_length: c.frame_length as usize,
        };
        let total_samples = reader.total_samples();
        // A fragmented stream carries no `mdhd` total (it reads 0), so decode
        // can't be gated on a known sample count — run until the `mdat` stream
        // is exhausted (`read_audio` returns 0). The final packet may then emit
        // up to `frame_length - 1` trailing padding samples, since there is no
        // sample table to trim it — acceptable, matching the ADTS/raw-AAC path.
        let samples_left = if reader.is_fragmented() {
            u64::MAX
        } else {
            total_samples
        };
        Ok(Self {
            reader,
            scratch: Scratch {
                mix_u: [0; MAX_FRAME],
                mix_v: [0; MAX_FRAME],
                resid: [0; MAX_FRAME],
                shift: [0; MAX_FRAME * MAX_CHANNELS],
                out: [0; MAX_FRAME * MAX_CHANNELS],
            },
            inbuf: [0; PACKET_CAP],
            filled: 0,
            used: 0,
            samples_left,
            total_samples,
            out_len: 0,
            out_pos: 0,
            eof: false,
            cfg,
            sample_rate: c.sample_rate,
        })
    }

    /// Total per-channel frames, from the container's `mdhd` duration.
    /// `None` for a fragmented stream, whose `moov` declares no total.
    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        if self.reader.is_fragmented() {
            None
        } else {
            Some(self.total_samples)
        }
    }

    /// Gapless encoder delay (leading priming, per-channel samples) from the
    /// MP4 `elst` edit list; `0` if the file declares none. The decoder emits
    /// the full decode — a player trims this many samples off the front for
    /// gapless playback. (ALAC is lossless and usually has no real delay, but a
    /// container-level edit list is still honored when present.)
    #[must_use]
    pub fn encoder_delay(&self) -> u64 {
        self.reader.encoder_delay()
    }

    /// Refill the sliding input buffer and decode one packet into `scratch.out`.
    /// Returns `Ok(false)` at end of stream.
    fn decode_next_packet(&mut self) -> Result<bool, DecodeError> {
        if self.samples_left == 0 {
            return Ok(false);
        }
        // Shift bytes left over from the previous packet to the front.
        if self.used > 0 {
            if self.used < self.filled {
                self.inbuf.copy_within(self.used..self.filled, 0);
                self.filled -= self.used;
            } else {
                self.filled = 0;
            }
            self.used = 0;
        }
        // Refill from the mdat stream (up to capacity). `read_audio` bounds the
        // read to the current `mdat` and, for a fragmented file, transparently
        // crosses into the next fragment's `mdat`; it returns 0 at true EOF.
        while self.filled < self.inbuf.len() {
            let n = self.reader.read_audio(&mut self.inbuf[self.filled..])?;
            if n == 0 {
                break;
            }
            self.filled += n;
        }
        if self.filled == 0 {
            return Ok(false);
        }
        let cfg = self.cfg;
        let (bytes, frames) = decode_packet(&self.inbuf[..self.filled], &mut self.scratch, cfg)?;
        self.used = bytes;
        self.samples_left = self.samples_left.saturating_sub(frames as u64);
        self.out_len = frames * self.cfg.channels;
        self.out_pos = 0;
        Ok(frames > 0)
    }
}

impl<S: Source> Decoder for AlacDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.sample_rate,
            channels: self.cfg.channels as u16,
            format: SampleFormat::Int {
                bits: self.cfg.bit_depth as u8,
            },
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let mut written = 0;
        while written < out.len() {
            if self.out_pos >= self.out_len {
                if self.eof || self.samples_left == 0 {
                    break;
                }
                if !self.decode_next_packet()? {
                    self.eof = true;
                    break;
                }
            }
            let n = (out.len() - written).min(self.out_len - self.out_pos);
            out[written..written + n]
                .copy_from_slice(&self.scratch.out[self.out_pos..self.out_pos + n]);
            self.out_pos += n;
            written += n;
        }
        Ok(written)
    }
}

impl<S: SeekSource> AlacDecoder<S> {
    /// Seek to per-channel `sample` (clamped to the stream length; returns the
    /// landed sample, always `== sample` when in range). **Sample-accurate**:
    /// ALAC packets are fully independent (each carries its own predictor
    /// coefficients and overwrites the scratch buffers), so there is no
    /// cross-frame state and no pre-roll — land on the coded frame containing
    /// the target via the MP4 sample table, then trim the first decoded packet
    /// to the exact sample.
    ///
    /// # Errors
    /// Propagates [`Mp4Reader::frame_file_offset`] table / IO errors and the
    /// first packet's decode error.
    pub fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        let frame_len = self.cfg.frame_length as u64;
        let target = sample.min(self.total_samples);
        let frame_index =
            u32::try_from(target / frame_len).map_err(|_| DecodeError::Unsupported)?;
        let frame_start = u64::from(frame_index) * frame_len;

        let num_frames = self.reader.coded_frame_count()?;
        let offset = self.reader.frame_file_offset(frame_index)?;
        let end_offset = self.reader.frame_file_offset(num_frames)?;

        self.reader
            .seek_audio(offset, end_offset.saturating_sub(offset))?;
        self.filled = 0;
        self.used = 0;
        self.out_len = 0;
        self.out_pos = 0;
        self.eof = false;
        self.samples_left = self.total_samples.saturating_sub(frame_start);

        // Trim within the landed frame to the exact target sample.
        let drop_frames = target - frame_start; // per-channel samples to discard
        if drop_frames > 0 && self.decode_next_packet()? {
            let drop = (drop_frames * self.cfg.channels as u64).min(self.out_len as u64) as usize;
            self.out_pos = drop;
        }
        Ok(target)
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to`]).
    ///
    /// # Errors
    /// As [`Self::seek_to`].
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        self.seek_to(crate::decoder::secs_to_samples(secs, self.sample_rate))
    }
}

impl<S: SeekSource> SeekableDecoder for AlacDecoder<S> {
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        AlacDecoder::seek_to(self, sample)
    }
    fn total_frames(&self) -> Option<u64> {
        AlacDecoder::total_frames(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first() {
        // 0b1010_0110, 0b1100_0000
        let mut r = BitReader::new(&[0xA6, 0xC0]);
        assert_eq!(r.read(3), 0b101); // top 3 bits
        assert_eq!(r.read(1), 0);
        assert_eq!(r.read(4), 0b0110);
        assert_eq!(r.read_one(), 1);
        assert_eq!(r.read(3), 0b100);
    }

    #[test]
    fn byte_align_and_peek32() {
        let mut r = BitReader::new(&[0xFF, 0x00, 0x12, 0x34, 0x56]);
        assert_eq!(r.read(4), 0xF);
        r.byte_align();
        assert_eq!(r.bit_pos(), 8);
        // peek32 from byte 1: 0x00123456
        assert_eq!(r.peek32(), 0x0012_3456);
        r.advance(8);
        assert_eq!(r.read(8), 0x12);
    }

    #[test]
    fn read_long_over_16() {
        let mut r = BitReader::new(&[0x12, 0x34, 0x56]);
        assert_eq!(r.read_long(24), 0x0012_3456);
    }
}

#[cfg(test)]
mod golden {
    extern crate alloc;

    use super::*;
    use crate::decoder::Decoder;
    use crate::io::SliceSource;

    const ALAC: &[u8] = include_bytes!("../../testdata/alac.m4a");
    const REF: &[u8] = include_bytes!("../../testdata/alac.ref.pcm"); // s16le interleaved
    const ALAC24: &[u8] = include_bytes!("../../testdata/alac24.m4a");
    const REF24: &[u8] = include_bytes!("../../testdata/alac24.ref.pcm"); // s24le interleaved

    #[test]
    fn decodes_bit_exact_vs_reference() {
        let mut dec = AlacDecoder::new(SliceSource::new(ALAC)).expect("open alac");
        let si = dec.stream_info();
        assert_eq!(si.sample_rate, 44_100);
        assert_eq!(si.channels, 2);
        assert_eq!(si.format, SampleFormat::Int { bits: 16 });

        // Decode all samples.
        let mut out = alloc::vec::Vec::new();
        let mut buf = [0i32; 8192];
        loop {
            let n = dec.decode_into(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        // Reference is interleaved s16le; ALAC is lossless -> our i32 samples must
        // equal the 16-bit reference exactly.
        assert_eq!(out.len(), REF.len() / 2);
        for (i, chunk) in REF.chunks_exact(2).enumerate() {
            let expect = i16::from_le_bytes([chunk[0], chunk[1]]) as i32;
            assert_eq!(out[i], expect, "sample {i}");
        }
    }

    #[test]
    fn decodes_24bit_bit_exact_vs_reference() {
        let mut dec = AlacDecoder::new(SliceSource::new(ALAC24)).expect("open alac24");
        let si = dec.stream_info();
        assert_eq!(si.sample_rate, 44_100);
        assert_eq!(si.channels, 2);
        assert_eq!(si.format, SampleFormat::Int { bits: 24 });

        // Decode all samples.
        let mut out = alloc::vec::Vec::new();
        let mut buf = [0i32; 8192];
        loop {
            let n = dec.decode_into(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        // Reference is interleaved s24le (3 bytes/sample, little-endian, signed);
        // sign-extend 24 -> 32. ALAC is lossless and our decoder emits native
        // 24-bit right-justified i32, so the samples must match exactly.
        assert_eq!(out.len(), REF24.len() / 3);
        for (i, chunk) in REF24.chunks_exact(3).enumerate() {
            let sign = if chunk[2] & 0x80 != 0 { 0xFF } else { 0 };
            let expect = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], sign]);
            assert_eq!(out[i], expect, "sample {i}");
        }
    }
}

/// Regression tests for reviewer-reproduced panics / OOB writes on malformed
/// `mdat` input against an otherwise-valid cookie. Each test hand-crafts a
/// minimal ALAC packet with a [`BitWriter`] (the write-side mirror of
/// [`BitReader`]'s bit order) and splices it over the golden fixture's `mdat`
/// payload, then asserts `decode_into` returns `Err`, never panics.
#[cfg(test)]
mod malformed {
    extern crate alloc;

    use super::*;
    use crate::decoder::Decoder;
    use crate::io::SliceSource;
    use alloc::vec::Vec;

    const ALAC: &[u8] = include_bytes!("../../testdata/alac.m4a");

    /// MSB-first bit writer, mirroring [`BitReader`]'s bit order.
    struct BitWriter {
        bytes: Vec<u8>,
        bit_pos: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                bit_pos: 0,
            }
        }

        /// Append the low `n` bits of `v`, MSB-first.
        fn push(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                let bit = (v >> i) & 1;
                let byte_idx = self.bit_pos / 8;
                while self.bytes.len() <= byte_idx {
                    self.bytes.push(0);
                }
                if bit != 0 {
                    self.bytes[byte_idx] |= 1 << (7 - (self.bit_pos % 8));
                }
                self.bit_pos += 1;
            }
        }
    }

    /// Locate the top-level `mdat` box in a (faststart) MP4 buffer. Returns
    /// `(content_offset, content_len)` — the byte range *after* the 8-byte box
    /// header. Walks top-level boxes generically (no hardcoded offset) so it
    /// stays valid if the fixture is regenerated.
    fn find_mdat(data: &[u8]) -> (usize, usize) {
        let mut off = 0usize;
        while off + 8 <= data.len() {
            let size = u32::from_be_bytes(data[off..off + 4].try_into().unwrap()) as usize;
            let fourcc: [u8; 4] = data[off + 4..off + 8].try_into().unwrap();
            if &fourcc == b"mdat" {
                return (off + 8, size - 8);
            }
            assert!(size >= 8, "unsupported/zero box size scanning for mdat");
            off += size;
        }
        panic!("mdat box not found in golden fixture");
    }

    /// Zero the entire `mdat` payload, then splice `packet` at its start.
    /// Bytes past the crafted packet then read back as zero — matching how
    /// [`BitReader`] zero-pads reads past the true end of a truncated packet
    /// — so the crafted packets below don't need to hand-encode bits they
    /// intend to leave unspecified (i.e. anything past the point the decoder
    /// is expected to fail, or a Golomb code's tail that should decode to a
    /// deterministic all-zero residual).
    fn splice_first_packet(packet: &[u8]) -> Vec<u8> {
        let mut data = ALAC.to_vec();
        let (content_off, content_len) = find_mdat(&data);
        assert!(packet.len() <= content_len, "crafted packet overflows mdat");
        data[content_off..content_off + content_len].fill(0);
        data[content_off..content_off + packet.len()].copy_from_slice(packet);
        data
    }

    /// CRITICAL: a packet with two elements — SCE then CPE — pushes
    /// `channel_index` from 0 to 1 to 3 on a 2-channel (stereo) stream. Before
    /// the fix, `decode_channel_element` writes the CPE unconditionally at
    /// `s.out[i*stride + channel_index (+1)]` with no bound check against
    /// `cfg.channels`, so a full-length (`MAX_FRAME` = 4096-sample) second
    /// element walks `s.out` (len `MAX_FRAME * MAX_CHANNELS` = 8192) out of
    /// bounds on its last sample: `s.out[4095*2 + 2] == s.out[8192]`.
    #[test]
    fn two_elements_sce_then_cpe_rejected_not_oob() {
        let mut w = BitWriter::new();
        // Element 1: SCE, verbatim (escape), 1 sample — minimal.
        w.push(0b000, 3); // tag: SCE
        w.push(0, 4); // instance
        w.push(0, 12); // must be zero
        w.push(0b1001, 4); // header: partial=1, bytes_shifted=0, escape=1
        w.push(0, 16); // num_samples hi
        w.push(1, 16); // num_samples lo -> num = 1
        w.push(0, 16); // one verbatim 16-bit sample

        // Element 2: CPE, verbatim, num overridden to MAX_FRAME (4096) — the
        // size that walks the OOB write on unfixed code.
        w.push(0b001, 3); // tag: CPE
        w.push(0, 4); // instance
        w.push(0, 12);
        w.push(0b1001, 4); // header: partial=1, bytes_shifted=0, escape=1
        w.push(0, 16);
        w.push(4096, 16); // num_samples lo -> num = 4096 = MAX_FRAME
        // Verbatim payload left unspecified (reads back as zero).

        let data = splice_first_packet(&w.bytes);
        let mut dec = AlacDecoder::new(SliceSource::new(&data)).expect("open alac");
        let mut buf = [0i32; 8192];
        assert_eq!(dec.decode_into(&mut buf), Err(DecodeError::Malformed));
    }

    /// IMPORTANT: `mix_bits = r.read(8)` (0..255) is used unchecked as
    /// `>> mix_bits` in the stereo un-mix. `mix_bits == 32` is a shift-amount
    /// overflow. A compressed (non-escape) CPE with `mix_res != 0` (so the
    /// shift is actually reached) and `mix_bits = 32` must be rejected.
    #[test]
    fn compressed_cpe_mix_bits_32_rejected_not_overflow() {
        let mut w = BitWriter::new();
        w.push(0b001, 3); // tag: CPE
        w.push(0, 4); // instance
        w.push(0, 12);
        w.push(0b1000, 4); // header: partial=1, bytes_shifted=0, escape=0 (compressed)
        w.push(0, 16);
        w.push(1, 16); // num_samples lo -> num = 1
        w.push(32, 8); // mix_bits = 32 (out of range)
        w.push(1, 8); // mix_res = 1 (nonzero, so the un-mix shift actually runs)
        // Per-channel headers (mode/denshift, pbfactor/numactive): all zero ->
        // numactive = 0 (no adaptive-FIR coefficients to encode) for both
        // sub-frames.
        w.push(0, 8);
        w.push(0, 8);
        w.push(0, 8);
        w.push(0, 8);
        // Residuals read back as zero: with mb=10, kb=14 this Golomb-decodes
        // to a single all-zero sample per sub-frame, deterministically.

        let data = splice_first_packet(&w.bytes);
        let mut dec = AlacDecoder::new(SliceSource::new(&data)).expect("open alac");
        let mut buf = [0i32; 8192];
        assert_eq!(dec.decode_into(&mut buf), Err(DecodeError::Malformed));
    }

    /// IMPORTANT: `chan_bits = bit_depth - bytes_shifted*8`; only
    /// `bytes_shifted == 3` was rejected, so a 16-bit SCE with
    /// `bytes_shifted = 2` gives `chan_bits = 0` -> `chanshift = 32 - 0 = 32`
    /// -> `del << 32` panics (in `dc31_inplace`, reached via a `mode != 0`
    /// sub-frame).
    #[test]
    fn compressed_sce_zero_chan_bits_rejected_not_shift_overflow() {
        let mut w = BitWriter::new();
        w.push(0b000, 3); // tag: SCE
        w.push(0, 4); // instance
        w.push(0, 12);
        w.push(0b1100, 4); // header: partial=1, bytes_shifted=2, escape=0
        w.push(0, 16);
        w.push(2, 16); // num_samples lo -> num = 2 (dc31_inplace's loop needs >= 2)
        w.push(0, 8); // mix_bits (unused for elem_ch == 1)
        w.push(0, 8); // mix_res
        w.push(0b0001_0000, 8); // mode = 1 (-> dc31_inplace), denshift = 0
        w.push(0, 8); // pbfactor = 0, numactive = 0
        // Wasted-bits region (bytes_shifted=2, elem_ch=1, num=2 -> 32 bits)
        // and residuals read back as zero.

        let data = splice_first_packet(&w.bytes);
        let mut dec = AlacDecoder::new(SliceSource::new(&data)).expect("open alac");
        let mut buf = [0i32; 8192];
        assert_eq!(dec.decode_into(&mut buf), Err(DecodeError::Malformed));
    }

    /// Find a direct child box named `want` within `[start, end)` (mirrors
    /// `mp4.rs`'s test helper of the same shape).
    fn find_box(data: &[u8], start: usize, end: usize, want: &[u8; 4]) -> (usize, usize) {
        let mut i = start;
        while i + 8 <= end {
            let size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            let fourcc: [u8; 4] = data[i + 4..i + 8].try_into().unwrap();
            if &fourcc == want {
                return (i, size);
            }
            assert!(size >= 8, "unsupported/zero box size scanning for {want:?}");
            i += size;
        }
        panic!(
            "box {:?} not found in [{start},{end})",
            core::str::from_utf8(want)
        );
    }

    /// Locate the 24-byte ALAC magic cookie inside the golden fixture's
    /// `moov/trak/mdia/minf/stbl/stsd` chain, by walking box headers (no
    /// hardcoded offset). Mirrors `demux::mp4::parse_stsd`'s own traversal:
    /// `stsd`'s 8-byte version+entry_count, then the `alac` sample entry's
    /// 8-byte box header + 28-byte `AudioSampleEntry` body + 12-byte inner
    /// `alac` `FullBox` header precede the cookie.
    fn find_alac_cookie_offset(data: &[u8]) -> usize {
        let (moov_off, moov_size) = find_box(data, 0, data.len(), b"moov");
        let (moov_content, moov_end) = (moov_off + 8, moov_off + moov_size);
        let (trak_off, trak_size) = find_box(data, moov_content, moov_end, b"trak");
        let (trak_content, trak_end) = (trak_off + 8, trak_off + trak_size);
        let (mdia_off, mdia_size) = find_box(data, trak_content, trak_end, b"mdia");
        let (mdia_content, mdia_end) = (mdia_off + 8, mdia_off + mdia_size);
        let (minf_off, minf_size) = find_box(data, mdia_content, mdia_end, b"minf");
        let (minf_content, minf_end) = (minf_off + 8, minf_off + minf_size);
        let (stbl_off, stbl_size) = find_box(data, minf_content, minf_end, b"stbl");
        let (stbl_content, stbl_end) = (stbl_off + 8, stbl_off + stbl_size);
        let (stsd_off, _stsd_size) = find_box(data, stbl_content, stbl_end, b"stsd");
        let entry_off = stsd_off + 8 + 8; // stsd hdr + version/flags/entry_count
        entry_off + 8 + 28 + 12 // alac entry hdr + AudioSampleEntry body + inner FullBox hdr
    }

    /// IMPORTANT: `AlacDecoder::new` didn't validate `kb` from the cookie.
    /// `kb == 0` drives `1u32 << kb` / a `>> (32 - kb)` shift-amount overflow
    /// deep in the adaptive-Golomb decode. Must be rejected at construction.
    #[test]
    fn cookie_kb_zero_rejected_at_open() {
        let mut data = ALAC.to_vec();
        let cookie_off = find_alac_cookie_offset(&data);
        assert_eq!(data[cookie_off + 8], 14, "kb offset sanity check");
        data[cookie_off + 8] = 0; // kb = 0
        assert_eq!(
            AlacDecoder::new(SliceSource::new(&data)).err(),
            Some(DecodeError::Unsupported)
        );
    }

    /// IMPORTANT: `AlacDecoder::new` didn't validate `bit_depth` from the
    /// cookie. 32-bit stereo is non-functional in this decoder (`chan_bits =
    /// 32 + 1 = 33` underflows `32 - chan_bits`) and out of v1 scope — must
    /// be rejected at construction, not left to corrupt or panic mid-decode.
    #[test]
    fn cookie_bit_depth_32_rejected_at_open() {
        let mut data = ALAC.to_vec();
        let cookie_off = find_alac_cookie_offset(&data);
        assert_eq!(data[cookie_off + 5], 16, "bit_depth offset sanity check");
        data[cookie_off + 5] = 32; // bit_depth = 32
        assert_eq!(
            AlacDecoder::new(SliceSource::new(&data)).err(),
            Some(DecodeError::Unsupported)
        );
    }
}

#[cfg(test)]
mod seek_tests {
    use super::*;
    use crate::io::SliceSource;

    const ALAC: &[u8] = include_bytes!("../../testdata/alac.m4a");
    const CH: usize = 2;
    const TOTAL: u64 = 13_230;

    fn decode_all<S: Source>(dec: &mut AlacDecoder<S>) -> Vec<i32> {
        let mut out = Vec::new();
        let mut buf = [0i32; 4096];
        loop {
            let n = dec.decode_into(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    fn full() -> Vec<i32> {
        decode_all(&mut AlacDecoder::new(SliceSource::new(ALAC)).unwrap())
    }

    /// ALAC is lossless + frame-independent ⇒ seeking is exactly bit-identical
    /// to decoding from the start and dropping the head. Covers frame
    /// boundaries, interiors, and the partial final frame.
    #[test]
    fn alac_seek_round_trip_bit_exact() {
        let reference = full();
        assert_eq!(reference.len(), TOTAL as usize * CH);
        for &target in &[0u64, 1, 100, 4095, 4096, 4097, 8192, 12_288, 12_289, 13_229] {
            let mut d = AlacDecoder::new(SliceSource::new(ALAC)).unwrap();
            let landed = d.seek_to(target).unwrap();
            assert_eq!(landed, target, "sample-accurate landing at {target}");
            let rest = decode_all(&mut d);
            let from = target as usize * CH;
            assert_eq!(
                rest.len(),
                reference.len() - from,
                "tail length at {target}"
            );
            assert_eq!(
                rest,
                reference[from..],
                "bit-exact tail after seek to {target}"
            );
        }
    }

    #[test]
    fn alac_seek_to_zero_matches_fresh() {
        let reference = full();
        let mut d = AlacDecoder::new(SliceSource::new(ALAC)).unwrap();
        let mut buf = [0i32; 1000];
        let _ = d.decode_into(&mut buf).unwrap(); // partial decode first
        assert_eq!(d.seek_to(0).unwrap(), 0);
        assert_eq!(decode_all(&mut d), reference, "seek-to-0 == fresh decode");
    }

    #[test]
    fn alac_backward_seek_matches() {
        let reference = full();
        let mut d = AlacDecoder::new(SliceSource::new(ALAC)).unwrap();
        d.seek_to(10_000).unwrap();
        d.seek_to(2_000).unwrap(); // backward
        let rest = decode_all(&mut d);
        assert_eq!(rest, reference[2_000 * CH..], "backward seek is exact");
    }

    #[test]
    fn alac_seek_beyond_end_clamps() {
        let mut d = AlacDecoder::new(SliceSource::new(ALAC)).unwrap();
        assert_eq!(d.seek_to(1_000_000).unwrap(), TOTAL, "clamps to length");
        let mut buf = [0i32; 16];
        assert_eq!(d.decode_into(&mut buf).unwrap(), 0, "at EOF after clamp");
    }

    #[test]
    fn alac_seek_to_time_matches_sample() {
        let reference = full();
        let mut d = AlacDecoder::new(SliceSource::new(ALAC)).unwrap();
        // 0.1 s @ 44100 = sample 4410.
        let landed = d.seek_to_time(0.1).unwrap();
        assert_eq!(landed, 4_410);
        assert_eq!(decode_all(&mut d), reference[4_410 * CH..]);
    }
}
