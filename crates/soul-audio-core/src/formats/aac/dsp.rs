//! AAC-LC inverse quantization + scalefactor application + short-window
//! group de-interleave (`aac-reference.md` §7 for the iquant/scalefactor
//! math, §5 for the `EIGHT_SHORT` grouping/de-interleave order). Turns Task
//! 4's quantized `Ics::coeffs` into dequantized `Ics::spec`.
//!
//! Reuses [`crate::formats::mp3_codec`]'s `pow_43`/`ldexp_q2` — the MP3
//! codec already needed both pieces AAC's inverse quant does (`pow_43` is a
//! sign-preserving-by-convention `|x|^(4/3)` table+polynomial port of
//! minimp3's `L3_pow_43`; `ldexp_q2(m, e)` is `m · 2^(-e/4)`) — rather than
//! adding `libm` or duplicating `pow_43`'s 145-entry table, per the task
//! brief. Only their *visibility* changed (`pow_43` private -> `pub(crate)`
//! in `mp3_codec.rs`); their logic, tables, and existing tests are
//! untouched.
//!
//! `dequantize`/`apply_stereo`/`apply_tns` feed the IMDCT
//! ([`super::filterbank`]) via [`super::AacDecoder`].
// Task 6's TNS PARCOR tables are transcribed verbatim from
// `aac-reference-verbatim.md` (itself transcribed from faad2 `tns.c`) so
// they can be diffed against the reference; the extra digits parse to the
// same `f32` — same rationale/precedent as `mp3_codec.rs`'s minimp3 tables.
#![allow(clippy::excessive_precision)]

use super::syntax::{EIGHT_SHORT, Ics, MAX_WINDOWS};
use super::tables;
use crate::formats::mp3_codec::{ldexp_q2, pow_43};

/// Section codebooks whose bands carry no spectral energy at this stage,
/// per `aac-reference.md` §5/§6 (`cb 13=PNS, 14/15=intensity`). Mirrors
/// `syntax.rs`'s private `HCB_*` constants of the same names/values;
/// duplicated here as raw values rather than widening their visibility,
/// since the task brief only calls for adding the `spec` field to the
/// parser, not otherwise changing it.
const HCB_ZERO: u8 = 0;
const HCB_NOISE: u8 = 13;
const HCB_INTENSITY2: u8 = 14;
const HCB_INTENSITY: u8 = 15;

/// `pow_43`'s "large-`x`" branch indexes a 145-entry table and is only
/// verified in-bounds up to `x == 8191` (the existing `pow_43_matches_
/// definition` test's own upper bound in `mp3_codec.rs`); `x >= 8224` walks
/// off the end of that table. AAC's codebook-11 escape can, on a
/// malformed/adversarial bitstream (the field widths alone don't forbid it
/// — see `huffman.rs`'s `apply_escape`), yield a magnitude far larger than
/// any real encoder produces. Clamp to this proven-safe bound rather than
/// touch `pow_43`'s logic, so `dequantize` can never panic regardless of
/// input.
const POW43_SAFE_MAX: u32 = 8191;

/// One dequantized spectral line: `sign(q)·|q|^(4/3) · gain` (`aac-
/// reference.md` §7). `pow_43` is only meant to be called with a
/// non-negative magnitude — mirroring its one existing call site in
/// `mp3_codec.rs`, which applies sign the same way externally — since the
/// table's low (would-be-negative-`x`) entries are vestigial from the
/// minimp3 port and do not actually hold `sign(x)·|x|^(4/3)` values (they
/// are not exercised by any existing test, and using them directly would
/// both give wrong magnitudes and risk an out-of-bounds table index for
/// `x <= -17`). So sign is stripped before the call and re-applied after.
fn dequant_line(q: i32, gain: f32) -> f32 {
    let mag = q.unsigned_abs().min(POW43_SAFE_MAX) as i32;
    let v = pow_43(mag) * gain;
    if q < 0 { -v } else { v }
}

/// `2^(exp_q4 / 4)` — the shared sign-safe quarter-power helper both
/// [`sfb_gain`] (§7: `2^(0.25·(sf-100))`) and `apply_stereo`'s intensity
/// scaling (§8: `2^(-0.25·is_pos)`) reduce to, generalized from what was
/// originally `sfb_gain`'s own inline logic (`sf - 100` there is just this
/// function's `exp_q4`).
///
/// `ldexp_q2(m, e) == m · 2^(-e/4)`, so the direct reading of `2^(exp_q4/4)`
/// is `ldexp_q2(1.0, -exp_q4)` — but `ldexp_q2` panics
/// (shift-right-with-overflow, verified empirically: its loop computes
/// `(1u32 << 30) >> (e >> 2)`, undefined/wrong for `e < 0`) whenever its
/// `exp_q2` argument is *negative*. Its one existing MP3 caller carefully
/// biases every argument to stay `>= 0` (see `mp3_codec.rs`'s
/// `MAX_SCFI`-offset gain computation), an invariant its own test
/// (`ldexp_q2_is_power_of_two_quarters`) never exercises past zero either.
/// Both AAC callers routinely need a *positive* `exp_q4` (`sf > 100` is
/// ordinary on real content; `is_pos` is a signed DPCM differential that can
/// be negative, making `-exp_q4` positive) — either would panic on a raw
/// `ldexp_q2` call, not just on adversarial input. Route around it instead
/// of touching `ldexp_q2`'s logic: when `exp_q4 > 0`, compute the
/// reciprocal via a *non-negative* exponent, `2^(e/4) == 1 / 2^(-e/4) == 1 /
/// ldexp_q2(1.0, e)`.
fn pow2_quarter(exp_q4: i32) -> f32 {
    if exp_q4 <= 0 {
        ldexp_q2(1.0, -exp_q4) // -exp_q4 >= 0: safe direct call
    } else {
        1.0 / ldexp_q2(1.0, exp_q4) // exp_q4 > 0: safe non-negative exponent, then invert
    }
}

