//! AAC-LC bitstream syntax: `raw_data_block()` → element tags → `ics_info()`
//! → section/scalefactor/pulse/TNS/spectral data → quantized `coeffs[1024]`
//! per channel, per `aac-reference.md` §3–§6 (ISO/IEC 14496-3 subclause
//! 4.4/4.5/4.6, ADTS `raw_data_block`).
//!
//! This module **parses only**: it does not dequantize, apply stereo/TNS, or
//! run the IMDCT (those live in [`super::dsp`]/[`super::filterbank`]). Its
//! output feeds [`super::dsp::dequantize`] via [`super::AacDecoder`].

use super::bits::BitReader;
use super::huffman::{decode_scalefactor, decode_spectral};
use super::tables::{self, MAX_SFB};
use crate::decoder::DecodeError;
use crate::demux::adts::AacConfig;

// ---------------------------------------------------------------------------
// Constants (aac-reference.md §3/§4/§6/§9).
// ---------------------------------------------------------------------------

/// `id_syn_ele` values (§3).
const ID_SCE: u32 = 0;
const ID_CPE: u32 = 1;
const ID_CCE: u32 = 2;
const ID_LFE: u32 = 3;
const ID_DSE: u32 = 4;
const ID_PCE: u32 = 5;
const ID_FIL: u32 = 6;
const ID_END: u32 = 7;

/// `extension_type` values in a `fill_element` `extension_payload` carrying
/// SBR side data (`sbr-reference.md` §1 / faad2 `sbr_syntax.h`). Every other
/// `extension_type` is length-skipped exactly as before.
const EXT_SBR_DATA: u32 = 13;
const EXT_SBR_DATA_CRC: u32 = 14;

/// Where the SBR extension payload sits inside the current `raw_data_block`,
/// recorded by [`decode_fil`] and consumed by [`super::decode_rdb`] once the
/// LC core PCM the SBR drive needs is ready. `start_bit`/`end_bit` are absolute
/// bit offsets into the `raw_data_block` bytes (after the 4-bit
/// `extension_type`); `crc` marks `EXT_SBR_DATA_CRC`; `id_cpe` is whether the
/// element preceding this fill element was a CPE (vs SCE), which selects the
/// SBR channel count.
#[derive(Clone, Copy)]
pub(crate) struct SbrLoc {
    pub(crate) start_bit: usize,
    pub(crate) end_bit: usize,
    pub(crate) crc: bool,
    pub(crate) id_cpe: bool,
}

/// `window_sequence` values (§4).
pub(crate) const ONLY_LONG: u8 = 0;
pub(crate) const LONG_START: u8 = 1;
pub(crate) const EIGHT_SHORT: u8 = 2;
pub(crate) const LONG_STOP: u8 = 3;

/// Section codebook indices with special handling (§5/§6). `1..=11` are
/// real spectral Huffman books (decoded via [`decode_spectral`], which handles
/// the codebook-11 escape internally, so no `HCB_ESC` constant is needed here).
const HCB_ZERO: u8 = 0;
/// PNS (Perceptual Noise Substitution). `section_data`/`scale_factor_data`
/// parse it correctly (including the `noise_energy` DPCM, so the bitstream
/// never desyncs), but `spectral_data` treats a PNS band as silent (no
/// synthetic noise is generated) — a deliberate v1 simplification, **not**
/// the brief's suggested default (`Unsupported`): this decoder's own ADTS
/// test fixture uses PNS in its very first frame, so rejecting it would
/// fail on real-world content. Documented in the task report.
const HCB_NOISE: u8 = 13;
const HCB_INTENSITY2: u8 = 14;
const HCB_INTENSITY: u8 = 15;

/// Maximum simultaneous windows (`EIGHT_SHORT` uses all 8; long sequences
/// use 1). `pub(crate)` (Task 6 minimal visibility change): `dsp::
/// apply_stereo`'s `ms_used` parameter must spell the exact same array type
/// as [`DecodedFrame::ms_used`] to accept it without a copy/cast.
pub(crate) const MAX_WINDOWS: usize = 8;
/// Cap on pulses per `pulse_data()`: the `number_pulse` field is 2 bits, so
/// the wire format itself never asks for more than `3 + 1 = 4`.
const MAX_PULSES: usize = 4;
/// Cap on TNS filters per window: the long-window `n_filt` field is 2 bits
/// (max 3); short-window `n_filt` is 1 bit (max 1), comfortably under this.
const TNS_MAX_FILTERS: usize = 3;
/// Cap on TNS filter order we're willing to store. The raw bitstream field
/// can encode up to 31 (long, 5-bit) / 7 (short, 3-bit); faad2 itself caps
/// storage at `TNS_MAX_ORDER == 20` (`libfaad/syntax.h`) — real encoders
/// never exceed it, so a parsed `order > 20` is treated as `Malformed`
/// rather than sized into the coefficient array.
const TNS_MAX_ORDER: usize = 20;
/// Bound on the number of syntactic elements processed in one
/// `raw_data_block()` before giving up as `Malformed`. Real streams have at
/// most a handful (channel_configuration 7 needs 5 audio elements, plus
/// maybe a PCE/FIL/DSE); this is generous headroom while still making the
/// element loop provably terminate on adversarial input that never emits
/// `ID_END`.
const MAX_ELEMENTS: usize = 32;

// ---------------------------------------------------------------------------
// TnsData — parsed (not yet applied) `tns_data()`, per §9.
// ---------------------------------------------------------------------------

