//! DFF / DSDIFF (Philips) DSD decoder — parses the big-endian IFF container and
//! streams audio as packed 1-bit DSD.
//!
//! Uncompressed DFF audio is already **MSB-first** and **channel-interleaved**
//! (clustered frames = one byte per channel), so `read_dsd` is a passthrough —
//! no bit reversal or transpose (unlike DSF). DST-compressed audio (a `DST `
//! sound-data chunk of `FRTE` + `DSTF` frames) is decoded frame-by-frame by
//! [`crate::formats::dst`] into the same MSB-first interleaved layout, so both
//! paths present an identical DSD byte stream. All fields are big-endian and
//! chunk sizes **exclude** the 12-byte chunk header.

use embedded_io::SeekFrom;

use crate::decoder::{DecodeError, DsdDecoder, DsdSeek};
use crate::demux::{read_exact, skip};
use crate::formats::dst::DstDecoder;
use crate::io::{SeekSource, Source};
use crate::model::{DsdRate, SampleFormat, StreamInfo};

/// Largest DST `DSTF` (compressed frame) payload we will buffer. A stored
/// (uncompressed) frame is `1 + frame_bytes`; the coded form is normally
/// smaller. Sized for stereo DSD128 (`frame_bytes = 18816`) plus headroom;
/// larger frames are declined with [`DecodeError::Unsupported`].
const MAX_DSTF_BYTES: usize = 18816 + 1024;

/// Per-stream DST decode state (present only for DST-compressed DFF).
struct DstState {
    decoder: DstDecoder,
    /// Scratch for the current `DSTF` compressed frame payload.
    frame_buf: [u8; MAX_DSTF_BYTES],
    /// Total `DSTF` frames declared by `FRTE`.
    num_frames: u64,
    /// `DSTF` frames not yet consumed.
    frames_remaining: u64,
    /// Decoded interleaved-DSD bytes in the current frame.
    cur_len: usize,
    /// Bytes already served from the current frame.
    cur_pos: usize,
    /// Interleaved-DSD bytes produced per decoded frame.
    frame_bytes: usize,
}

/// DFF decoder over a byte [`Source`].
pub struct DffDecoder<S> {
    src: S,
    info: StreamInfo,
    channels: u64,
    sound_data_start: u64,
    audio_total: u64,
    audio_bytes_remaining: u64,
    /// `Some` for DST-compressed streams; `None` for raw `DSD ` streams.
    dst: Option<DstState>,
}

impl<S: Source> DffDecoder<S> {
    /// Parse the DSDIFF header up to (and into) the sound-data chunk.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a bad container, [`DecodeError::Unsupported`]
    /// for a non-DSD rate or a DST stream outside the supported profile (more
    /// than stereo, or larger than DSD128).
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        // FRM8 <size:u64> "DSD "
        let mut top = [0u8; 16];
        read_exact(&mut src, &mut top)?;
        if &top[0..4] != b"FRM8" || &top[12..16] != b"DSD " {
            return Err(DecodeError::Malformed);
        }

        let mut sample_rate = 0u32;
        let mut channels = 0u16;
        let mut pos: u64 = 16;

