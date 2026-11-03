//! CELT frame decoder — the orchestration + synthesis DSP that turns one CELT
//! frame's range-coded bytes into PCM (RFC 6716 §4.3.5–4.3.7).
//!
//! [`CeltDecoder::decode_frame`] runs the exact `celt_decode_with_ec` order
//! (libopus **v1.5.2** `celt/celt_decoder.c`): preamble → coarse energy →
//! tf/spread/dynalloc/trim/allocation → fine energy → `quant_all_bands` →
//! anti-collapse flag → finalise energy → anti-collapse noise inject →
//! denormalise → inverse MDCT + overlap-add → pitch post-filter → de-emphasis →
//! i16 output. The range-coded stages are the bit-exact-verified Tasks 5–7
//! modules; this file adds the synthesis DSP, carries the cross-frame decoder
//! state (band-energy history, per-channel MDCT decode buffers, de-emphasis
//! memory, post-filter params, and the fold/anti-collapse LCG seed), and emits
//! interleaved 16-bit PCM.
//!
//! `no_std`, no-alloc, no `libm`: the only transcendental is [`celt_exp2`]
//! (denormalise gain + anti-collapse thresholds), transcribed from libopus's
//! `mathops.h` `FLOAT_APPROX` cubic — no `exp`/`pow`. The inverse MDCT
//! ([`super::mdct`]) is likewise `libm`-free.

use super::allocation::compute_allocation;
use super::bands::{MAX_NORM_PER_CH, celt_lcg_rand, quant_all_bands};
use super::energy::{
    celt_end_band, celt_lm, read_preamble, unquant_coarse_energy, unquant_energy_finalise,
    unquant_fine_energy,
};
use super::mdct::{OVERLAP, WINDOW, imdct_block};
use super::vq::{celt_rsqrt, renormalise_vector};
use crate::formats::opus::opus_tables::{E_MEANS, EBAND_5MS, NUM_BANDS};
use crate::formats::opus::range::RangeDecoder;
use crate::formats::opus::toc::Config;

/// libopus `celt/celt_decoder.c` `DECODE_BUFFER_SIZE` — the per-channel MDCT
/// history ring. Each channel's buffer additionally reserves `OVERLAP` tail
/// samples (`decode_mem[c]` is `DECODE_BUFFER_SIZE + overlap` long) so the
/// inverse MDCT's write region never runs off the end.
const DECODE_BUFFER_SIZE: usize = 2048;
/// Per-channel decode buffer length (`DECODE_BUFFER_SIZE + overlap`).
const DECODE_MEM_LEN: usize = DECODE_BUFFER_SIZE + OVERLAP;

/// Largest per-channel frame length `N = shortMdctSize << LM` (20 ms fullband:
/// `120 << 3`).
const MAX_FRAME_N: usize = 960;

/// libopus `celt/celt.h` comb-filter period bounds.
const COMBFILTER_MINPERIOD: i32 = 15;
const COMBFILTER_MAXPERIOD: i32 = 1024;

/// CELT short-block size (`mode->shortMdctSize`), the post-filter's first-segment
/// length and the transient block spacing.
const SHORT_MDCT_SIZE: usize = 120;

/// De-emphasis / pre-emphasis coefficient (libopus `mode->preemph[0]`,
/// `QCONST16(0.8500061f, 15)`), applied by the 1-pole de-emphasis filter.
const PREEMPH_COEF: f32 = 0.850_006_1;

/// libopus `VERY_SMALL` (float) — a denormal-avoiding bias added in de-emphasis.
const VERY_SMALL: f32 = 1e-30;

/// Log-energy floor for out-of-band reset (libopus `-QCONST16(28.f, DB_SHIFT)`).
const ENERGY_FLOOR: f32 = -28.0;

/// libopus `celt/celt.c` comb-filter tap gains, one row per tapset (Q15 → float).
static COMB_GAINS: [[f32; 3]; 3] = [
    [0.306_640_63, 0.217_041_02, 0.129_638_67],
    [0.463_867_2, 0.268_066_4, 0.0],
    [0.799_804_7, 0.100_097_656, 0.0],
];

