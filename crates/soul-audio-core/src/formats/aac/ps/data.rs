//! `ps_data()` bitstream parse (`ps-reference.md` §1): the PS header gate,
//! the `ps_data_available` availability state, and the frame-class/
//! `num_env_idx` framing, returning the exact bit count consumed — the one
//! number the enclosing SBR extension loop needs back to keep its own
//! bookkeeping correct (Task 9 wires this behind `EXTENSION_ID_PS` in
//! `sbr::bitstream`'s `bs_extension` loop; this task drives it directly
//! from tests, mirroring the SBR-Task-2 precedent).
//!
//! The parameter body (IID/ICC Huffman-delta decode, §3, via
//! [`super::params::read_iid`]/[`super::params::read_icc`]) and the
//! `enable_ext` IPD/OPD extension (`ps_extension_size` + `ps_ext_id` +
//! `ps_extension()` + fill bits, §1, via [`super::params::read_ipd`]/
//! [`super::params::read_opd`]) are parsed for real here (Task 4); a
//! disabled parameter type resets to its neutral dequantized value instead
//! of carrying over a stale prior frame's data (see `params::reset_*`). The
//! frame body's full layout (`num_env`,
//! `border_position` — both the fixed-class arithmetic and the
//! variable-class bitstream read — and the mode-derived band-count/`is34`/
//! `iid_quant` context) is delegated to [`super::frame::read_frame`] (Task
//! 3); `ps_data` itself only reads the unconditional `frame_class`/
//! `num_env_idx` pair ahead of it (§1's `ps_data()` pseudocode reads them
//! unconditionally itself — that much is this task's own scope, not a
//! delegate's).
//!
//! # The no-panic clamp
//!
//! Every read goes through [`clamped_read`] (mirroring
//! `sbr::bitstream::clamped_read`), which returns `None` — turned into
//! `Err(DecodeError::Malformed)` here — rather than reading past `end_bit`.
//! `iid_mode`/`icc_mode` are range-checked (`0..=5`, §2) before being
//! stored or used to index [`super::tables`]'s mode tables, so a malformed
//! mode value errors out instead of indexing out of bounds. This is the
//! firmware no-panic invariant: malformed PS input disables PS for the
//! frame (the caller's job, once wired in Task 9) rather than panicking.

use super::frame::read_frame;
use super::tables::{COS_ALPHAS, IPDOPD_COS, IPDOPD_SIN, SF_IID_NORMAL, SIN_ALPHAS};
use super::{MAX_PS_ENVELOPES, N_SLOTS, PsDecoder, params};
use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;

/// Bounded MSB-first read: `Some(v)` if `[.., end_bit)` allows `n` more
/// bits, else `None` without advancing `bits`. The PS-path analogue of
/// `sbr::bitstream::clamped_read` — every `ps_data` read goes through this.
pub(crate) fn clamped_read(bits: &mut BitReader, end_bit: usize, n: u32) -> Option<u32> {
    if bits.bit_pos() + n as usize > end_bit {
        return None;
    }
    Some(bits.read(n))
}

/// Skip `n` bits if doing so stays within `[.., end_bit)`, else `None`
/// without advancing. Mirrors `sbr::bitstream::clamped_skip`; reads in
/// `<=32`-bit chunks so it always terminates regardless of `n`. Used here
/// for the `enable_ext` block's trailing fill bits.
fn clamped_skip(bits: &mut BitReader, end_bit: usize, n: usize) -> Option<()> {
    if bits.bit_pos() + n > end_bit {
        return None;
    }
    let mut remaining = n;
    while remaining > 0 {
        let take = remaining.min(32);
        bits.read(take as u32);
        remaining -= take;
    }
    Some(())
}

