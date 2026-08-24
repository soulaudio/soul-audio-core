//! DSD → PCM adapters. Native SAI and DoP both need the raw 1-bit stream; only
//! decimation makes real PCM. The output *mode* is chosen by constructing the
//! matching adapter — there is no runtime flag.
//!
//! - [`DopAdapter`] wraps a [`DsdDecoder`] as a [`Decoder`] by DoP-framing (still
//!   DSD, for a DoP-capable DAC).
//! - [`DecimateAdapter`] wraps a [`DsdDecoder`] as a [`Decoder`] by decimating to
//!   real PCM (plays anywhere) via a table-driven low-pass FIR (8:1).
//!
//! For [`DsdMode::Native`](crate::model::DsdMode) use the [`DsdDecoder`]
//! directly (no adapter).

use crate::decoder::{DecodeError, Decoder, DsdDecoder};
use crate::model::{SampleFormat, StreamInfo};

use crate::decoder::DsdSeek;

/// The two DoP marker bytes. They alternate **once per DoP frame** (not per
/// channel) so a DoP DAC recognises the stream; every channel of a frame carries
/// the same marker. Phase is `frame_index & 1`.
const DOP_MARKER: [u8; 2] = [0x05, 0xFA];

/// Maximum channels the DoP framer supports (group buffer = 2 bytes/channel).
const MAX_DOP_CHANNELS: usize = 8;

/// Wraps a [`DsdDecoder`] and presents it as a PCM [`Decoder`] using DoP.
///
/// Each DoP frame packs 16 DSD bits **per channel** into a 24-bit word
/// `marker<<16 | b_t0<<8 | b_t1` (earlier byte high), emitted as ordinary
/// interleaved PCM frames (`L,R,…`) at `DSD_rate/16`. The `0x05`/`0xFA` marker
/// toggles once per frame and is shared by all channels (phase `frame_index &
/// 1`). The 24-bit word occupies the low 24 bits of each `i32`; a 32-bit
/// transport shell must left-justify so the marker is the true MSByte.
pub struct DopAdapter<D> {
    inner: D,
    channels: usize,
    frame_index: u64,
    group: [u8; 2 * MAX_DOP_CHANNELS], // t0 row (C bytes) then t1 row (C bytes)
    ch_pos: usize,                     // next channel to emit within the current frame
}

impl<D: DsdDecoder> DopAdapter<D> {
    /// Wrap `inner`. The first frame uses marker `0x05`.
    pub fn new(inner: D) -> Self {
        let channels = inner.stream_info().channels as usize;
        Self {
            inner,
            channels,
            frame_index: 0,
            group: [0u8; 2 * MAX_DOP_CHANNELS],
            ch_pos: 0,
        }
    }

    /// Read the next DoP frame's `2*C` DSD bytes (t0 row then t1 row) into
    /// `self.group`. Returns `false` at end of stream (an incomplete trailing
    /// group is dropped).
    fn load_group(&mut self) -> Result<bool, DecodeError> {
        let need = 2 * self.channels;
        let mut filled = 0;
        while filled < need {
            let k = self.inner.read_dsd(&mut self.group[filled..need])?;
            if k == 0 {
                break;
            }
            filled += k;
        }
        Ok(filled == need)
    }
}

impl<D: DsdDecoder + DsdSeek> DopAdapter<D> {
    /// Seek to output DoP `frame` (random access). Re-derives the marker phase
    /// from the landed position so the alternation stays unbroken.
    ///
    /// # Errors
    /// Propagates the inner [`DsdSeek::seek_to_sample`] error.
    pub fn seek_to_frame(&mut self, frame: u64) -> Result<u64, DecodeError> {
        // One DoP frame carries 16 DSD bits (2 bytes) per channel.
        let landed = self.inner.seek_to_sample(frame * 16)?;
        self.frame_index = landed / 16;
        self.ch_pos = 0; // drop any partial frame
        Ok(self.frame_index)
    }
}

