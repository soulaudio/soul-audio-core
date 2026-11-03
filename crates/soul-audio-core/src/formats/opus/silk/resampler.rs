//! SILK output resampler (RFC 6716 §4.2.9): converts the SILK internal sample
//! rate (8/12/16 kHz) up to the fixed 48 kHz Opus output rate.
//!
//! Transcribed from libopus **v1.5.2** `silk/resampler.c` (`silk_resampler_init`
//! and `silk_resampler`), `silk/resampler_private_IIR_FIR.c` (the polyphase
//! interpolation), and `silk/resampler_private_up2_HQ.c` (the 2x HQ allpass
//! upsampler). Every SILK→48 kHz ratio (8→48, 12→48, 16→48) is a non-integer
//! upsample, so libopus routes them all through the **IIR/FIR** path
//! (`USE_silk_resampler_private_IIR_FIR`, `up2x = 1`): a 2x allpass upsample
//! followed by a 12-phase FIR interpolation. The 2:1 (`up2_HQ`-only) and the
//! downsampling `AR2`/`down_FIR` paths libopus also carries are never reached
//! from a SILK-only decode, so they are intentionally omitted here.
//!
//! **Delay-line state** — the classic bug source: `s_iir` (the up2 allpass
//! memory), `s_fir_i16` (the FIR history), and `delay_buf` (the input-delay
//! compensation) all persist across frames in the [`Resampler`] and MUST be
//! advanced exactly as libopus does, or every frame boundary clicks.
//!
//! Pure fixed-point integer math — `no_std` / no-alloc / `#![forbid(unsafe_code)]`.

use super::silk_tables::{RESAMPLER_FRAC_FIR_12, RESAMPLER_UP2_HQ_0, RESAMPLER_UP2_HQ_1};
use super::{
    add32, div32, lshift, rshift_round, sat16, smlabb, smlawb, smulbb, smulwb, smulww, sub32,
};

/// `RESAMPLER_ORDER_FIR_12` — the FIR history length carried between calls.
const ORDER_FIR_12: usize = 8;
/// `RESAMPLER_MAX_BATCH_SIZE_MS` — inner-loop input batch (ms).
const MAX_BATCH_MS: usize = 10;
/// The widest internal SILK rate (kHz); sizes the interpolation scratch buffer.
const MAX_FS_IN_KHZ: usize = 16;
/// `2 * batchSize + RESAMPLER_ORDER_FIR_12` with `batchSize = 16 * 10` — the
/// largest polyphase working buffer any SILK→48 kHz call needs.
const MAX_BUF: usize = 2 * (MAX_FS_IN_KHZ * MAX_BATCH_MS) + ORDER_FIR_12;
/// Output rate in kHz.
const FS_OUT_KHZ: i32 = 48;

/// SILK output resampler state (fixed-size, no heap). One per decoded channel.
#[derive(Clone)]
pub(crate) struct Resampler {
    /// `sIIR[6]` — the 2x HQ up-sampler's 2nd-order allpass memory (Q10).
    s_iir: [i32; 6],
    /// `sFIR.i16[8]` — the polyphase FIR history carried across calls.
    s_fir_i16: [i16; ORDER_FIR_12],
    /// `delayBuf[48]` — input-delay compensation buffer (only the first
    /// `input_delay` entries are live).
    delay_buf: [i16; 48],
    /// `inputDelay` — samples of input held back to equalise total system delay.
    input_delay: usize,
    /// `invRatio_Q16` — Q16 input/output sample-rate ratio for the FIR phase.
    inv_ratio_q16: i32,
    /// `batchSize` — `Fs_in_kHz * RESAMPLER_MAX_BATCH_SIZE_MS`.
    batch_size: usize,
    /// `Fs_in_kHz` (8/12/16).
    fs_in_khz: usize,
}

impl Default for Resampler {
    fn default() -> Self {
        Self {
            s_iir: [0; 6],
            s_fir_i16: [0; ORDER_FIR_12],
            delay_buf: [0; 48],
            input_delay: 0,
            inv_ratio_q16: 0,
            batch_size: 0,
            fs_in_khz: 0,
        }
    }
}

