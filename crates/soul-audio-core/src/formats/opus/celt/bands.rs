//! CELT band-shape decode loop — RFC 6716 §4.3.4.
//!
//! [`quant_all_bands`] walks the coded bands, reading each band's normalised
//! MDCT shape: recursive mid/side + time splitting ([`quant_partition`]), stereo
//! angle decode ([`compute_theta`]), PVQ leaf decode ([`super::vq::alg_unquant`]),
//! spectral folding, and the per-band [collapse masks] Task 8's anti-collapse
//! consumes. The output is the unit-norm coefficient array(s) `x` (and `y` for
//! stereo) — Task 8 denormalises with the band energies and runs the inverse
//! MDCT.
//!
//! # Source correspondence (libopus **v1.5.2** `celt/bands.c`, **decoder** path)
//!
//! Transcribed with `encode = 0`, `resynth = 1`, `theta_rdo = 0`, which elides
//! every encoder-only branch (`stereo_itheta`, `intensity_stereo`,
//! `stereo_split`, `alg_quant`, the theta-RDO search). Function map:
//! [`quant_all_bands`] ← `quant_all_bands`; [`quant_band`] ← `quant_band`;
//! [`quant_partition`] ← `quant_partition`; [`quant_band_stereo`] ←
//! `quant_band_stereo`; [`quant_band_n1`] ← `quant_band_n1`; [`compute_theta`]
//! ← `compute_theta`; [`compute_qn`], [`haar1`], [`deinterleave_hadamard`],
//! [`interleave_hadamard`], [`stereo_merge`], [`bitexact_cos`],
//! [`bitexact_log2tan`], [`celt_lcg_rand`], [`isqrt32`] likewise. `bandE` is
//! `NULL` in the decode call (intensity_stereo is encoder-only), so band
//! energies are *not* needed here.
//!
//! # Buffer model (safe, no-alloc)
//!
//! libopus threads raw `celt_norm*` pointers into three buffers: the spectrum
//! `X`/`Y`, the folding reference `norm`, and a `lowband_scratch` aliased into
//! the spectrum tail. Here the spectrum is `x`/`y` (`&mut [f32]`, one per
//! channel), `norm` is a caller-supplied scratch, and the per-`quant_band`
//! `lowband_scratch` is a fixed [`MAX_BAND_N`] stack copy (functionally
//! identical to libopus aliasing the spectrum tail — the copy is written then
//! read within one call and never clobbers `norm`, so the folded values fed to
//! `quant_partition` are the same). `effEBands == nbEBands` for the standard
//! 48 kHz mode, so the `i >= effEBands` "X = norm" branch never fires.
//!
//! [collapse masks]: crate::formats::opus::celt::vq

use crate::formats::opus::opus_tables::{CACHE_BITS, CACHE_INDEX, EBAND_5MS, LOG_N, NUM_BANDS};
use crate::formats::opus::range::RangeDecoder;

use super::allocation::{bits2pulses, get_pulses, pulses2bits};
use super::cwrs::MAX_BAND_N;
use super::vq::{alg_unquant, celt_sqrt, renormalise_vector};

const BITRES: i32 = 3;
const QTHETA_OFFSET: i32 = 4;
const QTHETA_OFFSET_TWOPHASE: i32 = 16;
/// `SPREAD_AGGRESSIVE` (libopus `celt/bands.h`).
const SPREAD_AGGRESSIVE: u32 = 3;
/// `NORM_SCALING` / `Q15ONE` in the float build (libopus `celt/arch.h`).
const NORM_SCALING: f32 = 1.0;

/// Per-channel `norm` (folding-reference) capacity for the standard mode:
/// `M*eBands[nbEBands-1]` at `LM=3` (`8 * 78`). [`quant_all_bands`] needs a
/// `norm` scratch of at least `2 * MAX_NORM_PER_CH`.
pub(crate) const MAX_NORM_PER_CH: usize = 624;

/// Natural→ordered Hadamard permutation (libopus `celt/bands.c` `ordery_table`):
/// rows for stride N = 2, 4, 8, 16. `ordery_table + stride - 2` selects a row.
static ORDERY_TABLE: [usize; 30] = [
    1, 0, //
    3, 0, 2, 1, //
    7, 0, 4, 3, 6, 1, 5, 2, //
    15, 0, 8, 7, 12, 3, 11, 4, 14, 1, 9, 6, 13, 2, 10, 5,
];

/// libopus `celt/bands.c` `bit_interleave_table` (recombine fill remap).
static BIT_INTERLEAVE_TABLE: [u8; 16] = [0, 1, 1, 1, 2, 3, 3, 3, 2, 3, 3, 3, 2, 3, 3, 3];
/// libopus `celt/bands.c` `bit_deinterleave_table` (recombine collapse remap).
static BIT_DEINTERLEAVE_TABLE: [u8; 16] = [
    0x00, 0x03, 0x0C, 0x0F, 0x30, 0x33, 0x3C, 0x3F, 0xC0, 0xC3, 0xCC, 0xCF, 0xF0, 0xF3, 0xFC, 0xFF,
];

// --- small integer helpers (celt/entcode.h, celt/mathops.*) --------------

/// `EC_ILOG(x)` — `floor(log2 x) + 1`, `0` for `x == 0`.
fn ec_ilog(x: u32) -> i32 {
    (32 - x.leading_zeros()) as i32
}

/// libopus `celt_udiv` (unsigned): `n / d`. Guards `d <= 0` so malformed input
/// can't divide by zero.
fn celt_udiv(n: i32, d: i32) -> i32 {
    if d <= 0 {
        return 0;
    }
    ((n as u32) / (d as u32)) as i32
}

/// libopus `celt_sudiv` (signed truncating): `n / d`. Guards `d <= 0`.
fn celt_sudiv(n: i32, d: i32) -> i32 {
    if d <= 0 {
        return 0;
    }
    n / d
}

/// libopus `celt/mathops.h` `FRAC_MUL16`: `(16384 + (i16)a*(i16)b) >> 15`.
fn frac_mul16(a: i32, b: i32) -> i32 {
    (16384 + (a as i16 as i32) * (b as i16 as i32)) >> 15
}

/// libopus `celt/bands.c` `celt_lcg_rand` — the folding/anti-collapse PRNG.
pub(crate) fn celt_lcg_rand(seed: u32) -> u32 {
    seed.wrapping_mul(1664525).wrapping_add(1013904223)
}

