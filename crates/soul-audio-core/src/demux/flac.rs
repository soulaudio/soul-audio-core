//! FLAC demuxer — parses the native FLAC metadata chain into [`StreamInfo`] and
//! streams the audio bytes. Frame decoding (and precise frame delimiting) is the
//! codec's job, deferred; this reader surfaces stream parameters and hands the
//! raw post-metadata bytes to the (future) codec.

use embedded_io::SeekFrom;

use super::{Demuxer, Packet, read_exact, skip};
use crate::decoder::DecodeError;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// Maximum SEEKTABLE points retained (no-alloc). A typical encoder writes one
/// point per ~1–10 s; beyond this cap the extra points are ignored (seeking
/// stays correct, just coarser). Placeholders are never stored.
const MAX_SEEK_POINTS: usize = 512;

/// FLAC container parser over a byte [`Source`].
pub struct FlacDemuxer<S> {
    src: S,
    info: StreamInfo,
    total_samples: Option<u64>,
    /// Absolute byte offset of the first audio frame (after the metadata chain).
    audio_start: u64,
    /// SEEKTABLE points `(sample_number, byte_offset_from_audio_start)`, sorted,
    /// placeholders excluded. `seek_count` are valid.
    seek_points: [(u64, u64); MAX_SEEK_POINTS],
    seek_count: usize,
}

impl<S: Source> FlacDemuxer<S> {
    /// Parse the FLAC signature + metadata blocks (STREAMINFO required).
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a bad container, [`DecodeError::Unsupported`]
    /// for FLAC-in-Ogg.
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        let mut magic = [0u8; 4];
        read_exact(&mut src, &mut magic)?;
        let mut pos: u64 = 4;

        // Tolerate a leading ID3v2 tag (illegal but common), then expect `fLaC`.
        if &magic[0..3] == b"ID3" {
            pos += skip_id3v2(&mut src, magic)?;
            read_exact(&mut src, &mut magic)?;
            pos += 4;
        }
        if &magic == b"OggS" {
            return Err(DecodeError::Unsupported); // FLAC-in-Ogg is a different demuxer
        }
        if &magic != b"fLaC" {
            return Err(DecodeError::Malformed);
        }

        let mut stream_info: Option<(StreamInfo, Option<u64>)> = None;
        let mut seek_points = [(0u64, 0u64); MAX_SEEK_POINTS];
        let mut seek_count = 0usize;
        loop {
            let mut h = [0u8; 4];
            read_exact(&mut src, &mut h)?;
            pos += 4;
            let last = h[0] & 0x80 != 0;
            let btype = h[0] & 0x7F;
            let length = u32::from(h[1]) << 16 | u32::from(h[2]) << 8 | u32::from(h[3]);

            match btype {
                0 => {
                    // STREAMINFO (34 bytes).
                    if length < 34 {
                        return Err(DecodeError::Malformed);
                    }
                    let mut si = [0u8; 34];
                    read_exact(&mut src, &mut si)?;
                    skip(&mut src, u64::from(length) - 34)?;
                    stream_info = Some(parse_streaminfo(&si)?);
                }
                3 => {
                    // SEEKTABLE: length/18 points of 18 bytes each.
                    let points = length as usize / 18;
                    let mut pt = [0u8; 18];
                    for _ in 0..points {
                        read_exact(&mut src, &mut pt)?;
                        let sample = u64::from_be_bytes([
                            pt[0], pt[1], pt[2], pt[3], pt[4], pt[5], pt[6], pt[7],
                        ]);
                        // 0xFFFF..FF = placeholder point; ignore.
                        if sample != u64::MAX && seek_count < MAX_SEEK_POINTS {
                            let byte_off = u64::from_be_bytes([
                                pt[8], pt[9], pt[10], pt[11], pt[12], pt[13], pt[14], pt[15],
                            ]);
                            seek_points[seek_count] = (sample, byte_off);
                            seek_count += 1;
                        }
                    }
                    // Skip any trailing bytes not part of a whole point.
                    skip(&mut src, u64::from(length) - (points as u64) * 18)?;
                }
                _ => skip(&mut src, u64::from(length))?,
            }
            pos += u64::from(length);
            if last {
                break;
            }
        }