/// Parse `ps_data()` (§1): header gate, frame body (`frame_class` +
/// `num_env_idx` here, the rest of the layout via `frame::read_frame`),
/// parameter body (IID/ICC, §3) and `enable_ext`'s IPD/OPD extension (§1/§3).
/// Updates `ps`'s header/frame/params state and returns the exact bit count
/// consumed. `end_bit` is the sub-extension budget end — never read past it.
///
/// `[NOPANIC]`: any read past `end_bit`, or an out-of-range `iid_mode`/
/// `icc_mode` (must be `0..=5`, §2), returns `Err(DecodeError::Malformed)`;
/// the caller disables PS for the frame rather than panicking or indexing
/// out of bounds.
pub(crate) fn ps_data(
    ps: &mut PsDecoder,
    bits: &mut BitReader,
    end_bit: usize,
) -> Result<u32, DecodeError> {
    let entry_pos = bits.bit_pos();

    let enable_ps_header = clamped_read(bits, end_bit, 1).ok_or(DecodeError::Malformed)?;

    if enable_ps_header == 1 {
        let enable_iid = clamped_read(bits, end_bit, 1).ok_or(DecodeError::Malformed)? == 1;
        ps.enable_iid = enable_iid;
        if enable_iid {
            let iid_mode = clamped_read(bits, end_bit, 3).ok_or(DecodeError::Malformed)? as u8;
            if iid_mode > 5 {
                return Err(DecodeError::Malformed);
            }
            ps.iid_mode = iid_mode;
        }

        let enable_icc = clamped_read(bits, end_bit, 1).ok_or(DecodeError::Malformed)? == 1;
        ps.enable_icc = enable_icc;
        if enable_icc {
            let icc_mode = clamped_read(bits, end_bit, 3).ok_or(DecodeError::Malformed)? as u8;
            if icc_mode > 5 {
                return Err(DecodeError::Malformed);
            }
            ps.icc_mode = icc_mode;
        }

        ps.enable_ext = clamped_read(bits, end_bit, 1).ok_or(DecodeError::Malformed)? == 1;
        ps.ps_data_available = true;
    }
    // enable_ps_header==0: reuse the last accepted header verbatim (leave
    // ps.enable_iid/enable_icc/enable_ext/iid_mode/icc_mode untouched).

    if !ps.ps_data_available {
        // No header ever seen (this frame's absent header didn't change
        // that): a no-op frame, mono passes through unchanged (§1). Nothing
        // past the header gate is read.
        return Ok((bits.bit_pos() - entry_pos) as u32);
    }

    // Frame body (§1/§2): frame_class + num_env_idx are read every call
    // once a header has been seen, unconditional on this frame's
    // enable_ps_header. `frame::read_frame` (Task 3) turns them, plus the
    // header's iid_mode/icc_mode, into the full layout: num_env,
    // border_position (fixed-class arithmetic or variable-class bitstream
    // read), and the mode-derived band counts/is34/iid_quant.
    //
    // `is34` (§2/§4) is itself persisted (FFmpeg `aacps_common.c`'s
    // `ps->is34bands`): `read_frame` only recomputes it from THIS frame's
    // `enable_iid`/`enable_icc` when at least one of them is set, else it
    // holds the previous frame's value (`is34_old` below, captured from
    // `ps.frame` — itself persisted on `PsDecoder` — before this call
    // overwrites it) rather than being derived unconditionally from
    // possibly-stale `iid_mode`/`icc_mode` table lookups.
    let frame_class = clamped_read(bits, end_bit, 1).ok_or(DecodeError::Malformed)? as u8;
    let num_env_idx = clamped_read(bits, end_bit, 2).ok_or(DecodeError::Malformed)? as u8;
    let is34_old = ps.frame.is34;
    ps.frame = read_frame(
        bits,
        end_bit,
        frame_class,
        num_env_idx,
        ps.iid_mode,
        ps.icc_mode,
        ps.enable_iid,
        ps.enable_icc,
        is34_old,
    )?;

    // Parameter body (§3): IID then ICC, each either the real Huffman-delta
    // decode (`params::read_iid`/`read_icc`) when its header bit enabled
    // it, or reset to the neutral dequantized value (index 0 -> unity gain
    // / full coherence) when it didn't — mirroring FFmpeg's
    // `memset(iid_par, 0, ...)`: a frame that turns IID/ICC off must not
    // keep showing the previous frame's panning/coherence.
    if ps.enable_iid {
        params::read_iid(
            bits,
            end_bit,
            &ps.frame,
            &mut ps.iid_index_prev,
            &mut ps.params,
        )?;
    } else {
        params::reset_iid(&mut ps.iid_index_prev, &ps.frame, &mut ps.params);
    }
    if ps.enable_icc {
        params::read_icc(
            bits,
            end_bit,
            &ps.frame,
            &mut ps.icc_index_prev,
            &mut ps.params,
        )?;
    } else {
        params::reset_icc(&mut ps.icc_index_prev, &ps.frame, &mut ps.params);
    }

    // Extension (§1): `enable_ext`'s ps_extension_size-bounded loop of
    // (ps_ext_id, ps_extension()) pairs; id==0 carries IPD/OPD (§3).
    // `ps.enable_ipdopd` (persisted on `PsDecoder`, not `PsParams`) is
    // assigned ONLY inside the `ps_ext_id == 0` branch below, exactly like
    // faad2 `ps_syntax.c`'s `ps_extension()` (`ps->enable_ipdopd =
    // faad_get1bit(...)`, only reached when that record is present) and
    // FFmpeg `aacps_common.c`'s `ps_read_extension_data` (`ps->enable_ipdopd
    // = get_bits1(gb)`, same gate). A frame with `enable_ext == 0`, or with
    // `enable_ext == 1` but no id==0 record in its sub-extension loop, never
    // touches `ps.enable_ipdopd` at all -- it carries over from whenever it
    // was last assigned, same as `enable_iid`/`enable_icc`/`enable_ext`.
    // `[NOPANIC]`: every read stays `clamped_read`/`clamped_skip`-bounded by
    // `end_bit`; the `bits_left` bookkeeping clamps to 0 rather than going
    // negative if a sub-extension's variable-length Huffman data reads past
    // its own advertised size (a known non-fatal quirk, not malformed
    // framing — mirrors faad2's `ps_data()` comment on this exact clamp).
    if ps.enable_ext {
        let mut cnt = clamped_read(bits, end_bit, 4).ok_or(DecodeError::Malformed)?;
        if cnt == 15 {
            cnt += clamped_read(bits, end_bit, 8).ok_or(DecodeError::Malformed)?;
        }
        let mut bits_left: i64 = 8 * cnt as i64;
        while bits_left > 7 {
            let ps_ext_id = clamped_read(bits, end_bit, 2).ok_or(DecodeError::Malformed)?;
            bits_left -= 2;
            let body_start = bits.bit_pos();
            if ps_ext_id == 0 {
                ps.enable_ipdopd =
                    clamped_read(bits, end_bit, 1).ok_or(DecodeError::Malformed)? == 1;
                if ps.enable_ipdopd {
                    params::read_ipd(
                        bits,
                        end_bit,
                        &ps.frame,
                        &mut ps.ipd_index_prev,
                        &mut ps.params,
                    )?;
                    params::read_opd(
                        bits,
                        end_bit,
                        &ps.frame,
                        &mut ps.opd_index_prev,
                        &mut ps.params,
                    )?;
                }
                // reserved_ps
                clamped_read(bits, end_bit, 1).ok_or(DecodeError::Malformed)?;
            }
            // ps_ext_id != 0: unrecognized extension, nothing else defined
            // at this level (matches faad2/FFmpeg: only id==0 is decoded).
            let body_bits = (bits.bit_pos() - body_start) as i64;
            bits_left = if body_bits > bits_left {
                0
            } else {
                bits_left - body_bits
            };
        }
        // `bits_left` is now 0..=7: consume the trailing fill bits.
        clamped_skip(bits, end_bit, bits_left as usize).ok_or(DecodeError::Malformed)?;
    }
    // `enable_ext == 0`: `ps.enable_ipdopd` is left untouched (persisted).

    // The neutral reset fires ONLY when the (persisted) flag is currently
    // false — mirroring FFmpeg's `if (!ps->enable_ipdopd) { memset(ipd_par,
    // ...); memset(opd_par, ...); }`, run once after the whole `enable_ext`
    // block rather than per-branch. When `ps.enable_ipdopd` is true but this
    // frame had no fresh id==0 record, both the decode AND the reset are
    // skipped: `params.ipd_cos`/`ipd_sin`/`opd_cos`/`opd_sin` and
    // `ps.ipd_index_prev`/`opd_index_prev` hold their last decoded values
    // (FFmpeg's "don't touch the array").
    if !ps.enable_ipdopd {
        params::reset_ipd(&mut ps.ipd_index_prev, &ps.frame, &mut ps.params);
        params::reset_opd(&mut ps.opd_index_prev, &ps.frame, &mut ps.params);
    }
    // Mirror the persisted flag into `PsParams` so Task 8's mixing stage
    // still reads it off `params` per the existing interface.
    ps.params.enable_ipdopd = ps.enable_ipdopd;

    // Coverage fixup (FFmpeg "Fix up envelopes", `aacps_common.c` ~L222-257):
    // guarantee this frame fully covers all 32 QMF slots by appending a single
    // synthetic held-param envelope when the parsed envelopes fall short (a
    // `num_env == 0` reuse-previous frame, or a variable-class frame whose last
    // border stops short of slot 32). Runs AFTER the parameter body + the
    // `!enable_ipdopd` neutral reset (FFmpeg's memset order), so the synthetic
    // row sees fully-resolved params.
    coverage_fixup(ps);

    // `num_env_old` for the NEXT frame's reuse source = THIS frame's FINAL
    // (post-fixup) `num_env`. FFmpeg captures it at the top of `ff_ps_read_data`
    // (L170) from the persisted value that its previous call left; setting it
    // here at frame end leaves the identical value in place for the next call.
    ps.num_env_old = ps.frame.num_env as usize;

    Ok((bits.bit_pos() - entry_pos) as u32)
}

