//! `sbr-reference.md` §3 (§3.1–§3.4): the SBR frequency-band tables — master
//! table, derived HI/LO tables, `kx`/`M`/`N_high`/`N_low`, the noise table,
//! `table_map_k_to_g`, and (deferred) the limiter table. **Verify-first**:
//! [`derive`] is gated on §3.5's two fully-worked examples matching *every*
//! array element exactly — a table bug here silently corrupts every later
//! SBR stage (grid/envelope/HF-gen/HF-adjust all index off these tables).
//!
//! Uses the **FFmpeg live path** for `k0`/`k2` (`start_min` from `A(fs)`/
//! `fs`, the stop-channel power series via [`make_bands`]), not faad2's
//! doubled-rate `startMinTable` — the §3.5 worked numbers (`k0=14`/`13`) are
//! the FFmpeg-live values, and the Task-12 conformance fixtures are
//! ffmpeg-decoded.
//!
//! No `libm`/`f32::{floor,round,powf,sqrt,...}` (all std-only — this crate is
//! `no_std` outside `cfg(test)`): every transcendental need routes through
//! `sbr::tables::{LOG2_INT, POW2_FRAC}` (Task 1) via [`log2_ratio`]/[`pow2`],
//! and rounding routes through the hand-rolled [`round_i32`]/[`floor_i32}`]
//! (same no-libm convention as `formats::aac::round_i16`).

use super::header::SbrHeader;
use super::tables::{LOG2_INT, POW2_FRAC};
use crate::decoder::DecodeError;
use crate::formats::aac::tables::resolve_sfi;

/// Master + derived + noise frequency tables (§3.1–§3.4), built once per
/// accepted header (or header-change reset) and reused every frame until the
/// next reset. `f_lim`/`n_l` are left zeroed by [`derive`] — a separate call
/// to [`derive_limiter`] (after HF-gen patch construction, §8.3) fills them.
#[derive(Clone, Copy)]
pub(super) struct FreqTables {
    pub k0: u8,
    // `k2` (the SBR crossover's upper QMF-band edge) is fully absorbed into
    // `kx`/`m`/`f_master` by the time `derive()` returns — no downstream
    // consumer re-reads it off the struct — but it stays as a field
    // (mirroring the ISO §3.1 derivation's own named quantity) so this
    // module's own tests can assert the `k0..k2` span invariant directly.
    #[allow(dead_code)]
    pub k2: u8,
    pub kx: u8,
    pub m: u8,
    pub n_master: u8,
    pub n_high: u8,
    pub n_low: u8,
    pub n_q: u8,
    pub f_master: [u8; 64],
    pub f_high: [u8; 64],           // f_table_res[HI_RES], len n_high+1
    pub f_low: [u8; 64],            // f_table_res[LO_RES], len n_low+1
    pub f_noise: [u8; 64],          // len n_q+1
    pub table_map_k_to_g: [u8; 64], // per high band m in 0..M
    pub f_lim: [u8; 64],            // limiter borders (filled by derive_limiter)
    pub n_l: u8,                    // limiter band count
    // Not part of the Task-4 brief's public field list: `derive_limiter`'s
    // signature (`ft: &mut FreqTables, patch_borders: &[u8]`) has no header
    // parameter, so `bs_limiter_bands` (needed for the §3.4 special-case
    // `==0` band and the `LIMITER_BANDS_COMPARE` threshold pick) has to
    // survive from `derive()` somewhere. Appended last so the brief's
    // documented fields keep their exact given order.
    pub limiter_bands: u8,
}

// `#[derive(Default)]` doesn't reach this struct's `[u8; 64]` fields —
// `[T; N]: Default` is only implemented in `core` for `N <= 32` — so
// `Default` is implemented by hand instead (same all-zero value the derive
// would have produced for every listed field type).
impl Default for FreqTables {
    fn default() -> Self {
        Self {
            k0: 0,
            k2: 0,
            kx: 0,
            m: 0,
            n_master: 0,
            n_high: 0,
            n_low: 0,
            n_q: 0,
            f_master: [0; 64],
            f_high: [0; 64],
            f_low: [0; 64],
            f_noise: [0; 64],
            table_map_k_to_g: [0; 64],
            f_lim: [0; 64],
            n_l: 0,
            limiter_bands: 0,
        }
    }
}

impl FreqTables {
    /// Bands for envelope resolution `res` (0 = LO → n_low, 1 = HI → n_high).
    pub fn n(&self, res: u8) -> usize {
        if res == 0 {
            self.n_low as usize
        } else {
            self.n_high as usize
        }
    }
}

// ---------------------------------------------------------------------
// No-libm helpers (this file's only float ops beyond +,-,*,/,comparisons)
// ---------------------------------------------------------------------

/// Round-half-away-from-zero to the nearest integer. `f32::round` is
/// std-only (see `formats::aac::round_i16`); this is that same manual
/// `s ± 0.5` + truncating-cast convention.
fn round_i32(x: f32) -> i32 {
    if x >= 0.0 {
        (x + 0.5) as i32
    } else {
        (x - 0.5) as i32
    }
}