/// `celt_exp2(x)` — `2^x` without `libm` (libopus `celt/mathops.h`, the
/// `FLOAT_APPROX` non-fixed-point branch): split off the integer part with a
/// manual floor, evaluate a cubic on the fraction, then add the integer part
/// into the IEEE-754 exponent field. Precision is audio-quality-only (this runs
/// after the range coder), so the cubic approximation is fine.
pub(crate) fn celt_exp2(x: f32) -> f32 {
    // (int)floor(x) without libm.
    let mut integer = x as i32;
    if (integer as f32) > x {
        integer -= 1;
    }
    if integer < -50 {
        return 0.0;
    }
    let frac = x - integer as f32;
    let res = 0.999_925_2 + frac * (0.695_833_54 + frac * (0.226_067_16 + 0.078_024_52 * frac));
    // res.i = (res.i + ((opus_uint32)integer << 23)) & 0x7fffffff
    let bits = res.to_bits().wrapping_add((integer as u32) << 23) & 0x7fff_ffff;
    f32::from_bits(bits)
}

/// Unsigned division matching libopus `celt_udiv` (non-negative numerator,
/// positive denominator); the guard only stops a malformed packet panicking.
fn celt_udiv(n: i32, d: i32) -> i32 {
    if d <= 0 {
        0
    } else {
        ((n as u32) / (d as u32)) as i32
    }
}

/// Cross-frame CELT decoder state (fixed-size, no heap). Sized for the standard
/// 48 kHz mode with up to 2 channels.
pub(crate) struct CeltDecoder {
    /// Output channel count `CC` (from `OpusHead`).
    channels: usize,
    /// `oldBandE` — the running decoded log2-energy envelope (coarse + fine +
    /// finalise), relative to the band means; inter-frame prediction reads it.
    old_ebands: [f32; 2 * NUM_BANDS],
    /// `oldLogE` — the previous frame's final energies (anti-collapse `prev1`).
    old_log_e: [f32; 2 * NUM_BANDS],
    /// `oldLogE2` — two frames ago (anti-collapse `prev2`).
    old_log_e2: [f32; 2 * NUM_BANDS],
    /// Per-channel inverse-MDCT decode buffer (`decode_mem`).
    decode_mem: [[f32; DECODE_MEM_LEN]; 2],
    /// Per-channel de-emphasis (1-pole) filter memory.
    preemph_mem: [f32; 2],
    /// Pitch post-filter state (shared across channels), current and previous.
    pf_period: i32,
    pf_period_old: i32,
    pf_gain: f32,
    pf_gain_old: f32,
    pf_tapset: u32,
    pf_tapset_old: u32,
    /// PVQ-fold / anti-collapse LCG seed (`st->rng`), carried across frames.
    rng: u32,
}

impl CeltDecoder {
    /// Create a fresh decoder for `channels` (1 or 2) output channels.
    pub(crate) fn new(channels: usize) -> Self {
        Self {
            channels: channels.clamp(1, 2),
            old_ebands: [0.0; 2 * NUM_BANDS],
            old_log_e: [ENERGY_FLOOR; 2 * NUM_BANDS],
            old_log_e2: [ENERGY_FLOOR; 2 * NUM_BANDS],
            decode_mem: [[0.0; DECODE_MEM_LEN]; 2],
            preemph_mem: [0.0; 2],
            pf_period: 0,
            pf_period_old: 0,
            pf_gain: 0.0,
            pf_gain_old: 0.0,
            pf_tapset: 0,
            pf_tapset_old: 0,
            rng: 0,
        }
    }

    /// Decode one CELT-only frame (`frame` bytes) into `pcm_out` as interleaved
    /// 16-bit samples (in `i32` slots), applying `output_gain` (linear). Returns
    /// the number of `i32` written (`N * CC`). `pcm_out` must hold at least
    /// `N * CC`.
    pub(crate) fn decode_frame(
        &mut self,
        frame: &[u8],
        config: Config,
        output_gain: f32,
        pcm_out: &mut [i32],
    ) -> usize {
        let mut rd = RangeDecoder::new(frame);
        let (out_off, n, cc) = self.decode_body(&mut rd, frame.len(), config, 0);
        // (16–17) de-emphasis → i16 PCM, applying output_gain.
        self.deemphasis(out_off, n, cc, output_gain, pcm_out);
        n * cc
    }

    /// The range coder's final `rng` from the most recently decoded frame
    /// (libopus `st->rng`, saved as `dec.rng` at the end of `decode_body`). The
    /// Opus per-packet final-range check (`opus_demo` / RFC 8251) compares this
    /// (XORed with any redundant-frame rng) against the encoder's stored value.
    pub(crate) fn last_rng(&self) -> u32 {
        self.rng
    }