        let (info, total_samples) = stream_info.ok_or(DecodeError::Malformed)?;
        Ok(Self {
            src,
            info,
            total_samples,
            audio_start: pos,
            seek_points,
            seek_count,
        })
    }

    /// Total samples per channel, if STREAMINFO declared it.
    #[must_use]
    pub fn total_samples(&self) -> Option<u64> {
        self.total_samples
    }

    /// Mutable access to the underlying source — used by the FLAC-in-Ogg seek
    /// to drive [`OggReader::restart_audio`](crate::demux::ogg::OggReader),
    /// which the demuxer itself is unaware of (it sees only a byte stream).
    pub(crate) fn src_mut(&mut self) -> &mut S {
        &mut self.src
    }

    /// Read raw post-metadata audio bytes into `out` (thin wrapper over the
    /// source, mapping reader errors to [`DecodeError::Io`]).
    ///
    /// Returns the number of bytes read; `0` means end of stream. FLAC frames are
    /// not delimited, so the codec feeds these bytes to its own bit reader.
    pub(crate) fn read_audio(&mut self, out: &mut [u8]) -> Result<usize, DecodeError> {
        self.src.read(out).map_err(|_| DecodeError::Io)
    }

    /// The best SEEKTABLE point at or before per-channel `sample`:
    /// `(base_sample, byte_offset_from_audio_start)`. Falls back to the stream
    /// start `(0, 0)` when there is no earlier point. Points are ascending, so a
    /// binary search finds the greatest `sample_number <= sample`.
    pub(crate) fn seek_point_before(&self, sample: u64) -> (u64, u64) {
        let pts = &self.seek_points[..self.seek_count];
        let mut best = (0u64, 0u64);
        // partition_point: number of points with sample_number <= sample.
        let idx = pts.partition_point(|&(s, _)| s <= sample);
        if idx > 0 {
            best = pts[idx - 1];
        }
        best
    }
}

impl<S: SeekSource> FlacDemuxer<S> {
    /// Reposition the source to `byte_offset` bytes past the first audio frame
    /// (a SEEKTABLE byte offset). Gated on [`SeekSource`], so a streaming source
    /// cannot name it.
    ///
    /// # Errors
    /// [`DecodeError::Io`] if the underlying seek fails.
    pub(crate) fn seek_audio(&mut self, byte_offset: u64) -> Result<(), DecodeError> {
        self.src
            .seek(SeekFrom::Start(self.audio_start + byte_offset))
            .map_err(|_| DecodeError::Io)?;
        Ok(())
    }
}

impl<S: Source> Demuxer for FlacDemuxer<S> {
    fn stream_info(&self) -> StreamInfo {
        self.info
    }

    fn next_packet<'a>(
        &mut self,
        scratch: &'a mut [u8],
    ) -> Result<Option<Packet<'a>>, DecodeError> {
        // Frame boundaries aren't in the frame header, so hand the codec raw
        // audio bytes; it locates frames itself. `dur = 0` = unknown.
        let mut n = 0;
        while n < scratch.len() {
            match self.src.read(&mut scratch[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(Packet {
            ts: 0,
            dur: 0,
            data: &scratch[..n],
        }))
    }
}

/// Skip a leading ID3v2 tag. `head` holds the first 4 bytes (`"ID3"` + major).
/// Returns the number of bytes consumed **after** the initial 4 (i.e. the 6
/// remaining header bytes + the tag body + any footer).
fn skip_id3v2<S: Source>(src: &mut S, _head: [u8; 4]) -> Result<u64, DecodeError> {
    // We've read 4 of the 10-byte header; read the remaining 6.
    let mut rest = [0u8; 6];
    read_exact(src, &mut rest)?;
    let flags = rest[1];
    // Syncsafe 28-bit size (each byte's high bit is 0).
    let size = (u32::from(rest[2]) << 21)
        | (u32::from(rest[3]) << 14)
        | (u32::from(rest[4]) << 7)
        | u32::from(rest[5]);
    let footer = if flags & 0x10 != 0 { 10 } else { 0 };
    skip(src, u64::from(size) + footer)?;
    Ok(6 + u64::from(size) + footer)
}