impl Resampler {
    /// `silk_resampler_init` for the decoder direction from `fs_in_khz` (kHz) to
    /// 48 kHz: clears all delay-line state and computes the input delay + Q16
    /// resampling ratio. `fs_in_khz` is one of 8/12/16.
    pub(crate) fn init(fs_in_khz: i32) -> Self {
        let fs_in_hz = fs_in_khz * 1000;
        let fs_out_hz = FS_OUT_KHZ * 1000;
        // delay_matrix_dec[ rateID(fs_in) ][ rateID(48000) ] (silk/resampler.c):
        // rateID(48000) == 4, so this is the 8/12/16 → 48 kHz column.
        let input_delay = match fs_in_khz {
            8 => 0,
            12 => 4,
            _ => 7,
        };
        // invRatio_Q16 = LSHIFT( DIV32( LSHIFT(Fs_in, 14 + up2x), Fs_out ), 2 ),
        // with up2x = 1, rounded up.
        let mut inv = lshift(div32(lshift(fs_in_hz, 15), fs_out_hz), 2);
        while smulww(inv, fs_out_hz) < lshift(fs_in_hz, 1) {
            inv += 1;
        }
        Self {
            input_delay: input_delay as usize,
            inv_ratio_q16: inv,
            batch_size: (fs_in_khz as usize) * MAX_BATCH_MS,
            fs_in_khz: fs_in_khz as usize,
            ..Self::default()
        }
    }

    /// `silk_resampler`: resample `input` (`in_len` samples at the internal rate)
    /// into `out`, returning the number of 48 kHz samples written
    /// (`in_len * 48 / Fs_in_kHz`). `out` must hold at least that many; `input`
    /// must be at least `Fs_in_kHz` long.
    pub(crate) fn process(&mut self, out: &mut [i16], input: &[i16]) -> usize {
        let fs_in = self.fs_in_khz;
        let fs_out = FS_OUT_KHZ as usize;
        let in_len = input.len();
        if in_len < fs_in || self.input_delay > fs_in {
            return 0;
        }
        // Copy new samples into the delay buffer behind the held-over inputDelay.
        let n_samples = fs_in - self.input_delay;
        self.delay_buf[self.input_delay..self.input_delay + n_samples]
            .copy_from_slice(&input[..n_samples]);

        // First batch: the delay buffer's Fs_in_kHz samples (held-over + new).
        let mut head = [0i16; 48];
        head[..fs_in].copy_from_slice(&self.delay_buf[..fs_in]);
        let c1 = self.iir_fir(out, &head[..fs_in]);

        // Second batch: `inLen - Fs_in_kHz` samples starting at `in[nSamples]`
        // (the final `input_delay` samples are held back for the next frame),
        // placed after the first batch's output (libopus hardcodes the
        // `Fs_out_kHz` offset, since the first batch always yields exactly that).
        let c2 = if in_len > fs_in {
            self.iir_fir(
                &mut out[fs_out..],
                &input[n_samples..in_len - self.input_delay],
            )
        } else {
            0
        };

        // Save the tail into the delay buffer for the next frame.
        if self.input_delay > 0 {
            self.delay_buf[..self.input_delay]
                .copy_from_slice(&input[in_len - self.input_delay..in_len]);
        }
        c1 + c2
    }

    /// `silk_resampler_private_IIR_FIR`: 2x HQ upsample of `input` followed by
    /// the 12-phase FIR interpolation, carrying `s_iir`/`s_fir_i16` across calls.
    /// Returns the number of output samples written to `out`.
    fn iir_fir(&mut self, out: &mut [i16], input: &[i16]) -> usize {
        let mut buf = [0i16; MAX_BUF];
        // Prime with the saved FIR history.
        buf[..ORDER_FIR_12].copy_from_slice(&self.s_fir_i16);

        let increment = self.inv_ratio_q16;
        let mut in_off = 0usize;
        let mut in_len = input.len();
        let mut out_off = 0usize;
        loop {
            let n_in = in_len.min(self.batch_size);
            let n2 = n_in << 1;
            // Upsample 2x into buf[ORDER_FIR_12 .. ORDER_FIR_12 + 2*n_in].
            up2_hq(
                &mut self.s_iir,
                &mut buf[ORDER_FIR_12..ORDER_FIR_12 + n2],
                &input[in_off..in_off + n_in],
            );
            let max_index_q16 = lshift(n_in as i32, 17); // + 1 for the 2x upsample
            out_off += interpolate(&mut out[out_off..], &buf, max_index_q16, increment);
            in_off += n_in;
            in_len -= n_in;
            if in_len > 0 {
                // Carry the last ORDER_FIR_12 filtered samples to the front.
                buf.copy_within(n2..n2 + ORDER_FIR_12, 0);
            } else {
                // Save the FIR state (last ORDER_FIR_12 filtered samples) for
                // the next call.
                self.s_fir_i16.copy_from_slice(&buf[n2..n2 + ORDER_FIR_12]);
                break;
            }
        }
        out_off
    }
}