    /// Decode the CELT **high band** of one Hybrid frame (RFC 6716 hybrid; see
    /// `opus_decoder.c` `opus_decode_frame`). `rd` is the frame's range decoder
    /// **already positioned after the SILK low band** (and its redundancy flag);
    /// CELT resumes reading the same coder with `start = 17` (bands below 17 —
    /// the 0–8 kHz range SILK covers — are not coded). `frame_len` is the full
    /// Opus frame length in bytes, since CELT's bit budget spans the whole frame.
    ///
    /// Writes the **raw** de-emphasized signal (`CELT_SIG_SCALE = ±32768` domain,
    /// no rounding/clamping/gain) into `sig_out` — libopus sums the float CELT
    /// output with the SILK low band *before* the final round/clamp, so the
    /// caller ([`super::super::OpusDecoder`]) adds the SILK int16 low band, applies
    /// output gain, then rounds/clamps. Returns `N * CC` samples written.
    pub(crate) fn decode_hybrid_high(
        &mut self,
        rd: &mut RangeDecoder,
        frame_len: usize,
        config: Config,
        sig_out: &mut [f32],
    ) -> usize {
        self.decode_signal(rd, frame_len, config, 17, sig_out)
    }

    /// Decode one CELT frame at an arbitrary `start` band from `rd` into the raw
    /// de-emphasized float signal (`CELT_SIG_SCALE = ±32768` domain, no gain /
    /// rounding / clipping), updating the cross-frame CELT state. Generalises
    /// [`Self::decode_hybrid_high`] (start band 17) for the Opus **redundancy**
    /// and **hybrid→SILK silence-fade** frames of `opus_decode_frame`
    /// (RFC 8251 mode transitions), which decode a CELT-only frame at start band
    /// 0. `config` supplies the frame size (LM), bandwidth (end band), and
    /// stream stereo flag. Returns `N * CC` samples written to `sig_out`.
    pub(crate) fn decode_signal(
        &mut self,
        rd: &mut RangeDecoder,
        frame_len: usize,
        config: Config,
        start: usize,
        sig_out: &mut [f32],
    ) -> usize {
        let (out_off, n, cc) = self.decode_body(rd, frame_len, config, start);
        self.deemphasis_sig(out_off, n, cc, sig_out);
        n * cc
    }

