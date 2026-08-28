//! Fill-element SBR frame envelope (`sbr-reference.md` §1 / design-spec
//! §2.4): the `sbr_extension_data()` entry point that consumes the optional
//! CRC field and `bs_header_flag`, dispatches the header (`header::
//! sbr_header`) and — on a header change/reset — rebuilds the frequency tables
//! and re-inits the per-channel DSP state, then runs the §10 per-channel drive
//! ([`super::drive`], which decodes `sbr_data` and synthesises 2×-rate PCM),
//! then walks the trailing `bs_extension` loop of length-prefixed
//! `bs_extension_id`-tagged sub-extensions, length-skipping each in Phase 2
//! (Phase 3 dispatches `EXTENSION_ID_PS` to `ps::parse_frame` from this same
//! loop).
//!
//! # The no-panic clamp
//!
//! Every read in this module goes through [`clamped_read`] (or
//! [`clamped_skip`] for multi-bit skips), which returns `None` rather than
//! reading past `payload.end_bit`. On the first `None`, `sbr_extension_data`
//! stops and returns `Ok(false)` — SBR is discarded for this frame and the
//! decoder falls back to upsample-only, matching faad2's `if (8*cnt <
//! num_sbr_bits) return 1;` / FFmpeg's `sbr_turnoff()`. This is the firmware
//! no-panic invariant every later SBR parser (header/grid/envelope/HF-gen)
//! slots into: bounds-check against `end_bit`, never index/read blind.
//!
//! `sbr_extension_data` is reached from `syntax::decode_fil` (which records the
//! SBR payload's bit span) via `super::decode_fill_sbr` once the LC core PCM is
//! ready. `EXTENSION_ID_PS` (the PS seam) carries its own targeted
//! `dead_code` allow, still unused in Phase 2.

use super::header::sbr_header;
use super::{SbrOutput, SbrState};
use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;

/// `bs_extension_id` value reserved for Parametric Stereo (`sbr-reference.md`
/// §1 / design-spec §2.4). Phase 2 (this crate today) length-skips every
/// `bs_extension_id` uniformly, this one included; Phase 3 dispatches it to
/// `ps::parse_frame` from the loop in [`sbr_extension_data`] instead.
#[allow(dead_code)] // Phase-3 (PS) seam: dispatched by ps::parse_frame, unused in Phase 2
const EXTENSION_ID_PS: u32 = 2;

/// Which element preceded this fill element — selects the SBR channel count
/// and the SCE vs CPE (`bs_coupling`-aware) `sbr_data` parse in
/// [`super::drive`].
pub(super) enum IdAac {
    Sce,
    Cpe,
}

/// One fill element's SBR extension payload: the bit span to read from and
/// its exclusive end (`sbr-reference.md` §1 — the `EXT_SBR_DATA`/
/// `EXT_SBR_DATA_CRC` branch of `extension_payload(count)`, minus the 4-bit
/// `extension_type` the caller already consumed). `end_bit` is what every
/// read in this module clamps against — never read past it.
pub(super) struct SbrPayload<'a> {
    pub crc: bool,
    pub bits: BitReader<'a>,
    pub end_bit: usize,
}

/// Read `n` bits (`0 <= n <= 32`) if doing so stays within `[.., end_bit)`,
/// else `None` without advancing `bits`. The no-panic clamp every SBR-path
/// read goes through. `pub(super)` so sibling parsers (`header`, and later
/// grid/envelope/HF-gen) share this one clamp check instead of duplicating
/// it.
pub(super) fn clamped_read(bits: &mut BitReader, end_bit: usize, n: u32) -> Option<u32> {
    if bits.bit_pos() + n as usize > end_bit {
        return None;
    }
    Some(bits.read(n))
}

/// Skip `n` bits if doing so stays within `[.., end_bit)`, else `None`
/// without advancing `bits`. Reads in `<=32`-bit chunks (mirrors
/// `syntax::skip_bits`) so it always terminates regardless of `n`.
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

