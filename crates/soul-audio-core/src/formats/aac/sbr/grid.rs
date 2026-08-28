//! `sbr_grid()` — the per-channel time/frequency envelope grid
//! (`sbr-reference.md` §4 / ISO Table 4.61). Reads `bs_frame_class` then one
//! of four class-specific border layouts (FIXFIX/FIXVAR/VARFIX/VARVAR),
//! then derives `L_E`/`L_Q`/`t_env`/`t_noise`/`l_a` via the shared
//! post-processing all four classes go through alike.
//!
//! §4 is terse on the exact per-class arithmetic for the middle noise
//! border and `l_a` (the "short envelope" index), so this file is verified
//! line-for-line against the authoritative decode, FFmpeg
//! `libavcodec/aacsbr_template.c::read_sbr_grid` (confirmed against the
//! live `master` source, including its `ceil_log2[] = {0,1,2,2,3,3}` table,
//! doc-commented there as `ceil(log2(index+1))`). Each derived value below
//! cites the exact FFmpeg expression it mirrors.
//!
//! Borders (`t_env`/`t_noise`) are raw slot units, `0..=numTimeSlots` (16) —
//! not yet RATE-scaled (Task 4's resolved decision; the RATE multiply
//! happens where these borders are consumed, starting Task 6).

use super::bitstream::clamped_read;
use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;

pub(super) const FIXFIX: u8 = 0;
pub(super) const FIXVAR: u8 = 1;
pub(super) const VARFIX: u8 = 2;
pub(super) const VARVAR: u8 = 3;

/// `numTimeSlots` (§4): the fixed time-slot span every SBR frame's grid
/// covers, regardless of frame class.
const NUM_TIME_SLOTS: u8 = 16;

/// `ceil_log2[n] = ceil(log2(n+1))` — `bs_pointer`'s read width for a given
/// `bs_num_env = n` (FFmpeg `aacsbr_template.c`'s own table, doc-commented
/// there `/** ceil(log2(index+1)) */`). Indexed `0..=5` to cover every
/// `L_E` this decoder accepts (`1..=5`).
const CEIL_LOG2: [u32; 6] = [0, 1, 2, 2, 3, 3];

/// Parsed `sbr_grid()` fields (§4), post-derivation: `t_env`/`t_noise` are
/// already resolved to absolute raw-slot borders (not the raw
/// `bs_abs_bord`/`bs_rel_bord` bitstream fields), and `l_a`/the middle
/// noise border have already been picked per the class-specific rule.
#[derive(Clone, Copy, Default)]
pub(super) struct SbrGrid {
    pub frame_class: u8,   // 0..3 — one of FIXFIX/FIXVAR/VARFIX/VARVAR
    pub num_env: u8,       // L_E, 1..5
    pub num_noise: u8,     // L_Q, 1..2
    pub t_env: [u8; 6],    // raw-slot borders, valid indices 0..=L_E
    pub t_noise: [u8; 3],  // raw-slot borders, valid indices 0..=L_Q
    pub freq_res: [u8; 6], // per envelope, LO_RES=0 / HI_RES=1
    // `bs_pointer` is fully consumed inside this module (it derives
    // `l_a`/the middle noise border already folded into `t_noise` above);
    // no downstream consumer re-reads the raw value off the struct, but it
    // stays as a field — mirroring the ISO §4 `sbr_grid()` output shape —
    // so this module's own tests can assert the parsed value directly.
    #[allow(dead_code)]
    pub pointer: u8, // bs_pointer
    pub l_a: i8, // "short envelope" index, -1 when none
}

/// Read `n` bits, clamped to `end_bit`; a read that would cross it becomes
/// `Err(DecodeError::Malformed)` (caught upstream -> upsample-only
/// fallback). Mirrors `header::read_bits` — each SBR sub-parser keeps its
/// own thin wrapper around the shared [`clamped_read`] clamp.
fn read_bits(bits: &mut BitReader, end_bit: usize, n: u32) -> Result<u32, DecodeError> {
    clamped_read(bits, end_bit, n).ok_or(DecodeError::Malformed)
}