    /// Shared CELT decode: everything from the per-frame preamble through the
    /// inverse MDCT, post-filter, and cross-frame energy bookkeeping, leaving the
    /// synthesized samples in `self.decode_mem`. `start` is the CELT start band
    /// (0 for a CELT-only frame, 17 for the Hybrid high band); `frame_len` is the
    /// Opus frame length in bytes (CELT's total bit budget). `rd` may be a fresh
    /// coder (CELT-only) or one already advanced by SILK (Hybrid). Returns
    /// `(out_off, N, CC)` — the `out_syn` offset of the new samples, the
    /// per-channel frame length, and the output channel count.
    fn decode_body(
        &mut self,
        rd: &mut RangeDecoder,
        frame_len: usize,
        config: Config,
        start: usize,
    ) -> (usize, usize, usize) {
        let cc = self.channels;
        let c = if config.stereo { 2 } else { 1 };
        let lm = celt_lm(config.frame_size_48k);
        let end = celt_end_band(config.bandwidth);
        let eff_end = end.min(NUM_BANDS); // effEBands == nbEBands (21)
        let m = 1usize << lm; // MDCT sub-block count
        let n = m * SHORT_MDCT_SIZE; // frame length per channel

        let total_bits = (frame_len as i32) * 8;

        // Mono frame: fold the two stored channels' energy into channel 0 as the
        // inter-frame prediction base (libopus `celt_decode_with_ec`:
        // `if (C==1) for i: oldBandE[i] = MAX16(oldBandE[i], oldBandE[nbEBands+i])`).
        // On a steady mono run this is a no-op (the previous mono frame copied
        // channel 0 into channel 1), but across a **stereo→mono** transition it
        // is what carries the side channel's energy into the mono prediction —
        // omitting it made a mono frame after a stereo frame predict from a stale
        // channel-0 envelope, corrupting the CELT energy for the rest of the run.
        if c == 1 {
            for i in 0..NUM_BANDS {
                self.old_ebands[i] = self.old_ebands[i].max(self.old_ebands[NUM_BANDS + i]);
            }
        }

        // (2) Preamble: silence / post-filter (start==0 only) / transient / intra.
        let pre = read_preamble(rd, start, lm);
        let is_transient = pre.is_transient;
        let silence = pre.silence;

        // (3) Coarse energy (updates old_ebands from the previous frame's values).
        unquant_coarse_energy(rd, start, end, c, lm, pre.intra_ener, &mut self.old_ebands);

        // (4–7) tf / spread / dynalloc / trim / anti-collapse reserve / allocation.
        let alloc = compute_allocation(rd, start, end, c, lm, is_transient);

        // (8) Fine energy (adds to old_ebands).
        unquant_fine_energy(rd, start, end, c, &alloc.fine_quant, &mut self.old_ebands);

        // (9) PVQ band shapes → normalised X (and Y) + collapse masks.
        let mut x_full = [0.0f32; 2 * MAX_FRAME_N];
        let mut collapse = [0u8; NUM_BANDS * 2];
        let mut norm = [0.0f32; 2 * MAX_NORM_PER_CH];
        let mut seed = self.rng;
        let total_bits_frac = total_bits * 8 - alloc.anti_collapse_rsv; // len*(8<<BITRES) - rsv
        let disable_inv = self.channels == 1;
        {
            let (x_slice, y_slice) = x_full.split_at_mut(n);
            let y = if c == 2 {
                Some(&mut y_slice[..n])
            } else {
                None
            };
            quant_all_bands(
                rd,
                start,
                end,
                &mut x_slice[..n],
                y,
                &mut collapse,
                &alloc.pulses,
                is_transient,
                alloc.spread,
                alloc.dual_stereo,
                alloc.intensity,
                &alloc.tf_res,
                total_bits_frac,
                alloc.balance,
                lm,
                alloc.coded_bands,
                &mut seed,
                disable_inv,
                &mut norm,
            );
        }

        // (10) anti_collapse_on flag (one raw bit, between bands and finalise).
        let anti_collapse_on = alloc.anti_collapse_rsv > 0 && rd.dec_bits(1) != 0;

        // (11) Finalise energy (leftover fine bits).
        let bits_left = total_bits - rd.tell() as i32;
        unquant_energy_finalise(
            rd,
            start,
            end,
            c,
            &alloc.fine_quant,
            &alloc.fine_priority,
            bits_left,
            &mut self.old_ebands,
        );

        // (12) anti-collapse noise injection into collapsed short blocks.
        if anti_collapse_on {
            self.anti_collapse(
                &mut x_full,
                &collapse,
                lm,
                c,
                n,
                start,
                end,
                &alloc.pulses,
                seed,
            );
        }

        // Save the entropy coder's final range as next frame's fold seed.
        self.rng = rd.rng();

        // On a silent frame, force the whole band-energy envelope to the floor
        // (libopus `celt_decode_with_ec`: `if (silence) oldBandE[i] =
        // -QCONST16(28.f, DB_SHIFT)`, applied before `celt_synthesis`). This
        // both zeroes this frame's synthesis and — crucially — resets the
        // cross-frame energy state so the *next* frame's coarse-energy inter-
        // frame prediction starts from the floor, not the stale coarse values a
        // near-empty (e.g. 3-byte) frame happens to leave behind (RFC 6716
        // §4.3.1; the omission produced sustained divergence after a silent CELT
        // frame in the RFC 8251 vectors).
        if silence {
            for e in self.old_ebands[..c * NUM_BANDS].iter_mut() {
                *e = ENERGY_FLOOR;
            }
        }

        // (13–14) denormalise + inverse MDCT + overlap-add into decode_mem.
        let out_off = self.synthesis(&x_full, n, c, cc, lm, is_transient, silence, start, eff_end);

        // (15) pitch post-filter (comb) with the one-frame-delayed crossfade.
        self.post_filter(&pre, out_off, n, cc, lm);

        // Energy-history bookkeeping for the next frame.
        self.update_energy_history(is_transient, c, start, end);

        (out_off, n, cc)
    }

