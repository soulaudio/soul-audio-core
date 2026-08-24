//! MP3 (MPEG-1/2/2.5 Audio Layer III) demuxer — skips ID3v2, locks onto the
//! frame sync, parses stream parameters, and packetizes one MPEG frame per
//! packet. MP3 *decoding* (Huffman, IMDCT, the bit reservoir) is the codec's
//! job, deferred.

use super::{Demuxer, Packet, read_exact, skip};
use crate::decoder::DecodeError;
use crate::io::Source;
use crate::model::{SampleFormat, StreamInfo};

/// Layer III bitrate (kbps) by index — MPEG-1 and MPEG-2/2.5.
const BITRATE_V1: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];
const BITRATE_V2: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];
/// Sample rate by version bits (00=2.5, 01=reserved, 10=2, 11=1) and index.
const SRATE: [[u32; 4]; 4] = [
    [11025, 12000, 8000, 0],  // MPEG-2.5
    [0, 0, 0, 0],             // reserved
    [22050, 24000, 16000, 0], // MPEG-2
    [44100, 48000, 32000, 0], // MPEG-1
];

/// MP3 container parser over a byte [`Source`].
pub struct Mp3Demuxer<S> {
    src: S,
    info: StreamInfo,
    spf: u32,                 // samples per frame
    pending: Option<[u8; 4]>, // first frame header, read while parsing StreamInfo
    pos_samples: u64,
}

impl<S: Source> Mp3Demuxer<S> {
    /// Skip ID3v2, find the first valid Layer III frame, and read its parameters.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] if no valid frame header is found.
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        let mut head = [0u8; 4];
        read_exact(&mut src, &mut head)?;

        if &head[0..3] == b"ID3" {
            let mut rest = [0u8; 6];
            read_exact(&mut src, &mut rest)?;
            let flags = rest[1];
            let size = (u32::from(rest[2]) << 21)
                | (u32::from(rest[3]) << 14)
                | (u32::from(rest[4]) << 7)
                | u32::from(rest[5]);
            let footer = if flags & 0x10 != 0 { 10 } else { 0 };
            skip(&mut src, u64::from(size) + footer)?;
            read_exact(&mut src, &mut head)?;
        }

        // Resync to the first valid frame header.
        loop {
            if let Some(f) = parse_header(&head) {
                let spf = f.spf;
                return Ok(Self {
                    src,
                    info: StreamInfo {
                        sample_rate: f.sample_rate,
                        channels: f.channels,
                        format: SampleFormat::Int { bits: 16 },
                    },
                    spf,
                    pending: Some(head),
                    pos_samples: 0,
                });
            }
            head[0] = head[1];
            head[1] = head[2];
            head[2] = head[3];
            let mut b = [0u8; 1];
            read_exact(&mut src, &mut b)?; // EOF here → Malformed (no frame)
            head[3] = b[0];
        }
    }

    /// Read the next frame header (resyncing past garbage). `Ok(None)` at EOF.
    fn next_header(&mut self) -> Result<Option<[u8; 4]>, DecodeError> {
        let mut h = [0u8; 4];
        if !fill_or_eof(&mut self.src, &mut h)? {
            return Ok(None);
        }
        loop {
            if parse_header(&h).is_some() {
                return Ok(Some(h));
            }
            h[0] = h[1];
            h[1] = h[2];
            h[2] = h[3];
            let mut b = [0u8; 1];
            if !fill_or_eof(&mut self.src, &mut b)? {
                return Ok(None);
            }
            h[3] = b[0];
        }
    }
}

impl<S: Source> Demuxer for Mp3Demuxer<S> {
    fn stream_info(&self) -> StreamInfo {
        self.info
    }

    fn next_packet<'a>(
        &mut self,
        scratch: &'a mut [u8],
    ) -> Result<Option<Packet<'a>>, DecodeError> {
        let header = match self.pending.take() {
            Some(h) => h,
            None => match self.next_header()? {
                Some(h) => h,
                None => return Ok(None),
            },
        };
        let frame = parse_header(&header).ok_or(DecodeError::Malformed)?;
        if frame.len < 4 || frame.len > scratch.len() {
            return Err(DecodeError::Unsupported); // scratch too small for the frame
        }
        scratch[0..4].copy_from_slice(&header);
        // Read the frame body; a short read at EOF truncates the final frame.
        let mut n = 4;
        while n < frame.len {
            match self.src.read(&mut scratch[n..frame.len]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        let ts = self.pos_samples;
        self.pos_samples += u64::from(self.spf);
        Ok(Some(Packet {
            ts,
            dur: self.spf,
            data: &scratch[..n],
        }))
    }
}

struct Frame {
    sample_rate: u32,
    channels: u16,
    spf: u32,
    len: usize,
}