/// `floor(x)` without `f32::floor` (std-only): a truncating `as i32` cast
/// already rounds toward zero, so only a negative non-integer needs the −1
/// correction to become a true floor.
fn floor_i32(x: f32) -> i32 {
    let t = x as i32;
    if (t as f32) > x { t - 1 } else { t }
}

/// `log2(num/den)` for QMF-channel-domain integers (`1..=64`, clamped into
/// that range defensively so a malformed/out-of-range caller can't index out
/// of bounds — the no-panic invariant; a clamp here just means a degenerate
/// result that `derive`'s later bound checks catch, never a panic). Exact via
/// `LOG2_INT[i] = log2(i)` subtraction — no interpolation needed since both
/// endpoints are whole channel indices.
fn log2_ratio(num: u32, den: u32) -> f32 {
    let ni = (num as usize).min(64);
    let di = (den as usize).clamp(1, 64);
    LOG2_INT[ni] - LOG2_INT[di]
}

/// `2^y` for arbitrary real `y`, without `f32::powf`/`f32::exp2` (std-only):
/// split `y = floor(y) + frac`, build `2^floor(y)` by direct IEEE-754
/// exponent-field construction (exact for any in-range integer exponent) and
/// `2^frac` from the generated `POW2_FRAC` table, per that table's own doc
/// comment: `2^x = 2^floor(x) · POW2_FRAC[round(frac(x)·256)]`. `y` is
/// clamped to `[-30, 30]` first — well outside any exponent this module's
/// callers can produce (all derived from ratios of QMF channel indices
/// `<=64`), just a no-panic safety margin for the exponent-bit construction.
fn pow2(y: f32) -> f32 {
    let y = y.clamp(-30.0, 30.0);
    let ip = floor_i32(y);
    let frac = y - ip as f32; // in [0, 1)
    let idx = round_i32(frac * 256.0).clamp(0, 256) as usize;
    let frac_pow = POW2_FRAC[idx];
    let bits = ((ip + 127) as u32) << 23;
    f32::from_bits(bits) * frac_pow
}

/// Fixed-size ascending insertion sort, `s.len() <= 64` — every caller's
/// slice is well under that (band counts are bounded by §3.2's span limits),
/// so this beats bringing in an allocator-backed sort for arrays this small.
fn insertion_sort(s: &mut [i32]) {
    for i in 1..s.len() {
        let key = s[i];
        let mut j = i;
        while j > 0 && s[j - 1] > key {
            s[j] = s[j - 1];
            j -= 1;
        }
        s[j] = key;
    }
}

/// §3.2/§3.3's shared "diverging geometric power series" builder (FFmpeg
/// `make_bands`): `n` diffs geometrically spanning `start..stop`
/// (`base = 2^(log2(stop/start)/n)`, each point rounded to the nearest
/// channel and diffed against the previous), except the last diff, which is
/// forced to `stop − previous` so the series always lands exactly on `stop`
/// regardless of intermediate rounding. Output diffs are **not** sorted —
/// callers that need ascending order (both §3.2 and §3.3 do) sort
/// afterward. Writes exactly `out[0..n]`; no-ops (leaving `out` untouched)
/// if `n` doesn't fit `out` — callers size `out` generously and validate `n`
/// themselves before/after.
fn make_bands(out: &mut [i32], start: i32, stop: i32, n: usize) {
    if n == 0 || n > out.len() {
        return;
    }
    if n == 1 || start >= stop || start < 1 {
        out[0] = stop - start;
        for slot in out.iter_mut().take(n).skip(1) {
            *slot = 0;
        }
        return;
    }
    let base_log2 = log2_ratio(stop as u32, start as u32) / n as f32;
    let mut previous = start;
    for (k, slot) in out.iter_mut().take(n - 1).enumerate() {
        let exp = base_log2 * (k as f32 + 1.0);
        let present = round_i32(start as f32 * pow2(exp));
        *slot = present - previous;
        previous = present;
    }
    out[n - 1] = stop - previous;
}

// ---------------------------------------------------------------------
// §3.1 k0 / §3.2 k2
// ---------------------------------------------------------------------

/// §3.1 `offsetIndexTable[sr_index]` — selects which [`OFFSET`] row `k0`'s
/// `bs_start_freq` lookup uses, indexed by the **output** rate's standard
/// `sampling_frequency_index` (`0..=11`; index 12, 7350 Hz, has no SBR row
/// and is rejected by [`derive`] before this table is touched).
const OFFSET_INDEX_TABLE: [u8; 12] = [5, 5, 4, 4, 4, 3, 2, 1, 0, 6, 6, 6];

