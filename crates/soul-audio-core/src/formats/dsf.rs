//! DSF (Sony) DSD decoder — parses the container and normalizes audio to
//! **MSB-first, channel-interleaved** packed 1-bit DSD (the canonical form the
//! [`DopAdapter`](crate::dsd::DopAdapter) consumes).
//!
//! DSF stores audio **planar** (per-channel 4096-byte blocks) and **LSB-first**;
//! this reader transposes planar→interleaved and bit-reverses each byte
//! (LSB→MSB). All header fields are little-endian and chunk sizes *include* the
//! 12-byte chunk header.

use embedded_io::SeekFrom;

use super::MAX_DSD_CHANNELS;
use crate::decoder::{DecodeError, DsdDecoder, DsdSeek};
use crate::demux::read_exact;
use crate::io::{SeekSource, Source};
use crate::model::{DsdRate, SampleFormat, StreamInfo};

/// Fixed per-channel block size (bytes) mandated by the DSF spec.
const BLOCK: usize = 4096;

/// LSB↔MSB byte reversal table, built at compile time.
const BIT_REVERSE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let (mut b, mut r, mut k) = (i as u8, 0u8, 0);
        while k < 8 {
            r = (r << 1) | (b & 1);
            b >>= 1;
            k += 1;
        }
        t[i] = r;
        i += 1;
    }
    t
};

/// DSF decoder over a byte [`Source`], yielding normalized MSB-first DSD.
pub struct DsfDecoder<S> {
    src: S,
    info: StreamInfo,
    channels: usize,
    reverse: bool,      // bits-per-sample == 1 ⇒ audio is LSB-first, reverse it
    data_start: u64,    // absolute byte offset of the audio (fixed 92 for DSF)
    audio_total: u64,   // total audio bytes (all channels, all rows)
    total_samples: u64, // per-channel sample count (fmt `sample count`)
    audio_bytes_remaining: u64,
    valid_bytes_per_channel: u64, // trims trailing zero padding
    block: [u8; MAX_DSD_CHANNELS * BLOCK],
    row_len: usize, // valid time indices in the current block-row
    emit_i: usize,  // interleaved emit cursor within the current row
}

impl<S: Source> DsfDecoder<S> {
    /// Parse the DSF header.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a bad container, [`DecodeError::Unsupported`]
    /// for a non-DSD rate or more than [`MAX_DSD_CHANNELS`] channels.
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        // DSD chunk (28) + fmt chunk (52) = 80 bytes.
        let mut h = [0u8; 80];
        read_exact(&mut src, &mut h)?;
        if &h[0..4] != b"DSD " || &h[28..32] != b"fmt " {
            return Err(DecodeError::Malformed);
        }
        let channels = u32::from_le_bytes([h[52], h[53], h[54], h[55]]) as usize;
        let freq = u32::from_le_bytes([h[56], h[57], h[58], h[59]]);
        let bits = u32::from_le_bytes([h[60], h[61], h[62], h[63]]);
        let sample_count =
            u64::from_le_bytes([h[64], h[65], h[66], h[67], h[68], h[69], h[70], h[71]]);
        let block_size = u32::from_le_bytes([h[72], h[73], h[74], h[75]]) as usize;

        if channels == 0 || channels > MAX_DSD_CHANNELS || block_size != BLOCK {
            return Err(DecodeError::Unsupported);
        }
        let rate = dsd_rate(freq).ok_or(DecodeError::Unsupported)?;

        // data chunk header (12 bytes); audio size excludes the header.
        let mut dh = [0u8; 12];
        read_exact(&mut src, &mut dh)?;
        if &dh[0..4] != b"data" {
            return Err(DecodeError::Malformed);
        }
        let data_chunk =
            u64::from_le_bytes([dh[4], dh[5], dh[6], dh[7], dh[8], dh[9], dh[10], dh[11]]);
        let audio_bytes = data_chunk.saturating_sub(12);

