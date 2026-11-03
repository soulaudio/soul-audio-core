//! CELT CWRS (combinatorial pulse-vector enumeration) — RFC 6716 §4.3.4.
//!
//! Decodes a range-coded index back into the integer pulse vector `y` with
//! `sum(|y|) == K` (the "V-codebook" entry). This is the entropy read that
//! **must** stay bit-exact with libopus or the whole frame desyncs.
//!
//! # Source correspondence
//!
//! Transcribed from libopus **v1.5.2** `celt/cwrs.c`, the **`SMALL_FOOTPRINT`**
//! branch (lines 543-715): [`unext`]/[`uprev`] (the `U(n,k)` row recurrence),
//! [`ncwrs_urow`] (build row `n` of `U` and return `V(n,k)`), [`cwrsi`] (walk
//! the index into pulses), and [`decode_pulses`]. libopus's default build ships
//! a 1272-entry static `CELT_PVQ_U_DATA` table instead; the `SMALL_FOOTPRINT`
//! path computes those same `U(n,k)` values on the fly with `O(K)` scratch and
//! `u32` add/sub only — no table, which suits this `no_std`/no-alloc core (the
//! same correctness-first "compute it, don't tabulate it" choice the project's
//! naïve-IMDCT precedent makes). The `icwrs` encode direction (the round-trip
//! oracle) is transcribed in the tests.
//!
//! # Modular `u32` arithmetic
//!
//! libopus works over `opus_uint32` and relies on it wrapping (`UADD32`/`USUB32`
//! in [`unext`]/[`uprev`]). Those sites use `wrapping_add`/`wrapping_sub` so a
//! Rust debug build stays bit-identical instead of panicking; every value is
//! guaranteed `< 2^32` for CELT's valid `(N,K)` ranges (`fits_in32`), so the
//! wrap never actually fires on a well-formed stream — it only guards against a
//! corrupt one.

use crate::formats::opus::range::RangeDecoder;

/// Largest pulse count `K` any CELT band in the standard 48 kHz mode is coded
/// with. `K = get_pulses(q)` where `q` is bounded by the pulse cache's max
/// pseudo-pulse count (40 → `get_pulses(40) = 128`); larger `K` would overflow
/// `V(N,K)` past `u32` (`fits_in32`) and never occurs. The `U`-row scratch is
/// `K + 2` entries.
pub(crate) const MAX_PULSES: usize = 128;

/// Largest band width `N` (MDCT bins) in the standard mode: band 20 spans
/// `22` short-block bins, `<< LM=3` → `176`. Bounds the pulse-vector buffer.
pub(crate) const MAX_BAND_N: usize = 176;

/// libopus `celt/cwrs.c` `unext` — advance a `U`-recurrence row/column in place:
/// `u[i][j] = u[i-1][j] + u[i][j-1] + u[i-1][j-1]`, base case `u0`. The
/// `UADD32`s wrap (see module docs).
fn unext(u: &mut [u32], len: usize, mut u0: u32) {
    // libopus's do-while overruns for len < 2; every caller passes len >= 2, but
    // guard so a malformed stream can't index out of bounds.
    if len < 1 {
        return;
    }
    let mut j = 1;
    while j < len {
        let ui1 = u[j].wrapping_add(u[j - 1]).wrapping_add(u0);
        u[j - 1] = u0;
        u0 = ui1;
        j += 1;
    }
    u[len - 1] = u0;
}

/// libopus `celt/cwrs.c` `uprev` — step a `U`-recurrence row/column *backwards*:
/// `u[i-1][j] = u[i][j] - u[i][j-1] - u[i-1][j-1]`, base case `u0`. The
/// `USUB32`s wrap (see module docs).
fn uprev(u: &mut [u32], n: usize, mut u0: u32) {
    if n < 1 {
        return;
    }
    let mut j = 1;
    while j < n {
        let ui1 = u[j].wrapping_sub(u[j - 1]).wrapping_sub(u0);
        u[j - 1] = u0;
        u0 = ui1;
        j += 1;
    }
    u[n - 1] = u0;
}