        loop {
            let (id, size) = read_chunk_header(&mut src)?;
            pos += 12;
            match &id {
                b"PROP" => {
                    let (sr, ch) = parse_prop(&mut src, size)?;
                    sample_rate = sr;
                    channels = ch;
                    pos += size;
                }
                b"DSD " => {
                    // Raw interleaved DSD sound data — positioned at audio start.
                    if sample_rate == 0 || channels == 0 {
                        return Err(DecodeError::Malformed);
                    }
                    let rate = dsd_rate(sample_rate).ok_or(DecodeError::Unsupported)?;
                    return Ok(Self {
                        src,
                        info: stream_info(channels, rate),
                        channels: u64::from(channels),
                        sound_data_start: pos,
                        audio_total: size,
                        audio_bytes_remaining: size,
                        dst: None,
                    });
                }
                b"DST " => {
                    // DST-compressed sound data: FRTE header then DSTF frames.
                    if sample_rate == 0 || channels == 0 {
                        return Err(DecodeError::Malformed);
                    }
                    let rate = dsd_rate(sample_rate).ok_or(DecodeError::Unsupported)?;
                    return Self::open_dst(src, sample_rate, channels, rate, pos);
                }
                _ => {
                    let padded = size + (size & 1); // FVER, ABSS, ...
                    skip(&mut src, padded)?;
                    pos += padded;
                }
            }
        }
    }

    /// Finish construction for a DST-compressed stream: read the `FRTE` header
    /// and set up the frame decoder positioned at the first `DSTF` chunk.
    fn open_dst(
        mut src: S,
        sample_rate: u32,
        channels: u16,
        rate: DsdRate,
        mut pos: u64,
    ) -> Result<Self, DecodeError> {
        // First sub-chunk of the DST list must be FRTE {numFrames:u32, frameRate:u16}.
        let (fid, fsize) = read_chunk_header(&mut src)?;
        pos += 12;
        if &fid != b"FRTE" || fsize < 6 {
            return Err(DecodeError::Malformed);
        }
        let mut fb = [0u8; 6];
        read_exact(&mut src, &mut fb)?;
        let num_frames = u64::from(u32::from_be_bytes([fb[0], fb[1], fb[2], fb[3]]));
        let frame_rate = u16::from_be_bytes([fb[4], fb[5]]);
        pos += 6;
        let fpad = fsize + (fsize & 1);
        if fpad > 6 {
            skip(&mut src, fpad - 6)?;
            pos += fpad - 6;
        }
        if frame_rate == 0 {
            return Err(DecodeError::Malformed);
        }

        // 1-bit samples per channel per frame = DSD rate / frame rate.
        let samples_per_frame = (sample_rate / u32::from(frame_rate)) as usize;
        let decoder = DstDecoder::new(usize::from(channels), samples_per_frame)?;
        let frame_bytes = decoder.frame_bytes();

        let audio_total = num_frames * frame_bytes as u64;
        Ok(Self {
            src,
            info: stream_info(channels, rate),
            channels: u64::from(channels),
            sound_data_start: pos,
            audio_total,
            audio_bytes_remaining: audio_total,
            dst: Some(DstState {
                decoder,
                frame_buf: [0u8; MAX_DSTF_BYTES],
                num_frames,
                frames_remaining: num_frames,
                cur_len: 0,
                cur_pos: 0,
                frame_bytes,
            }),
        })
    }

    /// Read the next `DSTF` frame (skipping `DSTC` CRC chunks) and decode it into
    /// the decoder's frame buffer. Returns `false` when no frames remain.
    fn fetch_next_frame(&mut self) -> Result<bool, DecodeError> {
        loop {
            if self.dst.as_ref().map_or(0, |d| d.frames_remaining) == 0 {
                return Ok(false);
            }
            let (id, size) = read_chunk_header(&mut self.src)?;
            let padded = size + (size & 1);
            if &id == b"DSTF" {
                let len = size as usize;
                if len == 0 || len > MAX_DSTF_BYTES {
                    return Err(DecodeError::Unsupported);
                }
                read_exact(
                    &mut self.src,
                    &mut self.dst.as_mut().expect("dst set").frame_buf[..len],
                )?;
                if padded > size {
                    skip(&mut self.src, padded - size)?;
                }
                let dst = self.dst.as_mut().expect("dst set");
                dst.frames_remaining -= 1;
                dst.decoder.decode_frame(&dst.frame_buf[..len])?;
                dst.cur_len = dst.decoder.frame_bytes();
                dst.cur_pos = 0;
                return Ok(true);
            }
            // DSTC (CRC) or any other sub-chunk: skip.
            skip(&mut self.src, padded)?;
        }
    }

    /// Serve decoded DSD bytes for a DST stream, decoding frames as needed.
    fn read_dsd_dst(&mut self, out: &mut [u8]) -> Result<usize, DecodeError> {
        let mut n = 0;
        while n < out.len() {
            let (cur_pos, cur_len) = {
                let d = self.dst.as_ref().expect("dst set");
                (d.cur_pos, d.cur_len)
            };
            if cur_pos >= cur_len {
                if !self.fetch_next_frame()? {
                    break;
                }
                continue;
            }
            let take = (cur_len - cur_pos).min(out.len() - n);
            {
                let frame = self.dst.as_ref().expect("dst set").decoder.last_frame();
                out[n..n + take].copy_from_slice(&frame[cur_pos..cur_pos + take]);
            }
            self.dst.as_mut().expect("dst set").cur_pos += take;
            n += take;
        }
        Ok(n)
    }
}

impl<S: Source> DsdDecoder for DffDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        self.info
    }

    fn read_dsd(&mut self, out: &mut [u8]) -> Result<usize, DecodeError> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.dst.is_some() {
            return self.read_dsd_dst(out);
        }
        if self.audio_bytes_remaining == 0 {
            return Ok(0);
        }
        let want = (out.len() as u64).min(self.audio_bytes_remaining) as usize;
        let mut n = 0;
        while n < want {
            match self.src.read(&mut out[n..want]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        self.audio_bytes_remaining -= n as u64;
        Ok(n)
    }
}

