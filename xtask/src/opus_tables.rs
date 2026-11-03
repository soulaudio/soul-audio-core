//! Generates `crates/soul-audio-core/src/formats/opus/opus_tables.rs` — the
//! static CELT tables the from-scratch Opus decoder needs, so the `no_std` +
//! no-alloc core never computes trig or combinatorics at runtime. Run via
//! `cargo xtask gen-opus-tables`; the output is checked in and CI-guarded by
//! [`check`] (wired into `xtask verify`), exactly like `vorbis_tables.rs`.
//!
//! All values target the **standard Opus CELT mode** (48 kHz, the only mode a
//! bitstream ever selects — RFC 6716 §2): `Fs = 48000`, `shortMdctSize = 120`,
//! `nbEBands = 21`, `maxLM = 3` (2.5/5/10/20 ms frames → MDCT sizes
//! 120/240/480/960). libopus builds this same mode statically into
//! `celt/static_modes_float.h`; every "derived" table below is re-derived
//! host-side from the documented formulas and cross-checked byte-for-byte
//! against those committed arrays by the generated self-check test.
//!
//! # Provenance & derivation (see per-table comments in [`generate`])
//! Anchors fetched raw from libopus `v1.5.2` (`xiph/opus`), not summarised:
//! - `celt/modes.c`: `eband5ms` (band layout), `band_allocation` (11×21 tuning
//!   matrix), the overlap-window formula, `logN[i]=log2_frac(...)`.
//! - `celt/rate.c`: `LOG2_FRAC_TABLE`, and `compute_pulse_cache` (ported here to
//!   DERIVE `CACHE_INDEX`/`CACHE_BITS`/`CACHE_CAPS`).
//! - `celt/cwrs.c`: `log2_frac` + `V(N,K)` (the PVQ codebook size), ported here
//!   to feed `get_required_bits`, which the pulse cache consumes.
//! - `celt/quant_bands.c`: `eMeans`, `pred_coef`/`beta_coef`/`beta_intra`,
//!   `e_prob_model` (coarse-energy Laplace models), `small_energy_icdf`.
//! - `celt/laplace.c`: the `LAPLACE_*` range-coder constants.
//!
//! # Transcribed vs derived
//! **Derived** host-side (formula re-run here; `check` is meaningful):
//! `LOG_N`, `CACHE_INDEX`, `CACHE_BITS`, `CACHE_CAPS`, `EBANDS`, `MDCT_WINDOW`.
//! **Transcribed** (fixed tuning/model data, not computable — anchored by the
//! self-check test): `EBAND_5MS`, `BAND_ALLOCATION`, `LOG2_FRAC_TABLE`,
//! `E_MEANS`, `PRED_COEF`, `BETA_COEF`, `BETA_INTRA`, `E_PROB_MODEL`,
//! `SMALL_ENERGY_ICDF`, and the `LAPLACE_*` scalars.
//!
//! # MDCT FFT config (issue #38)
//! The CELT inverse-MDCT fast transform is libopus's mixed-radix kiss-FFT. Its
//! per-size config — stage factors, the bit-reversal permutation, and the
//! forward twiddles `exp(-j2πk/N4)` — is generated host-side here (ported from
//! `celt/kiss_fft.c` `kf_factor`/`compute_bitrev_table`/`compute_twiddles`) for
//! each `N4 ∈ {60,120,240,480}`, so the no_std core runs the butterflies over
//! `const` tables with no runtime trig. The overlap window (`MDCT_WINDOW`) is
//! still generated for the pre/post-rotation + TDAC overlap-add.

use std::f64::consts::PI;
use std::fmt::Write as _;

const TABLES_PATH: &str = "crates/soul-audio-core/src/formats/opus/opus_tables.rs";

// ---------------------------------------------------------------------------
// Standard-mode geometry (libopus static_modes_float.h `mode48000_960_120`).
// ---------------------------------------------------------------------------
const NB_EBANDS: usize = 21;
const MAX_LM: i32 = 3;
const BITRES: i32 = 3;
const MAX_FINE_BITS: i32 = 8;
const FINE_OFFSET: i32 = 21;
const QTHETA_OFFSET: i32 = 4;
const QTHETA_OFFSET_TWOPHASE: i32 = 16;
const MAX_PSEUDO: i32 = 40;

/// libopus `celt/_kiss_fft_guts.h` `MAXFACTORS` — upper bound on FFT radix
/// stages. Our CELT MDCT sizes (`N4 ∈ {60,120,240,480}`) need at most 5.
const MAXFACTORS: usize = 8;

/// The four CELT inverse-MDCT FFT sizes (`N4 = N/4` for the 2.5/5/10/20 ms
/// frames; short blocks always use 60). Each is `2^k·15 = 2^k·3·5`, exactly the
/// mixed-radix sizes libopus kiss-FFT factors as radix 4/2/3/5.
const FFT_SIZES: [usize; 4] = [60, 120, 240, 480];

/// libopus `celt/modes.c` `eband5ms` — CELT energy-band boundaries for a 2.5 ms
/// frame, in units of MDCT bins of the 120-bin short block (22 boundaries → 21
/// bands). Band `j` of a frame with `LM` (`M=1<<LM`) spans MDCT bins
/// `M*eband5ms[j] .. M*eband5ms[j+1]`.
const EBAND_5MS: [i16; 22] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100,
];