/// One `bs_rel_bord` entry: `2 * read(2) + 2`, i.e. one of `{2,4,6,8}` —
/// every class's relative-border encoding (§4).
fn read_rel_border(bits: &mut BitReader, end_bit: usize) -> Result<i32, DecodeError> {
    Ok(2 * read_bits(bits, end_bit, 2)? as i32 + 2)
}

/// Write `t_env[idx] = value`, rejecting rather than panicking if `idx`
/// doesn't fit the fixed `[u8; 6]` backing array or `value` doesn't fit a
/// `u8` (e.g. a negative border from a malformed relative-border chain).
/// Defense-in-depth: every caller's `idx`/`value` are already bounded by a
/// validated `bs_num_env <= 5` and the `{2,4,6,8}` relative-border range,
/// but a fixed-size array write must never be allowed out of bounds
/// regardless of how well-reasoned the caller's math is.
fn set_t_env(t_env: &mut [u8; 6], idx: usize, value: i32) -> Result<(), DecodeError> {
    let slot = t_env.get_mut(idx).ok_or(DecodeError::Malformed)?;
    *slot = u8::try_from(value).map_err(|_| DecodeError::Malformed)?;
    Ok(())
}

/// The class-specific parse's raw output, before the shared post-processing
/// (monotonicity check, `L_Q`/`t_noise`/`l_a` derivation) below.
struct RawGrid {
    num_env: u8,
    t_env: [u8; 6],
    freq_res: [u8; 6],
    pointer: u8,
}

/// FIXFIX (§4): `bs_num_env = 1 << read(2)` (reject `>5` — non-USAC caps at
/// 4; FFmpeg: `if (bs_num_env > (usac ? 8 : 5)) return -1`), one
/// `bs_freq_res` for every envelope, interior borders equally spaced.
/// FFmpeg's exact spacing (`read_sbr_grid`'s `FIXFIX` case):
/// `step = (numTimeSlots + (bs_num_env>>1)) / bs_num_env`, then
/// `t_env[i+1] = t_env[i] + step`. `bs_num_env` is always a power of two
/// here (1/2/4), so this is exactly equally-spaced (`step = 16/L_E`); the
/// step formula is kept (not simplified to `i*16/L_E`) to mirror FFmpeg
/// bit-for-bit. Never reads `bs_pointer` (stays 0).
fn parse_fixfix(bits: &mut BitReader, end_bit: usize) -> Result<RawGrid, DecodeError> {
    let pow = read_bits(bits, end_bit, 2)?;
    let num_env_u32 = 1u32 << pow; // 1, 2, 4, or 8
    if num_env_u32 == 0 || num_env_u32 > 5 {
        return Err(DecodeError::Malformed);
    }
    let num_env = num_env_u32 as u8;

    let mut t_env = [0u8; 6];
    set_t_env(&mut t_env, 0, 0)?;
    set_t_env(&mut t_env, num_env as usize, NUM_TIME_SLOTS as i32)?;

    let step = (NUM_TIME_SLOTS as i32 + (num_env as i32 >> 1)) / num_env as i32;
    for i in 0..(num_env as usize - 1) {
        let val = t_env[i] as i32 + step;
        set_t_env(&mut t_env, i + 1, val)?;
    }

    let freq_res_bit = read_bits(bits, end_bit, 1)? as u8;
    let mut freq_res = [0u8; 6];
    for slot in freq_res.iter_mut().take(num_env as usize) {
        *slot = freq_res_bit;
    }

    Ok(RawGrid {
        num_env,
        t_env,
        freq_res,
        pointer: 0,
    })
}