/// §3.1 `offset[7][16]` (rows 0..=5 are the dual-rate/`RATE==2` rows this
/// decoder always uses — reached both directly for most `sr_index` values
/// and via row 6 for the lowest three; row 6 doubles as the single-rate
/// `RATE==1` row, which this decoder never selects since `RATE` is always 2
/// here, §0).
#[rustfmt::skip]
const OFFSET: [[i8; 16]; 7] = [
    [-8, -7, -6, -5, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7],
    [-5, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13],
    [-5, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16],
    [-6, -4, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16],
    [-4, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16, 20],
    [-2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16, 20, 24],
    [0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 13, 16, 20, 24, 28, 33],
];

/// §3.2 `stopMinTable[sr_index]`, same output-rate `sr_index` domain
/// (`0..=11`) as [`OFFSET_INDEX_TABLE`].
const STOP_MIN_TABLE: [u8; 12] = [13, 15, 20, 21, 23, 32, 32, 35, 48, 64, 70, 96];

/// §3.1 `k0` via the FFmpeg live path: `start_min = round(A(fs)·128/fs)`
/// (`A` = 3000/4000/5000 by rate band), then `+ OFFSET[row][bs_start_freq]`.
fn compute_k0(output_rate: u32, sr_index: u8, start_freq: u8) -> Result<u8, DecodeError> {
    let a: u32 = if output_rate < 32_000 {
        3000
    } else if output_rate < 64_000 {
        4000
    } else {
        5000
    };
    let start_min = (a * 128 + output_rate / 2) / output_rate;

    let row = *OFFSET_INDEX_TABLE
        .get(sr_index as usize)
        .ok_or(DecodeError::Malformed)? as usize;
    let off_row = OFFSET.get(row).ok_or(DecodeError::Malformed)?;
    let off = *off_row
        .get(start_freq as usize)
        .ok_or(DecodeError::Malformed)?;

    let k0 = start_min as i32 + off as i32;
    if !(0..64).contains(&k0) {
        return Err(DecodeError::Malformed);
    }
    Ok(k0 as u8)
}

/// §3.2 `k2` via the FFmpeg live path: `bs_stop_freq` 14/15 are the
/// `2·k0`/`3·k0` special cases; otherwise `stop_min` (table) plus a live
/// 13-entry geometric power series (sorted ascending, summed up to
/// `bs_stop_freq` terms).
fn compute_k2(k0: u8, sr_index: u8, stop_freq: u8) -> Result<u8, DecodeError> {
    let k2: u32 = if stop_freq == 15 {
        (3 * k0 as u32).min(64)
    } else if stop_freq == 14 {
        (2 * k0 as u32).min(64)
    } else {
        let stop_min = *STOP_MIN_TABLE
            .get(sr_index as usize)
            .ok_or(DecodeError::Malformed)? as i32;
        let mut diffs = [0i32; 13];
        make_bands(&mut diffs, stop_min, 64, 13);
        insertion_sort(&mut diffs);
        let take = (stop_freq as usize).min(13);
        let sum: i32 = diffs[..take].iter().sum();
        (stop_min + sum).clamp(0, 64) as u32
    };
    if k2 > 64 {
        return Err(DecodeError::Malformed);
    }
    Ok(k2 as u8)
}

// ---------------------------------------------------------------------
// §3.3 master table
// ---------------------------------------------------------------------

/// Path A (`bs_freq_scale == 0`, linear, `master_frequency_table_fs0`).
fn build_master_linear(k0: u8, k2: u8, alter_scale: u8) -> Result<([u8; 64], u8), DecodeError> {
    let dk: i32 = if alter_scale != 0 { 2 } else { 1 };
    let span = k2 as i32 - k0 as i32;
    let nr_bands: i32 = if alter_scale != 0 {
        ((span + 2) >> 2) << 1
    } else {
        (span >> 1) << 1
    };
    if !(1..64).contains(&nr_bands) {
        return Err(DecodeError::Malformed);
    }
    let nr_bands_u = nr_bands as usize;

    let mut vdk = [dk; 64];
    let k2_achieved = k0 as i32 + nr_bands * dk;
    let k2diff = k2 as i32 - k2_achieved;
    if k2diff < 0 {
        vdk[0] -= 1;
        if k2diff < -1 {
            vdk[1] -= 1;
        }
    } else if k2diff > 0 {
        vdk[nr_bands_u - 1] += 1;
    }

    let mut f_master = [0u8; 64];
    f_master[0] = k0;
    let mut cur = k0 as i32;
    for i in 0..nr_bands_u {
        cur += vdk[i];
        if !(0..64).contains(&cur) {
            return Err(DecodeError::Malformed);
        }
        f_master[i + 1] = cur as u8;
    }
    Ok((f_master, nr_bands as u8))
}

/// Geometric-diff series from `start` to `stop` in `n` steps (§3.3's
/// `make_bands`-then-sort convention), written sorted ascending into
/// `buf[0..n]`.
fn geometric_diffs_sorted(
    start: i32,
    stop: i32,
    n: usize,
    buf: &mut [i32; 64],
) -> Result<(), DecodeError> {
    if n == 0 || n >= buf.len() {
        return Err(DecodeError::Malformed);
    }
    make_bands(buf, start, stop, n);
    insertion_sort(&mut buf[..n]);
    Ok(())
}