impl<S: SeekSource> DsdSeek for DffDecoder<S> {
    fn seek_to_sample(&mut self, s: u64) -> Result<u64, DecodeError> {
        let total_samples = self.audio_total / self.channels * 8;
        let s = s.min(total_samples);
        let group = s / 8; // time-byte index (8 samples/channel per byte)

        if self.dst.is_some() {
            return self.seek_dst(group);
        }

        let byte = self.sound_data_start + group * self.channels;
        self.src
            .seek(SeekFrom::Start(byte))
            .map_err(|_| DecodeError::Io)?;
        self.audio_bytes_remaining = self.audio_total - group * self.channels;
        Ok(group * 8)
    }
}

impl<S: SeekSource> DffDecoder<S> {
    /// Frame-granular seek for DST: rewind to the first `DSTF`, then decode and
    /// discard whole frames up to the one containing the target byte group.
    fn seek_dst(&mut self, group: u64) -> Result<u64, DecodeError> {
        let (frame_bytes, num_frames) = {
            let d = self.dst.as_ref().expect("dst set");
            (d.frame_bytes as u64, d.num_frames)
        };
        let target_byte = group * self.channels;
        let target_frame = target_byte / frame_bytes;
        let byte_in_frame = (target_byte % frame_bytes) as usize;

        self.src
            .seek(SeekFrom::Start(self.sound_data_start))
            .map_err(|_| DecodeError::Io)?;
        {
            let d = self.dst.as_mut().expect("dst set");
            d.frames_remaining = num_frames;
            d.cur_len = 0;
            d.cur_pos = 0;
        }
        // Decode and discard frames before the target.
        for _ in 0..target_frame {
            if !self.fetch_next_frame()? {
                return Ok(group * 8);
            }
        }
        // Decode the target frame and position within it.
        if self.fetch_next_frame()? {
            self.dst.as_mut().expect("dst set").cur_pos = byte_in_frame.min(frame_bytes as usize);
        }
        Ok(group * 8)
    }
}

/// Build the [`StreamInfo`] for a DSD stream of `channels` channels at `rate`.
fn stream_info(channels: u16, rate: DsdRate) -> StreamInfo {
    StreamInfo {
        sample_rate: 44_100,
        channels,
        format: SampleFormat::Dsd { rate },
    }
}

/// Read a 12-byte IFF chunk header: 4-byte id + big-endian `u64` size.
fn read_chunk_header<S: Source>(src: &mut S) -> Result<([u8; 4], u64), DecodeError> {
    let mut h = [0u8; 12];
    read_exact(src, &mut h)?;
    let id = [h[0], h[1], h[2], h[3]];
    let size = u64::from_be_bytes([h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11]]);
    Ok((id, size))
}

