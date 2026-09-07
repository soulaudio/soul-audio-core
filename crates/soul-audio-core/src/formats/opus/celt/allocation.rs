//! CELT bit allocation (RFC 6716 §4.3.3) — the deterministic, table-driven
//! derivation of the exact per-band pulse/fine-bit budget the encoder used.
//! A one-bit mismatch here desyncs the whole frame, so this is a line-for-line
//! transcription of libopus **v1.5.2**, not a reimplementation.
//!
//! # Source correspondence
//!
//! - [`tf_decode`] ← `celt/celt_decoder.c` `tf_decode` (time-frequency
//!   resolution flags).
//! - [`compute_allocation`] runs the post-coarse-energy sequence of
//!   `celt_decode_with_ec` (`celt/celt_decoder.c`, the block from `tf_decode`
//!   through the `clt_compute_allocation` call): tf decode, spread, dynalloc
//!   boosts, alloc-trim, the anti-collapse reservation + budget setup, then
//!   [`clt_compute_allocation`].
//! - [`clt_compute_allocation`] / [`interp_bits2pulses`] ← `celt/rate.c`
//!   `clt_compute_allocation` / `interp_bits2pulses` (the alloc-vector binary
//!   search + interpolation, skip decisions, intensity/dual-stereo decode, and
//!   the pulses-vs-fine-bits split).
//! - [`init_caps`] ← `celt/celt.c` `init_caps`.
//! - [`get_pulses`] / [`bits2pulses`] / [`pulses2bits`] ← `celt/rate.h`.
//!
//! # Units: `tell()` vs `tell_frac()` (BITRES = 3)
//!
//! This is the classic Opus desync source. `tf_decode` and the `spread` gate
//! work in **whole bits** ([`RangeDecoder::tell`], budget `storage*8`).
//! Everything from dynalloc onward — the dynalloc gating, alloc-trim gating,
//! and the `bits` budget fed to [`clt_compute_allocation`] — works in
//! **eighth-bit** units ([`RangeDecoder::tell_frac`], budget `storage*8 <<
//! BITRES`). The two are never mixed.
//!
//! This crate builds in **float** mode, but bit allocation is pure
//! integer arithmetic in libopus regardless of the fixed/float flavour, so
//! every value here is `i32` (libopus `int` / `opus_int32`), transcribed with
//! the same operator grouping (shifts parenthesised so Rust's precedence and
//! `clippy::precedence` both agree with C).

use super::energy::MAX_FINE_BITS;
use crate::formats::opus::opus_tables::{
    BAND_ALLOCATION, CACHE_BITS, CACHE_CAPS, CACHE_INDEX, EBAND_5MS, LOG_N, LOG2_FRAC_TABLE,
    NUM_BANDS,
};
use crate::formats::opus::range::RangeDecoder;

/// Fractional-bit resolution (libopus `celt/entcode.h` `BITRES`): `tell_frac`
/// and every dynalloc/allocation budget below are in eighth-of-a-bit units.
const BITRES: i32 = 3;
/// Binary-search iteration count for the pulse interpolation (libopus
/// `celt/rate.c` `ALLOC_STEPS`).
const ALLOC_STEPS: i32 = 6;
/// `log2(MAX_PSEUDO)` — the `bits2pulses` binary-search width (libopus
/// `celt/rate.h` `LOG_MAX_PSEUDO`).
const LOG_MAX_PSEUDO: i32 = 6;
/// Fine-energy allocation offset (libopus `celt/rate.h` `FINE_OFFSET`).
const FINE_OFFSET: i32 = 21;

/// `SPREAD_NORMAL` (libopus `celt/bands.h`): the default spread decision used
/// when the frame has no budget left to code one.
pub(crate) const SPREAD_NORMAL: u32 = 2;

/// TF-change resolution table (libopus `celt/celt.c` `tf_select_table[4][8]`).
/// First index is `LM`; second is `4*isTransient + 2*tf_select + per_band`.
/// Positive = better frequency resolution, negative = better time resolution.
static TF_SELECT_TABLE: [[i8; 8]; 4] = [
    [0, -1, 0, -1, 0, -1, 0, -1], // 2.5 ms
    [0, -1, 0, -2, 1, 0, 1, -1],  // 5 ms
    [0, -2, 0, -3, 2, 0, 1, -1],  // 10 ms
    [0, -2, 0, -3, 3, 0, 1, -1],  // 20 ms
];

/// Spread-decision inverse CDF (libopus `celt/celt.h` `spread_icdf`).
static SPREAD_ICDF: [u8; 4] = [25, 23, 2, 0];

/// Allocation-trim inverse CDF (libopus `celt/celt.h` `trim_icdf`).
static TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];

/// The full result of CELT bit allocation for one frame. Task 7's PVQ consumes
/// [`Self::pulses`], [`Self::tf_res`], [`Self::spread`], [`Self::coded_bands`],
/// [`Self::balance`], [`Self::intensity`], [`Self::dual_stereo`] and (after the
/// PVQ pass) [`Self::anti_collapse_rsv`]; Task 5's fine-energy passes consume
/// [`Self::fine_quant`] / [`Self::fine_priority`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Allocation {
    /// Per-band time-frequency resolution (libopus `tf_res`), for the PVQ.
    pub tf_res: [i32; NUM_BANDS],
    /// Spread decision (0..=3), consumed by the PVQ (`quant_all_bands`).
    pub spread: u32,
    /// Per-band PVQ bit allocation in eighth-bit units (libopus `pulses` /
    /// `quant_all_bands`'s `b`). *Not* a pulse count — Task 7 converts each via
    /// [`bits2pulses`].
    pub pulses: [i32; NUM_BANDS],
    /// Per-band fine-energy bits (libopus `ebits`), for `unquant_fine_energy`.
    pub fine_quant: [i32; NUM_BANDS],
    /// Per-band finalise-pass priority (libopus `fine_priority`).
    pub fine_priority: [i32; NUM_BANDS],
    /// Number of non-skipped bands (libopus `codedBands`).
    pub coded_bands: usize,
    /// Leftover bits carried into the PVQ re-balancing (libopus `balance`).
    pub balance: i32,
    /// Intensity-stereo band boundary (libopus `intensity`); 0 for mono.
    pub intensity: usize,
    /// Dual-stereo flag (libopus `dual_stereo`); false for mono.
    pub dual_stereo: bool,
    /// Bits reserved for the anti-collapse flag (`1<<BITRES` or 0), read back
    /// after the PVQ pass in Task 8.
    pub anti_collapse_rsv: i32,
}

