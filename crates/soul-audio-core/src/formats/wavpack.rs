//! From-scratch WavPack (`.wv`) **lossless** decoder (dbry/WavPack format).
//!
//! Scope (v1): pure lossless, mono + stereo, 8/16/24-bit integer PCM, joint
//! stereo, and the full decorrelation term set (including the negative
//! cross-channel terms). Hybrid (lossy), float, DSD, >2-channel block
//! stitching, and the `wvc`/`wvx` correction/extension streams are out of scope
//! and decline as [`DecodeError::Unsupported`]. Seeking rewinds and
//! decode-discards forward (a block-index scan would make it O(log n) — a
//! follow-up); `total_frames` comes from the stream header.
//!
//! A WavPack file is a sequence of blocks; each block carries a 32-byte header,
//! a run of metadata sub-blocks (decorrelation terms/weights/samples, entropy
//! medians, …), and one entropy-coded audio bitstream. Decode per block:
//! entropy-decode residuals → run the decorrelation passes → un-mix joint
//! stereo → shift to the final integer resolution. Lossless, so the result is
//! bit-exact to the original PCM. All integer math wraps (two's complement),
//! matching the reference C.

use embedded_io::SeekFrom;

use crate::decoder::{DecodeError, Decoder};
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

// ---- flags (WavpackHeader.flags) ------------------------------------------
const FLAG_BYTES_STORED: u32 = 0x0000_0003;
const FLAG_MONO: u32 = 0x0000_0004;
const FLAG_HYBRID: u32 = 0x0000_0008;
const FLAG_JOINT_STEREO: u32 = 0x0000_0010;
const FLAG_FLOAT_DATA: u32 = 0x0000_0080;
const FLAG_INT32_DATA: u32 = 0x0000_0100;
const FLAG_SHIFT_MASK: u32 = 0x001F_E000;
const FLAG_SHIFT_LSB: u32 = 13;
const FLAG_SRATE_MASK: u32 = 0x0780_0000;
const FLAG_SRATE_LSB: u32 = 23;
const FLAG_FALSE_STEREO: u32 = 0x4000_0000;
const FLAG_DSD: u32 = 0x8000_0000;

/// "treat the block as a single channel of samples".
const FLAG_MONO_DATA: u32 = FLAG_MONO | FLAG_FALSE_STEREO;

// ---- metadata sub-block IDs -----------------------------------------------
const ID_LARGE: u8 = 0x80;
const ID_ODD_SIZE: u8 = 0x40;
const ID_OPTIONAL: u8 = 0x20;
const ID_DECORR_TERMS: u8 = 0x02;
const ID_DECORR_WEIGHTS: u8 = 0x03;
const ID_DECORR_SAMPLES: u8 = 0x04;
const ID_ENTROPY_VARS: u8 = 0x05;
const ID_INT32_INFO: u8 = 0x09;
const ID_WV_BITSTREAM: u8 = 0x0a;

const MAX_TERMS: usize = 16;
/// Largest decoded block we buffer: `block_samples × channels` interleaved i32.
/// Covers the common ~0.5–1 s blocks encoders emit (ffmpeg uses 22050 stereo /
/// 44100 mono); a larger block declines as `Unsupported`.
const MAX_BLOCK_PCM: usize = 65536;

const SAMPLE_RATES: [u32; 15] = [
    6000, 8000, 9600, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000,
    192_000,
];

/// 256-entry fractional-mantissa table for [`wp_exp2s`] (`entropy_utils.c`).
#[rustfmt::skip]
const EXP2_TABLE: [u8; 256] = [
    0x00,0x01,0x01,0x02,0x03,0x03,0x04,0x05,0x06,0x06,0x07,0x08,0x08,0x09,0x0a,0x0b,
    0x0b,0x0c,0x0d,0x0e,0x0e,0x0f,0x10,0x10,0x11,0x12,0x13,0x13,0x14,0x15,0x16,0x16,
    0x17,0x18,0x19,0x19,0x1a,0x1b,0x1c,0x1d,0x1d,0x1e,0x1f,0x20,0x20,0x21,0x22,0x23,
    0x24,0x24,0x25,0x26,0x27,0x28,0x28,0x29,0x2a,0x2b,0x2c,0x2c,0x2d,0x2e,0x2f,0x30,
    0x30,0x31,0x32,0x33,0x34,0x35,0x35,0x36,0x37,0x38,0x39,0x3a,0x3a,0x3b,0x3c,0x3d,
    0x3e,0x3f,0x40,0x41,0x41,0x42,0x43,0x44,0x45,0x46,0x47,0x48,0x48,0x49,0x4a,0x4b,
    0x4c,0x4d,0x4e,0x4f,0x50,0x51,0x51,0x52,0x53,0x54,0x55,0x56,0x57,0x58,0x59,0x5a,
    0x5b,0x5c,0x5d,0x5e,0x5e,0x5f,0x60,0x61,0x62,0x63,0x64,0x65,0x66,0x67,0x68,0x69,
    0x6a,0x6b,0x6c,0x6d,0x6e,0x6f,0x70,0x71,0x72,0x73,0x74,0x75,0x76,0x77,0x78,0x79,
    0x7a,0x7b,0x7c,0x7d,0x7e,0x7f,0x80,0x81,0x82,0x83,0x84,0x85,0x87,0x88,0x89,0x8a,
    0x8b,0x8c,0x8d,0x8e,0x8f,0x90,0x91,0x92,0x93,0x95,0x96,0x97,0x98,0x99,0x9a,0x9b,
    0x9c,0x9d,0x9f,0xa0,0xa1,0xa2,0xa3,0xa4,0xa5,0xa6,0xa8,0xa9,0xaa,0xab,0xac,0xad,
    0xaf,0xb0,0xb1,0xb2,0xb3,0xb4,0xb6,0xb7,0xb8,0xb9,0xba,0xbc,0xbd,0xbe,0xbf,0xc0,
    0xc2,0xc3,0xc4,0xc5,0xc6,0xc8,0xc9,0xca,0xcb,0xcd,0xce,0xcf,0xd0,0xd2,0xd3,0xd4,
    0xd6,0xd7,0xd8,0xd9,0xdb,0xdc,0xdd,0xde,0xe0,0xe1,0xe2,0xe4,0xe5,0xe6,0xe8,0xe9,
    0xea,0xec,0xed,0xee,0xf0,0xf1,0xf2,0xf4,0xf5,0xf6,0xf8,0xf9,0xfa,0xfc,0xfd,0xff,
];