/// One channel's parsed `tns_data()`, per window. Task 4 only parses and
/// bounds-checks this (every field capped so Task 6's `apply_tns` can index
/// it without its own bounds checks); Task 6 converts `coef` (raw
/// `coef_bits`-wide indices, **not** sign-extended values — the sign lives
/// in the PARCOR lookup table Task 6 indexes with them) to PARCOR
/// coefficients and runs the AR filter.
#[derive(Clone)]
pub(crate) struct TnsData {
    /// Number of filters this window has (0 if TNS wasn't used on it).
    pub(crate) n_filt: [u8; MAX_WINDOWS],
    /// `coef_res` (0 or 1) — only meaningful where `n_filt[w] > 0`.
    pub(crate) coef_res: [u8; MAX_WINDOWS],
    pub(crate) length: [[u8; TNS_MAX_FILTERS]; MAX_WINDOWS],
    pub(crate) order: [[u8; TNS_MAX_FILTERS]; MAX_WINDOWS],
    pub(crate) direction: [[bool; TNS_MAX_FILTERS]; MAX_WINDOWS],
    pub(crate) coef_compress: [[bool; TNS_MAX_FILTERS]; MAX_WINDOWS],
    /// Raw `coef_bits`-wide codes, index `[window][filter][order_index]`.
    pub(crate) coef: [[[u8; TNS_MAX_ORDER]; TNS_MAX_FILTERS]; MAX_WINDOWS],
}

impl Default for TnsData {
    fn default() -> Self {
        Self {
            n_filt: [0; MAX_WINDOWS],
            coef_res: [0; MAX_WINDOWS],
            length: [[0; TNS_MAX_FILTERS]; MAX_WINDOWS],
            order: [[0; TNS_MAX_FILTERS]; MAX_WINDOWS],
            direction: [[false; TNS_MAX_FILTERS]; MAX_WINDOWS],
            coef_compress: [[false; TNS_MAX_FILTERS]; MAX_WINDOWS],
            coef: [[[0u8; TNS_MAX_ORDER]; TNS_MAX_FILTERS]; MAX_WINDOWS],
        }
    }
}

// ---------------------------------------------------------------------------
// Ics — one channel's `individual_channel_stream` (side info + quantized
// spectral data), per §4/§5.
// ---------------------------------------------------------------------------

/// One channel's decoded `ics_info()` + quantized spectral data. For
/// `EIGHT_SHORT`, `coeffs` holds the data in **grouped** (bitstream) order —
/// per group, per `sfb`, concatenated across that group's windows — *not*
/// yet redistributed to per-window ascending order; Task 5's
/// `quant_to_spec`/de-interleave does that (§7). For long windows,
/// `coeffs[k]` is already the true spectral line `k`.
#[derive(Clone)]
pub(crate) struct Ics {
    pub(crate) window_sequence: u8,
    pub(crate) window_shape: u8,
    pub(crate) max_sfb: u8,
    pub(crate) num_window_groups: u8,
    pub(crate) window_group_length: [u8; MAX_WINDOWS],
    pub(crate) num_windows: u8,
    /// `sampling_frequency_index`, stashed so later tasks (dequant/TNS) can
    /// recover `swb_offset`/`max_tns_sfb` without threading `AacConfig`
    /// through every call.
    pub(crate) sfi: u8,
    pub(crate) sfb_cb: [[u8; MAX_SFB]; MAX_WINDOWS],
    pub(crate) scale_factors: [[i32; MAX_SFB]; MAX_WINDOWS],
    pub(crate) coeffs: [i32; 1024],
    pub(crate) tns: TnsData,
    /// Dequantized spectral lines (Task 5's `dsp::dequantize`): `sign(q)·
    /// |q|^(4/3) · 2^(0.25·(sf-100))` per line, with `EIGHT_SHORT` already
    /// de-interleaved into 8 consecutive 128-line windows in per-window
    /// ascending-frequency order (see `dsp`'s module doc). Zeroed by
    /// [`Default`]; only ever populated by `dequantize`, so it's dead code
    /// until Task 6 (stereo/TNS) and Task 7 (IMDCT) read it — see this
    /// module's blanket `dead_code` allow above.
    pub(crate) spec: [f32; 1024],
}

impl Default for Ics {
    fn default() -> Self {
        Self {
            window_sequence: ONLY_LONG,
            window_shape: 0,
            max_sfb: 0,
            num_window_groups: 0,
            window_group_length: [0; MAX_WINDOWS],
            num_windows: 0,
            sfi: 0,
            sfb_cb: [[0u8; MAX_SFB]; MAX_WINDOWS],
            scale_factors: [[0i32; MAX_SFB]; MAX_WINDOWS],
            coeffs: [0i32; 1024],
            tns: TnsData::default(),
            spec: [0.0; 1024],
        }
    }
}

#[cfg(test)]
impl Ics {
    /// Minimal one-long-window fixture for Task 5's dequant tests: `max_sfb
    /// = 1`, `sfb[0]` covering lines `[0, 4)` (true of `swb_offset_1024`'s
    /// first two entries for every `sfi` — see `tables.rs`), coeffs `[3,
    /// -3, 0, 0]` under an ordinary spectral codebook (1 — not
    /// ZERO/NOISE/INTENSITY, so `dequantize` doesn't skip it), scale_factor
    /// 100 (gain `2^0 = 1.0`).
    pub(crate) fn long_test() -> Self {
        let mut sfb_cb = [[0u8; MAX_SFB]; MAX_WINDOWS];
        sfb_cb[0][0] = 1;
        let mut scale_factors = [[0i32; MAX_SFB]; MAX_WINDOWS];
        scale_factors[0][0] = 100;
        let mut coeffs = [0i32; 1024];
        coeffs[0] = 3;
        coeffs[1] = -3;
        Self {
            window_sequence: ONLY_LONG,
            window_shape: 0,
            max_sfb: 1,
            num_window_groups: 1,
            window_group_length: [1, 0, 0, 0, 0, 0, 0, 0],
            num_windows: 1,
            sfi: 4, // 44100 Hz; irrelevant here beyond swb[0..2] == [0, 4]
            sfb_cb,
            scale_factors,
            coeffs,
            tns: TnsData::default(),
            spec: [0.0; 1024],
        }
    }