    /// (12) `anti_collapse` (libopus `celt/bands.c`) — inject `celt_lcg_rand`
    /// noise, scaled by the per-band depth and the drop from the previous
    /// frames' energy, into short-block slots whose collapse mask is clear.
    #[allow(clippy::too_many_arguments)]
    fn anti_collapse(
        &self,
        x_full: &mut [f32],
        collapse_masks: &[u8],
        lm: u32,
        c: usize,
        size: usize,
        start: usize,
        end: usize,
        pulses: &[i32; NUM_BANDS],
        mut seed: u32,
    ) {
        let nb = NUM_BANDS;
        for i in start..end {
            let width = i32::from(EBAND_5MS[i + 1]) - i32::from(EBAND_5MS[i]);
            let n0 = width as usize;
            let depth = celt_udiv(1 + pulses[i], width) >> lm;
            let thresh = 0.5 * celt_exp2(-0.125 * depth as f32);
            let sqrt_1 = celt_rsqrt((n0 << lm) as f32);

            for cc in 0..c {
                let mut prev1 = self.old_log_e[cc * nb + i];
                let mut prev2 = self.old_log_e2[cc * nb + i];
                if c == 1 {
                    prev1 = prev1.max(self.old_log_e[nb + i]);
                    prev2 = prev2.max(self.old_log_e2[nb + i]);
                }
                let ediff = (self.old_ebands[cc * nb + i] - prev1.min(prev2)).max(0.0);
                let mut r = 2.0 * celt_exp2(-ediff);
                if lm == 3 {
                    r *= core::f32::consts::SQRT_2;
                }
                r = r.min(thresh) * sqrt_1;

                let xoff = cc * size + ((EBAND_5MS[i] as usize) << lm);
                let mut renorm = false;
                for k in 0..(1usize << lm) {
                    if collapse_masks[i * c + cc] & (1 << k) == 0 {
                        for j in 0..n0 {
                            seed = celt_lcg_rand(seed);
                            x_full[xoff + (j << lm) + k] = if seed & 0x8000 != 0 { r } else { -r };
                        }
                        renorm = true;
                    }
                }
                if renorm {
                    let len = n0 << lm;
                    renormalise_vector(&mut x_full[xoff..xoff + len], len, 1.0);
                }
            }
        }
    }

    /// (13–14) `celt_synthesis` (libopus `celt/celt_decoder.c`): shift each
    /// channel's decode buffer, denormalise the band energies onto the spectrum,
    /// and inverse-MDCT + overlap-add each block into the decode buffer. Returns
    /// the `out_syn` offset (`DECODE_BUFFER_SIZE - N`) of the new samples.
    #[allow(clippy::too_many_arguments)]
    fn synthesis(
        &mut self,
        x_full: &[f32],
        n: usize,
        c: usize,
        cc: usize,
        lm: u32,
        is_transient: bool,
        silence: bool,
        start: usize,
        eff_end: usize,
    ) -> usize {
        let m = 1usize << lm;
        let (b, nb) = if is_transient {
            (m, SHORT_MDCT_SIZE)
        } else {
            (1, n)
        };
        let n2 = nb; // per-block frequency-line count == block spacing

        // Shift each output channel's decode buffer left by N (OPUS_MOVE).
        for ch in 0..cc {
            self.decode_mem[ch].copy_within(n..DECODE_MEM_LEN, 0);
        }
        let out_off = DECODE_BUFFER_SIZE - n;

        let mut freq = [0.0f32; MAX_FRAME_N];
        let mut freq2 = [0.0f32; MAX_FRAME_N];

        if cc == 2 && c == 1 {
            // Mono frame upmixed to a stereo output: one spectrum, both channels.
            denormalise_bands(
                &x_full[..n],
                &self.old_ebands[..],
                &mut freq[..n],
                start,
                eff_end,
                m,
                silence,
            );
            for ch in 0..2 {
                for blk in 0..b {
                    imdct_block(
                        &freq[blk..],
                        n2,
                        b,
                        &mut self.decode_mem[ch][out_off + nb * blk..],
                    );
                }
            }
        } else if cc == 1 && c == 2 {
            // Stereo frame downmixed to mono: average the two spectra.
            denormalise_bands(
                &x_full[..n],
                &self.old_ebands[..],
                &mut freq[..n],
                start,
                eff_end,
                m,
                silence,
            );
            denormalise_bands(
                &x_full[n..2 * n],
                &self.old_ebands[NUM_BANDS..],
                &mut freq2[..n],
                start,
                eff_end,
                m,
                silence,
            );
            for j in 0..n {
                freq[j] = 0.5 * freq[j] + 0.5 * freq2[j];
            }
            for blk in 0..b {
                imdct_block(
                    &freq[blk..],
                    n2,
                    b,
                    &mut self.decode_mem[0][out_off + nb * blk..],
                );
            }
        } else {
            // Normal case: C == CC.
            for ch in 0..cc {
                denormalise_bands(
                    &x_full[ch * n..ch * n + n],
                    &self.old_ebands[ch * NUM_BANDS..],
                    &mut freq[..n],
                    start,
                    eff_end,
                    m,
                    silence,
                );
                for blk in 0..b {
                    imdct_block(
                        &freq[blk..],
                        n2,
                        b,
                        &mut self.decode_mem[ch][out_off + nb * blk..],
                    );
                }
            }
        }

        out_off
    }