/// libopus `celt/cwrs.c` `ncwrs_urow` — fill `u[0..=k+1]` with row `n` of the
/// `U(n,·)` table and return `V(n,k) = U(n,k) + U(n,k+1)` (the PVQ codebook
/// size, i.e. the `ft` for the range read). `u` must have length `k + 2`.
fn ncwrs_urow(n: usize, k: usize, u: &mut [u32]) -> u32 {
    let len = k + 2;
    debug_assert!(u.len() >= len && len >= 3 && n >= 2);
    u[0] = 0;
    u[1] = 1; // um2
    // u[kk] = 2*kk - 1 for kk in 2..len.
    for (kk, slot) in u.iter_mut().enumerate().take(len).skip(2) {
        *slot = ((kk as u32) << 1) - 1;
    }
    // Advance from row 2 up to row n.
    let mut r = 2;
    while r < n {
        unext(&mut u[1..], k + 1, 1);
        r += 1;
    }
    u[k].wrapping_add(u[k + 1])
}

/// libopus `celt/cwrs.c` `cwrsi` (`SMALL_FOOTPRINT`) — decode index `i` into the
/// pulse vector `y[0..n]` (`sum(|y|) == k`), returning `yy = sum(y[j]^2)` (the
/// `Ryy` [`super::vq::normalise_residual`] scales by). `u` holds row `n` of
/// `U()` on entry and is destructively stepped down by [`uprev`].
///
/// `k` is mutated as pulses are peeled off; every `u` index stays in
/// `0..=k+1 <= u.len()-1`. The `u[--k]` walk terminates because `u[0] == 0` is
/// never `> i` (a `u32`), so it can't underflow — but the loop is still guarded
/// on `k > 0` against a corrupt row.
fn cwrsi(n: usize, mut k: i32, mut i: u32, y: &mut [i32], u: &mut [u32]) -> f32 {
    let mut yy = 0.0f32;
    let mut j = 0;
    loop {
        let ku = k as usize;
        // Are the pulses in this dimension negative? (p = u[k+1])
        let p = u[ku + 1];
        let neg = i >= p;
        if neg {
            i -= p;
        }
        // Count how many pulses were placed here: walk k down while u[k] > i.
        let yj0 = k;
        let mut p = u[k as usize];
        while p > i && k > 0 {
            k -= 1;
            p = u[k as usize];
        }
        i -= p;
        let yj = yj0 - k; // number of pulses at this position
        let val = if neg { -yj } else { yj };
        y[j] = val;
        yy += (val as f32) * (val as f32);
        // Step the row back one dimension.
        uprev(u, (k as usize) + 2, 0);
        j += 1;
        if j >= n {
            break;
        }
    }
    yy
}

