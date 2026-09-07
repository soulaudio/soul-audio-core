//! CELT band-energy decode — coarse (Laplace-coded, 2-D predicted), fine, and
//! finalise passes — plus the per-frame preamble that positions the range
//! coder for them (RFC 6716 §4.3.1–4.3.3).
//!
//! Transcribed from libopus **v1.5.2** `celt/laplace.c` (`ec_laplace_decode` /
//! `ec_laplace_get_freq1`), `celt/quant_bands.c` (`unquant_coarse_energy` /
//! `unquant_fine_energy` / `unquant_energy_finalise`), and the
//! `celt_decode_with_ec` preamble in `celt/celt_decoder.c` (silence /
//! post-filter / transient / intra reads). This crate builds in **float**
//! mode, so every `SHL*/SHR*/PSHR*/QCONST*/EXTEND32` macro in the libopus
//! fixed-point flavour collapses to the identity / the plain float constant;
//! the transcriptions below already fold those away (see each function's
//! notes), which is why the energy update is pure `f32` arithmetic with no
//! shifts.
//!
//! # What lands here (Task 5) vs. later
//!
//! Coarse energy is decoded end-to-end (the entropy symbols do **not** depend
//! on the prediction, so the reconstructed log2-energy envelope is produced in
//! full). [`unquant_fine_energy`] / [`unquant_energy_finalise`] take the
//! per-band bit counts (`fine_quant` / `fine_priority` / `bits_left`) as
//! parameters because those come from the bit allocation (Task 6/8); they are
//! implemented now and unit-tested against synthetic inputs. The post-filter
//! parameters are *parsed* here (skipping their bits would desync the coder)
//! but their **application** is deferred to Task 8 — [`read_preamble`] returns
//! them for the decoder driver to stash.
//!
//! Energies live in the log2 domain **relative to the band means** (`eMeans`
//! is added only at denormalization time, Task 8), in a caller-owned
//! `[f32; NUM_BANDS * 2]` indexed `i + c * NUM_BANDS`. The array persists
//! across frames: inter-frame prediction reads the previous frame's values.

use crate::formats::opus::opus_tables::{
    BETA_COEF, BETA_INTRA, E_PROB_MODEL, LAPLACE_LOG_MINP, LAPLACE_MINP, LAPLACE_NMIN, NUM_BANDS,
    PRED_COEF, SMALL_ENERGY_ICDF,
};
use crate::formats::opus::range::RangeDecoder;
use crate::formats::opus::toc::Bandwidth;

/// Maximum fine-energy bits a band may carry (libopus `quant_bands.c`
/// `MAX_FINE_BITS`). The finalise pass never adds a bit to a band already at
/// this resolution.
pub(crate) const MAX_FINE_BITS: i32 = 8;

/// Post-filter tapset inverse-CDF (libopus `celt_decoder.c` `tapset_icdf`).
static TAPSET_ICDF: [u8; 3] = [2, 1, 0];

/// Post-filter parameters parsed from a CELT frame's preamble. The filter
/// itself is applied in a later task (Task 8); these are carried so parsing
/// them here — required to keep the range coder in sync — is not lost work.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PostFilterParams {
    /// Pitch period in samples, `(16 << octave) + raw - 1`.
    pub period: i32,
    /// Comb-filter gain, `0.09375 * (qg + 1)`.
    pub gain: f32,
    /// Tap set index (0–2) selecting the filter's tap weights.
    pub tapset: u32,
}

/// The outcome of the CELT per-frame preamble (RFC 6716 §4.3.1). Every field
/// corresponds to a gated read that *must* run — even when unused here — to
/// leave the range coder positioned for energy decode.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CeltPreamble {
    /// Whole frame is silent: energies are forced to the minimum and the coder
    /// is fast-forwarded past the (empty) remainder.
    pub silence: bool,
    /// Post-filter parameters, if the post-filter flag was set (Task 8).
    pub post_filter: Option<PostFilterParams>,
    /// Transient frame (short MDCT blocks); only readable when `LM > 0`.
    pub is_transient: bool,
    /// Intra energy: coarse energy uses no inter-frame prediction this frame.
    pub intra_ener: bool,
}

/// `LM = log2(number of MDCT sub-blocks)` for a CELT frame duration expressed
/// as a 48 kHz sample count (2.5/5/10/20 ms → 120/240/480/960 → 0/1/2/3).
pub(crate) fn celt_lm(frame_size_48k: u16) -> u32 {
    match frame_size_48k {
        120 => 0,
        240 => 1,
        480 => 2,
        _ => 3, // 960 (20 ms)
    }
}