/// Per-sfb dequant gain, `2^(0.25·(sf-100))` (`aac-reference.md` §7).
fn sfb_gain(sf: i32) -> f32 {
    pow2_quarter(sf - 100)
}

/// Turn one channel's quantized `coeffs` (Task 4) into dequantized `spec`.
///
/// For each scalefactor band (grouped for `EIGHT_SHORT`, a single implicit
/// group for long windows), the per-band `gain = 2^(0.25·(sf-100))` (see
/// [`sfb_gain`]) is computed once and applied to every quantized line in
/// that band via [`dequant_line`].
///
/// `EIGHT_SHORT` de-interleave (§5): Task 4 stores short-block coefficients
/// in bitstream/grouped order — per group, per `sfb`, concatenated across
/// that group's windows (`[window0's band, window1's band, ...]`). This
/// scatters each window's slice of the band into its true position,
/// `window*128 + swb_offset_128[sfb]`, so `spec` ends up as 8 consecutive
/// 128-line windows in per-window ascending-frequency order — ready for
/// Task 7's IMDCT. Long windows are a single group covering the whole
/// 1024-line spectrum, so the same loop reduces to a straight 1:1 copy.
///
/// ZERO and INTENSITY bands are left at `0.0` here (intensity is reconstructed
/// from the other channel in `apply_stereo`). NOISE (PNS, codebook 13) bands
/// are **filled with energy-scaled noise** ([`fill_pns`]) — they carry no
/// spectral data, only a noise scalefactor.
///
/// Never panics: every `spec` write and `coeffs` read is index-guarded
/// against the table/array bounds, and [`dequant_line`] clamps `pow_43`'s
/// input to its proven-safe range — so even a malformed `max_sfb`/grouping,
/// or an escape-coded coefficient far outside any real encoder's range,
/// cannot cause an out-of-bounds access.
pub(crate) fn dequantize(ics: &mut Ics) {
    ics.spec = [0.0; 1024];

    let short = ics.window_sequence == EIGHT_SHORT;
    let swb: &[u16] = if short {
        tables::swb_offset_128(ics.sfi)
    } else {
        tables::swb_offset_1024(ics.sfi)
    };
    let max_sfb = ics.max_sfb as usize;
    if max_sfb >= swb.len() {
        return; // defensive: decode_ics_info() already guarantees max_sfb <= num_swb
    }

    let mut pos: usize = 0; // read cursor into ics.coeffs (grouped order)
    let mut window_base: usize = 0; // first absolute window index of the current group
    for g in 0..ics.num_window_groups as usize {
        let glen = ics.window_group_length[g] as usize;
        for sfb in 0..max_sfb {
            let lo = usize::from(swb[sfb]);
            let hi = usize::from(swb[sfb + 1]);
            let band_width = hi.saturating_sub(lo);
            let total = glen * band_width;
            if pos + total > ics.coeffs.len() {
                return; // defensive: spectral_data() already guarantees this fits
            }

            let cb = ics.sfb_cb[g][sfb];
            if cb == HCB_NOISE {
                // PNS (§6): no spectral data — fill the band with pseudo-random
                // noise scaled to the energy its noise scalefactor implies, so
                // it reconstructs as noise of the intended loudness, not silence.
                let gain = sfb_gain(ics.scale_factors[g][sfb]);
                for w in 0..glen {
                    let window = window_base + w;
                    let dst_base = if short { window * 128 + lo } else { lo };
                    let end = (dst_base + band_width).min(ics.spec.len());
                    if dst_base < end {
                        // Deterministic per-band seed (reproducible; a PNS band's
                        // noise is decoder-specific — never bit/SNR-matched to
                        // another decoder's PRNG).
                        let seed = (ics.scale_factors[g][sfb] as u32).wrapping_mul(2_654_435_761)
                            ^ (dst_base as u32).wrapping_mul(40_503)
                            ^ (g as u32);
                        fill_pns(&mut ics.spec[dst_base..end], gain, seed);
                    }
                }
            } else if cb != HCB_ZERO && cb != HCB_INTENSITY2 && cb != HCB_INTENSITY {
                let gain = sfb_gain(ics.scale_factors[g][sfb]);
                for w in 0..glen {
                    let window = window_base + w;
                    let src = pos + w * band_width;
                    let dst_base = if short { window * 128 + lo } else { lo };
                    for i in 0..band_width {
                        let dst = dst_base + i;
                        if dst < ics.spec.len() && src + i < ics.coeffs.len() {
                            ics.spec[dst] = dequant_line(ics.coeffs[src + i], gain);
                        }
                    }
                }
            }
            pos += total;
        }
        window_base += glen;
    }
}