/// FIXVAR (§4): trailing border fixed relative to `numTimeSlots`
/// (`bs_abs_bord = read(2) + numTimeSlots`), leading borders built
/// *downward* from it (`bs_rel_bord` chain), `bs_pointer` width
/// `ceil_log2[num_env]`, `bs_freq_res` read **reversed** (last envelope
/// first — FFmpeg's `bs_freq_res[bs_num_env - i]` for `i` ascending, which
/// this function un-reverses into forward-indexed `freq_res[0..num_env]` as
/// it reads).
fn parse_fixvar(bits: &mut BitReader, end_bit: usize) -> Result<RawGrid, DecodeError> {
    let abs_bord_trail = NUM_TIME_SLOTS as i32 + read_bits(bits, end_bit, 2)? as i32;
    let num_rel_trail = read_bits(bits, end_bit, 2)? as usize; // 0..3
    let num_env = (num_rel_trail + 1) as u8; // 1..4

    let mut t_env = [0u8; 6];
    set_t_env(&mut t_env, 0, 0)?;
    set_t_env(&mut t_env, num_env as usize, abs_bord_trail)?;
    for i in 0..num_rel_trail {
        let rel = read_rel_border(bits, end_bit)?;
        let src = *t_env
            .get(num_env as usize - i)
            .ok_or(DecodeError::Malformed)? as i32;
        set_t_env(&mut t_env, num_env as usize - 1 - i, src - rel)?;
    }

    let pointer_width = *CEIL_LOG2
        .get(num_env as usize)
        .ok_or(DecodeError::Malformed)?;
    let pointer = read_bits(bits, end_bit, pointer_width)? as u8;

    let mut freq_res = [0u8; 6];
    for i in 0..num_env as usize {
        let bit = read_bits(bits, end_bit, 1)? as u8;
        let env_idx = num_env as usize - 1 - i;
        *freq_res.get_mut(env_idx).ok_or(DecodeError::Malformed)? = bit;
    }

    Ok(RawGrid {
        num_env,
        t_env,
        freq_res,
        pointer,
    })
}

/// VARFIX (§4): symmetric to FIXVAR — leading border free (`bs_abs_bord =
/// read(2)`), trailing border fixed at `numTimeSlots`, interior borders
/// built *upward* via the `bs_rel_bord` chain, `bs_freq_res` read forward
/// (envelope 0 first).
fn parse_varfix(bits: &mut BitReader, end_bit: usize) -> Result<RawGrid, DecodeError> {
    let t0 = read_bits(bits, end_bit, 2)? as i32;
    let num_rel_lead = read_bits(bits, end_bit, 2)? as usize; // 0..3
    let num_env = (num_rel_lead + 1) as u8; // 1..4

    let mut t_env = [0u8; 6];
    set_t_env(&mut t_env, 0, t0)?;
    set_t_env(&mut t_env, num_env as usize, NUM_TIME_SLOTS as i32)?;
    for i in 0..num_rel_lead {
        let rel = read_rel_border(bits, end_bit)?;
        let src = t_env[i] as i32;
        set_t_env(&mut t_env, i + 1, src + rel)?;
    }

    let pointer_width = *CEIL_LOG2
        .get(num_env as usize)
        .ok_or(DecodeError::Malformed)?;
    let pointer = read_bits(bits, end_bit, pointer_width)? as u8;

    let mut freq_res = [0u8; 6];
    for slot in freq_res.iter_mut().take(num_env as usize) {
        *slot = read_bits(bits, end_bit, 1)? as u8;
    }

    Ok(RawGrid {
        num_env,
        t_env,
        freq_res,
        pointer,
    })
}

/// VARVAR (§4): both borders free (`bs_abs_bord_0 = read(2)`,
/// `bs_abs_bord_1 = read(2) + numTimeSlots`), `bs_num_rel_0` borders built
/// upward from the leading border, `bs_num_rel_1` borders built downward
/// from the trailing border (disjoint interior index ranges — leading fills
/// `1..=num_rel_0`, trailing fills `(num_rel_0+1)..=num_env-1`), rejecting
/// `bs_num_env = bs_num_rel_0 + bs_num_rel_1 + 1 > 5` (the one class where
/// the naive sum can exceed the cap, since FIXVAR/VARFIX's single
/// `read(2)` count is already bounded to `<=4`). `bs_freq_res` read forward.
fn parse_varvar(bits: &mut BitReader, end_bit: usize) -> Result<RawGrid, DecodeError> {
    let t0 = read_bits(bits, end_bit, 2)? as i32;
    let abs_bord_trail = NUM_TIME_SLOTS as i32 + read_bits(bits, end_bit, 2)? as i32;
    let num_rel_lead = read_bits(bits, end_bit, 2)? as usize; // 0..3
    let num_rel_trail = read_bits(bits, end_bit, 2)? as usize; // 0..3

    let num_env_raw = num_rel_lead + num_rel_trail + 1;
    if num_env_raw > 5 {
        return Err(DecodeError::Malformed);
    }
    let num_env = num_env_raw as u8;

    let mut t_env = [0u8; 6];
    set_t_env(&mut t_env, 0, t0)?;
    set_t_env(&mut t_env, num_env as usize, abs_bord_trail)?;
    for i in 0..num_rel_lead {
        let rel = read_rel_border(bits, end_bit)?;
        let src = t_env[i] as i32;
        set_t_env(&mut t_env, i + 1, src + rel)?;
    }
    for i in 0..num_rel_trail {
        let rel = read_rel_border(bits, end_bit)?;
        let src = *t_env
            .get(num_env as usize - i)
            .ok_or(DecodeError::Malformed)? as i32;
        set_t_env(&mut t_env, num_env as usize - 1 - i, src - rel)?;
    }

    let pointer_width = *CEIL_LOG2
        .get(num_env as usize)
        .ok_or(DecodeError::Malformed)?;
    let pointer = read_bits(bits, end_bit, pointer_width)? as u8;

    let mut freq_res = [0u8; 6];
    for slot in freq_res.iter_mut().take(num_env as usize) {
        *slot = read_bits(bits, end_bit, 1)? as u8;
    }

    Ok(RawGrid {
        num_env,
        t_env,
        freq_res,
        pointer,
    })
}