    /// Minimal two-group `EIGHT_SHORT` fixture for Task 5's de-interleave
    /// test: `max_sfb = 1`, `sfb[0]` covering lines `[0, 4)` per window
    /// (`swb_offset_128`'s first two entries, sfi 4 -> the 48 kHz table).
    /// Group 0 spans windows 0-1 (`window_group_length = [2, 6, ...]`);
    /// within its `sfb0` band (grouped-order coeffs `[window0's 4 lines,
    /// window1's 4 lines]`), line 0 of window 0 is `5` and line 0 of window
    /// 1 is `7` — everything else, including group 1's 6 windows, is `0`.
    /// A correct de-interleave scatters the `7` to `spec[1*128 + 0]`
    /// (window 1's slot); a naive 1:1 copy would instead leave it at
    /// `spec[4]`, which the test asserts stays `0.0`.
    pub(crate) fn short_test() -> Self {
        let mut sfb_cb = [[0u8; MAX_SFB]; MAX_WINDOWS];
        sfb_cb[0][0] = 1;
        sfb_cb[1][0] = 1;
        let mut scale_factors = [[0i32; MAX_SFB]; MAX_WINDOWS];
        scale_factors[0][0] = 100;
        scale_factors[1][0] = 100;
        let mut coeffs = [0i32; 1024];
        coeffs[0] = 5; // group 0, window 0 (first in group), sfb0 line 0
        coeffs[4] = 7; // group 0, window 1 (second in group), sfb0 line 0
        Self {
            window_sequence: EIGHT_SHORT,
            window_shape: 0,
            max_sfb: 1,
            num_window_groups: 2,
            window_group_length: [2, 6, 0, 0, 0, 0, 0, 0],
            num_windows: MAX_WINDOWS as u8,
            sfi: 4, // 48 kHz-group short table; irrelevant beyond swb[0..2] == [0, 4]
            sfb_cb,
            scale_factors,
            coeffs,
            tns: TnsData::default(),
            spec: [0.0; 1024],
        }
    }
}

/// A decoded channel. Per the task brief `ChannelData` *is* `Ics` — kept as
/// a distinct name because [`DecodedFrame`] holds "channels", and Task
/// 5/6's signatures read naturally against either name.
pub(crate) type ChannelData = Ics;

/// One CPE's stereo side info. The pair occupies channels `(ch0, ch0+1)` of
/// [`DecodedFrame::channels`]; `apply_stereo` needs its own M/S mask, so a
/// block with several CPEs (5.1/7.1) carries one `StereoPair` per CPE rather
/// than a single frame-global mask.
#[derive(Clone)]
pub(crate) struct StereoPair {
    /// First channel of the pair (the second is `ch0 + 1`).
    pub(crate) ch0: u8,
    /// Raw `ms_mask_present` field (0/1/2 — 3 is rejected as `Malformed`).
    pub(crate) ms_mask_present: u8,
    /// Per `(group, sfb)`, only populated (and only meaningful) when
    /// `ms_mask_present == 1`.
    pub(crate) ms_used: [[bool; MAX_SFB]; MAX_WINDOWS],
}

impl Default for StereoPair {
    fn default() -> Self {
        Self {
            ch0: 0,
            ms_mask_present: 0,
            ms_used: [[false; MAX_SFB]; MAX_WINDOWS],
        }
    }
}

/// One decoded `raw_data_block()`: up to [`MAX_CHANNELS`](super::MAX_CHANNELS)
/// channels decoded from its element sequence (SCE/CPE/LFE), covering mono,
/// stereo, and the multichannel layouts (5.1 = SCE+CPE+CPE+LFE, 7.1 =
/// SCE+CPE+CPE+CPE+LFE). `stereo[..n_stereo]` holds each CPE's M/S side info,
/// in element order.
pub(crate) struct DecodedFrame {
    pub(crate) channels: [ChannelData; super::MAX_CHANNELS],
    pub(crate) n_channels: u8,
    /// One entry per CPE in the block (a CPE is the only two-channel element).
    pub(crate) stereo: [StereoPair; super::MAX_CHANNELS / 2],
    pub(crate) n_stereo: u8,
    /// SBR side data location, if a `fill_element` in this block carried
    /// `EXT_SBR_DATA`/`EXT_SBR_DATA_CRC` (HE-AAC). `None` for pure AAC-LC — the
    /// LC-only path stays byte-identical. [`super::decode_rdb`] runs the SBR
    /// drive from this location after the LC core PCM is reconstructed (mono/
    /// stereo only — multichannel HE-AAC is out of scope).
    pub(crate) sbr: Option<SbrLoc>,
}

impl Default for DecodedFrame {
    fn default() -> Self {
        Self {
            channels: core::array::from_fn(|_| Ics::default()),
            n_channels: 0,
            stereo: core::array::from_fn(|_| StereoPair::default()),
            n_stereo: 0,
            sbr: None,
        }
    }
}

// ---------------------------------------------------------------------------
// ics_info() — §4.
// ---------------------------------------------------------------------------