/// The CELT coding end band for a TOC bandwidth (libopus `opus_decoder.c`
/// end-band map). `start` is always 0 for a CELT-only frame; `end` bounds the
/// per-band energy/allocation loops.
pub(crate) fn celt_end_band(bandwidth: Bandwidth) -> usize {
    match bandwidth {
        Bandwidth::Nb => 13,
        Bandwidth::Mb | Bandwidth::Wb => 17,
        Bandwidth::Swb => 19,
        Bandwidth::Fb => 21,
    }
}

/// `ec_laplace_get_freq1` (libopus `celt/laplace.c`): the frequency of the
/// first decaying step of the Laplace PDF. `decay` is positive and at most
/// 11456, so the `i32` product cannot overflow.
fn ec_laplace_get_freq1(fs0: u32, decay: i32) -> u32 {
    // ft = 32768 - LAPLACE_MINP*(2*LAPLACE_NMIN) - fs0.
    let ft = 32768 - LAPLACE_MINP * (2 * LAPLACE_NMIN) - fs0;
    // return ft*(16384-decay) >> 15.
    ((ft as i32 * (16384 - decay)) >> 15) as u32
}

/// `ec_laplace_decode(dec, fs, decay)` (libopus `celt/laplace.c`): decode one
/// signed value against a two-sided geometric ("Laplace") PDF whose head
/// frequency is `fs` and whose tail decays by `decay/32768` per step. Uses the
/// 15-bit binary entropy read (`ec_decode_bin(dec, 15)`, bit-identical to
/// [`RangeDecoder::decode`]`(1 << 15)` — division by a power of two equals the
/// shift), walks the decaying part, then finalises with `dec_update`.
fn ec_laplace_decode(rd: &mut RangeDecoder, mut fs: u32, decay: i32) -> i32 {
    let mut val: i32 = 0;
    let mut fl: u32 = 0;
    let fm: u32 = rd.decode(32768);
    if fm >= fs {
        val += 1;
        fl = fs;
        fs = ec_laplace_get_freq1(fs, decay) + LAPLACE_MINP;
        // Search the decaying part of the PDF.
        while fs > LAPLACE_MINP && fm >= fl + 2 * fs {
            fs *= 2;
            fl += fs;
            fs = (((fs - 2 * LAPLACE_MINP) as i32 * decay) >> 15) as u32;
            fs += LAPLACE_MINP;
            val += 1;
        }
        // Everything beyond that has probability LAPLACE_MINP.
        if fs <= LAPLACE_MINP {
            let di = ((fm - fl) >> (LAPLACE_LOG_MINP + 1)) as i32;
            val += di;
            fl += (2 * di) as u32 * LAPLACE_MINP;
        }
        if fm < fl + fs {
            val = -val;
        } else {
            fl += fs;
        }
    }
    rd.dec_update(fl, core::cmp::min(fl + fs, 32768), 32768);
    val
}

/// Read the CELT per-frame preamble (RFC 6716 §4.3.1) in the exact libopus
/// order: silence → post-filter → transient → intra. Each read is gated on the
/// running bit budget precisely as `celt_decode_with_ec` does; the `tell`
/// local threads through those gates just as the C source's does (the
/// post-filter/transient reads re-sync it via `ec_tell` after consuming bits).
///
/// `start` is the CELT start band (0 for a normal frame); `lm` is
/// `log2(MDCT blocks)`.
pub(crate) fn read_preamble(rd: &mut RangeDecoder, start: usize, lm: u32) -> CeltPreamble {
    let total_bits = (rd.storage() as u32) * 8;
    let mut tell = rd.tell();

    // Silence flag (only present when exactly one bit has been consumed; a
    // full/over-full budget means implicit silence).
    let silence = if tell >= total_bits {
        true
    } else if tell == 1 {
        rd.decode_bit_logp(15) != 0
    } else {
        false
    };
    if silence {
        // Pretend we've read all the remaining bits.
        tell = total_bits;
        rd.skip_to_end(total_bits);
    }

    // Post-filter (start band 0 only). Its bits are consumed here; the filter
    // is applied in Task 8.
    let mut post_filter = None;
    if start == 0 && tell + 16 <= total_bits {
        if rd.decode_bit_logp(1) != 0 {
            let octave = rd.dec_uint(6);
            let period = (16i32 << octave) + rd.dec_bits(4 + octave) as i32 - 1;
            let qg = rd.dec_bits(3);
            let tapset = if rd.tell() + 2 <= total_bits {
                rd.decode_icdf(&TAPSET_ICDF, 2)
            } else {
                0
            };
            let gain = 0.09375f32 * (qg + 1) as f32;
            post_filter = Some(PostFilterParams {
                period,
                gain,
                tapset,
            });
        }
        tell = rd.tell();
    }

    // Transient flag (only for LM > 0 frames).
    let is_transient = if lm > 0 && tell + 3 <= total_bits {
        let t = rd.decode_bit_logp(3) != 0;
        tell = rd.tell();
        t
    } else {
        false
    };

    // Intra-energy flag (disables inter-frame prediction in coarse energy).
    let intra_ener = tell + 3 <= total_bits && rd.decode_bit_logp(3) != 0;

    CeltPreamble {
        silence,
        post_filter,
        is_transient,
        intra_ener,
    }
}