/// Path B (`bs_freq_scale ∈ {1,2,3}`, warped log, `master_frequency_table`).
fn build_master_warped(
    k0: u8,
    k2: u8,
    freq_scale: u8,
    alter_scale: u8,
) -> Result<([u8; 64], u8), DecodeError> {
    let bands: f32 = match freq_scale {
        1 => 6.0,
        2 => 5.0,
        3 => 4.0,
        _ => return Err(DecodeError::Malformed),
    };
    let k0i = k0 as i32;
    let k2i = k2 as i32;
    // twoRegions = k2/k0 > 2.2449... ; FFmpeg's integer-safe form: 49k2>110k0.
    let two_regions = 49 * k2i > 110 * k0i;
    let k1 = if two_regions { 2 * k0i } else { k2i };
    if !(k0i < k1 && k1 < 64) {
        return Err(DecodeError::Malformed);
    }

    let nr_band0 = 2 * round_i32(bands * log2_ratio(k1 as u32, k0i as u32));
    if !(1..64).contains(&nr_band0) {
        return Err(DecodeError::Malformed);
    }
    let nr_band0_u = nr_band0 as usize;
    let mut diffs0 = [0i32; 64];
    geometric_diffs_sorted(k0i, k1, nr_band0_u, &mut diffs0)?;

    if !two_regions {
        let mut f_master = [0u8; 64];
        f_master[0] = k0;
        let mut cur = k0i;
        for i in 0..nr_band0_u {
            cur += diffs0[i];
            if !(0..64).contains(&cur) {
                return Err(DecodeError::Malformed);
            }
            f_master[i + 1] = cur as u8;
        }
        return Ok((f_master, nr_band0 as u8));
    }

    // Region 1: k1 -> k2. `warp` = 1/1.3 when bs_alter_scale, else 1.
    let warp: f32 = if alter_scale != 0 { 1.0 / 1.3 } else { 1.0 };
    let nr_band1 = 2 * round_i32(bands * warp * log2_ratio(k2i as u32, k1 as u32));
    if !(1..64).contains(&nr_band1) {
        return Err(DecodeError::Malformed);
    }
    let nr_band1_u = nr_band1 as usize;
    let mut diffs1 = [0i32; 64];
    geometric_diffs_sorted(k1, k2i, nr_band1_u, &mut diffs1)?;

    // Boundary smoothing so region 1 doesn't open with a gap smaller than
    // region 0's last (largest, post-sort) gap — verified against FFmpeg
    // `libavcodec/aacsbr_template.c` `sbr_make_f_master`:
    //
    //   if (vdk1_min < vdk0_max) {
    //       change = FFMIN(vdk0_max - vk1[1], (vk1[num_bands_1] - vk1[1]) >> 1);
    //       vk1[1]           += change;
    //       vk1[num_bands_1] -= change;
    //       <re-sort vk1>
    //   }
    //
    // This is a bounded, **region-1-internal** redistribution between
    // region 1's smallest and largest (post-sort) diffs: `diffs0` is never
    // touched, so Σdiffs0 (and thus `f_master[nr_band0]==k1`, the seam) is
    // preserved exactly. A prior version of this code did a full-value
    // `mem::swap(diffs0[last], diffs1[0])`, which moves a unit of *span*
    // from region 0 into region 1 — that changes Σdiffs0, silently shifting
    // the k1 seam (and every f_master entry from `nr_band0` on) by the
    // swapped delta. See `two_regions` regression test below + the Task 4
    // fix report for a worked before/after.
    let last0 = nr_band0_u - 1;
    let last1 = nr_band1_u - 1;
    let vdk0_max = diffs0[last0];
    let dmin = diffs1[0];
    let dmax = diffs1[last1];
    if dmin < vdk0_max {
        let change = (vdk0_max - dmin).min((dmax - dmin) >> 1);
        diffs1[0] += change;
        diffs1[last1] -= change;
        insertion_sort(&mut diffs1[..nr_band1_u]);
    }

    if nr_band0_u + nr_band1_u >= 64 {
        return Err(DecodeError::Malformed);
    }
    let mut f_master = [0u8; 64];
    f_master[0] = k0;
    let mut cur = k0i;
    for i in 0..nr_band0_u {
        cur += diffs0[i];
        if !(0..64).contains(&cur) {
            return Err(DecodeError::Malformed);
        }
        f_master[i + 1] = cur as u8;
    }
    for i in 0..nr_band1_u {
        cur += diffs1[i];
        if !(0..64).contains(&cur) {
            return Err(DecodeError::Malformed);
        }
        f_master[nr_band0_u + i + 1] = cur as u8;
    }
    Ok((f_master, (nr_band0_u + nr_band1_u) as u8))
}

// ---------------------------------------------------------------------
// §3.4 derived tables
// ---------------------------------------------------------------------

