//! A keyed pseudo-random permutation over `[0, n)`, seeded by a `u64`, evaluated
//! positionally with O(1) time and space (no stored permutation array). Used to
//! shuffle a source context of any size. Built from a small Feistel network over
//! a power-of-two domain plus cycle-walking down to `n`.

const ROUNDS: u32 = 4;

/// A strong 64-bit mix (splitmix64 finalizer) of the round key material.
fn mix(seed: u64, round: u32, r: u64) -> u64 {
    let mut z = seed
        .wrapping_add(0x9e37_79b9_7f4a_7c15u64.wrapping_mul(round as u64 + 1))
        .wrapping_add(r.wrapping_mul(0xff51_afd7_ed55_8ccd));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Number of bits to represent `[0, n)`, rounded UP to an even number so the
/// Feistel halves are equal. `n >= 2`.
fn even_bits(n: usize) -> u32 {
    let need = 64 - (n as u64 - 1).leading_zeros();
    let need = need.max(1);
    if need % 2 == 0 { need } else { need + 1 }
}

/// Feistel encrypt over the `2*half`-bit domain.
fn feistel(seed: u64, half: u32, x: u64) -> u64 {
    let mask = (1u64 << half) - 1;
    let mut l = x >> half;
    let mut r = x & mask;
    for round in 0..ROUNDS {
        let f = mix(seed, round, r) & mask;
        let nl = r;
        let nr = l ^ f;
        l = nl;
        r = nr;
    }
    (l << half) | r
}

/// Feistel decrypt (exact inverse of `feistel`).
fn feistel_inv(seed: u64, half: u32, y: u64) -> u64 {
    let mask = (1u64 << half) - 1;
    let mut l = y >> half;
    let mut r = y & mask;
    for round in (0..ROUNDS).rev() {
        // forward was: nl=r_prev, nr=l_prev ^ f(r_prev). Given (l=nl, r=nr):
        // r_prev = l ; l_prev = r ^ f(round, r_prev)
        let f = mix(seed, round, l) & mask;
        let pl = r ^ f;
        let pr = l;
        l = pl;
        r = pr;
    }
    (l << half) | r
}

/// The shuffled position of `i` under `seed` over `[0, n)`. Bijection on `[0, n)`.
#[must_use]
pub fn shuffle_index(seed: u64, i: usize, n: usize) -> usize {
    if n <= 1 {
        return i;
    }
    let half = even_bits(n) / 2;
    let mut x = i as u64;
    loop {
        x = feistel(seed, half, x);
        if (x as usize) < n {
            return x as usize;
        }
    }
}

/// Inverse of [`shuffle_index`]: `unshuffle_index(s, shuffle_index(s, i, n), n) == i`.
#[must_use]
pub fn unshuffle_index(seed: u64, j: usize, n: usize) -> usize {
    if n <= 1 {
        return j;
    }
    let half = even_bits(n) / 2;
    let mut x = j as u64;
    loop {
        x = feistel_inv(seed, half, x);
        if (x as usize) < n {
            return x as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The crate compiles as std under cfg(test), so `Vec`/`vec!` come from the
    // std prelude — no `extern crate alloc` needed.

    fn assert_bijection(seed: u64, n: usize) {
        let mut seen = vec![false; n];
        for i in 0..n {
            let s = shuffle_index(seed, i, n);
            assert!(s < n, "n={n} seed={seed} i={i} -> {s} out of range");
            assert!(!seen[s], "n={n} seed={seed}: collision at {s}");
            seen[s] = true;
            assert_eq!(
                unshuffle_index(seed, s, n),
                i,
                "inverse mismatch n={n} i={i}"
            );
        }
        assert!(seen.into_iter().all(|b| b), "not a full permutation n={n}");
    }

    #[test]
    fn is_a_bijection_across_sizes_and_seeds() {
        for &n in &[1usize, 2, 3, 4, 7, 8, 9, 16, 17, 31, 100, 256, 1000, 4095] {
            for &seed in &[0u64, 1, 42, 0xdead_beef, u64::MAX] {
                assert_bijection(seed, n);
            }
        }
    }

    #[test]
    fn seed_zero_is_still_a_permutation_not_identity_for_large_n() {
        // Not asserting non-identity strictly, but the round keys depend on the
        // position, so seed 0 still permutes; just re-check bijection at a big n.
        assert_bijection(0, 4096);
    }

    #[test]
    fn even_bits_domain_covers_n_and_is_even() {
        for &n in &[
            2usize, 3, 4, 5, 8, 9, 16, 17, 100, 255, 256, 1000, 4096, 65_537,
        ] {
            let b = even_bits(n);
            assert!(b % 2 == 0, "n={n}: bits {b} not even");
            assert!(
                (1u64 << b) >= n as u64,
                "n={n}: domain 2^{b} < n (cycle-walk would not terminate)"
            );
            // Not wastefully large: one even step below must be too small.
            assert!(
                b == 2 || (1u64 << (b - 2)) < n as u64,
                "n={n}: domain 2^{b} larger than necessary"
            );
        }
    }
}