/// Signed-log → value (`entropy_utils.c:wp_exp2s`).
fn wp_exp2s(log: i32) -> i32 {
    if log < 0 {
        return -wp_exp2s(-log);
    }
    let value = u32::from(EXP2_TABLE[(log & 0xff) as usize]) | 0x100;
    let e = log >> 8;
    (if e <= 9 {
        value >> (9 - e)
    } else {
        value << ((e - 9) & 0x1f)
    }) as i32
}

/// Restore a decorrelation weight from its stored signed byte
/// (`entropy_utils.c:restore_weight`).
fn restore_weight(w: i8) -> i32 {
    let mut r = i32::from(w) << 3;
    if r > 0 {
        r += (r + 64) >> 7;
    }
    r
}

/// `count_bits(av)` = `floor(log2(av)) + 1`, 0 for 0 (`entropy_utils.c`).
fn count_bits(av: u32) -> u32 {
    if av == 0 { 0 } else { 32 - av.leading_zeros() }
}

/// Little-endian, LSB-first bit reader over a bounded region of a [`Source`].
/// Bits come out bit-0-first within each byte, bytes in file order — the
/// byte-wise equivalent of WavPack's 16-bit-short reader (the region length is
/// always even). Past the region it yields 0 and flags EOF.
struct WvBits<'a, S> {
    src: &'a mut S,
    remaining: usize,
    cur: u8,
    bits_left: u8,
    eof: bool,
}

impl<'a, S: Source> WvBits<'a, S> {
    fn new(src: &'a mut S, byte_length: usize) -> Self {
        Self {
            src,
            remaining: byte_length,
            cur: 0,
            bits_left: 0,
            eof: false,
        }
    }

    fn getbit(&mut self) -> u32 {
        if self.bits_left == 0 {
            if self.remaining == 0 {
                self.eof = true;
                return 0;
            }
            let mut b = [0u8; 1];
            match self.src.read(&mut b) {
                Ok(1) => {}
                _ => {
                    self.eof = true;
                    return 0;
                }
            }
            self.cur = b[0];
            self.bits_left = 8;
            self.remaining -= 1;
        }
        let bit = u32::from(self.cur & 1);
        self.cur >>= 1;
        self.bits_left -= 1;
        bit
    }

    fn getbits(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for i in 0..n {
            v |= self.getbit() << i;
        }
        v
    }

    /// Consume the rest of the region (so the caller can continue past it).
    fn drain(&mut self) -> Result<(), DecodeError> {
        let mut buf = [0u8; 256];
        while self.remaining > 0 {
            let want = self.remaining.min(buf.len());
            read_exact(self.src, &mut buf[..want])?;
            self.remaining -= want;
        }
        Ok(())
    }
}

/// Phase-in binary code in `[0, maxcode]` (`read_words.c:read_code`).
fn read_code<S: Source>(bs: &mut WvBits<S>, maxcode: u32) -> u32 {
    if maxcode < 2 {
        return if maxcode != 0 { bs.getbit() } else { 0 };
    }
    let bitcount = count_bits(maxcode);
    // `1 << bitcount` is computed in `u64` so a crafted `maxcode` with
    // `bitcount == 32` can't overflow the shift; the rest wraps like the C.
    let extras = ((1u64 << bitcount) as u32)
        .wrapping_sub(maxcode)
        .wrapping_sub(1);
    let mut code = bs.getbits(bitcount - 1);
    if code >= extras {
        code = (code << 1).wrapping_sub(extras).wrapping_add(bs.getbit());
    }
    code
}

// ---- entropy decoder state ------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Chan {
    median: [u32; 3],
}

// The reference decoder's median registers are `uint32_t`; a malformed
// bitstream can drive them (and the `low`/`high` span derived from them) to the
// top of the range, where the C silently wraps. These helpers wrap to match —
// valid streams keep the medians small (sample magnitudes), so the wrap never
// triggers there; it only turns a crafted-input debug overflow panic into the
// same bounded garbage the C produces (which then decodes to `Err`/junk, not a
// crash — the robustness-sweep invariant).
impl Chan {
    fn get_med(&self, k: usize) -> u32 {
        (self.median[k] >> 4).wrapping_add(1)
    }
    fn inc_med(&mut self, k: usize) {
        let d = [128u32, 64, 32][k];
        let step = (self.median[k].wrapping_add(d) / d).wrapping_mul(5);
        self.median[k] = self.median[k].wrapping_add(step);
    }
    fn dec_med(&mut self, k: usize) {
        let (d, sub) = [(128u32, 126u32), (64, 62), (32, 30)][k];
        let step = (self.median[k].wrapping_add(sub) / d).wrapping_mul(2);
        self.median[k] = self.median[k].wrapping_sub(step);
    }
}