/// libopus `celt/bands.c` `bitexact_cos` — a bit-exact `cos` approximation
/// (Q14 input `x` in `(0, 16384)`, Q15 output). Bit-exactness matters: it drives
/// the mid/side bit split, which the range coder must reproduce identically.
fn bitexact_cos(x: i32) -> i32 {
    let tmp = (4096 + (x as i16 as i32) * (x as i16 as i32)) >> 13;
    let mut x2 = tmp;
    x2 = (32767 - x2) + frac_mul16(x2, -7651 + frac_mul16(x2, 8277 + frac_mul16(-626, x2)));
    1 + x2
}

/// libopus `celt/bands.c` `bitexact_log2tan` — Q11 `log2(tan)` of the mid/side
/// cosines, again bit-exact for coder sync.
fn bitexact_log2tan(mut isin: i32, mut icos: i32) -> i32 {
    let lc = ec_ilog(icos as u32);
    let ls = ec_ilog(isin as u32);
    icos <<= 15 - lc;
    isin <<= 15 - ls;
    (ls - lc) * (1 << 11) + frac_mul16(isin, frac_mul16(isin, -2597) + 7932)
        - frac_mul16(icos, frac_mul16(icos, -2597) + 7932)
}

/// libopus `celt/mathops.c` `isqrt32` — `floor(sqrt(val))` in exact integer
/// arithmetic (triangular-pdf theta decode).
fn isqrt32(mut val: u32) -> u32 {
    let mut g = 0u32;
    let mut bshift = (ec_ilog(val) - 1) >> 1;
    let mut b = 1u32 << bshift;
    loop {
        let t = ((g << 1) + b) << bshift;
        if t <= val {
            g += b;
            val -= t;
        }
        b >>= 1;
        bshift -= 1;
        if bshift < 0 {
            break;
        }
    }
    g
}

/// libopus `celt/bands.c` `haar1` — in-place 2-point orthonormal (Hadamard)
/// lift over `stride`-interleaved pairs. Float build: the `PSHR32(...,15)` is a
/// plain `f32` add/sub.
fn haar1(x: &mut [f32], n0: usize, stride: usize) {
    let n0 = n0 >> 1;
    const R: f32 = core::f32::consts::FRAC_1_SQRT_2; // 0.70710678
    for i in 0..stride {
        for j in 0..n0 {
            let a = stride * 2 * j + i;
            let b = stride * (2 * j + 1) + i;
            let tmp1 = R * x[a];
            let tmp2 = R * x[b];
            x[a] = tmp1 + tmp2;
            x[b] = tmp1 - tmp2;
        }
    }
}

