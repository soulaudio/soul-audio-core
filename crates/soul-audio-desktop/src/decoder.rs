//! The owned, `Send` file decoder the deck holds by value and the loader thread
//! moves across the channel: multi-bit PCM (WAV/FLAC/MP3/Ogg-FLAC/AAC/ALAC) or
//! DSD (DSF/DFF) decimated to PCM. Built off the pump thread by [`open_track`].

use std::fs::File;
use std::io::BufReader as IoBufReader;
use std::io::{Read, Seek};
use std::path::Path;

use anyhow::{Result, anyhow};
use embedded_io_adapters::std::FromStd;
use soul_audio_core::demux::MatroskaDemuxer;
use soul_audio_core::formats::dff::DffDecoder;
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::formats::matroska::{MatroskaDecoder, MatroskaVorbisOgg};
use soul_audio_core::formats::vorbis::VorbisDecoder;
use soul_audio_core::model::DsdMode;
use soul_audio_core::{
    AnyDecoder, DecodeError, Decoder, DopAdapter, Format, PlaybackDecimateAdapter, SampleFormat,
    SeekableDecoder, SourceCodec, StreamInfo, open_seekable, probe,
};

/// Any `std::io::Read + Seek` byte source — a local `File`, an in-memory
/// `Cursor`, or a network reader (e.g. an HTTP range client) that fetches
/// bytes on demand. Blanket-implemented, so nothing needs to implement it by
/// hand; it only exists to name the trait object [`Src`] boxes.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek + ?Sized> ReadSeek for T {}

/// A fully **owned**, `Send` byte source: std buffers the reader, so the
/// decoder built on it holds no borrowed scratch and can live in the deck and
/// move across threads. Boxed so the same `Src` — and therefore the same
/// `OwnedDecoder` — serves a local file ([`open_track`]) or any other seekable
/// source such as a network stream ([`open_seekable_src`]); the vtable call
/// through the box happens once per 64 KiB buffer fill (see the capacity
/// below), negligible next to the cost of decoding that buffer.
pub type Src = FromStd<IoBufReader<Box<dyn ReadSeek + Send>>>;

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

/// Build the block-staging storage `open`/`open_seekable`'s `WavPack` variant
/// needs (decoder-memory-externalization spec, Lever B — see
/// `soul_audio_core::decoder_scratch_len`). Shared by [`open_track`] and the CLI's
/// `decode` command, same sizing policy as [`vorbis_arena`].
#[must_use]
pub fn wavpack_pcm() -> Box<[i32]> {
    vec![0i32; soul_audio_core::decoder_scratch_len()].into_boxed_slice()
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
    Pcm(AnyDecoder<Src, Box<[i32]>>),
    Vorbis(VorbisDecoder<Src, Box<[u8]>>),
    /// Vorbis inside Matroska/WebM — also arena-backed (consumer-dispatched like
    /// bare Vorbis), over the Ogg-synthesizing [`MatroskaVorbisOgg`] adapter.
    MatroskaVorbis(VorbisDecoder<MatroskaVorbisOgg<Src>, Box<[u8]>>),
    Dsf(PlaybackDecimateAdapter<DsfDecoder<Src>>),
    Dff(PlaybackDecimateAdapter<DffDecoder<Src>>),
    /// DSF delivered as **DoP** — the file's 1-bit stream packed into a 24-bit
    /// PCM carrier at `dsd_rate / 16`, for a DoP-capable DAC on an exclusive
    /// output. Built by [`open_track_dsd`] with [`DsdMode::Dop`]; see
    /// [`soul_audio_core::dop`] for what the device must satisfy.
    DsfDop(DopAdapter<DsfDecoder<Src>>),
    /// DFF delivered as DoP (as [`OwnedDecoder::DsfDop`]).
    DffDop(DopAdapter<DffDecoder<Src>>),
}