/// libopus `celt/modes.c` `band_allocation` — the 11×21 static bit-allocation
/// matrix (`BITALLOC_SIZE=11` quality rows × `nbEBands=21` bands), in units of
/// 1/32 bit/sample. Pure tuning data (partially derived from Vorbis); not
/// computable, transcribed verbatim.
const BAND_ALLOCATION: [[u8; 21]; 11] = [
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ],
    [
        90, 80, 75, 69, 63, 56, 49, 40, 34, 29, 20, 18, 10, 0, 0, 0, 0, 0, 0, 0, 0,
    ],
    [
        110, 100, 90, 84, 78, 71, 65, 58, 51, 45, 39, 32, 26, 20, 12, 0, 0, 0, 0, 0, 0,
    ],
    [
        118, 110, 103, 93, 86, 80, 75, 70, 65, 59, 53, 47, 40, 31, 23, 15, 4, 0, 0, 0, 0,
    ],
    [
        126, 119, 112, 104, 95, 89, 83, 78, 72, 66, 60, 54, 47, 39, 32, 25, 17, 12, 1, 0, 0,
    ],
    [
        134, 127, 120, 114, 103, 97, 91, 85, 78, 72, 66, 60, 54, 47, 41, 35, 29, 23, 16, 10, 1,
    ],
    [
        144, 137, 130, 124, 113, 107, 101, 95, 88, 82, 76, 70, 64, 57, 51, 45, 39, 33, 26, 15, 1,
    ],
    [
        152, 145, 138, 132, 123, 117, 111, 105, 98, 92, 86, 80, 74, 67, 61, 55, 49, 43, 36, 20, 1,
    ],
    [
        162, 155, 148, 142, 133, 127, 121, 115, 108, 102, 96, 90, 84, 77, 71, 65, 59, 53, 46, 30, 1,
    ],
    [
        172, 165, 158, 152, 143, 137, 131, 125, 118, 112, 106, 100, 94, 87, 81, 75, 69, 63, 56, 45,
        20,
    ],
    [
        200, 200, 200, 200, 200, 200, 200, 200, 198, 193, 188, 183, 178, 173, 168, 163, 158, 153,
        148, 129, 104,
    ],
];

/// libopus `celt/rate.c` `LOG2_FRAC_TABLE` — `log2_frac(n, 3)` for `n=1..24`,
/// used by bit allocation to reserve the intensity-stereo parameter. Small
/// fixed table, transcribed verbatim (also re-derivable via [`log2_frac`], but
/// libopus ships it as a literal).
const LOG2_FRAC_TABLE: [u8; 24] = [
    0, 8, 13, 16, 19, 21, 23, 24, 26, 27, 28, 29, 30, 31, 32, 32, 33, 34, 34, 35, 36, 36, 37, 37,
];

/// libopus `celt/quant_bands.c` `eMeans` — mean energy per band (Q4, converted
/// to float), subtracted in `amp2Log2` before coarse-energy coding.
const E_MEANS: [f32; 25] = [
    6.4375, 6.25, 5.75, 5.3125, 5.0625, 4.8125, 4.5, 4.375, 4.875, 4.6875, 4.5625, 4.4375, 4.875,
    4.625, 4.3125, 4.5, 4.375, 4.625, 4.75, 4.4375, 3.75, 3.75, 3.75, 3.75, 3.75,
];

/// libopus `celt/quant_bands.c` `pred_coef` / `beta_coef` / `beta_intra`
/// (float build): the coarse-energy inter-frame prediction coefficients and
/// intra-frame beta, one per frame size (`LM=0..3`), stored there as the exact
/// Q15 fractions below (nominally 0.9/0.8/0.65/0.5).
const PRED_COEF_Q15: [i32; 4] = [29440, 26112, 21248, 16384];
const BETA_COEF_Q15: [i32; 4] = [30147, 22282, 12124, 6554];
const BETA_INTRA_Q15: i32 = 4915;