/// libopus `celt/bands.c` `deinterleave_hadamard` — reorder `stride`-interleaved
/// MDCTs into time order (`hadamard` selects the bit-reversed-Gray permutation).
fn deinterleave_hadamard(x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    let n = n0 * stride;
    let mut tmp = [0.0f32; MAX_BAND_N];
    if hadamard {
        let ord = stride.wrapping_sub(2);
        for i in 0..stride {
            let oi = ORDERY_TABLE.get(ord + i).copied().unwrap_or(0);
            for j in 0..n0 {
                tmp[oi * n0 + j] = x[j * stride + i];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[i * n0 + j] = x[j * stride + i];
            }
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

/// libopus `celt/bands.c` `interleave_hadamard` — the inverse of
/// [`deinterleave_hadamard`] (resynth reorder back to frequency order).
fn interleave_hadamard(x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    let n = n0 * stride;
    let mut tmp = [0.0f32; MAX_BAND_N];
    if hadamard {
        let ord = stride.wrapping_sub(2);
        for i in 0..stride {
            let oi = ORDERY_TABLE.get(ord + i).copied().unwrap_or(0);
            for j in 0..n0 {
                tmp[j * stride + i] = x[oi * n0 + j];
            }
        }
    } else {
        for i in 0..stride {
            for j in 0..n0 {
                tmp[j * stride + i] = x[i * n0 + j];
            }
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

/// libopus `celt/bands.c` `stereo_merge` (float build) — reconstruct L/R from
/// the decoded mid `X` (scaled by `mid`) and side `Y`. If either reconstructed
/// energy is ~0 it copies X→Y (degenerate guard), exactly as libopus.
fn stereo_merge(x: &mut [f32], y: &mut [f32], mid: f32, n: usize) {
    let mut xp = 0.0f32;
    let mut side = 0.0f32;
    for j in 0..n {
        xp += y[j] * x[j];
        side += y[j] * y[j];
    }
    xp *= mid;
    let mid2 = mid; // SHR16 is identity in float
    let el = mid2 * mid2 + side - 2.0 * xp;
    let er = mid2 * mid2 + side + 2.0 * xp;
    if er < 6e-4 || el < 6e-4 {
        y[..n].copy_from_slice(&x[..n]);
        return;
    }
    let lgain = if el > 0.0 { 1.0 / celt_sqrt(el) } else { 0.0 };
    let rgain = if er > 0.0 { 1.0 / celt_sqrt(er) } else { 0.0 };
    for j in 0..n {
        let l = mid * x[j];
        let r = y[j];
        x[j] = lgain * (l - r);
        y[j] = rgain * (l + r);
    }
}

/// libopus `celt/bands.c` `compute_qn` — resolution (number of quantisation
/// steps) for the split angle theta.
fn compute_qn(n: i32, b: i32, offset: i32, pulse_cap: i32, stereo: bool) -> i32 {
    static EXP2_TABLE8: [i32; 8] = [16384, 17866, 19483, 21247, 23170, 25267, 27554, 30048];
    let mut n2 = 2 * n - 1;
    if stereo && n == 2 {
        n2 -= 1;
    }
    let mut qb = celt_sudiv(b + n2 * offset, n2);
    qb = qb.min(b - pulse_cap - (4 << BITRES));
    qb = qb.min(8 << BITRES);
    if qb < (1 << BITRES >> 1) {
        1
    } else {
        let mut qn = EXP2_TABLE8[(qb & 0x7) as usize] >> (14 - (qb >> BITRES));
        qn = (qn + 1) >> 1 << 1;
        qn
    }
}

/// The decoder-side split parameters [`compute_theta`] produces (libopus
/// `struct split_ctx`).
struct SplitCtx {
    inv: bool,
    imid: i32,
    iside: i32,
    delta: i32,
    itheta: i32,
    qalloc: i32,
}

/// Per-band decode context (libopus `struct band_ctx`, decoder fields only —
/// `theta_round`/`avoid_split_noise`/`arch` are encoder-only and omitted).
struct BandCtx<'a, 'b> {
    rd: &'a mut RangeDecoder<'b>,
    i: usize,
    intensity: usize,
    spread: u32,
    tf_change: i32,
    remaining_bits: i32,
    seed: u32,
    disable_inv: bool,
}

/// libopus `celt/bands.c` `compute_theta` (decoder path) — decode the mid/side
/// (or time-)split angle `theta`, deriving `imid`/`iside`/`delta` and consuming
/// `qalloc` bits from `*b`. `lm` is the current split level, `stereo` selects
/// the stereo pdf, and `*fill` is masked per the decoded angle.
#[allow(clippy::too_many_arguments)]
fn compute_theta(
    ctx: &mut BandCtx,
    n: i32,
    b: &mut i32,
    big_b: i32,
    b0: i32,
    lm: i32,
    stereo: bool,
    fill: &mut i32,
) -> SplitCtx {
    let i = ctx.i;
    let pulse_cap = i32::from(LOG_N[i]) + lm * (1 << BITRES);
    let offset = (pulse_cap >> 1)
        - if stereo && n == 2 {
            QTHETA_OFFSET_TWOPHASE
        } else {
            QTHETA_OFFSET
        };
    let mut qn = compute_qn(n, *b, offset, pulse_cap, stereo);
    if stereo && i >= ctx.intensity {
        qn = 1;
    }
    let tell = ctx.rd.tell_frac() as i32;
    let mut itheta = 0i32;
    let mut inv = false;

    if qn != 1 {
        // Entropy-decode the angle: step pdf for stereo, uniform for time
        // splits, triangular otherwise.
        if stereo && n > 2 {
            let p0 = 3i32;
            let x0 = qn / 2;
            let ft = p0 * (x0 + 1) + x0;
            let fs = ctx.rd.decode(ft as u32) as i32;
            let x = if fs < (x0 + 1) * p0 {
                fs / p0
            } else {
                x0 + 1 + (fs - (x0 + 1) * p0)
            };
            let (fl, fh) = if x <= x0 {
                (p0 * x, p0 * (x + 1))
            } else {
                ((x - 1 - x0) + (x0 + 1) * p0, (x - x0) + (x0 + 1) * p0)
            };
            ctx.rd.dec_update(fl as u32, fh as u32, ft as u32);
            itheta = x;
        } else if b0 > 1 || stereo {
            itheta = ctx.rd.dec_uint((qn + 1) as u32) as i32;
        } else {
            let ft = ((qn >> 1) + 1) * ((qn >> 1) + 1);
            let fm = ctx.rd.decode(ft as u32) as i32;
            let (fl, fs) = if fm < (((qn >> 1) * ((qn >> 1) + 1)) >> 1) {
                itheta = ((isqrt32((8 * fm + 1) as u32) as i32) - 1) >> 1;
                ((itheta * (itheta + 1)) >> 1, itheta + 1)
            } else {
                itheta = (2 * (qn + 1) - (isqrt32((8 * (ft - fm - 1) + 1) as u32) as i32)) >> 1;
                (
                    ft - (((qn + 1 - itheta) * (qn + 2 - itheta)) >> 1),
                    qn + 1 - itheta,
                )
            };
            ctx.rd.dec_update(fl as u32, (fl + fs) as u32, ft as u32);
        }
        debug_assert!(itheta >= 0);
        itheta = celt_udiv(itheta * 16384, qn);
    } else if stereo {
        if *b > 2 << BITRES && ctx.remaining_bits > 2 << BITRES {
            inv = ctx.rd.decode_bit_logp(2) != 0;
        }
        if ctx.disable_inv {
            inv = false;
        }
        itheta = 0;
    }

    let qalloc = ctx.rd.tell_frac() as i32 - tell;
    *b -= qalloc;

    let (imid, iside, delta);
    if itheta == 0 {
        imid = 32767;
        iside = 0;
        *fill &= (1 << big_b) - 1;
        delta = -16384;
    } else if itheta == 16384 {
        imid = 0;
        iside = 32767;
        *fill &= ((1 << big_b) - 1) << big_b;
        delta = 16384;
    } else {
        imid = bitexact_cos(itheta);
        iside = bitexact_cos(16384 - itheta);
        delta = frac_mul16((n - 1) << 7, bitexact_log2tan(iside, imid));
    }

    SplitCtx {
        inv,
        imid,
        iside,
        delta,
        itheta,
        qalloc,
    }
}

/// libopus `celt/bands.c` `quant_band_n1` — the single-coefficient band: one
/// sign bit per channel (`x[0] = ±1`), and a folding sample.
fn quant_band_n1(
    ctx: &mut BandCtx,
    x: &mut [f32],
    y: Option<&mut [f32]>,
    lowband_out: Option<&mut [f32]>,
) -> u32 {
    // channel 0
    let mut sign0 = 0u32;
    if ctx.remaining_bits >= 1 << BITRES {
        sign0 = ctx.rd.dec_bits(1);
        ctx.remaining_bits -= 1 << BITRES;
    }
    x[0] = if sign0 != 0 {
        -NORM_SCALING
    } else {
        NORM_SCALING
    };
    // channel 1
    if let Some(y) = y {
        let mut sign1 = 0u32;
        if ctx.remaining_bits >= 1 << BITRES {
            sign1 = ctx.rd.dec_bits(1);
            ctx.remaining_bits -= 1 << BITRES;
        }
        y[0] = if sign1 != 0 {
            -NORM_SCALING
        } else {
            NORM_SCALING
        };
    }
    if let Some(lo) = lowband_out {
        lo[0] = x[0]; // SHR16(X[0],4) is identity in float; uses the decoded X[0]
    }
    1
}

/// The max split-cost (`cache[cache[0]]`) for band `i` at level `lm`, from the
/// pulse cache. Used only for the split decision; returns 0 when there's no
/// entry (guards a `-1` `CACHE_INDEX`).
fn cache_max_cost(i: usize, lm: i32) -> i32 {
    if lm < 0 {
        return 0;
    }
    let idx = ((lm + 1) as usize) * NUM_BANDS + i;
    match CACHE_INDEX.get(idx) {
        Some(&off) if off >= 0 => {
            let base = off as usize;
            let k = CACHE_BITS[base] as usize;
            i32::from(CACHE_BITS[base + k])
        }
        _ => 0,
    }
}

/// libopus `celt/bands.c` `quant_partition` — decode one mono partition,
/// recursively splitting into two half-bands (mid/side or time) when the band is
/// wide and has bits to spare, else PVQ-decoding the leaf (or folding when it
/// gets no pulses). Returns the collapse mask.
#[allow(clippy::too_many_arguments)]
fn quant_partition(
    ctx: &mut BandCtx,
    x: &mut [f32],
    mut n: usize,
    mut b: i32,
    mut bb: i32,
    lowband: Option<&[f32]>,
    mut lm: i32,
    gain: f32,
    mut fill: i32,
) -> u32 {
    let i = ctx.i;
    let b0 = bb;

    if lm != -1 && n > 2 && b > cache_max_cost(i, lm) + 12 {
        // --- split ---
        n >>= 1;
        let y_off = n;
        lm -= 1;
        if bb == 1 {
            fill = (fill & 1) | (fill << 1);
        }
        bb = (bb + 1) >> 1;

        let sctx = compute_theta(ctx, n as i32, &mut b, bb, b0, lm, false, &mut fill);
        let mid = sctx.imid as f32 * (1.0 / 32768.0);
        let side = sctx.iside as f32 * (1.0 / 32768.0);
        let itheta = sctx.itheta;
        let mut delta = sctx.delta;

        // Give more bits to low-energy MDCTs than they'd otherwise deserve.
        if b0 > 1 && (itheta & 0x3fff) != 0 {
            if itheta > 8192 {
                delta -= delta >> (4 - lm);
            } else {
                delta = 0.min(delta + (((n as i32) << BITRES) >> (5 - lm)));
            }
        }
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= sctx.qalloc;

        let (x_lo, x_hi) = x.split_at_mut(y_off);
        let (lb_lo, lb_hi) = match lowband {
            Some(lb) => {
                let (a, c) = lb.split_at(y_off);
                (Some(a), Some(c))
            }
            None => (None, None),
        };

        if mbits >= sbits {
            let rebalance = ctx.remaining_bits;
            let mut c = quant_partition(ctx, x_lo, n, mbits, bb, lb_lo, lm, gain * mid, fill);
            let rebalance = mbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 0 {
                sbits += rebalance - (3 << BITRES);
            }
            c |= quant_partition(ctx, x_hi, n, sbits, bb, lb_hi, lm, gain * side, fill >> bb)
                << (b0 >> 1);
            c
        } else {
            let rebalance = ctx.remaining_bits;
            let mut c =
                quant_partition(ctx, x_hi, n, sbits, bb, lb_hi, lm, gain * side, fill >> bb)
                    << (b0 >> 1);
            let rebalance = sbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 16384 {
                mbits += rebalance - (3 << BITRES);
            }
            c |= quant_partition(ctx, x_lo, n, mbits, bb, lb_lo, lm, gain * mid, fill);
            c
        }
    } else {
        // --- leaf ---
        let mut q = bits2pulses(i, lm, b);
        let mut curr_bits = pulses2bits(i, lm, q);
        ctx.remaining_bits -= curr_bits;
        while ctx.remaining_bits < 0 && q > 0 {
            ctx.remaining_bits += curr_bits;
            q -= 1;
            curr_bits = pulses2bits(i, lm, q);
            ctx.remaining_bits -= curr_bits;
        }

        if q != 0 {
            let k = get_pulses(q);
            alg_unquant(x, n, k, ctx.spread, bb.max(0) as usize, ctx.rd, gain)
        } else {
            // No pulses: fold from previous content, LCG noise, or clear.
            let cm_mask = if bb >= 32 { u32::MAX } else { (1u32 << bb) - 1 };
            fill &= cm_mask as i32;
            if fill == 0 {
                for v in x.iter_mut().take(n) {
                    *v = 0.0;
                }
                0
            } else {
                match lowband {
                    None => {
                        // Noise.
                        for v in x.iter_mut().take(n) {
                            ctx.seed = celt_lcg_rand(ctx.seed);
                            *v = ((ctx.seed as i32) >> 20) as f32;
                        }
                        renormalise_vector(x, n, gain);
                        cm_mask
                    }
                    Some(lb) => {
                        // Folded spectrum ~48 dB below normal, plus dither.
                        for j in 0..n {
                            ctx.seed = celt_lcg_rand(ctx.seed);
                            let tmp = if ctx.seed & 0x8000 != 0 {
                                1.0 / 256.0
                            } else {
                                -1.0 / 256.0
                            };
                            x[j] = lb[j] + tmp;
                        }
                        renormalise_vector(x, n, gain);
                        fill as u32
                    }
                }
            }
        }
    }
}

/// libopus `celt/bands.c` `quant_band` — decode one mono band: apply the tf
/// recombine/time-split reorganisation to the folding reference, decode the
/// partition tree, then undo the reorganisation on the output and emit the
/// folding output for later bands. Returns the collapse mask.
#[allow(clippy::too_many_arguments)]
fn quant_band(
    ctx: &mut BandCtx,
    norm: &mut [f32],
    x: &mut [f32],
    n: usize,
    b: i32,
    bb_in: i32,
    lowband_off: Option<usize>,
    lm: i32,
    lowband_out_off: Option<usize>,
    gain: f32,
    mut fill: i32,
) -> u32 {
    let n0 = n;
    let b0 = bb_in;
    let long_blocks = b0 == 1;
    let mut bb = bb_in;
    let mut n_b = n / (bb.max(1) as usize);
    let mut tf_change = ctx.tf_change;
    let mut recombine = 0i32;
    let mut time_divide = 0i32;

    if n == 1 {
        let lo = lowband_out_off.map(|o| &mut norm[o..o + 1]);
        return quant_band_n1(ctx, x, None, lo);
    }

    if tf_change > 0 {
        recombine = tf_change;
    }

    // Always work the folding reference on a private scratch copy so haar1 /
    // deinterleave never clobber `norm` (functionally identical to libopus
    // aliasing the spectrum tail — see module docs).
    let mut scratch = [0.0f32; MAX_BAND_N];
    let lb_present = lowband_off.is_some();
    if let Some(off) = lowband_off {
        scratch[..n].copy_from_slice(&norm[off..off + n]);
    }

    for k in 0..recombine {
        if lb_present {
            haar1(&mut scratch, n >> k, 1 << k);
        }
        let f = fill as usize;
        fill = i32::from(BIT_INTERLEAVE_TABLE[f & 0xF])
            | (i32::from(BIT_INTERLEAVE_TABLE[(f >> 4) & 0xF]) << 2);
    }
    bb >>= recombine;
    n_b <<= recombine;

    while (n_b & 1) == 0 && tf_change < 0 {
        if lb_present {
            haar1(&mut scratch, n_b, bb as usize);
        }
        fill |= fill << bb;
        bb <<= 1;
        n_b >>= 1;
        time_divide += 1;
        tf_change += 1;
    }
    let b0b = bb;
    let n_b0 = n_b;

    if b0b > 1 && lb_present {
        deinterleave_hadamard(
            &mut scratch,
            n_b >> recombine,
            (b0b << recombine) as usize,
            long_blocks,
        );
    }

    let lowband: Option<&[f32]> = if lb_present {
        Some(&scratch[..n])
    } else {
        None
    };
    let mut cm = quant_partition(ctx, x, n, b, bb, lowband, lm, gain, fill);

    // --- resynth: undo the time/freq reorganisation on the decoded output ---
    if b0b > 1 {
        interleave_hadamard(
            x,
            n_b0 >> recombine,
            (b0b << recombine) as usize,
            long_blocks,
        );
    }
    let mut n_b = n_b0;
    let mut bb = b0b;
    for _ in 0..time_divide {
        bb >>= 1;
        n_b <<= 1;
        if bb < 32 {
            cm |= cm >> bb;
        }
        haar1(x, n_b, bb as usize);
    }
    for k in 0..recombine {
        cm = u32::from(BIT_DEINTERLEAVE_TABLE[(cm as usize) & 0xF]);
        haar1(x, n0 >> k, 1 << k);
    }
    bb <<= recombine;

    if let Some(o) = lowband_out_off {
        let nn = celt_sqrt(n0 as f32);
        for j in 0..n0 {
            norm[o + j] = nn * x[j];
        }
    }
    let mask = if bb >= 32 { u32::MAX } else { (1u32 << bb) - 1 };
    cm & mask
}

/// libopus `celt/bands.c` `quant_band_stereo` — decode one stereo band: decode
/// the mid/side angle, split the bit budget across mid (`X`) and side (`Y`),
/// then reconstruct L/R via [`stereo_merge`] (or the N=2 orthogonal special
/// case). Returns the collapse mask.
#[allow(clippy::too_many_arguments)]
fn quant_band_stereo(
    ctx: &mut BandCtx,
    norm: &mut [f32],
    x: &mut [f32],
    y: &mut [f32],
    n: usize,
    b: i32,
    bb: i32,
    lowband_off: Option<usize>,
    lm: i32,
    lowband_out_off: Option<usize>,
    mut fill: i32,
) -> u32 {
    if n == 1 {
        let lo = lowband_out_off.map(|o| &mut norm[o..o + 1]);
        return quant_band_n1(ctx, x, Some(y), lo);
    }

    let orig_fill = fill;
    let mut b = b;
    let sctx = compute_theta(ctx, n as i32, &mut b, bb, bb, lm, true, &mut fill);
    let inv = sctx.inv;
    let itheta = sctx.itheta;
    let mid = sctx.imid as f32 * (1.0 / 32768.0);
    let side = sctx.iside as f32 * (1.0 / 32768.0);
    let delta = sctx.delta;
    let qalloc = sctx.qalloc;

    let mut cm;
    if n == 2 {
        let mut mbits = b;
        let mut sbits = 0i32;
        if itheta != 0 && itheta != 16384 {
            sbits = 1 << BITRES;
        }
        mbits -= sbits;
        let c = itheta > 8192;
        ctx.remaining_bits -= qalloc + sbits;

        let mut sign = 0u32;
        if sbits != 0 {
            sign = ctx.rd.dec_bits(1);
        }
        let signf = (1 - 2 * sign as i32) as f32;

        if c {
            // x2 = Y (mid), y2 = X.
            cm = quant_band(
                ctx,
                norm,
                y,
                n,
                mbits,
                bb,
                lowband_off,
                lm,
                lowband_out_off,
                NORM_SCALING,
                orig_fill,
            );
            x[0] = -signf * y[1];
            x[1] = signf * y[0];
        } else {
            cm = quant_band(
                ctx,
                norm,
                x,
                n,
                mbits,
                bb,
                lowband_off,
                lm,
                lowband_out_off,
                NORM_SCALING,
                orig_fill,
            );
            y[0] = -signf * x[1];
            y[1] = signf * x[0];
        }
        // Orthogonal N=2 reconstruction.
        let mx0 = mid * x[0];
        let mx1 = mid * x[1];
        let sy0 = side * y[0];
        let sy1 = side * y[1];
        x[0] = mx0 - sy0;
        y[0] = mx0 + sy0;
        x[1] = mx1 - sy1;
        y[1] = mx1 + sy1;
    } else {
        let mut mbits = 0.max(b.min((b - delta) / 2));
        let mut sbits = b - mbits;
        ctx.remaining_bits -= qalloc;

        if mbits >= sbits {
            let rebalance = ctx.remaining_bits;
            cm = quant_band(
                ctx,
                norm,
                x,
                n,
                mbits,
                bb,
                lowband_off,
                lm,
                lowband_out_off,
                NORM_SCALING,
                fill,
            );
            let rebalance = mbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 0 {
                sbits += rebalance - (3 << BITRES);
            }
            cm |= quant_band(ctx, norm, y, n, sbits, bb, None, lm, None, side, fill >> bb);
        } else {
            let rebalance = ctx.remaining_bits;
            cm = quant_band(ctx, norm, y, n, sbits, bb, None, lm, None, side, fill >> bb);
            let rebalance = sbits - (rebalance - ctx.remaining_bits);
            if rebalance > 3 << BITRES && itheta != 16384 {
                mbits += rebalance - (3 << BITRES);
            }
            cm |= quant_band(
                ctx,
                norm,
                x,
                n,
                mbits,
                bb,
                lowband_off,
                lm,
                lowband_out_off,
                NORM_SCALING,
                fill,
            );
        }
    }

    if n != 2 {
        stereo_merge(x, y, mid, n);
    }
    if inv {
        for v in y.iter_mut().take(n) {
            *v = -*v;
        }
    }
    cm
}

/// libopus `celt/bands.c` `special_hybrid_folding` — duplicate enough of the
/// first band's folding data to fold the second (a no-op for CELT-only
/// `start == 0`, where `n2 == n1`).
fn special_hybrid_folding(norm: &mut [f32], norm_sz: usize, start: usize, m: usize, dual: bool) {
    let n1 = m * ((EBAND_5MS[start + 1] as usize) - (EBAND_5MS[start] as usize));
    let n2 = m * ((EBAND_5MS[start + 2] as usize) - (EBAND_5MS[start + 1] as usize));
    if n2 > n1 {
        norm.copy_within((2 * n1 - n2)..n1, n1);
        if dual {
            norm.copy_within((norm_sz + 2 * n1 - n2)..(norm_sz + n1), norm_sz + n1);
        }
    }
}

/// libopus `celt/bands.c` `quant_all_bands` (**decoder** path) — decode every
/// band's normalised MDCT shape into `x` (and `y` for stereo), writing the
/// per-band [collapse masks]. This is the orchestrator Task 8 calls between bit
/// allocation and denormalisation.
///
/// - `x` / `y`: per-channel normalised coefficient buffers, each `>= M*eBands[end]`
///   long (`y = None` for mono). Filled in place, band `[M*eBands[i], M*eBands[i+1])`.
/// - `collapse_masks`: `C*NUM_BANDS` bytes, one mask per band/channel (Task 8).
/// - `pulses` / `tf_res`: the [`super::allocation::Allocation`] PVQ budget and tf
///   resolution; `spread`, `dual_stereo`, `intensity`, `balance`, `coded_bands`
///   likewise. `total_bits` is `len*(8<<BITRES) - anti_collapse_rsv` (eighth-bits).
/// - `seed`: the CELT fold PRNG (`st->rng`); read as the initial seed and updated.
/// - `norm`: folding-reference scratch, `>= 2*MAX_NORM_PER_CH` (`>= 2*(M*eBands[nbEBands-1])`).
///
/// [collapse masks]: crate::formats::opus::celt::vq
#[allow(clippy::too_many_arguments)]
pub(crate) fn quant_all_bands(
    rd: &mut RangeDecoder,
    start: usize,
    end: usize,
    x: &mut [f32],
    mut y: Option<&mut [f32]>,
    collapse_masks: &mut [u8],
    pulses: &[i32; NUM_BANDS],
    short_blocks: bool,
    spread: u32,
    mut dual_stereo: bool,
    intensity: usize,
    tf_res: &[i32; NUM_BANDS],
    total_bits: i32,
    mut balance: i32,
    lm: u32,
    coded_bands: usize,
    seed: &mut u32,
    disable_inv: bool,
    norm: &mut [f32],
) {
    let lm_i = lm as i32;
    let m = 1usize << lm;
    let big_b0 = if short_blocks { m as i32 } else { 1 };
    let channels = if y.is_some() { 2 } else { 1 };
    let norm_offset = m * (EBAND_5MS[start] as usize);
    let norm_sz = m * (EBAND_5MS[NUM_BANDS - 1] as usize) - norm_offset;

    let mut ctx = BandCtx {
        rd,
        i: start,
        intensity,
        spread,
        tf_change: 0,
        remaining_bits: 0,
        seed: *seed,
        disable_inv,
    };
    let mut lowband_offset = 0usize;
    let mut update_lowband = true;

    for i in start..end {
        ctx.i = i;
        let last = i == end - 1;
        let xoff = m * (EBAND_5MS[i] as usize);
        let n = m * (EBAND_5MS[i + 1] as usize) - xoff;
        let tell = ctx.rd.tell_frac() as i32;
        if i != start {
            balance -= tell;
        }
        ctx.remaining_bits = total_bits - tell - 1;
        // Kept as libopus's literal `i <= codedBands-1` (not `i < codedBands`)
        // for transcription fidelity.
        #[allow(clippy::int_plus_one)]
        let b = if (i as i32) <= coded_bands as i32 - 1 {
            let curr_balance = celt_sudiv(balance, 3.min(coded_bands as i32 - i as i32));
            0.max(16383.min((ctx.remaining_bits + 1).min(pulses[i] + curr_balance)))
        } else {
            0
        };

        if ((xoff as i32 - n as i32) >= norm_offset as i32 || i == start + 1)
            && (update_lowband || lowband_offset == 0)
        {
            lowband_offset = i;
        }
        if i == start + 1 {
            special_hybrid_folding(norm, norm_sz, start, m, dual_stereo);
        }
        let tf_change = tf_res[i];
        ctx.tf_change = tf_change;

        // Conservative collapse-mask estimate for the bands we may fold from.
        let mut effective_lowband: i32 = -1;
        let mut x_cm;
        let mut y_cm;
        if lowband_offset != 0 && (spread != SPREAD_AGGRESSIVE || big_b0 > 1 || tf_change < 0) {
            effective_lowband =
                (m as i32 * i32::from(EBAND_5MS[lowband_offset]) - norm_offset as i32 - n as i32)
                    .max(0);
            let target = effective_lowband + norm_offset as i32;
            let target2 = target + n as i32;
            let mut fold_start = lowband_offset;
            while fold_start > 0 {
                fold_start -= 1;
                if m as i32 * i32::from(EBAND_5MS[fold_start]) <= target {
                    break;
                }
            }
            let mut fold_end = lowband_offset as i32 - 1;
            loop {
                fold_end += 1;
                if !(fold_end < i as i32
                    && m as i32 * i32::from(EBAND_5MS[fold_end as usize]) < target2)
                {
                    break;
                }
            }
            x_cm = 0u32;
            y_cm = 0u32;
            let mut fold_i = fold_start;
            loop {
                x_cm |= u32::from(collapse_masks[fold_i * channels]);
                y_cm |= u32::from(collapse_masks[fold_i * channels + channels - 1]);
                fold_i += 1;
                if (fold_i as i32) >= fold_end {
                    break;
                }
            }
        } else {
            let mask = if big_b0 >= 32 {
                u32::MAX
            } else {
                (1u32 << big_b0) - 1
            };
            x_cm = mask;
            y_cm = mask;
        }

        if dual_stereo && i == intensity {
            // Switch off dual stereo to do intensity: merge the two folding refs.
            dual_stereo = false;
            let count = xoff - norm_offset;
            for j in 0..count {
                norm[j] = 0.5 * (norm[j] + norm[norm_sz + j]);
            }
        }

        let lb0 = (effective_lowband >= 0).then_some(effective_lowband as usize);
        let lo0 = (!last).then_some(xoff - norm_offset);

        if dual_stereo {
            let x_band = &mut x[xoff..xoff + n];
            x_cm = quant_band(
                &mut ctx,
                norm,
                x_band,
                n,
                b / 2,
                big_b0,
                lb0,
                lm_i,
                lo0,
                NORM_SCALING,
                x_cm as i32,
            );
            // Guard the offset arithmetic behind the `>= 0` check: `then_some`
            // evaluates its argument eagerly, so the `-1` sentinel would wrap
            // `effective_lowband as usize` and overflow the `norm_sz +` add in a
            // debug build (the dual-stereo path the Task-7 fixture never took).
            let lb1 = if effective_lowband >= 0 {
                Some(norm_sz + effective_lowband as usize)
            } else {
                None
            };
            let lo1 = (!last).then_some(norm_sz + xoff - norm_offset);
            let y_band = &mut y.as_deref_mut().unwrap()[xoff..xoff + n];
            y_cm = quant_band(
                &mut ctx,
                norm,
                y_band,
                n,
                b / 2,
                big_b0,
                lb1,
                lm_i,
                lo1,
                NORM_SCALING,
                y_cm as i32,
            );
        } else if let Some(y_buf) = y.as_deref_mut() {
            let x_band = &mut x[xoff..xoff + n];
            let y_band = &mut y_buf[xoff..xoff + n];
            x_cm = quant_band_stereo(
                &mut ctx,
                norm,
                x_band,
                y_band,
                n,
                b,
                big_b0,
                lb0,
                lm_i,
                lo0,
                (x_cm | y_cm) as i32,
            );
            y_cm = x_cm;
        } else {
            let x_band = &mut x[xoff..xoff + n];
            x_cm = quant_band(
                &mut ctx,
                norm,
                x_band,
                n,
                b,
                big_b0,
                lb0,
                lm_i,
                lo0,
                NORM_SCALING,
                (x_cm | y_cm) as i32,
            );
            y_cm = x_cm;
        }

        collapse_masks[i * channels] = x_cm as u8;
        collapse_masks[i * channels + channels - 1] = y_cm as u8;
        balance += pulses[i] + tell;
        update_lowband = b > (n as i32) << BITRES;
    }
    *seed = ctx.seed;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::ogg_packets::OggPackets;
    use crate::formats::opus::celt::allocation::compute_allocation;
    use crate::formats::opus::celt::energy::{
        celt_end_band, celt_lm, read_preamble, unquant_coarse_energy, unquant_fine_energy,
    };
    use crate::formats::opus::toc::{self, Mode};
    use crate::io::SliceSource;

    #[test]
    fn haar1_is_orthonormal_involution() {
        // Two haar1 passes (stride 1) return the original (H is its own inverse).
        let orig = [0.3f32, -0.7, 0.5, 0.1];
        let mut x = orig;
        haar1(&mut x, 4, 1);
        haar1(&mut x, 4, 1);
        for (a, b) in x.iter().zip(orig.iter()) {
            assert!((a - b).abs() < 1e-5, "haar1 not involutive: {a} vs {b}");
        }
        // And it preserves energy.
        let mut y = orig;
        let e0: f32 = orig.iter().map(|v| v * v).sum();
        haar1(&mut y, 4, 1);
        let e1: f32 = y.iter().map(|v| v * v).sum();
        assert!((e0 - e1).abs() < 1e-5);
    }

    #[test]
    fn deinterleave_interleave_round_trip() {
        // interleave undoes deinterleave for both hadamard modes.
        for hadamard in [false, true] {
            let orig: [f32; 8] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
            let mut x = orig;
            deinterleave_hadamard(&mut x, 2, 4, hadamard);
            interleave_hadamard(&mut x, 2, 4, hadamard);
            assert_eq!(x, orig, "hadamard={hadamard}");
        }
    }

    #[test]
    fn bitexact_cos_endpoints_and_monotonic() {
        // cos(0)=1 -> ~32767; strictly decreasing over (0,16384).
        assert!(bitexact_cos(1) > 32000);
        let mut prev = i32::MAX;
        for x in (1..16384).step_by(97) {
            let c = bitexact_cos(x);
            assert!(c < prev, "not monotonic at {x}");
            assert!(c > 0);
            prev = c;
        }
    }

    #[test]
    fn isqrt32_matches_floor_sqrt() {
        for &v in &[
            1u32, 2, 3, 4, 8, 15, 16, 17, 100, 255, 256, 1_000_000, 133_128,
        ] {
            let got = isqrt32(v);
            assert_eq!(got, (v as f64).sqrt().floor() as u32, "isqrt32({v})");
        }
    }

    // --- Real-frame band-shape decode of testdata/tone.opus ------------------
    //
    // The first audio packet is a CELT-only fullband stereo 20 ms frame (config
    // 31, C=2, LM=3). We run the full preamble -> coarse energy -> allocation ->
    // fine energy -> quant_all_bands chain. Fine energy MUST run: it consumes
    // range-coder bits, so skipping it would desync the coder. libopus cannot run
    // at test time, so this is a structural gate: every decoded band is finite
    // and (per RFC 6716) unit-norm or zero before denormalisation, and the coder
    // does not overrun the frame budget. Exact coefficients are validated at Task
    // 8 PCM golden; the reviewer bands.c/vq.c/cwrs.c diff is the direct check.
    #[test]
    fn quant_all_bands_decodes_real_tone_opus_first_celt_frame() {
        // State is large (norm + spectrum + recursion) — run on a big stack.
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(run_real_frame_band_decode)
            .unwrap()
            .join()
            .unwrap();
    }

    fn run_real_frame_band_decode() {
        let bytes = include_bytes!("../../../../../../testdata/tone.opus");
        let mut ogg = OggPackets::new(SliceSource::new(bytes.as_slice())).expect("open ogg-opus");
        let mut hdr = [0u8; 512];
        ogg.next_packet(&mut hdr).expect("OpusHead");
        ogg.next_packet(&mut hdr).expect("OpusTags");

        let mut old_ebands = [0.0f32; NUM_BANDS * 2];
        let mut pkt = [0u8; 4096];
        loop {
            let np = ogg.next_packet(&mut pkt).expect("audio packet");
            assert!(np > 0, "no CELT frame found");
            let mut frames: [&[u8]; toc::MAX_FRAMES] = [&[][..]; toc::MAX_FRAMES];
            let (config, _fcount) = toc::frames(&pkt[..np], &mut frames).expect("split frames");
            if config.mode != Mode::CeltOnly {
                continue;
            }
            let channels = if config.stereo { 2 } else { 1 };
            let lm = celt_lm(config.frame_size_48k);
            let end = celt_end_band(config.bandwidth);
            let frame = frames[0];

            let mut rd = RangeDecoder::new(frame);
            let pre = read_preamble(&mut rd, 0, lm);
            unquant_coarse_energy(
                &mut rd,
                0,
                end,
                channels,
                lm,
                pre.intra_ener,
                &mut old_ebands,
            );
            let alloc = compute_allocation(&mut rd, 0, end, channels, lm, pre.is_transient);
            unquant_fine_energy(
                &mut rd,
                0,
                end,
                channels,
                &alloc.fine_quant,
                &mut old_ebands,
            );

            let m = 1usize << lm;
            let frame_n = m * 120;
            let mut x = vec![0.0f32; frame_n];
            let mut yb = vec![0.0f32; frame_n];
            let mut collapse = [0u8; NUM_BANDS * 2];
            let mut norm = vec![0.0f32; 2 * MAX_NORM_PER_CH];
            let mut seed = 0u32;
            // total_bits = len*(8<<BITRES) - anti_collapse_rsv (celt_decoder.c).
            let total_bits = (frame.len() as i32) * (8 << BITRES) - alloc.anti_collapse_rsv;
            let y = if channels == 2 {
                Some(yb.as_mut_slice())
            } else {
                None
            };

            quant_all_bands(
                &mut rd,
                0,
                end,
                &mut x,
                y,
                &mut collapse,
                &alloc.pulses,
                pre.is_transient,
                alloc.spread,
                alloc.dual_stereo,
                alloc.intensity,
                &alloc.tf_res,
                total_bits,
                alloc.balance,
                lm,
                alloc.coded_bands,
                &mut seed,
                false,
                &mut norm,
            );

            assert_eq!(
                channels, 2,
                "fixture first CELT frame is stereo (config 31)"
            );

            // No range-coder overrun / overflow.
            let storage_bits = frame.len() as u32 * 8;
            assert!(
                rd.tell() <= storage_bits,
                "range overrun: tell {} > {storage_bits}",
                rd.tell()
            );
            assert!(!rd.error(), "range decoder error flag set");
            // The decode consumed a sane share of the frame (a desync typically
            // stalls early or overruns).
            assert!(
                rd.tell() > storage_bits / 8,
                "suspiciously few bits consumed: {} of {storage_bits}",
                rd.tell()
            );

            // Every band, every channel: finite and unit-norm (coded) or zero.
            let mut unit_bands = 0usize;
            for c in 0..channels {
                let buf = if c == 0 { &x } else { &yb };
                for i in 0..end {
                    let lo = m * (EBAND_5MS[i] as usize);
                    let hi = m * (EBAND_5MS[i + 1] as usize);
                    let seg = &buf[lo..hi];
                    for &v in seg {
                        assert!(v.is_finite(), "band {i} ch {c}: non-finite coeff");
                    }
                    let nrm: f32 = seg.iter().map(|&v| v * v).sum();
                    assert!(
                        (nrm - 1.0).abs() < 0.05 || nrm < 0.05,
                        "band {i} ch {c}: norm {nrm} is neither unit nor zero"
                    );
                    if (nrm - 1.0).abs() < 0.05 {
                        unit_bands += 1;
                    }
                }
            }
            // A real fullband frame codes most bands, so the great majority must
            // be genuinely unit-norm (not all-zero).
            assert!(
                unit_bands >= channels * end / 2,
                "only {unit_bands} unit-norm bands of {} — likely a desync",
                channels * end
            );

            // Determinism: a second identical run gives identical coefficients.
            let mut x2 = vec![0.0f32; frame_n];
            let mut yb2 = vec![0.0f32; frame_n];
            let mut collapse2 = [0u8; NUM_BANDS * 2];
            let mut norm2 = vec![0.0f32; 2 * MAX_NORM_PER_CH];
            let mut old2 = [0.0f32; NUM_BANDS * 2];
            let mut rd2 = RangeDecoder::new(frame);
            let pre2 = read_preamble(&mut rd2, 0, lm);
            unquant_coarse_energy(&mut rd2, 0, end, channels, lm, pre2.intra_ener, &mut old2);
            let alloc2 = compute_allocation(&mut rd2, 0, end, channels, lm, pre2.is_transient);
            unquant_fine_energy(&mut rd2, 0, end, channels, &alloc2.fine_quant, &mut old2);
            let mut seed2 = 0u32;
            quant_all_bands(
                &mut rd2,
                0,
                end,
                &mut x2,
                Some(yb2.as_mut_slice()),
                &mut collapse2,
                &alloc2.pulses,
                pre2.is_transient,
                alloc2.spread,
                alloc2.dual_stereo,
                alloc2.intensity,
                &alloc2.tf_res,
                total_bits,
                alloc2.balance,
                lm,
                alloc2.coded_bands,
                &mut seed2,
                false,
                &mut norm2,
            );
            assert_eq!(x, x2, "channel 0 not deterministic");
            assert_eq!(yb, yb2, "channel 1 not deterministic");
            assert_eq!(collapse, collapse2, "collapse masks not deterministic");
            return;
        }
    }
}