impl Decoder for OwnedDecoder {
    fn stream_info(&self) -> StreamInfo {
        match self {
            OwnedDecoder::Pcm(d) => d.stream_info(),
            OwnedDecoder::Vorbis(d) => d.stream_info(),
            OwnedDecoder::MatroskaVorbis(d) => d.stream_info(),
            OwnedDecoder::Dsf(d) => d.stream_info(),
            OwnedDecoder::DsfDop(d) => d.stream_info(),
            OwnedDecoder::DffDop(d) => d.stream_info(),
            OwnedDecoder::Dff(d) => d.stream_info(),
        }
    }
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        match self {
            OwnedDecoder::Pcm(d) => d.decode_into(out),
            OwnedDecoder::Vorbis(d) => d.decode_into(out),
            OwnedDecoder::MatroskaVorbis(d) => d.decode_into(out),
            OwnedDecoder::Dsf(d) => d.decode_into(out),
            OwnedDecoder::DsfDop(d) => d.decode_into(out),
            OwnedDecoder::DffDop(d) => d.decode_into(out),
            OwnedDecoder::Dff(d) => d.decode_into(out),
        }
    }
    /// Forward to each variant's source format so a DSD track reports its native
    /// `Dsd { rate }` (the DSF/DFF decimators override it), not the decimated PCM.
    fn source_format(&self) -> SampleFormat {
        match self {
            OwnedDecoder::Pcm(d) => d.source_format(),
            OwnedDecoder::Vorbis(d) => d.source_format(),
            OwnedDecoder::MatroskaVorbis(d) => d.source_format(),
            OwnedDecoder::Dsf(d) => d.source_format(),
            OwnedDecoder::DsfDop(d) => d.source_format(),
            OwnedDecoder::DffDop(d) => d.source_format(),
            OwnedDecoder::Dff(d) => d.source_format(),
        }
    }
    /// The source codec: the dispatch decoder names itself (incl. the MP4 ALAC/AAC
    /// split); the consumer-dispatched Vorbis variants are Vorbis, and the DSF/DFF
    /// decimators are native DSD (their decimated-PCM output notwithstanding).
    fn codec(&self) -> SourceCodec {
        match self {
            OwnedDecoder::Pcm(d) => d.codec(),
            OwnedDecoder::Vorbis(_) | OwnedDecoder::MatroskaVorbis(_) => SourceCodec::Vorbis,
            OwnedDecoder::Dsf(_)
            | OwnedDecoder::Dff(_)
            | OwnedDecoder::DsfDop(_)
            | OwnedDecoder::DffDop(_) => SourceCodec::Dsd,
        }
    }
    /// Forward the source channel layout so a decoder's layout (e.g. WAV `dwChannelMask`,
    /// FLAC per-count canonical, Vorbis/Opus per-count order) reaches the deck through this
    /// wrapper instead of the `None` default — the app's real playback path builds
    /// `OwnedDecoder`, so the multichannel fold depends on this forward.
    /// Only the DoP variants hand the device the file's DSD bits untouched;
    /// the decimators produce PCM (a conversion), and every other variant is
    /// PCM to begin with.
    fn dsd_passthrough(&self) -> bool {
        matches!(self, OwnedDecoder::DsfDop(_) | OwnedDecoder::DffDop(_))
    }

    fn channel_layout(&self) -> Option<soul_audio_core::channels::ChannelLayout> {
        match self {
            OwnedDecoder::Pcm(d) => d.channel_layout(),
            OwnedDecoder::Vorbis(d) => d.channel_layout(),
            OwnedDecoder::MatroskaVorbis(d) => d.channel_layout(),
            OwnedDecoder::Dsf(d) => d.channel_layout(),
            OwnedDecoder::Dff(d) => d.channel_layout(),
            // DoP carries the stream's own channels in their own order; there
            // is no layout to fold (a DoP word is not a PCM sample).
            OwnedDecoder::DsfDop(_) | OwnedDecoder::DffDop(_) => None,
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
            OwnedDecoder::DsfDop(d) => d.seek_to_frame(sample),
            OwnedDecoder::DffDop(d) => d.seek_to_frame(sample),
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
            // DoP frames, i.e. DSD samples / 16 per channel.
            OwnedDecoder::DsfDop(d) => d.total_frames(),
            OwnedDecoder::DffDop(d) => d.total_frames(),
        }
    }
}

