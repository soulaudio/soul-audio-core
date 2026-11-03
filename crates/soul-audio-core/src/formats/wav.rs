//! WAV decoder = [`WavDemuxer`](crate::demux::WavDemuxer) (container) + PCM
//! decoding (codec). Streams interleaved `i32` PCM.
//!
//! Integer PCM (8/16/24/32-bit) is widened preserving its true depth. IEEE
//! **float** PCM (32/64-bit) is converted to full-scale 32-bit integer PCM
//! (`clamp(f, ±1) × 2³¹`) so consumers always receive uniform integer samples;
//! [`stream_info`](Decoder::stream_info) then reports `Int { bits: 32 }`.

use crate::decoder::{DecodeError, Decoder, secs_to_samples};
use crate::demux::Demuxer;
use crate::demux::WavDemuxer;
use crate::demux::wav::Companding;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// Largest IMA-ADPCM compressed block we buffer (covers the common 256–2048 byte
/// block sizes). A mono 2048-byte block decodes to `1 + (2048-4)*2 = 4089`
/// samples; the PCM buffer is rounded up.
const MAX_ADPCM_BLOCK: usize = 2048;
const MAX_ADPCM_PCM: usize = 4096;
const MAX_ADPCM_CHANNELS: usize = 2;

/// IMA-ADPCM step-size table (89 entries) and index-adjust table (16 entries).
#[rustfmt::skip]
const IMA_STEP: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45,
    50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230,
    253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963,
    1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327,
    3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442,
    11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
];
#[rustfmt::skip]
const IMA_INDEX: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];

/// Decode one IMA-ADPCM nibble, advancing the channel's predictor + step index.
/// Uses the `((2·delta+1)·step) >> 3` diff (as FFmpeg's `adpcm_ima`), not the
/// bit-by-bit accumulation — they differ by a rounding LSB.
fn ima_nibble(pred: &mut i32, idx: &mut usize, nib: u8) -> i32 {
    let step = IMA_STEP[*idx];
    let delta = i32::from(nib & 7);
    let diff = ((2 * delta + 1) * step) >> 3;
    if nib & 8 != 0 {
        *pred -= diff;
    } else {
        *pred += diff;
    }
    *pred = (*pred).clamp(-32768, 32767);
    *idx = (*idx as i32 + IMA_INDEX[nib as usize]).clamp(0, 88) as usize;
    *pred
}

/// Decode one IMA-ADPCM WAV block into interleaved `i32` PCM in `out`. Returns
/// the number of interleaved samples written (whole frames only). Each channel
/// begins with a 4-byte header (predictor:i16, step index:u8, reserved); the
/// remaining data is 4-byte (8-nibble) words round-robin per channel.
fn decode_ima_block(block: &[u8], channels: usize, spb: usize, out: &mut [i32]) -> usize {
    let mut pred = [0i32; MAX_ADPCM_CHANNELS];
    let mut idx = [0usize; MAX_ADPCM_CHANNELS];
    let mut done = [0usize; MAX_ADPCM_CHANNELS];
    let mut hp = 0;
    for c in 0..channels {
        if hp + 4 > block.len() {
            return 0;
        }
        pred[c] = i32::from(i16::from_le_bytes([block[hp], block[hp + 1]]));
        idx[c] = usize::from(block[hp + 2]).min(88);
        out[c] = pred[c]; // the header sample
        done[c] = 1;
        hp += 4;
    }
    let data = &block[channels * 4..];
    let mut dp = 0;
    'outer: loop {
        let mut progressed = false;
        for c in 0..channels {
            if done[c] >= spb {
                continue;
            }
            progressed = true;
            for _ in 0..4 {
                if dp >= data.len() {
                    break 'outer;
                }
                let byte = data[dp];
                dp += 1;
                for nib in [byte & 0x0F, byte >> 4] {
                    if done[c] < spb {
                        out[done[c] * channels + c] = ima_nibble(&mut pred[c], &mut idx[c], nib);
                        done[c] += 1;
                    }
                }
            }
        }
        if !progressed {
            break;
        }
    }
    // Emit only whole interleaved frames.
    done[..channels].iter().copied().min().unwrap_or(0) * channels
}