impl<D: DsdDecoder> Decoder for DopAdapter<D> {
    fn stream_info(&self) -> StreamInfo {
        let si = self.inner.stream_info();
        // DoP carries 16 DSD bits per frame → frame rate = bit rate / 16.
        let frame_rate = match si.format {
            SampleFormat::Dsd { rate } => rate.hz() / 16,
            _ => si.sample_rate,
        };
        StreamInfo {
            sample_rate: frame_rate,
            channels: si.channels,
            format: SampleFormat::Int { bits: 24 },
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let c = self.channels;
        if c == 0 || c > MAX_DOP_CHANNELS {
            return Err(DecodeError::Unsupported);
        }
        let mut w = 0;
        while w < out.len() {
            // Load a fresh frame only at a frame boundary; a partial frame from a
            // previous call resumes from the persisted group.
            if self.ch_pos == 0 && !self.load_group()? {
                break; // end of stream
            }
            let marker = DOP_MARKER[(self.frame_index & 1) as usize];
            let b_t0 = self.group[self.ch_pos];
            let b_t1 = self.group[c + self.ch_pos];
            out[w] = (i32::from(marker) << 16) | (i32::from(b_t0) << 8) | i32::from(b_t1);
            w += 1;
            self.ch_pos += 1;
            if self.ch_pos == c {
                self.ch_pos = 0;
                self.frame_index += 1;
            }
        }
        Ok(w)
    }
}

/// Fixed decimation ratio: one PCM sample per input **byte** (8 DSD bits → 1
/// sample). DSD64 (2.8224 MHz) → 352.8 kHz; DSD128 → 705.6 kHz; DSD256 → 1.4112
/// MHz; DSD512 → 2.8224 MHz.
const DECIMATION: u32 = 8;

/// FIR length in taps (a multiple of 8): a 64-tap symmetric Blackman-windowed
/// sinc low-pass with cutoff at the output Nyquist (`f_in / 16`).
const TAPS: usize = 64;

/// Byte-groups spanning the filter — one history byte (8 bits) per group.
const GROUPS: usize = TAPS / 8;

/// Maximum channels the decimator supports (fixed per-channel history state).
const MAX_DECIMATE_CHANNELS: usize = 8;

/// 24-bit signed PCM full-scale bounds; FIR overshoot is saturated into range.
const PCM_MAX: i32 = (1 << 23) - 1;
const PCM_MIN: i32 = -(1 << 23);

/// FIR taps, DC-normalised so the integer coefficients sum to exactly
/// [`PCM_MAX`]: a constant all-ones DSD input (every bit → `+1`) maps to
/// `+PCM_MAX`, all-zeros (every bit → `-1`) to `-PCM_MAX`. Symmetric (linear
/// phase). Generated from a Blackman-windowed sinc (cutoff `f_in/16`) rounded to
/// i32; the Nyquist (alternating-bit) response is ≈ 0, rejecting the DSD noise.
const COEFFS: [i32; TAPS] = [
    0, -44, -273, -764, -1439, -2035, -2100, -1081, 1527, 5962, 11956, 18542, 23990, 25951, 21863,
    9580, -11845, -41385, -75490, -107960, -130378, -133165, -107147, -45391, 55026, 191699,
    356375, 535444, 711441, 865394, 979612, 1040438, 1040439, 979612, 865394, 711441, 535444,
    356375, 191699, 55026, -45391, -107147, -133165, -130378, -107960, -75490, -41385, -11845,
    9580, 21863, 25951, 23990, 18542, 11956, 5962, 1527, -1081, -2100, -2035, -1439, -764, -273,
    -44, 0,
];

/// Per-(group, byte) partial-sum table, à la Gesemann `dsd2pcm` / FFmpeg
/// `dsd.c`: `TABLE[g][b]` is the FIR contribution of the 8 DSD bits packed in
/// byte `b` at tap-group `g`, so filtering a byte is `GROUPS` lookups + adds —
/// no per-bit branching at run time.
const TABLE: [[i32; 256]; GROUPS] = build_table();

/// Build [`TABLE`] at compile time from [`COEFFS`]. For tap-group `g` and byte
/// `b`, bit `p` (LSB = the newest sample within the byte) contributes
/// `±COEFFS[g*8 + p]`, `+` when the bit is set. Tap index `g*8 + p` equals the
/// sample's age (0 = newest), so this is a standard time-ordered FIR.
const fn build_table() -> [[i32; 256]; GROUPS] {
    let mut table = [[0i32; 256]; GROUPS];
    let mut g = 0;
    while g < GROUPS {
        let mut b = 0usize;
        while b < 256 {
            let mut acc = 0i32;
            let mut p = 0usize;
            while p < 8 {
                let coeff = COEFFS[g * 8 + p];
                if ((b >> p) & 1) == 1 {
                    acc += coeff;
                } else {
                    acc -= coeff;
                }
                p += 1;
            }
            table[g][b] = acc;
            b += 1;
        }
        g += 1;
    }
    table
}

/// Wraps a [`DsdDecoder`] and presents it as a PCM [`Decoder`] by decimating the
/// 1-bit stream to multi-bit PCM (plays on any DAC).
///
/// Runs a 64-tap table-driven low-pass FIR per channel and decimates 8:1 (one
/// 24-bit PCM sample per input byte), de-interleaving the clustered-frame byte
/// stream into per-channel filters and re-interleaving the PCM output (`L,R,…`).
/// Supports up to [`MAX_DECIMATE_CHANNELS`] channels; more are rejected as
/// [`DecodeError::Unsupported`].
pub struct DecimateAdapter<D> {
    inner: D,
    channels: usize,
    /// Ring of the last [`GROUPS`] input bytes per channel (newest at `ring_pos`).
    hist: [[u8; GROUPS]; MAX_DECIMATE_CHANNELS],
    /// Slot of the newest history byte; advances once per clustered frame.
    ring_pos: usize,
    /// Scratch for one clustered frame (one byte per channel).
    frame: [u8; MAX_DECIMATE_CHANNELS],
}

impl<D: DsdDecoder> DecimateAdapter<D> {
    /// Wrap `inner`. Per-channel history starts silent (zeroed).
    pub fn new(inner: D) -> Self {
        let channels = inner.stream_info().channels as usize;
        Self {
            inner,
            channels,
            hist: [[0u8; GROUPS]; MAX_DECIMATE_CHANNELS],
            ring_pos: GROUPS - 1, // first frame advances to slot 0
            frame: [0u8; MAX_DECIMATE_CHANNELS],
        }
    }