/// Open `path` into an owned decoder: probe the container, then build the PCM
/// codec, the arena-backed Vorbis decoder, or wrap a DSD decoder in the
/// DSD→PCM decimator.
pub fn open_track(path: &Path) -> Result<OwnedDecoder> {
    let file = File::open(path).map_err(|e| anyhow!("opening {}: {e}", path.display()))?;
    let boxed: Box<dyn ReadSeek + Send> = Box::new(file);
    build_owned_decoder(boxed, &path.display().to_string())
}

/// Open `path` into an owned decoder with an explicit **DSD delivery mode** —
/// the entry point a host uses once it has a plan from
/// [`soul_audio_core::dop::plan`].
///
/// - [`DsdMode::Decimate`] is [`open_track`]: DSD becomes PCM that plays
///   anywhere. The default, because it always works.
/// - [`DsdMode::Dop`] wraps the DSD decoder in the DoP framer instead, so the
///   file's own 1-bit stream reaches a DoP-capable DAC untouched, carried in
///   24-bit PCM words at `dsd_rate / 16`. **The device must be opened at that
///   carrier rate, exclusively, at ≥ 24 bits, with no gain, no resampling and
///   no DSP** — the marker byte is the top byte of every word, so anything
///   that touches the samples turns the stream into full-level noise. Ask
///   `dop::plan` rather than deciding here.
/// - [`DsdMode::Native`] is not an `OwnedDecoder`: a native 1-bit stream is not
///   PCM and does not go through the PCM deck at all (it is the DAP's SAI
///   path), so this returns an error rather than quietly handing back
///   something else.
///
/// A non-DSD file ignores the mode entirely — there is nothing to deliver
/// differently.
///
/// # Errors
/// As [`open_track`], plus [`DsdMode::Native`] (not a PCM decoder).
pub fn open_track_dsd(path: &Path, mode: DsdMode) -> Result<OwnedDecoder> {
    let file = File::open(path).map_err(|e| anyhow!("opening {}: {e}", path.display()))?;
    let boxed: Box<dyn ReadSeek + Send> = Box::new(file);
    build_owned_decoder_with(boxed, &path.display().to_string(), mode)
}

/// Open any `Read + Seek` byte source into an owned decoder — the same
/// container-probe → codec-dispatch path as [`open_track`], but for a source
/// that isn't a local file: a network reader (e.g. an HTTP range client), an
/// in-memory `Cursor`, or anything else that can seek. Lets a downstream
/// player stream a remote track through the exact same [`OwnedDecoder`] type
/// local playback uses.
pub fn open_seekable_src<R: Read + Seek + Send + 'static>(reader: R) -> Result<OwnedDecoder> {
    let boxed: Box<dyn ReadSeek + Send> = Box::new(reader);
    build_owned_decoder(boxed, "stream")
}

/// Open `path` and present only frames `start .. start + frames` of it — the
/// playable form of a **cue-sheet track**.
///
/// A single-file album's tracks all carry the same `file_name` plus their own
/// slice ([`TrackRec::cue_range`](soul_audio_catalog::TrackRec::cue_range), as
/// the index bakes it); this turns one such record into a decoder whose
/// position 0 is the track's first frame and whose end is the track's end. The
/// deck, its gapless preload, seeking and the duration display then work on it
/// unchanged — the swap from one cue track to the next is sample-exact, with no
/// device reset and no re-seek.
///
/// `frames: None` runs to the end of the file (a sheet's last track when the
/// file's length is unknown). For an ordinary whole-file track, use
/// [`open_track`]: wrapping it in a full-file range would work but costs a seek
/// and says something less clear.
///
/// # Errors
/// As [`open_track`], plus a decoder that cannot seek (a range of an unseekable
/// stream is not playable).
pub fn open_track_range(
    path: &Path,
    start: u64,
    frames: Option<u64>,
) -> Result<soul_audio_core::SubRange<OwnedDecoder>> {
    let inner = open_track(path)?;
    soul_audio_core::SubRange::new(inner, start, frames)
        .map_err(|e| anyhow!("sub-range of {}: {e:?}", path.display()))
}