/// IMA-ADPCM streaming state: a compressed-block scratch and the decoded PCM.
struct Adpcm {
    block_align: usize,
    samples_per_block: usize, // per channel
    channels: usize,
    block: [u8; MAX_ADPCM_BLOCK],
    pcm: [i32; MAX_ADPCM_PCM],
    pcm_len: usize,
    pcm_pos: usize,
}

/// WAV decoder over a byte [`Source`]. Handles integer PCM (8/16/24/32-bit),
/// IEEE float PCM (32/64-bit), G.711 A-law/µ-law (8-bit → 16-bit), and
/// IMA/DVI-ADPCM (4-bit → 16-bit).
pub struct WavDecoder<S> {
    demux: WavDemuxer<S>,
    bits: u8,
    float: bool,
    companding: Option<Companding>,
    big_endian: bool,
    adpcm: Option<Adpcm>,
    channels: u16,
    sample_pos: u64, // interleaved samples produced since the start
}

impl<S: Source> WavDecoder<S> {
    /// Parse the WAV header and prepare to stream PCM.
    ///
    /// # Errors
    /// Propagates [`WavDemuxer::new`] errors; [`DecodeError::Unsupported`] for an
    /// unsupported bit depth.
    pub fn new(src: S) -> Result<Self, DecodeError> {
        let demux = WavDemuxer::new(src)?;
        let info = demux.stream_info();
        if info.channels == 0 {
            return Err(DecodeError::Unsupported);
        }
        let companding = demux.companding();

        // IMA-ADPCM: block-based 4-bit → 16-bit.
        let adpcm = if let Some(spb) = demux.adpcm_samples_per_block() {
            let block_align = demux.block_align() as usize;
            let channels = info.channels as usize;
            if block_align == 0
                || block_align > MAX_ADPCM_BLOCK
                || channels > MAX_ADPCM_CHANNELS
                || spb as usize > MAX_ADPCM_PCM / channels
            {
                return Err(DecodeError::Unsupported);
            }
            Some(Adpcm {
                block_align,
                samples_per_block: spb as usize,
                channels,
                block: [0; MAX_ADPCM_BLOCK],
                pcm: [0; MAX_ADPCM_PCM],
                pcm_len: 0,
                pcm_pos: 0,
            })
        } else {
            None
        };

        // Companded samples are 1 byte in, 16-bit out; ADPCM is 16-bit out.
        let (bits, float) = if companding.is_some() || adpcm.is_some() {
            (8, false)
        } else {
            match info.format {
                SampleFormat::Int { bits } => (bits, false),
                SampleFormat::Float { bits } => (bits, true),
                // DSD never reaches this codec.
                SampleFormat::Dsd { .. } => return Err(DecodeError::Unsupported),
            }
        };
        if adpcm.is_none() {
            let valid = if float {
                bits == 32 || bits == 64
            } else {
                bits != 0 && bits % 8 == 0 && bits <= 32
            };
            if !valid {
                return Err(DecodeError::Unsupported);
            }
        }
        let big_endian = demux.big_endian();
        Ok(Self {
            demux,
            bits,
            float,
            companding,
            big_endian,
            adpcm,
            channels: info.channels,
            sample_pos: 0,
        })
    }

    /// Stream IMA-ADPCM: refill from a decoded block, emit interleaved `i32`.
    fn decode_adpcm(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let Self {
            demux,
            adpcm,
            sample_pos,
            ..
        } = self;
        let a = adpcm.as_mut().expect("adpcm state");
        let mut w = 0;
        while w < out.len() {
            if a.pcm_pos >= a.pcm_len {
                // Read one compressed block (short read at EOF is fine).
                let ba = a.block_align;
                let mut n = 0;
                while n < ba {
                    let k = demux.read_data(&mut a.block[n..ba])?;
                    if k == 0 {
                        break;
                    }
                    n += k;
                }
                if n < 4 * a.channels {
                    break; // no full block header → end of stream
                }
                a.pcm_len =
                    decode_ima_block(&a.block[..n], a.channels, a.samples_per_block, &mut a.pcm);
                a.pcm_pos = 0;
                if a.pcm_len == 0 {
                    break;
                }
            }
            out[w] = a.pcm[a.pcm_pos];
            a.pcm_pos += 1;
            w += 1;
        }
        *sample_pos += w as u64;
        Ok(w)
    }