/// The middle noise border's index into `t_env` (§4 `middleBorder`, only
/// consulted when `L_Q == 2`). FFmpeg `read_sbr_grid`'s exact per-class
/// `idx` arithmetic:
/// ```text
/// FIXFIX            : idx = num_env >> 1
/// FIXVAR or VARVAR   : idx = num_env - max(pointer - 1, 1)
/// VARFIX             : pointer==0 -> 1 ; pointer==1 -> num_env-1 ; else pointer-1
/// ```
/// (FFmpeg dispatches the middle two on `frame_class & 1` — true for
/// FIXVAR(1)/VARVAR(3) — and the last on `frame_class == VARFIX(2)`, with
/// FIXFIX(0) handled separately above; this mirrors that exactly.)
fn middle_noise_border_idx(frame_class: u8, num_env: u8, pointer: u8) -> usize {
    let num_env = num_env as i32;
    let pointer = pointer as i32;
    let idx = if frame_class == FIXFIX {
        num_env >> 1
    } else if frame_class & 1 == 1 {
        // FIXVAR or VARVAR
        num_env - (pointer - 1).max(1)
    } else {
        // VARFIX
        if pointer == 0 {
            1
        } else if pointer == 1 {
            num_env - 1
        } else {
            pointer - 1
        }
    };
    idx.max(0) as usize
}

/// `l_a`, the "short envelope" index (§4): default `-1` (no short
/// envelope); FIXVAR/VARVAR set it when `bs_pointer != 0`
/// (`num_env + 1 - pointer`); VARFIX sets it when `bs_pointer > 1`
/// (`pointer - 1`); FIXFIX never sets it. Mirrors FFmpeg's `e_a[1]`
/// computation in `read_sbr_grid` exactly (this function is the single-
/// frame `e_a[1]`; `e_a[0]`, the *previous* frame's carried-over value used
/// by envelope-prediction, has no home in this stateless per-frame
/// `SbrGrid` and is left to whichever later task needs cross-frame state).
fn compute_l_a(frame_class: u8, num_env: u8, pointer: u8) -> i8 {
    if frame_class & 1 == 1 && pointer != 0 {
        (num_env as i32 + 1 - pointer as i32) as i8
    } else if frame_class == VARFIX && pointer > 1 {
        (pointer as i32 - 1) as i8
    } else {
        -1
    }
}