        Ok(Self {
            src,
            info: StreamInfo {
                sample_rate: 44_100, // base-family rate; DsdRate carries the bit rate
                channels: channels as u16,
                format: SampleFormat::Dsd { rate },
            },
            channels,
            reverse: bits == 1,
            data_start: 80 + 12, // DSD(28) + fmt(52) header + data chunk header(12)
            audio_total: audio_bytes,
            total_samples: sample_count,
            audio_bytes_remaining: audio_bytes,
            valid_bytes_per_channel: sample_count.div_ceil(8),
            block: [0u8; MAX_DSD_CHANNELS * BLOCK],
            row_len: 0,
            emit_i: 0,
        })
    }

    /// Load the next block-row (`channels × 4096` bytes) into `self.block`.
    /// Returns `false` at end of audio.
    fn load_row(&mut self) -> Result<bool, DecodeError> {
        let row_bytes = self.channels * BLOCK;
        if self.audio_bytes_remaining < row_bytes as u64 || self.valid_bytes_per_channel == 0 {
            return Ok(false);
        }
        read_exact(&mut self.src, &mut self.block[..row_bytes])?;
        self.audio_bytes_remaining -= row_bytes as u64;
        self.row_len = self.valid_bytes_per_channel.min(BLOCK as u64) as usize;
        self.valid_bytes_per_channel -= self.row_len as u64;
        self.emit_i = 0;
        Ok(true)
    }
}

impl<S: Source> DsdDecoder for DsfDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        self.info
    }

    fn read_dsd(&mut self, out: &mut [u8]) -> Result<usize, DecodeError> {
        let n = self.channels;
        let mut w = 0;
        while w < out.len() {
            if self.emit_i >= self.row_len * n && (!self.load_row()? || self.row_len == 0) {
                break;
            }
            let time = self.emit_i / n;
            let ch = self.emit_i % n;
            let b = self.block[ch * BLOCK + time];
            out[w] = if self.reverse {
                BIT_REVERSE[b as usize]
            } else {
                b
            };
            self.emit_i += 1;
            w += 1;
        }
        Ok(w)
    }
}

impl<S: SeekSource> DsdSeek for DsfDecoder<S> {
    fn seek_to_sample(&mut self, s: u64) -> Result<u64, DecodeError> {
        let s = s.min(self.total_samples);
        let block = BLOCK as u64;
        let row_bytes = self.channels as u64 * block;
        // Clamp the target row to what the `data` chunk actually holds: a header
        // whose declared sample count exceeds its data size must not drive the row
        // past the available bytes, or the remaining-bytes bookkeeping underflows.
        let max_row = self.audio_total / row_bytes;
        let row_r = (s / (block * 8)).min(max_row); // which block-row (32768 samples/row)
        let byte_in = (s / 8) % block; // time-byte within the row

        // Snap the source to the block-row boundary (planar → can't land mid-row).
        self.src
            .seek(SeekFrom::Start(self.data_start + row_r * row_bytes))
            .map_err(|_| DecodeError::Io)?;

        // Re-prime the streaming state as if we had read up to this row. `row_r`
        // is bounded by the data chunk above; saturate the sample-domain figure
        // too in case the declared count and data size disagree.
        let valid_total = self.total_samples.div_ceil(8);
        self.audio_bytes_remaining = self.audio_total - row_r * row_bytes;
        self.valid_bytes_per_channel = valid_total.saturating_sub(row_r * block);
        self.row_len = 0;
        self.emit_i = 0;

        // Refine within the row: reload it and advance the interleaved cursor.
        if byte_in > 0 && self.load_row()? {
            self.emit_i = (byte_in as usize * self.channels).min(self.row_len * self.channels);
        }
        let landed_byte = row_r * block + self.emit_i as u64 / self.channels as u64;
        Ok(landed_byte * 8)
    }
}