#[derive(Default)]
struct Words {
    c: [Chan; 2],
    holding_zero: bool,
    holding_one: u32,
    zeros_acc: u32,
}

const LIMIT_ONES: u32 = 16;

/// Read `nsamp` interleaved residuals into `buffer` (`read_words.c:
/// get_words_lossless`). `stereo` selects the per-channel median set by the
/// interleave parity. Faithful port of the reference C; see the module tests
/// and the e2e golden for the bit-exactness proof.
fn get_words_lossless<S: Source>(
    bs: &mut WvBits<S>,
    buffer: &mut [i32],
    w: &mut Words,
    stereo: bool,
) {
    let total = buffer.len();
    let mut cs = 0usize;
    while cs < total {
        let mut ci = if stereo { cs & 1 } else { 0 };

        // Deferred `holding_zero` sample (the `ones_count == 0` case inlined):
        // decode it, advance, and fall through to decode a fresh sample.
        if w.holding_zero {
            w.holding_zero = false;
            let maxc = w.c[ci].get_med(0) - 1;
            let low = read_code(bs, maxc);
            w.c[ci].dec_med(0);
            let sign = bs.getbit();
            buffer[cs] = if sign != 0 { !(low as i32) } else { low as i32 };
            cs += 1;
            if cs >= total {
                break;
            }
            ci = if stereo { cs & 1 } else { 0 };
        }

        // Zero-run: both channels quiet and not mid-pair.
        if w.c[0].median[0] < 2 && w.holding_one == 0 && w.c[1].median[0] < 2 {
            let mut fell_through = false;
            if w.zeros_acc > 0 {
                w.zeros_acc -= 1;
                if w.zeros_acc > 0 {
                    buffer[cs] = 0;
                    cs += 1;
                    continue;
                }
                fell_through = true;
            }
            if !fell_through {
                let mut cbits = 0u32;
                while cbits < 33 && bs.getbit() != 0 {
                    cbits += 1;
                }
                if cbits == 33 {
                    break;
                }
                if cbits < 2 {
                    w.zeros_acc = cbits;
                } else {
                    let mut mask = 1u32;
                    w.zeros_acc = 0;
                    for _ in 0..cbits - 1 {
                        if bs.getbit() != 0 {
                            w.zeros_acc |= mask;
                        }
                        mask <<= 1;
                    }
                    w.zeros_acc |= mask;
                }
                if w.zeros_acc > 0 {
                    w.c[0].median = [0; 3];
                    w.c[1].median = [0; 3];
                    buffer[cs] = 0;
                    cs += 1;
                    continue;
                }
            }
        }

        // `ones_count` unary run with the LIMIT_ONES escape.
        let mut ones_count = 0u32;
        while ones_count < LIMIT_ONES + 1 && bs.getbit() != 0 {
            ones_count += 1;
        }
        if ones_count >= LIMIT_ONES {
            if ones_count == LIMIT_ONES + 1 {
                break;
            }
            let mut cbits = 0u32;
            while cbits < 33 && bs.getbit() != 0 {
                cbits += 1;
            }
            if cbits == 33 {
                break;
            }
            let base = if cbits < 2 {
                cbits
            } else {
                let mut mask = 1u32;
                let mut v = 0u32;
                for _ in 0..cbits - 1 {
                    if bs.getbit() != 0 {
                        v |= mask;
                    }
                    mask <<= 1;
                }
                v | mask
            };
            ones_count = base + LIMIT_ONES;
        }

        // holding_one / holding_zero fold: one unary run encodes two samples.
        let low_bit = w.holding_one;
        w.holding_one = ones_count & 1;
        w.holding_zero = (!ones_count & 1) != 0;
        ones_count = (ones_count >> 1) + low_bit;

        // Map ones_count → [low, high] via the three medians of channel `ci`.
        // Wrapping throughout: on malformed input the medians (and this span)
        // can reach the top of `u32`, where the reference C wraps; valid streams
        // keep these small, so the wrap only tames crafted-input overflow.
        let c = &mut w.c[ci];
        let (mut low, high);
        if ones_count == 0 {
            low = 0;
            high = c.get_med(0).wrapping_sub(1);
            c.dec_med(0);
        } else {
            low = c.get_med(0);
            c.inc_med(0);
            if ones_count == 1 {
                high = low.wrapping_add(c.get_med(1)).wrapping_sub(1);
                c.dec_med(1);
            } else {
                low = low.wrapping_add(c.get_med(1));
                c.inc_med(1);
                if ones_count == 2 {
                    high = low.wrapping_add(c.get_med(2)).wrapping_sub(1);
                    c.dec_med(2);
                } else {
                    low = low.wrapping_add(ones_count.wrapping_sub(2).wrapping_mul(c.get_med(2)));
                    high = low.wrapping_add(c.get_med(2)).wrapping_sub(1);
                    c.inc_med(2);
                }
            }
        }

        low = low.wrapping_add(read_code(bs, high.wrapping_sub(low)));
        let sign = bs.getbit();
        buffer[cs] = if sign != 0 { !(low as i32) } else { low as i32 };
        cs += 1;
    }
    // Any remaining samples (shouldn't happen for valid data) stay zero.
    for s in buffer[cs..].iter_mut() {
        *s = 0;
    }
}