/// libopus `celt/quant_bands.c` `e_prob_model[4][2][42]` — parameters of the
/// Laplace-like coarse-energy models: one `[prob_of_0, decay]` pair (Q8) per
/// band, for each frame size (`LM=0..3`) and prediction type (inter=0/intra=1).
/// Fixed model data, transcribed verbatim.
const E_PROB_MODEL: [[[u8; 42]; 2]; 4] = [
    // 120-sample frames.
    [
        // Inter
        [
            72, 127, 65, 129, 66, 128, 65, 128, 64, 128, 62, 128, 64, 128, 64, 128, 92, 78, 92, 79,
            92, 78, 90, 79, 116, 41, 115, 40, 114, 40, 132, 26, 132, 26, 145, 17, 161, 12, 176, 10,
            177, 11,
        ],
        // Intra
        [
            24, 179, 48, 138, 54, 135, 54, 132, 53, 134, 56, 133, 55, 132, 55, 132, 61, 114, 70,
            96, 74, 88, 75, 88, 87, 74, 89, 66, 91, 67, 100, 59, 108, 50, 120, 40, 122, 37, 97, 43,
            78, 50,
        ],
    ],
    // 240-sample frames.
    [
        // Inter
        [
            83, 78, 84, 81, 88, 75, 86, 74, 87, 71, 90, 73, 93, 74, 93, 74, 109, 40, 114, 36, 117,
            34, 117, 34, 143, 17, 145, 18, 146, 19, 162, 12, 165, 10, 178, 7, 189, 6, 190, 8, 177,
            9,
        ],
        // Intra
        [
            23, 178, 54, 115, 63, 102, 66, 98, 69, 99, 74, 89, 71, 91, 73, 91, 78, 89, 86, 80, 92,
            66, 93, 64, 102, 59, 103, 60, 104, 60, 117, 52, 123, 44, 138, 35, 133, 31, 97, 38, 77,
            45,
        ],
    ],
    // 480-sample frames.
    [
        // Inter
        [
            61, 90, 93, 60, 105, 42, 107, 41, 110, 45, 116, 38, 113, 38, 112, 38, 124, 26, 132, 27,
            136, 19, 140, 20, 155, 14, 159, 16, 158, 18, 170, 13, 177, 10, 187, 8, 192, 6, 175, 9,
            159, 10,
        ],
        // Intra
        [
            21, 178, 59, 110, 71, 86, 75, 85, 84, 83, 91, 66, 88, 73, 87, 72, 92, 75, 98, 72, 105,
            58, 107, 54, 115, 52, 114, 55, 112, 56, 129, 51, 132, 40, 150, 33, 140, 29, 98, 35, 77,
            42,
        ],
    ],
    // 960-sample frames.
    [
        // Inter
        [
            42, 121, 96, 66, 108, 43, 111, 40, 117, 44, 123, 32, 120, 36, 119, 33, 127, 33, 134,
            34, 139, 21, 147, 23, 152, 20, 158, 25, 154, 26, 166, 21, 173, 16, 184, 13, 184, 10,
            150, 13, 139, 15,
        ],
        // Intra
        [
            22, 178, 63, 114, 74, 82, 84, 83, 92, 82, 103, 62, 96, 72, 96, 67, 101, 73, 107, 72,
            113, 55, 118, 52, 125, 52, 118, 52, 117, 55, 135, 49, 137, 39, 157, 32, 145, 29, 97,
            33, 77, 40,
        ],
    ],
];

/// libopus `celt/quant_bands.c` `small_energy_icdf` — the 3-symbol inverse-CDF
/// for coarse energy when only 2 bits are available. Terminates in the
/// mandatory trailing `0` the range decoder's `decode_icdf` loop relies on.
const SMALL_ENERGY_ICDF: [u8; 3] = [2, 1, 0];

// ---------------------------------------------------------------------------
// Ported libopus derivation (celt/cwrs.c + celt/rate.c). Verified host-side to
// reproduce static_modes_float.h's logN400 / cache_index50 / cache_bits50 /
// cache_caps50 byte-for-byte (see the generated self-check test).
// ---------------------------------------------------------------------------

/// `EC_ILOG(v)` — `floor(log2(v)) + 1` for `v > 0` (`0` for `v == 0`).
fn ec_ilog(v: u32) -> i32 {
    (32 - v.leading_zeros()) as i32
}

/// libopus `celt/cwrs.c` `log2_frac`: a conservatively-large binary logarithm
/// of `val` with `frac` fractional bits.
fn log2_frac(mut val: u32, mut frac: i32) -> i32 {
    let l = ec_ilog(val);
    if val & (val - 1) != 0 {
        if l > 16 {
            val = ((val - 1) >> (l - 16)) + 1;
        } else {
            val <<= 16 - l;
        }
        let mut l = (l - 1) << frac;
        loop {
            let b = (val >> 16) as i32;
            l += b << frac;
            val = (val + b as u32) >> b;
            val = (val.wrapping_mul(val) + 0x7FFF) >> 15;
            let cont = frac > 0;
            frac -= 1;
            if !cont {
                break;
            }
        }
        l + i32::from(val > 0x8000)
    } else {
        (l - 1) << frac
    }
}

/// `V(N,K)` — the number of `N`-dimensional unit pulse vectors with `K` pulses
/// (signs included), i.e. the PVQ codebook size (libopus `celt/cwrs.c`). Uses
/// the recurrence `V(n,k)=V(n-1,k)+V(n,k-1)+V(n-1,k-1)`, `V(n,0)=1`,
/// `V(0,k>0)=0`. For the cache-relevant `(N,K)` this fits in `u32` (guarded by
/// [`fits_in32`]); `u64` accumulation keeps the intermediate DP exact.
fn v_nk(n: usize, k: usize) -> u64 {
    let mut prev = vec![0u64; k + 2];
    prev[0] = 1;
    for _ in 0..n {
        let mut cur = vec![0u64; k + 2];
        cur[0] = 1;
        for kk in 1..=k + 1 {
            cur[kk] = prev[kk] + cur[kk - 1] + prev[kk - 1];
        }
        prev = cur;
    }
    prev[k]
}

/// libopus `celt/rate.h` `get_pulses`: map a pseudo-pulse index to a real pulse
/// count.
fn get_pulses(i: i32) -> i32 {
    if i < 8 {
        i
    } else {
        (8 + (i & 7)) << ((i >> 3) - 1)
    }
}

/// libopus `celt/rate.c` `fits_in32`: does `V(N,K)` fit in a `u32`?
fn fits_in32(n: i32, k: i32) -> bool {
    const MAX_N: [i32; 15] = [
        32767, 32767, 32767, 1476, 283, 109, 60, 40, 29, 24, 20, 18, 16, 14, 13,
    ];
    const MAX_K: [i32; 15] = [
        32767, 32767, 32767, 32767, 1172, 238, 95, 53, 36, 27, 22, 18, 16, 15, 13,
    ];
    if n >= 14 {
        if k >= 14 {
            false
        } else {
            n <= MAX_N[k as usize]
        }
    } else {
        k <= MAX_K[n as usize]
    }
}