/// `LIMITER_BANDS_COMPARE[bs_limiter_bands-1]` octave-ratio thresholds
/// (§3.4), `bs_limiter_bands ∈ {1,2,3}`.
const LIMITER_BANDS_COMPARE: [f32; 3] = [1.327_152, 1.185_093, 1.119_872];

/// Defense-in-depth guard for `derive()`'s master/high/low border tables.
/// Every builder here only checks the *aggregate* span (`M`) and that each
/// cumulative border stays in `0..64` while it's written — neither catches
/// a non-monotonic or duplicate *interior* border, which naive rounding
/// could in principle produce, and which the `two_regions` boundary-
/// smoothing bug fixed in this file (a raw-value swap between the two
/// regions' diff arrays, replaced by the bounded region-1-internal
/// redistribution above) could have produced had it corrupted a diff into
/// a non-positive value. `f_high`/`f_low`/`f_noise`/HF-gen/HF-adjust all
/// index off these borders, so a corrupted-but-in-range table must fail
/// `derive()` outright rather than silently propagate. `borders[0..=n]`
/// must be strictly increasing.
fn check_strictly_increasing(borders: &[u8], n: usize) -> Result<(), DecodeError> {
    if n >= borders.len() {
        return Err(DecodeError::Malformed);
    }
    for i in 1..=n {
        if borders[i] <= borders[i - 1] {
            return Err(DecodeError::Malformed);
        }
    }
    Ok(())
}

/// Build master + derived + noise tables (§3.1–§3.4) from the header at the
/// **output** sample rate `output_rate` (= 2×core). Validates every bound
/// (§3.5 "No-panic"); on any failure returns `Err` (→ upsample-only).
pub(super) fn derive(hdr: &SbrHeader, output_rate: u32) -> Result<FreqTables, DecodeError> {
    let sr_index = resolve_sfi(output_rate)?;
    if sr_index >= 12 {
        // 7350 Hz: no SBR offset/stop-min row (§3.1/§3.2 tables are 0..=11).
        return Err(DecodeError::Malformed);
    }

    let k0 = compute_k0(output_rate, sr_index, hdr.start_freq)?;
    let k2 = compute_k2(k0, sr_index, hdr.stop_freq)?;
    if k2 <= k0 {
        return Err(DecodeError::Malformed);
    }
    let span_limit = if output_rate <= 32_000 {
        48
    } else if output_rate == 44_100 {
        45
    } else {
        32
    };
    if (k2 as i32 - k0 as i32) > span_limit {
        return Err(DecodeError::Malformed);
    }

    let (f_master, n_master) = if hdr.freq_scale == 0 {
        build_master_linear(k0, k2, hdr.alter_scale)?
    } else {
        build_master_warped(k0, k2, hdr.freq_scale, hdr.alter_scale)?
    };
    if f_master[0] != k0 {
        return Err(DecodeError::Malformed);
    }
    check_strictly_increasing(&f_master, n_master as usize)?;

    if hdr.xover_band as usize >= n_master as usize {
        return Err(DecodeError::Malformed);
    }
    let xover = hdr.xover_band as usize;
    let n_high = n_master as usize - xover;
    if n_high == 0 || n_high >= 64 {
        return Err(DecodeError::Malformed);
    }
    let n_low = (n_high >> 1) + (n_high & 1);

    let mut f_high = [0u8; 64];
    for (k, slot) in f_high.iter_mut().enumerate().take(n_high + 1) {
        *slot = *f_master.get(k + xover).ok_or(DecodeError::Malformed)?;
    }
    check_strictly_increasing(&f_high, n_high)?;
    let m_i32 = f_high[n_high] as i32 - f_high[0] as i32;
    if !(1..=49).contains(&m_i32) {
        return Err(DecodeError::Malformed);
    }
    let m = m_i32 as u8;
    let kx = f_high[0];
    if kx > 32 || kx as u16 + m as u16 > 64 {
        return Err(DecodeError::Malformed);
    }

    let minus = n_high & 1;
    let mut f_low = [0u8; 64];
    f_low[0] = f_high[0];
    for (k, slot) in f_low.iter_mut().enumerate().take(n_low + 1).skip(1) {
        let idx = 2 * k - minus;
        *slot = *f_high.get(idx).ok_or(DecodeError::Malformed)?;
    }
    check_strictly_increasing(&f_low, n_low)?;

    // Noise table (§3.4): N_Q borders over the LO grid.
    let n_q: usize = if hdr.noise_bands == 0 {
        1
    } else {
        let raw = round_i32(hdr.noise_bands as f32 * log2_ratio(k2 as u32, kx as u32));
        raw.clamp(1, 5) as usize
    };
    let mut f_noise = [0u8; 64];
    f_noise[0] = f_low[0];
    let mut i = 0usize;
    for (k, slot) in f_noise.iter_mut().enumerate().take(n_q + 1).skip(1) {
        let denom = n_q + 1 - k;
        if denom == 0 {
            return Err(DecodeError::Malformed);
        }
        i += (n_low - i) / denom;
        *slot = *f_low.get(i).ok_or(DecodeError::Malformed)?;
    }

    // table_map_k_to_g[m] = g s.t. f_noise[g] <= kx+m < f_noise[g+1].
    let mut table_map_k_to_g = [0u8; 64];
    for (rel, slot) in table_map_k_to_g.iter_mut().take(m as usize).enumerate() {
        let k = kx as u16 + rel as u16;
        let mut g = 0usize;
        for (gg, &border) in f_noise.iter().enumerate().take(n_q) {
            if k >= border as u16 {
                g = gg;
            } else {
                break;
            }
        }
        *slot = g as u8;
    }

    Ok(FreqTables {
        k0,
        k2,
        kx,
        m,
        n_master,
        n_high: n_high as u8,
        n_low: n_low as u8,
        n_q: n_q as u8,
        f_master,
        f_high,
        f_low,
        f_noise,
        table_map_k_to_g,
        f_lim: [0u8; 64],
        n_l: 0,
        limiter_bands: hdr.limiter_bands,
    })
}