/// Shared probe → match → build body for [`open_track`] and
/// [`open_seekable_src`], so the two entry points can never drift: `label` is
/// only used to name the source in error messages (a path for the former, a
/// generic "stream" for the latter).
fn build_owned_decoder(reader: Box<dyn ReadSeek + Send>, label: &str) -> Result<OwnedDecoder> {
    build_owned_decoder_with(reader, label, DsdMode::Decimate)
}

/// [`build_owned_decoder`] with an explicit DSD delivery mode (see
/// [`open_track_dsd`]).
fn build_owned_decoder_with(
    reader: Box<dyn ReadSeek + Send>,
    label: &str,
    dsd_mode: DsdMode,
) -> Result<OwnedDecoder> {
    // 64 KiB of std buffering; the decoder reads through it via embedded-io.
    let mut src = FromStd::new(IoBufReader::with_capacity(64 * 1024, reader));
    let format = probe(&mut src).map_err(|e| anyhow!("probing {label}: {e:?}"))?;
    match format {
        // DSD playback: decimate all the way to the native 44.1 kHz family (not the
        // archival 352.8 kHz `DecimateAdapter`) so the deck's device-rate resample is
        // cheap enough to sustain real-time — single-stage 352.8 kHz resampling runs at
        // ~0.5× real-time even in release and starves the audio callback (stutter heard
        // as slowed/stretched playback). See `PlaybackDecimateAdapter`.
        Format::Dsf => {
            let inner = DsfDecoder::new(src).map_err(|e| anyhow!("DSF {label}: {e:?}"))?;
            match dsd_mode {
                DsdMode::Dop => Ok(OwnedDecoder::DsfDop(DopAdapter::new(inner))),
                DsdMode::Decimate => Ok(OwnedDecoder::Dsf(PlaybackDecimateAdapter::new(inner))),
                DsdMode::Native => Err(anyhow!(
                    "native DSD is not a PCM decoder (use the DSD decoder directly): {label}"
                )),
            }
        }
        Format::Dff => {
            let inner = DffDecoder::new(src).map_err(|e| anyhow!("DFF {label}: {e:?}"))?;
            match dsd_mode {
                DsdMode::Dop => Ok(OwnedDecoder::DffDop(DopAdapter::new(inner))),
                DsdMode::Decimate => Ok(OwnedDecoder::Dff(PlaybackDecimateAdapter::new(inner))),
                DsdMode::Native => Err(anyhow!(
                    "native DSD is not a PCM decoder (use the DSD decoder directly): {label}"
                )),
            }
        }
        // Vorbis needs a caller-provided arena (`VorbisDecoder<S, A>`), so it
        // is not an `AnyDecoder` variant `open` can build — special-cased
        // here, same as the DSF/DFF split above, ahead of the `open` catch-all
        // (design spec, "Dispatch: per-platform, NOT `AnyDecoder`").
        Format::Vorbis => Ok(OwnedDecoder::Vorbis(
            VorbisDecoder::new(src, vorbis_arena())
                .map_err(|e| anyhow!("Vorbis {label}: {e:?}"))?,
        )),
        // Matroska/WebM: peek the track codec. `A_VORBIS` needs the arena (like
        // bare Vorbis), so it's built here over the Ogg-synthesizing adapter;
        // Opus/FLAC/AAC ride the arena-free `AnyDecoder::Matroska` path, built
        // from the same already-parsed demuxer (no re-read).
        Format::Matroska => {
            let demux =
                MatroskaDemuxer::new(src).map_err(|e| anyhow!("Matroska {label}: {e:?}"))?;
            if demux.codec_id() == b"A_VORBIS" {
                let adapter = MatroskaVorbisOgg::new(demux)
                    .map_err(|e| anyhow!("Vorbis-in-Matroska {label}: {e:?}"))?;
                Ok(OwnedDecoder::MatroskaVorbis(
                    VorbisDecoder::new(adapter, vorbis_arena())
                        .map_err(|e| anyhow!("Vorbis-in-Matroska {label}: {e:?}"))?,
                ))
            } else {
                Ok(OwnedDecoder::Pcm(AnyDecoder::Matroska(
                    MatroskaDecoder::from_demuxer_in(demux, wavpack_pcm())
                        .map_err(|e| anyhow!("Matroska {label}: {e:?}"))?,
                )))
            }
        }
        // WAV/FLAC/MP3/Ogg-FLAC/AAC/ALAC/CAF — `open_seekable` re-probes (cheap)
        // and builds the codec. The source is always seekable, so MP4 goes
        // through the seekable reader (decoding non-faststart `.m4a` too); every
        // other format delegates to the same `open` path as before.
        _ => {
            let mut any = open_seekable(src, wavpack_pcm())
                .map_err(|e| anyhow!("decoding {label}: {e:?}"))?;
            // A CBR MP1/MP2 (or a Xing-less MP3) carries no in-band length, so
            // `total_frames()` would be `None` and the player would show an
            // unknown/0 duration. Resolve a CBR byte-size estimate now (a no-op
            // for every other format) so the deck reports a real length.
            any.resolve_lengths();
            Ok(OwnedDecoder::Pcm(any))
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
        // 64 MiB, like the matroska sibling below: building the desktop-profile
        // `VorbisDecoder` (~668 KiB by value) multiplies through many debug-build
        // move frames, which overflows a 16 MiB thread on the Windows MSVC debug
        // toolchain (STATUS_STACK_OVERFLOW). The runtime pump/loader threads carry
        // 16–64 MiB for the same reason.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
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

    /// A real DSF fixture opened in **DoP** mode: the file's own 1-bit stream,
    /// packed into a 24-bit PCM carrier for a DoP DAC rather than decimated.
    ///
    /// Three things have to be right or a DAC plays noise, and all three are
    /// checked against the same fixture's decimated open: the carrier rate is
    /// the DSD rate over 16 (not the DSD rate, and not the decimated rate), the
    /// stream is still reported as DSD at source, and every word carries the
    /// `0x05`/`0xFA` marker in its top byte, alternating once per frame and
    /// shared by both channels.
    #[test]
    fn open_track_dsd_delivers_dop_frames_at_the_carrier_rate() {
        // `OwnedDecoder` is large by value (the Vorbis variant dominates it),
        // so build it on a big stack like the sibling tests do.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(dop_open_body)
            .expect("spawn")
            .join()
            .expect("DoP open thread panicked");
    }

    fn dop_open_body() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tone.dsf");
        let mut dop = open_track_dsd(&path, DsdMode::Dop).expect("open DSF as DoP");
        let decimated = open_track(&path).expect("open DSF decimated");

        // The source is DSD64 either way; only the delivery differs.
        let rate = match dop.source_format() {
            SampleFormat::Dsd { rate } => rate,
            other => panic!("DoP must still report a DSD source, got {other:?}"),
        };
        assert_eq!(dop.codec(), SourceCodec::Dsd);
        assert_eq!(
            dop.stream_info().sample_rate,
            soul_audio_core::dop::dop_carrier_rate(rate),
            "DoP must be delivered at the dsd_rate/16 carrier"
        );
        assert_eq!(dop.stream_info().sample_rate, 176_400, "DSD64 → 176.4 kHz");
        assert_eq!(dop.stream_info().format, SampleFormat::Int { bits: 24 });
        assert_eq!(
            decimated.stream_info().sample_rate,
            44_100,
            "the decimated path is unchanged"
        );
        assert_eq!(dop.stream_info().channels, 2);

        // Every word's top byte is the marker, alternating per frame.
        let mut buf = [0i32; 2 * 64];
        let n = dop.decode_into(&mut buf).expect("decode DoP");
        assert!(n > 0, "no DoP frames decoded");
        for (f, frame) in buf[..n].chunks_exact(2).enumerate() {
            let want = if f % 2 == 0 { 0x05 } else { 0xFA };
            for (ch, &word) in frame.iter().enumerate() {
                // The word is a canonical signed-24 sample carrying the DoP bit
                // pattern in its low 24 bits (a `0xFA` marker sets bit 23, so
                // the sample is negative) — read it as the pattern, and check
                // it really is in range, because the deck clamps what is not.
                assert_eq!(
                    (word as u32 >> 16) & 0xFF,
                    want,
                    "frame {f} channel {ch}: marker"
                );
                assert!(
                    (-(1 << 23)..1 << 23).contains(&word),
                    "frame {f} channel {ch}: {word:#x} is outside canonical signed-24"
                );
            }
        }

        // A DoP track still reports a duration (DSD samples / 16), so the deck
        // can trim and the UI can show a position.
        assert!(dop.total_frames().is_some(), "DoP must report its length");

        // Native DSD is not a PCM decoder and says so rather than silently
        // handing back something else.
        assert!(open_track_dsd(&path, DsdMode::Native).is_err());
        // A non-DSD file ignores the mode entirely.
        let flac =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tone.flac");
        let a = open_track_dsd(&flac, DsdMode::Dop).expect("FLAC ignores the DSD mode");
        assert_eq!(a.stream_info().sample_rate, 44_100);
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

    /// `open_track` reports the real source **codec** per container — crucially the
    /// ALAC-vs-AAC split inside one `.m4a` MP4 container (a lossless ALAC and a
    /// lossy AAC file must never read the same in a signal-path display), plus the
    /// consumer-dispatched Vorbis and the DSD decimators.
    #[test]
    fn open_track_reports_the_source_codec() {
        use soul_audio_core::{Decoder, SourceCodec};
        let td = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata");
        let cases: &[(&str, SourceCodec)] = &[
            ("tone.wav", SourceCodec::Wav),
            ("tone.flac", SourceCodec::Flac),
            ("tone.m4a", SourceCodec::Alac),
            ("complex-alac.m4a", SourceCodec::Alac),
            ("chirp-aac.m4a", SourceCodec::Aac),
            ("tone.aac", SourceCodec::Aac),
            ("complex.mp3", SourceCodec::Mp3),
            ("complex.opus", SourceCodec::Opus),
            ("complex-vorbis.ogg", SourceCodec::Vorbis),
            ("antiphase.wv", SourceCodec::WavPack),
            // CAF and Matroska containers report their real payload codec, not
            // the container — the lossless-vs-lossy distinction (CAF-ALAC and
            // Matroska-FLAC are lossless; Matroska-Opus/AAC are lossy).
            ("tone.caf", SourceCodec::Pcm),
            ("tone-alac.caf", SourceCodec::Alac),
            ("tone-flac.mka", SourceCodec::Flac),
            ("tone-aac.mka", SourceCodec::Aac),
            ("tone-opus.webm", SourceCodec::Opus),
            ("tone-vorbis.mka", SourceCodec::Vorbis),
            ("tone.dsf", SourceCodec::Dsd),
            ("tone.dff", SourceCodec::Dsd),
        ];
        // 64 MiB: `open_track` builds the large `OwnedDecoder` by value, like the
        // Vorbis tests above (the default stack overflows on Windows MSVC debug).
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let mut failures = Vec::new();
                for (file, want) in cases {
                    // AAC decode is patent-gated off by default (Via LA); `open_track`
                    // declines AAC as Unsupported, so skip those cases unless `aac` is on.
                    if !cfg!(feature = "aac") && *want == SourceCodec::Aac {
                        continue;
                    }
                    let path = td.join(file);
                    match open_track(&path) {
                        Ok(d) => {
                            let got = d.codec();
                            if got != *want {
                                failures.push(format!("{file}: codec {got:?}, want {want:?}"));
                            }
                        }
                        Err(e) => failures.push(format!("{file}: open failed {e:?}")),
                    }
                }
                assert!(
                    failures.is_empty(),
                    "source-codec mismatches:\n  {}",
                    failures.join("\n  ")
                );
            })
            .expect("spawn codec thread")
            .join()
            .expect("codec thread panicked");
    }

    /// `open_seekable_src` over an in-memory `Cursor` must decode identically
    /// to `open_track` reading the same bytes from disk — proving the
    /// non-file path (what a network/HTTP-range reader would use) really
    /// decodes real audio, not just that it builds without erroring.
    #[test]
    fn open_seekable_src_matches_open_track() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tone.wav");
        let bytes = std::fs::read(&path).expect("read tone.wav fixture");

        // 64 MiB, like the sibling tests above: two `OwnedDecoder`s by value in
        // one frame multiplies through debug-build move frames further still.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let mut from_file = open_track(&path).expect("open_track on tone.wav");
                let mut from_cursor = open_seekable_src(std::io::Cursor::new(bytes))
                    .expect("open_seekable_src on tone.wav bytes");

                assert_eq!(
                    from_file.stream_info().channels,
                    from_cursor.stream_info().channels,
                    "channel count differs between file and cursor sources"
                );

                let mut buf_a = [0i32; 4096];
                let mut buf_b = [0i32; 4096];
                let n_a = from_file
                    .decode_into(&mut buf_a)
                    .expect("decode block from file source");
                let n_b = from_cursor
                    .decode_into(&mut buf_b)
                    .expect("decode block from cursor source");
                assert!(n_a > 0, "decoded no samples from the file source");
                assert_eq!(
                    n_a, n_b,
                    "block sizes differ between file and cursor sources"
                );
                assert_eq!(
                    &buf_a[..n_a],
                    &buf_b[..n_b],
                    "decoded samples differ between file and cursor sources"
                );
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

    /// A cue track's slice, through the desktop entry point a host calls with
    /// a catalog record's `cue_range`: position 0 is the track's first frame,
    /// the length is the track's, and the decode stops at the slice's end
    /// rather than running into the next track's audio.
    #[test]
    fn open_track_range_plays_only_its_slice() {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(|| {
                let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../testdata/tone.flac");
                // The whole file first, as the reference.
                let mut whole = open_track(&path).expect("open whole");
                let total = whole.total_frames().expect("tone.flac declares a length");
                let mut all = Vec::new();
                let mut buf = vec![0i32; 4096];
                loop {
                    let n = whole.decode_into(&mut buf).expect("decode");
                    if n == 0 {
                        break;
                    }
                    all.extend_from_slice(&buf[..n]);
                }

                // A slice from the middle: 0.4 s in, 0.8 s long, at 44.1 kHz.
                let (start, frames) = (17_640u64, 35_280u64);
                assert!(start + frames < total);
                let mut slice = open_track_range(&path, start, Some(frames)).expect("open range");
                assert_eq!(slice.total_frames(), Some(frames), "the track's length");
                let ch = usize::from(slice.stream_info().channels);
                let mut got = Vec::new();
                loop {
                    let n = slice.decode_into(&mut buf).expect("decode slice");
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                assert_eq!(got.len(), frames as usize * ch, "exactly the slice");
                let from = start as usize * ch;
                assert_eq!(
                    got,
                    all[from..from + got.len()],
                    "the slice must be the file's own frames for that range"
                );
            })
            .expect("spawn")
            .join()
            .expect("range thread panicked");
    }
}