    /// (15) Pitch post-filter (`comb_filter`) with the one-frame delay: the first
    /// short-block spends the crossfade between the two previous frames' params,
    /// the remainder crossfades the previous frame's into this frame's decoded
    /// params. State is shared across channels and advanced afterward.
    fn post_filter(
        &mut self,
        pre: &super::energy::CeltPreamble,
        out_off: usize,
        n: usize,
        cc: usize,
        lm: u32,
    ) {
        // Newly decoded post-filter params (or an off filter: gain 0).
        let (pf_pitch, pf_gain_new, pf_tapset_new) = match pre.post_filter {
            Some(p) => (p.period, p.gain, p.tapset),
            None => (COMBFILTER_MINPERIOD, 0.0, 0),
        };

        self.pf_period = self.pf_period.max(COMBFILTER_MINPERIOD);
        self.pf_period_old = self.pf_period_old.max(COMBFILTER_MINPERIOD);

        for ch in 0..cc {
            comb_filter(
                &mut self.decode_mem[ch],
                out_off,
                SHORT_MDCT_SIZE.min(n),
                self.pf_period_old,
                self.pf_period,
                self.pf_gain_old,
                self.pf_gain,
                self.pf_tapset_old,
                self.pf_tapset,
            );
            if lm != 0 {
                comb_filter(
                    &mut self.decode_mem[ch],
                    out_off + SHORT_MDCT_SIZE,
                    n - SHORT_MDCT_SIZE,
                    self.pf_period,
                    pf_pitch,
                    self.pf_gain,
                    pf_gain_new,
                    self.pf_tapset,
                    pf_tapset_new,
                );
            }
        }

        // Advance the post-filter state (libopus's interleaved save).
        self.pf_period_old = self.pf_period;
        self.pf_gain_old = self.pf_gain;
        self.pf_tapset_old = self.pf_tapset;
        self.pf_period = pf_pitch;
        self.pf_gain = pf_gain_new;
        self.pf_tapset = pf_tapset_new;
        if lm != 0 {
            self.pf_period_old = self.pf_period;
            self.pf_gain_old = self.pf_gain;
            self.pf_tapset_old = self.pf_tapset;
        }
    }

    /// (16–17) 1-pole de-emphasis per channel, then scale to the i16 domain
    /// (apply `output_gain`, round, clip) and interleave into `pcm_out`.
    ///
    /// libopus keeps the internal signal at the `±32768` (`CELT_SIG_SCALE`)
    /// domain; de-emphasis emits `tmp/32768` and the API multiplies back by
    /// 32768 — the two cancel, so the i16 value is simply the rounded, clipped
    /// de-emphasised sample (times any output gain).
    fn deemphasis(
        &mut self,
        out_off: usize,
        n: usize,
        cc: usize,
        output_gain: f32,
        pcm_out: &mut [i32],
    ) {
        for ch in 0..cc {
            let mut mem = self.preemph_mem[ch];
            for i in 0..n {
                let x = self.decode_mem[ch][out_off + i];
                let tmp = x + VERY_SMALL + mem;
                mem = PREEMPH_COEF * tmp;
                let scaled = tmp * output_gain;
                let rounded = if scaled >= 0.0 {
                    (scaled + 0.5) as i32
                } else {
                    (scaled - 0.5) as i32
                };
                pcm_out[i * cc + ch] = rounded.clamp(-32768, 32767);
            }
            self.preemph_mem[ch] = mem;
        }
    }

    /// (16) 1-pole de-emphasis per channel, emitting the **raw** de-emphasized
    /// signal (`CELT_SIG_SCALE = ±32768` domain) as `f32` with no gain, rounding,
    /// or clipping — the Hybrid combine path ([`Self::decode_hybrid_high`]) needs
    /// the un-rounded CELT high band so it can add the SILK low band before the
    /// single final round/clamp (matching libopus's float `pcm[i] = pcm[i] +
    /// (1/32768)*pcm_silk[i]` sum, which happens prior to `float2int16`).
    fn deemphasis_sig(&mut self, out_off: usize, n: usize, cc: usize, sig_out: &mut [f32]) {
        for ch in 0..cc {
            let mut mem = self.preemph_mem[ch];
            for i in 0..n {
                let x = self.decode_mem[ch][out_off + i];
                let tmp = x + VERY_SMALL + mem;
                mem = PREEMPH_COEF * tmp;
                sig_out[i * cc + ch] = tmp;
            }
            self.preemph_mem[ch] = mem;
        }
    }