/// Apply a decorrelation weight: `(weight·sample + 512) >> 10`
/// (`wavpack_local.h:apply_weight`), 64-bit-exact with wrapping.
fn apply_weight(weight: i32, sample: i32) -> i32 {
    ((i64::from(weight) * i64::from(sample) + 512) >> 10) as i32
}

/// Term-17 linear extrapolation `2·s0 − s1`, and term-18 `(3·s0 − s1) >> 1`.
/// The reference's predictions are `int32_t` and wrap; a malformed stream can
/// feed huge history samples, so these wrap to match (never a debug overflow).
fn predict17(s0: i32, s1: i32) -> i32 {
    s0.wrapping_mul(2).wrapping_sub(s1)
}
fn predict18(s0: i32, s1: i32) -> i32 {
    s0.wrapping_mul(3).wrapping_sub(s1) >> 1
}

/// Positive-term weight update (`wavpack_local.h:update_weight`).
fn update_weight(weight: &mut i32, delta: i32, source: i32, result: i32) {
    if source != 0 && result != 0 {
        let s = (source ^ result) >> 31;
        *weight = (delta ^ s).wrapping_add(weight.wrapping_sub(s));
    }
}

/// Negative/cross-term weight update with ±1024 clamp
/// (`wavpack_local.h:update_weight_clip`).
fn update_weight_clip(weight: &mut i32, delta: i32, source: i32, result: i32) {
    if source != 0 && result != 0 {
        let s = (source ^ result) >> 31;
        let mut ww = (*weight ^ s).wrapping_add(delta - s);
        if ww > 1024 {
            ww = 1024;
        }
        *weight = (ww ^ s).wrapping_sub(s);
    }
}

/// One decorrelation pass over the whole block buffer.
struct DecorrPass {
    term: i32,
    delta: i32,
    weight_a: i32,
    weight_b: i32,
    samples_a: [i32; 8],
    samples_b: [i32; 8],
}

impl DecorrPass {
    fn run_mono(&mut self, buf: &mut [i32]) {
        let (mut a0, mut a1) = (self.samples_a[0], self.samples_a[1]);
        match self.term {
            17 | 18 => {
                for x in buf.iter_mut() {
                    let sam = if self.term == 17 {
                        predict17(a0, a1)
                    } else {
                        predict18(a0, a1)
                    };
                    a1 = a0;
                    let out = apply_weight(self.weight_a, sam).wrapping_add(*x);
                    update_weight(&mut self.weight_a, self.delta, sam, *x);
                    a0 = out;
                    *x = out;
                }
                self.samples_a[0] = a0;
                self.samples_a[1] = a1;
            }
            t if (1..=8).contains(&t) => {
                let tt = t as usize;
                let mut hist = self.samples_a;
                let mut m = 0usize;
                let mut k = tt & 7;
                for x in buf.iter_mut() {
                    let sam = hist[m];
                    let out = apply_weight(self.weight_a, sam).wrapping_add(*x);
                    update_weight(&mut self.weight_a, self.delta, sam, *x);
                    hist[k] = out;
                    *x = out;
                    m = (m + 1) & 7;
                    k = (k + 1) & 7;
                }
                self.samples_a = hist;
            }
            _ => {}
        }
    }

    fn run_stereo(&mut self, buf: &mut [i32]) {
        let n = buf.len() / 2;
        match self.term {
            17 | 18 => {
                let (mut a0, mut a1) = (self.samples_a[0], self.samples_a[1]);
                let (mut b0, mut b1) = (self.samples_b[0], self.samples_b[1]);
                for i in 0..n {
                    let sl = if self.term == 17 {
                        predict17(a0, a1)
                    } else {
                        predict18(a0, a1)
                    };
                    a1 = a0;
                    let ol = apply_weight(self.weight_a, sl).wrapping_add(buf[2 * i]);
                    update_weight(&mut self.weight_a, self.delta, sl, buf[2 * i]);
                    a0 = ol;
                    buf[2 * i] = ol;

                    let sr = if self.term == 17 {
                        predict17(b0, b1)
                    } else {
                        predict18(b0, b1)
                    };
                    b1 = b0;
                    let or_ = apply_weight(self.weight_b, sr).wrapping_add(buf[2 * i + 1]);
                    update_weight(&mut self.weight_b, self.delta, sr, buf[2 * i + 1]);
                    b0 = or_;
                    buf[2 * i + 1] = or_;
                }
                self.samples_a[0] = a0;
                self.samples_a[1] = a1;
                self.samples_b[0] = b0;
                self.samples_b[1] = b1;
            }
            t if (1..=8).contains(&t) => {
                let tt = t as usize;
                let mut ha = self.samples_a;
                let mut hb = self.samples_b;
                let mut m = 0usize;
                let mut k = tt & 7;
                for i in 0..n {
                    let sl = ha[m];
                    let ol = apply_weight(self.weight_a, sl).wrapping_add(buf[2 * i]);
                    update_weight(&mut self.weight_a, self.delta, sl, buf[2 * i]);
                    ha[k] = ol;
                    buf[2 * i] = ol;

                    let sr = hb[m];
                    let or_ = apply_weight(self.weight_b, sr).wrapping_add(buf[2 * i + 1]);
                    update_weight(&mut self.weight_b, self.delta, sr, buf[2 * i + 1]);
                    hb[k] = or_;
                    buf[2 * i + 1] = or_;

                    m = (m + 1) & 7;
                    k = (k + 1) & 7;
                }
                self.samples_a = ha;
                self.samples_b = hb;
            }
            -1 => {
                for i in 0..n {
                    let sam =
                        buf[2 * i].wrapping_add(apply_weight(self.weight_a, self.samples_a[0]));
                    update_weight_clip(
                        &mut self.weight_a,
                        self.delta,
                        self.samples_a[0],
                        buf[2 * i],
                    );
                    buf[2 * i] = sam;
                    let a = buf[2 * i + 1].wrapping_add(apply_weight(self.weight_b, sam));
                    update_weight_clip(&mut self.weight_b, self.delta, sam, buf[2 * i + 1]);
                    buf[2 * i + 1] = a;
                    self.samples_a[0] = a;
                }
            }
            -2 => {
                for i in 0..n {
                    let sam =
                        buf[2 * i + 1].wrapping_add(apply_weight(self.weight_b, self.samples_b[0]));
                    update_weight_clip(
                        &mut self.weight_b,
                        self.delta,
                        self.samples_b[0],
                        buf[2 * i + 1],
                    );
                    buf[2 * i + 1] = sam;
                    let b = buf[2 * i].wrapping_add(apply_weight(self.weight_a, sam));
                    update_weight_clip(&mut self.weight_a, self.delta, sam, buf[2 * i]);
                    buf[2 * i] = b;
                    self.samples_b[0] = b;
                }
            }
            -3 => {
                for i in 0..n {
                    let sam_a =
                        buf[2 * i].wrapping_add(apply_weight(self.weight_a, self.samples_a[0]));
                    update_weight_clip(
                        &mut self.weight_a,
                        self.delta,
                        self.samples_a[0],
                        buf[2 * i],
                    );
                    let sam_b =
                        buf[2 * i + 1].wrapping_add(apply_weight(self.weight_b, self.samples_b[0]));
                    update_weight_clip(
                        &mut self.weight_b,
                        self.delta,
                        self.samples_b[0],
                        buf[2 * i + 1],
                    );
                    self.samples_b[0] = sam_a;
                    self.samples_a[0] = sam_b;
                    buf[2 * i] = sam_a;
                    buf[2 * i + 1] = sam_b;
                }
            }
            _ => {}
        }
    }
}