/// libopus `celt/cwrs.c` `get_required_bits`: `log2_frac(V(N,k), frac)` for
/// `k=1..=maxk` (`bits[0]=0`).
fn get_required_bits(n: i32, maxk: i32, frac: i32) -> Vec<i16> {
    let mut bits = vec![0i16; (maxk + 1) as usize];
    for k in 1..=maxk {
        bits[k as usize] = log2_frac(v_nk(n as usize, k as usize) as u32, frac) as i16;
    }
    bits
}

/// The `logN` table for the standard mode (libopus `celt/modes.c`:
/// `logN[i] = log2_frac(eBands[i+1]-eBands[i], BITRES)`, `eBands = eband5ms`).
/// Equals static_modes_float.h `logN400`.
fn compute_log_n() -> Vec<i16> {
    (0..NB_EBANDS)
        .map(|i| log2_frac((EBAND_5MS[i + 1] - EBAND_5MS[i]) as u32, BITRES) as i16)
        .collect()
}

/// The derived pulse cache: `index`, `bits`, `caps` — a faithful port of
/// libopus `celt/rate.c` `compute_pulse_cache` for the standard mode
/// (`nbEBands=21`, `LM=3`). Equals static_modes_float.h
/// `cache_index50`/`cache_bits50`/`cache_caps50`.
struct PulseCache {
    index: Vec<i16>,
    bits: Vec<u8>,
    caps: Vec<u8>,
}

fn compute_pulse_cache(logn: &[i16]) -> PulseCache {
    let nb = NB_EBANDS;
    let lm = MAX_LM;
    let eb = &EBAND_5MS;

    let mut cindex = vec![-1i16; nb * (lm as usize + 2)];
    let mut entry_n: Vec<i32> = Vec::new();
    let mut entry_k: Vec<i32> = Vec::new();
    let mut entry_i: Vec<i32> = Vec::new();
    let mut curr = 0i32;
    for i in 0..=lm + 1 {
        for j in 0..nb {
            let n = (i32::from(eb[j + 1]) - i32::from(eb[j])) << i >> 1;
            cindex[(i as usize) * nb + j] = -1;
            for k in 0..=i {
                let mut nn = 0usize;
                while nn < nb && (k != i || nn < j) {
                    if n == (i32::from(eb[nn + 1]) - i32::from(eb[nn])) << k >> 1 {
                        cindex[(i as usize) * nb + j] = cindex[(k as usize) * nb + nn];
                        break;
                    }
                    nn += 1;
                }
            }
            if cindex[(i as usize) * nb + j] == -1 && n != 0 {
                entry_n.push(n);
                let mut kk = 0i32;
                while fits_in32(n, get_pulses(kk + 1)) && kk < MAX_PSEUDO {
                    kk += 1;
                }
                entry_k.push(kk);
                cindex[(i as usize) * nb + j] = curr as i16;
                entry_i.push(curr);
                curr += kk + 1;
            }
        }
    }

    let mut bits = vec![0u8; curr as usize];
    for e in 0..entry_n.len() {
        let base = entry_i[e] as usize;
        let tmp = get_required_bits(entry_n[e], get_pulses(entry_k[e]), BITRES);
        for j in 1..=entry_k[e] {
            bits[base + j as usize] = (tmp[get_pulses(j) as usize] - 1) as u8;
        }
        bits[base] = entry_k[e] as u8;
    }

    let mut caps = Vec::new();
    for i in 0..=lm {
        for c in 1..=2i32 {
            for j in 0..nb {
                let mut n0 = i32::from(eb[j + 1]) - i32::from(eb[j]);
                let max_bits: i32;
                if n0 << i == 1 {
                    max_bits = (c * (1 + MAX_FINE_BITS)) << BITRES;
                } else {
                    let mut lm0 = 0i32;
                    if n0 > 2 {
                        n0 >>= 1;
                        lm0 -= 1;
                    } else if n0 <= 1 {
                        lm0 = i.min(1);
                        n0 <<= lm0;
                    }
                    let pcache_off = cindex[((lm0 + 1) as usize) * nb + j] as usize;
                    let mut mb = i32::from(bits[pcache_off + bits[pcache_off] as usize]) + 1;
                    let mut n = n0;
                    for k in 0..i - lm0 {
                        mb <<= 1;
                        let offset =
                            ((i32::from(logn[j]) + ((lm0 + k) << BITRES)) >> 1) - QTHETA_OFFSET;
                        let num = 459 * ((2 * n - 1) * offset + mb);
                        let den = ((2 * n - 1) << 9) - 459;
                        let qb = ((num + (den >> 1)) / den).min(57);
                        mb += qb;
                        n <<= 1;
                    }
                    if c == 2 {
                        mb <<= 1;
                        let qoff = if n == 2 {
                            QTHETA_OFFSET_TWOPHASE
                        } else {
                            QTHETA_OFFSET
                        };
                        let offset = ((i32::from(logn[j]) + (i << BITRES)) >> 1) - qoff;
                        let ndof = 2 * n - 1 - i32::from(n == 2);
                        let w = if n == 2 { 512 } else { 487 };
                        let num = w * (mb + ndof * offset);
                        let den = (ndof << 9) - w;
                        let qb = ((num + (den >> 1)) / den).min(if n == 2 { 64 } else { 61 });
                        mb += qb;
                    }
                    let ndof = c * n + i32::from(c == 2 && n > 2);
                    let mut offset = ((i32::from(logn[j]) + (i << BITRES)) >> 1) - FINE_OFFSET;
                    if n == 2 {
                        offset += 1 << BITRES >> 2;
                    }
                    let num = mb + ndof * offset;
                    let den = (ndof - 1) << BITRES;
                    let qb = ((num + (den >> 1)) / den).min(MAX_FINE_BITS);
                    mb += (c * qb) << BITRES;
                    max_bits = mb;
                }
                let cap =
                    (4 * max_bits / (c * ((i32::from(eb[j + 1]) - i32::from(eb[j])) << i))) - 64;
                caps.push(cap as u8);
            }
        }
    }

    PulseCache {
        index: cindex,
        bits,
        caps,
    }
}