/// Parse a `PROP` container of `size` bytes: propType `SND ` then its property
/// chunks (`FS `, `CHNL`, `CMPR`, …). Returns `(sample_rate, channels)`. The
/// compression type is carried by the sound-data chunk id (`DSD ` vs `DST `),
/// so `CMPR` here is informational and simply skipped.
fn parse_prop<S: Source>(src: &mut S, size: u64) -> Result<(u32, u16), DecodeError> {
    let mut prop_type = [0u8; 4];
    read_exact(src, &mut prop_type)?;
    if &prop_type != b"SND " {
        return Err(DecodeError::Malformed);
    }
    let mut rem = size - 4;
    let mut sample_rate = 0u32;
    let mut channels = 0u16;

    while rem >= 12 {
        let (id, csize) = read_chunk_header(src)?;
        rem -= 12;
        let padded = csize + (csize & 1);
        match &id {
            b"FS  " => {
                let mut b = [0u8; 4];
                read_exact(src, &mut b)?;
                sample_rate = u32::from_be_bytes(b);
                skip(src, padded - 4)?;
            }
            b"CHNL" => {
                let mut b = [0u8; 2];
                read_exact(src, &mut b)?;
                channels = u16::from_be_bytes(b);
                skip(src, padded - 2)?; // channel IDs
            }
            _ => skip(src, padded)?, // CMPR, etc.
        }
        rem -= padded;
    }
    Ok((sample_rate, channels))
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

    /// Build a DSDIFF file: PROP(FS, CHNL, CMPR) + a raw `DSD ` sound chunk.
    fn dff(freq: u32, channels: u16, cmpr: &[u8; 4], audio: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&prop_chunk(freq, channels, cmpr));
        body.extend_from_slice(b"DSD ");
        body.extend_from_slice(&(audio.len() as u64).to_be_bytes());
        body.extend_from_slice(audio);
        frm8(&body)
    }

    /// Build a DSDIFF file whose sound data is a `DST ` list: FRTE + DSTF frames.
    fn dff_dst(freq: u32, channels: u16, frame_rate: u16, frames: &[&[u8]]) -> Vec<u8> {
        // DST list body: FRTE then one DSTF per frame.
        let mut dst = Vec::new();
        dst.extend_from_slice(b"FRTE");
        dst.extend_from_slice(&6u64.to_be_bytes());
        dst.extend_from_slice(&(frames.len() as u32).to_be_bytes());
        dst.extend_from_slice(&frame_rate.to_be_bytes());
        for f in frames {
            dst.extend_from_slice(b"DSTF");
            dst.extend_from_slice(&(f.len() as u64).to_be_bytes());
            dst.extend_from_slice(f);
            if f.len() % 2 == 1 {
                dst.push(0);
            }
        }

        let mut body = Vec::new();
        body.extend_from_slice(&prop_chunk(freq, channels, b"DST "));
        body.extend_from_slice(b"DST ");
        body.extend_from_slice(&(dst.len() as u64).to_be_bytes());
        body.extend_from_slice(&dst);
        frm8(&body)
    }

    /// A PROP(SND) container: FS + CHNL + CMPR.
    fn prop_chunk(freq: u32, channels: u16, cmpr: &[u8; 4]) -> Vec<u8> {
        let mut prop = Vec::new();
        prop.extend_from_slice(b"SND ");
        prop.extend_from_slice(b"FS  ");
        prop.extend_from_slice(&4u64.to_be_bytes());
        prop.extend_from_slice(&freq.to_be_bytes());
        let chnl_body = 2 + (channels as usize) * 4;
        prop.extend_from_slice(b"CHNL");
        prop.extend_from_slice(&(chnl_body as u64).to_be_bytes());
        prop.extend_from_slice(&channels.to_be_bytes());
        for _ in 0..channels {
            prop.extend_from_slice(b"SLFT");
        }
        let name = b"raw";
        let cmpr_body = 4 + 1 + name.len();
        prop.extend_from_slice(b"CMPR");
        prop.extend_from_slice(&(cmpr_body as u64).to_be_bytes());
        prop.extend_from_slice(cmpr);
        prop.push(name.len() as u8);
        prop.extend_from_slice(name);
        if cmpr_body % 2 == 1 {
            prop.push(0);
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"PROP");
        out.extend_from_slice(&(prop.len() as u64).to_be_bytes());
        out.extend_from_slice(&prop);
        out
    }

    /// Wrap a body in the FRM8/DSD top-level chunk.
    fn frm8(body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"FRM8");
        v.extend_from_slice(&(4u64 + body.len() as u64).to_be_bytes());
        v.extend_from_slice(b"DSD "); // formType
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn parses_and_passes_through_msb_interleaved() {
        let audio = [0xAA, 0x55, 0x0F, 0xF0];
        let bytes = dff(2_822_400, 2, b"DSD ", &audio);
        let mut d = DffDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(
            d.stream_info().format,
            SampleFormat::Dsd {
                rate: DsdRate::Dsd64
            }
        );
        assert_eq!(d.stream_info().channels, 2);
        let mut out = [0u8; 8];
        let n = d.read_dsd(&mut out).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&out[..4], &audio); // passthrough, no bit reversal
        assert_eq!(d.read_dsd(&mut out).unwrap(), 0);
    }

    #[test]
    fn seeks_by_sample_group() {
        // Stereo clustered frames: [c0,c1] per time-byte (8 samples/ch each).
        let audio = [10, 20, 11, 21, 12, 22, 13, 23];
        let bytes = dff(2_822_400, 2, b"DSD ", &audio);
        let mut d = DffDecoder::new(SliceSource::new(&bytes)).unwrap();
        // Seek to per-channel sample 16 → group 2 → byte offset 2*channels.
        assert_eq!(d.seek_to_sample(16).unwrap(), 16);
        let mut out = [0u8; 8];
        let n = d.read_dsd(&mut out).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&out[..4], &[12, 22, 13, 23]); // groups 2 and 3
    }

    #[test]
    fn dff_mono_decodes_end_to_end_through_dop() {
        use crate::decoder::Decoder;
        use crate::dsd::DopAdapter;

        // Mono DSD: consecutive bytes are 16 bits of the one channel, so the
        // (channel-naive) DoP adapter frames them correctly.
        let audio = [0xAA, 0xBB, 0xCC, 0xDD];
        let bytes = dff(2_822_400, 1, b"DSD ", &audio);
        let dff = DffDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut dop = DopAdapter::new(dff);
        let mut out = [0i32; 4];
        let n = dop.decode_into(&mut out).unwrap();
        assert_eq!(n, 2); // 4 DSD bytes → 2 DoP frames
        assert_eq!(out[0], 0x0005_AABB); // marker 0x05
        assert_eq!(out[1], 0x00FA_CCDD); // marker 0xFA
    }

    /// A DST container with uncompressed (stored) `DSTF` frames must decode back
    /// to the raw DSD — proving the FRTE/DSTF container→codec wiring. (Frames
    /// coded with the real arithmetic coder are round-tripped in `formats::dst`.)
    #[test]
    fn dst_container_bypass_frames_roundtrip() {
        // DSD64 stereo: 2_822_400 / 75 / 8 = 4704 bytes/ch, ×2 = 9408 bytes/frame.
        let frame_bytes = 9408usize;
        // Build two stored frames: header byte 0x00 then raw interleaved DSD.
        let mut raw0 = vec![0u8; frame_bytes];
        let mut raw1 = vec![0u8; frame_bytes];
        for (i, b) in raw0.iter_mut().enumerate() {
            *b = (i * 7 + 1) as u8;
        }
        for (i, b) in raw1.iter_mut().enumerate() {
            *b = (i * 13 + 5) as u8;
        }
        let mut f0 = vec![0u8; 1 + frame_bytes];
        f0[1..].copy_from_slice(&raw0);
        let mut f1 = vec![0u8; 1 + frame_bytes];
        f1[1..].copy_from_slice(&raw1);

        let bytes = dff_dst(2_822_400, 2, 75, &[&f0, &f1]);
        let mut d = DffDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(
            d.stream_info().format,
            SampleFormat::Dsd {
                rate: DsdRate::Dsd64
            }
        );

        let mut out = vec![0u8; 2 * frame_bytes];
        let mut got = 0;
        loop {
            let n = d.read_dsd(&mut out[got..]).unwrap();
            if n == 0 {
                break;
            }
            got += n;
        }
        assert_eq!(got, 2 * frame_bytes);
        assert_eq!(&out[..frame_bytes], &raw0[..]);
        assert_eq!(&out[frame_bytes..], &raw1[..]);
    }

    /// Frame-granular DST seek: landing in the second frame serves that frame's
    /// bytes from the correct in-frame offset.
    #[test]
    fn dst_seek_lands_in_target_frame() {
        let frame_bytes = 9408usize;
        let mut raw0 = vec![0u8; frame_bytes];
        let mut raw1 = vec![0u8; frame_bytes];
        for (i, b) in raw0.iter_mut().enumerate() {
            *b = (i * 7 + 1) as u8;
        }
        for (i, b) in raw1.iter_mut().enumerate() {
            *b = (i * 13 + 5) as u8;
        }
        let mut f0 = vec![0u8; 1 + frame_bytes];
        f0[1..].copy_from_slice(&raw0);
        let mut f1 = vec![0u8; 1 + frame_bytes];
        f1[1..].copy_from_slice(&raw1);

        let bytes = dff_dst(2_822_400, 2, 75, &[&f0, &f1]);
        let mut d = DffDecoder::new(SliceSource::new(&bytes)).unwrap();

        // sample 40000 → group 5000 → byte 10000 → frame 1, offset 592.
        let landed = d.seek_to_sample(40000).unwrap();
        assert_eq!(landed, 40000);
        let mut out = [0u8; 16];
        assert_eq!(d.read_dsd(&mut out).unwrap(), 16);
        assert_eq!(&out[..], &raw1[592..608]);
    }

    #[test]
    fn dst_unsupported_geometry_is_declined() {
        // DSD64 but 3 channels — beyond the stereo DST profile.
        let frame_bytes = 2_822_400 / 75 / 8 * 3;
        let mut f = vec![0u8; 1 + frame_bytes];
        f[0] = 0x00;
        let bytes = dff_dst(2_822_400, 3, 75, &[&f]);
        assert_eq!(
            DffDecoder::new(SliceSource::new(&bytes)).err(),
            Some(DecodeError::Unsupported)
        );
    }
}