/// `unquant_coarse_energy` (libopus `celt/quant_bands.c`): decode the coarse
/// log2-energy envelope for bands `start..end`, each of `channels` channels,
/// into `old_ebands` (indexed `i + c * NUM_BANDS`).
///
/// Coarse energy is predicted in two dimensions: from the previous band in the
/// same frame (via `prev`, an accumulator reset per channel) and from the same
/// band in the previous frame (via `coef * old_ebands[..]`, the persisted
/// array — zero on an intra frame). The delta `qi` is entropy-coded three ways
/// depending on how much budget remains: the full Laplace model, a 3-symbol
/// `small_energy_icdf` fallback (with a signed unpack), a single sign bit, or
/// the implicit `-1`.
///
/// Float-mode notes: `q = qi` (the `SHL32(EXTEND32(qi), DB_SHIFT)` is the
/// identity in float); the previous value is clamped to `>= -9.0` before
/// prediction (`MAX16(-QCONST16(9,DB_SHIFT), ..)`); the fixed-point lower
/// clamp on `tmp` (`MAX32(-28, ..)`) exists only under `FIXED_POINT` and is
/// intentionally absent here; and `prev[c] += q - beta*q` (both `SHL32(q,7)`
/// and `PSHR32(q,8)` are identities in float).
///
/// The `budget` is `storage * 8` and the `budget - tell` comparisons are
/// **signed** (libopus `opus_int32`), so an exhausted/over-read budget (e.g.
/// after a silence fast-forward) correctly falls through to `qi = -1`.
pub(crate) fn unquant_coarse_energy(
    rd: &mut RangeDecoder,
    start: usize,
    end: usize,
    channels: usize,
    lm: u32,
    intra: bool,
    old_ebands: &mut [f32],
) {
    let prob_model = &E_PROB_MODEL[lm as usize][usize::from(intra)];
    let (coef, beta) = if intra {
        (0.0f32, BETA_INTRA)
    } else {
        (PRED_COEF[lm as usize], BETA_COEF[lm as usize])
    };
    let budget = (rd.storage() as i32) * 8;
    let mut prev = [0.0f32; 2];

    for i in start..end {
        // `c` also drives the `old_ebands` channel offset, so iterate `prev`
        // by mutable reference while keeping the index in hand.
        for (c, prev_c) in prev.iter_mut().enumerate().take(channels) {
            let avail = budget - rd.tell() as i32;
            let qi: i32 = if avail >= 15 {
                let pi = 2 * core::cmp::min(i, 20);
                ec_laplace_decode(
                    rd,
                    u32::from(prob_model[pi]) << 7,
                    i32::from(prob_model[pi + 1]) << 6,
                )
            } else if avail >= 2 {
                let qi = rd.decode_icdf(&SMALL_ENERGY_ICDF, 2) as i32;
                (qi >> 1) ^ -(qi & 1)
            } else if avail >= 1 {
                -(rd.decode_bit_logp(1) as i32)
            } else {
                -1
            };
            let q = qi as f32;
            let idx = i + c * NUM_BANDS;
            // Clamp the previous frame's value first (MAX16(-9, ..)), then
            // apply inter-band + inter-frame prediction. Evaluation order is
            // kept left-to-right to match libopus's float rounding exactly.
            let clamped = old_ebands[idx].max(-9.0);
            old_ebands[idx] = coef * clamped + *prev_c + q;
            *prev_c = *prev_c + q - beta * q;
        }
    }
}

