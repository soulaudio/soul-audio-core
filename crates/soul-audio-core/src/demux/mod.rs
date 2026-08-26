//! Container demuxers: parse a container into [`StreamInfo`] + a pull-stream of
//! codec [`Packet`]s. Demux (container structure) is kept separate from codec
//! (sample decoding), per the FormatReader/AVFormatContext model — adapted to
//! no-alloc: a packet is a `Copy` descriptor over a **borrowed** payload, never
//! an owned buffer.

use crate::decoder::DecodeError;
use crate::io::Source;
use crate::model::StreamInfo;

pub mod flac;
pub mod mp3;
pub mod mp4;
pub mod ogg;
pub mod wav;

pub use flac::FlacDemuxer;
pub use mp3::Mp3Demuxer;
pub use wav::WavDemuxer;

/// A demuxed unit of one stream — a `Copy` descriptor over a borrowed payload.
///
/// `data` borrows the caller-provided scratch that [`Demuxer::next_packet`]
/// filled, so nothing is allocated.
#[derive(Debug, Clone, Copy)]
pub struct Packet<'a> {
    /// Presentation timestamp, in frames from the start of the stream.
    pub ts: u64,
    /// Number of frames carried by this packet.
    pub dur: u32,
    /// The packet payload (codec bitstream bytes).
    pub data: &'a [u8],
}

/// A container parser: header → [`StreamInfo`], then a forward pull-stream of
/// packets. Owns its [`Source`] (like the decoders). Allocation-free.
pub trait Demuxer {
    /// Stream parameters, known after construction.
    fn stream_info(&self) -> StreamInfo;

    /// Demux the next packet into `scratch`, returning a descriptor that borrows
    /// it. `Ok(None)` means end of stream.
    fn next_packet<'a>(&mut self, scratch: &'a mut [u8])
    -> Result<Option<Packet<'a>>, DecodeError>;
}

/// Read exactly `buf.len()` bytes, or fail. Maps a short read to
/// [`DecodeError::UnexpectedEof`] and a reader error to [`DecodeError::Io`].
pub(crate) fn read_exact<S: Source>(src: &mut S, buf: &mut [u8]) -> Result<(), DecodeError> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => return Err(DecodeError::UnexpectedEof),
            Ok(k) => n += k,
            Err(_) => return Err(DecodeError::Io),
        }
    }
    Ok(())
}

/// Discard `n` bytes from `src` (forward-only; no `Seek` required).
pub(crate) fn skip<S: Source>(src: &mut S, mut n: u64) -> Result<(), DecodeError> {
    let mut buf = [0u8; 256];
    while n > 0 {
        let want = n.min(buf.len() as u64) as usize;
        read_exact(src, &mut buf[..want])?;
        n -= want as u64;
    }
    Ok(())
}
