//! The owned, `Send` file decoder the deck holds by value and the loader thread
//! moves across the channel: multi-bit PCM (WAV/FLAC/MP3/Ogg-FLAC/AAC/ALAC) or
//! DSD (DSF/DFF) decimated to PCM. Built off the pump thread by [`open_track`].

use std::fs::File;
use std::io::BufReader as IoBufReader;
use std::path::Path;

use anyhow::{Result, anyhow};
use embedded_io_adapters::std::FromStd;
use soul_audio_core::demux::MatroskaDemuxer;
use soul_audio_core::formats::dff::DffDecoder;
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::formats::matroska::{MatroskaDecoder, MatroskaVorbisOgg};
use soul_audio_core::formats::vorbis::VorbisDecoder;
use soul_audio_core::{
    AnyDecoder, DecodeError, Decoder, Format, PlaybackDecimateAdapter, SeekableDecoder, StreamInfo,
    open_seekable, probe,
};

/// A fully **owned**, `Send` byte source: std buffers the file, so the decoder
/// built on it holds no borrowed scratch and can live in the deck and move
/// across threads.
pub type Src = FromStd<IoBufReader<File>>;

/// Default byte size of the heap arena [`VorbisDecoder::new`] bump-allocates
/// its setup-header tables (codebooks/floors/residues/mappings/modes) into.
/// Generous over Xiph's own budget guidance (design spec: "~150 KB typical,
/// 250 KB 'use low-mem below this'"), with extra headroom since #34: the
/// desktop profile also builds a `4 KiB` FAST_HUFFMAN acceleration table per
/// codebook here (best-effort — books that don't fit fall back to the scan, so
/// this only affects decode *speed* on dense many-codebook files, never
/// correctness).
pub const VORBIS_DEFAULT_ARENA: usize = 512 * 1024;

/// "Accept-anything" arena size, used instead of [`VORBIS_DEFAULT_ARENA`] when
/// `SOUL_VORBIS_ROBUST` is set in the environment — an escape hatch for an
/// exceptionally large setup header (design spec: "~500 KB rare worst case")
/// plus its FAST_HUFFMAN tables (#34), without paying the cost by default.
pub const VORBIS_ROBUST_ARENA: usize = 1024 * 1024;

/// Build the heap arena a desktop `VorbisDecoder::new` call bump-allocates
/// into, sized by the `SOUL_VORBIS_ROBUST` environment variable (see
/// [`VORBIS_DEFAULT_ARENA`] / [`VORBIS_ROBUST_ARENA`]). Shared by
/// [`open_track`] and the CLI's `decode` command so both dispatch sites use
/// the same sizing policy.
#[must_use]
pub fn vorbis_arena() -> Box<[u8]> {
    let size = if std::env::var_os("SOUL_VORBIS_ROBUST").is_some() {
        VORBIS_ROBUST_ARENA
    } else {
        VORBIS_DEFAULT_ARENA
    };
    vec![0u8; size].into_boxed_slice()
}

/// A playable owned decoder: multi-bit PCM (WAV/FLAC/MP3/Ogg-FLAC/AAC/ALAC),
/// Vorbis, or DSD (DSF/DFF) decimated to PCM. DSD is decimated in-decoder
/// (DSD64 → 352.8 kHz 24-bit) and the deck then resamples it to the device
/// rate like any other source.
///
/// The PCM variant is `AnyDecoder` (~315 KiB by value — now multichannel-sized
/// after AAC 5.1/7.1 and Opus surround multistream landed); the desktop-profile
/// `Vorbis` variant is larger still (~668 KiB — its per-frame DSP scratch,
/// spectrum/residue/overlap, is sized for an 8192-sample long block at the full
/// profile's 8-channel cap) and dominates `OwnedDecoder`'s size — but the deck
/// holds decoders by value on a big-stack thread by design (matching the core's
/// no-alloc `AnyDecoder` and `VorbisDecoder`), so the size gap is accepted
/// rather than boxed. A test (`owned_decoder_size_is_bounded`) guards against
/// this growing further unnoticed.
#[allow(clippy::large_enum_variant)]
pub enum OwnedDecoder {
    Pcm(AnyDecoder<Src>),
    Vorbis(VorbisDecoder<Src, Box<[u8]>>),
    /// Vorbis inside Matroska/WebM — also arena-backed (consumer-dispatched like
    /// bare Vorbis), over the Ogg-synthesizing [`MatroskaVorbisOgg`] adapter.
    MatroskaVorbis(VorbisDecoder<MatroskaVorbisOgg<Src>, Box<[u8]>>),
    Dsf(PlaybackDecimateAdapter<DsfDecoder<Src>>),
    Dff(PlaybackDecimateAdapter<DffDecoder<Src>>),
}