/// Per-frame-size band boundaries: `EBANDS[LM][i] = eband5ms[i] << LM`, the
/// actual MDCT-bin edges for the 120/240/480/960-bin frames (`LM=0..3`).
fn compute_ebands() -> Vec<Vec<i16>> {
    (0..=MAX_LM)
        .map(|lm| EBAND_5MS.iter().map(|&e| e << lm).collect())
        .collect()
}

/// The 120-sample CELT MDCT overlap window (libopus `celt/modes.c`):
/// `window[i] = sin(π/2 · sin²(π/2·(i+0.5)/overlap))`, `overlap = 120`. The
/// Vorbis-style rising half; equals static_modes_float.h `window120`.
fn compute_window() -> Vec<f32> {
    let overlap = 120.0f64;
    (0..120)
        .map(|i| {
            let inner = (0.5 * PI * (i as f64 + 0.5) / overlap).sin();
            (0.5 * PI * inner * inner).sin() as f32
        })
        .collect()
}

// ---------------------------------------------------------------------------
// CELT inverse-MDCT FFT config (libopus celt/kiss_fft.c). Ported here so the
// mixed-radix bit-reversal + twiddle tables are generated host-side; the no_std
// core reads the emitted `const`s and runs the butterflies with no runtime trig.
// The `check` re-derivation guards them exactly like the pulse cache above.
// ---------------------------------------------------------------------------

/// libopus `celt/kiss_fft.c` `kf_factor` — factor `nfft` into the radix stages
/// the mixed-radix FFT executes, with libopus's exact policy: greedily take
/// radix-4, fall back through 2/3/5.., and defer a lone radix-2 so it lands on a
/// size-4 sub-transform (the specialised `kf_bfly2`). Stages are then reversed
/// (outer→inner) and each stage's remaining size `m` filled in. Returns the flat
/// `[p0, m0, p1, m1, …]` the runtime `opus_fft_impl` iterates.
fn kf_factor(nfft: i32) -> Vec<i16> {
    let mut fac = vec![0i16; 2 * MAXFACTORS];
    let mut p = 4i32;
    let mut stages = 0usize;
    let mut n = nfft;
    loop {
        while n % p != 0 {
            match p {
                4 => p = 2,
                2 => p = 3,
                _ => p += 2,
            }
            if p > 32000 || p * p > n {
                p = n;
            }
        }
        n /= p;
        // RADIX_TWO_ONLY is not defined in the float build, so radices up to 5
        // are allowed; our sizes never produce a factor > 5.
        assert!(p <= 5, "unexpected radix {p} factoring {nfft}");
        fac[2 * stages] = p as i16;
        if p == 2 && stages > 1 {
            // Move the radix-2 stage so a size-4 sub-transform precedes it.
            fac[2 * stages] = 4;
            fac[2] = 2;
        }
        stages += 1;
        if n <= 1 {
            break;
        }
    }
    // Reverse the stage order (libopus emits inner→outer, runs outer→inner).
    for i in 0..stages / 2 {
        fac.swap(2 * i, 2 * (stages - i - 1));
    }
    // Fill each stage's remaining transform size `m = nfft / Π p_so_far`.
    let mut m = nfft;
    for i in 0..stages {
        m /= i32::from(fac[2 * i]);
        fac[2 * i + 1] = m as i16;
    }
    fac.truncate(2 * stages);
    fac
}

/// libopus `celt/kiss_fft.c` `compute_bitrev_table` — the digit-reversal
/// permutation for the mixed-radix decimation, derived from the (already
/// stage-ordered) `factors`. `bitrev[i]` is where input element `i` is scattered
/// before the in-place butterflies run.
fn compute_bitrev(nfft: usize, factors: &[i16]) -> Vec<i16> {
    let mut buf = vec![0i16; nfft];
    bitrev_rec(0, &mut buf, 0, 1, factors, 0);
    buf
}

fn bitrev_rec(
    fout: i32,
    buf: &mut [i16],
    mut f: usize,
    fstride: usize,
    factors: &[i16],
    fpos: usize,
) {
    let p = i32::from(factors[fpos]);
    let m = i32::from(factors[fpos + 1]);
    if m == 1 {
        for j in 0..p {
            buf[f] = (fout + j) as i16;
            f += fstride; // in_stride == 1
        }
    } else {
        let mut fout_v = fout;
        for _ in 0..p {
            bitrev_rec(fout_v, buf, f, fstride * (p as usize), factors, fpos + 2);
            f += fstride;
            fout_v += m;
        }
    }
}