/// `get_pulses(i)` (libopus `celt/rate.h`): map a pseudo-pulse index to a pulse
/// count — `i` for `i < 8`, then geometrically spaced. Used by the Task 7 PVQ.
pub(crate) fn get_pulses(i: i32) -> i32 {
    if i < 8 {
        i
    } else {
        (8 + (i & 7)) << ((i >> 3) - 1)
    }
}

/// `bits2pulses(m, band, LM, bits)` (libopus `celt/rate.h`): binary-search the
/// pulse cache for the pulse count whose coding cost is closest to `bits`.
/// Returns 0 for bands with no cache entry (guards a `-1` `CACHE_INDEX`).
pub(crate) fn bits2pulses(band: usize, lm: i32, bits: i32) -> i32 {
    // LM++ (cache is indexed by the split level, one above the frame LM).
    let lm1 = (lm + 1).max(0) as usize;
    let idx = lm1 * NUM_BANDS + band;
    let cache_off = match CACHE_INDEX.get(idx) {
        Some(&off) if off >= 0 => off as usize,
        _ => return 0,
    };
    let cache = &CACHE_BITS[cache_off..];
    let mut lo = 0i32;
    let mut hi = i32::from(cache[0]);
    let bits = bits - 1;
    for _ in 0..LOG_MAX_PSEUDO {
        let mid = (lo + hi + 1) >> 1;
        // OPT in libopus: a conditional move.
        if i32::from(cache[mid as usize]) >= bits {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let lo_cost = if lo == 0 {
        -1
    } else {
        i32::from(cache[lo as usize])
    };
    if bits - lo_cost <= i32::from(cache[hi as usize]) - bits {
        lo
    } else {
        hi
    }
}

/// `pulses2bits(m, band, LM, pulses)` (libopus `celt/rate.h`): the coding cost
/// in eighth-bits of `pulses` pulses in `band`. Used by the Task 7 PVQ.
pub(crate) fn pulses2bits(band: usize, lm: i32, pulses: i32) -> i32 {
    if pulses == 0 {
        return 0;
    }
    let lm1 = (lm + 1).max(0) as usize;
    let idx = lm1 * NUM_BANDS + band;
    let cache_off = match CACHE_INDEX.get(idx) {
        Some(&off) if off >= 0 => off as usize,
        _ => return 0,
    };
    match CACHE_BITS.get(cache_off + pulses as usize) {
        Some(&b) => i32::from(b) + 1,
        None => 0,
    }
}

/// `init_caps(m, cap, LM, C)` (libopus `celt/celt.c`): the per-band maximum
/// allocation in eighth-bits, from the derived `cache_caps` table.
pub(crate) fn init_caps(lm: u32, channels: usize, cap: &mut [i32; NUM_BANDS]) {
    let c = channels as i32;
    let lm_i = lm as i32;
    for (i, cap_i) in cap.iter_mut().enumerate() {
        let n = (i32::from(EBAND_5MS[i + 1]) - i32::from(EBAND_5MS[i])) << lm_i;
        // cache.caps[nbEBands*(2*LM+C-1)+i]; the (2*LM+C-1) row selector is in
        // 0..=7 for LM in 0..=3, C in 1..=2.
        let row = 2 * lm as usize + channels - 1;
        let caps = CACHE_CAPS.get(NUM_BANDS * row + i).copied().unwrap_or(0);
        *cap_i = ((i32::from(caps) + 64) * c * n) >> 2;
    }
}

/// Unsigned division matching libopus `celt_udiv` (all call sites here pass a
/// non-negative numerator and a strictly-positive denominator; the `d == 0`
/// guard only exists so malformed input can't panic).
fn celt_udiv(n: i32, d: i32) -> i32 {
    if d <= 0 {
        return 0;
    }
    ((n as u32) / (d as u32)) as i32
}

/// `tf_decode(start, end, isTransient, tf_res, LM, dec)` (libopus
/// `celt/celt_decoder.c`): decode the per-band time-frequency resolution flags.
/// Whole-bit budget (`tell()`, `storage*8`).
pub(crate) fn tf_decode(
    rd: &mut RangeDecoder,
    start: usize,
    end: usize,
    is_transient: bool,
    lm: u32,
) -> [i32; NUM_BANDS] {
    let mut tf_res = [0i32; NUM_BANDS];
    let mut budget = (rd.storage() as u32) * 8;
    let mut tell = rd.tell();
    let mut logp: u32 = if is_transient { 2 } else { 4 };
    // Mirrors libopus's `tell+logp+1<=budget` verbatim (the `+1` reserves the
    // tf_select bit); kept literal for transcription fidelity.
    #[allow(clippy::int_plus_one)]
    let tf_select_rsv = u32::from(lm > 0 && tell + logp + 1 <= budget);
    budget -= tf_select_rsv;
    let mut curr = 0i32;
    let mut tf_changed = 0i32;
    for tf in tf_res.iter_mut().take(end).skip(start) {
        if tell + logp <= budget {
            curr ^= rd.decode_bit_logp(logp) as i32;
            tell = rd.tell();
            tf_changed |= curr;
        }
        *tf = curr;
        logp = if is_transient { 4 } else { 5 };
    }
    let lm_row = &TF_SELECT_TABLE[lm as usize];
    let base = 4 * usize::from(is_transient);
    let mut tf_select = 0i32;
    if tf_select_rsv != 0
        && lm_row[base + tf_changed as usize] != lm_row[base + 2 + tf_changed as usize]
    {
        tf_select = rd.decode_bit_logp(1) as i32;
    }
    for tf in tf_res.iter_mut().take(end).skip(start) {
        let sel = base + 2 * tf_select as usize + *tf as usize;
        *tf = i32::from(lm_row[sel]);
    }
    tf_res
}

/// The alloc-vector binary search from `clt_compute_allocation` (libopus
/// `celt/rate.c`, the `do { … } while (lo <= hi)` loop): given the per-band
/// thresholds/trim offsets and the total budget, return the upper
/// interpolation vector index (libopus's post-loop `lo`, before `hi = lo--`).
#[allow(clippy::too_many_arguments)]
fn alloc_vector_search(
    start: usize,
    end: usize,
    offsets: &[i32; NUM_BANDS],
    cap: &[i32; NUM_BANDS],
    thresh: &[i32; NUM_BANDS],
    trim_offset: &[i32; NUM_BANDS],
    total: i32,
    c: i32,
    lm: i32,
) -> i32 {
    let nb_alloc_vectors = BAND_ALLOCATION.len() as i32;
    let mut lo = 1i32;
    let mut hi = nb_alloc_vectors - 1;
    loop {
        let mut done = false;
        let mut psum = 0i32;
        let mid = (lo + hi) >> 1;
        for j in (start..end).rev() {
            let n = i32::from(EBAND_5MS[j + 1]) - i32::from(EBAND_5MS[j]);
            let mut bitsj = ((c * n * i32::from(BAND_ALLOCATION[mid as usize][j])) << lm) >> 2;
            if bitsj > 0 {
                bitsj = 0.max(bitsj + trim_offset[j]);
            }
            bitsj += offsets[j];
            if bitsj >= thresh[j] || done {
                done = true;
                psum += bitsj.min(cap[j]);
            } else if bitsj >= (c << BITRES) {
                psum += c << BITRES;
            }
        }
        if psum > total {
            hi = mid - 1;
        } else {
            lo = mid + 1;
        }
        if lo > hi {
            break;
        }
    }
    lo
}

/// `interp_bits2pulses(...)` (libopus `celt/rate.c`) — the decode-mode
/// interpolation between two allocation vectors, skip decisions, intensity /
/// dual-stereo decode, and the split of each band's budget into PVQ bits
/// (`bits`, returned via `pulses`) and fine-energy bits (`ebits`). `encode`,
/// `prev` and `signalBandwidth` are always the decoder's constants (0), so the
/// encoder-only branches are elided.
#[allow(clippy::too_many_arguments)]
fn interp_bits2pulses(
    start: usize,
    end: usize,
    skip_start: i32,
    bits1: &[i32; NUM_BANDS],
    bits2: &[i32; NUM_BANDS],
    thresh: &[i32; NUM_BANDS],
    cap: &[i32; NUM_BANDS],
    mut total: i32,
    balance_out: &mut i32,
    skip_rsv: i32,
    intensity: &mut i32,
    mut intensity_rsv: i32,
    dual_stereo: &mut i32,
    mut dual_stereo_rsv: i32,
    bits: &mut [i32; NUM_BANDS],
    ebits: &mut [i32; NUM_BANDS],
    fine_priority: &mut [i32; NUM_BANDS],
    c: i32,
    lm: i32,
    rd: &mut RangeDecoder,
) -> usize {
    let start_i = start as i32;
    let alloc_floor = c << BITRES;
    let stereo = i32::from(c > 1);
    let log_m = lm << BITRES;

    let mut lo = 0i32;
    let mut hi = 1i32 << ALLOC_STEPS;
    for _ in 0..ALLOC_STEPS {
        let mid = (lo + hi) >> 1;
        let mut psum = 0i32;
        let mut done = false;
        for j in (start..end).rev() {
            let tmp = bits1[j] + ((mid * bits2[j]) >> ALLOC_STEPS);
            if tmp >= thresh[j] || done {
                done = true;
                psum += tmp.min(cap[j]);
            } else if tmp >= alloc_floor {
                psum += alloc_floor;
            }
        }
        if psum > total {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    let mut psum = 0i32;
    let mut done = false;
    for j in (start..end).rev() {
        let mut tmp = bits1[j] + ((lo * bits2[j]) >> ALLOC_STEPS);
        if tmp < thresh[j] && !done {
            tmp = if tmp >= alloc_floor { alloc_floor } else { 0 };
        } else {
            done = true;
        }
        tmp = tmp.min(cap[j]);
        bits[j] = tmp;
        psum += tmp;
    }

    // Decide which bands to skip, working backwards from the end.
    let mut coded_bands = end as i32;
    loop {
        let j = coded_bands - 1;
        // Never skip the first band, nor a dynalloc-boosted band.
        if j <= skip_start {
            total += skip_rsv;
            break;
        }
        let jj = j as usize;
        let cb = coded_bands as usize;
        let denom = i32::from(EBAND_5MS[cb]) - i32::from(EBAND_5MS[start]);
        let mut left = total - psum;
        let percoeff = celt_udiv(left, denom);
        // rate.c:337 `left -= (...)*percoeff;` relies on C's wrap-on-overflow
        // signed arithmetic when `left` has gone negative on a malformed
        // stream; `wrapping_mul`/`wrapping_sub` reproduce that bit-for-bit
        // without UB, and are no-ops on any valid (non-overflowing) input.
        left = left.wrapping_sub(denom.wrapping_mul(percoeff));
        let rem = (left - (i32::from(EBAND_5MS[jj]) - i32::from(EBAND_5MS[start]))).max(0);
        let band_width = i32::from(EBAND_5MS[cb]) - i32::from(EBAND_5MS[jj]);
        // rate.c:340 `band_bits = (int)(bits[j] + percoeff*band_width + rem);`
        // — same wrap-on-overflow rationale for the `percoeff*band_width` term.
        let mut band_bits = bits[jj] + percoeff.wrapping_mul(band_width) + rem;
        // Only code a skip decision when above the threshold; else force-skip.
        if band_bits >= thresh[jj].max(alloc_floor + (1 << BITRES)) {
            if rd.decode_bit_logp(1) != 0 {
                break;
            }
            // We used a bit to skip this band.
            psum += 1 << BITRES;
            band_bits -= 1 << BITRES;
        }
        // Reclaim the bits originally allocated to this band.
        psum -= bits[jj] + intensity_rsv;
        if intensity_rsv > 0 {
            intensity_rsv = i32::from(LOG2_FRAC_TABLE[(j - start_i) as usize]);
        }
        psum += intensity_rsv;
        if band_bits >= alloc_floor {
            // Enough for a fine energy bit per channel.
            psum += alloc_floor;
            bits[jj] = alloc_floor;
        } else {
            bits[jj] = 0;
        }
        coded_bands -= 1;
    }

    // Code the intensity and dual stereo parameters.
    if intensity_rsv > 0 {
        *intensity = start_i + rd.dec_uint((coded_bands + 1 - start_i) as u32) as i32;
    } else {
        *intensity = 0;
    }
    if *intensity <= start_i {
        total += dual_stereo_rsv;
        dual_stereo_rsv = 0;
    }
    if dual_stereo_rsv > 0 {
        *dual_stereo = rd.decode_bit_logp(1) as i32;
    } else {
        *dual_stereo = 0;
    }

    // Allocate the remaining bits.
    let cb = coded_bands as usize;
    let denom = i32::from(EBAND_5MS[cb]) - i32::from(EBAND_5MS[start]);
    let mut left = total - psum;
    let percoeff = celt_udiv(left, denom);
    // rate.c:425 `left -= (...)*percoeff;` — same wrap-on-overflow rationale
    // as the identical pattern above (rate.c:337).
    left = left.wrapping_sub(denom.wrapping_mul(percoeff));
    for j in start..coded_bands as usize {
        // rate.c:427 `bits[j] += ((int)percoeff*(m->eBands[j+1]-m->eBands[j]));`
        // — same wrap-on-overflow rationale for the `percoeff*width` term.
        bits[j] += percoeff.wrapping_mul(i32::from(EBAND_5MS[j + 1]) - i32::from(EBAND_5MS[j]));
    }
    for j in start..coded_bands as usize {
        let tmp = left.min(i32::from(EBAND_5MS[j + 1]) - i32::from(EBAND_5MS[j]));
        bits[j] += tmp;
        left -= tmp;
    }

    let mut balance = 0i32;
    for j in start..coded_bands as usize {
        let n0 = i32::from(EBAND_5MS[j + 1]) - i32::from(EBAND_5MS[j]);
        let n = n0 << lm;
        let bit = bits[j] + balance;
        let mut excess;
        if n > 1 {
            excess = (bit - cap[j]).max(0);
            bits[j] = bit - excess;

            // Compensate for the extra DoF in stereo.
            let den =
                c * n + i32::from(c == 2 && n > 2 && *dual_stereo == 0 && (j as i32) < *intensity);
            let n_c_log_n = den * (i32::from(LOG_N[j]) + log_m);
            let mut offset = (n_c_log_n >> 1) - den * FINE_OFFSET;
            // N=2 is the only point that doesn't match the curve.
            if n == 2 {
                offset += (den << BITRES) >> 2;
            }
            // Offsets for the second and third fine energy bit.
            if bits[j] + offset < (den * 2) << BITRES {
                offset += n_c_log_n >> 2;
            } else if bits[j] + offset < (den * 3) << BITRES {
                offset += n_c_log_n >> 3;
            }
            // Divide with rounding.
            let mut eb = (bits[j] + offset + (den << (BITRES - 1))).max(0);
            eb = celt_udiv(eb, den) >> BITRES;
            // Make sure not to bust.
            if c * eb > (bits[j] >> BITRES) {
                eb = (bits[j] >> stereo) >> BITRES;
            }
            // As far as PVQ can go.
            eb = eb.min(MAX_FINE_BITS);
            ebits[j] = eb;
            // If we rounded down or capped, mark for the final fine energy pass.
            fine_priority[j] = i32::from(eb * (den << BITRES) >= bits[j] + offset);
            // Remove the allocated fine bits; the rest goes to PVQ.
            bits[j] -= (c * eb) << BITRES;
        } else {
            // N=1: all bits to fine energy except a single sign bit.
            excess = (bit - (c << BITRES)).max(0);
            bits[j] = bit - excess;
            ebits[j] = 0;
            fine_priority[j] = 1;
        }

        // Fine energy can't use quant_all_bands's re-balancing; do it here.
        if excess > 0 {
            let extra_fine = (excess >> (stereo + BITRES)).min(MAX_FINE_BITS - ebits[j]);
            ebits[j] += extra_fine;
            let extra_bits = (extra_fine * c) << BITRES;
            fine_priority[j] = i32::from(extra_bits >= excess - balance);
            excess -= extra_bits;
        }
        balance = excess;
    }
    // Save leftover bits over the cap for quant_all_bands's rebalancing.
    *balance_out = balance;

    // Skipped bands spend all their bits on fine energy.
    for j in coded_bands as usize..end {
        ebits[j] = (bits[j] >> stereo) >> BITRES;
        bits[j] = 0;
        fine_priority[j] = i32::from(ebits[j] < 1);
    }

    coded_bands as usize
}

/// `clt_compute_allocation(...)` (libopus `celt/rate.c`): reserve the skip /
/// intensity / dual-stereo bits, tilt the static allocation curve by
/// `alloc_trim`, binary-search + interpolate the allocation vectors, then hand
/// off to [`interp_bits2pulses`]. Returns `codedBands`.
#[allow(clippy::too_many_arguments)]
fn clt_compute_allocation(
    start: usize,
    end: usize,
    offsets: &[i32; NUM_BANDS],
    cap: &[i32; NUM_BANDS],
    alloc_trim: i32,
    intensity: &mut i32,
    dual_stereo: &mut i32,
    mut total: i32,
    balance: &mut i32,
    pulses: &mut [i32; NUM_BANDS],
    ebits: &mut [i32; NUM_BANDS],
    fine_priority: &mut [i32; NUM_BANDS],
    c: i32,
    lm: i32,
    rd: &mut RangeDecoder,
) -> usize {
    let nb_alloc_vectors = BAND_ALLOCATION.len() as i32;
    total = total.max(0);
    let mut skip_start = start as i32;
    // Reserve a bit to signal the end of manually skipped bands.
    let skip_rsv = if total >= (1 << BITRES) {
        1 << BITRES
    } else {
        0
    };
    total -= skip_rsv;
    // Reserve bits for the intensity and dual stereo parameters.
    let mut intensity_rsv = 0i32;
    let mut dual_stereo_rsv = 0i32;
    if c == 2 {
        intensity_rsv = i32::from(LOG2_FRAC_TABLE[end - start]);
        if intensity_rsv > total {
            intensity_rsv = 0;
        } else {
            total -= intensity_rsv;
            dual_stereo_rsv = if total >= (1 << BITRES) {
                1 << BITRES
            } else {
                0
            };
            total -= dual_stereo_rsv;
        }
    }

    let mut thresh = [0i32; NUM_BANDS];
    let mut trim_offset = [0i32; NUM_BANDS];
    for j in start..end {
        let width = i32::from(EBAND_5MS[j + 1]) - i32::from(EBAND_5MS[j]);
        // Below this threshold, we're sure not to allocate any PVQ bits.
        thresh[j] = (c << BITRES).max((((3 * width) << lm) << BITRES) >> 4);
        // Tilt of the allocation curve.
        trim_offset[j] = (c
            * width
            * (alloc_trim - 5 - lm)
            * (end as i32 - j as i32 - 1)
            * (1 << (lm + BITRES)))
            >> 6;
        // Single-coefficient bands get more benefit from a coarse value each.
        if (width << lm) == 1 {
            trim_offset[j] -= c << BITRES;
        }
    }

    let vec_hi = alloc_vector_search(
        start,
        end,
        offsets,
        cap,
        &thresh,
        &trim_offset,
        total,
        c,
        lm,
    );
    // libopus `hi = lo--`: the upper vector is the loop's final `lo`, the lower
    // is one below it.
    let hi = vec_hi;
    let lo = vec_hi - 1;

    let mut bits1 = [0i32; NUM_BANDS];
    let mut bits2 = [0i32; NUM_BANDS];
    for j in start..end {
        let n = i32::from(EBAND_5MS[j + 1]) - i32::from(EBAND_5MS[j]);
        let mut bits1j = ((c * n * i32::from(BAND_ALLOCATION[lo as usize][j])) << lm) >> 2;
        let mut bits2j = if hi >= nb_alloc_vectors {
            cap[j]
        } else {
            ((c * n * i32::from(BAND_ALLOCATION[hi as usize][j])) << lm) >> 2
        };
        if bits1j > 0 {
            bits1j = 0.max(bits1j + trim_offset[j]);
        }
        if bits2j > 0 {
            bits2j = 0.max(bits2j + trim_offset[j]);
        }
        if lo > 0 {
            bits1j += offsets[j];
        }
        bits2j += offsets[j];
        if offsets[j] > 0 {
            skip_start = j as i32;
        }
        bits2j = 0.max(bits2j - bits1j);
        bits1[j] = bits1j;
        bits2[j] = bits2j;
    }

    interp_bits2pulses(
        start,
        end,
        skip_start,
        &bits1,
        &bits2,
        &thresh,
        cap,
        total,
        balance,
        skip_rsv,
        intensity,
        intensity_rsv,
        dual_stereo,
        dual_stereo_rsv,
        pulses,
        ebits,
        fine_priority,
        c,
        lm,
        rd,
    )
}

/// Run the CELT post-coarse-energy bit allocation for one frame (libopus
/// `celt_decode_with_ec`, the block from `tf_decode` through
/// `clt_compute_allocation`). Must be called on the same [`RangeDecoder`],
/// immediately after coarse-energy decode.
///
/// `start`/`end` are the CELT band range (0..=21), `channels` is 1 or 2, `lm`
/// is `log2(MDCT sub-blocks)`, and `is_transient` comes from the preamble.
pub(crate) fn compute_allocation(
    rd: &mut RangeDecoder,
    start: usize,
    end: usize,
    channels: usize,
    lm: u32,
    is_transient: bool,
) -> Allocation {
    let c = channels as i32;
    let lm_i = lm as i32;

    // 1. tf_decode (whole-bit budget).
    let tf_res = tf_decode(rd, start, end, is_transient, lm);

    // 2. spread (whole-bit budget: storage*8).
    let total_bits = (rd.storage() as u32) * 8;
    let spread = if rd.tell() + 4 <= total_bits {
        rd.decode_icdf(&SPREAD_ICDF, 5)
    } else {
        SPREAD_NORMAL
    };

    // 3. init_caps.
    let mut cap = [0i32; NUM_BANDS];
    init_caps(lm, channels, &mut cap);

    // 4. dynalloc boosts (eighth-bit budget: total_bits << BITRES, mutated as
    //    boosts are spent).
    let mut offsets = [0i32; NUM_BANDS];
    let mut dynalloc_logp = 6i32;
    let mut total_bits_frac = (total_bits as i32) << BITRES;
    let mut tell_frac = rd.tell_frac() as i32;
    for i in start..end {
        let width = (c * (i32::from(EBAND_5MS[i + 1]) - i32::from(EBAND_5MS[i]))) << lm_i;
        // quanta: 6 bits, no more than 1 bit/sample, no less than 1/8 bit/sample.
        let quanta = (width << BITRES).min((6 << BITRES).max(width));
        let mut dynalloc_loop_logp = dynalloc_logp;
        let mut boost = 0i32;
        while tell_frac + (dynalloc_loop_logp << BITRES) < total_bits_frac && boost < cap[i] {
            let flag = rd.decode_bit_logp(dynalloc_loop_logp as u32);
            tell_frac = rd.tell_frac() as i32;
            if flag == 0 {
                break;
            }
            boost += quanta;
            total_bits_frac -= quanta;
            dynalloc_loop_logp = 1;
        }
        offsets[i] = boost;
        // Making dynalloc more likely.
        if boost > 0 {
            dynalloc_logp = 2.max(dynalloc_logp - 1);
        }
    }

    // 5. alloc_trim (eighth-bit budget).
    let alloc_trim = if tell_frac + (6 << BITRES) <= total_bits_frac {
        rd.decode_icdf(&TRIM_ICDF, 7) as i32
    } else {
        5
    };

    // 6. budget + anti-collapse reservation. `bits` recomputes from the full
    //    storage (not the dynalloc-reduced total), in eighth-bits, minus one.
    let mut bits = (((rd.storage() as i32) * 8) << BITRES) - (rd.tell_frac() as i32) - 1;
    let anti_collapse_rsv = if is_transient && lm_i >= 2 && bits >= ((lm_i + 2) << BITRES) {
        1 << BITRES
    } else {
        0
    };
    bits -= anti_collapse_rsv;

    // 7. clt_compute_allocation.
    let mut pulses = [0i32; NUM_BANDS];
    let mut fine_quant = [0i32; NUM_BANDS];
    let mut fine_priority = [0i32; NUM_BANDS];
    let mut intensity = 0i32;
    let mut dual_stereo = 0i32;
    let mut balance = 0i32;
    let coded_bands = clt_compute_allocation(
        start,
        end,
        &offsets,
        &cap,
        alloc_trim,
        &mut intensity,
        &mut dual_stereo,
        bits,
        &mut balance,
        &mut pulses,
        &mut fine_quant,
        &mut fine_priority,
        c,
        lm_i,
        rd,
    );

    Allocation {
        tf_res,
        spread,
        pulses,
        fine_quant,
        fine_priority,
        coded_bands,
        balance,
        intensity: intensity.max(0) as usize,
        dual_stereo: dual_stereo != 0,
        anti_collapse_rsv,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::ogg_packets::OggPackets;
    use crate::formats::opus::celt::energy::{
        celt_end_band, celt_lm, read_preamble, unquant_coarse_energy,
    };
    use crate::formats::opus::toc::{self, Mode};
    use crate::io::SliceSource;

    // --- get_pulses (rate.h) ---------------------------------------------

    #[test]
    fn get_pulses_matches_formula_and_is_monotonic() {
        // i < 8 => i.
        for i in 0..8 {
            assert_eq!(get_pulses(i), i);
        }
        // (8 + (i&7)) << ((i>>3)-1).
        assert_eq!(get_pulses(8), 8); // (8+0)<<0
        assert_eq!(get_pulses(9), 9); // (8+1)<<0
        assert_eq!(get_pulses(15), 15); // (8+7)<<0
        assert_eq!(get_pulses(16), 16); // (8+0)<<1
        assert_eq!(get_pulses(17), 18); // (8+1)<<1
        assert_eq!(get_pulses(24), 32); // (8+0)<<2
        // Strictly increasing across the pseudo-pulse range.
        for i in 1..40 {
            assert!(get_pulses(i) > get_pulses(i - 1), "not monotonic at {i}");
        }
    }

    // --- bits2pulses / pulses2bits monotonicity against CACHE_* -----------

    #[test]
    fn bits2pulses_is_monotonic_nondecreasing_in_bits() {
        // Band 8, LM 3 is a real cache entry (CACHE_INDEX[(3+1)*21+8] >= 0).
        let (band, lm) = (8usize, 3i32);
        let mut prev = -1;
        for bits in 0..400 {
            let p = bits2pulses(band, lm, bits);
            assert!(p >= 0, "negative pulse count");
            assert!(p >= prev, "bits2pulses decreased at bits={bits}");
            prev = p;
        }
    }

    #[test]
    fn pulses2bits_is_monotonic_and_pairs_with_bits2pulses() {
        let (band, lm) = (12usize, 2i32);
        // The entry's pseudo-pulse count K bounds the valid pulse range: cache
        // byte 0 is K, bytes 1..=K are the (monotone) costs. Reading past K
        // would spill into the next band's entry, so bound the sweep by K read
        // straight from the tables (an independent check of my indexing).
        let cache_off = CACHE_INDEX[((lm + 1) as usize) * NUM_BANDS + band] as usize;
        let k = i32::from(CACHE_BITS[cache_off]);
        // pulses2bits non-decreasing across the valid pulse range.
        let mut prev = -1;
        for pulses in 1..=k {
            let b = pulses2bits(band, lm, pulses);
            assert!(b >= prev, "pulses2bits decreased at pulses={pulses}");
            prev = b;
        }
        assert_eq!(pulses2bits(band, lm, 0), 0);
        // Feeding a pulse count's cost back through bits2pulses lands on (or
        // very near) the same pulse count — the cache is a nearest-cost search.
        for pulses in 1..=k {
            let cost = pulses2bits(band, lm, pulses);
            let back = bits2pulses(band, lm, cost);
            assert!(
                (back - pulses).abs() <= 1,
                "round-trip drifted: pulses={pulses} cost={cost} back={back}"
            );
        }
    }

    #[test]
    fn cache_lookups_guard_out_of_range_indices() {
        // Row 0 of CACHE_INDEX (the -1 sentinels) is only reachable at LM==-1,
        // which bits2pulses never uses (`LM++`); an out-of-range LM must fold to
        // 0 rather than index out of bounds — the malformed-input guard.
        assert_eq!(bits2pulses(0, -1, 100), 0); // hits a -1 sentinel row
        assert_eq!(bits2pulses(0, 100, 100), 0); // index past the table
        assert_eq!(pulses2bits(0, -1, 5), 0);
        assert_eq!(pulses2bits(0, 100, 5), 0);
    }

    // --- init_caps for hand-computed bands --------------------------------

    #[test]
    fn init_caps_matches_hand_computation() {
        // LM=0, C=1: row = 2*0+1-1 = 0.
        let mut cap = [0i32; NUM_BANDS];
        init_caps(0, 1, &mut cap);
        // band 0: N=(1-0)<<0=1, (CACHE_CAPS[0]=224 + 64)*1*1 >> 2 = 288>>2 = 72.
        assert_eq!(cap[0], 72);
        // band 20: N=(100-78)<<0=22, (CACHE_CAPS[20]=37 + 64)*1*22 >> 2
        //          = 101*22>>2 = 2222>>2 = 555.
        assert_eq!(cap[20], 555);

        // LM=3, C=2: row = 2*3+2-1 = 7 -> CACHE_CAPS offset 21*7 = 147.
        let mut cap2 = [0i32; NUM_BANDS];
        init_caps(3, 2, &mut cap2);
        // band 0: N=(1-0)<<3=8, (CACHE_CAPS[147] + 64)*2*8 >> 2.
        let caps147 = i32::from(CACHE_CAPS[147]);
        assert_eq!(cap2[0], ((caps147 + 64) * 2 * 8) >> 2);
        // Every cap is non-negative and fits the u8-derived domain.
        for &v in &cap2 {
            assert!(v >= 0);
        }
    }

    // --- allocation-vector binary search ----------------------------------

    #[test]
    fn alloc_vector_search_is_monotonic_and_hits_both_ends() {
        // Flat thresholds/trim and a huge cap isolate the search's dependence
        // on the total budget: psum(mid) rises with the BAND_ALLOCATION row, so
        // the chosen vector must rise with `total`.
        let (start, end, c, lm) = (0usize, 21usize, 2i32, 3i32);
        let offsets = [0i32; NUM_BANDS];
        let thresh = [0i32; NUM_BANDS];
        let trim = [0i32; NUM_BANDS];
        let cap = [1i32 << 20; NUM_BANDS];
        let nb = BAND_ALLOCATION.len() as i32;

        // total=0 forces the lowest interpolation (upper vector index 1).
        assert_eq!(
            alloc_vector_search(start, end, &offsets, &cap, &thresh, &trim, 0, c, lm),
            1
        );
        // A vast budget saturates to nbAllocVectors (cap-interpolation regime).
        assert_eq!(
            alloc_vector_search(start, end, &offsets, &cap, &thresh, &trim, i32::MAX, c, lm),
            nb
        );
        // Sweeping the budget upward never lowers the chosen vector.
        let mut prev = 0;
        for &total in &[0, 500, 1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 64_000] {
            let v = alloc_vector_search(start, end, &offsets, &cap, &thresh, &trim, total, c, lm);
            assert!(v >= prev, "vector decreased at total={total}");
            assert!((1..=nb).contains(&v));
            prev = v;
        }
    }

    // --- real-frame allocation on testdata/tone.opus ----------------------
    //
    // The first audio packet of the fixture is a CELT-only fullband stereo
    // 20 ms frame (config 31, C=2, LM=3), so this exercises the stereo path
    // (intensity + dual_stereo reservation/decode) and every band. libopus
    // cannot run at test time to emit a per-band reference, so this is a
    // determinism + budget/range gate; the exact per-band pulse counts are
    // validated bit-for-bit at Task 8's end-to-end PCM golden, and the
    // reviewer's independent rate.c diff is the direct code check (same
    // deferral as Task 5's coarse-energy real-frame test).

    /// Decode the preamble + coarse energy + allocation for the first CELT
    /// frame, returning the allocation and the frame length in bytes.
    fn decode_first_celt_allocation(bytes: &[u8]) -> (Allocation, usize, usize, usize, usize) {
        let mut ogg = OggPackets::new(SliceSource::new(bytes)).expect("open ogg-opus");
        let mut hdr = [0u8; 512];
        ogg.next_packet(&mut hdr).expect("OpusHead");
        ogg.next_packet(&mut hdr).expect("OpusTags");

        let mut old_ebands = [0.0f32; NUM_BANDS * 2];
        let mut pkt = [0u8; 4096];
        loop {
            let n = ogg.next_packet(&mut pkt).expect("audio packet");
            assert!(n > 0, "no CELT frame found");
            let mut frames: [&[u8]; toc::MAX_FRAMES] = [&[][..]; toc::MAX_FRAMES];
            let (config, _fcount) = toc::frames(&pkt[..n], &mut frames).expect("split frames");
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
            return (alloc, frame.len(), channels, end, lm as usize);
        }
    }

    #[test]
    fn allocation_on_real_tone_opus_is_deterministic_and_within_budget() {
        let bytes = include_bytes!("../../../../../../testdata/tone.opus");

        // Determinism: identical input -> identical allocation.
        let (a, len, channels, end, lm) = decode_first_celt_allocation(bytes.as_slice());
        let (b, ..) = decode_first_celt_allocation(bytes.as_slice());
        assert_eq!(a, b, "allocation is not deterministic");

        let c = channels as i32;
        assert_eq!(
            channels, 2,
            "fixture's first CELT frame is stereo (config 31)"
        );

        // Budget: the total allocated (PVQ + fine) never exceeds the frame's
        // eighth-bit budget, and every band respects its cap.
        let mut cap = [0i32; NUM_BANDS];
        init_caps(lm as u32, channels, &mut cap);
        let mut total_alloc = 0i64;
        #[allow(clippy::needless_range_loop)]
        for i in 0..end {
            assert!(a.pulses[i] >= 0, "band {i}: negative PVQ bits");
            assert!(a.pulses[i] <= cap[i], "band {i}: PVQ bits exceed cap");
            assert!(
                (0..=MAX_FINE_BITS).contains(&a.fine_quant[i]),
                "band {i}: fine_quant {} out of [0,8]",
                a.fine_quant[i]
            );
            assert!(
                a.fine_priority[i] == 0 || a.fine_priority[i] == 1,
                "band {i}: fine_priority not a flag"
            );
            total_alloc += i64::from(a.pulses[i]) + i64::from((c * a.fine_quant[i]) << BITRES);
        }
        let frame_bits_frac = (len as i64) * 8 * 8;
        assert!(
            (0..=frame_bits_frac).contains(&total_alloc),
            "allocated {total_alloc} eighth-bits exceeds frame budget {frame_bits_frac}"
        );

        // coded_bands and the stereo parameters are in range.
        assert!(
            a.coded_bands >= 1 && a.coded_bands <= end,
            "coded_bands {} out of (0,{end}]",
            a.coded_bands
        );
        assert!(a.intensity <= a.coded_bands, "intensity beyond coded_bands");
        assert!(a.spread <= 3, "spread out of 0..=3");
        // Anti-collapse is reserved only for transient LM>=2 frames.
        assert!(a.anti_collapse_rsv == 0 || a.anti_collapse_rsv == (1 << BITRES));
    }

    // --- robustness: hostile/degenerate input must never panic ------------
    //
    // `compute_allocation` has no preconditions on the range-coder bytes
    // beyond "positioned wherever the caller left it" — a corrupted or
    // adversarial frame drives the same code path as a well-formed one, just
    // with different bit values coming back from `decode_bit_logp`/`dec_uint`.
    // This sweeps degenerate byte patterns (all-zero, all-one, alternating,
    // truncated) across every channel/LM/transient combination and asserts
    // none of them panics, i.e. the `interp_bits2pulses` wrapping fix (and
    // everything else on this path) holds up under attacker-controlled input,
    // not just the real, well-formed `tone.opus` frame above.
    #[test]
    fn compute_allocation_never_panics_on_hostile_bytes() {
        use std::panic::{self, AssertUnwindSafe};

        let patterns: [&[u8]; 6] = [
            &[0x00; 64],
            &[0xff; 64],
            &[0xaa; 64],
            &[0x55; 64],
            &[0x00, 0xff, 0x00, 0xff, 0x00, 0xff, 0x00, 0xff],
            &[0xff], // truncated to a single byte
        ];

        for bytes in patterns {
            for channels in [1usize, 2usize] {
                for lm in 0u32..=3 {
                    for is_transient in [false, true] {
                        let result = panic::catch_unwind(AssertUnwindSafe(|| {
                            let mut rd = RangeDecoder::new(bytes);
                            compute_allocation(&mut rd, 0, NUM_BANDS, channels, lm, is_transient)
                        }));
                        assert!(
                            result.is_ok(),
                            "compute_allocation panicked: bytes={bytes:?} channels={channels} \
                             lm={lm} is_transient={is_transient}"
                        );
                    }
                }
            }
        }

        // Wider randomized sweep (xorshift, no external dep) over buffer
        // length/content, on top of the fixed patterns above.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let len = 1 + (next() % 128) as usize;
            let buf: Vec<u8> = (0..len).map(|_| (next() & 0xff) as u8).collect();
            let channels = if next() % 2 == 0 { 1 } else { 2 };
            let lm = (next() % 4) as u32;
            let is_transient = next() % 2 == 0;
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let mut rd = RangeDecoder::new(&buf);
                compute_allocation(&mut rd, 0, NUM_BANDS, channels, lm, is_transient)
            }));
            assert!(
                result.is_ok(),
                "compute_allocation panicked: bytes={buf:?} channels={channels} lm={lm} \
                 is_transient={is_transient}"
            );
        }
    }
}