/// Fine-energy offset for a `fine_quant`-bit code word `q2`
/// (`unquant_fine_energy`, float branch): a mid-tread dequantizer step
/// `(q2 + 0.5) / 2^fine_quant - 0.5`, written in the exact libopus form so the
/// float rounding is bit-identical.
fn fine_energy_offset(q2: u32, fine_quant: i32) -> f32 {
    (q2 as f32 + 0.5) * (1u32 << (14 - fine_quant)) as f32 * (1.0 / 16384.0) - 0.5
}

/// `unquant_fine_energy` (libopus `celt/quant_bands.c`): refine the coarse
/// envelope with `fine_quant[i]` raw bits per band/channel, adding the
/// mid-tread offset. Bands with no fine bits are skipped.
pub(crate) fn unquant_fine_energy(
    rd: &mut RangeDecoder,
    start: usize,
    end: usize,
    channels: usize,
    fine_quant: &[i32],
    old_ebands: &mut [f32],
) {
    for i in start..end {
        if fine_quant[i] <= 0 {
            continue;
        }
        for c in 0..channels {
            let q2 = rd.dec_bits(fine_quant[i] as u32);
            old_ebands[i + c * NUM_BANDS] += fine_energy_offset(q2, fine_quant[i]);
        }
    }
}

/// Finalise-pass offset for a single bit `q2` at band resolution `fine_quant`
/// (`unquant_energy_finalise`, float branch): `(q2 - 0.5) / 2^(fine_quant+1)`
/// — half a fine step, in the exact libopus float form.
fn finalise_energy_offset(q2: u32, fine_quant: i32) -> f32 {
    (q2 as f32 - 0.5) * (1u32 << (14 - fine_quant - 1)) as f32 * (1.0 / 16384.0)
}