/// libopus `celt/cwrs.c` `decode_pulses` — read the CWRS index for a band of
/// size `n` with `k` pulses and expand it into `y`. Returns `Ryy`. This is the
/// bit-exact range read at the heart of PVQ shape decode.
///
/// `n >= 2` and `k >= 1` (libopus `celt_assert`); `k` is bounded by
/// [`MAX_PULSES`]. On a malformed `k`/`n` the buffers are clamped so no index
/// can panic (the decode is meaningless then, but the coder must not crash).
pub(crate) fn decode_pulses(y: &mut [i32], n: usize, k: i32, rd: &mut RangeDecoder) -> f32 {
    if n < 2 || k < 1 {
        for v in y.iter_mut().take(n) {
            *v = 0;
        }
        return 0.0;
    }
    let k = (k as usize).min(MAX_PULSES) as i32;
    let mut u = [0u32; MAX_PULSES + 2];
    let ulen = (k as usize) + 2;
    let v = ncwrs_urow(n, k as usize, &mut u[..ulen]);
    if v <= 1 {
        for e in y.iter_mut().take(n) {
            *e = 0;
        }
        return 0.0;
    }
    let idx = rd.dec_uint(v);
    cwrsi(n, k, idx, y, &mut u[..ulen])
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Independently-transcribed encode direction (libopus icwrs) ----------
    //
    // `icwrs` (celt/cwrs.c `SMALL_FOOTPRINT`) is the inverse of `cwrsi`: given a
    // pulse vector it returns the index and `V(N,K)`. Transcribing it here (a
    // DIFFERENT function than the `cwrsi`/`decode_pulses` under test) gives a
    // genuine round-trip oracle: a shared bug would have to appear in both the
    // forward recurrence walk and the backward index accumulation and cancel,
    // which a single typo can't do.

    /// libopus `icwrs1`: `*k = |y[0]|`, returns `y[0] < 0`.
    fn icwrs1(y0: i32) -> (u32, i32) {
        ((y0 < 0) as u32, y0.abs())
    }

    /// libopus `icwrs` (`SMALL_FOOTPRINT`): index of pulse vector `y[0..n]`,
    /// and `V(n,k)` via `*nc`. `u` must have length `k + 2`.
    fn icwrs(n: usize, k: usize, y: &[i32], u: &mut [u32]) -> (u32, u32) {
        u[0] = 0;
        for (kk, slot) in u.iter_mut().enumerate().take(k + 2).skip(1) {
            *slot = ((kk as u32) << 1) - 1;
        }
        let (mut i, mut kacc) = icwrs1(y[n - 1]);
        let mut jj = n as i32 - 2;
        i = i.wrapping_add(u[kacc as usize]);
        kacc += y[jj as usize].abs();
        if y[jj as usize] < 0 {
            i = i.wrapping_add(u[(kacc + 1) as usize]);
        }
        while {
            let cont = jj > 0;
            jj -= 1;
            cont
        } {
            unext(u, k + 2, 0);
            i = i.wrapping_add(u[kacc as usize]);
            kacc += y[jj as usize].abs();
            if y[jj as usize] < 0 {
                i = i.wrapping_add(u[(kacc + 1) as usize]);
            }
        }
        let nc = u[kacc as usize].wrapping_add(u[(kacc + 1) as usize]);
        (i, nc)
    }

    /// `V(n,k)` via a fresh `U`-row (no decoder needed).
    fn celt_pvq_v(n: usize, k: usize) -> u32 {
        let mut u = [0u32; MAX_PULSES + 2];
        ncwrs_urow(n, k, &mut u[..k + 2])
    }

    // --- V(N,K) against hand-computed values ---------------------------------

    #[test]
    fn v_nk_matches_reference_table() {
        // From celt/cwrs.c's documented V[N][K] table (the comment block):
        //   V[2] = {1, 4, 8, 12, 16, ...}, V[3] = {1, 6, 18, 38, 66, ...},
        //   V[4] = {1, 8, 32, 88, 192, ...}, V[5] = {1,10,50,170,450, ...}.
        // (The brief's V(2,1)=4, V(2,2)=8 among them — verified, not trusted.)
        assert_eq!(celt_pvq_v(2, 1), 4);
        assert_eq!(celt_pvq_v(2, 2), 8);
        assert_eq!(celt_pvq_v(2, 3), 12);
        assert_eq!(celt_pvq_v(3, 1), 6);
        assert_eq!(celt_pvq_v(3, 2), 18);
        assert_eq!(celt_pvq_v(3, 3), 38);
        assert_eq!(celt_pvq_v(3, 4), 66);
        assert_eq!(celt_pvq_v(4, 1), 8);
        assert_eq!(celt_pvq_v(4, 2), 32);
        assert_eq!(celt_pvq_v(4, 3), 88);
        assert_eq!(celt_pvq_v(5, 2), 50);
        assert_eq!(celt_pvq_v(5, 3), 170);
        // Note: V(N,K)=U(N,K)+U(N,K+1) is NOT symmetric (only U is); e.g.
        // V(2,3)=12 != V(3,2)=18. The exhaustive round-trip below is the real
        // bijection gate.
    }

    // --- vector -> icwrs -> index -> cwrsi -> vector round-trips --------------

    /// Enumerate every pulse vector of `sum(|y|)==k` in `n` dims and round-trip
    /// each through `icwrs`/`cwrsi`, asserting the index is in `[0, V)`, the
    /// recovered vector matches, and `Ryy == sum(y^2)`.
    fn exhaustive_round_trip(n: usize, k: usize) {
        let v = celt_pvq_v(n, k);
        let mut y = vec![0i32; n];

        // Round-trip one fully-populated vector: icwrs -> index -> cwrsi -> vector.
        fn check_one(n: usize, k: usize, y: &[i32], seen: &mut Vec<u32>) {
            let mut u = [0u32; MAX_PULSES + 2];
            let (idx, nc) = icwrs(n, k, y, &mut u[..k + 2]);
            let mut u2 = [0u32; MAX_PULSES + 2];
            assert_eq!(nc, ncwrs_urow(n, k, &mut u2[..k + 2]));
            assert!(idx < nc, "index {idx} >= V={nc} for {y:?}");
            seen.push(idx);
            ncwrs_urow(n, k, &mut u2[..k + 2]);
            let mut y2 = vec![0i32; n];
            let ryy = cwrsi(n, k as i32, idx, &mut y2, &mut u2[..k + 2]);
            assert_eq!(y2, *y, "cwrsi did not invert icwrs for {y:?} (idx {idx})");
            let want: f32 = y.iter().map(|&v| (v * v) as f32).sum();
            assert_eq!(ryy, want, "Ryy mismatch for {y:?}");
        }

        // Recursive enumeration of signed vectors with sum|y| == k (n, k are the
        // constants captured from the enclosing call).
        fn rec(pos: usize, left: i32, n: usize, k: usize, y: &mut [i32], seen: &mut Vec<u32>) {
            if pos == n - 1 {
                for &s in &[1i32, -1i32] {
                    y[pos] = s * left;
                    if left == 0 && s == -1 {
                        continue; // avoid -0 duplicate
                    }
                    check_one(n, k, y, seen);
                }
                return;
            }
            for mag in 0..=left {
                for &s in &[1i32, -1i32] {
                    y[pos] = s * mag;
                    if mag == 0 && s == -1 {
                        continue;
                    }
                    rec(pos + 1, left - mag, n, k, y, seen);
                }
            }
        }
        let mut seen = Vec::new();
        rec(0, k as i32, n, k, &mut y, &mut seen);
        // Every codeword index in [0, V) is hit exactly once (bijection).
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len() as u32,
            v,
            "V({n},{k})={v} but {} distinct indices",
            seen.len()
        );
    }

    #[test]
    fn cwrs_round_trips_small_cases() {
        for &(n, k) in &[
            (2usize, 1usize),
            (2, 2),
            (2, 3),
            (3, 1),
            (3, 2),
            (3, 3),
            (4, 1),
            (4, 2),
            (4, 3),
            (5, 2),
            (6, 2),
        ] {
            exhaustive_round_trip(n, k);
        }
    }

    // --- decode_pulses via a real range stream -------------------------------

    #[test]
    fn decode_pulses_yields_sum_abs_equals_k() {
        // Drive decode_pulses on assorted (N,K) with a fixed byte pattern; the
        // decoded vector must always satisfy sum(|y|)==K and Ryy==sum(y^2).
        let bytes = [
            0x9au8, 0x3c, 0xf1, 0x05, 0xd7, 0x2b, 0x88, 0x60, 0x41, 0xbe, 0x17, 0xe9,
        ];
        for &(n, k) in &[(2usize, 3i32), (3, 4), (4, 5), (8, 6), (16, 3), (24, 2)] {
            let mut rd = RangeDecoder::new(&bytes);
            let mut y = [0i32; MAX_BAND_N];
            let ryy = decode_pulses(&mut y[..n], n, k, &mut rd);
            let sum_abs: i32 = y[..n].iter().map(|v| v.abs()).sum();
            assert_eq!(sum_abs, k, "sum|y| != K for N={n} K={k}: {:?}", &y[..n]);
            let want: f32 = y[..n].iter().map(|&v| (v * v) as f32).sum();
            assert_eq!(ryy, want, "Ryy mismatch N={n} K={k}");
        }
    }
}