impl Decoder for OwnedDecoder {
    fn stream_info(&self) -> StreamInfo {
        match self {
            OwnedDecoder::Pcm(d) => d.stream_info(),
            OwnedDecoder::Vorbis(d) => d.stream_info(),
            OwnedDecoder::MatroskaVorbis(d) => d.stream_info(),
            OwnedDecoder::Dsf(d) => d.stream_info(),
            OwnedDecoder::Dff(d) => d.stream_info(),
        }
    }
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        match self {
            OwnedDecoder::Pcm(d) => d.decode_into(out),
            OwnedDecoder::Vorbis(d) => d.decode_into(out),
            OwnedDecoder::MatroskaVorbis(d) => d.decode_into(out),
            OwnedDecoder::Dsf(d) => d.decode_into(out),
            OwnedDecoder::Dff(d) => d.decode_into(out),
        }
    }
}

impl SeekableDecoder for OwnedDecoder {
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        match self {
            OwnedDecoder::Pcm(d) => d.seek_to(sample),
            OwnedDecoder::Vorbis(d) => d.seek_to(sample),
            // Vorbis-in-Matroska seeks over a synthesized Ogg stream whose byte
            // offsets don't map to the container, so the Ogg granule bisection
            // can't apply — seeking is a follow-up.
            OwnedDecoder::MatroskaVorbis(_) => Err(DecodeError::Unsupported),
            // The decimator's PCM frame ↔ DSD sample mapping lives in its inherent
            // `seek_to_frame`.
            OwnedDecoder::Dsf(d) => d.seek_to_frame(sample),
            OwnedDecoder::Dff(d) => d.seek_to_frame(sample),
        }
    }
    fn total_frames(&self) -> Option<u64> {
        match self {
            OwnedDecoder::Pcm(d) => d.total_frames(),
            OwnedDecoder::Vorbis(d) => d.total_frames(),
            OwnedDecoder::MatroskaVorbis(_) => None,
            // DSD: the decimator reports the decimated PCM frame count from the
            // container's declared DSD sample count (`None` if not declared).
            OwnedDecoder::Dsf(d) => d.total_frames(),
            OwnedDecoder::Dff(d) => d.total_frames(),
        }
    }
}