    /// Read one clustered frame (`channels` bytes, one per channel) into
    /// `self.frame`. Returns `false` at end of stream (a partial trailing frame
    /// is dropped).
    fn load_frame(&mut self) -> Result<bool, DecodeError> {
        let need = self.channels;
        let mut filled = 0;
        while filled < need {
            let k = self.inner.read_dsd(&mut self.frame[filled..need])?;
            if k == 0 {
                break;
            }
            filled += k;
        }
        Ok(filled == need)
    }
}

impl<D: DsdDecoder> Decoder for DecimateAdapter<D> {
    fn stream_info(&self) -> StreamInfo {
        let si = self.inner.stream_info();
        // Decimated PCM rate = 1-bit rate / ratio; fall back to the base rate for
        // an already-PCM inner (defensive — an inner DSD decoder reports Dsd).
        let sample_rate = match si.format {
            SampleFormat::Dsd { rate } => rate.hz() / DECIMATION,
            _ => si.sample_rate,
        };
        StreamInfo {
            sample_rate,
            channels: si.channels,
            format: SampleFormat::Int { bits: 24 },
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let c = self.channels;
        if c == 0 || c > MAX_DECIMATE_CHANNELS {
            return Err(DecodeError::Unsupported);
        }
        let mut w = 0;
        // Emit whole PCM frames only (one sample per channel per clustered frame).
        while w + c <= out.len() {
            if !self.load_frame()? {
                break; // end of stream
            }
            self.ring_pos = (self.ring_pos + 1) % GROUPS;
            let slot = self.ring_pos;
            let mut ch = 0;
            while ch < c {
                self.hist[ch][slot] = self.frame[ch];
                // FIR dot product: group g holds the byte of age g (0 = newest).
                let mut acc = 0i32;
                let mut g = 0;
                while g < GROUPS {
                    let idx = (slot + GROUPS - g) % GROUPS;
                    acc += TABLE[g][self.hist[ch][idx] as usize];
                    g += 1;
                }
                out[w] = acc.clamp(PCM_MIN, PCM_MAX);
                w += 1;
                ch += 1;
            }
        }
        Ok(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DsdRate, StreamInfo};

    /// A fake DSD decoder that hands out a fixed byte sequence.
    struct FakeDsd {
        bytes: &'static [u8],
        pos: usize,
        rate: DsdRate,
        channels: u16,
    }

    impl DsdDecoder for FakeDsd {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: 44_100,
                channels: self.channels,
                format: SampleFormat::Dsd { rate: self.rate },
            }
        }
        fn read_dsd(&mut self, out: &mut [u8]) -> Result<usize, DecodeError> {
            let rem = &self.bytes[self.pos..];
            let n = rem.len().min(out.len());
            out[..n].copy_from_slice(&rem[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl DsdSeek for FakeDsd {
        fn seek_to_sample(&mut self, s: u64) -> Result<u64, DecodeError> {
            // DoP frame f starts at byte f * 2 * channels in the interleaved stream.
            self.pos = ((s / 16) * 2 * u64::from(self.channels)) as usize;
            Ok(s)
        }
    }

    #[test]
    fn dop_mono_toggles_marker_per_frame() {
        let fake = FakeDsd {
            bytes: &[0xAA, 0xBB, 0xCC, 0xDD],
            pos: 0,
            rate: DsdRate::Dsd64,
            channels: 1,
        };
        let mut dop = DopAdapter::new(fake);
        let mut out = [0i32; 4];
        let n = dop.decode_into(&mut out).unwrap();
        assert_eq!(n, 2); // 4 DSD bytes → 2 mono frames
        assert_eq!(out[0], 0x0005_AABB); // frame 0, marker 0x05
        assert_eq!(out[1], 0x00FA_CCDD); // frame 1, marker 0xFA
    }

    #[test]
    fn dop_stereo_shares_marker_across_channels_and_deinterleaves() {
        // Interleaved input: [c0t0, c1t0, c0t1, c1t1] per frame.
        let fake = FakeDsd {
            bytes: &[0xA0, 0xB0, 0xA1, 0xB1, 0xC0, 0xD0, 0xC1, 0xD1],
            pos: 0,
            rate: DsdRate::Dsd64,
            channels: 2,
        };
        let mut dop = DopAdapter::new(fake);
        let mut out = [0i32; 4];
        let n = dop.decode_into(&mut out).unwrap();
        assert_eq!(n, 4); // 2 stereo frames = 4 DoP words (L,R,L,R)
        // Frame 0 (marker 0x05, shared by L and R): L uses ch0's t0/t1, R uses ch1's.
        assert_eq!(out[0], 0x0005_A0A1); // L
        assert_eq!(out[1], 0x0005_B0B1); // R — SAME marker as L
        // Frame 1 (marker toggled to 0xFA, again shared by L and R).
        assert_eq!(out[2], 0x00FA_C0C1); // L
        assert_eq!(out[3], 0x00FA_D0D1); // R
    }

    #[test]
    fn dop_stream_info_is_pcm_at_bitrate_over_16() {
        let fake = FakeDsd {
            bytes: &[],
            pos: 0,
            rate: DsdRate::Dsd64,
            channels: 2,
        };
        let dop = DopAdapter::new(fake);
        let si = dop.stream_info();
        assert_eq!(si.sample_rate, 2_822_400 / 16); // 176_400
        assert_eq!(si.channels, 2);
        assert_eq!(si.format, SampleFormat::Int { bits: 24 });
    }

    #[test]
    fn dop_seek_re_derives_marker_phase() {
        let fake = FakeDsd {
            bytes: &[0xA0, 0xB0, 0xA1, 0xB1, 0xC0, 0xD0, 0xC1, 0xD1],
            pos: 0,
            rate: DsdRate::Dsd64,
            channels: 2,
        };
        let mut dop = DopAdapter::new(fake);
        // Seek to DoP frame 1 (odd) → marker phase must be 0xFA, not 0x05.
        assert_eq!(dop.seek_to_frame(1).unwrap(), 1);
        let mut out = [0i32; 2];
        assert_eq!(dop.decode_into(&mut out).unwrap(), 2);
        assert_eq!(out[0], 0x00FA_C0C1); // L, marker 0xFA (re-derived from index)
        assert_eq!(out[1], 0x00FA_D0D1); // R, same marker
    }

    // ---- DecimateAdapter (DSD → PCM decimation FIR) ----

    /// Stereo clustered frames: ch0 = `0xFF` (all-ones), ch1 = `0x00` (all-zeros),
    /// interleaved `[c0, c1, c0, c1, …]` for 16 time indices (32 bytes).
    const STEREO_FF_00: [u8; 32] = {
        let mut a = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            a[i] = if i % 2 == 0 { 0xFF } else { 0x00 };
            i += 1;
        }
        a
    };

    fn decimate(bytes: &'static [u8], channels: u16, rate: DsdRate) -> DecimateAdapter<FakeDsd> {
        DecimateAdapter::new(FakeDsd {
            bytes,
            pos: 0,
            rate,
            channels,
        })
    }

    #[test]
    fn decimate_dc_all_ones_is_positive_full_scale() {
        // 16 input bytes → 16 PCM samples; history (8 bytes) settles by sample 8.
        let mut dec = decimate(&[0xFF; 16], 1, DsdRate::Dsd64);
        let mut out = [0i32; 16];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 16);
        // Once the filter is settled a constant +1 input maps to +full-scale.
        assert_eq!(out[15], PCM_MAX);
        assert!(
            out[8] > PCM_MAX * 9 / 10,
            "settled sample not near +FS: {}",
            out[8]
        );
    }

    #[test]
    fn decimate_dc_all_zeros_is_negative_full_scale() {
        let mut dec = decimate(&[0x00; 16], 1, DsdRate::Dsd64);
        let mut out = [0i32; 16];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 16);
        assert_eq!(out[15], -PCM_MAX);
        assert!(
            out[8] < -PCM_MAX * 9 / 10,
            "settled sample not near -FS: {}",
            out[8]
        );
    }

    #[test]
    fn decimate_rejects_nyquist_tone() {
        // 0x55 = 0b0101_0101 repeated → perfectly alternating bits = Nyquist,
        // which the low-pass rejects to ≈ 0.
        let mut dec = decimate(&[0x55; 32], 1, DsdRate::Dsd64);
        let mut out = [0i32; 32];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 32);
        // Settled magnitude is tiny compared with full-scale (~8.4M).
        assert!(out[31].abs() < 1_000, "Nyquist not rejected: {}", out[31]);
    }