    /// End-of-frame energy-history bookkeeping (libopus `celt_decode_with_ec`
    /// tail): mono-duplicate, shift `oldLogE → oldLogE2` and `oldBandE → oldLogE`
    /// (or take the min on a transient), then reset out-of-band energies.
    fn update_energy_history(&mut self, is_transient: bool, c: usize, start: usize, end: usize) {
        let nb = NUM_BANDS;
        // Mono streams mirror channel 0 into the channel-1 slot.
        if c == 1 {
            let (a, b) = self.old_ebands.split_at_mut(nb);
            b.copy_from_slice(&a[..nb]);
        }
        if !is_transient {
            self.old_log_e2 = self.old_log_e;
            self.old_log_e = self.old_ebands;
        } else {
            for i in 0..2 * nb {
                self.old_log_e[i] = self.old_log_e[i].min(self.old_ebands[i]);
            }
        }
        // Reset energies outside the coded band range (both channel slots).
        for ch in 0..2 {
            for i in 0..start {
                let idx = ch * nb + i;
                self.old_ebands[idx] = 0.0;
                self.old_log_e[idx] = ENERGY_FLOOR;
                self.old_log_e2[idx] = ENERGY_FLOOR;
            }
            for i in end..nb {
                let idx = ch * nb + i;
                self.old_ebands[idx] = 0.0;
                self.old_log_e[idx] = ENERGY_FLOOR;
                self.old_log_e2[idx] = ENERGY_FLOOR;
            }
        }
    }
}

/// `denormalise_bands` (libopus `celt/bands.c`, float path): scale each band's
/// unit-norm shape `x` by `celt_exp2(bandLogE[i] + eMeans[i])` into `freq`, and
/// zero everything outside the coded range. `silence` forces an all-zero
/// spectrum.
fn denormalise_bands(
    x: &[f32],
    band_log_e: &[f32],
    freq: &mut [f32],
    start: usize,
    end: usize,
    m: usize,
    silence: bool,
) {
    let n = freq.len();
    let mut start = start;
    let mut end = end;
    let mut bound = m * (EBAND_5MS[end] as usize);
    if silence {
        bound = 0;
        start = 0;
        end = 0;
    }
    let lead = m * (EBAND_5MS[start] as usize);
    for f in freq.iter_mut().take(lead) {
        *f = 0.0;
    }
    for i in start..end {
        let lg = (band_log_e[i] + E_MEANS[i]).min(32.0);
        let g = celt_exp2(lg);
        let j0 = m * (EBAND_5MS[i] as usize);
        let j1 = m * (EBAND_5MS[i + 1] as usize);
        for j in j0..j1 {
            freq[j] = x[j] * g;
        }
    }
    for f in freq.iter_mut().take(n).skip(bound) {
        *f = 0.0;
    }
}

/// `comb_filter` (libopus `celt/celt.c`, float path): in-place pitch post-filter
/// over `buf[base .. base+n)`, crossfading from `(t0, g0, tapset0)` to
/// `(t1, g1, tapset1)` across the first `OVERLAP` samples, then a constant
/// `(t1, g1)` filter for the rest. Reads history at negative offsets (previous
/// frames) directly from `buf`.
#[allow(clippy::too_many_arguments)]
fn comb_filter(
    buf: &mut [f32],
    base: usize,
    n: usize,
    t0: i32,
    t1: i32,
    g0: f32,
    g1: f32,
    tapset0: u32,
    tapset1: u32,
) {
    if g0 == 0.0 && g1 == 0.0 {
        return; // in-place: nothing to do
    }
    let t0 = t0.clamp(COMBFILTER_MINPERIOD, COMBFILTER_MAXPERIOD);
    let t1 = t1.clamp(COMBFILTER_MINPERIOD, COMBFILTER_MAXPERIOD);
    let ts0 = (tapset0 as usize).min(2);
    let ts1 = (tapset1 as usize).min(2);
    let g00 = g0 * COMB_GAINS[ts0][0];
    let g01 = g0 * COMB_GAINS[ts0][1];
    let g02 = g0 * COMB_GAINS[ts0][2];
    let g10 = g1 * COMB_GAINS[ts1][0];
    let g11 = g1 * COMB_GAINS[ts1][1];
    let g12 = g1 * COMB_GAINS[ts1][2];

    let bi = base as i32;
    // Running history for the new-period (t1) taps.
    let mut x1 = buf[(bi - t1 + 1) as usize];
    let mut x2 = buf[(bi - t1) as usize];
    let mut x3 = buf[(bi - t1 - 1) as usize];
    let mut x4 = buf[(bi - t1 - 2) as usize];

    let overlap = if g0 == g1 && t0 == t1 && ts0 == ts1 {
        0
    } else {
        OVERLAP.min(n)
    };

    for i in 0..overlap {
        let ii = i as i32;
        let x0 = buf[(bi + ii - t1 + 2) as usize];
        let f = WINDOW[i] * WINDOW[i];
        let old = (1.0 - f)
            * (g00 * buf[(bi + ii - t0) as usize]
                + g01 * (buf[(bi + ii - t0 + 1) as usize] + buf[(bi + ii - t0 - 1) as usize])
                + g02 * (buf[(bi + ii - t0 + 2) as usize] + buf[(bi + ii - t0 - 2) as usize]));
        let new = f * (g10 * x2 + g11 * (x1 + x3) + g12 * (x0 + x4));
        buf[base + i] += old + new;
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
    }

    if g1 == 0.0 {
        return; // remainder is an identity copy (in-place)
    }
    comb_filter_const(buf, base + overlap, n - overlap, t1, g10, g11, g12);
}

