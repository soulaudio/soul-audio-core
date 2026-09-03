//! The owned, `Send` file decoder the deck holds by value and the loader thread
//! moves across the channel: multi-bit PCM (WAV/FLAC/MP3/Ogg-FLAC/AAC/ALAC) or
//! DSD (DSF/DFF) decimated to PCM. Built off the pump thread by [`open_track`].

use std::fs::File;
use std::io::BufReader as IoBufReader;
use std::path::Path;

use anyhow::{Result, anyhow};
use embedded_io_adapters::std::FromStd;
use soul_audio_core::formats::dff::DffDecoder;
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::{
    AnyDecoder, DecimateAdapter, DecodeError, Decoder, Format, SeekableDecoder, StreamInfo, open,
    probe,
};

/// A fully **owned**, `Send` byte source: std buffers the file, so the decoder
/// built on it holds no borrowed scratch and can live in the deck and move
/// across threads.
pub type Src = FromStd<IoBufReader<File>>;

/// A playable owned decoder: multi-bit PCM (WAV/FLAC/MP3/Ogg-FLAC/AAC/ALAC)
/// or DSD (DSF/DFF) decimated to PCM. DSD is decimated in-decoder (DSD64 →
/// 352.8 kHz 24-bit) and the deck then resamples it to the device rate like
/// any other source.
///
/// The PCM variant is `AnyDecoder` (~150 KiB by value, FLAC-dominated) — much
/// larger than the DSD variants — but the deck holds decoders by value on a
/// big-stack thread by design (matching the core's no-alloc `AnyDecoder`), so
/// the size gap is accepted rather than boxed.
#[allow(clippy::large_enum_variant)]
pub enum OwnedDecoder {
    Pcm(AnyDecoder<Src>),
    Dsf(DecimateAdapter<DsfDecoder<Src>>),
    Dff(DecimateAdapter<DffDecoder<Src>>),
}

impl Decoder for OwnedDecoder {
    fn stream_info(&self) -> StreamInfo {
        match self {
            OwnedDecoder::Pcm(d) => d.stream_info(),
            OwnedDecoder::Dsf(d) => d.stream_info(),
            OwnedDecoder::Dff(d) => d.stream_info(),
        }
    }
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        match self {
            OwnedDecoder::Pcm(d) => d.decode_into(out),
            OwnedDecoder::Dsf(d) => d.decode_into(out),
            OwnedDecoder::Dff(d) => d.decode_into(out),
        }
    }
}

impl SeekableDecoder for OwnedDecoder {
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        match self {
            OwnedDecoder::Pcm(d) => d.seek_to(sample),
            // The decimator's PCM frame ↔ DSD sample mapping lives in its inherent
            // `seek_to_frame`.
            OwnedDecoder::Dsf(d) => d.seek_to_frame(sample),
            OwnedDecoder::Dff(d) => d.seek_to_frame(sample),
        }
    }
    fn total_frames(&self) -> Option<u64> {
        match self {
            OwnedDecoder::Pcm(d) => d.total_frames(),
            // DSD: the decimator reports the decimated PCM frame count from the
            // container's declared DSD sample count (`None` if not declared).
            OwnedDecoder::Dsf(d) => d.total_frames(),
            OwnedDecoder::Dff(d) => d.total_frames(),
        }
    }
}

/// Open `path` into an owned decoder: probe the container, then build the PCM
/// codec or wrap a DSD decoder in the DSD→PCM decimator.
pub fn open_track(path: &Path) -> Result<OwnedDecoder> {
    let file = File::open(path).map_err(|e| anyhow!("opening {}: {e}", path.display()))?;
    // 64 KiB of std buffering; the decoder reads through it via embedded-io.
    let mut src = FromStd::new(IoBufReader::with_capacity(64 * 1024, file));
    let format = probe(&mut src).map_err(|e| anyhow!("probing {}: {e:?}", path.display()))?;
    match format {
        Format::Dsf => Ok(OwnedDecoder::Dsf(DecimateAdapter::new(
            DsfDecoder::new(src).map_err(|e| anyhow!("DSF {}: {e:?}", path.display()))?,
        ))),
        Format::Dff => Ok(OwnedDecoder::Dff(DecimateAdapter::new(
            DffDecoder::new(src).map_err(|e| anyhow!("DFF {}: {e:?}", path.display()))?,
        ))),
        // WAV/FLAC/MP3/Ogg-FLAC — `open` re-probes (cheap) and builds the codec.
        _ => {
            Ok(OwnedDecoder::Pcm(open(src).map_err(|e| {
                anyhow!("decoding {}: {e:?}", path.display())
            })?))
        }
    }
}