/// Read exactly `buf.len()` bytes or fail.
fn read_exact<S: Source>(src: &mut S, buf: &mut [u8]) -> Result<(), DecodeError> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => return Err(DecodeError::UnexpectedEof),
            Ok(k) => n += k,
            Err(_) => return Err(DecodeError::Io),
        }
    }
    Ok(())
}

/// Try to read exactly `buf.len()` bytes; `Ok(false)` on a clean EOF at the
/// start (no bytes), `Ok(true)` on success.
fn read_full_or_eof<S: Source>(src: &mut S, buf: &mut [u8]) -> Result<bool, DecodeError> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => {
                if n == 0 {
                    return Ok(false);
                }
                return Err(DecodeError::UnexpectedEof);
            }
            Ok(k) => n += k,
            Err(_) => return Err(DecodeError::Io),
        }
    }
    Ok(true)
}

fn skip<S: Source>(src: &mut S, mut n: u64) -> Result<(), DecodeError> {
    let mut buf = [0u8; 256];
    while n > 0 {
        let want = n.min(buf.len() as u64) as usize;
        read_exact(src, &mut buf[..want])?;
        n -= want as u64;
    }
    Ok(())
}

/// WavPack lossless decoder over a byte [`Source`].
pub struct WavpackDecoder<S> {
    src: S,
    info: StreamInfo,
    total_frames: Option<u64>,
    channels: usize,
    shift: u32,
    pcm: [i32; MAX_BLOCK_PCM],
    pcm_len: usize,
    pcm_pos: usize,
    eof: bool,
}