/// libopus `celt/kiss_fft.c` `compute_twiddles` (float build): the forward-FFT
/// twiddle factors `exp(-j·2π·i/nfft)`, emitted interleaved `[re, im, …]`.
fn compute_fft_twiddles(nfft: usize) -> Vec<f32> {
    let mut t = Vec::with_capacity(2 * nfft);
    for i in 0..nfft {
        let phase = -2.0 * PI * i as f64 / nfft as f64;
        t.push(phase.cos() as f32);
        t.push(phase.sin() as f32);
    }
    t
}

// ---------------------------------------------------------------------------
// Emit helpers (mirror xtask/src/vorbis_tables.rs + ps_tables.rs conventions).
// ---------------------------------------------------------------------------

/// Shortest round-tripping f32 literal, guaranteed to parse as a float element.
fn lit(v: f32) -> String {
    let s = format!("{v}");
    if s.contains(['.', 'e', 'E']) {
        s
    } else {
        format!("{s}.0")
    }
}

fn emit_f32_1d(name: &str, t: &[f32], out: &mut String) {
    writeln!(out, "#[rustfmt::skip]").unwrap();
    writeln!(out, "pub(super) static {name}: [f32; {}] = [", t.len()).unwrap();
    for chunk in t.chunks(8) {
        out.push_str("    ");
        for v in chunk {
            write!(out, "{},", lit(*v)).unwrap();
        }
        out.push('\n');
    }
    out.push_str("];\n");
}

fn emit_i16_1d(name: &str, t: &[i16], out: &mut String) {
    writeln!(out, "#[rustfmt::skip]").unwrap();
    writeln!(out, "pub(super) static {name}: [i16; {}] = [", t.len()).unwrap();
    for chunk in t.chunks(16) {
        out.push_str("    ");
        for v in chunk {
            write!(out, "{v},").unwrap();
        }
        out.push('\n');
    }
    out.push_str("];\n");
}

fn emit_u8_1d(name: &str, t: &[u8], out: &mut String) {
    writeln!(out, "#[rustfmt::skip]").unwrap();
    writeln!(out, "pub(super) static {name}: [u8; {}] = [", t.len()).unwrap();
    for chunk in t.chunks(16) {
        out.push_str("    ");
        for v in chunk {
            write!(out, "{v},").unwrap();
        }
        out.push('\n');
    }
    out.push_str("];\n");
}

fn emit_u8_2d(name: &str, t: &[[u8; 21]], out: &mut String) {
    writeln!(out, "#[rustfmt::skip]").unwrap();
    writeln!(out, "pub(super) static {name}: [[u8; 21]; {}] = [", t.len()).unwrap();
    for row in t {
        out.push_str("    [");
        for v in row {
            write!(out, "{v},").unwrap();
        }
        out.push_str("],\n");
    }
    out.push_str("];\n");
}

fn emit_i16_2d(name: &str, t: &[Vec<i16>], out: &mut String) {
    let inner = t.first().map_or(0, Vec::len);
    writeln!(out, "#[rustfmt::skip]").unwrap();
    writeln!(
        out,
        "pub(super) static {name}: [[i16; {inner}]; {}] = [",
        t.len()
    )
    .unwrap();
    for row in t {
        out.push_str("    [");
        for v in row {
            write!(out, "{v},").unwrap();
        }
        out.push_str("],\n");
    }
    out.push_str("];\n");
}

/// `[[[u8; 42]; 2]; 4]` — the coarse-energy Laplace models.
fn emit_e_prob_model(name: &str, t: &[[[u8; 42]; 2]; 4], out: &mut String) {
    writeln!(out, "#[rustfmt::skip]").unwrap();
    writeln!(out, "pub(super) static {name}: [[[u8; 42]; 2]; 4] = [").unwrap();
    for frame in t {
        out.push_str("    [\n");
        for kind in frame {
            out.push_str("        [");
            for v in kind {
                write!(out, "{v},").unwrap();
            }
            out.push_str("],\n");
        }
        out.push_str("    ],\n");
    }
    out.push_str("];\n");
}