/// One xorshift32 step (nonzero state in), mapped to `[-1, 1)`. A `libm`-free,
/// no-alloc PRNG for PNS noise generation (the only randomness the decoder
/// needs).
#[inline]
fn pns_rand(state: &mut u32) -> f32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    (x as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// Fill a Perceptual Noise Substitution band (§6, codebook 13) with
/// pseudo-random noise whose **per-line RMS equals `gain`** — the band's noise
/// scalefactor energy. A two-pass normalize (fill → measure energy → rescale)
/// makes the output energy exact regardless of the finite sample set, so the
/// band reconstructs as noise of the intended loudness instead of silence.
/// `seed` is deterministic per band (reproducible); the actual noise values are
/// decoder-specific and intentionally not matched to any other decoder's PRNG —
/// PNS is never bit/SNR-reproducible across decoders.
fn fill_pns(dst: &mut [f32], gain: f32, seed: u32) {
    if dst.is_empty() {
        return;
    }
    let mut state = seed | 1; // xorshift requires a nonzero state
    let mut energy = 0.0f32;
    for v in dst.iter_mut() {
        let r = pns_rand(&mut state);
        *v = r;
        energy += r * r;
    }
    if energy > 0.0 {
        // scale·√(Σr²/N) == gain  ⇒  scale = gain·√(N/energy). `f32::sqrt` is
        // std-only; the crate's no_std software sqrt is in `sbr::hfadj`.
        let scale = gain * super::sbr::hfadj::sqrtf(dst.len() as f32 / energy);
        for v in dst.iter_mut() {
            *v *= scale;
        }
    }
}

// ---------------------------------------------------------------------------
// Task 6: stereo tools (M/S + intensity) — `aac-reference.md` §8.
// ---------------------------------------------------------------------------

/// `true` for the codebooks a stereo band must never be M/S-combined
/// across: PNS (13, the band is synthesized noise, not real spectral data
/// yet — see `HCB_NOISE`'s doc comment) and both intensity codebooks
/// (14/15, the band's "spectral data" is really just an intensity-position
/// signal, reconstructed from the *other* channel below).
fn is_pns_or_intensity_cb(cb: u8) -> bool {
    cb == HCB_NOISE || cb == HCB_INTENSITY2 || cb == HCB_INTENSITY
}

/// Apply M/S then intensity stereo to a CPE's two channels' dequantized
/// `spec` (`aac-reference.md` §8), in place. Must run AFTER `dequantize`
/// (Task 5) and BEFORE `apply_tns`/the IMDCT filterbank (Task 7), per the
/// ISO 14496-3 decoder block diagram's per-CPE pipeline order (`(pns) → ms →
/// is → tns → filterbank`).
///
/// `ms_mask_present`/`ms_used` are [`super::syntax::DecodedFrame`]'s fields
/// of the same names (only meaningful, and only populated by the parser,
/// when the CPE used `common_window` — see that struct's doc comment); the
/// per-band intensity position is [`Ics::scale_factors`]`[g][sfb]` on the
/// *right* channel (`r`) — `scale_factor_data` (`syntax.rs`) stores it there
/// under codebook 14/15, confirmed by reading that function before writing
/// this one, per the task brief.
///
/// **M/S** (§8): applied per scalefactor band where `ms_mask_present == 2`
/// (all bands) or (`== 1` and `ms_used[g][sfb]`), *except* any band where
/// either channel's codebook is noise/intensity ([`is_pns_or_intensity_cb`])
/// — checking both channels (not just the conventional "right channel signals
/// intensity" one) is the conservative reading of the brief's "never apply
/// M/S to an intensity/noise band": it can only ever cause this function to
/// skip a combine it might otherwise incorrectly perform, never the reverse.
///
/// **Intensity** (§8): for every band where the right channel's codebook is
/// 14/15, `r[k] = sign · l[k] · 2^(-0.25·is_pos)` — `is_intensity` is `+1`
/// for cb 15, `-1` for cb 14, and gets multiplied by `invert_intensity =
/// -1` when `ms_mask_present == 1 && ms_used[g][sfb]`, else `+1` (this
/// exact sign rule, including which cb is `+1`, is transcribed from
/// `aac-reference.md` §8's own worked-out formula — the task's paraphrase
/// of it disagreed on which of cb14/cb15 is `+1`, which is exactly why that
/// paraphrase said to "verify against the reference"). The scale factor
/// `2^(-0.25·is_pos)` is [`pow2_quarter`]`(-is_pos)` — the same sign-safe
/// helper `sfb_gain` uses, so a negative `is_pos` (an entirely ordinary DPCM
/// differential, not just adversarial input) can never panic here either.
///
/// Defensive: if `l`/`r` don't actually share a window layout (only
/// possible if the CPE didn't use `common_window`, in which case the
/// bitstream can't validly carry M/S or intensity side info anyway), this
/// is a no-op rather than risk indexing one channel's `spec`/`sfb_cb` using
/// the other's layout — and every band's line range is independently
/// re-clamped against `spec`'s real `[0, 1024)` bounds and `MAX_WINDOWS`, so
/// a corrupted `max_sfb`/`window_group_length` can never cause an
/// out-of-bounds access.
// `g`/`sfb` below index half a dozen different arrays (`ms_used`,
// `l`/`r`'s `window_group_length`/`sfb_cb`/`scale_factors`/`spec`) in the
// same loop body — clippy's single-collection `.iter().enumerate()`
// rewrite doesn't apply cleanly to that.
#[allow(clippy::needless_range_loop)]
pub(crate) fn apply_stereo(
    l: &mut Ics,
    r: &mut Ics,
    ms_mask_present: u8,
    ms_used: &[[bool; tables::MAX_SFB]; MAX_WINDOWS],
) {
    if l.window_sequence != r.window_sequence
        || l.max_sfb != r.max_sfb
        || l.num_window_groups != r.num_window_groups
    {
        return;
    }

    let short = l.window_sequence == EIGHT_SHORT;
    let swb: &[u16] = if short {
        tables::swb_offset_128(l.sfi)
    } else {
        tables::swb_offset_1024(l.sfi)
    };
    if swb.len() < 2 {
        return; // defensive: every real table has >= 2 entries
    }
    let max_sfb = (l.max_sfb as usize).min(swb.len() - 1).min(tables::MAX_SFB);
    let window_len = if short { 128usize } else { 1024usize };
    let num_groups = (l.num_window_groups as usize).min(MAX_WINDOWS);

    let mut window_base: usize = 0;
    for g in 0..num_groups {
        let glen = (l.window_group_length[g] as usize).min(MAX_WINDOWS);
        for sfb in 0..max_sfb {
            let lo = usize::from(swb[sfb]);
            let hi = usize::from(swb[sfb + 1]);
            if hi <= lo || hi > window_len {
                continue;
            }
            let l_cb = l.sfb_cb[g][sfb];
            let r_cb = r.sfb_cb[g][sfb];

            let ms_here = !is_pns_or_intensity_cb(l_cb)
                && !is_pns_or_intensity_cb(r_cb)
                && match ms_mask_present {
                    2 => true,
                    1 => ms_used[g][sfb],
                    _ => false,
                };
            if ms_here {
                for w in 0..glen {
                    let window = window_base + w;
                    if window >= MAX_WINDOWS {
                        break;
                    }
                    let base = if short { window * window_len } else { 0 };
                    for k in lo..hi {
                        let idx = base + k;
                        if idx < l.spec.len() && idx < r.spec.len() {
                            let tmp = l.spec[idx] - r.spec[idx];
                            l.spec[idx] += r.spec[idx];
                            r.spec[idx] = tmp;
                        }
                    }
                }
            }

            if r_cb == HCB_INTENSITY2 || r_cb == HCB_INTENSITY {
                let is_pos = r.scale_factors[g][sfb];
                let scale = pow2_quarter(-is_pos);
                let is_intensity_sign: i32 = if r_cb == HCB_INTENSITY { 1 } else { -1 };
                let invert_intensity: i32 = if ms_mask_present == 1 && ms_used[g][sfb] {
                    -1
                } else {
                    1
                };
                let negate = (is_intensity_sign * invert_intensity) < 0;

                for w in 0..glen {
                    let window = window_base + w;
                    if window >= MAX_WINDOWS {
                        break;
                    }
                    let base = if short { window * window_len } else { 0 };
                    for k in lo..hi {
                        let idx = base + k;
                        if idx < l.spec.len() && idx < r.spec.len() {
                            let v = l.spec[idx] * scale;
                            r.spec[idx] = if negate { -v } else { v };
                        }
                    }
                }
            }
        }
        window_base += glen;
    }
}

// ---------------------------------------------------------------------------
// Task 6: TNS (Temporal Noise Shaping) — `aac-reference.md` §9 /
// `aac-reference-verbatim.md`.
// ---------------------------------------------------------------------------

/// Cap on TNS filter order this module independently trusts, regardless of
/// whatever `syntax.rs`'s parser already bounded — the task brief's
/// mandatory firmware invariant ("Task 4's parser bounds are deliberately
/// looser"). Kept as a private duplicate of `syntax::TNS_MAX_ORDER`'s value
/// (`20`) rather than widening that constant's visibility, matching this
/// file's existing `HCB_*` duplication convention; also sizes every local
/// PARCOR/LPC coefficient array below.
const TNS_MAX_ORDER: usize = 20;

/// The 4 TNS PARCOR (reflection-coefficient) dequantization tables,
/// selected by `(coef_compress, coef_res)`, transcribed verbatim from
/// `aac-reference-verbatim.md` (faad2 `tns.c`). Index = the raw transmitted
/// `coef_bits`-wide code — never sign-extended; the sign lives in the table
/// itself.
#[rustfmt::skip]
const TNS_COEF_0_3: [f32; 8] = [
    0.0, 0.4338837391, 0.7818314825, 0.9749279122,
    -0.9848077530, -0.8660254038, -0.6427876097, -0.3420201433,
];
#[rustfmt::skip]
const TNS_COEF_0_4: [f32; 16] = [
    0.0, 0.2079116908, 0.4067366431, 0.5877852523,
    0.7431448255, 0.8660254038, 0.9510565163, 0.9945218954,
    -0.9957341763, -0.9618256432, -0.8951632914, -0.7980172273,
    -0.6736956436, -0.5264321629, -0.3612416662, -0.1837495178,
];
#[rustfmt::skip]
const TNS_COEF_1_3: [f32; 8] = [
    0.0, 0.4338837391, -0.6427876097, -0.3420201433,
    0.9749279122, 0.7818314825, -0.6427876097, -0.3420201433,
];
#[rustfmt::skip]
const TNS_COEF_1_4: [f32; 16] = [
    0.0, 0.2079116908, 0.4067366431, 0.5877852523,
    -0.6736956436, -0.5264321629, -0.3612416662, -0.1837495178,
    0.9945218954, 0.9510565163, 0.8660254038, 0.7431448255,
    -0.6736956436, -0.5264321629, -0.3612416662, -0.1837495178,
];

/// Dequantize one transmitted TNS coefficient code to a PARCOR coefficient
/// via the matching table. `code` is never sign-extended before this lookup
/// (the sign lives in the table). Clamps `code` to the table's length
/// rather than indexing out of bounds — part of this function's share of
/// the mandatory independent TNS bounds-safety (a corrupted/out-of-spec
/// `coef_bits` is the only way `code` could otherwise exceed the table).
fn parcor_coef(code: u8, coef_res: u8, coef_compress: bool) -> f32 {
    let table: &[f32] = match (coef_compress, coef_res == 0) {
        (false, true) => &TNS_COEF_0_3,
        (false, false) => &TNS_COEF_0_4,
        (true, true) => &TNS_COEF_1_3,
        (true, false) => &TNS_COEF_1_4,
    };
    table[(code as usize).min(table.len() - 1)]
}

/// PARCOR (reflection) coefficients -> direct-form LPC coefficients via the
/// step-up recursion (`aac-reference-verbatim.md`): `a[0]=1; for m in
/// 1..=order { a[m]=parcor[m-1]; for i in 1..m { b[i]=a[i]+a[m]*a[m-i] };
/// copy b->a }`. Returns `a[1..=order]` (the filter taps [`ar_filter`]
/// wants), zero-padded past `order`.
///
/// `order` is independently re-clamped to `TNS_MAX_ORDER` and to
/// `parcor.len()` here too (not just by the caller): every array index this
/// function computes is then provably `<= order <= TNS_MAX_ORDER`, so the
/// fixed-size local `a`/`b` (length `TNS_MAX_ORDER + 1`) and the `parcor`
/// read can never go out of bounds regardless of what the caller passes.
fn parcor_to_lpc(parcor: &[f32], order: usize) -> [f32; TNS_MAX_ORDER] {
    let order = order.min(TNS_MAX_ORDER).min(parcor.len());
    let mut a = [0f32; TNS_MAX_ORDER + 1];
    let mut b = [0f32; TNS_MAX_ORDER + 1];
    a[0] = 1.0;
    for m in 1..=order {
        a[m] = parcor[m - 1];
        for i in 1..m {
            b[i] = a[i] + a[m] * a[m - i];
        }
        a[1..m].copy_from_slice(&b[1..m]);
    }
    let mut lpc = [0f32; TNS_MAX_ORDER];
    lpc[..order].copy_from_slice(&a[1..=order]);
    lpc
}

/// TNS AR (all-pole) synthesis filter, in place, in forward index order:
/// `y[n] = x[n] - Σ_{i=1}^{order} lpc[i-1]·y[n-i]` (`order = lpc.len()`),
/// with history implicitly zero before index 0 — `aac-reference.md` §9's
/// "state reset per filter" (`taps = lpc.len().min(m)` below never reads
/// `buf[m-1-i]` for an `i` that would land before index 0).
///
/// Also used for TNS's `direction == 1` (backward) case: the caller
/// reverses the affected slice, calls this, then reverses back. Backward
/// processes lines from the top of the band down using the same
/// recursive-history AR structure — mathematically identical to reversing
/// the slice, filtering forward, then reversing the result back.
fn ar_filter(buf: &mut [f32], lpc: &[f32]) {
    for m in 0..buf.len() {
        let taps = lpc.len().min(m);
        let mut acc = buf[m];
        for (i, &coef) in lpc.iter().enumerate().take(taps) {
            acc -= coef * buf[m - 1 - i];
        }
        buf[m] = acc;
    }
}

/// Apply this channel's TNS filters (`aac-reference.md` §9) to its
/// dequantized `spec`, per window, honoring `direction`. Must run AFTER
/// `apply_stereo` and BEFORE the IMDCT filterbank (Task 7), per the ISO
/// 14496-3 decoder block diagram; SCE/LFE (mono, no `apply_stereo` call)
/// still get TNS.
///
/// Per window `w`, filters are applied top-down and consume consecutive
/// scalefactor bands: `bottom_sfb` starts at `num_swb` (this `sfi`/window
/// type's *total* band count, not `max_sfb`); each filter's `top_sfb =
/// bottom_sfb` (the previous filter's bottom), then `bottom_sfb = top_sfb -
/// length` (floored at 0). The actual spectral-line range is then this
/// `[bottom_sfb, top_sfb)` *independently clamped* to `mmm = min
/// (max_tns_sfb(sfi, short), max_sfb, num_swb)` before converting through
/// `swb_offset` — matching FFmpeg `aacdec.c`'s `apply_tns` (cross-checked
/// against the faad2-derived `aac-reference.md` prose, which is terser on
/// this specific cumulative-band bookkeeping).
///
/// **Mandatory independent bounds** (per the task brief — never trust
/// `syntax.rs`'s own, deliberately looser, parse-time bounds): `order` is
/// re-clamped to `TNS_MAX_ORDER`; every sfb/line index is re-clamped to
/// `mmm`/`swb.len()`/the window's own `[0, 128)` or `[0, 1024)` line range/
/// `spec.len()` before use. Any out-of-range field causes that one filter
/// to be skipped (`continue`) rather than clamped-and-applied-anyway when
/// its band would be empty or invalid — this function never panics,
/// indexes out of bounds, or loops unboundedly regardless of `ics.tns`'s
/// contents.
pub(crate) fn apply_tns(ics: &mut Ics, sfi: u8) {
    let short = ics.window_sequence == EIGHT_SHORT;
    let swb: &[u16] = if short {
        tables::swb_offset_128(sfi)
    } else {
        tables::swb_offset_1024(sfi)
    };
    if swb.len() < 2 {
        return; // defensive: every real table has >= 2 entries
    }
    let num_swb = if short {
        tables::num_swb_128(sfi)
    } else {
        tables::num_swb_1024(sfi)
    } as usize;
    let window_len = if short { 128usize } else { 1024usize };
    let num_windows = (ics.num_windows as usize).min(ics.tns.n_filt.len());

    let mmm = (tables::max_tns_sfb(sfi, short) as usize)
        .min(ics.max_sfb as usize)
        .min(num_swb)
        .min(swb.len() - 1);

    for w in 0..num_windows {
        let n_filt = (ics.tns.n_filt[w] as usize).min(ics.tns.length[w].len());
        let mut bottom_sfb = num_swb;
        for f in 0..n_filt {
            let top_sfb = bottom_sfb;
            let length = ics.tns.length[w][f] as usize;
            bottom_sfb = top_sfb.saturating_sub(length);

            // MANDATORY independent order clamp.
            let order = (ics.tns.order[w][f] as usize)
                .min(TNS_MAX_ORDER)
                .min(ics.tns.coef[w][f].len());
            if order == 0 {
                continue;
            }

            // MANDATORY independent band-range clamp.
            let start_sfb = bottom_sfb.min(mmm);
            let end_sfb = top_sfb.min(mmm);
            if start_sfb >= end_sfb || end_sfb >= swb.len() {
                continue; // empty/invalid band for this filter: skip safely
            }
            let start_line = usize::from(swb[start_sfb]);
            let end_line = usize::from(swb[end_sfb]);
            if end_line <= start_line || end_line > window_len {
                continue;
            }

            let window_offset = w * window_len;
            let abs_start = window_offset + start_line;
            let abs_end = window_offset + end_line;
            if abs_start >= abs_end || abs_end > ics.spec.len() {
                continue; // never index spec[] out of bounds
            }

            let coef_res = ics.tns.coef_res[w];
            let compress = ics.tns.coef_compress[w][f];
            let mut parcor = [0f32; TNS_MAX_ORDER];
            for (i, p) in parcor.iter_mut().enumerate().take(order) {
                *p = parcor_coef(ics.tns.coef[w][f][i], coef_res, compress);
            }
            let lpc = parcor_to_lpc(&parcor[..order], order);

            let seg = &mut ics.spec[abs_start..abs_end];
            if ics.tns.direction[w][f] {
                seg.reverse();
                ar_filter(seg, &lpc[..order]);
                seg.reverse();
            } else {
                ar_filter(seg, &lpc[..order]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dequant_applies_power_and_scalefactor() {
        let mut ics = Ics::long_test(); // 1 window, max_sfb 1, swb [0,4], coeffs[0..4]=[3,-3,0,0], sf[0]=100
        dequantize(&mut ics);
        let s = &ics.spec;
        assert!((s[0] - 3f32.powf(4.0 / 3.0)).abs() < 1e-3);
        assert!((s[1] + 3f32.powf(4.0 / 3.0)).abs() < 1e-3);
        assert_eq!(s[2], 0.0);
    }

    #[test]
    fn gain_identity_matches_2_pow_quarter_sf_minus_100() {
        // sf == 100 -> gain == 2^0 == 1.0 (the brief's own worked example).
        assert!((sfb_gain(100) - 1.0).abs() < 1e-6);
        // sf == 96 -> gain == 2^-1 == 0.5 (safe direct ldexp_q2 call, d <= 0).
        assert!((sfb_gain(96) - 0.5).abs() < 1e-6);
        // sf == 104 -> gain == 2^1 == 2.0 (the d > 0 / reciprocal path —
        // see sfb_gain's doc comment: a *direct* ldexp_q2(1.0, 100 - 104)
        // call panics, which is exactly why this indirection exists).
        assert!((sfb_gain(104) - 2.0).abs() < 1e-5);
        // sf == 200: still no panic, and still the right order of magnitude
        // (2^25 == 33_554_432).
        assert!((sfb_gain(200) - 2f32.powi(25)).abs() / 2f32.powi(25) < 1e-3);
    }

    #[test]
    fn short_block_deinterleaves_grouped_coeffs_into_per_window_slots() {
        let mut ics = Ics::short_test();
        dequantize(&mut ics);

        let want0 = 5f32.powf(4.0 / 3.0);
        let want1 = 7f32.powf(4.0 / 3.0);
        assert!(
            (ics.spec[0] - want0).abs() < 1e-2,
            "window 0 slot: {}",
            ics.spec[0]
        );
        assert!(
            (ics.spec[128] - want1).abs() < 1e-2,
            "window 1 slot: {}",
            ics.spec[128]
        );
        // A naive 1:1 copy (no de-interleave) would have left the "7" here
        // instead of at spec[128].
        assert_eq!(ics.spec[4], 0.0);
        // Untouched region (group 1's 6 windows, all-zero coeffs) stays silent.
        assert_eq!(ics.spec[2 * 128], 0.0);
        assert_eq!(ics.spec[7 * 128 + 3], 0.0);
    }

    #[test]
    fn zero_and_intensity_bands_stay_silent() {
        // ZERO + both intensity codebooks carry no spectral data for this channel
        // (intensity is reconstructed from the other channel in `apply_stereo`),
        // so `dequantize` must leave them at 0.0. (NOISE/PNS is handled
        // separately below — it is now *filled* with noise, not left silent.)
        for cb in [HCB_ZERO, HCB_INTENSITY2, HCB_INTENSITY] {
            let mut ics = Ics::long_test();
            ics.sfb_cb[0][0] = cb;
            ics.coeffs[0] = 3; // would be nonzero if dequantize ignored the codebook
            dequantize(&mut ics);
            assert_eq!(ics.spec[0], 0.0, "cb {cb}");
            assert_eq!(ics.spec[1], 0.0, "cb {cb}");
        }
    }

    #[test]
    fn noise_band_is_filled_not_silent() {
        // A PNS band (codebook 13) must reconstruct as noise, not silence.
        let mut ics = Ics::long_test();
        ics.sfb_cb[0][0] = HCB_NOISE;
        ics.scale_factors[0][0] = 100; // sfb_gain(100) = 1.0
        dequantize(&mut ics);
        // swb[0] of the long table is several lines wide; at least one must be
        // non-zero now that PNS synthesizes noise.
        assert!(
            ics.spec[..4].iter().any(|&v| v != 0.0),
            "PNS band must be filled with noise, not left silent"
        );
    }

    #[test]
    fn fill_pns_matches_target_rms() {
        // The two-pass normalize must make the band's per-line RMS equal the
        // target `gain` exactly (within f32 rounding), for any gain.
        for &gain in &[0.25f32, 1.0, 4.0, 32.0] {
            let mut buf = [0.0f32; 512];
            fill_pns(&mut buf, gain, 0x9E37_79B9);
            let energy: f32 = buf.iter().map(|&v| v * v).sum();
            let rms = (energy / buf.len() as f32).sqrt();
            assert!(
                (rms - gain).abs() <= 1e-3 * gain,
                "PNS RMS {rms} vs target gain {gain}"
            );
            assert!(buf.iter().any(|&v| v != 0.0));
        }
    }

    #[test]
    fn dequantize_never_panics_on_extreme_escape_coded_magnitude() {
        // Codebook-11's escape can, on an adversarial bitstream, produce a
        // magnitude far beyond any real encoder (see POW43_SAFE_MAX's doc
        // comment) — dequant_line's clamp must keep this from ever
        // panicking (an out-of-bounds table index inside pow_43).
        let mut ics = Ics::long_test();
        ics.coeffs[0] = 65_535;
        ics.coeffs[1] = -65_535;
        dequantize(&mut ics); // must not panic
        assert!(ics.spec[0] > 0.0);
        assert!(ics.spec[1] < 0.0);
    }

    #[test]
    fn dequantize_never_panics_when_scalefactor_exceeds_100() {
        // sf > 100 is entirely ordinary on real content (not just
        // adversarial) — see sfb_gain's doc comment. Regression test for
        // the ldexp_q2 negative-exponent panic this module routes around.
        let mut ics = Ics::long_test();
        ics.scale_factors[0][0] = 180;
        dequantize(&mut ics); // must not panic
        let want = 3f32.powf(4.0 / 3.0) * sfb_gain(180);
        assert!((ics.spec[0] - want).abs() / want < 1e-3);
    }

    #[test]
    fn dequant_line_is_sign_preserving_and_zero_at_zero() {
        assert_eq!(dequant_line(0, 1.0), 0.0);
        assert!(dequant_line(8, 1.0) > 0.0);
        assert!(dequant_line(-8, 1.0) < 0.0);
        assert!((dequant_line(8, 1.0) + dequant_line(-8, 1.0)).abs() < 1e-6);
    }

    #[test]
    fn dequantizes_every_channel_of_every_real_fixture_frame_without_panicking() {
        // End-to-end sanity beyond the hand-built fixtures above: parse
        // every frame of the real ADTS fixture (Task 4's own test data,
        // which starts with LONG_START -> EIGHT_SHORT -> LONG_STOP ->
        // ONLY_LONG, so both the long and grouped-short code paths run
        // here with real scalefactors/coefficients) and dequantize both
        // channels of each. Must never panic; every spec line must be
        // finite (no Inf/NaN from an unexpected gain/pow_43 edge case).
        use super::super::bits::BitReader;
        use super::super::syntax::decode_raw_data_block;
        use crate::demux::adts::AdtsReader;
        use crate::io::SliceSource;

        const AAC: &[u8] = include_bytes!("../../../testdata/aac.aac");
        let mut adts = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        let config = adts.config();
        let mut buf = [0u8; 8192];
        let mut frames = 0;
        while let Some(rdb) = adts.next_frame(&mut buf).unwrap() {
            let mut r = BitReader::new(rdb);
            let mut frame = decode_raw_data_block(&mut r, config).unwrap();
            for ch in 0..frame.n_channels as usize {
                dequantize(&mut frame.channels[ch]);
                assert!(
                    frame.channels[ch].spec.iter().all(|v| v.is_finite()),
                    "frame {frames} channel {ch} produced a non-finite spec value"
                );
            }
            frames += 1;
        }
        assert!(frames > 10);
    }

    // -----------------------------------------------------------------
    // Task 6: stereo (M/S + intensity) + TNS. TDD RED step (per the task
    // brief's Step 1): these reference `apply_stereo`/`apply_tns`/
    // `ar_filter`, which don't exist yet, so this module fails to compile
    // until they're implemented — the brief's three worked examples,
    // verbatim.
    // -----------------------------------------------------------------

    /// Brief (a): M/S on a single band, `l=[1,2], r=[3,4]` -> `l=[4,6],
    /// r=[-2,-2]`.
    #[test]
    fn ms_stereo_matches_brief_example() {
        let mut l = Ics::long_test(); // max_sfb 1, swb[0..1] == [0,4), sfb_cb[0][0] == 1 (ordinary)
        let mut r = Ics::long_test();
        l.spec[0] = 1.0;
        l.spec[1] = 2.0;
        r.spec[0] = 3.0;
        r.spec[1] = 4.0;
        let ms_used = [[false; tables::MAX_SFB]; MAX_WINDOWS];
        apply_stereo(&mut l, &mut r, 2, &ms_used); // ms_mask_present == 2: all bands
        assert_eq!(l.spec[0], 4.0);
        assert_eq!(l.spec[1], 6.0);
        assert_eq!(r.spec[0], -2.0);
        assert_eq!(r.spec[1], -2.0);
    }

    /// Brief (c): `is_pos == 0` -> scale `1.0` -> `r == ±l` (sign per cb
    /// 14/15).
    #[test]
    fn intensity_zero_position_scales_by_one() {
        let ms_used = [[false; tables::MAX_SFB]; MAX_WINDOWS];

        let mut l = Ics::long_test();
        let mut r = Ics::long_test();
        l.spec[0] = 5.0;
        l.spec[1] = -3.0;
        r.sfb_cb[0][0] = HCB_INTENSITY; // cb 15
        r.scale_factors[0][0] = 0; // is_pos == 0
        apply_stereo(&mut l, &mut r, 0, &ms_used);
        assert!((r.spec[0] - 5.0).abs() < 1e-6);
        assert!((r.spec[1] - -3.0).abs() < 1e-6);

        let mut l2 = Ics::long_test();
        let mut r2 = Ics::long_test();
        l2.spec[0] = 5.0;
        r2.sfb_cb[0][0] = HCB_INTENSITY2; // cb 14: opposite sign
        r2.scale_factors[0][0] = 0;
        apply_stereo(&mut l2, &mut r2, 0, &ms_used);
        assert!((r2.spec[0] - -5.0).abs() < 1e-6);
    }

    /// M/S must never touch a band where either channel's codebook is
    /// noise(13)/intensity(14/15), even when `ms_mask_present == 2` ("all
    /// bands").
    #[test]
    fn ms_never_applied_to_intensity_or_noise_band() {
        let ms_used = [[false; tables::MAX_SFB]; MAX_WINDOWS];
        for cb in [HCB_NOISE, HCB_INTENSITY2, HCB_INTENSITY] {
            let mut l = Ics::long_test();
            let mut r = Ics::long_test();
            l.spec[0] = 1.0;
            r.spec[0] = 3.0;
            r.sfb_cb[0][0] = cb;
            apply_stereo(&mut l, &mut r, 2, &ms_used);
            assert_eq!(l.spec[0], 1.0, "cb {cb}: M/S must not have run");
        }
    }

    /// Brief (b): order-1 AR filter, `a=[1, 0.5]` on `x=[1,0,0,0]` ->
    /// `y=[1,-0.5,0.25,-0.125]` (`y[n] = x[n] - 0.5*y[n-1]`).
    #[test]
    fn ar_filter_order_one_matches_brief_example() {
        let mut buf = [1.0f32, 0.0, 0.0, 0.0];
        ar_filter(&mut buf, &[0.5]);
        assert!((buf[0] - 1.0).abs() < 1e-6, "{buf:?}");
        assert!((buf[1] - -0.5).abs() < 1e-6, "{buf:?}");
        assert!((buf[2] - 0.25).abs() < 1e-6, "{buf:?}");
        assert!((buf[3] - -0.125).abs() < 1e-6, "{buf:?}");
    }

    /// Full `apply_tns` pipeline (table lookup -> step-up -> AR filter),
    /// both `direction` values, using `sfi == 5` (32 kHz) where
    /// `max_tns_sfb == num_swb_1024 == 51`, so the filter's declared band
    /// isn't top-clamped away. `TNS_COEF_0_3[1] == 0.4338837391`
    /// (independently retyped from `aac-reference-verbatim.md`, not
    /// imported from the table, so this also cross-checks the transcribed
    /// table).
    #[test]
    fn tns_filters_forward_and_backward_directions_end_to_end() {
        let a = 0.4338837391_f32;
        let want = [1.0, -a, a * a, -a * a * a];

        // Forward: impulse at the band's first line (992), band == [992, 1024).
        let mut fwd = Ics::long_test();
        fwd.sfi = 5;
        fwd.max_sfb = 51;
        fwd.num_windows = 1;
        fwd.tns.n_filt[0] = 1;
        fwd.tns.length[0][0] = 1; // top sfb only: [50, 51) -> lines [992, 1024)
        fwd.tns.order[0][0] = 1;
        fwd.tns.coef_res[0] = 0;
        fwd.tns.coef_compress[0][0] = false;
        fwd.tns.coef[0][0][0] = 1; // TNS_COEF_0_3[1] == a
        fwd.spec[992] = 1.0;
        let sfi = fwd.sfi;
        apply_tns(&mut fwd, sfi);
        for (i, &w) in want.iter().enumerate() {
            assert!(
                (fwd.spec[992 + i] - w).abs() < 1e-3,
                "forward[{i}]: {}",
                fwd.spec[992 + i]
            );
        }

        // Backward: impulse at the band's LAST line (1023); direction == 1
        // processes from the top of the band down.
        let mut bwd = Ics::long_test();
        bwd.sfi = 5;
        bwd.max_sfb = 51;
        bwd.num_windows = 1;
        bwd.tns.n_filt[0] = 1;
        bwd.tns.length[0][0] = 1;
        bwd.tns.order[0][0] = 1;
        bwd.tns.coef_res[0] = 0;
        bwd.tns.coef_compress[0][0] = false;
        bwd.tns.coef[0][0][0] = 1;
        bwd.tns.direction[0][0] = true;
        bwd.spec[1023] = 1.0;
        let sfi = bwd.sfi;
        apply_tns(&mut bwd, sfi);
        for (i, &w) in want.iter().enumerate() {
            assert!(
                (bwd.spec[1023 - i] - w).abs() < 1e-3,
                "backward[{i}]: {}",
                bwd.spec[1023 - i]
            );
        }
    }

    /// Mandatory independent-clamp regression test: an out-of-range TNS
    /// `order`/`length` (far beyond `TNS_MAX_ORDER`/any real `num_swb`)
    /// must be clamped or safely skipped, never panic or produce
    /// non-finite output — on both long and `EIGHT_SHORT` fixtures.
    #[test]
    fn tns_never_panics_on_out_of_range_order_or_length() {
        let mut long = Ics::long_test();
        long.tns.n_filt[0] = 1;
        long.tns.length[0][0] = 255; // far beyond any real num_swb
        long.tns.order[0][0] = 200; // far beyond TNS_MAX_ORDER (20)
        long.tns.coef_res[0] = 1;
        long.tns.coef_compress[0][0] = true;
        for c in long.tns.coef[0][0].iter_mut() {
            *c = 255; // also out-of-range table indices
        }
        let sfi = long.sfi;
        apply_tns(&mut long, sfi);
        assert!(long.spec.iter().all(|v| v.is_finite()));

        let mut short = Ics::short_test();
        short.tns.n_filt[7] = 1;
        short.tns.length[7][0] = 255;
        short.tns.order[7][0] = 255;
        short.tns.coef_res[7] = 1;
        for c in short.tns.coef[7][0].iter_mut() {
            *c = 255;
        }
        let sfi = short.sfi;
        apply_tns(&mut short, sfi);
        assert!(short.spec.iter().all(|v| v.is_finite()));
    }

    /// End-to-end, real-fixture regression: dequantize -> stereo -> TNS for
    /// every channel of every frame in the ADTS fixture, matching the
    /// pipeline order `apply_stereo` (CPE only) then `apply_tns` (every
    /// channel). Must never panic and must always leave `spec` finite.
    #[test]
    fn stereo_and_tns_never_panic_across_every_real_fixture_frame() {
        use super::super::bits::BitReader;
        use super::super::syntax::decode_raw_data_block;
        use crate::demux::adts::AdtsReader;
        use crate::io::SliceSource;

        const AAC: &[u8] = include_bytes!("../../../testdata/aac.aac");
        let mut adts = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        let config = adts.config();
        let mut buf = [0u8; 8192];
        let mut frames = 0;
        while let Some(rdb) = adts.next_frame(&mut buf).unwrap() {
            let mut r = BitReader::new(rdb);
            let mut frame = decode_raw_data_block(&mut r, config).unwrap();
            for ch in 0..frame.n_channels as usize {
                dequantize(&mut frame.channels[ch]);
            }
            if frame.n_channels == 2 {
                let ms_mask_present = frame.stereo[0].ms_mask_present;
                let ms_used = frame.stereo[0].ms_used;
                let (l, rest) = frame.channels.split_at_mut(1);
                apply_stereo(&mut l[0], &mut rest[0], ms_mask_present, &ms_used);
            }
            for ch in 0..frame.n_channels as usize {
                let sfi = frame.channels[ch].sfi;
                apply_tns(&mut frame.channels[ch], sfi);
                assert!(
                    frame.channels[ch].spec.iter().all(|v| v.is_finite()),
                    "frame {frames} channel {ch} produced a non-finite spec value after stereo+TNS"
                );
            }
            frames += 1;
        }
        assert!(frames > 10);
    }
}