/// FFmpeg's "Fix up envelopes" coverage guarantee (`libavcodec/aacps_common.c`
/// `ff_ps_read_data` ~L222-257), mirrored exactly:
///
/// ```c
/// if (!ps->num_env || ps->border_position[ps->num_env] < numQMFSlots - 1) {
///     int source = ps->num_env ? ps->num_env - 1 : ps->num_env_old - 1;
///     if (source >= 0 && source != ps->num_env) {
///         if (ps->enable_iid)   memcpy(iid_par+num_env, iid_par+source, ...);
///         if (ps->enable_icc)   memcpy(icc_par+num_env, icc_par+source, ...);
///         if (ps->enable_ipdopd){memcpy(ipd_par+num_env, ipd_par+source, ...);
///                                memcpy(opd_par+num_env, opd_par+source, ...);}
///     }
///     ps->num_env++;
///     ps->border_position[ps->num_env] = numQMFSlots - 1;
/// }
/// ```
///
/// # Border convention (the trap)
/// FFmpeg borders are `-1`-based (`border[0] = -1`; full coverage
/// `= numQMFSlots - 1 = 31`). OUR `frame.rs` borders are `0`-based
/// (`border[0] = 0`; full coverage `= 32 = N_SLOTS`, and `mix.rs` reads slots
/// `[border[e], border[e+1])`). So FFmpeg's `border[num_env] < numQMFSlots - 1`
/// becomes OUR `border[num_env] < N_SLOTS`, and the extended border
/// `numQMFSlots - 1` becomes OUR `N_SLOTS`.
///
/// # Source param row (cross-frame)
/// `source = num_env ? num_env-1 : num_env_old-1`: for a variable-short frame
/// (`num_env > 0`) the current frame's last envelope, already dequantized in
/// `ps.params`; for a `num_env == 0` reuse frame the PREVIOUS frame's last
/// envelope (`num_env_old - 1`), which OUR `ps.params` STILL holds because a
/// `num_env == 0` frame overwrites no param rows (its decode loops zero times)
/// — the exact analogue of FFmpeg's persistent `iid_par`/`icc_par`/... arrays.
/// FFmpeg's `source != num_env` self-copy skip: when `num_env == 0 &&
/// num_env_old == 1` the retained `params[0]` already equals the wanted row,
/// so we leave it untouched (a self-memcpy FFmpeg also elides).
///
/// # Neutral / cold-start
/// FFmpeg's persisted arrays are RAW indices where `0` dequantizes to the
/// centre (unity IID gain, `alpha = 0`, zero phase); OUR persisted arrays are
/// the dequantized floats, so a raw-zero row is NOT already neutral in our
/// domain. Two cases need explicit neutralization to match FFmpeg's observable
/// result: (1) a DISABLED parameter type — FFmpeg whole-array `memset` leaves
/// the synthetic row at raw 0 → neutral; (2) the cold-start-reuse edge
/// (`num_env == 0 && num_env_old == 0`, so `source == -1`) — FFmpeg's
/// zero-initialized arrays → neutral, and its `source >= 0` guard skips the
/// copy. Both fill the synthetic row with the dequantized centre value.
///
/// `[NOPANIC]`: guarded by `num_env + 1 <= MAX_PS_ENVELOPES` so the synthetic
/// envelope never writes past the fixed `border_position`/`PsParams` arrays
/// (NUM_ENV_TAB caps `num_env` at 4, but `read_frame` clamps a malformed value
/// to `MAX_PS_ENVELOPES`; if there is genuinely no room the frame is left as
/// parsed and `apply`'s centre-mono defense covers the tail). Never panics on
/// the cold-start edge.
fn coverage_fixup(ps: &mut PsDecoder) {
    let num_env = ps.frame.num_env as usize;

    // Already fully covered? Fixed-class frames always are (last border == 32),
    // so only `num_env == 0` or a variable-short last border triggers the fixup.
    if num_env != 0 && ps.frame.border_position[num_env] as usize >= N_SLOTS {
        return;
    }
    // No room to append the synthetic envelope — leave the frame as parsed
    // (the `apply` centre-mono defense covers the uncovered tail).
    if num_env + 1 > MAX_PS_ENVELOPES {
        return;
    }

    let ne = num_env; // synthetic-envelope row index (pre-increment)
    // FFmpeg's signed (`-1`-based) source index into the persisted param rows.
    let source: isize = if num_env > 0 {
        num_env as isize - 1
    } else {
        ps.num_env_old as isize - 1
    };
    // A valid *other* row to copy from (FFmpeg `source >= 0 && source != num_env`).
    let copy_from: Option<usize> = if source >= 0 && source as usize != ne {
        Some(source as usize)
    } else {
        None
    };
    // Neutralize when the type carries no held row: cold start (`source < 0`)
    // — the other non-copy case — matches FFmpeg's zero-initialized arrays.
    // (`source == ne` is the self-copy skip: leave the retained row as-is.)
    let cold = source < 0;

    let is34 = ps.frame.is34;
    let iidicc_w = if is34 { 34 } else { 20 };
    let ipdopd_w = if is34 { 17 } else { 11 };

    // IID: copy the held row (enabled + valid source), else neutralize
    // (disabled -> FFmpeg memset; or cold-start -> FFmpeg zero-init). When
    // enabled with `source == ne`, leave the retained row untouched.
    if ps.enable_iid {
        if let Some(src) = copy_from {
            ps.params.iid_c[ne] = ps.params.iid_c[src];
        } else if cold {
            ps.params.iid_c[ne][..iidicc_w].fill(SF_IID_NORMAL[7]);
        }
    } else {
        ps.params.iid_c[ne][..iidicc_w].fill(SF_IID_NORMAL[7]);
    }

    // ICC.
    if ps.enable_icc {
        if let Some(src) = copy_from {
            ps.params.icc_cos[ne] = ps.params.icc_cos[src];
            ps.params.icc_sin[ne] = ps.params.icc_sin[src];
        } else if cold {
            ps.params.icc_cos[ne][..iidicc_w].fill(COS_ALPHAS[0]);
            ps.params.icc_sin[ne][..iidicc_w].fill(SIN_ALPHAS[0]);
        }
    } else {
        ps.params.icc_cos[ne][..iidicc_w].fill(COS_ALPHAS[0]);
        ps.params.icc_sin[ne][..iidicc_w].fill(SIN_ALPHAS[0]);
    }

    // IPD/OPD (gated together on `enable_ipdopd`, exactly like FFmpeg).
    if ps.enable_ipdopd {
        if let Some(src) = copy_from {
            ps.params.ipd_cos[ne] = ps.params.ipd_cos[src];
            ps.params.ipd_sin[ne] = ps.params.ipd_sin[src];
            ps.params.opd_cos[ne] = ps.params.opd_cos[src];
            ps.params.opd_sin[ne] = ps.params.opd_sin[src];
        } else if cold {
            ps.params.ipd_cos[ne][..ipdopd_w].fill(IPDOPD_COS[0]);
            ps.params.ipd_sin[ne][..ipdopd_w].fill(IPDOPD_SIN[0]);
            ps.params.opd_cos[ne][..ipdopd_w].fill(IPDOPD_COS[0]);
            ps.params.opd_sin[ne][..ipdopd_w].fill(IPDOPD_SIN[0]);
        }
    } else {
        ps.params.ipd_cos[ne][..ipdopd_w].fill(IPDOPD_COS[0]);
        ps.params.ipd_sin[ne][..ipdopd_w].fill(IPDOPD_SIN[0]);
        ps.params.opd_cos[ne][..ipdopd_w].fill(IPDOPD_COS[0]);
        ps.params.opd_sin[ne][..ipdopd_w].fill(IPDOPD_SIN[0]);
    }

    // Extend coverage: append the synthetic envelope (FFmpeg `num_env++` then
    // `border_position[num_env] = numQMFSlots - 1`, i.e. OUR `N_SLOTS`).
    ps.frame.num_env = (num_env + 1) as u8;
    ps.frame.border_position[num_env + 1] = N_SLOTS as u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;

    #[test]
    fn header_absent_before_any_header_is_noop() {
        // enable_ps_header=0 -> no header yet -> ps_data_available stays
        // false, consumes exactly 1 bit, returns Ok(1).
        let buf = [0u8; 2]; // first bit 0
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        let n = ps_data(&mut ps, &mut b, 16).unwrap();
        assert_eq!(n, 1);
        assert!(!ps.ps_data_available);
    }

    #[test]
    fn header_sets_modes_and_availability() {
        // enable_ps_header=1, enable_iid=1, iid_mode=010(2), enable_icc=1,
        // icc_mode=001(1), enable_ext=0, then frame_class=0, num_env_idx=00.
        // MSB-first: 1 1 010 1 001 0 | 0 00  = 1101 0100 1000 0(pad)
        let buf = [0b1101_0100, 0b1000_0000, 0, 0];
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        let _ = ps_data(&mut ps, &mut b, 32).unwrap();
        assert!(ps.ps_data_available);
        assert!(ps.enable_iid && ps.enable_icc && !ps.enable_ext);
        assert_eq!(ps.iid_mode, 2);
        assert_eq!(ps.icc_mode, 1);
    }

    #[test]
    fn rejects_out_of_range_mode() {
        // enable_ps_header=1, enable_iid=1, iid_mode=111(7) -> invalid (>5)
        // -> Err.
        let buf = [0b1111_1000, 0, 0, 0];
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        assert!(ps_data(&mut ps, &mut b, 32).is_err());
    }

    #[test]
    fn overrun_past_end_bit_is_malformed() {
        let buf = [0b1000_0000u8]; // header flag=1 but no room for the rest
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        assert!(ps_data(&mut ps, &mut b, 1).is_err());
    }

    #[test]
    fn truncated_between_header_and_frame_body_is_malformed() {
        // A fully-present, in-budget header (enable_ps_header=1,
        // enable_iid=0, enable_icc=0, enable_ext=0 -> 4 bits) but end_bit
        // stops right there, leaving no room for the unconditional
        // frame_class/num_env_idx read. Must error, not panic/OOB.
        let buf = [0b1000_0000u8];
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        assert!(ps_data(&mut ps, &mut b, 4).is_err());
        assert!(b.bit_pos() <= 4);
    }

    #[test]
    fn header_absent_reuses_previous_header() {
        // First call: a full header (enable_iid=1, iid_mode=011(3);
        // enable_icc=0; enable_ext=0), then frame_class=0, num_env_idx=00.
        // MSB-first: 1 1 011 0 0 | 0 00 = 1101 1000 0(pad)
        let buf = [0b1101_1000u8, 0, 0, 0];
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        let _ = ps_data(&mut ps, &mut b, 32).unwrap();
        assert!(ps.enable_iid && ps.iid_mode == 3 && !ps.enable_icc);

        // Second call, fresh reader: enable_ps_header=0 -> reuse the stored
        // header. frame_class=0, num_env_idx=00 -> num_env=0 (fixed class,
        // NUM_ENV_TAB[0][0]), so the parameter/ext bodies read 0 further
        // bits (enable_ext is also false from the reused header).
        let buf2 = [0b0000_0000u8, 0]; // header_absent(0), frame_class(0), num_env_idx(00)
        let mut b2 = BitReader::new(&buf2);
        let n = ps_data(&mut ps, &mut b2, 16).unwrap();
        assert_eq!(n, 4); // header gate(1) + frame body(3); num_env=0 -> no more bits
        assert!(ps.ps_data_available);
        assert!(ps.enable_iid && ps.iid_mode == 3 && !ps.enable_icc);
    }

    #[test]
    fn wires_real_iid_and_disabled_icc_ipdopd_to_neutral() {
        // header: enable_ps_header=1, enable_iid=1, iid_mode=000(0, 10-band
        // coarse), enable_icc=0, enable_ext=0. frame: frame_class=0,
        // num_env_idx=01 -> num_env=NUM_ENV_TAB[0][1]=1. IID: dt=0
        // (delta-freq), 10 zero-delta codewords ("0" each, F_HUFF_IID_DEF
        // root leaf). All bits after that are don't-care padding.
        // bits: 1 1 000 0 0 | 0 01 | 0 0000000000  (21 bits, padded)
        let buf = [0xC0u8, 0x40, 0x00, 0x00];
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        let n = ps_data(&mut ps, &mut b, 32).unwrap();
        assert_eq!(n, 21);
        assert_eq!(ps.frame.num_env, 1);
        assert_eq!(ps.frame.nr_iid_par, 10);
        assert!(!ps.frame.is34);

        // All-zero deltas -> IID stays at the unity-gain center (q=0) after
        // the 10->20 resolution-expand (is34=false).
        for band in 0..20 {
            assert!((ps.params.iid_c[0][band] - 1.0).abs() < 1e-6);
        }
        // ICC disabled this frame -> reset to neutral (fully coherent).
        for band in 0..20 {
            assert!((ps.params.icc_cos[0][band] - 1.0).abs() < 1e-6);
            assert!(ps.params.icc_sin[0][band].abs() < 1e-6);
        }
        // Extension disabled this frame -> IPD/OPD neutral, flag stays false.
        assert!(!ps.params.enable_ipdopd);
        for band in 0..11 {
            assert!((ps.params.ipd_cos[0][band] - 1.0).abs() < 1e-6);
            assert!((ps.params.opd_cos[0][band] - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn wires_real_ipdopd_extension_and_sets_enable_flag() {
        // Same header/frame/IID as above except enable_ext=1, followed by a
        // minimal ps_extension(): ps_extension_size=2 (4 bits), ps_ext_id=0
        // (2 bits), enable_ipdopd=1, IPD dt=0 + 5 zero-delta codewords
        // (F_HUFF_IPD root leaf on bit1, codeword "1"), OPD the same
        // (F_HUFF_OPD shares the same root-leaf codeword "1"), reserved_ps
        // bit. Hand-built + cross-checked against a Python mirror of
        // decode_param (41 total bits consumed).
        let buf = [0xC2u8, 0x40, 0x01, 0x17, 0xDF, 0x00];
        let mut b = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        let n = ps_data(&mut ps, &mut b, 48).unwrap();
        assert_eq!(n, 41);
        assert!(ps.enable_ext);
        assert!(ps.params.enable_ipdopd);
        assert_eq!(ps.frame.nr_ipdopd_par, 5);
        // All-zero deltas -> IPD/OPD stay at the zero-phase center (q=0)
        // after the 5->11 resolution-expand (is34=false -> target 11).
        for band in 0..11 {
            assert!((ps.params.ipd_cos[0][band] - 1.0).abs() < 1e-6);
            assert!(ps.params.ipd_sin[0][band].abs() < 1e-6);
            assert!((ps.params.opd_cos[0][band] - 1.0).abs() < 1e-6);
            assert!(ps.params.opd_sin[0][band].abs() < 1e-6);
        }
    }

    /// Test-only MSB-first bit accumulator: hand-laying-out a whole
    /// `ps_data()` frame (header + frame body + IID + extension + fill) as
    /// raw hex bytes is exactly the error-prone process the existing tests
    /// above did by hand; this builder does the same MSB-first packing
    /// programmatically for the FIX-1 regression test below, which needs
    /// two full frames.
    struct BitBuilder {
        bytes: std::vec::Vec<u8>,
        acc: u32,
        nbits: u32,
    }

    impl BitBuilder {
        fn new() -> Self {
            Self {
                bytes: std::vec::Vec::new(),
                acc: 0,
                nbits: 0,
            }
        }

        fn push(&mut self, val: u32, n: u32) {
            for i in (0..n).rev() {
                let bit = (val >> i) & 1;
                self.acc = (self.acc << 1) | bit;
                self.nbits += 1;
                if self.nbits == 8 {
                    self.bytes.push(self.acc as u8);
                    self.acc = 0;
                    self.nbits = 0;
                }
            }
        }

        fn finish(mut self) -> std::vec::Vec<u8> {
            if self.nbits > 0 {
                self.acc <<= 8 - self.nbits;
                self.bytes.push(self.acc as u8);
            }
            // Pad a few trailing zero bytes so `BitReader`'s fixed-width
            // reads never run off a too-short buffer mid-frame.
            self.bytes.extend_from_slice(&[0u8; 4]);
            self.bytes
        }
    }

    #[test]
    fn enable_ipdopd_persists_across_frames_and_freezes_ipd_when_no_fresh_ext() {
        // Frame A: full header (enable_iid=1, iid_mode=0 -> 10/5 bands,
        // enable_icc=0, enable_ext=1), frame_class=0, num_env_idx=1 ->
        // num_env=1. IID: dt=0, 10 zero-delta codewords (neutral, not under
        // test). Extension: ps_extension_size=3 (8*3=24 budget), ps_ext_id=0,
        // enable_ipdopd=1, IPD dt=0 (freq) with band0's codeword walking
        // F_HUFF_IPD to the delta=+1 leaf ("000") and bands 1..4 zero-delta
        // ("1" root leaf) -- freq-direction accumulation carries that +1
        // forward, so raw IPD index is 1 (nonzero) across all 5 transmitted
        // bands. OPD: dt=0, all-zero (not under test). reserved_ps bit, then
        // 6 fill bits (bits_left lands at 6 after the extension body).
        let mut a = BitBuilder::new();
        a.push(1, 1); // enable_ps_header
        a.push(1, 1); // enable_iid
        a.push(0, 3); // iid_mode = 0
        a.push(0, 1); // enable_icc
        a.push(1, 1); // enable_ext
        a.push(0, 1); // frame_class
        a.push(1, 2); // num_env_idx = 1 -> num_env = 1
        a.push(0, 1); // IID dt = 0 (freq)
        for _ in 0..10 {
            a.push(0, 1); // 10 zero-delta IID codewords
        }
        a.push(3, 4); // ps_extension_size = 3 -> 24-bit budget
        a.push(0, 2); // ps_ext_id = 0
        a.push(1, 1); // enable_ipdopd = 1
        a.push(0, 1); // IPD dt = 0 (freq)
        a.push(0b000, 3); // band0: F_HUFF_IPD "000" -> leaf -30 -> delta +1
        for _ in 0..4 {
            a.push(1, 1); // bands 1..4: root leaf "1" -> delta 0 (carries the +1)
        }
        a.push(0, 1); // OPD dt = 0 (freq)
        for _ in 0..5 {
            a.push(1, 1); // 5 zero-delta OPD codewords
        }
        a.push(0, 1); // reserved_ps
        for _ in 0..6 {
            a.push(0, 1); // trailing fill bits (bits_left == 6 here)
        }
        let buf_a = a.finish();
        let mut ba = BitReader::new(&buf_a);
        let mut ps = PsDecoder::default();
        ps_data(&mut ps, &mut ba, buf_a.len() * 8).unwrap();

        assert!(ps.enable_ipdopd);
        assert!(ps.params.enable_ipdopd);
        // Band 10 is forced to 0 by the 5->11 expand (no 11th source band);
        // bands 0..10 all carry the nonzero index-1 phase.
        for band in 0..10 {
            assert!(
                (ps.params.ipd_cos[0][band] - core::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6,
                "band {band}: {}",
                ps.params.ipd_cos[0][band]
            );
            assert!(
                (ps.params.ipd_sin[0][band] - core::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6,
                "band {band}: {}",
                ps.params.ipd_sin[0][band]
            );
        }
        let frame_a_ipd_cos = ps.params.ipd_cos;
        let frame_a_ipd_sin = ps.params.ipd_sin;

        // Frame B: a fresh header that explicitly disables everything
        // (enable_iid=0, enable_icc=0, enable_ext=0), frame_class=0,
        // num_env_idx=1 -> num_env=1. No id==0 extension record this frame
        // at all (enable_ext=0) -- the persisted `ps.enable_ipdopd` must
        // stay true, and since it's true, `ipd_cos`/`ipd_sin` must NOT be
        // reset to neutral: they hold frame A's values untouched (FFmpeg's
        // "don't touch the array" -- the memset only fires when the
        // persisted flag is false).
        let mut bld = BitBuilder::new();
        bld.push(1, 1); // enable_ps_header
        bld.push(0, 1); // enable_iid = 0
        bld.push(0, 1); // enable_icc = 0
        bld.push(0, 1); // enable_ext = 0
        bld.push(0, 1); // frame_class
        bld.push(1, 2); // num_env_idx = 1 -> num_env = 1
        let buf_b = bld.finish();
        let mut bb = BitReader::new(&buf_b);
        ps_data(&mut ps, &mut bb, buf_b.len() * 8).unwrap();

        assert!(!ps.enable_ext);
        // The persisted flag must survive a frame with no fresh extension.
        assert!(
            ps.enable_ipdopd,
            "enable_ipdopd must persist across a frame with enable_ext=0"
        );
        assert!(ps.params.enable_ipdopd);
        // ipd_cos/ipd_sin must be frozen at frame A's values, not reset to
        // the neutral (1, 0) center.
        assert_eq!(ps.params.ipd_cos, frame_a_ipd_cos);
        assert_eq!(ps.params.ipd_sin, frame_a_ipd_sin);
        for band in 0..10 {
            assert!((ps.params.ipd_cos[0][band] - 1.0).abs() > 1e-3);
        }
    }

    // --- Task 9b: synthetic-envelope coverage fixup (FFmpeg aacps_common.c
    // "Fix up envelopes" ~L222-257) ---

    #[test]
    fn num_env_zero_frame_reuses_previous_env() {
        use crate::formats::aac::ps::tables::SF_IID_NORMAL;
        // Frame A: header enable_iid=1 (iid_mode=0 -> 10 bands), enable_icc=0,
        // enable_ext=0; fixed class, num_env_idx=2 -> num_env=2 (borders
        // [0,16,32], fully covered -> no fixup). IID env0 all-zero-delta ->
        // every band index 0 (neutral SF[7]); IID env1 band0 "100" (+1) then
        // zero -> every band index 1 (SF[8]). After A: num_env_old = 2 and
        // params.iid_c[1] (the LAST env) = SF[8].
        let mut a = BitBuilder::new();
        a.push(1, 1); // enable_ps_header
        a.push(1, 1); // enable_iid
        a.push(0, 3); // iid_mode = 0 (10 band)
        a.push(0, 1); // enable_icc = 0
        a.push(0, 1); // enable_ext = 0
        a.push(0, 1); // frame_class = 0 (fixed)
        a.push(2, 2); // num_env_idx = 2 -> num_env = 2
        a.push(0, 1); // env0 IID dt = 0 (freq)
        for _ in 0..10 {
            a.push(0, 1); // 10 zero-delta codewords -> all index 0
        }
        a.push(0, 1); // env1 IID dt = 0 (freq)
        a.push(0b100, 3); // band0 delta +1
        for _ in 0..9 {
            a.push(0, 1); // bands 1..9 zero-delta -> all index 1
        }
        let buf_a = a.finish();
        let mut ba = BitReader::new(&buf_a);
        let mut ps = PsDecoder::default();
        ps_data(&mut ps, &mut ba, buf_a.len() * 8).unwrap();
        assert_eq!(ps.frame.num_env, 2);
        assert!((ps.params.iid_c[1][0] - SF_IID_NORMAL[8]).abs() < 1e-6);
        assert!((ps.params.iid_c[0][0] - SF_IID_NORMAL[7]).abs() < 1e-6);
        let prev_last = ps.params.iid_c[1]; // frame A's last-env row

        // Frame B: header absent (reuse iid on), fixed class, num_env_idx=0 ->
        // num_env=0 (reuse-previous). The fixup must append ONE synthetic
        // envelope sourced from the PREVIOUS frame's last env (num_env_old-1 =
        // 1): num_env==1, border [0,32], params.iid_c[0] == frame A's env1.
        let mut b = BitBuilder::new();
        b.push(0, 1); // enable_ps_header = 0 (reuse)
        b.push(0, 1); // frame_class = 0
        b.push(0, 2); // num_env_idx = 0 -> num_env = 0
        let buf_b = b.finish();
        let mut bb = BitReader::new(&buf_b);
        ps_data(&mut ps, &mut bb, buf_b.len() * 8).unwrap();

        assert_eq!(ps.frame.num_env, 1, "synthetic env appended");
        assert_eq!(
            &ps.frame.border_position[..2],
            &[0, 32],
            "coverage complete"
        );
        for (band, (&got, &want)) in ps.params.iid_c[0]
            .iter()
            .zip(prev_last.iter())
            .take(20)
            .enumerate()
        {
            assert!(
                (got - want).abs() < 1e-6,
                "band {band}: env-0 must equal the previous frame's last env"
            );
        }
        // The held value is the non-neutral SF[8], not a centre-mono neutral.
        assert!((ps.params.iid_c[0][0] - SF_IID_NORMAL[8]).abs() < 1e-6);
    }

    #[test]
    fn variable_short_frame_appends_synthetic_env() {
        use crate::formats::aac::ps::tables::SF_IID_NORMAL;
        // Variable-class frame, num_env_idx=0 -> num_env=1, ONE border read as
        // read(5)+1 = 9+1 = 10 (< 32 -> not fully covered). IID env0 band0
        // "100" (+1) then zero -> every band index 1 (SF[8]). The fixup must
        // append a synthetic env: num_env==2, borders [0,10,32], with
        // params.iid_c[1] copied from the prior env-0.
        let mut f = BitBuilder::new();
        f.push(1, 1); // enable_ps_header
        f.push(1, 1); // enable_iid
        f.push(0, 3); // iid_mode = 0
        f.push(0, 1); // enable_icc = 0
        f.push(0, 1); // enable_ext = 0
        f.push(1, 1); // frame_class = 1 (variable)
        f.push(0, 2); // num_env_idx = 0 -> num_env = 1
        f.push(9, 5); // border raw = 9 -> border = 10
        f.push(0, 1); // env0 IID dt = 0 (freq)
        f.push(0b100, 3); // band0 delta +1
        for _ in 0..9 {
            f.push(0, 1); // bands 1..9 zero-delta -> all index 1
        }
        let buf = f.finish();
        let mut br = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        ps_data(&mut ps, &mut br, buf.len() * 8).unwrap();

        assert_eq!(ps.frame.num_env, 2, "synthetic env appended");
        assert_eq!(
            &ps.frame.border_position[..3],
            &[0, 10, 32],
            "coverage extended to slot 32"
        );
        for band in 0..20 {
            assert!(
                (ps.params.iid_c[1][band] - ps.params.iid_c[0][band]).abs() < 1e-6,
                "band {band}: synthetic env must copy the prior env"
            );
        }
        assert!((ps.params.iid_c[1][0] - SF_IID_NORMAL[8]).abs() < 1e-6);
    }

    #[test]
    fn cold_start_reuse_frame_does_not_panic() {
        use crate::formats::aac::ps::tables::SF_IID_NORMAL;
        // First-ever frame is a reuse frame: header enable_iid=1, enable_icc=0,
        // enable_ext=0; fixed class num_env_idx=0 -> num_env=0 with NO prior
        // envelope (num_env_old==0 at cold start). The fixup's source = -1
        // guards the copy (nothing to copy); it must not panic and must fill a
        // safe neutral synthetic env (centre mono): num_env==1, border [0,32].
        let mut f = BitBuilder::new();
        f.push(1, 1); // enable_ps_header
        f.push(1, 1); // enable_iid
        f.push(0, 3); // iid_mode = 0
        f.push(0, 1); // enable_icc = 0
        f.push(0, 1); // enable_ext = 0
        f.push(0, 1); // frame_class = 0
        f.push(0, 2); // num_env_idx = 0 -> num_env = 0
        let buf = f.finish();
        let mut br = BitReader::new(&buf);
        let mut ps = PsDecoder::default();
        ps_data(&mut ps, &mut br, buf.len() * 8).unwrap(); // must not panic

        assert_eq!(ps.frame.num_env, 1, "synthetic env still appended cold");
        assert_eq!(&ps.frame.border_position[..2], &[0, 32]);
        for band in 0..20 {
            assert!(
                (ps.params.iid_c[0][band] - SF_IID_NORMAL[7]).abs() < 1e-6,
                "band {band}: cold-start synthetic env must be neutral"
            );
        }
    }
}