impl<S: Source> WavpackDecoder<S> {
    /// Parse the first block's header for stream parameters and decode it.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a non-WavPack / bad stream;
    /// [`DecodeError::Unsupported`] for hybrid/float/DSD/>2-ch/oversized blocks.
    pub fn new(src: S) -> Result<Self, DecodeError> {
        let mut d = Self {
            src,
            info: StreamInfo {
                sample_rate: 0,
                channels: 0,
                format: SampleFormat::Int { bits: 16 },
            },
            total_frames: None,
            channels: 0,
            shift: 0,
            pcm: [0; MAX_BLOCK_PCM],
            pcm_len: 0,
            pcm_pos: 0,
            eof: false,
        };
        // Decode the first audio block to establish stream parameters.
        if !d.decode_block()? {
            return Err(DecodeError::Malformed);
        }
        Ok(d)
    }

    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    /// Decode one block into [`Self::pcm`]. Returns `false` at a clean EOF.
    fn decode_block(&mut self) -> Result<bool, DecodeError> {
        loop {
            let mut hdr = [0u8; 32];
            if !read_full_or_eof(&mut self.src, &mut hdr)? {
                self.eof = true;
                return Ok(false);
            }
            if &hdr[0..4] != b"wvpk" {
                // Trailing non-audio data (e.g. an APEv2 / ID3 tag ffmpeg appends)
                // after the last block is a clean end of stream, not corruption —
                // but junk before the very first block is malformed.
                if self.channels != 0 {
                    self.eof = true;
                    return Ok(false);
                }
                return Err(DecodeError::Malformed);
            }
            let ck_size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
            if ck_size < 24 {
                return Err(DecodeError::Malformed);
            }
            let version = u16::from_le_bytes([hdr[8], hdr[9]]);
            if hdr[9] != 0x04 || !(0x0402..=0x0410).contains(&version) {
                return Err(DecodeError::Unsupported);
            }
            let total_samples_u8 = hdr[11];
            let total_samples = u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]);
            let block_samples = u32::from_le_bytes([hdr[20], hdr[21], hdr[22], hdr[23]]);
            let flags = u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]);
            let mut body_remaining = u64::from(ck_size) - 24;

            // Metadata-only block (e.g. leading wrapper): skip its body.
            if block_samples == 0 {
                skip(&mut self.src, body_remaining)?;
                continue;
            }

            // Reject out-of-scope stream kinds.
            if flags & (FLAG_HYBRID | FLAG_FLOAT_DATA | FLAG_DSD) != 0
                || (flags & FLAG_MONO_DATA) == FLAG_MONO_DATA
            {
                return Err(DecodeError::Unsupported);
            }
            let mono = flags & FLAG_MONO != 0;
            let false_stereo = flags & FLAG_FALSE_STEREO != 0;
            let decode_mono = mono || false_stereo;
            let out_channels = if mono { 1 } else { 2 };
            let bytes_per_sample = ((flags & FLAG_BYTES_STORED) + 1) as usize;
            let shift = (flags & FLAG_SHIFT_MASK) >> FLAG_SHIFT_LSB;
            let srate_idx = ((flags & FLAG_SRATE_MASK) >> FLAG_SRATE_LSB) as usize;
            let int32 = flags & FLAG_INT32_DATA != 0;
            let joint = flags & FLAG_JOINT_STEREO != 0;

            let n_samples = block_samples as usize * if decode_mono { 1 } else { 2 };
            if n_samples > MAX_BLOCK_PCM {
                return Err(DecodeError::Unsupported);
            }

            // Parse metadata sub-blocks up to (and decoding) the WV bitstream.
            let mut passes: [DecorrPass; MAX_TERMS] = core::array::from_fn(|_| DecorrPass {
                term: 0,
                delta: 0,
                weight_a: 0,
                weight_b: 0,
                samples_a: [0; 8],
                samples_b: [0; 8],
            });
            let mut num_terms = 0usize;
            let mut words = Words::default();
            let mut int32_info = [0u8; 4];
            let mut have_int32 = false;
            let mut decoded = false;

            while body_remaining >= 2 && !decoded {
                let mut idb = [0u8; 2];
                read_exact(&mut self.src, &mut idb)?;
                body_remaining = body_remaining.saturating_sub(2);
                let mut id = idb[0];
                let mut byte_length = u64::from(idb[1]) << 1;
                if id & ID_LARGE != 0 {
                    id &= !ID_LARGE;
                    let mut ext = [0u8; 2];
                    read_exact(&mut self.src, &mut ext)?;
                    body_remaining = body_remaining.saturating_sub(2);
                    byte_length += (u64::from(ext[0]) << 9) + (u64::from(ext[1]) << 17);
                }
                let odd = id & ID_ODD_SIZE != 0;
                let optional = id & ID_OPTIONAL != 0;
                id &= !(ID_ODD_SIZE | ID_OPTIONAL);
                let payload = byte_length; // bytes to read (padded below)
                let pad = byte_length & 1;

                match id {
                    ID_DECORR_TERMS => {
                        let mut t = [0u8; MAX_TERMS];
                        let cnt = (payload as usize).min(MAX_TERMS);
                        read_exact(&mut self.src, &mut t[..cnt])?;
                        skip(&mut self.src, payload - cnt as u64 + pad)?;
                        num_terms = cnt;
                        // Stored in reverse encode order; fill passes[cnt-1..0].
                        for (j, &b) in t[..cnt].iter().enumerate() {
                            let p = &mut passes[cnt - 1 - j];
                            p.term = i32::from(b & 0x1f) - 5;
                            p.delta = i32::from((b >> 5) & 0x07);
                        }
                    }
                    ID_DECORR_WEIGHTS => {
                        let mut raw = [0u8; MAX_TERMS * 2];
                        let cnt = (payload as usize).min(raw.len());
                        read_exact(&mut self.src, &mut raw[..cnt])?;
                        skip(&mut self.src, payload - cnt as u64 + pad)?;
                        let per = if decode_mono { 1 } else { 2 };
                        let weights = cnt / per;
                        let mut idx = 0usize;
                        for j in 0..weights {
                            let p = &mut passes[num_terms - 1 - j];
                            p.weight_a = restore_weight(raw[idx] as i8);
                            idx += 1;
                            if !decode_mono {
                                p.weight_b = restore_weight(raw[idx] as i8);
                                idx += 1;
                            }
                        }
                    }
                    ID_DECORR_SAMPLES => {
                        let mut raw = [0u8; 256];
                        let cnt = (payload as usize).min(raw.len());
                        read_exact(&mut self.src, &mut raw[..cnt])?;
                        skip(&mut self.src, payload - cnt as u64 + pad)?;
                        parse_decorr_samples(&raw[..cnt], &mut passes[..num_terms], decode_mono);
                    }
                    ID_ENTROPY_VARS => {
                        let mut raw = [0u8; 12];
                        let cnt = (payload as usize).min(raw.len());
                        read_exact(&mut self.src, &mut raw[..cnt])?;
                        skip(&mut self.src, payload - cnt as u64 + pad)?;
                        for ch in 0..if decode_mono { 1 } else { 2 } {
                            for k in 0..3 {
                                let o = ch * 6 + k * 2;
                                if o + 1 < cnt {
                                    let log = i32::from(i16::from_le_bytes([raw[o], raw[o + 1]]));
                                    words.c[ch].median[k] = wp_exp2s(log) as u32;
                                }
                            }
                        }
                    }
                    ID_INT32_INFO => {
                        let cnt = (payload as usize).min(4);
                        read_exact(&mut self.src, &mut int32_info[..cnt])?;
                        skip(&mut self.src, payload - cnt as u64 + pad)?;
                        have_int32 = true;
                    }
                    ID_WV_BITSTREAM => {
                        // Decode residuals straight from the stream.
                        let Self { src, pcm, .. } = self;
                        let mut bits = WvBits::new(src, payload as usize);
                        get_words_lossless(
                            &mut bits,
                            &mut pcm[..n_samples],
                            &mut words,
                            !decode_mono,
                        );
                        bits.drain()?;
                        skip(&mut self.src, pad)?;
                        body_remaining = body_remaining.saturating_sub(payload + pad);
                        decoded = true;
                    }
                    _ => {
                        if !optional && id != 0 {
                            // A mandatory-but-unknown sub-block: be lenient and skip
                            // (unknown mandatory blocks are rare in practice).
                        }
                        let _ = odd;
                        skip(&mut self.src, payload + pad)?;
                    }
                }
                if id != ID_WV_BITSTREAM {
                    body_remaining = body_remaining.saturating_sub(payload + pad);
                }
            }

            if !decoded {
                return Err(DecodeError::Malformed);
            }
            // Skip any trailing sub-blocks (e.g. block checksum).
            skip(&mut self.src, body_remaining)?;

            // Decorrelation passes, in array order.
            for p in passes[..num_terms].iter_mut() {
                if decode_mono {
                    p.run_mono(&mut self.pcm[..n_samples]);
                } else {
                    p.run_stereo(&mut self.pcm[..n_samples]);
                }
            }

            // Joint-stereo un-mix (wrapping i32, per the reference — a malformed
            // stream can decorrelate to out-of-range samples that must stay
            // bounded garbage, not a debug overflow panic).
            if joint && !decode_mono {
                for i in 0..n_samples / 2 {
                    let s = self.pcm[2 * i];
                    let d = self.pcm[2 * i + 1];
                    let r = d.wrapping_sub(s >> 1);
                    self.pcm[2 * i + 1] = r;
                    self.pcm[2 * i] = s.wrapping_add(r);
                }
            }

            // int32 redundant-bit restore (simple no-sent-bits case) + shift.
            let mut shift = shift;
            if int32 && have_int32 {
                let sent = int32_info[0];
                let zeros = int32_info[1];
                let ones = int32_info[2];
                let dups = int32_info[3];
                if sent != 0 {
                    return Err(DecodeError::Unsupported); // needs the wvx extension stream
                }
                if zeros | ones | dups != 0 {
                    // Shift amounts come from the (possibly corrupt) int32 info;
                    // `wrapping_shl` masks them to < 32 and the ±1 wraps, so a
                    // crafted value can't panic (the reference's int32 math wraps).
                    let (zeros, ones, dups) = (u32::from(zeros), u32::from(ones), u32::from(dups));
                    for v in self.pcm[..n_samples].iter_mut() {
                        if zeros != 0 {
                            *v = v.wrapping_shl(zeros);
                        } else if ones != 0 {
                            *v = v.wrapping_add(1).wrapping_shl(ones).wrapping_sub(1);
                        } else if dups != 0 {
                            let b = *v & 1;
                            *v = v.wrapping_add(b).wrapping_shl(dups).wrapping_sub(b);
                        }
                    }
                } else {
                    // u32 sum: the four bytes can each be up to 255 on malformed
                    // input, which would overflow a u8 add.
                    shift += u32::from(zeros) + u32::from(sent) + u32::from(ones) + u32::from(dups);
                }
            }
            let shift = shift & 0x1f;
            if shift != 0 {
                for v in self.pcm[..n_samples].iter_mut() {
                    *v = v.wrapping_shl(shift);
                }
            }

            // FALSE_STEREO: duplicate the single decoded channel to two.
            if false_stereo {
                for i in (0..block_samples as usize).rev() {
                    let v = self.pcm[i];
                    self.pcm[2 * i] = v;
                    self.pcm[2 * i + 1] = v;
                }
            }

            // First block: lock stream parameters.
            if self.channels == 0 {
                self.channels = out_channels;
                self.shift = shift;
                let bits = ((bytes_per_sample * 8) as u8).saturating_sub(shift as u8);
                self.info = StreamInfo {
                    sample_rate: SAMPLE_RATES.get(srate_idx).copied().unwrap_or(44_100),
                    channels: out_channels as u16,
                    format: SampleFormat::Int {
                        bits: if int32 { 32 } else { bits },
                    },
                };
                let ts = if total_samples == 0xFFFF_FFFF {
                    None
                } else {
                    Some(u64::from(total_samples) + (u64::from(total_samples_u8) << 32))
                };
                self.total_frames = ts;
            }

            self.pcm_len = block_samples as usize * self.channels;
            self.pcm_pos = 0;
            return Ok(true);
        }
    }
}

