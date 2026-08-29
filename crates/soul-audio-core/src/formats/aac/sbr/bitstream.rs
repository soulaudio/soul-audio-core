//! Fill-element SBR frame envelope (`sbr-reference.md` §1 / design-spec
//! §2.4): the `sbr_extension_data()` entry point that consumes the optional
//! CRC field and `bs_header_flag`, dispatches the header (`header::
//! sbr_header`) and — on a header change/reset — rebuilds the frequency tables
//! and re-inits the per-channel DSP state, then runs the §10 per-channel drive
//! ([`super::drive`], which decodes `sbr_data`, parses the trailing
//! `bs_extension` structure via [`parse_extensions`], and synthesises 2×-rate
//! PCM). The conformant `bs_extension` parse ([`parse_extensions`]) reads the
//! `bs_extended_data` gate and its length-prefixed sub-extensions, dispatching
//! `EXTENSION_ID_PS` to `PsDecoder::parse_frame` (Phase 3 PS). It runs inside
//! `drive` — before the QMF-domain synthesis (R3) — because PS's per-frame
//! parameters must be decoded before `apply` consumes the QMF matrix.
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
//! ready. `EXTENSION_ID_PS` (the PS seam) is dispatched by [`parse_extensions`].

use super::header::sbr_header;
use super::{SbrOutput, SbrState};
use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;

/// `bs_extension_id` value reserved for Parametric Stereo (`sbr-reference.md`
/// §1 / design-spec §2.4). [`parse_extensions`] dispatches it to
/// `PsDecoder::parse_frame`, trusting the returned bit count to advance the
/// conformant `sbr_extension_data` loop.
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

    // The trailing `bs_extension` loop (§1 / design-spec §2.4) has already run
    // inside [`super::drive`] via [`parse_extensions`] — placed there (R2/R3) so
    // PS's per-frame parameters are decoded *before* the QMF-domain synthesis
    // that consumes them. All that remains here is to advance to `end_bit`:
    // `parse_extensions` conformantly consumes the `bs_extended_data` gate and
    // its sub-extensions, leaving the trailing fill/pad bits, and a malformed
    // sub-extension may `break` with `bit_pos` still short of `end_bit`.
    // `clamped_skip` handles either case, landing exactly on `end_bit`; no
    // assertion (malformed input must never panic — the §3.3 firmware invariant).
    let pad = end_bit.saturating_sub(payload.bits.bit_pos());
    let _ = clamped_skip(&mut payload.bits, end_bit, pad);

    // `applied` is true whenever SBR was active this frame (2048 samples/channel
    // written to `out`); false for the pure-LC pass-through (no header yet).
    Ok(applied)
}

