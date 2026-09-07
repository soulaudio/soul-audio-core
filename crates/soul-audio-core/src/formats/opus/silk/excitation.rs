//! SILK excitation decode (RFC 6716 §4.2.7.8): shell-coded pulse locations,
//! LSB magnitude extension, sign coding, and the excitation reconstruction
//! (LCG pseudo-random sign flip + quantization offset).
//!
//! Transcribed line-for-line from libopus **v1.5.2**: `silk/decode_pulses.c`
//! (rate level + pulse counts + LSB escapes + LSB decode), `silk/shell_coder.c`
//! (`silk_shell_decoder`, the combinatorial split — the classic SILK decode
//! bug source), `silk/code_signs.c` (`silk_decode_signs`), and the excitation
//! reconstruction loop of `silk/decode_core.c` (`silk_decode_core`). Pure
//! integer math — the LCG uses intentionally-modular `wrapping_*` `u32`.
//!
//! This stops at the integer/`Q14` excitation residual `e[]`; gain scaling and
//! the LTP+LPC synthesis filtering are Task 11.

use super::silk_tables::{
    LSB_ICDF, PULSES_PER_BLOCK_ICDF, QUANTIZATION_OFFSETS_Q10, RATE_LEVELS_ICDF,
    SHELL_CODE_TABLE_OFFSETS, SHELL_CODE_TABLE0, SHELL_CODE_TABLE1, SHELL_CODE_TABLE2,
    SHELL_CODE_TABLE3, SIGN_ICDF,
};
use super::{
    LOG2_SHELL_CODEC_FRAME_LENGTH, MAX_NB_SHELL_BLOCKS, N_RATE_LEVELS, QUANT_LEVEL_ADJUST_Q10,
    SHELL_CODEC_FRAME_LENGTH, SILK_MAX_PULSES, lshift, silk_rand,
};
use crate::formats::opus::range::RangeDecoder;

/// One recursion level of `silk_shell_decoder`: split a parent's `p` pulses
/// into two halves. `p_child1` is read from the shell-code iCDF selected by
/// the parent count `p` (via `silk_shell_code_table_offsets`), and
/// `p_child2 = p - p_child1`. `p == 0` yields `(0, 0)` with no bitstream read.
#[inline]
fn decode_split(rd: &mut RangeDecoder, p: i32, table: &[u8]) -> (i32, i32) {
    if p > 0 {
        let off = SHELL_CODE_TABLE_OFFSETS
            .get(p as usize)
            .copied()
            .unwrap_or(0) as usize;
        // `&table[off..]` is the parent-`p` iCDF; it terminates in the mandatory
        // 0 within the 152-byte table, so `decode_icdf`'s loop is bounded.
        let icdf = table.get(off..).unwrap_or(&[0]);
        let c1 = rd.decode_icdf(icdf, 8) as i32;
        (c1, p - c1)
    } else {
        (0, 0)
    }
}

/// `silk_shell_decoder` (silk/shell_coder.c): reconstruct the 16 nonnegative
/// pulse magnitudes of one shell-code block from the total `pulses4`. The tree
/// is 16→8→4→2→1: `pulses4` splits into two 8-sums, each into two 4-sums, and
/// so on down to individual samples, using `silk_shell_code_table{3,2,1,0}` at
/// levels selecting 2, 4, 8, 16-wide nodes respectively. The exact recursion
/// structure, per-level table, and offset indexing must match libopus byte for
/// byte — this is the named bug source for the task.
fn shell_decode(pulses0: &mut [i16], rd: &mut RangeDecoder, pulses4: i32) {
    let mut p3 = [0i32; 2];
    let mut p2 = [0i32; 4];
    let mut p1 = [0i32; 8];

    (p3[0], p3[1]) = decode_split(rd, pulses4, &SHELL_CODE_TABLE3);

    (p2[0], p2[1]) = decode_split(rd, p3[0], &SHELL_CODE_TABLE2);

    (p1[0], p1[1]) = decode_split(rd, p2[0], &SHELL_CODE_TABLE1);
    set2(pulses0, 0, decode_split(rd, p1[0], &SHELL_CODE_TABLE0));
    set2(pulses0, 2, decode_split(rd, p1[1], &SHELL_CODE_TABLE0));

    (p1[2], p1[3]) = decode_split(rd, p2[1], &SHELL_CODE_TABLE1);
    set2(pulses0, 4, decode_split(rd, p1[2], &SHELL_CODE_TABLE0));
    set2(pulses0, 6, decode_split(rd, p1[3], &SHELL_CODE_TABLE0));

    (p2[2], p2[3]) = decode_split(rd, p3[1], &SHELL_CODE_TABLE2);

    (p1[4], p1[5]) = decode_split(rd, p2[2], &SHELL_CODE_TABLE1);
    set2(pulses0, 8, decode_split(rd, p1[4], &SHELL_CODE_TABLE0));
    set2(pulses0, 10, decode_split(rd, p1[5], &SHELL_CODE_TABLE0));

    (p1[6], p1[7]) = decode_split(rd, p2[3], &SHELL_CODE_TABLE1);
    set2(pulses0, 12, decode_split(rd, p1[6], &SHELL_CODE_TABLE0));
    set2(pulses0, 14, decode_split(rd, p1[7], &SHELL_CODE_TABLE0));
}