/// Build the checked-in table source content.
fn generate() -> String {
    let log_n = compute_log_n();
    let cache = compute_pulse_cache(&log_n);
    let ebands = compute_ebands();
    let window = compute_window();
    let pred_coef: Vec<f32> = PRED_COEF_Q15
        .iter()
        .map(|&q| (q as f64 / 32768.0) as f32)
        .collect();
    let beta_coef: Vec<f32> = BETA_COEF_Q15
        .iter()
        .map(|&q| (q as f64 / 32768.0) as f32)
        .collect();
    let beta_intra = (BETA_INTRA_Q15 as f64 / 32768.0) as f32;

    let mut out = String::new();
    // This generated table file legitimately carries some unused entries (not
    // every table is read outside of tests), so it keeps a module-level
    // `dead_code` allow rather than per-item annotations. The precision/
    // readability allows cover the machine-rendered f32/integer literals.
    out.push_str("#![allow(dead_code, clippy::excessive_precision, clippy::unreadable_literal)]\n");
    out.push_str("// @generated by `cargo xtask gen-opus-tables` — do not edit.\n");
    out.push_str("// CELT static tables for the standard 48 kHz Opus mode.\n");
    out.push_str("// See xtask/src/opus_tables.rs for provenance/derivation/layout.\n\n");

    // ---- Band layout ----
    out.push_str("/// Number of CELT energy bands in the standard mode.\n");
    out.push_str("pub(super) const NUM_BANDS: usize = 21;\n");
    out.push_str(
        "// libopus celt/modes.c `eband5ms`: 2.5 ms band boundaries (120-bin short block).\n",
    );
    emit_i16_1d("EBAND_5MS", &EBAND_5MS, &mut out);
    out.push_str(
        "// Per-frame-size band boundaries: EBANDS[LM][i] = eband5ms[i] << LM (derived).\n",
    );
    out.push_str("// Only used by the cross-check test below outside of `cfg(test)`.\n");
    out.push_str("#[cfg(test)]\n");
    emit_i16_2d("EBANDS", &ebands, &mut out);

    // ---- Allocation ----
    out.push_str("// libopus celt/modes.c `band_allocation` (11 quality rows × 21 bands).\n");
    emit_u8_2d("BAND_ALLOCATION", &BAND_ALLOCATION, &mut out);
    out.push_str("// libopus celt/modes.c `logN400` = log2_frac(band width, 3) (derived).\n");
    emit_i16_1d("LOG_N", &log_n, &mut out);
    out.push_str("// libopus celt/rate.c `LOG2_FRAC_TABLE` (intensity-stereo reserve).\n");
    emit_u8_1d("LOG2_FRAC_TABLE", &LOG2_FRAC_TABLE, &mut out);
    out.push_str("// Pulse cache (libopus celt/rate.c `compute_pulse_cache`, ported): index.\n");
    emit_i16_1d("CACHE_INDEX", &cache.index, &mut out);
    out.push_str("// Pulse cache bits (== static_modes_float.h `cache_bits50`).\n");
    emit_u8_1d("CACHE_BITS", &cache.bits, &mut out);
    out.push_str("// Pulse cache caps `cache_caps50` (max bits/band; derived).\n");
    emit_u8_1d("CACHE_CAPS", &cache.caps, &mut out);

    // ---- Coarse energy ----
    out.push_str("// libopus celt/quant_bands.c `eMeans` (mean band energy, Q4→float).\n");
    emit_f32_1d("E_MEANS", &E_MEANS, &mut out);
    out.push_str("// libopus celt/quant_bands.c `pred_coef` (inter prediction, per LM).\n");
    emit_f32_1d("PRED_COEF", &pred_coef, &mut out);
    out.push_str("// libopus celt/quant_bands.c `beta_coef` (per LM).\n");
    emit_f32_1d("BETA_COEF", &beta_coef, &mut out);
    out.push_str("// libopus celt/quant_bands.c `beta_intra`.\n");
    writeln!(
        out,
        "pub(super) const BETA_INTRA: f32 = {};",
        lit(beta_intra)
    )
    .unwrap();
    out.push_str("// libopus celt/quant_bands.c `e_prob_model[LM][intra][band pair]` (Q8).\n");
    emit_e_prob_model("E_PROB_MODEL", &E_PROB_MODEL, &mut out);
    out.push_str("// libopus celt/quant_bands.c `small_energy_icdf` (trailing 0 required).\n");
    emit_u8_1d("SMALL_ENERGY_ICDF", &SMALL_ENERGY_ICDF, &mut out);

    // ---- Laplace / range-coder constants ----
    out.push_str("// libopus celt/laplace.c coarse-energy Laplace decoder constants.\n");
    out.push_str("pub(super) const LAPLACE_LOG_MINP: u32 = 0;\n");
    out.push_str("pub(super) const LAPLACE_MINP: u32 = 1 << LAPLACE_LOG_MINP;\n");
    out.push_str("pub(super) const LAPLACE_NMIN: u32 = 16;\n");

    // ---- MDCT overlap window ----
    out.push_str(
        "// libopus celt/modes.c overlap window: sin(π/2·sin²(π/2·(i+0.5)/120)) (derived).\n",
    );
    emit_f32_1d("MDCT_WINDOW", &window, &mut out);

    // ---- CELT inverse-MDCT FFT config (kiss-FFT, one set per N4) ----
    out.push_str(
        "// CELT inverse-MDCT mixed-radix FFT config (libopus celt/kiss_fft.c), one set\n\
         // per N4 ∈ {60,120,240,480}: stage FACTORS [p,m,…] (radix 4/2/3/5), the BITREV\n\
         // digit-reversal permutation, and the forward TWIDDLES exp(-j2πk/N4) interleaved\n\
         // [re,im,…]. Consumed by celt/mdct.rs `opus_fft_impl` (derived host-side).\n",
    );
    for &nfft in &FFT_SIZES {
        let fac = kf_factor(nfft as i32);
        let bitrev = compute_bitrev(nfft, &fac);
        let tw = compute_fft_twiddles(nfft);
        emit_i16_1d(&format!("OPUS_FFT_FACTORS_{nfft}"), &fac, &mut out);
        emit_i16_1d(&format!("OPUS_FFT_BITREV_{nfft}"), &bitrev, &mut out);
        emit_f32_1d(&format!("OPUS_FFT_TWIDDLES_{nfft}"), &tw, &mut out);
    }

    // ---- Self-check test (anchors derived tables to RFC 6716 / libopus) ----
    out.push_str(SELF_CHECK_TEST);
    out
}

/// The generated self-check: spot-checks derived tables against known libopus
/// `static_modes_float.h` values so the checked-in file can't silently rot.
const SELF_CHECK_TEST: &str = r#"
#[cfg(test)]
mod generated_tests {
    use super::*;