/// `comb_filter_const_c` (libopus `celt/celt.c`, float path): the constant
/// `(T, g)` comb over `buf[base .. base+n)`, in place, reading its own filtered
/// history at offset `T` (a feedback comb, exactly as libopus applies it
/// in-place on the decode buffer).
fn comb_filter_const(buf: &mut [f32], base: usize, n: usize, t: i32, g10: f32, g11: f32, g12: f32) {
    let bi = base as i32;
    let mut x1 = buf[(bi - t + 1) as usize];
    let mut x2 = buf[(bi - t) as usize];
    let mut x3 = buf[(bi - t - 1) as usize];
    let mut x4 = buf[(bi - t - 2) as usize];
    for i in 0..n {
        let x0 = buf[(bi + i as i32 - t + 2) as usize];
        buf[base + i] += g10 * x2 + g11 * (x1 + x3) + g12 * (x0 + x4);
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn celt_exp2_matches_pow2() {
        // Against an f64 reference over the range denormalise/anti-collapse use.
        for k in -400..400 {
            let x = k as f32 / 10.0;
            let got = celt_exp2(x);
            let want = 2f64.powf(x as f64) as f32;
            let tol = 3e-3 * want.max(1e-20);
            assert!(
                (got - want).abs() <= tol,
                "celt_exp2({x}) = {got}, want {want}"
            );
        }
        // Underflow guard.
        assert_eq!(celt_exp2(-60.0), 0.0);
    }

    #[test]
    fn celt_exp2_zero_is_one() {
        assert!((celt_exp2(0.0) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn denormalise_silence_zeroes_and_gain_scales() {
        let m = 8;
        let n = m * 120;
        let mut freq = vec![0.0f32; n];
        let x = vec![0.5f32; n];
        let mut e = [0.0f32; NUM_BANDS];
        e[0] = 1.0;
        denormalise_bands(&x, &e, &mut freq, 0, 21, m, false);
        // Band 0 (bins 0..m) scaled by celt_exp2(1.0 + E_MEANS[0]).
        let g = celt_exp2(1.0 + E_MEANS[0]);
        assert!((freq[0] - 0.5 * g).abs() < 1e-2 * g);
        // Silence zeroes everything.
        denormalise_bands(&x, &e, &mut freq, 0, 21, m, true);
        assert!(freq.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn comb_filter_zero_gain_is_identity() {
        let mut buf = [0.0f32; 2048];
        for (i, v) in buf.iter_mut().enumerate() {
            *v = (i as f32 * 0.01).sin();
        }
        let before = buf;
        comb_filter(&mut buf, 1500, 120, 100, 100, 0.0, 0.0, 0, 0);
        assert_eq!(buf, before, "zero-gain comb filter must be identity");
    }

    #[test]
    fn decoder_state_size_is_bounded() {
        // Guards the no-alloc, fixed-capacity decoder-state budget. decode_mem
        // (2 × (2048+120) f32) dominates; the whole struct must stay well under
        // the AnyDecoder envelope (FLAC's ~147 KiB variant is the ceiling).
        let sz = core::mem::size_of::<CeltDecoder>();
        assert!(sz < 40 * 1024, "CeltDecoder grew to {sz} bytes");
    }
}
