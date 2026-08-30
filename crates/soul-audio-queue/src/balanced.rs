//! `balanced_shuffle` — an anti-clustering shuffle for BOUNDED contexts, where
//! the app supplies each item's group id (e.g. artist). Spreads same-group items
//! as evenly as possible while staying random-feeling. For library-scale
//! "shuffle everything", use the queue's keyed permutation instead.

use alloc::vec::Vec;

use crate::shuffle::shuffle_index;

/// A permutation of `0..n` spreading items with equal `group_of(i)` apart.
///
/// Method: bucket indices by group; shuffle within each bucket (keyed by `seed`);
/// then merge buckets by placing each bucket's items at evenly-spaced fractional
/// positions in `[0, 1)` (offset per bucket) and sorting — a dithering spread.
#[must_use]
pub fn balanced_shuffle<F: Fn(usize) -> u64>(n: usize, seed: u64, group_of: F) -> Vec<usize> {
    if n <= 1 {
        return (0..n).collect();
    }
    // 1. Bucket by group id, preserving a stable first-seen group order.
    let mut group_ids: Vec<u64> = Vec::new();
    let mut buckets: Vec<Vec<usize>> = Vec::new();
    for i in 0..n {
        let g = group_of(i);
        match group_ids.iter().position(|&x| x == g) {
            Some(b) => buckets[b].push(i),
            None => {
                group_ids.push(g);
                buckets.push(alloc::vec![i]);
            }
        }
    }
    // 2. Shuffle within each bucket (keyed), so same-group order isn't stable.
    for (b, bucket) in buckets.iter_mut().enumerate() {
        let m = bucket.len();
        let key = seed ^ ((b as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let shuffled: Vec<usize> = (0..m).map(|k| bucket[shuffle_index(key, k, m)]).collect();
        *bucket = shuffled;
    }
    // 3. Assign each item a fractional position in [0, 1): item k of a bucket of
    //    size m sits at (k + offset) / m, with a per-bucket offset in [0, 1) so
    //    buckets don't all pin to 0. Sort by fractional position.
    // NOTE: `num.wrapping_mul(SCALE)` below can overflow `u64` for a single group
    // larger than roughly 4096 items (num ~ m^2, and m^2 * SCALE > u64::MAX past
    // that size). The wrap keeps `placed` a valid permutation either way — sort
    // order just gets noisier, degrading spread quality, not correctness.
    let mut placed: Vec<(u64, usize)> = Vec::with_capacity(n); // (fixed-point pos, item)
    const SCALE: u64 = 1 << 40;
    for (b, bucket) in buckets.iter().enumerate() {
        let m = bucket.len() as u64;
        // deterministic per-bucket offset in [0, m) from the key
        let off = shuffle_index(seed ^ (b as u64).wrapping_add(1), 0, m.max(1) as usize) as u64;
        for (k, &item) in bucket.iter().enumerate() {
            // position = (k + off/m) / m  == (k*m + off) / (m*m), scaled
            let num = (k as u64) * m + off;
            let pos = num.wrapping_mul(SCALE) / (m * m).max(1);
            placed.push((pos, item));
        }
    }
    placed.sort_by_key(|&(p, _)| p);
    placed.into_iter().map(|(_, item)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_a_permutation() {
        let order = balanced_shuffle(20, 123, |i| (i % 4) as u64);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn spreads_same_group_better_than_grouped_input() {
        // 3 groups of 6, interleaved output should have few same-group adjacencies.
        let n = 18;
        let order = balanced_shuffle(n, 7, |i| (i / 6) as u64); // groups: 0..6, 6..12, 12..18
        let group = |idx: usize| idx / 6;
        let adj = order
            .windows(2)
            .filter(|w| group(w[0]) == group(w[1]))
            .count();
        // Grouped input would have 15 same-group adjacencies; a good spread has few.
        assert!(adj <= 4, "too many same-group adjacencies: {adj}");
    }

    #[test]
    fn trivial_sizes() {
        assert_eq!(balanced_shuffle(0, 1, |_| 0), Vec::<usize>::new());
        assert_eq!(balanced_shuffle(1, 1, |_| 0), alloc::vec![0]);
    }
}