/// Parse the fill-element SBR envelope: consume the optional CRC field and
/// the `bs_header_flag`, dispatch the header (`header::sbr_header`) + table
/// rebuild/reset, run the §10 drive ([`super::drive`]), then walk the trailing
/// `bs_extension` loop. Returns `Ok(true)` when SBR was active this frame (2048
/// samples/channel written to `out`), `Ok(false)` for the pure-LC pass-through
/// (no header accepted yet). Never reads past `payload.end_bit`.
///
/// A rejected/overrun header (`sbr_header` returns `Err`) falls back to
/// upsample-only for this frame without touching `state`, matching §1's "if
/// the SBR read overruns the declared byte count, discard the frame's SBR".
/// An accepted header promotes `state.header`/`state.active` and (on a reset /
/// the first header) rebuilds the frequency tables and re-inits the per-channel
/// DSP state (`sbr-reference.md` §2/§3); the §10 drive ([`super::drive`]) then
/// runs whenever SBR is active, writing 2048 samples/channel into `out`.
/// `output_rate` is the SBR operating rate (= 2×core) the table math keys off.
pub(super) fn sbr_extension_data(
    payload: &mut SbrPayload,
    id_aac: IdAac,
    state: &mut SbrState,
    core_pcm: &[[f32; 1024]; 2],
    n_ch: usize,
    output_rate: u32,
    out: &mut SbrOutput,
) -> Result<bool, DecodeError> {
    let end_bit = payload.end_bit;

    if payload.crc {
        // bs_sbr_crc_bits: decoders may validate or skip; this one skips —
        // consumed only so bit-position bookkeeping stays correct.
        if clamped_read(&mut payload.bits, end_bit, 10).is_none() {
            return Ok(false);
        }
    }

    let bs_header_flag = match clamped_read(&mut payload.bits, end_bit, 1) {
        Some(v) => v,
        None => return Ok(false),
    };

    if bs_header_flag == 1 {
        match sbr_header(&mut payload.bits, end_bit, state.header.as_ref()) {
            Ok((parsed, reset)) => {
                let need_tables = reset.reset || state.tables.is_none();
                state.header = Some(parsed);
                state.active = true;
                if need_tables {
                    // §3 recompute + §2/§10 per-channel re-init (requirement #2).
                    state.rebuild_tables(output_rate);
                    state.reset_channels();
                } else if reset.limiter_only {
                    // Just the limiter table changed; a full rebuild is a
                    // superset and keeps the code path single.
                    state.rebuild_tables(output_rate);
                }
            }
            Err(_) => return Ok(false),
        }
    }

    // §4-§10 drive: once SBR is active, run the full per-channel decode
    // (analysis → HF gen/adjust → assemble → synthesis) writing 2048
    // samples/channel into `out`. Advances `payload.bits` past `sbr_data`.
    let applied = if state.active {
        super::drive(payload, id_aac, state, core_pcm, n_ch, out)
    } else {
        false
    };

    // Trailing `bs_extension` loop (§1 / design-spec §2.4): zero or more
    // length-prefixed `bs_extension_id`-tagged sub-extensions. Phase 2
    // length-skips every one (including `EXTENSION_ID_PS`); Phase 3
    // dispatches `EXTENSION_ID_PS` to `ps::parse_frame` here instead,
    // trusting its returned bit count for this loop's own bookkeeping.
    // Each iteration consumes >=10 clamped bits (id + length) before it can
    // loop again, so the loop always makes forward progress or bails.
    // A clamped-read failure here just stops extension parsing: the §10 drive
    // above already produced this frame's output (`applied`), and the outer
    // `decode_fil` consumes the whole fill element by its byte count, so the
    // exact trailing-bit position no longer gates the result — `break`, don't
    // discard the decoded frame.
    while end_bit.saturating_sub(payload.bits.bit_pos()) >= 8 {
        let _bs_extension_id = match clamped_read(&mut payload.bits, end_bit, 2) {
            Some(v) => v,
            None => break,
        };
        let ext_len_bytes = match clamped_read(&mut payload.bits, end_bit, 8) {
            Some(v) => v,
            None => break,
        };
        if clamped_skip(&mut payload.bits, end_bit, ext_len_bytes as usize * 8).is_none() {
            break;
        }
    }

    // Advance to `end_bit`. Normally the loop leaves <8 padding bits, but a
    // malformed sub-extension whose declared length overran the clamp `break`s
    // with `bit_pos` still short of `end_bit` (the failed `clamped_skip` didn't
    // advance) — so `pad` can be >= 8. `clamped_skip` handles either case,
    // landing exactly on `end_bit`; no assertion (malformed input must never
    // panic — the §3.3 firmware invariant).
    let pad = end_bit.saturating_sub(payload.bits.bit_pos());
    let _ = clamped_skip(&mut payload.bits, end_bit, pad);

    // `applied` is true whenever SBR was active this frame (2048 samples/channel
    // written to `out`); false for the pure-LC pass-through (no header yet).
    Ok(applied)
}