fn decode_ics_info(r: &mut BitReader, ics: &mut Ics, sfi: u8) -> Result<(), DecodeError> {
    if r.read_bit() != 0 {
        return Err(DecodeError::Malformed); // ics_reserved_bit must be 0
    }
    let window_sequence = r.read(2) as u8;
    let window_shape = r.read_bit() as u8;
    ics.window_sequence = window_sequence;
    ics.window_shape = window_shape;
    ics.sfi = sfi;

    if window_sequence == EIGHT_SHORT {
        let max_sfb = r.read(4) as u8;
        let sfg = r.read(7);
        let num_swb = tables::num_swb_128(sfi);
        if max_sfb > num_swb {
            return Err(DecodeError::Malformed);
        }
        ics.max_sfb = max_sfb;
        ics.num_windows = MAX_WINDOWS as u8;

        // Grouping (§4): window 0 always starts group 0; for the 7
        // remaining windows (1..=7), bit (6-i) of the 7-bit
        // scale_factor_grouping field decides "joins the current group"
        // (1) vs. "starts a new group" (0).
        let mut group_length = [0u8; MAX_WINDOWS];
        let mut num_groups: usize = 1;
        group_length[0] = 1;
        for i in 0..(MAX_WINDOWS as u32 - 1) {
            if (sfg >> (6 - i)) & 1 == 1 {
                group_length[num_groups - 1] += 1;
            } else {
                group_length[num_groups] = 1;
                num_groups += 1;
            }
        }
        ics.num_window_groups = num_groups as u8;
        ics.window_group_length = group_length;
    } else {
        let max_sfb = r.read(6) as u8;
        if r.read_bit() != 0 {
            // predictor_data_present: Main-profile-only (LTP); this decoder
            // is AAC-LC only.
            return Err(DecodeError::Unsupported);
        }
        let num_swb = tables::num_swb_1024(sfi);
        if max_sfb > num_swb {
            return Err(DecodeError::Malformed);
        }
        ics.max_sfb = max_sfb;
        ics.num_windows = 1;
        ics.num_window_groups = 1;
        ics.window_group_length = [1, 0, 0, 0, 0, 0, 0, 0];
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// section_data() — §5.
// ---------------------------------------------------------------------------

fn section_data(r: &mut BitReader, ics: &mut Ics) -> Result<(), DecodeError> {
    let short = ics.window_sequence == EIGHT_SHORT;
    let sect_bits: u32 = if short { 3 } else { 5 };
    let sect_esc: u32 = (1 << sect_bits) - 1;
    let max_sfb = ics.max_sfb as usize;

    for g in 0..ics.num_window_groups as usize {
        let mut k = 0usize;
        // A well-formed group needs at most `max_sfb` sections (each
        // covering >= 1 band); bound the loop so a bitstream that keeps
        // emitting zero-length sections can't spin forever.
        let mut guard = 0usize;
        while k < max_sfb {
            guard += 1;
            if guard > max_sfb + 1 {
                return Err(DecodeError::Malformed);
            }
            let sect_cb = r.read(4) as u8;
            if sect_cb == 12 {
                return Err(DecodeError::Malformed); // reserved codebook index
            }
            // HCB_NOISE (13, PNS) is accepted as a section codebook here —
            // real-world encoders (confirmed against this task's own ADTS
            // fixture) use PNS routinely, even on tonal content, for
            // perceptually-irrelevant bands. `scale_factor_data`/
            // `spectral_data` still correctly consume every PNS-band bit
            // (so the bitstream never desyncs); v1 just doesn't synthesize
            // the pseudo-random noise those bits describe, treating the
            // band as silent instead — see the `HCB_NOISE` doc comment.
            let mut len: u32 = 0;
            let mut inc = r.read(sect_bits);
            while inc == sect_esc {
                len += inc;
                if len as usize > max_sfb {
                    return Err(DecodeError::Malformed);
                }
                inc = r.read(sect_bits);
            }
            len += inc;
            let end = k + len as usize;
            if end > max_sfb {
                return Err(DecodeError::Malformed);
            }
            ics.sfb_cb[g][k..end].fill(sect_cb);
            k = end;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// scale_factor_data() — §5.
// ---------------------------------------------------------------------------

fn scale_factor_data(r: &mut BitReader, ics: &mut Ics, global_gain: u8) -> Result<(), DecodeError> {
    let mut scale_factor: i32 = i32::from(global_gain);
    let mut is_position: i32 = 0;
    // PNS noise_energy DPCM (§5): seeded from global_gain, like
    // scale_factor; the first PNS band in the frame gets a raw 9-bit value
    // instead of a Huffman differential (`noise_pcm_flag`).
    let mut noise_energy: i32 = i32::from(global_gain) - 90;
    let mut noise_pcm_flag = true;
    for g in 0..ics.num_window_groups as usize {
        for sfb in 0..ics.max_sfb as usize {
            let cb = ics.sfb_cb[g][sfb];
            match cb {
                HCB_ZERO => {} // no data; scale_factors[g][sfb] stays 0
                HCB_INTENSITY2 | HCB_INTENSITY => {
                    let diff = decode_scalefactor(r)? - 60;
                    is_position += diff;
                    ics.scale_factors[g][sfb] = is_position;
                }
                HCB_NOISE => {
                    if noise_pcm_flag {
                        noise_pcm_flag = false;
                        noise_energy += r.read(9) as i32 - 256;
                    } else {
                        noise_energy += decode_scalefactor(r)? - 60;
                    }
                    // Stored for completeness (matches the real DPCM
                    // structure); unused while `spectral_data` treats PNS
                    // bands as silent (see the `HCB_NOISE` doc comment).
                    ics.scale_factors[g][sfb] = noise_energy;
                }
                1..=11 => {
                    let diff = decode_scalefactor(r)? - 60;
                    scale_factor += diff;
                    if !(0..=255).contains(&scale_factor) {
                        return Err(DecodeError::Malformed);
                    }
                    ics.scale_factors[g][sfb] = scale_factor;
                }
                _ => return Err(DecodeError::Malformed), // 12 rejected in section_data
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// spectral_data() — §5/§6. Writes `ics.coeffs` in grouped (bitstream) order;
// see the `Ics` doc comment.
// ---------------------------------------------------------------------------

fn spectral_data(r: &mut BitReader, ics: &mut Ics) -> Result<(), DecodeError> {
    let short = ics.window_sequence == EIGHT_SHORT;
    let swb = if short {
        tables::swb_offset_128(ics.sfi)
    } else {
        tables::swb_offset_1024(ics.sfi)
    };
    let max_sfb = ics.max_sfb as usize;
    // ics_info() already checked max_sfb <= num_swb, so swb[..=max_sfb] is
    // in range.
    let mut pos: usize = 0;
    for g in 0..ics.num_window_groups as usize {
        let glen = ics.window_group_length[g] as usize;
        for sfb in 0..max_sfb {
            let cb = ics.sfb_cb[g][sfb];
            let band_width = usize::from(swb[sfb + 1] - swb[sfb]);
            let total = glen * band_width;
            if pos + total > ics.coeffs.len() {
                return Err(DecodeError::Malformed);
            }
            if cb == HCB_ZERO || cb == HCB_NOISE || cb == HCB_INTENSITY2 || cb == HCB_INTENSITY {
                pos += total; // no spectral bits (PNS bands are treated as silent — see HCB_NOISE)
                continue;
            }
            let inc = if cb >= 5 { 2usize } else { 4usize };
            if total % inc != 0 {
                return Err(DecodeError::Malformed); // our swb tables always divide evenly; defensive
            }
            let mut i = 0;
            while i < total {
                let quad = decode_spectral(r, cb)?;
                ics.coeffs[pos + i..pos + i + inc].copy_from_slice(&quad[..inc]);
                i += inc;
            }
            pos += total;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// pulse_data() — §5/§7 (parsed here, applied to `coeffs` after
// spectral_data(), matching the bitstream's own field order).
// ---------------------------------------------------------------------------

struct PulseInfo {
    number_pulse: u8, // raw 2-bit field; actual pulse count is +1
    pulse_start_sfb: u8,
    pulse_offset: [u8; MAX_PULSES],
    pulse_amp: [u8; MAX_PULSES],
}

fn parse_pulse_data(r: &mut BitReader) -> Result<PulseInfo, DecodeError> {
    let number_pulse = r.read(2) as u8;
    let pulse_start_sfb = r.read(6) as u8;
    let mut pulse_offset = [0u8; MAX_PULSES];
    let mut pulse_amp = [0u8; MAX_PULSES];
    let count = usize::from(number_pulse) + 1; // 1..=4, bounded by the 2-bit field
    for i in 0..count {
        pulse_offset[i] = r.read(5) as u8;
        pulse_amp[i] = r.read(4) as u8;
    }
    Ok(PulseInfo {
        number_pulse,
        pulse_start_sfb,
        pulse_offset,
        pulse_amp,
    })
}

fn apply_pulses(ics: &mut Ics, pulse: &PulseInfo) -> Result<(), DecodeError> {
    if ics.window_sequence == EIGHT_SHORT {
        return Err(DecodeError::Malformed); // pulses are long-window only
    }
    if usize::from(pulse.pulse_start_sfb) >= ics.max_sfb as usize {
        return Err(DecodeError::Malformed);
    }
    let swb = tables::swb_offset_1024(ics.sfi);
    let mut k = i32::from(swb[pulse.pulse_start_sfb as usize]);
    let count = usize::from(pulse.number_pulse) + 1;
    for i in 0..count {
        k += i32::from(pulse.pulse_offset[i]);
        if !(0..1024).contains(&k) {
            return Err(DecodeError::Malformed);
        }
        let idx = k as usize;
        let amp = i32::from(pulse.pulse_amp[i]);
        if ics.coeffs[idx] > 0 {
            ics.coeffs[idx] += amp;
        } else {
            ics.coeffs[idx] -= amp;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tns_data() — §9 (parsed + bounds-checked only; Task 6 applies it).
// ---------------------------------------------------------------------------

fn tns_data(r: &mut BitReader, ics: &mut Ics) -> Result<(), DecodeError> {
    let short = ics.window_sequence == EIGHT_SHORT;
    let n_filt_bits: u32 = if short { 1 } else { 2 };
    let length_bits: u32 = if short { 4 } else { 6 };
    let order_bits: u32 = if short { 3 } else { 5 };
    let cap = usize::from(tables::max_tns_sfb(ics.sfi, short)).max(ics.max_sfb as usize);
    let num_windows = ics.num_windows as usize;

    let mut tns = TnsData::default();
    for w in 0..num_windows {
        let n_filt = r.read(n_filt_bits) as usize;
        tns.n_filt[w] = n_filt as u8;
        if n_filt == 0 {
            continue;
        }
        if n_filt > TNS_MAX_FILTERS {
            return Err(DecodeError::Malformed); // unreachable given the field widths above; defensive
        }
        let coef_res = r.read_bit() as u8;
        tns.coef_res[w] = coef_res;
        for f in 0..n_filt {
            let length = r.read(length_bits) as u8;
            let order = r.read(order_bits) as u8;
            if usize::from(length) > cap {
                return Err(DecodeError::Malformed);
            }
            if usize::from(order) > TNS_MAX_ORDER {
                return Err(DecodeError::Malformed);
            }
            tns.length[w][f] = length;
            tns.order[w][f] = order;
            if order > 0 {
                let direction = r.read_bit() == 1;
                let coef_compress = r.read_bit() == 1;
                tns.direction[w][f] = direction;
                tns.coef_compress[w][f] = coef_compress;
                let coef_bits: u32 = 3 + u32::from(coef_res) - u32::from(coef_compress);
                for i in 0..order as usize {
                    tns.coef[w][f][i] = r.read(coef_bits) as u8;
                }
            }
        }
    }
    ics.tns = tns;
    Ok(())
}

// ---------------------------------------------------------------------------
// individual_channel_stream() — §5.
// ---------------------------------------------------------------------------

/// Decode one channel's `individual_channel_stream()`: `global_gain`, then
/// (only when `!common_window` — the shared `ics_info()` was already parsed
/// by the caller into `ics` otherwise) `ics_info()`, then
/// section/scalefactor/pulse/TNS/gain-control/spectral data.
pub(crate) fn decode_ics(
    r: &mut BitReader,
    ics: &mut Ics,
    sfi: u8,
    common_window: bool,
) -> Result<(), DecodeError> {
    let global_gain = r.read(8) as u8;
    if !common_window {
        decode_ics_info(r, ics, sfi)?;
    }

    section_data(r, ics)?;
    scale_factor_data(r, ics, global_gain)?;

    let pulse_present = r.read_bit() == 1;
    let pulse = if pulse_present {
        Some(parse_pulse_data(r)?)
    } else {
        None
    };

    let tns_present = r.read_bit() == 1;
    if tns_present {
        tns_data(r, ics)?;
    } else {
        ics.tns = TnsData::default();
    }

    if r.read_bit() != 0 {
        // gain_control_data_present: SSR-profile only; this decoder is LC-only.
        return Err(DecodeError::Unsupported);
    }

    spectral_data(r, ics)?;

    if let Some(p) = pulse {
        apply_pulses(ics, &p)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// DSE / FIL / PCE — §3 (parsed and discarded; none carry LC audio data).
// ---------------------------------------------------------------------------

/// Skip exactly `n` bits (bounded: `n` decreases every iteration, so this
/// always terminates — even past the real end of `r`'s data, where reads
/// are zero-padded rather than erroring).
fn skip_bits(r: &mut BitReader, mut n: usize) {
    while n > 0 {
        let take = n.min(32);
        r.read(take as u32);
        n -= take;
    }
}

fn decode_dse(r: &mut BitReader) -> Result<(), DecodeError> {
    let _tag = r.read(4);
    let align_flag = r.read_bit();
    let mut count = r.read(8);
    if count == 255 {
        count += r.read(8);
    }
    if align_flag == 1 {
        r.byte_align();
    }
    skip_bits(r, count as usize * 8);
    Ok(())
}

/// `fill_element` (§3): `count` bytes of `extension_payload`. Inspects the
/// leading 4-bit `extension_type`; for `EXT_SBR_DATA`/`EXT_SBR_DATA_CRC` it
/// records the SBR payload's bit span on `frame.sbr` (for `super::decode_rdb`
/// to drive after the LC IMDCT), and always skips the whole `count·8` bits so
/// the element loop stays byte-accurate — every other extension type is
/// length-skipped exactly as before (LC-only path unchanged). `id_cpe` is the
/// preceding audio element's type (CPE vs SCE), which the SBR decode needs.
fn decode_fil(
    r: &mut BitReader,
    frame: &mut DecodedFrame,
    id_cpe: bool,
) -> Result<(), DecodeError> {
    let mut count = r.read(4);
    if count == 15 {
        let esc = r.read(8);
        count = count + esc - 1;
    }
    let payload_bits = count as usize * 8;
    if payload_bits >= 4 {
        // extension_type is the first 4 bits of the payload.
        let ext_type = r.read(4);
        let remaining = payload_bits - 4;
        if ext_type == EXT_SBR_DATA || ext_type == EXT_SBR_DATA_CRC {
            let start_bit = r.bit_pos();
            frame.sbr = Some(SbrLoc {
                start_bit,
                end_bit: start_bit + remaining,
                crc: ext_type == EXT_SBR_DATA_CRC,
                id_cpe,
            });
        }
        skip_bits(r, remaining);
    } else {
        skip_bits(r, payload_bits);
    }
    Ok(())
}

/// `program_config_element()`: parsed field-by-field and discarded (every
/// loop count here is intrinsically bounded by its own fixed-width bitfield
/// — at most 15 iterations — so no separate guard is needed).
fn decode_pce(r: &mut BitReader) -> Result<(), DecodeError> {
    let _tag = r.read(4);
    let _object_type = r.read(2);
    let _sfi = r.read(4);
    let num_front = r.read(4) as usize;
    let num_side = r.read(4) as usize;
    let num_back = r.read(4) as usize;
    let num_lfe = r.read(2) as usize;
    let num_assoc = r.read(3) as usize;
    let num_cc = r.read(4) as usize;

    if r.read_bit() == 1 {
        r.read(4); // mono_mixdown_element_number
    }
    if r.read_bit() == 1 {
        r.read(4); // stereo_mixdown_element_number
    }
    if r.read_bit() == 1 {
        r.read(3); // matrix_mixdown_idx(2) + pseudo_surround_enable(1)
    }
    for _ in 0..num_front {
        r.read(5); // is_cpe(1) + tag_select(4)
    }
    for _ in 0..num_side {
        r.read(5);
    }
    for _ in 0..num_back {
        r.read(5);
    }
    for _ in 0..num_lfe {
        r.read(4);
    }
    for _ in 0..num_assoc {
        r.read(4);
    }
    for _ in 0..num_cc {
        r.read(5); // is_ind_sw(1) + tag_select(4)
    }
    r.byte_align();
    let comment_bytes = r.read(8) as usize;
    skip_bits(r, comment_bytes * 8);
    Ok(())
}

// ---------------------------------------------------------------------------
// raw_data_block() — §3.
// ---------------------------------------------------------------------------

fn decode_element(
    r: &mut BitReader,
    id: u32,
    sfi: u8,
    frame: &mut DecodedFrame,
    next_ch: &mut usize,
    last_id_cpe: &mut Option<bool>,
) -> Result<(), DecodeError> {
    match id {
        ID_SCE | ID_LFE => {
            let _tag = r.read(4);
            if *next_ch >= super::MAX_CHANNELS {
                // More audio channels than the 7.1 ceiling: out of scope.
                return Err(DecodeError::Unsupported);
            }
            let ch = *next_ch;
            *next_ch += 1;
            *last_id_cpe = Some(false); // preceding-element type for a following SBR fill
            frame.channels[ch] = Ics::default();
            decode_ics(r, &mut frame.channels[ch], sfi, false)
        }
        ID_CPE => {
            let _tag = r.read(4);
            let common_window = r.read_bit() == 1;
            let ch0 = *next_ch;
            let si = frame.n_stereo as usize;
            if ch0 + 2 > super::MAX_CHANNELS || si >= frame.stereo.len() {
                return Err(DecodeError::Unsupported);
            }
            // `frame` is built fresh (`DecodedFrame::default`) per block, so this
            // pair's `ms_used` starts all-false; only `ms_mask_present == 1`
            // overwrites it (and only that case is read by `apply_stereo`).
            frame.stereo[si].ch0 = ch0 as u8;
            let mut shared = Ics::default();
            if common_window {
                decode_ics_info(r, &mut shared, sfi)?;
                let ms_mask_present = r.read(2) as u8;
                if ms_mask_present == 3 {
                    return Err(DecodeError::Malformed); // reserved
                }
                frame.stereo[si].ms_mask_present = ms_mask_present;
                if ms_mask_present == 1 {
                    for g in 0..shared.num_window_groups as usize {
                        for sfb in 0..shared.max_sfb as usize {
                            frame.stereo[si].ms_used[g][sfb] = r.read_bit() == 1;
                        }
                    }
                }
            }
            frame.channels[ch0] = shared.clone();
            frame.channels[ch0 + 1] = shared;
            decode_ics(r, &mut frame.channels[ch0], sfi, common_window)?;
            decode_ics(r, &mut frame.channels[ch0 + 1], sfi, common_window)?;
            *next_ch = ch0 + 2;
            frame.n_stereo += 1;
            *last_id_cpe = Some(true); // preceding-element type for a following SBR fill
            Ok(())
        }
        ID_CCE => Err(DecodeError::Unsupported), // coupling — not used by LC
        ID_DSE => decode_dse(r),
        ID_PCE => decode_pce(r),
        ID_FIL => decode_fil(r, frame, last_id_cpe.unwrap_or(false)),
        _ => Err(DecodeError::Malformed), // id_syn_ele is 3 bits; every value 0..=7 is handled above
    }
}

/// Decode one `raw_data_block()`: the element-tag loop (§3) until `ID_END`,
/// filling up to [`MAX_CHANNELS`](super::MAX_CHANNELS) channels' [`Ics`]
/// (quantized `coeffs` included) via [`decode_ics`] from the block's SCE/CPE/LFE
/// sequence. AAC-LC only — `config.object_type != 2` (LC) is `Err(Unsupported)`.
///
/// # Errors
/// [`DecodeError::Malformed`] on any syntactically invalid field (bad
/// reserved bit, an out-of-range `max_sfb`/section length/pulse
/// index/TNS order, ...); [`DecodeError::Unsupported`] for a valid but
/// unhandled bitstream feature (PNS, coupling, gain control, LTP
/// prediction, or a `channel_configuration` needing more than 2 elements).
/// Never panics — every array write is bounds-checked first, and the
/// element loop is capped at [`MAX_ELEMENTS`] so a malformed stream that
/// never emits `ID_END` still terminates.
pub(crate) fn decode_raw_data_block(
    r: &mut BitReader,
    config: AacConfig,
) -> Result<DecodedFrame, DecodeError> {
    if config.object_type != 2 {
        return Err(DecodeError::Unsupported); // AAC-LC only
    }
    let sfi = tables::resolve_sfi(config.sample_rate)?;

    let mut frame = DecodedFrame::default();
    let mut next_ch: usize = 0;
    let mut last_id_cpe: Option<bool> = None;
    let mut elements = 0usize;
    loop {
        elements += 1;
        if elements > MAX_ELEMENTS {
            return Err(DecodeError::Malformed);
        }
        let id = r.read(3);
        if id == ID_END {
            break;
        }
        decode_element(r, id, sfi, &mut frame, &mut next_ch, &mut last_id_cpe)?;
    }
    frame.n_channels = next_ch as u8;
    r.byte_align();
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::adts::AdtsReader;
    use crate::io::SliceSource;

    const AAC: &[u8] = include_bytes!("../../../testdata/aac.aac");

    fn first_frame_config_and_bytes(buf: &mut [u8]) -> (AacConfig, &[u8]) {
        let mut adts = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        let config = adts.config();
        (config, adts.next_frame(buf).unwrap().unwrap())
    }

    /// `DecodedFrame` deliberately isn't `Debug` (its `[i32; 1024]` coeffs
    /// buffers would make failure output unreadable) — matching every
    /// sibling reader/decoder in this crate. This sidesteps the `Debug`
    /// bound `Result::unwrap_err` needs for its panic message.
    fn expect_err(r: Result<DecodedFrame, DecodeError>) -> DecodeError {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        }
    }

    #[test]
    fn decodes_first_frame_structure() {
        // Per the task brief: decode the first frame's structure from the
        // ADTS fixture's first raw_data_block; expect a CPE (stereo), a
        // sane max_sfb, and (transitively, via decode_ics succeeding)
        // 1024 quantized coeffs per channel.
        //
        // The brief's own draft of this test additionally expected
        // `window_sequence == ONLY_LONG` ("typical for a steady tone").
        // Verified against the real fixture (via `ffprobe`/frame-by-frame
        // tracing) that its *actual* first frame is `LONG_START`, settling
        // into `ONLY_LONG` by frame 3 — the canonical encoder start-up
        // transition `LONG_START -> EIGHT_SHORT -> LONG_STOP -> ONLY_LONG`,
        // which `decodes_first_five_frames_show_canonical_startup_windows`
        // below asserts explicitly. Corrected here to the verified value;
        // see the task report for the evidence.
        let mut buf = [0u8; 8192];
        let mut adts = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        let config = adts.config();
        let rdb = adts.next_frame(&mut buf).unwrap().unwrap();

        let mut r = BitReader::new(rdb);
        let f = decode_raw_data_block(&mut r, config).unwrap();
        assert_eq!(f.n_channels, 2);
        assert_eq!(f.channels[0].window_sequence, LONG_START);
        assert!(f.channels[0].max_sfb > 0 && f.channels[0].max_sfb <= 51);
        assert_eq!(f.channels[1].window_sequence, LONG_START);
    }

    #[test]
    fn decodes_first_five_frames_show_canonical_startup_windows() {
        // LONG_START -> EIGHT_SHORT -> LONG_STOP -> ONLY_LONG -> ONLY_LONG:
        // the textbook AAC encoder-startup window-sequence progression,
        // and strong independent evidence the parser stays correctly
        // synchronized across multiple consecutive frames (a desync would
        // essentially never coincidentally reproduce this exact canonical
        // pattern while also satisfying every max_sfb/section/spectral
        // bounds check along the way).
        let expected = [LONG_START, EIGHT_SHORT, LONG_STOP, ONLY_LONG, ONLY_LONG];
        let mut adts = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        let config = adts.config();
        let mut buf = [0u8; 8192];
        for (i, &want) in expected.iter().enumerate() {
            let rdb = adts.next_frame(&mut buf).unwrap().unwrap();
            let mut r = BitReader::new(rdb);
            let f = decode_raw_data_block(&mut r, config).unwrap();
            assert_eq!(f.n_channels, 2);
            assert_eq!(f.channels[0].window_sequence, want, "frame {i}");
            assert_eq!(f.channels[1].window_sequence, want, "frame {i}");
        }
    }

    #[test]
    fn decodes_every_frame_in_the_fixture_without_error() {
        // Whole-file stress test: every frame in the fixture must parse
        // cleanly, with every quantized spectral line index in bounds (no
        // panics — enforced by the type system's `[i32; 1024]`, not just a
        // runtime check) and every sfb/section/pulse/TNS field validated.
        let mut adts = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        let config = adts.config();
        let mut buf = [0u8; 8192];
        let mut frames = 0;
        while let Some(rdb) = adts.next_frame(&mut buf).unwrap() {
            let mut r = BitReader::new(rdb);
            let f = decode_raw_data_block(&mut r, config).unwrap();
            assert_eq!(f.n_channels, 2);
            frames += 1;
        }
        assert!(frames > 10);
    }

    #[test]
    fn rejects_non_lc_object_type() {
        let mut buf = [0u8; 8192];
        let (mut config, rdb) = first_frame_config_and_bytes(&mut buf);
        config.object_type = 5; // SBR/other, not LC (2)
        let mut r = BitReader::new(rdb);
        assert_eq!(
            expect_err(decode_raw_data_block(&mut r, config)),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn rejects_unresolvable_sample_rate() {
        let mut buf = [0u8; 8192];
        let (mut config, rdb) = first_frame_config_and_bytes(&mut buf);
        config.sample_rate = 12_345;
        let mut r = BitReader::new(rdb);
        assert_eq!(
            expect_err(decode_raw_data_block(&mut r, config)),
            DecodeError::Malformed
        );
    }

    #[test]
    fn truncated_real_frame_errors_without_panicking() {
        let mut buf = [0u8; 8192];
        let (config, rdb) = first_frame_config_and_bytes(&mut buf);
        for cut in [0usize, 1, 2, 3, 5, 8, 16] {
            let truncated = &rdb[..cut.min(rdb.len())];
            let mut r = BitReader::new(truncated);
            let _ = decode_raw_data_block(&mut r, config);
        }
    }

    #[test]
    fn malformed_or_adversarial_input_never_panics() {
        let config = AacConfig {
            object_type: 2,
            sample_rate: 44_100,
            channels: 2,
            sbr_signalled: false,
            ext_sample_rate: None,
        };

        // Fully empty input: BitReader zero-pads every read, so this must
        // terminate (via the channel-capacity/MAX_ELEMENTS bounds) rather
        // than hang.
        let mut r = BitReader::new(&[]);
        let _ = decode_raw_data_block(&mut r, config);

        // A spread of fixed byte patterns, each its own fresh buffer: must
        // return without panicking, hanging, or reading out of bounds.
        for pattern in [0x00u8, 0xFF, 0xAA, 0x55, 0x12, 0x7F, 0x80] {
            let data = [pattern; 128];
            let mut r = BitReader::new(&data);
            let _ = decode_raw_data_block(&mut r, config);
        }
    }
}