/// Map a DSD sampling frequency (Hz) to its [`DsdRate`].
fn dsd_rate(freq: u32) -> Option<DsdRate> {
    match freq {
        2_822_400 => Some(DsdRate::Dsd64),
        5_644_800 => Some(DsdRate::Dsd128),
        11_289_600 => Some(DsdRate::Dsd256),
        22_579_200 => Some(DsdRate::Dsd512),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// Build a stereo DSF: `ch0`/`ch1` give the leading bytes of each channel's
    /// (single) 4096-byte block; `sample_count` is per-channel bits.
    fn dsf(freq: u32, bits: u32, sample_count: u64, ch0: &[u8], ch1: &[u8]) -> Vec<u8> {
        let channels = 2u32;
        let audio = (channels as usize) * BLOCK;
        let mut v = Vec::new();
        // DSD chunk (28)
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&28u64.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes()); // total size (unused here)
        v.extend_from_slice(&0u64.to_le_bytes()); // metadata pointer
        // fmt chunk (52)
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&52u64.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // version
        v.extend_from_slice(&0u32.to_le_bytes()); // format id
        v.extend_from_slice(&2u32.to_le_bytes()); // channel type (stereo)
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&freq.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(&sample_count.to_le_bytes());
        v.extend_from_slice(&(BLOCK as u32).to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // reserved
        // data chunk header (size includes the 12-byte header)
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(audio as u64 + 12).to_le_bytes());
        // planar audio: ch0 block then ch1 block, each 4096 (zero-padded)
        let mut b0 = vec![0u8; BLOCK];
        b0[..ch0.len()].copy_from_slice(ch0);
        let mut b1 = vec![0u8; BLOCK];
        b1[..ch1.len()].copy_from_slice(ch1);
        v.extend_from_slice(&b0);
        v.extend_from_slice(&b1);
        v
    }

    #[test]
    fn normalizes_planar_lsb_to_interleaved_msb() {
        // 16 samples/channel = 2 valid bytes/channel.
        let bytes = dsf(2_822_400, 1, 16, &[0x01, 0x02], &[0x03, 0x04]);
        let mut d = DsfDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(
            d.stream_info().format,
            SampleFormat::Dsd {
                rate: DsdRate::Dsd64
            }
        );
        assert_eq!(d.stream_info().channels, 2);

        let mut out = [0u8; 16];
        let n = d.read_dsd(&mut out).unwrap();
        // Interleaved MSB-first: t0(ch0,ch1), t1(ch0,ch1); each byte bit-reversed.
        // rev(0x01)=0x80 rev(0x03)=0xC0 rev(0x02)=0x40 rev(0x04)=0x20
        assert_eq!(n, 4);
        assert_eq!(&out[..4], &[0x80, 0xC0, 0x40, 0x20]);
        assert_eq!(d.read_dsd(&mut out).unwrap(), 0); // trimmed past valid bytes
    }

    #[test]
    fn seek_with_sample_count_exceeding_data_does_not_underflow() {
        use crate::decoder::DsdSeek;
        // Header declares an enormous per-channel sample count, but the `data`
        // chunk holds a single block-row. Seeking toward the declared end must
        // clamp to the bytes that actually exist, not underflow the remaining-
        // bytes bookkeeping (a debug-overflow panic before the row was clamped).
        let bytes = dsf(2_822_400, 1, u64::MAX / 2, &[0x01], &[0x02]);
        let mut d = DsfDecoder::new(SliceSource::new(&bytes)).unwrap();
        let landed = d.seek_to_sample(u64::MAX).unwrap();
        assert!(landed <= u64::MAX / 2);
        // A read after the clamped seek must also not panic.
        let mut out = [0u8; 16];
        let _ = d.read_dsd(&mut out);
    }

    #[test]
    fn eight_bit_dsd_is_not_reversed() {
        let bytes = dsf(5_644_800, 8, 8, &[0xAA], &[0x55]);
        let mut d = DsfDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(
            d.stream_info().format,
            SampleFormat::Dsd {
                rate: DsdRate::Dsd128
            }
        );
        let mut out = [0u8; 8];
        let n = d.read_dsd(&mut out).unwrap();
        assert_eq!(n, 2); // 8 bits/ch = 1 valid byte/ch × 2 ch
        assert_eq!(&out[..2], &[0xAA, 0x55]); // MSB-first already, no reversal
    }

    #[test]
    fn seeks_within_a_block_row() {
        // 4 valid bytes/channel; distinct bytes at time indices 0..3.
        let bytes = dsf(
            2_822_400,
            1,
            32, // 32 bits/ch = 4 valid bytes/ch
            &[0x01, 0x02, 0x04, 0x08],
            &[0x10, 0x20, 0x40, 0x80],
        );
        let mut d = DsfDecoder::new(SliceSource::new(&bytes)).unwrap();
        // Seek to per-channel sample 16 → time-byte index 2 within the row.
        assert_eq!(d.seek_to_sample(16).unwrap(), 16);
        let mut out = [0u8; 8];
        let n = d.read_dsd(&mut out).unwrap();
        assert_eq!(n, 4); // 2 remaining time indices × 2 channels
        // time 2: rev(0x04)=0x20, rev(0x40)=0x02 ; time 3: rev(0x08)=0x10, rev(0x80)=0x01
        assert_eq!(&out[..4], &[0x20, 0x02, 0x10, 0x01]);
    }

    #[test]
    fn rejects_non_dsd_rate() {
        let bytes = dsf(3_072_000, 1, 16, &[0x01], &[0x02]); // 48k-based, unsupported
        assert_eq!(
            DsfDecoder::new(SliceSource::new(&bytes)).err(),
            Some(DecodeError::Unsupported)
        );
    }

    /// Build an N-channel DSF (one 4096-byte planar block per channel); each
    /// `chans[c]` supplies the leading bytes of channel `c`'s block.
    fn dsf_n(freq: u32, sample_count: u64, chans: &[&[u8]]) -> Vec<u8> {
        let channels = chans.len() as u32;
        let audio = chans.len() * BLOCK;
        let mut v = Vec::new();
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&28u64.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&52u64.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes()); // channel type ≈ count
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&freq.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // bits = 1
        v.extend_from_slice(&sample_count.to_le_bytes());
        v.extend_from_slice(&(BLOCK as u32).to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(audio as u64 + 12).to_le_bytes());
        for ch in chans {
            let mut b = vec![0u8; BLOCK];
            b[..ch.len()].copy_from_slice(ch);
            v.extend_from_slice(&b);
        }
        v
    }

    #[test]
    fn decodes_6_channel_dsf() {
        // 5.1 DSD: 6 planar blocks → interleaved MSB-first clustered frames.
        // 16 samples/ch = 2 valid bytes/ch. channel c byte t = 0x10*c + t + 1.
        let per_ch: Vec<[u8; 2]> = (0..6u8).map(|c| [0x10 * c + 1, 0x10 * c + 2]).collect();
        let refs: Vec<&[u8]> = per_ch.iter().map(|a| a.as_slice()).collect();
        let bytes = dsf_n(2_822_400, 16, &refs);

        let mut d = DsfDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.stream_info().channels, 6);
        let mut out = [0u8; 32];
        let n = d.read_dsd(&mut out).unwrap();
        assert_eq!(n, 12); // 2 time indices × 6 channels
        // Interleaved + bit-reversed (LSB→MSB): out[t*6 + c] = rev(0x10*c + t + 1).
        for t in 0..2usize {
            for c in 0..6usize {
                let src = 0x10 * c as u8 + t as u8 + 1;
                assert_eq!(out[t * 6 + c], src.reverse_bits());
            }
        }
    }
}