/// `unquant_energy_finalise` (libopus `celt/quant_bands.c`): spend any bits
/// left over after the main allocation on one extra fine bit per band, in two
/// priority passes (priority-0 bands first, then priority-1), stopping when
/// fewer than `channels` bits remain. Bands already at [`MAX_FINE_BITS`] are
/// skipped.
// Mirrors libopus's fixed 9-parameter `unquant_energy_finalise` signature
// (allocation arrays + band range + budget); grouping them would obscure the
// 1:1 transcription.
#[allow(clippy::too_many_arguments)]
pub(crate) fn unquant_energy_finalise(
    rd: &mut RangeDecoder,
    start: usize,
    end: usize,
    channels: usize,
    fine_quant: &[i32],
    fine_priority: &[i32],
    mut bits_left: i32,
    old_ebands: &mut [f32],
) {
    for prio in 0..2 {
        let mut i = start;
        while i < end && bits_left >= channels as i32 {
            if fine_quant[i] >= MAX_FINE_BITS || fine_priority[i] != prio {
                i += 1;
                continue;
            }
            for c in 0..channels {
                let q2 = rd.dec_bits(1);
                old_ebands[i + c * NUM_BANDS] += finalise_energy_offset(q2, fine_quant[i]);
                bits_left -= 1;
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::ogg_packets::OggPackets;
    use crate::formats::opus::toc::{self, Mode};
    use crate::io::SliceSource;

    // ================================================================
    // Independently-transcribed Laplace ENCODER (test-only).
    //
    // Transcribed from a DIFFERENT libopus file (`celt/laplace.c`
    // `ec_laplace_encode` + `celt/entenc.c`'s arithmetic encoder) than the
    // decoder under test. The encoder writes arithmetic bytes FORWARD from the
    // front; the decoder reads them forward too, but the two transcriptions are
    // independent, so a compensating bug would have to appear in both. Laplace
    // coding uses only `ec_encode_bin` (no raw bits), so this minimal encoder
    // omits the backward raw-bit machinery entirely.
    // ================================================================

    const EC_CODE_TOP: u32 = 0x8000_0000;
    const EC_CODE_BOT: u32 = 0x0080_0000;
    const EC_CODE_SHIFT: u32 = 23;

    fn ilog(x: u32) -> u32 {
        32 - x.leading_zeros()
    }

    struct MiniEnc {
        buf: Vec<u8>,
        rng: u32,
        val: u32,
        ext: u32,
        rem: i32,
        nbits_total: u32,
    }

    impl MiniEnc {
        fn new() -> Self {
            Self {
                buf: Vec::new(),
                rng: EC_CODE_TOP,
                val: 0,
                ext: 0,
                rem: -1,
                nbits_total: 33,
            }
        }

        fn write_byte(&mut self, value: u32) {
            self.buf.push((value & 0xFF) as u8);
        }

        fn carry_out(&mut self, c: i32) {
            if c as u32 != 0xFF {
                let carry = c >> 8;
                if self.rem >= 0 {
                    self.write_byte((self.rem + carry) as u32);
                }
                if self.ext > 0 {
                    let sym = 0xFFu32.wrapping_add(carry as u32) & 0xFF;
                    loop {
                        self.write_byte(sym);
                        self.ext -= 1;
                        if self.ext == 0 {
                            break;
                        }
                    }
                }
                self.rem = (c as u32 & 0xFF) as i32;
            } else {
                self.ext += 1;
            }
        }

        fn normalize(&mut self) {
            while self.rng <= EC_CODE_BOT {
                self.carry_out((self.val >> EC_CODE_SHIFT) as i32);
                self.val = self.val.wrapping_shl(8) & (EC_CODE_TOP - 1);
                self.rng <<= 8;
                self.nbits_total += 8;
            }
        }

        fn encode(&mut self, fl: u32, fh: u32, ft: u32) {
            let r = self.rng / ft;
            if fl > 0 {
                self.val = self.val.wrapping_add(self.rng - r.wrapping_mul(ft - fl));
                self.rng = r.wrapping_mul(fh - fl);
            } else {
                self.rng -= r.wrapping_mul(ft - fh);
            }
            self.normalize();
        }

        fn encode_bin(&mut self, fl: u32, fh: u32) {
            self.encode(fl, fh, 32768);
        }

        fn done(&mut self) {
            let mut l = (32 - ilog(self.rng)) as i32;
            let mut msk = (EC_CODE_TOP - 1) >> l;
            let mut end = self.val.wrapping_add(msk) & !msk;
            if (end | msk) >= self.val.wrapping_add(self.rng) {
                l += 1;
                msk >>= 1;
                end = self.val.wrapping_add(msk) & !msk;
            }
            while l > 0 {
                self.carry_out((end >> EC_CODE_SHIFT) as i32);
                end = end.wrapping_shl(8) & (EC_CODE_TOP - 1);
                l -= 8;
            }
            if self.rem >= 0 || self.ext > 0 {
                self.carry_out(0);
            }
        }
    }

    /// `ec_laplace_encode` (libopus `celt/laplace.c`) — encodes `*value` and
    /// writes back the value actually representable (clamped to the tail).
    fn ec_laplace_encode(enc: &mut MiniEnc, value: &mut i32, mut fs: u32, decay: i32) {
        let mut fl: u32 = 0;
        let mut val = *value;
        if val != 0 {
            let s: i32 = -((val < 0) as i32);
            val = (val + s) ^ s;
            fl = fs;
            fs = ec_laplace_get_freq1(fs, decay);
            let mut i: i32 = 1;
            while fs > 0 && i < val {
                fs *= 2;
                fl += fs + 2 * LAPLACE_MINP;
                fs = ((fs as i32 * decay) >> 15) as u32;
                i += 1;
            }
            if fs == 0 {
                let ndi_max = ((32768 - fl + LAPLACE_MINP - 1) >> LAPLACE_LOG_MINP) as i32;
                let ndi_max = (ndi_max - s) >> 1;
                let di = core::cmp::min(val - i, ndi_max - 1);
                fl = fl.wrapping_add(((2 * di + 1 + s) as u32).wrapping_mul(LAPLACE_MINP));
                fs = core::cmp::min(LAPLACE_MINP, 32768 - fl);
                *value = (i + di + s) ^ s;
            } else {
                fs += LAPLACE_MINP;
                fl += fs & !(s as u32);
            }
        }
        enc.encode_bin(fl, fl + fs);
    }

    /// Encode a sequence of Laplace symbols into one buffer, then decode them
    /// back and assert each equals the (possibly tail-clamped) encoded value.
    /// Proves sign handling, the decay walk, and the far-tail path invert.
    #[test]
    fn ec_laplace_decode_round_trips_encode() {
        // (fs, decay) pairs drawn from real e_prob_model entries, exercised
        // over signed magnitudes from the well-behaved head (|v| small) out to
        // the LAPLACE_MINP tail (|v| large).
        let models: [(u32, i32); 3] = [
            (42u32 << 7, 121i32 << 6), // E_PROB_MODEL[3][0][0..2]
            (72u32 << 7, 127i32 << 6), // E_PROB_MODEL[0][0][0..2]
            (24u32 << 7, 179i32 << 6), // E_PROB_MODEL[0][1][0..2]
        ];
        let values: [i32; 13] = [0, 1, -1, 2, -2, 3, -3, 5, -6, 9, -12, 30, -40];
        for &(fs, decay) in &models {
            // Encode all values into one stream (a sequence, like a real
            // per-band coarse-energy run), then decode the sequence back.
            let mut enc = MiniEnc::new();
            let mut expected = [0i32; 13];
            for (slot, &v) in expected.iter_mut().zip(values.iter()) {
                let mut value = v;
                ec_laplace_encode(&mut enc, &mut value, fs, decay);
                *slot = value; // value after any tail clamp
            }
            enc.done();

            let mut rd = RangeDecoder::new(&enc.buf);
            for (k, &want) in expected.iter().enumerate() {
                let got = ec_laplace_decode(&mut rd, fs, decay);
                assert_eq!(
                    got, want,
                    "laplace seq mismatch at {k}: input={} fs={fs} decay={decay}",
                    values[k]
                );
            }
        }
    }

    #[test]
    fn fine_energy_offset_is_mid_tread_dequant() {
        // 1 bit: two levels at ±0.25 (mid-tread quarter steps).
        assert!((fine_energy_offset(0, 1) - (-0.25)).abs() < 1e-6);
        assert!((fine_energy_offset(1, 1) - 0.25).abs() < 1e-6);
        // 3 bits: 8 levels, step 1/8, endpoints ±0.4375.
        assert!((fine_energy_offset(0, 3) - (-0.4375)).abs() < 1e-6);
        assert!((fine_energy_offset(7, 3) - 0.4375).abs() < 1e-6);
        // The offset is always within (-0.5, 0.5).
        for fq in 1..=8 {
            for q2 in 0..(1u32 << fq) {
                let o = fine_energy_offset(q2, fq);
                assert!(o > -0.5 && o < 0.5, "fq={fq} q2={q2} o={o}");
            }
        }
    }

    #[test]
    fn finalise_energy_offset_is_half_step() {
        // 1 bit finalise: ±0.125 (half of the fine 1-bit ±0.25 step).
        assert!((finalise_energy_offset(0, 1) - (-0.125)).abs() < 1e-6);
        assert!((finalise_energy_offset(1, 1) - 0.125).abs() < 1e-6);
        // 2 bits: ±0.0625.
        assert!((finalise_energy_offset(0, 2) - (-0.0625)).abs() < 1e-6);
        assert!((finalise_energy_offset(1, 2) - 0.0625).abs() < 1e-6);
    }

    #[test]
    fn unquant_fine_energy_reads_bits_and_adds_offset() {
        // dec_bits reads backward from the END of the buffer; a fresh decoder's
        // first dec_bits(3) returns the low 3 bits of the last byte. Set it to
        // 5 so q2 == 5, then check band 0 gains exactly fine_energy_offset(5,3).
        let buf = [0u8, 0, 0, 0, 0, 0, 0, 5];
        let mut rd = RangeDecoder::new(&buf);
        let mut old = [0.0f32; NUM_BANDS * 2];
        unquant_fine_energy(&mut rd, 0, 1, 1, &[3], &mut old);
        assert!((old[0] - fine_energy_offset(5, 3)).abs() < 1e-6);
        // A zero-bit band is skipped (no read, no change).
        let mut old2 = [1.5f32; NUM_BANDS * 2];
        let mut rd2 = RangeDecoder::new(&buf);
        unquant_fine_energy(&mut rd2, 0, 1, 1, &[0], &mut old2);
        assert_eq!(old2[0], 1.5);
    }

    #[test]
    fn unquant_energy_finalise_spends_leftover_bit() {
        // Last byte low bit = 1 -> dec_bits(1) == 1 -> +finalise_energy_offset(1,2).
        let buf = [0u8, 0, 0, 0, 0, 0, 0, 1];
        let mut rd = RangeDecoder::new(&buf);
        let mut old = [0.0f32; NUM_BANDS * 2];
        unquant_energy_finalise(&mut rd, 0, 1, 1, &[2], &[0], 1, &mut old);
        assert!((old[0] - finalise_energy_offset(1, 2)).abs() < 1e-6);
        // A band already at MAX_FINE_BITS is skipped even with bits left.
        let mut old2 = [0.0f32; NUM_BANDS * 2];
        let mut rd2 = RangeDecoder::new(&buf);
        unquant_energy_finalise(&mut rd2, 0, 1, 1, &[MAX_FINE_BITS], &[0], 1, &mut old2);
        assert_eq!(old2[0], 0.0);
    }

    // ================================================================
    // Real-frame coarse-energy decode of testdata/tone.opus.
    //
    // The workspace-root fixture is a libopus (ffmpeg -c:a libopus) encode; its
    // first audio packet is a CELT-only fullband stereo 20 ms frame (config
    // 31). We run the preamble + coarse-energy decode on the first few CELT
    // frames and assert the log2-energy envelope is finite and in a sane range,
    // and stays bounded frame-to-frame (a divergent inter-frame prediction bug
    // would blow past the bound within a few frames). Exact-value validation is
    // deferred to Task 8's end-to-end PCM golden — coarse-only state is carried
    // across frames here (fine-energy refinement, which also feeds inter-frame
    // prediction in libopus, arrives with the Task 6 allocation), so this is a
    // sanity gate, not a bit-exact one.
    // ================================================================
    #[test]
    fn coarse_energy_decodes_real_tone_opus_celt_frames() {
        let bytes = include_bytes!("../../../../../../testdata/tone.opus");
        let mut ogg = OggPackets::new(SliceSource::new(bytes.as_slice())).expect("open ogg-opus");

        // Skip the two header packets (OpusHead, OpusTags).
        let mut hdr = [0u8; 512];
        ogg.next_packet(&mut hdr).expect("OpusHead");
        ogg.next_packet(&mut hdr).expect("OpusTags");

        // Energy envelope persists across frames (inter-frame prediction).
        let mut old_ebands = [0.0f32; NUM_BANDS * 2];
        let mut pkt = [0u8; 4096];
        let mut celt_frames = 0usize;
        let mut saw_inter = false;

        while celt_frames < 4 {
            let n = match ogg.next_packet(&mut pkt) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            let mut frames: [&[u8]; toc::MAX_FRAMES] = [&[][..]; toc::MAX_FRAMES];
            let (config, _fcount) = toc::frames(&pkt[..n], &mut frames).expect("split frames");
            if config.mode != Mode::CeltOnly {
                continue;
            }
            let channels = if config.stereo { 2 } else { 1 };
            let lm = celt_lm(config.frame_size_48k);
            let end = celt_end_band(config.bandwidth);

            // Decode the first CELT frame of this packet.
            let mut rd = RangeDecoder::new(frames[0]);
            let pre = read_preamble(&mut rd, 0, lm);
            saw_inter |= !pre.intra_ener;
            unquant_coarse_energy(
                &mut rd,
                0,
                end,
                channels,
                lm,
                pre.intra_ener,
                &mut old_ebands,
            );

            // Measured envelope over the first four CELT frames of this
            // fixture spans roughly [-9.5, 5.1] in the log2-minus-mean domain
            // (frame 0 intra, frames 1–3 inter-predicted). The ±30 bound is
            // ~3× that spread: wide enough for other content, tight enough that
            // a divergent inter-frame prediction (a runaway `coef` feedback)
            // blows past it within these few frames, and NaN/Inf fails outright.
            for c in 0..channels {
                for i in 0..end {
                    let v = old_ebands[i + c * NUM_BANDS];
                    assert!(
                        v.is_finite(),
                        "frame {celt_frames} band {i} ch {c}: {v} not finite"
                    );
                    assert!(
                        (-30.0..=30.0).contains(&v),
                        "frame {celt_frames} band {i} ch {c}: {v} out of sane log2 range"
                    );
                }
            }
            celt_frames += 1;
        }

        assert!(celt_frames >= 1, "no CELT frame found in tone.opus");
        // With the first frame intra and later frames inter-predicted, the
        // decode should have exercised the inter-frame prediction path too.
        assert!(
            saw_inter || celt_frames == 1,
            "expected at least one inter-predicted CELT frame"
        );
    }
}