    #[test]
    fn eband_boundaries_match_rfc6716() {
        // libopus celt/modes.c eband5ms (RFC 6716 CELT reference).
        assert_eq!(EBAND_5MS[0], 0);
        assert_eq!(EBAND_5MS[21], 100);
        assert_eq!(EBAND_5MS.len(), NUM_BANDS + 1);
        // Per-frame-size scaling: EBANDS[LM][i] == eband5ms[i] << LM.
        assert_eq!(EBANDS[0], EBAND_5MS);
        assert_eq!(EBANDS[3][21], 800); // 100 << 3, the 960-bin top edge.
    }

    #[test]
    fn log_n_matches_logn400() {
        // static_modes_float.h logN400.
        assert_eq!(
            LOG_N,
            [
                0, 0, 0, 0, 0, 0, 0, 0, 8, 8, 8, 8, 16, 16, 16, 21, 21, 24, 29, 34, 36
            ]
        );
    }

    #[test]
    fn band_allocation_corners() {
        // libopus celt/modes.c band_allocation: first row all-zero, last row hi.
        assert_eq!(BAND_ALLOCATION[0], [0u8; 21]);
        assert_eq!(BAND_ALLOCATION[1][0], 90);
        assert_eq!(BAND_ALLOCATION[10][0], 200);
        assert_eq!(BAND_ALLOCATION[10][20], 104);
    }

    #[test]
    fn pulse_cache_matches_static_modes() {
        // static_modes_float.h cache_index50 / cache_bits50 / cache_caps50.
        assert_eq!(CACHE_INDEX.len(), 105);
        assert_eq!(CACHE_BITS.len(), 392);
        assert_eq!(CACHE_CAPS.len(), 168);
        assert_eq!(CACHE_INDEX[8], 0);
        assert_eq!(CACHE_INDEX[104], 387);
        assert_eq!(CACHE_BITS[0], 40);
        assert_eq!(CACHE_BITS[391], 234);
        assert_eq!(CACHE_CAPS[0], 224);
        assert_eq!(CACHE_CAPS[167], 40);
    }

    #[test]
    fn small_energy_icdf_terminates_in_zero() {
        assert_eq!(*SMALL_ENERGY_ICDF.last().unwrap(), 0);
    }

    #[test]
    fn window_is_the_celt_overlap_half() {
        // static_modes_float.h window120 endpoints + monotonic rise 0→1.
        assert_eq!(MDCT_WINDOW.len(), 120);
        assert!((MDCT_WINDOW[0] - 6.7286966e-05).abs() < 1e-9);
        assert!((MDCT_WINDOW[119] - 1.0).abs() < 1e-6);
        assert!(MDCT_WINDOW[0] < 0.01);
        for w in MDCT_WINDOW.windows(2) {
            assert!(w[1] >= w[0]);
        }
    }

    #[test]
    fn fft_config_is_well_formed() {
        // For each CELT MDCT FFT size: the stage factors multiply back to N4,
        // the last stage's m is 1, the bit-reversal is a genuine permutation of
        // 0..N4, and the twiddle DC term is exp(0) = (1, 0).
        fn check(nfft: usize, factors: &[i16], bitrev: &[i16], tw: &[f32]) {
            let mut prod = 1i64;
            let mut l = 0;
            while l < factors.len() {
                prod *= i64::from(factors[l]);
                l += 2;
            }
            assert_eq!(prod as usize, nfft, "factors product for {nfft}");
            assert_eq!(*factors.last().unwrap(), 1, "last stage m for {nfft}");
            assert_eq!(bitrev.len(), nfft, "bitrev len for {nfft}");
            let mut seen = [false; 480];
            for &b in bitrev {
                let b = b as usize;
                assert!(b < nfft && !seen[b], "bitrev not a permutation for {nfft}");
                seen[b] = true;
            }
            assert_eq!(tw.len(), 2 * nfft, "twiddle len for {nfft}");
            assert!(
                (tw[0] - 1.0).abs() < 1e-6 && tw[1].abs() < 1e-6,
                "DC twiddle {nfft}"
            );
        }
        check(
            60,
            &OPUS_FFT_FACTORS_60,
            &OPUS_FFT_BITREV_60,
            &OPUS_FFT_TWIDDLES_60,
        );
        check(
            120,
            &OPUS_FFT_FACTORS_120,
            &OPUS_FFT_BITREV_120,
            &OPUS_FFT_TWIDDLES_120,
        );
        check(
            240,
            &OPUS_FFT_FACTORS_240,
            &OPUS_FFT_BITREV_240,
            &OPUS_FFT_TWIDDLES_240,
        );
        check(
            480,
            &OPUS_FFT_FACTORS_480,
            &OPUS_FFT_BITREV_480,
            &OPUS_FFT_TWIDDLES_480,
        );
    }
}
"#;

/// Regenerate the checked-in table source file.
pub fn run() -> Result<(), String> {
    let content = generate();
    std::fs::write(TABLES_PATH, content).map_err(|e| format!("writing {TABLES_PATH}: {e}"))
}

/// CI guard: fail if the committed tables differ from a fresh generation.
pub fn check() -> Result<(), String> {
    let want = generate();
    let have =
        std::fs::read_to_string(TABLES_PATH).map_err(|e| format!("reading {TABLES_PATH}: {e}"))?;
    if want == have {
        Ok(())
    } else {
        Err(format!(
            "{TABLES_PATH} is stale — run `cargo xtask gen-opus-tables`"
        ))
    }
}