/// `silk_resampler_private_IIR_FIR_INTERPOL`: interpolate the 2x-upsampled
/// signal in `buf` at the Q16 rate `increment`, writing to `out`. Returns the
/// number of samples produced.
fn interpolate(out: &mut [i16], buf: &[i16], max_index_q16: i32, increment: i32) -> usize {
    let f = &RESAMPLER_FRAC_FIR_12;
    let mut count = 0usize;
    let mut index_q16 = 0i32;
    while index_q16 < max_index_q16 {
        if count >= out.len() {
            break;
        }
        let ti = (smulwb(index_q16 & 0xFFFF, 12) as usize).min(11);
        let tj = 11 - ti;
        let bp = (index_q16 >> 16) as usize;
        if bp + 7 >= buf.len() {
            break;
        }
        let b = &buf[bp..bp + 8];
        let mut res = smulbb(i32::from(b[0]), i32::from(f[ti][0]));
        res = smlabb(res, i32::from(b[1]), i32::from(f[ti][1]));
        res = smlabb(res, i32::from(b[2]), i32::from(f[ti][2]));
        res = smlabb(res, i32::from(b[3]), i32::from(f[ti][3]));
        res = smlabb(res, i32::from(b[4]), i32::from(f[tj][3]));
        res = smlabb(res, i32::from(b[5]), i32::from(f[tj][2]));
        res = smlabb(res, i32::from(b[6]), i32::from(f[tj][1]));
        res = smlabb(res, i32::from(b[7]), i32::from(f[tj][0]));
        out[count] = sat16(rshift_round(res, 15)) as i16;
        count += 1;
        index_q16 += increment;
    }
    count
}

/// `silk_resampler_private_up2_HQ`: upsample `input` by 2x with the two 3-stage
/// 2nd-order allpass filter banks (state in Q10), writing `2 * input.len()`
/// samples to `out`.
fn up2_hq(s: &mut [i32; 6], out: &mut [i16], input: &[i16]) {
    let c0 = &RESAMPLER_UP2_HQ_0;
    let c1 = &RESAMPLER_UP2_HQ_1;
    for (k, &sample) in input.iter().enumerate() {
        let in32 = lshift(i32::from(sample), 10);

        // Even output sample: three allpass sections.
        let mut y = sub32(in32, s[0]);
        let mut x = smulwb(y, i32::from(c0[0]));
        let mut out32_1 = add32(s[0], x);
        s[0] = add32(in32, x);

        y = sub32(out32_1, s[1]);
        x = smulwb(y, i32::from(c0[1]));
        let mut out32_2 = add32(s[1], x);
        s[1] = add32(out32_1, x);

        y = sub32(out32_2, s[2]);
        x = smlawb(y, y, i32::from(c0[2]));
        out32_1 = add32(s[2], x);
        s[2] = add32(out32_2, x);

        out[2 * k] = sat16(rshift_round(out32_1, 10)) as i16;

        // Odd output sample.
        y = sub32(in32, s[3]);
        x = smulwb(y, i32::from(c1[0]));
        out32_1 = add32(s[3], x);
        s[3] = add32(in32, x);

        y = sub32(out32_1, s[4]);
        x = smulwb(y, i32::from(c1[1]));
        out32_2 = add32(s[4], x);
        s[4] = add32(out32_1, x);

        y = sub32(out32_2, s[5]);
        x = smlawb(y, y, i32::from(c1[2]));
        out32_1 = add32(s[5], x);
        s[5] = add32(out32_2, x);

        out[2 * k + 1] = sat16(rshift_round(out32_1, 10)) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invratio_matches_libopus_for_each_internal_rate() {
        // Golden invRatio_Q16 from silk_resampler_init (up2x = 1), computed by
        // hand from the libopus formula + round-up loop.
        assert_eq!(Resampler::init(16).inv_ratio_q16, 43691);
        assert_eq!(Resampler::init(12).inv_ratio_q16, 32768);
        assert_eq!(Resampler::init(8).inv_ratio_q16, 21846);
        assert_eq!(Resampler::init(16).input_delay, 7);
        assert_eq!(Resampler::init(12).input_delay, 4);
        assert_eq!(Resampler::init(8).input_delay, 0);
    }

    #[test]
    fn upsamples_by_the_expected_ratio() {
        // 16 kHz → 48 kHz: one 20 ms frame (320 samples) → 960 output samples.
        let mut r = Resampler::init(16);
        let input = [0i16; 320];
        let mut out = [0i16; 960];
        let n = r.process(&mut out, &input);
        assert_eq!(n, 960);
        // Silence in → silence out (state starts zeroed).
        assert!(out.iter().all(|&s| s == 0));
    }

    #[test]
    fn a_dc_step_produces_bounded_continuous_output() {
        // A constant input must resample to a bounded, eventually-constant
        // output (no delay-line blow-up / clicks).
        let mut r = Resampler::init(16);
        let input = [1000i16; 320];
        let mut out = [0i16; 960];
        let n = r.process(&mut out, &input);
        assert_eq!(n, 960);
        // The tail (past the filter warm-up) settles near the DC input level.
        let tail = &out[600..960];
        assert!(tail.iter().all(|&s| (800..=1200).contains(&s)));
    }
}