/// Parse a 34-byte STREAMINFO body.
fn parse_streaminfo(si: &[u8; 34]) -> Result<(StreamInfo, Option<u64>), DecodeError> {
    // Bytes 10..18 pack: sample_rate(20) | channels-1(3) | bps-1(5) | total(36).
    let v = u64::from_be_bytes([
        si[10], si[11], si[12], si[13], si[14], si[15], si[16], si[17],
    ]);
    let sample_rate = (v >> 44) as u32 & 0xF_FFFF;
    let channels = (((v >> 41) & 0x7) as u16) + 1;
    let bits = (((v >> 36) & 0x1F) as u8) + 1;
    let total = v & 0xF_FFFF_FFFF;
    if sample_rate == 0 {
        return Err(DecodeError::Malformed);
    }
    Ok((
        StreamInfo {
            sample_rate,
            channels,
            format: SampleFormat::Int { bits },
        },
        if total == 0 { None } else { Some(total) },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    fn streaminfo(rate: u32, channels: u16, bits: u8, total: u64) -> [u8; 34] {
        let mut si = [0u8; 34];
        si[0..2].copy_from_slice(&4096u16.to_be_bytes()); // min block
        si[2..4].copy_from_slice(&4096u16.to_be_bytes()); // max block
        let v: u64 = (u64::from(rate) << 44)
            | ((u64::from(channels) - 1) << 41)
            | ((u64::from(bits) - 1) << 36)
            | (total & 0xF_FFFF_FFFF);
        si[10..18].copy_from_slice(&v.to_be_bytes());
        si
    }

    fn flac_file(prefix: &[u8], si: &[u8; 34], audio: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(prefix);
        v.extend_from_slice(b"fLaC");
        v.push(0x80); // last block + type 0 (STREAMINFO)
        v.extend_from_slice(&[0, 0, 34]); // length = 34
        v.extend_from_slice(si);
        v.extend_from_slice(audio);
        v
    }

    #[test]
    fn parses_streaminfo() {
        let bytes = flac_file(&[], &streaminfo(96_000, 2, 24, 123_456), &[1, 2, 3, 4]);
        let d = FlacDemuxer::new(SliceSource::new(&bytes)).unwrap();
        let si = d.stream_info();
        assert_eq!(si.sample_rate, 96_000);
        assert_eq!(si.channels, 2);
        assert_eq!(si.format, SampleFormat::Int { bits: 24 });
        assert_eq!(d.total_samples(), Some(123_456));
    }

    #[test]
    fn tolerates_leading_id3v2() {
        // ID3v2 header (10 bytes) + 5-byte payload, syncsafe size = 5.
        let mut id3 = Vec::new();
        id3.extend_from_slice(b"ID3");
        id3.extend_from_slice(&[0x04, 0x00, 0x00]); // ver, flags
        id3.extend_from_slice(&[0, 0, 0, 5]); // syncsafe size = 5
        id3.extend_from_slice(&[0xAA; 5]); // payload
        let bytes = flac_file(&id3, &streaminfo(44_100, 1, 16, 0), &[]);
        let d = FlacDemuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.stream_info().sample_rate, 44_100);
        assert_eq!(d.stream_info().channels, 1);
        assert_eq!(d.total_samples(), None); // total 0 → unknown
    }

    #[test]
    fn rejects_ogg_flac() {
        let bytes = b"OggS\0\0\0\0";
        assert_eq!(
            FlacDemuxer::new(SliceSource::new(bytes)).err(),
            Some(DecodeError::Unsupported)
        );
    }

    #[test]
    fn next_packet_yields_audio_after_metadata() {
        let bytes = flac_file(&[], &streaminfo(44_100, 2, 16, 0), &[9, 8, 7, 6, 5]);
        let mut d = FlacDemuxer::new(SliceSource::new(&bytes)).unwrap();
        let mut scratch = [0u8; 16];
        let pkt = d.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(pkt.data, &[9, 8, 7, 6, 5]);
        assert!(d.next_packet(&mut scratch).unwrap().is_none());
    }
}