    /// Current playback position, in interleaved frames from the start.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.sample_pos / u64::from(self.channels)
    }
}

impl<S: SeekSource> WavDecoder<S> {
    /// Seek to interleaved `frame`. Returns the frame landed on.
    ///
    /// # Errors
    /// [`DecodeError::Io`] if the seek fails.
    pub fn seek_to(&mut self, frame: u64) -> Result<u64, DecodeError> {
        let landed = self.demux.seek_to(frame)?;
        self.sample_pos = landed * u64::from(self.channels);
        Ok(landed)
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to`]). Returns the
    /// frame landed on.
    ///
    /// # Errors
    /// [`DecodeError::Io`] if the seek fails.
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        self.seek_to(secs_to_samples(secs, self.demux.stream_info().sample_rate))
    }
}

impl<S: Source> WavDecoder<S> {
    /// Total per-channel frames, if the `data` chunk size was known.
    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        self.demux.frames_total()
    }
}

impl<S: Source> Decoder for WavDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        let si = self.demux.stream_info();
        // Float is converted to full-scale 32-bit integer PCM on output.
        if self.float {
            StreamInfo {
                format: SampleFormat::Int { bits: 32 },
                ..si
            }
        } else {
            si
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        if self.adpcm.is_some() {
            return self.decode_adpcm(out);
        }
        let bps = usize::from(self.bits) / 8;
        // Batch buffer: 768 is divisible by 1..4 and 8 (every sample width).
        let mut buf = [0u8; 768];
        let mut produced = 0;
        while produced < out.len() {
            let cap = (out.len() - produced).min(buf.len() / bps) * bps;
            if cap == 0 {
                break;
            }
            let n = self.demux.read_data(&mut buf[..cap])?;
            if n == 0 {
                break;
            }
            let samples = n / bps;
            convert_samples(
                &buf[..n],
                bps,
                self.bits,
                self.float,
                self.companding,
                self.big_endian,
                &mut out[produced..produced + samples],
            );
            produced += samples;
            if n < cap {
                break; // end of data (or a short final read)
            }
        }
        self.sample_pos += produced as u64;
        Ok(produced)
    }
}

/// Convert `buf.len() / bps` interleaved raw PCM samples into `i32` at
/// `out[..samples]`, applying bit-width widening, IEEE-float scaling, or G.711
/// companding. `big_endian` reverses each multi-byte sample to the little-endian
/// order the widen/float helpers expect (AIFF and RIFX store samples big-endian;
/// WAV `sowt`/canonical are little-endian). Shared by [`WavDecoder`] and
/// [`crate::formats::aiff::AiffDecoder`] — the one place PCM sample bytes become
/// `i32`. Returns the number of samples written.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_samples(
    buf: &[u8],
    bps: usize,
    bits: u8,
    float: bool,
    companding: Option<Companding>,
    big_endian: bool,
    out: &mut [i32],
) -> usize {
    let samples = (buf.len() / bps).min(out.len());
    for (i, slot) in out[..samples].iter_mut().enumerate() {
        let raw = &buf[i * bps..i * bps + bps];
        let mut swapped = [0u8; 8];
        let sample = if big_endian && bps > 1 {
            for (d, s) in swapped[..bps].iter_mut().zip(raw.iter().rev()) {
                *d = *s;
            }
            &swapped[..bps]
        } else {
            raw
        };
        *slot = match companding {
            Some(Companding::ALaw) => i32::from(alaw_to_i16(sample[0])),
            Some(Companding::MuLaw) => i32::from(mulaw_to_i16(sample[0])),
            None if float => float_to_i32(sample, bits),
            None => widen(sample, bits),
        };
    }
    samples
}

/// Widen one little-endian integer PCM sample to `i32`, preserving true depth
/// (values are not rescaled). 8-bit PCM is unsigned; 16/24/32-bit are signed
/// two's-complement.
fn widen(bytes: &[u8], bits: u8) -> i32 {
    match bits {
        8 => i32::from(bytes[0]) - 128,
        16 => i32::from(i16::from_le_bytes([bytes[0], bytes[1]])),
        24 => {
            let raw =
                u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16);
            ((raw << 8) as i32) >> 8 // sign-extend from bit 23
        }
        _ => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
    }
}

/// G.711 µ-law → 16-bit PCM (ITU-T; the classic sox `ulaw2linear`).
fn mulaw_to_i16(mu: u8) -> i16 {
    const BIAS: i32 = 0x84;
    let u = !mu;
    let mut t = (i32::from(u & 0x0F) << 3) + BIAS;
    t <<= (u & 0x70) >> 4;
    (if u & 0x80 != 0 { BIAS - t } else { t - BIAS }) as i16
}

/// G.711 A-law → 16-bit PCM (ITU-T; the classic sox `alaw2linear`).
fn alaw_to_i16(a: u8) -> i16 {
    let a = a ^ 0x55;
    let mut t = i32::from(a & 0x0F) << 4;
    let seg = (a & 0x70) >> 4;
    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }
    (if a & 0x80 != 0 { t } else { -t }) as i16
}

/// Convert one little-endian IEEE float sample (32- or 64-bit) to full-scale
/// 32-bit integer PCM: `clamp(f, ±1) × (2³¹ − 1)`, rounded half away from zero.
/// `no_std`-safe — no `libm`, no `f64::clamp`. NaN maps to 0 (saturating cast).
fn float_to_i32(bytes: &[u8], bits: u8) -> i32 {
    let f: f64 = if bits == 64 {
        f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    } else {
        f64::from(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    // `f64::clamp` is in `core` (comparisons only, no `libm`). NaN → NaN → 0.
    let scaled = f.clamp(-1.0, 1.0) * 2_147_483_647.0;
    let rounded = scaled + if scaled >= 0.0 { 0.5 } else { -0.5 };
    rounded as i32 // saturating float→int cast
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    fn wav(bits: u16, data: &[u8]) -> Vec<u8> {
        let channels = 1u16;
        let block_align = channels * (bits / 8);
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&44_100u32.to_le_bytes());
        v.extend_from_slice(&(44_100 * u32::from(block_align)).to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    /// Build a mono IEEE-float WAV (`bits` = 32 or 64, format tag 3).
    fn wav_float(bits: u16, data: &[u8]) -> Vec<u8> {
        let channels = 1u16;
        let block_align = channels * (bits / 8);
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&3u16.to_le_bytes()); // WAVE_FORMAT_IEEE_FLOAT
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&44_100u32.to_le_bytes());
        v.extend_from_slice(&(44_100 * u32::from(block_align)).to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    /// Build a mono 8-bit G.711 WAV (`tag` = 6 A-law, 7 µ-law).
    fn wav_companded(tag: u16, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&8000u32.to_le_bytes());
        v.extend_from_slice(&8000u32.to_le_bytes()); // byte rate
        v.extend_from_slice(&1u16.to_le_bytes()); // block align
        v.extend_from_slice(&8u16.to_le_bytes()); // bits
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    /// FNV-1a/64 over interleaved `i32` samples.
    fn fnv_i32(s: &[i32]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &v in s {
            h ^= u64::from(v as u32);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h
    }

    fn decode_all_wav(bytes: &[u8]) -> Vec<i32> {
        let mut dec = WavDecoder::new(SliceSource::new(bytes)).unwrap();
        let mut out = Vec::new();
        let mut buf = [0i32; 512];
        loop {
            let n = dec.decode_into(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[test]
    fn decodes_ima_adpcm_mono_matches_ffmpeg() {
        // Real ffmpeg-encoded IMA-ADPCM; reference = ffmpeg's own decode.
        let wav = include_bytes!("../../testdata/ima-adpcm-mono.wav");
        let out = decode_all_wav(wav);
        assert_eq!(out.len(), 4082);
        assert_eq!(fnv_i32(&out), 0x341c_4128_0cc6_a908);
    }

    #[test]
    fn decodes_ima_adpcm_stereo_matches_ffmpeg() {
        let wav = include_bytes!("../../testdata/ima-adpcm-stereo.wav");
        let out = decode_all_wav(wav);
        assert_eq!(out.len(), 4068);
        assert_eq!(fnv_i32(&out), 0x53c0_9443_dcaf_18a2);
    }

    #[test]
    fn decodes_rifx_big_endian() {
        // RIFX: magic, chunk sizes, fmt fields, and PCM samples are big-endian.
        let samples = [100i16, -100, 32767, -32768];
        let mut data = Vec::new();
        for s in samples {
            data.extend_from_slice(&s.to_be_bytes());
        }
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFX");
        v.extend_from_slice(&(36u32 + data.len() as u32).to_be_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_be_bytes());
        v.extend_from_slice(&1u16.to_be_bytes()); // PCM
        v.extend_from_slice(&1u16.to_be_bytes()); // mono
        v.extend_from_slice(&44_100u32.to_be_bytes());
        v.extend_from_slice(&88_200u32.to_be_bytes());
        v.extend_from_slice(&2u16.to_be_bytes());
        v.extend_from_slice(&16u16.to_be_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(&data);

        let mut dec = WavDecoder::new(SliceSource::new(&v)).unwrap();
        assert_eq!(dec.stream_info().sample_rate, 44_100);
        assert_eq!(dec.stream_info().channels, 1);
        let mut out = [0i32; 4];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 4);
        assert_eq!(out, [100, -100, 32767, -32768]);
    }

    #[test]
    fn decodes_g711_mulaw() {
        // ITU-T G.711 µ-law reference values.
        let bytes = wav_companded(7, &[0x00, 0xFF, 0x80, 0x7F]);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(dec.stream_info().format, SampleFormat::Int { bits: 16 });
        let mut out = [0i32; 4];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 4);
        assert_eq!(out, [-32124, 0, 32124, 0]);
    }

    #[test]
    fn decodes_g711_alaw() {
        // ITU-T G.711 A-law reference values (± full-scale 32256, min step ±8).
        let bytes = wav_companded(6, &[0xD5, 0x55, 0xAA, 0x2A]);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(dec.stream_info().format, SampleFormat::Int { bits: 16 });
        let mut out = [0i32; 4];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 4);
        assert_eq!(out, [8, -8, 32256, -32256]);
    }

    #[test]
    fn seek_to_time_converts_seconds() {
        // 200-frame mono 16-bit ramp at 44_100 Hz: sample[i] = i.
        let mut data = Vec::new();
        for i in 0..200i16 {
            data.extend_from_slice(&i.to_le_bytes());
        }
        let bytes = wav(16, &data);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        // 0.002 s × 44_100 = 88.2 → rounds to frame 88.
        assert_eq!(dec.seek_to_time(0.002).unwrap(), 88);
        let mut out = [0i32; 1];
        dec.decode_into(&mut out).unwrap();
        assert_eq!(out[0], 88);
        // Negative time clamps to 0.
        assert_eq!(dec.seek_to_time(-1.0).unwrap(), 0);
    }

    #[test]
    fn decodes_6_channel_pcm() {
        // 5.1 (6-channel) 16-bit WAV: sample[frame][ch] = frame*10 + ch.
        let channels = 6u16;
        let frames = 4usize;
        let block_align = channels * 2;
        let mut data = Vec::new();
        let mut expected = Vec::new();
        for f in 0..frames {
            for c in 0..channels {
                let s = (f as i16) * 10 + c as i16;
                data.extend_from_slice(&s.to_le_bytes());
                expected.push(i32::from(s));
            }
        }
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&44_100u32.to_le_bytes());
        v.extend_from_slice(&(44_100 * u32::from(block_align)).to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);

        let mut dec = WavDecoder::new(SliceSource::new(&v)).unwrap();
        assert_eq!(dec.stream_info().channels, 6);
        let mut out = [0i32; 32];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &expected[..]);
    }

    #[test]
    fn decodes_f32_to_full_scale_i32() {
        // 0.0, +1.0, -1.0, 0.5, -0.5, and out-of-range values (clamped).
        let samples = [0.0f32, 1.0, -1.0, 0.5, -0.5, 2.0, -3.0];
        let mut data = Vec::new();
        for s in samples {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let bytes = wav_float(32, &data);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(dec.stream_info().format, SampleFormat::Int { bits: 32 });
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 7);
        const MAX: i32 = 2_147_483_647;
        assert_eq!(out[0], 0);
        assert_eq!(out[1], MAX); // +1.0
        assert_eq!(out[2], -MAX); // -1.0 → -2147483647 (symmetric)
        assert_eq!(out[3], 1_073_741_824); // 0.5 * MAX rounded
        assert_eq!(out[4], -1_073_741_824);
        assert_eq!(out[5], MAX); // 2.0 clamped to +1.0
        assert_eq!(out[6], -MAX); // -3.0 clamped to -1.0
    }

    #[test]
    fn decodes_f64_float() {
        let samples = [0.0f64, 1.0, -0.25];
        let mut data = Vec::new();
        for s in samples {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let bytes = wav_float(64, &data);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(dec.stream_info().format, SampleFormat::Int { bits: 32 });
        let mut out = [0i32; 4];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 3);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 2_147_483_647);
        assert_eq!(out[2], -536_870_912); // -0.25 * MAX rounded
    }

    #[test]
    fn decodes_16bit_pcm_including_negatives() {
        // samples: 0, 1, -1, 32767, -32768
        let mut data = Vec::new();
        for s in [0i16, 1, -1, 32767, -32768] {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let bytes = wav(16, &data);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&out[..5], &[0, 1, -1, 32767, -32768]);
        assert_eq!(dec.decode_into(&mut out).unwrap(), 0); // EOF
    }

    #[test]
    fn decodes_24bit_pcm_sign_extended() {
        // 0x000000 = 0, 0x7FFFFF = max+, 0x800000 = max- (-8388608), 0xFFFFFF = -1
        let data = [
            0x00, 0x00, 0x00, // 0
            0xFF, 0xFF, 0x7F, // 8388607
            0x00, 0x00, 0x80, // -8388608
            0xFF, 0xFF, 0xFF, // -1
        ];
        let bytes = wav(24, &data);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 4];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 4);
        assert_eq!(out, [0, 8_388_607, -8_388_608, -1]);
    }

    #[test]
    fn decodes_8bit_unsigned_to_signed() {
        let data = [128u8, 129, 127, 255, 0]; // → 0, 1, -1, 127, -128
        let bytes = wav(8, &data);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 5];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 5);
        assert_eq!(out, [0, 1, -1, 127, -128]);
    }

    #[test]
    fn decode_after_seek_lands_on_the_right_frame() {
        let mut data = Vec::new();
        for s in 0i16..10 {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let bytes = wav(16, &data); // mono 16-bit, frames 0..9
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(dec.seek_to(5).unwrap(), 5);
        let mut out = [0i32; 3];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &[5, 6, 7]);
    }

    #[test]
    fn decode_drains_across_multiple_calls() {
        let mut data = Vec::new();
        for s in 0i16..100 {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let bytes = wav(16, &data);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 30];
        let mut total = 0;
        loop {
            let n = dec.decode_into(&mut out).unwrap();
            if n == 0 {
                break;
            }
            for (i, &s) in out[..n].iter().enumerate() {
                assert_eq!(s, (total + i) as i32);
            }
            total += n;
        }
        assert_eq!(total, 100);
    }
}