#[cfg(test)]
// `crc_field_consumed_when_flagged`/`overrun_falls_back_to_upsample` are
// transcribed verbatim from the task brief, `assert_eq!(.., false)` included.
#[allow(clippy::bool_assert_comparison)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;

    #[test]
    fn crc_field_consumed_when_flagged() {
        // 10-bit CRC then bs_header_flag=0 -> no header yet -> upsample-only.
        let buf = [0u8; 4];
        let mut p = SbrPayload {
            crc: true,
            bits: BitReader::new(&buf),
            end_bit: 32,
        };
        let mut st = SbrState::default();
        let core = [[0f32; 1024]; 2];
        let mut out = SbrOutput::default();
        assert_eq!(
            sbr_extension_data(&mut p, IdAac::Sce, &mut st, &core, 1, 44_100, &mut out).unwrap(),
            false
        );
        assert!(p.bits.bit_pos() >= 11); // 10 CRC + 1 header_flag consumed
    }

    #[test]
    fn overrun_falls_back_to_upsample() {
        // Declare a tiny span but a header_flag=1 that would demand more bits.
        let buf = [0b1000_0000u8]; // header_flag=1 at first bit
        let mut p = SbrPayload {
            crc: false,
            bits: BitReader::new(&buf),
            end_bit: 3,
        };
        let mut st = SbrState::default();
        let core = [[0f32; 1024]; 2];
        let mut out = SbrOutput::default();
        // Must not panic / must not read past end_bit.
        let r =
            sbr_extension_data(&mut p, IdAac::Sce, &mut st, &core, 1, 44_100, &mut out).unwrap();
        assert_eq!(r, false);
        assert!(p.bits.bit_pos() <= 3);
    }

    #[test]
    fn bs_extension_loop_length_skips_bogus_sub_extension() {
        // bit0: bs_header_flag=0
        // bits1-2: bs_extension_id=0b10 (=2, EXTENSION_ID_PS — Phase 2 skips
        //          it like any other id)
        // bits3-10: an 8-bit byte length = 1 (skip one byte)
        // bits11-18: 8 bits of arbitrary sub-extension payload to skip
        // end_bit=19 lands exactly on the skip's end, so a correct
        // length-skip must leave bit_pos == end_bit, not overrun it.
        let buf = [0x40u8, 0x3F, 0xE0];
        let mut p = SbrPayload {
            crc: false,
            bits: BitReader::new(&buf),
            end_bit: 19,
        };
        let mut st = SbrState::default();
        let core = [[0f32; 1024]; 2];
        let mut out = SbrOutput::default();
        let r =
            sbr_extension_data(&mut p, IdAac::Sce, &mut st, &core, 1, 44_100, &mut out).unwrap();
        assert_eq!(r, false);
        assert_eq!(p.bits.bit_pos(), 19);
    }
}