/// Parse the conformant trailing `bs_extension` structure (`ps-reference.md`
/// §1), dispatching a Parametric Stereo payload to `state.ps`. Called from
/// [`super::drive`] after `parse_sbr_data` and before the per-channel synthesis
/// (R2/R3), so PS's parameters for this frame are decoded before `apply`
/// consumes the QMF matrix.
///
/// Structure: `bs_extended_data` (1-bit gate); if set, `cnt = read(4)` (+
/// `read(8)` when `cnt == 15`) gives the extension byte count. `nr_bits_left =
/// 8·cnt` (clamped to the payload) then drives
/// `while nr_bits_left > 7 { id = read(2); nr_bits_left -= 2; … }`, dispatching
/// `EXTENSION_ID_PS` to `PsDecoder::parse_frame` and subtracting its returned
/// bit count. Every read goes through [`clamped_read`]; any overrun or a PS
/// parse error stops the loop (never panics — the firmware no-panic invariant).
/// Residual fill/pad bits are consumed by `sbr_extension_data`'s pad-to-`end_bit`.
///
/// The non-PS path is untouched: a `0` gate reads one bit and returns; an
/// unknown `bs_extension_id` breaks. Neither activates `state.ps`, and because
/// this only advances `payload.bits` (never read by synthesis) the SBR output
/// stays bit-identical.
pub(super) fn parse_extensions(payload: &mut SbrPayload, state: &mut SbrState) {
    let end_bit = payload.end_bit;
    let bits = &mut payload.bits;

    // `bs_extended_data` gate (0 ⇒ no extension).
    let Some(gate) = clamped_read(bits, end_bit, 1) else {
        return;
    };
    if gate != 1 {
        return;
    }

    // `cnt` extension byte count ([NOPANIC]: every read clamped to `end_bit`).
    let Some(mut cnt) = clamped_read(bits, end_bit, 4) else {
        return;
    };
    if cnt == 15 {
        let Some(extra) = clamped_read(bits, end_bit, 8) else {
            return;
        };
        cnt += extra;
    }

    // `nr_bits_left = 8·cnt`, clamped to what the payload actually holds.
    let remaining = end_bit.saturating_sub(bits.bit_pos());
    let mut nr_bits_left = (8 * cnt as usize).min(remaining) as i64;

    while nr_bits_left > 7 {
        let Some(id) = clamped_read(bits, end_bit, 2) else {
            break;
        };
        nr_bits_left -= 2;
        match id {
            EXTENSION_ID_PS => {
                // Bound the PS parse to the remaining sub-extension budget.
                let ext_end = end_bit.min(bits.bit_pos() + nr_bits_left.max(0) as usize);
                match state.ps.parse_frame(bits, ext_end) {
                    Ok(used) => nr_bits_left -= used as i64,
                    Err(_) => break,
                }
            }
            // No other `bs_extension_id` is defined at this level; stop (the
            // pad-to-`end_bit` consumes whatever remains).
            _ => break,
        }
    }
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
    fn parse_extensions_dispatches_ps_and_activates() {
        // The conformant `bs_extension` structure carrying a PS payload (R2):
        //   bs_extended_data = 1            (1 bit, gate)
        //   cnt              = 2            (4 bits -> nr_bits_left = 16)
        //   bs_extension_id  = 2  (PS)      (2 bits)
        //   ps_data(): enable_ps_header=1, enable_iid=0, enable_icc=0,
        //             enable_ext=0, frame_class=0, num_env_idx=0  (7 bits)
        // MSB-first: 1 0010 10 1000000  =  0x95 0x00.  A header was seen, so
        // `ps_data_available` (is_active) flips true; 14 bits are consumed
        // (gate 1 + cnt 4 + id 2 + ps_data 7), leaving nr_bits_left == 7.
        let buf = [0x95u8, 0x00, 0x00, 0x00];
        let mut p = SbrPayload {
            crc: false,
            bits: BitReader::new(&buf),
            end_bit: 32,
        };
        let mut st = SbrState::default();
        assert!(!st.ps.is_active());
        parse_extensions(&mut p, &mut st);
        assert!(
            st.ps.is_active(),
            "EXTENSION_ID_PS must be dispatched to ps::parse_frame"
        );
        assert_eq!(p.bits.bit_pos(), 14);
    }

    #[test]
    fn parse_extensions_no_gate_is_inert() {
        // bs_extended_data = 0: no extension present. `parse_extensions` reads
        // exactly the gate bit and returns; PS stays inactive (the non-PS path
        // is untouched — the SBR conformance golden relies on this).
        let buf = [0x00u8, 0x00];
        let mut p = SbrPayload {
            crc: false,
            bits: BitReader::new(&buf),
            end_bit: 16,
        };
        let mut st = SbrState::default();
        parse_extensions(&mut p, &mut st);
        assert!(!st.ps.is_active());
        assert_eq!(p.bits.bit_pos(), 1);
    }

    #[test]
    fn parse_extensions_unknown_id_breaks_without_panic() {
        // bs_extended_data = 1, cnt = 1 (nr_bits_left = 8), bs_extension_id = 0
        // (not PS): the loop breaks on the unknown id. No PS activation, no
        // panic, and no read past `end_bit`.
        // MSB-first: 1 0001 00 ...  = 0x88.
        let buf = [0x88u8, 0x00];
        let mut p = SbrPayload {
            crc: false,
            bits: BitReader::new(&buf),
            end_bit: 16,
        };
        let mut st = SbrState::default();
        parse_extensions(&mut p, &mut st);
        assert!(!st.ps.is_active());
        assert!(p.bits.bit_pos() <= 16);
    }
}