/// Store a leaf split `(child1, child2)` into `pulses0[i]`, `pulses0[i+1]`
/// (guarded so a short block slice can't panic).
#[inline]
fn set2(pulses0: &mut [i16], i: usize, (c1, c2): (i32, i32)) {
    if let Some(s) = pulses0.get_mut(i) {
        *s = c1 as i16;
    }
    if let Some(s) = pulses0.get_mut(i + 1) {
        *s = c2 as i16;
    }
}

/// `silk_decode_pulses` (silk/decode_pulses.c): decode the whole frame's
/// excitation magnitudes (unsigned + sign) into `pulses[..frame_length]`.
/// `pulses` must be at least `MAX_FRAME_LENGTH` long (the final shell block can
/// extend past `frame_length` for the 12 kHz 10 ms case, exactly as libopus
/// writes into its `MAX_FRAME_LENGTH` buffer).
pub(super) fn decode_pulses(
    rd: &mut RangeDecoder,
    pulses: &mut [i16],
    signal_type: i32,
    quant_offset_type: i32,
    frame_length: usize,
) {
    let mut sum_pulses = [0i32; MAX_NB_SHELL_BLOCKS];
    let mut n_lshifts = [0i32; MAX_NB_SHELL_BLOCKS];

    // Decode rate level (iCDF selected by signal type).
    let rl_row = ((signal_type >> 1).clamp(0, 1)) as usize;
    let rate_level = rd.decode_icdf(&RATE_LEVELS_ICDF[rl_row], 8) as usize;

    // Number of 16-sample shell blocks (the +1 only fires for 12 kHz 10 ms).
    let mut iter = frame_length >> LOG2_SHELL_CODEC_FRAME_LENGTH;
    if iter * SHELL_CODEC_FRAME_LENGTH < frame_length {
        iter += 1;
    }
    iter = iter.min(MAX_NB_SHELL_BLOCKS);

    // Sum-weighted-pulses decode, with the LSB (escape) indication loop.
    let cdf = &PULSES_PER_BLOCK_ICDF[rate_level.min(N_RATE_LEVELS - 1)];
    for i in 0..iter {
        n_lshifts[i] = 0;
        sum_pulses[i] = rd.decode_icdf(cdf, 8) as i32;
        while sum_pulses[i] == SILK_MAX_PULSES + 1 {
            n_lshifts[i] += 1;
            // Once 10 LSBs are read, shift the escape iCDF by one so a further
            // (SILK_MAX_PULSES + 1) escape can't be signalled.
            let off = usize::from(n_lshifts[i] == 10);
            let esc = &PULSES_PER_BLOCK_ICDF[N_RATE_LEVELS - 1][off..];
            sum_pulses[i] = rd.decode_icdf(esc, 8) as i32;
        }
    }

    // Shell decode each block (or zero it when it holds no pulses).
    for (i, &sp) in sum_pulses.iter().enumerate().take(iter) {
        let base = i * SHELL_CODEC_FRAME_LENGTH;
        if let Some(block) = pulses.get_mut(base..base + SHELL_CODEC_FRAME_LENGTH) {
            if sp > 0 {
                shell_decode(block, rd, sp);
            } else {
                block.fill(0);
            }
        }
    }

    // LSB decode: extend each magnitude by the `n_lshifts[i]` decoded LSBs
    // (MSB-first), then mark the pulse count nonzero for sign decoding.
    for i in 0..iter {
        if n_lshifts[i] > 0 {
            let n_ls = n_lshifts[i];
            let base = i * SHELL_CODEC_FRAME_LENGTH;
            for k in 0..SHELL_CODEC_FRAME_LENGTH {
                if let Some(slot) = pulses.get_mut(base + k) {
                    let mut abs_q = i32::from(*slot);
                    for _ in 0..n_ls {
                        abs_q = lshift(abs_q, 1) + rd.decode_icdf(&LSB_ICDF, 8) as i32;
                    }
                    *slot = abs_q as i16;
                }
            }
            sum_pulses[i] |= n_ls << 5;
        }
    }

    // Decode and attach signs.
    decode_signs(
        rd,
        pulses,
        frame_length,
        signal_type,
        quant_offset_type,
        &sum_pulses,
    );
}