impl<S: SeekSource> WavpackDecoder<S> {
    /// Seek to per-channel `sample` by walking block **headers** (no entropy
    /// decode) to the block that contains it, then decoding only that block and
    /// discarding the within-block head. WavPack blocks are independently
    /// decodable (decorrelation state resets per block, its seeds carried in the
    /// block), so the target block decodes stand-alone bit-identically — and each
    /// header carries an exact `block_index` (first per-channel sample), so the
    /// landing stays sample-accurate with no time conversion. This is O(blocks)
    /// 32-byte header reads + one block decode, versus decode-discarding every
    /// prior block. Returns the frame landed on.
    ///
    /// # Errors
    /// [`DecodeError::Io`] on seek failure, [`DecodeError::Malformed`] on a bad
    /// block header, plus the forward-decode errors of the landing block.
    pub fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        self.eof = false;
        self.pcm_len = 0;
        self.pcm_pos = 0;
        let ch = self.channels.max(1);
        let mut pos = 0u64; // absolute offset of the header about to be read
        let mut last_end = 0u64; // first sample past the last audio block seen
        loop {
            self.src
                .seek(SeekFrom::Start(pos))
                .map_err(|_| DecodeError::Io)?;
            let mut hdr = [0u8; 32];
            if !read_full_or_eof(&mut self.src, &mut hdr)? {
                break; // ran past the last block — `sample` is at/after the end
            }
            if &hdr[0..4] != b"wvpk" {
                break; // trailing tag (e.g. APEv2) — treat as end
            }
            let ck_size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
            if ck_size < 24 {
                return Err(DecodeError::Malformed);
            }
            let full_block = 8 + u64::from(ck_size); // ckID+ckSize + body
            let block_index = u64::from(u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]))
                + (u64::from(hdr[10]) << 32);
            let block_samples = u32::from_le_bytes([hdr[20], hdr[21], hdr[22], hdr[23]]);
            if block_samples == 0 {
                // Metadata-only block (leading wrapper) — advance, no samples.
                pos += full_block;
                continue;
            }
            let block_end = block_index + u64::from(block_samples);
            if sample < block_end {
                // Target is in (or before) this block: decode it and discard the
                // head. Seek back so `decode_block` re-reads the header.
                self.src
                    .seek(SeekFrom::Start(pos))
                    .map_err(|_| DecodeError::Io)?;
                if !self.decode_block()? {
                    break;
                }
                let within = sample
                    .saturating_sub(block_index)
                    .min(u64::from(block_samples));
                self.pcm_pos = within as usize * ch;
                return Ok(block_index + within);
            }
            last_end = block_end;
            pos += full_block;
        }
        // `sample` is at or past the end — leave the decoder at a clean EOF.
        self.eof = true;
        self.pcm_len = 0;
        self.pcm_pos = 0;
        Ok(last_end)
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to`]).
    ///
    /// # Errors
    /// As [`Self::seek_to`].
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        self.seek_to(crate::decoder::secs_to_samples(
            secs,
            self.info.sample_rate.max(1),
        ))
    }
}

impl<S: Source> Decoder for WavpackDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        self.info
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let mut w = 0;
        while w < out.len() {
            if self.pcm_pos >= self.pcm_len {
                if self.eof {
                    break;
                }
                match self.decode_block() {
                    Ok(true) => {}
                    Ok(false) => break,
                    // Return any already-decoded samples first; re-surface the
                    // error on the next call rather than dropping buffered PCM.
                    Err(e) => {
                        if w > 0 {
                            return Ok(w);
                        }
                        return Err(e);
                    }
                }
            }
            let avail = self.pcm_len - self.pcm_pos;
            let take = avail.min(out.len() - w);
            out[w..w + take].copy_from_slice(&self.pcm[self.pcm_pos..self.pcm_pos + take]);
            self.pcm_pos += take;
            w += take;
        }
        Ok(w)
    }
}

/// Parse `ID_DECORR_SAMPLES` into the passes' history seeds (from last pass to
/// first; each value is an i16 LE through [`wp_exp2s`]).
fn parse_decorr_samples(raw: &[u8], passes: &mut [DecorrPass], mono: bool) {
    let mut p = 0usize;
    let get = |raw: &[u8], o: usize| -> i32 {
        if o + 1 < raw.len() {
            wp_exp2s(i32::from(i16::from_le_bytes([raw[o], raw[o + 1]])))
        } else {
            0
        }
    };
    let n = passes.len();
    for j in 0..n {
        let pass = &mut passes[n - 1 - j];
        let term = pass.term;
        if term > 8 {
            pass.samples_a[0] = get(raw, p);
            pass.samples_a[1] = get(raw, p + 2);
            p += 4;
            if !mono {
                pass.samples_b[0] = get(raw, p);
                pass.samples_b[1] = get(raw, p + 2);
                p += 4;
            }
        } else if term < 0 {
            pass.samples_a[0] = get(raw, p);
            pass.samples_b[0] = get(raw, p + 2);
            p += 4;
        } else {
            for m in 0..term as usize {
                pass.samples_a[m] = get(raw, p);
                p += 2;
                if !mono {
                    pass.samples_b[m] = get(raw, p);
                    p += 2;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp2s_matches_reference_points() {
        // wp_exp2s(0) = 0x100 >> 9 = 0; wp_exp2s(0x800) = (table[0]|0x100)<<(8-9.. )
        assert_eq!(wp_exp2s(0), 0);
        assert_eq!(wp_exp2s(-1), -(wp_exp2s(1)));
    }

    #[test]
    fn count_bits_basic() {
        assert_eq!(count_bits(0), 0);
        assert_eq!(count_bits(1), 1);
        assert_eq!(count_bits(255), 8);
        assert_eq!(count_bits(256), 9);
    }

    #[test]
    fn apply_weight_rounds_like_reference() {
        // (1024 * 100 + 512) >> 10 = 100 (weight 1024 == unity)
        assert_eq!(apply_weight(1024, 100), 100);
        assert_eq!(apply_weight(512, 100), 50);
    }
}