/// Parse `sbr_grid()` (§4). Reads `bs_frame_class` then the class-specific
/// border layout, then applies the shared post-processing every class goes
/// through alike: `bs_pointer <= num_env+1` (else `Err`), strict
/// monotonicity of every `t_env` border (else `Err`), `L_Q` derivation, the
/// middle noise border, and `l_a`. Never reads past `end_bit`; every
/// malformed input (bad pointer, non-monotone borders, an over-long
/// VARVAR envelope count, a read past `end_bit`) returns
/// `Err(DecodeError::Malformed)` rather than panicking.
pub(super) fn sbr_grid(bits: &mut BitReader, end_bit: usize) -> Result<SbrGrid, DecodeError> {
    let frame_class = read_bits(bits, end_bit, 2)? as u8;
    let raw = match frame_class {
        FIXFIX => parse_fixfix(bits, end_bit)?,
        FIXVAR => parse_fixvar(bits, end_bit)?,
        VARFIX => parse_varfix(bits, end_bit)?,
        VARVAR => parse_varvar(bits, end_bit)?,
        _ => return Err(DecodeError::Malformed), // read(2) is always 0..=3; kept for defense-in-depth
    };
    let RawGrid {
        num_env,
        t_env,
        freq_res,
        pointer,
    } = raw;

    if num_env == 0 || num_env as usize >= t_env.len() {
        return Err(DecodeError::Malformed);
    }
    if pointer as u16 > num_env as u16 + 1 {
        return Err(DecodeError::Malformed);
    }
    for i in 1..=num_env as usize {
        if t_env[i - 1] >= t_env[i] {
            return Err(DecodeError::Malformed); // not strictly monotone
        }
    }

    let num_noise: u8 = if num_env > 1 { 2 } else { 1 };

    let mut t_noise = [0u8; 3];
    t_noise[0] = t_env[0];
    *t_noise
        .get_mut(num_noise as usize)
        .ok_or(DecodeError::Malformed)? = t_env[num_env as usize];
    if num_noise > 1 {
        let idx = middle_noise_border_idx(frame_class, num_env, pointer);
        t_noise[1] = *t_env.get(idx).ok_or(DecodeError::Malformed)?;
    }

    let l_a = compute_l_a(frame_class, num_env, pointer);

    Ok(SbrGrid {
        frame_class,
        num_env,
        num_noise,
        t_env,
        t_noise,
        freq_res,
        pointer,
        l_a,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;

    #[test]
    fn fixfix_single_env() {
        // frame_class=00, bs_num_env pow=00 -> L_E=1, bs_freq_res=1.
        // bits MSB-first: 00 00 1 -> 0b00001000 = 0x08.
        let buf = [0x08];
        let g = sbr_grid(&mut BitReader::new(&buf), 8).unwrap();
        assert_eq!(g.frame_class, FIXFIX);
        assert_eq!(g.num_env, 1);
        assert_eq!(g.freq_res[0], 1);
        assert_eq!(g.num_noise, 1); // L_E==1 -> L_Q=1
        assert_eq!(&g.t_env[..2], &[0, 16]); // raw slots 0..numTimeSlots
        assert_eq!(&g.t_noise[..2], &[0, 16]);
        assert_eq!(g.l_a, -1); // FIXFIX
    }

    #[test]
    fn fixfix_four_env_equal_spacing() {
        // frame_class=00, pow=10 -> L_E=4; freq_res=0. bits: 00 10 0 = 0b00100000 = 0x20.
        let buf = [0x20];
        let g = sbr_grid(&mut BitReader::new(&buf), 8).unwrap();
        assert_eq!(g.num_env, 4);
        assert_eq!(&g.t_env[..5], &[0, 4, 8, 12, 16]); // equally spaced
        assert_eq!(g.num_noise, 2); // L_E>1 -> L_Q=2
    }

    #[test]
    fn varfix_two_env_hand_derived() {
        // VARFIX frame, hand-derived bit-for-bit against FFmpeg's
        // `read_sbr_grid` VARFIX case:
        //   frame_class = VARFIX = 10
        //   bs_abs_bord (t_env[0])        = 00      -> 0
        //   bs_num_rel_lead               = 01      -> num_rel_lead=1, L_E=2
        //   bs_rel_bord[0]                = 00      -> 2*0+2=2 -> t_env[1]=0+2=2
        //   bs_pointer (ceil_log2[2]=2b)  = 00       -> pointer=0
        //     (CEIL_LOG2[2] = ceil(log2(2+1)) = 2 bits, not 1 -- covers the
        //     full valid pointer range 0..=num_env+1 = 0..=3)
        //   bs_freq_res[0] (forward)      = 1
        //   bs_freq_res[1] (forward)      = 0
        // Concatenated: 10 00 01 00 00 1 0 = 100001000010, padded to 2 bytes:
        // 1000010000100000 = 0x84, 0x20.
        let buf = [0x84, 0x20];
        let g = sbr_grid(&mut BitReader::new(&buf), 16).unwrap();
        assert_eq!(g.frame_class, VARFIX);
        assert_eq!(g.num_env, 2);
        assert_eq!(g.num_noise, 2); // L_E>1 -> L_Q=2
        assert_eq!(&g.t_env[..3], &[0, 2, 16]);
        assert_eq!(g.pointer, 0);
        // VARFIX middle border: pointer==0 -> idx=1 -> t_env[1]=2.
        assert_eq!(&g.t_noise[..3], &[0, 2, 16]);
        // VARFIX l_a: only set when pointer>1; pointer==0 here -> -1.
        assert_eq!(g.l_a, -1);
        assert_eq!(g.freq_res[0], 1);
        assert_eq!(g.freq_res[1], 0);

        // Invariants a variable-class bug would break (per task brief):
        assert!(g.t_env[0] == 0 && g.t_env[g.num_env as usize] == 16);
        for w in g.t_env[..=g.num_env as usize].windows(2) {
            assert!(w[1] > w[0], "t_env not strictly increasing: {:?}", g.t_env);
        }
        assert!((1..=5).contains(&g.num_env));
        assert!((1..=2).contains(&g.num_noise));
        assert!((-1..=g.num_env as i8).contains(&g.l_a));
    }

    #[test]
    fn varfix_overlong_rel_border_chain_is_malformed() {
        // VARFIX frame engineered to push t_env past the fixed trailing
        // border (16) via a maxed-out relative-border chain, so the
        // strict-monotonicity check must reject it:
        //   frame_class = VARFIX = 10
        //   bs_abs_bord (t_env[0])   = 11 -> 3
        //   bs_num_rel_lead          = 11 -> 3, L_E=4
        //   bs_rel_bord[0..3]        = 11,11,11 -> 8,8,8
        //     t_env[1]=3+8=11, t_env[2]=11+8=19, t_env[3]=19+8=27
        //   t_env[4] fixed = 16 -> 27 >= 16, not strictly monotone -> Err
        // (whatever bs_pointer (ceil_log2[4]=3b) / bs_freq_res bits follow
        // don't matter -- the trailing zero padding is enough headroom for
        // those reads to complete before sbr_grid's monotonicity check
        // rejects the already-malformed t_env chain.)
        // Concatenated prefix: 10 11 11 11 11 11 (12 bits), zero-padded to 3
        // bytes: 101111111111000000000000 = 0xBF, 0xF0, 0x00.
        let buf = [0xBF, 0xF0, 0x00];
        let r = sbr_grid(&mut BitReader::new(&buf), 24);
        assert!(r.is_err(), "non-monotone t_env chain must be rejected");
    }

    #[test]
    fn read_past_end_bit_is_malformed_not_panic() {
        // frame_class=VARVAR needs many reads; declaring a tiny end_bit
        // must fail cleanly rather than panic/read out of bounds.
        let buf = [0xFFu8];
        let r = sbr_grid(&mut BitReader::new(&buf), 3);
        assert!(r.is_err());
    }

    #[test]
    fn fixvar_frame_borders() {
        // FIXVAR frame, hand-derived bit-for-bit against `parse_fixvar`
        // (mirrors FFmpeg's `read_sbr_grid` FIXVAR case):
        //   frame_class = FIXVAR                    = 01
        //   bs_abs_bord (trailing, var_bord)         = 01      -> 16+1=17
        //   bs_num_rel_trail                         = 10      -> num_rel=2, L_E=3
        //   bs_rel_bord[0] (built downward from t_env[3]=17)  = 00 -> 2*0+2=2
        //     -> t_env[2] = 17 - 2 = 15
        //   bs_rel_bord[1] (built downward from t_env[2]=15)  = 01 -> 2*1+2=4
        //     -> t_env[1] = 15 - 4 = 11
        //   bs_pointer (ceil_log2[3]=2b)              = 00      -> pointer=0
        //   bs_freq_res, read bitstream-order (last envelope first, per
        //   `parse_fixvar`'s un-reversal): bit0->freq_res[2]=1,
        //   bit1->freq_res[1]=0, bit2->freq_res[0]=1
        // Concatenated: 01 01 10 00 01 00 1 0 1 (15 bits), zero-padded to 2
        // bytes: 0101100001001010 = 0x58, 0x4A.
        let buf = [0x58, 0x4A];
        let g = sbr_grid(&mut BitReader::new(&buf), 16).unwrap();
        assert_eq!(g.frame_class, FIXVAR);
        assert_eq!(g.num_env, 3);
        assert_eq!(g.pointer, 0);
        assert_eq!(&g.t_env[..4], &[0, 11, 15, 17]);
        assert_eq!(g.num_noise, 2); // L_E>1 -> L_Q=2
        // FIXVAR/VARVAR middle border: idx = L_E - max(pointer-1,1) = 3-1=2
        // -> t_env[2] = 15.
        assert_eq!(&g.t_noise[..3], &[0, 15, 17]);
        assert_eq!(&g.freq_res[..3], &[1, 0, 1]);
        // FIXVAR l_a: only set when bs_pointer != 0; pointer==0 here -> -1.
        assert_eq!(g.l_a, -1);

        // Invariants a variable-class bug would break (per task brief):
        assert!(g.t_env[0] == 0 && g.t_env[g.num_env as usize] == 17);
        for w in g.t_env[..=g.num_env as usize].windows(2) {
            assert!(w[1] > w[0], "t_env not strictly increasing: {:?}", g.t_env);
        }
        assert!((1..=5).contains(&g.num_env));
        assert!((1..=2).contains(&g.num_noise));
        assert!((-1..=g.num_env as i8).contains(&g.l_a));
    }

    #[test]
    fn varvar_frame_borders() {
        // VARVAR frame, hand-derived bit-for-bit against `parse_varvar`
        // (mirrors FFmpeg's `read_sbr_grid` VARVAR case):
        //   frame_class = VARVAR                     = 11
        //   bs_abs_bord_0 (leading, var_bord_0)       = 01      -> 1
        //   bs_abs_bord_1 (trailing, var_bord_1)      = 10      -> 16+2=18
        //   bs_num_rel_0 (leading)                    = 01      -> 1
        //   bs_num_rel_1 (trailing)                   = 01      -> 1, L_E=1+1+1=3
        //   bs_rel_bord (leading, built upward from t_env[0]=1) = 00 -> 2*0+2=2
        //     -> t_env[1] = 1 + 2 = 3
        //   bs_rel_bord (trailing, built downward from t_env[3]=18) = 01 -> 2*1+2=4
        //     -> t_env[2] = 18 - 4 = 14
        //   bs_pointer (ceil_log2[3]=2b)               = 10      -> pointer=2
        //   bs_freq_res, read forward: 1, 1, 0
        // Concatenated: 11 01 10 01 01 00 01 10 1 1 0 (19 bits), zero-padded
        // to 3 bytes: 110110010100011011000000 = 0xD9, 0x46, 0xC0.
        let buf = [0xD9, 0x46, 0xC0];
        let g = sbr_grid(&mut BitReader::new(&buf), 24).unwrap();
        assert_eq!(g.frame_class, VARVAR);
        assert_eq!(g.num_env, 3);
        assert_eq!(g.pointer, 2);
        assert_eq!(&g.t_env[..4], &[1, 3, 14, 18]);
        assert_eq!(g.num_noise, 2); // L_E>1 -> L_Q=2
        // FIXVAR/VARVAR middle border: idx = L_E - max(pointer-1,1) = 3-1=2
        // -> t_env[2] = 14.
        assert_eq!(&g.t_noise[..3], &[1, 14, 18]);
        assert_eq!(&g.freq_res[..3], &[1, 1, 0]);
        // VARVAR l_a: set when bs_pointer != 0 -> L_E+1-pointer = 3+1-2=2.
        assert_eq!(g.l_a, 2);

        // Invariants a variable-class bug would break (per task brief):
        assert!(g.t_env[0] == 1 && g.t_env[g.num_env as usize] == 18);
        for w in g.t_env[..=g.num_env as usize].windows(2) {
            assert!(w[1] > w[0], "t_env not strictly increasing: {:?}", g.t_env);
        }
        assert!((1..=5).contains(&g.num_env));
        assert!((1..=2).contains(&g.num_noise));
        assert!((-1..=g.num_env as i8).contains(&g.l_a));
    }
}