    #[test]
    fn decimate_ratio_is_eight_to_one() {
        // N bytes/channel → N*8/ratio = N PCM frames (ratio = 8, 8 bits/byte).
        let mut dec = decimate(&[0xF0; 20], 1, DsdRate::Dsd64);
        let mut out = [0i32; 32];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 20);
    }

    #[test]
    fn decimate_stereo_separates_channels() {
        // ch0 all-ones → L positive; ch1 all-zeros → R negative.
        let mut dec = decimate(&STEREO_FF_00, 2, DsdRate::Dsd64);
        let mut out = [0i32; 32];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 32); // 16 stereo frames = 32 interleaved samples
        // Frame 15 (indices 30 = L, 31 = R) is fully settled.
        assert_eq!(out[30], PCM_MAX); // L = +full-scale
        assert_eq!(out[31], -PCM_MAX); // R = -full-scale
        // Every settled frame keeps L positive and R negative.
        for f in 8..16 {
            assert!(out[2 * f] > 0, "L not positive at frame {f}");
            assert!(out[2 * f + 1] < 0, "R not negative at frame {f}");
        }
    }

    #[test]
    fn decimate_stream_info_is_pcm_at_bitrate_over_ratio() {
        let dec = decimate(&[], 2, DsdRate::Dsd64);
        let si = dec.stream_info();
        assert_eq!(si.sample_rate, DsdRate::Dsd64.hz() / 8); // 2_822_400 / 8 = 352_800
        assert_eq!(si.sample_rate, 352_800);
        assert_eq!(si.channels, 2);
        assert_eq!(si.format, SampleFormat::Int { bits: 24 });
    }

    #[test]
    fn decimate_rejects_too_many_channels() {
        let mut dec = decimate(
            &[0xFF; 32],
            (MAX_DECIMATE_CHANNELS + 1) as u16,
            DsdRate::Dsd64,
        );
        let mut out = [0i32; 32];
        assert_eq!(
            dec.decode_into(&mut out).unwrap_err(),
            DecodeError::Unsupported
        );
    }
}