/// `silk_decode_signs` (silk/code_signs.c): for every strictly-positive pulse,
/// read one sign bit and map it to ±1 via `silk_dec_map(a) = (a << 1) - 1`.
/// The sign iCDF row is selected by `7 * (quantOffsetType + (signalType << 1))`
/// and the column by `min(sum_pulses & 0x1F, 6)`.
fn decode_signs(
    rd: &mut RangeDecoder,
    pulses: &mut [i16],
    length: usize,
    signal_type: i32,
    quant_offset_type: i32,
    sum_pulses: &[i32],
) {
    let base_i = 7 * (quant_offset_type + lshift(signal_type, 1));
    let n_blocks = (length + SHELL_CODEC_FRAME_LENGTH / 2) >> LOG2_SHELL_CODEC_FRAME_LENGTH;
    for i in 0..n_blocks {
        let p = sum_pulses.get(i).copied().unwrap_or(0);
        if p > 0 {
            let col = (p & 0x1F).min(6);
            let icdf0 = SIGN_ICDF
                .get((base_i + col).max(0) as usize)
                .copied()
                .unwrap_or(0);
            let icdf = [icdf0, 0u8];
            let block = i * SHELL_CODEC_FRAME_LENGTH;
            for j in 0..SHELL_CODEC_FRAME_LENGTH {
                if let Some(slot) = pulses.get_mut(block + j) {
                    if *slot > 0 {
                        // q *= silk_dec_map( ec_dec_icdf ) : 0 -> -1, 1 -> +1.
                        let bit = rd.decode_icdf(&icdf, 8) as i32;
                        let s = lshift(bit, 1) - 1;
                        *slot = (i32::from(*slot) * s) as i16;
                    }
                }
            }
        }
    }
}

/// Excitation reconstruction from `silk_decode_core` (silk/decode_core.c): turn
/// the signed pulses into the `Q14` excitation residual `exc_Q14[]`, applying
/// the quantization offset and the LCG pseudo-random sign flip. Leaves gain
/// scaling + LTP/LPC synthesis to Task 11.
pub(super) fn reconstruct_excitation(
    exc_q14: &mut [i32],
    pulses: &[i16],
    seed: i32,
    signal_type: i32,
    quant_offset_type: i32,
    frame_length: usize,
) {
    let row = ((signal_type >> 1).clamp(0, 1)) as usize;
    let col = quant_offset_type.clamp(0, 1) as usize;
    let offset_q10 = i32::from(QUANTIZATION_OFFSETS_Q10[row][col]);
    let n = frame_length.min(pulses.len()).min(exc_q14.len());
    let mut rand_seed = seed;
    for i in 0..n {
        rand_seed = silk_rand(rand_seed);
        let mut e = lshift(i32::from(pulses[i]), 14);
        if e > 0 {
            e -= QUANT_LEVEL_ADJUST_Q10 << 4;
        } else if e < 0 {
            e += QUANT_LEVEL_ADJUST_Q10 << 4;
        }
        e += offset_q10 << 4;
        if rand_seed < 0 {
            e = -e;
        }
        exc_q14[i] = e;
        // silk_ADD32_ovflw( rand_seed, pulses[i] ) — modular u32 accumulate.
        rand_seed = rand_seed.wrapping_add(i32::from(pulses[i]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a 16-sample shell block from a crafted byte buffer with a known
    /// pulse count, returning the 16 magnitudes.
    fn shell_of(count: i32, bytes: &[u8]) -> [i16; 16] {
        let mut rd = RangeDecoder::new(bytes);
        let mut p = [0i16; 16];
        shell_decode(&mut p, &mut rd, count);
        p
    }

    #[test]
    fn shell_split_matches_libopus_oracle() {
        // Golden vectors from the verbatim libopus v1.5.2 `silk_shell_decoder`
        // gcc oracle (see the Task 10 report) on crafted range-coder buffers.
        // Each split's pulses sum back to the input count — the shell tree +
        // per-level table selection must match libopus exactly.
        assert_eq!(
            shell_of(5, &[130, 90, 200, 15]),
            [0, 0, 0, 0, 1, 1, 0, 1, 0, 0, 0, 0, 1, 0, 0, 1]
        );
        assert_eq!(
            shell_of(16, &[200, 10, 55, 120, 7, 240]),
            [4, 1, 0, 0, 1, 2, 1, 1, 0, 0, 1, 1, 0, 0, 1, 3]
        );
        assert_eq!(
            shell_of(3, &[44, 201, 9, 17]),
            [0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0, 1, 0, 0]
        );
        assert_eq!(
            shell_of(10, &[255, 1, 128, 64, 32, 200, 7]),
            [0, 0, 0, 0, 3, 2, 2, 3, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn shell_split_always_conserves_pulse_count() {
        // For any count/bytes, the decoded magnitudes must sum to the count.
        for &count in &[1i32, 2, 3, 5, 8, 11, 16] {
            for seed in 0u8..16 {
                let bytes = [
                    seed.wrapping_mul(37),
                    seed ^ 0xA5,
                    0x0F,
                    seed.wrapping_add(3),
                ];
                let p = shell_of(count, &bytes);
                let sum: i32 = p.iter().map(|&x| i32::from(x)).sum();
                assert_eq!(sum, count, "count={count} seed={seed}");
            }
        }
    }

    #[test]
    fn zero_count_block_reads_nothing_and_is_all_zero() {
        assert_eq!(shell_of(0, &[123, 45, 67]), [0i16; 16]);
    }
}