/// Fill `f_lim`/`n_l` (§3.4) by merging LO borders with the HF-gen patch
/// borders (§8.3), sorting, and pruning borders closer than the octave
/// threshold. Stored relative to `kx` (matching the `bs_limiter_bands==0`
/// single-band case's own `f_LO[..] − kx` convention). `patch_borders` is
/// assumed to be in the same absolute QMF-channel domain as `f_low`
/// (Task 9's HF-gen patch borders — confirm/adjust at that call site).
pub(super) fn derive_limiter(ft: &mut FreqTables, patch_borders: &[u8]) -> Result<(), DecodeError> {
    let kx = ft.kx;
    let n_low = ft.n_low as usize;
    if n_low >= ft.f_low.len() {
        return Err(DecodeError::Malformed);
    }

    if ft.limiter_bands == 0 {
        let last = ft.f_low[n_low];
        ft.f_lim[0] = 0;
        ft.f_lim[1] = last.saturating_sub(kx);
        ft.n_l = 1;
        return Ok(());
    }

    // Merge LO borders with patch borders (absolute channel domain), sort.
    let mut merged = [0i32; 64];
    let mut is_patch = [false; 64];
    let mut count = 0usize;
    for &v in &ft.f_low[..=n_low] {
        if count >= merged.len() {
            return Err(DecodeError::Malformed);
        }
        merged[count] = v as i32;
        count += 1;
    }
    for &b in patch_borders {
        if count >= merged.len() {
            return Err(DecodeError::Malformed);
        }
        merged[count] = b as i32;
        is_patch[count] = true;
        count += 1;
    }
    // Sort (value, is_patch) together via a simple insertion sort so the
    // patch-origin flag travels with its border.
    for i in 1..count {
        let (key, key_patch) = (merged[i], is_patch[i]);
        let mut j = i;
        while j > 0 && merged[j - 1] > key {
            merged[j] = merged[j - 1];
            is_patch[j] = is_patch[j - 1];
            j -= 1;
        }
        merged[j] = key;
        is_patch[j] = key_patch;
    }
    // Dedupe adjacent equals (keep patch-origin if either was a patch border).
    let mut uniq = 0usize;
    for i in 0..count {
        if uniq == 0 || merged[uniq - 1] != merged[i] {
            merged[uniq] = merged[i];
            is_patch[uniq] = is_patch[i];
            uniq += 1;
        } else {
            is_patch[uniq - 1] |= is_patch[i];
        }
    }
    count = uniq;
    if count < 2 {
        return Err(DecodeError::Malformed);
    }

    let threshold = LIMITER_BANDS_COMPARE[(ft.limiter_bands as usize - 1).min(2)];

    // Greedy left-to-right prune: always keep the first and last border
    // (kx and kx+M); an interior border is kept only if it's at least
    // `threshold` above the last *kept* border, preferring a patch border
    // over a plain LO border when one must be dropped in favour of the
    // other.
    let mut kept = [0i32; 64];
    let mut kept_patch = [false; 64];
    let mut kept_n = 0usize;
    for i in 0..count {
        let is_last = i == count - 1;
        if kept_n == 0 {
            kept[0] = merged[0];
            kept_patch[0] = is_patch[0];
            kept_n = 1;
            continue;
        }
        let prev = kept[kept_n - 1].max(1);
        let ratio = merged[i] as f32 / prev as f32;
        if ratio >= threshold || is_last {
            if kept_n >= kept.len() {
                return Err(DecodeError::Malformed);
            }
            kept[kept_n] = merged[i];
            kept_patch[kept_n] = is_patch[i];
            kept_n += 1;
        } else if is_patch[i] && !kept_patch[kept_n - 1] {
            kept[kept_n - 1] = merged[i];
            kept_patch[kept_n - 1] = true;
        }
    }
    if kept_n < 2 {
        return Err(DecodeError::Malformed);
    }

    for (dst, &v) in ft.f_lim.iter_mut().zip(kept[..kept_n].iter()) {
        *dst = (v - kx as i32).max(0) as u8;
    }
    ft.n_l = (kept_n - 1) as u8;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::sbr::header::SbrHeader;
    fn hdr(start: u8, stop: u8, fscale: u8, alter: u8, xover: u8, nbands: u8) -> SbrHeader {
        SbrHeader {
            amp_res: 1,
            start_freq: start,
            stop_freq: stop,
            xover_band: xover,
            freq_scale: fscale,
            alter_scale: alter,
            noise_bands: nbands,
            limiter_bands: 2,
            limiter_gains: 2,
            interpol_freq: 1,
            smoothing_mode: 1,
        }
    }
    #[test]
    fn worked_example_linear_44100() {
        // §3.5 first example: output fs=44100, start=5, stop=6,
        // freq_scale=0, alter=0, xover=3, noise=2.
        let ft = derive(&hdr(5, 6, 0, 0, 3, 2), 44100).unwrap();
        assert_eq!(ft.k0, 14);
        assert_eq!(ft.k2, 37);
        assert_eq!(ft.n_master, 22);
        assert_eq!(
            &ft.f_master[..23],
            &[
                14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34,
                35, 37
            ]
        );
        assert_eq!(ft.kx, 17);
        assert_eq!(ft.m, 20);
        assert_eq!(ft.n_high, 19);
        assert_eq!(ft.n_low, 10);
        assert_eq!(
            &ft.f_high[..20],
            &[
                17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 37
            ]
        );
        assert_eq!(
            &ft.f_low[..11],
            &[17, 18, 20, 22, 24, 26, 28, 30, 32, 34, 37]
        );
        assert_eq!(ft.n_q, 2);
        assert_eq!(&ft.f_noise[..3], &[17, 26, 37]);
    }
    #[test]
    fn worked_example_warped_48000() {
        // §3.5 second example: output fs=48000, start=5, stop=3,
        // freq_scale=2, alter=1, xover=4, noise=2.
        let ft = derive(&hdr(5, 3, 2, 1, 4, 2), 48000).unwrap();
        assert_eq!(ft.k0, 13);
        assert_eq!(ft.k2, 27);
        assert_eq!(ft.n_master, 10);
        assert_eq!(
            &ft.f_master[..11],
            &[13, 14, 15, 16, 17, 18, 19, 21, 23, 25, 27]
        );
        assert_eq!(ft.kx, 17);
        assert_eq!(ft.m, 10);
        assert_eq!(ft.n_high, 6);
        assert_eq!(ft.n_low, 3);
        assert_eq!(&ft.f_high[..7], &[17, 18, 19, 21, 23, 25, 27]);
        assert_eq!(&ft.f_low[..4], &[17, 19, 23, 27]);
        assert_eq!(ft.n_q, 1);
        assert_eq!(&ft.f_noise[..2], &[17, 27]);
    }
    #[test]
    fn worked_example_warped_freq_scale_3_xover_0() {
        // The Task-12 SBR conformance fixture's real `sbr_header`
        // (`testdata/sbr.{aac,m4a}`): output fs=44100, start=5, stop=6,
        // freq_scale=3 (warped Path B, `bands=4`), alter_scale=1, xover=0,
        // noise_bands=1. This exact parameter set — `freq_scale=3` AND
        // `xover=0` — is exercised by NEITHER §3.5 worked example above
        // (which cover freq_scale∈{0,2}, xover∈{3,4}), so it went untested
        // until the HF-reconstruction fidelity investigation. Every value
        // below is hand-derived and cross-checked (k0/k2 via the FFmpeg live
        // path, the two-region warped master with `k1=2·k0`, the
        // xover=0 ⇒ f_high==f_master identity, the LO/noise decimation).
        let ft = derive(&hdr(5, 6, 3, 1, 0, 1), 44100).unwrap();
        assert_eq!(ft.k0, 14);
        assert_eq!(ft.k2, 37);
        assert_eq!(ft.n_master, 10);
        assert_eq!(
            &ft.f_master[..11],
            &[14, 15, 16, 18, 20, 22, 24, 26, 28, 32, 37]
        );
        // xover_band == 0 ⇒ the whole master table is the HI-res table.
        assert_eq!(ft.kx, 14);
        assert_eq!(ft.m, 23);
        assert_eq!(ft.n_high, 10);
        assert_eq!(ft.n_low, 5);
        assert_eq!(
            &ft.f_high[..11],
            &[14, 15, 16, 18, 20, 22, 24, 26, 28, 32, 37]
        );
        assert_eq!(&ft.f_low[..6], &[14, 16, 20, 24, 28, 37]);
        assert_eq!(ft.n_q, 1);
        assert_eq!(&ft.f_noise[..2], &[14, 37]);
    }

    #[test]
    fn rejects_xover_ge_n_master() {
        // xover_band must be < N_master.
        let mut h = hdr(5, 6, 0, 0, 7, 2);
        h.xover_band = 30;
        assert!(derive(&h, 44100).is_err());
    }

    #[test]
    fn derive_limiter_basic_sanity() {
        // Non-tested-by-worked-example sanity check: the limiter table
        // covers the whole kx..kx+M span, starts at 0, and is monotone.
        let mut ft = derive(&hdr(5, 6, 0, 0, 3, 2), 44100).unwrap();
        let patch_borders = [ft.kx, 26, ft.kx + ft.m];
        derive_limiter(&mut ft, &patch_borders).unwrap();
        assert!(ft.n_l >= 1);
        assert_eq!(ft.f_lim[0], 0);
        assert_eq!(ft.f_lim[ft.n_l as usize], ft.m);
        for i in 1..=(ft.n_l as usize) {
            assert!(ft.f_lim[i] > ft.f_lim[i - 1]);
        }
    }

    #[test]
    fn derive_limiter_zero_bands_single_span() {
        let mut ft = derive(&hdr(5, 6, 0, 0, 3, 2), 44100).unwrap();
        ft.limiter_bands = 0;
        derive_limiter(&mut ft, &[]).unwrap();
        assert_eq!(ft.n_l, 1);
        assert_eq!(ft.f_lim[0], 0);
        assert_eq!(ft.f_lim[1], ft.m);
    }

    #[test]
    fn two_regions_master_table_invariants() {
        // Header chosen to hit `two_regions == true` (fires when
        // `49*k2 > 110*k0`, i.e. `k2/k0 ≳ 2.2449`) *and* to actually trigger
        // the boundary-smoothing branch (`min(diffs1) < max(diffs0)`) — most
        // two_regions headers don't reach that inner branch, so it isn't
        // enough to just pick any two_regions header.
        //
        // output_rate=48000 (sr_index=3), start_freq=2, stop_freq=5,
        // freq_scale=3, alter_scale=0, xover_band=4, noise_bands=2:
        //   k0 = 10, k2 = 32          → 49·32=1568 > 110·10=1100 ⇒ two_regions
        //   k1 = 2·k0 = 20
        //   nr_band0 = 8, diffs0 (sorted) = [1,1,1,1,1,1,2,2] → vdk0_max = 2
        //   nr_band1 = 6, diffs1 (sorted) = [1,2,2,2,2,3]     → dmin=1, dmax=3
        //   dmin(1) < vdk0_max(2) ⇒ smoothing triggers:
        //     change = min(2-1, (3-1)>>1) = min(1,1) = 1
        //     diffs1 becomes [2,2,2,2,2,2] (re-sorted, still sums to 12=k2-k1)
        //   f_master = [10,11,12,13,14,15,16,18,20,22,24,26,28,30,32]  (n_master=14)
        //
        // I verified this by hand *and* by probing the real implementation
        // (a throwaway `eprintln!` test, since removed) against both the old
        // buggy `mem::swap` code and the fixed redistribution:
        //   old (buggy):   [10,11,12,13,14,15,16,18,19,21,23,25,27,29,32]
        //   fixed (this):  [10,11,12,13,14,15,16,18,20,22,24,26,28,30,32]
        // The old code's seam (`f_master[nr_band0]`, index 8) lands on 19 —
        // one short of `k1=20` — because the raw-value swap moves a unit of
        // *span* from region 0 into region 1 (Σdiffs0 drops from 10 to 9);
        // the total span k0..k2 is still conserved end-to-end (a value swap
        // can't change the grand total), which is exactly why the generic
        // "endpoints + monotonic" invariants below don't by themselves
        // distinguish old from fixed here — so this test also pins the
        // interior seam value directly (`f_master[8] == 20`), which *does*
        // fail against the old swap (19 != 20).
        let ft = derive(&hdr(2, 5, 3, 0, 4, 2), 48000).unwrap();
        assert_eq!(ft.k0, 10);
        assert_eq!(ft.k2, 32);
        assert!(
            49 * (ft.k2 as u32) > 110 * (ft.k0 as u32),
            "header must trigger two_regions"
        );
        assert_eq!(ft.n_master, 14);

        // (a) strictly monotonically increasing.
        let borders = &ft.f_master[..=(ft.n_master as usize)];
        for w in borders.windows(2) {
            assert!(w[1] > w[0], "f_master not strictly increasing: {borders:?}");
        }
        // (b) seam/endpoints exact: sum is preserved end-to-end...
        assert_eq!(ft.f_master[0], ft.k0);
        assert_eq!(ft.f_master[ft.n_master as usize], ft.k2);
        // ...and, specifically, the k1 seam between the two regions (the
        // exact quantity the old `mem::swap` bug corrupted).
        assert_eq!(
            ft.f_master[8], 20,
            "k1 seam (region0/region1 boundary) must land exactly on k1"
        );
        assert_eq!(
            &ft.f_master[..15],
            &[10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 26, 28, 30, 32]
        );
        // (c) `derive()` returned `Ok` (implicit in the `.unwrap()` above).
    }
}