/// Open `path` into an owned decoder: probe the container, then build the PCM
/// codec, the arena-backed Vorbis decoder, or wrap a DSD decoder in the
/// DSD→PCM decimator.
pub fn open_track(path: &Path) -> Result<OwnedDecoder> {
    let file = File::open(path).map_err(|e| anyhow!("opening {}: {e}", path.display()))?;
    // 64 KiB of std buffering; the decoder reads through it via embedded-io.
    let mut src = FromStd::new(IoBufReader::with_capacity(64 * 1024, file));
    let format = probe(&mut src).map_err(|e| anyhow!("probing {}: {e:?}", path.display()))?;
    match format {
        // DSD playback: decimate all the way to the native 44.1 kHz family (not the
        // archival 352.8 kHz `DecimateAdapter`) so the deck's device-rate resample is
        // cheap enough to sustain real-time — single-stage 352.8 kHz resampling runs at
        // ~0.5× real-time even in release and starves the audio callback (stutter heard
        // as slowed/stretched playback). See `PlaybackDecimateAdapter`.
        Format::Dsf => Ok(OwnedDecoder::Dsf(PlaybackDecimateAdapter::new(
            DsfDecoder::new(src).map_err(|e| anyhow!("DSF {}: {e:?}", path.display()))?,
        ))),
        Format::Dff => Ok(OwnedDecoder::Dff(PlaybackDecimateAdapter::new(
            DffDecoder::new(src).map_err(|e| anyhow!("DFF {}: {e:?}", path.display()))?,
        ))),
        // Vorbis needs a caller-provided arena (`VorbisDecoder<S, A>`), so it
        // is not an `AnyDecoder` variant `open` can build — special-cased
        // here, same as the DSF/DFF split above, ahead of the `open` catch-all
        // (design spec, "Dispatch: per-platform, NOT `AnyDecoder`").
        Format::Vorbis => Ok(OwnedDecoder::Vorbis(
            VorbisDecoder::new(src, vorbis_arena())
                .map_err(|e| anyhow!("Vorbis {}: {e:?}", path.display()))?,
        )),
        // Matroska/WebM: peek the track codec. `A_VORBIS` needs the arena (like
        // bare Vorbis), so it's built here over the Ogg-synthesizing adapter;
        // Opus/FLAC/AAC ride the arena-free `AnyDecoder::Matroska` path, built
        // from the same already-parsed demuxer (no re-read).
        Format::Matroska => {
            let demux = MatroskaDemuxer::new(src)
                .map_err(|e| anyhow!("Matroska {}: {e:?}", path.display()))?;
            if demux.codec_id() == b"A_VORBIS" {
                let adapter = MatroskaVorbisOgg::new(demux)
                    .map_err(|e| anyhow!("Vorbis-in-Matroska {}: {e:?}", path.display()))?;
                Ok(OwnedDecoder::MatroskaVorbis(
                    VorbisDecoder::new(adapter, vorbis_arena())
                        .map_err(|e| anyhow!("Vorbis-in-Matroska {}: {e:?}", path.display()))?,
                ))
            } else {
                Ok(OwnedDecoder::Pcm(AnyDecoder::Matroska(
                    MatroskaDecoder::from_demuxer(demux)
                        .map_err(|e| anyhow!("Matroska {}: {e:?}", path.display()))?,
                )))
            }
        }
        // WAV/FLAC/MP3/Ogg-FLAC/AAC/ALAC — `open_seekable` re-probes (cheap) and
        // builds the codec. Seekable (not plain `open`) so MP4/.m4a with `moov`
        // after `mdat` — ffmpeg's default muxing — decodes, not just faststart.
        _ => {
            Ok(OwnedDecoder::Pcm(open_seekable(src).map_err(|e| {
                anyhow!("decoding {}: {e:?}", path.display())
            })?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real Vorbis, end to end through `open_track` — the desktop dispatch
    /// path a plain `VorbisDecoder`-only test (in `soul_audio_core`) can't
    /// exercise. Guards the `Format::Vorbis` special case specifically (not
    /// just "some codec routes correctly"), on the crate's established
    /// big-stack thread pattern.
    #[test]
    fn open_track_decodes_real_vorbis() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tone-vorbis.ogg");
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let mut d = open_track(&path).expect("open real Vorbis fixture");
                assert_eq!(d.stream_info().channels, 2);
                let mut buf = [0i32; 4096];
                let mut total = 0usize;
                loop {
                    let n = d.decode_into(&mut buf).expect("decode Vorbis packet");
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                assert!(total > 0, "decoded no samples from a real Vorbis fixture");
            })
            .expect("spawn decode thread")
            .join()
            .expect("decode thread panicked");
    }

    /// Vorbis-in-Matroska through `open_track` — guards the consumer-dispatch
    /// path that peeks the Matroska track codec and routes `A_VORBIS` to the
    /// arena-backed Ogg-synthesizing adapter (the core suite builds this
    /// directly; only the shell wires the arena + codec peek).
    #[test]
    fn open_track_decodes_vorbis_in_matroska() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tone-vorbis.mka");
        // 64 MiB: building `VorbisDecoder<MatroskaVorbisOgg<Src>>` (~712 KiB by
        // value) multiplies through many debug-build move frames, like the
        // runtime pump/loader threads (`soul-audio-service`/CLI) carry.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let mut d = open_track(&path).expect("open Vorbis-in-Matroska fixture");
                assert_eq!(d.stream_info().channels, 2);
                let mut buf = [0i32; 4096];
                let mut total = 0usize;
                loop {
                    let n = d
                        .decode_into(&mut buf)
                        .expect("decode Vorbis-in-mka packet");
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                assert!(total > 0, "decoded no samples from Vorbis-in-Matroska");
            })
            .expect("spawn decode thread")
            .join()
            .expect("decode thread panicked");
    }

    /// `OwnedDecoder`'s size is dominated by whichever variant is largest: the
    /// desktop-profile Vorbis decoder, ~668 KiB at the 8-channel/8192-block cap
    /// (its per-frame DSP scratch — spectrum/residue/overlap — scales with the
    /// channel cap, which is 8 in the full profile; see the type's doc comment).
    /// The `MatroskaVorbis` variant is that plus the ~48 KiB Ogg-synthesizing
    /// adapter (~712 KiB), so it is now the ceiling. Bounded here so a future
    /// profile change that silently balloons
    /// it further gets caught as a test failure instead of a surprise stack
    /// overflow three call frames away (the bug this same growth already
    /// triggered once — see `soul-audio-cli`'s worker/loader thread
    /// `stack_size` comments). The runtime worker threads carry 16–64 MiB
    /// stacks, so this is far from an overflow; the bound is a drift tripwire.
    #[test]
    fn owned_decoder_size_is_bounded() {
        let size = std::mem::size_of::<OwnedDecoder>();
        assert!(
            size < 768 * 1024,
            "OwnedDecoder grew to {size} bytes — bump the CLI/driver worker \
             thread stack sizes (and this bound) if that's intentional"
        );
    }
}