/// Parse+validate a 4-byte MPEG Layer III frame header. `None` if not a valid L3
/// frame (reserved fields, free/bad bitrate, non-Layer-III, etc.).
fn parse_header(h: &[u8; 4]) -> Option<Frame> {
    if h[0] != 0xFF || (h[1] & 0xE0) != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 0x3; // 0=2.5, 1=reserved, 2=v2, 3=v1
    let layer = (h[1] >> 1) & 0x3; // 1 = Layer III
    let br_idx = ((h[2] >> 4) & 0xF) as usize;
    let sr_idx = ((h[2] >> 2) & 0x3) as usize;
    let padding = u32::from((h[2] >> 1) & 0x1);
    let chan_mode = (h[3] >> 6) & 0x3;

    if version == 1 || layer != 1 || br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    let sample_rate = SRATE[version as usize][sr_idx];
    if sample_rate == 0 {
        return None;
    }
    let bitrate_kbps = if version == 3 {
        BITRATE_V1[br_idx]
    } else {
        BITRATE_V2[br_idx]
    };
    let spf = if version == 3 { 1152 } else { 576 };
    let coef = spf / 8; // 144 (MPEG-1) or 72 (MPEG-2/2.5)
    let len = (coef * bitrate_kbps * 1000 / sample_rate + padding) as usize;
    let channels = if chan_mode == 3 { 1 } else { 2 };
    Some(Frame {
        sample_rate,
        channels,
        spf,
        len,
    })
}

/// Read exactly `buf.len()` bytes; `Ok(true)` if filled, `Ok(false)` if EOF
/// reached before filling.
fn fill_or_eof<S: Source>(src: &mut S, buf: &mut [u8]) -> Result<bool, DecodeError> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => return Ok(false),
            Ok(k) => n += k,
            Err(_) => return Err(DecodeError::Io),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// Build a 4-byte MPEG-1 Layer III header (stereo unless `mono`).
    fn header_v1(br_idx: u8, sr_idx: u8, padding: bool, mono: bool) -> [u8; 4] {
        let b1 = 0xE0 | (0b11 << 3) | (0b01 << 1); // sync tail, version 1, layer III, prot=1
        let b2 = (br_idx << 4) | (sr_idx << 2) | (u8::from(padding) << 1);
        let b3 = if mono { 0b11 << 6 } else { 0 };
        [0xFF, b1, b2, b3]
    }

    fn header_v2(br_idx: u8, sr_idx: u8) -> [u8; 4] {
        let b1 = 0xE0 | (0b10 << 3) | (0b01 << 1); // version 2, layer III
        let b2 = (br_idx << 4) | (sr_idx << 2);
        [0xFF, b1, b2, 0]
    }

    #[test]
    fn parses_mpeg1_l3_stream_info() {
        // 128 kbps (idx 9), 44100 (idx 0), stereo.
        let h = header_v1(9, 0, false, false);
        let mut bytes = h.to_vec();
        bytes.resize(2048, 0); // frame body padding
        let d = Mp3Demuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.stream_info().sample_rate, 44_100);
        assert_eq!(d.stream_info().channels, 2);
        assert_eq!(d.stream_info().format, SampleFormat::Int { bits: 16 });
    }

    #[test]
    fn frame_len_uses_version_specific_coefficient() {
        // MPEG-1 128k/44100: 144*128000/44100 = 417 bytes.
        let h1 = header_v1(9, 0, false, false);
        let mut b1 = h1.to_vec();
        b1.resize(1024, 0);
        let mut d1 = Mp3Demuxer::new(SliceSource::new(&b1)).unwrap();
        let mut scratch = [0u8; 2048];
        let p1 = d1.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(p1.data.len(), 417);
        assert_eq!(p1.dur, 1152);

        // MPEG-2 64k/22050: 72*64000/22050 = 208 bytes, spf 576.
        let h2 = header_v2(8, 0); // 64 kbps (idx 8 in V2 table), 22050 (idx 0)
        let mut b2 = h2.to_vec();
        b2.resize(1024, 0);
        let mut d2 = Mp3Demuxer::new(SliceSource::new(&b2)).unwrap();
        assert_eq!(d2.stream_info().sample_rate, 22_050);
        let p2 = d2.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(p2.data.len(), 208);
        assert_eq!(p2.dur, 576);
    }

    #[test]
    fn mono_header_reports_one_channel() {
        let h = header_v1(9, 0, false, true);
        let mut bytes = h.to_vec();
        bytes.resize(1024, 0);
        let d = Mp3Demuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.stream_info().channels, 1);
    }

    #[test]
    fn skips_id3v2_and_resyncs_past_garbage() {
        let mut bytes = Vec::new();
        // ID3v2 header + 4-byte payload (syncsafe size = 4).
        bytes.extend_from_slice(b"ID3");
        bytes.extend_from_slice(&[0x04, 0x00, 0x00]);
        bytes.extend_from_slice(&[0, 0, 0, 4]);
        bytes.extend_from_slice(&[0xAA; 4]);
        // Some non-sync garbage before the first frame.
        bytes.extend_from_slice(&[0x00, 0x13, 0x37]);
        bytes.extend_from_slice(&header_v1(9, 0, false, false));
        bytes.resize(bytes.len() + 512, 0);
        let d = Mp3Demuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.stream_info().sample_rate, 44_100);
    }

    #[test]
    fn ts_advances_by_samples_per_frame() {
        let h = header_v1(9, 0, false, false);
        let mut bytes = h.to_vec();
        // two frames back-to-back: 417 bytes each
        bytes.resize(417, 0);
        bytes.extend_from_slice(&h);
        bytes.resize(417 * 2, 0);
        let mut d = Mp3Demuxer::new(SliceSource::new(&bytes)).unwrap();
        let mut scratch = [0u8; 2048];
        let p0 = d.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(p0.ts, 0);
        let p1 = d.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(p1.ts, 1152);
    }
}
