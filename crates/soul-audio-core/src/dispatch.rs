//! Runtime format dispatch: [`probe`](crate::probe) → [`AnyDecoder`] → uniform
//! [`Decoder`].
//!
//! `no_std` + no-alloc rules out `Box<dyn Decoder>`, so [`AnyDecoder`] is the
//! stand-in: an enum over the concrete PCM decoders that delegates the trait to
//! whichever variant is active. DSD containers are *identified* by `probe` but
//! are **not** routed here — they require an explicit
//! [`DsdMode`](crate::model::DsdMode) (see [`crate::dsd`]).

use crate::decoder::{DecodeError, Decoder, SeekableDecoder};
use crate::demux::adts::AdtsReader;
use crate::demux::mp4::{Mp4Codec, Mp4Reader};
use crate::demux::ogg::OggReader;
use crate::formats::caf::CafDecoder;
use crate::formats::matroska::MatroskaDecoder;
use crate::formats::opus::multistream::MultistreamOpusDecoder;
use crate::formats::opus::{OpusDecoder, open_opus_headers};
use crate::formats::wavpack::WavpackDecoder;
use crate::formats::{
    aac::AacDecoder, aiff::AiffDecoder, alac::AlacDecoder, flac::FlacDecoder, mp3::Mp3Decoder,
    wav::WavDecoder,
};
use crate::io::BufSource;
use crate::model::{Format, StreamInfo};
use crate::probe::probe;

/// A PCM decoder whose concrete format was chosen at runtime by [`open`].
///
/// The `Flac` variant embeds an inline, no-alloc decoded-block buffer, so it is
/// far larger than the other variants. Clippy's suggested remedy (boxing) is
/// impossible here — this crate is `#![no_std]` with no allocator — so the size
/// difference is accepted deliberately.
#[allow(clippy::large_enum_variant)]
pub enum AnyDecoder<S> {
    /// RIFF/WAVE.
    Wav(WavDecoder<S>),
    /// AIFF / AIFF-C (big-endian PCM, plus `sowt`, IEEE float, and G.711).
    Aiff(AiffDecoder<S>),
    /// Native FLAC.
    Flac(FlacDecoder<S>),
    /// MPEG Layer III.
    Mp3(Mp3Decoder<S>),
    /// FLAC in an Ogg container.
    OggFlac(FlacDecoder<OggReader<S>>),
    /// Apple Lossless in an MP4/M4A container.
    Alac(AlacDecoder<S>),
    /// AAC-LC in an MP4/M4A container or a raw ADTS (`.aac`) elementary stream.
    Aac(AacDecoder<S>),
    /// Opus audio in an Ogg container (RFC 7845/6716). Unlike Vorbis, every
    /// Opus table is RFC-fixed (no per-file setup), so it needs no
    /// caller-provided arena and is a normal by-value variant here.
    Opus(OpusDecoder<S>),
    /// Multichannel (surround) Opus — RFC 7845 mapping family 1/255. A separate,
    /// larger decoder (one core per stream); decode-only (no seek / declared
    /// length), so its `total_frames`/`seek_to` arms report `None`/`Unsupported`.
    OpusMs(MultistreamOpusDecoder<S>),
    /// Matroska / WebM carrying an audio track (`A_OPUS`/`A_FLAC` in v1). The
    /// contained codec decodes the frames; decode-only for now (Cues seeking is
    /// a follow-up), so `total_frames`/`seek_to` report `None`/`Unsupported`.
    Matroska(MatroskaDecoder<S>),
    /// WavPack (`.wv`) lossless. Length comes from the stream header; block-index
    /// seeking is a follow-up, so `seek_to` reports `Unsupported`.
    WavPack(WavpackDecoder<S>),
    /// Apple CAF carrying `lpcm` or `alac`; decode-only for now (seeking is a
    /// follow-up), so `seek_to` reports `Unsupported`.
    Caf(CafDecoder<S>),
}

impl<S: crate::io::SeekSource> AnyDecoder<S> {
    /// Total per-channel frames, if the stream declares a length. `None` for a
    /// raw MP3 without a Xing/Info/VBRI header, or for an Opus stream with no
    /// discoverable final granule.
    ///
    /// Bound on [`SeekSource`](crate::io::SeekSource) — every other format's
    /// length comes from a header parsed at `open()` time and needs no
    /// seeking, but Opus's declared length is the final Ogg page's granule,
    /// found only by a seek-and-scan
    /// ([`crate::demux::ogg_packets::OggPackets::end_granule`]); every real
    /// caller of this method already goes through [`crate::decoder::SeekableDecoder`]
    /// (see the trait impl below, and `Deck`, which requires it), so this
    /// bound costs nothing in practice while making Opus's case sound.
    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        match self {
            AnyDecoder::Wav(d) => d.total_frames(),
            AnyDecoder::Aiff(d) => d.total_frames(),
            AnyDecoder::Flac(d) => d.total_frames(),
            AnyDecoder::Mp3(d) => d.total_frames(),
            AnyDecoder::OggFlac(d) => d.total_frames(),
            AnyDecoder::Alac(d) => d.total_frames(),
            AnyDecoder::Aac(d) => d.total_frames(),
            AnyDecoder::Opus(d) => d.total_frames(),
            // Surround Opus is decode-only; no declared length.
            AnyDecoder::OpusMs(_) => None,
            AnyDecoder::Matroska(d) => d.total_frames(),
            AnyDecoder::WavPack(d) => d.total_frames(),
            AnyDecoder::Caf(d) => d.total_frames(),
        }
    }

    /// Seek to per-channel `sample`. Returns the frame actually landed on.
    /// Granularity: **sample-accurate** for WAV, FLAC (native + Ogg), ALAC, AAC,
    /// and Opus; **frame-granular** for MP3 (a short format-inherent warm-up).
    ///
    /// # Errors
    /// Propagates the underlying decoder's seek error. (Every `open()`-produced
    /// variant seeks; only DSD and bare Ogg-Vorbis — neither routed through
    /// `open` — are constructed directly and seek via their own types.)
    pub fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        match self {
            AnyDecoder::Wav(d) => d.seek_to(sample),
            AnyDecoder::Aiff(d) => d.seek_to(sample),
            AnyDecoder::Flac(d) => d.seek_to(sample),
            AnyDecoder::Mp3(d) => d.seek_to(sample),
            // FLAC-in-Ogg seeks via its SeekableDecoder impl (its source isn't a
            // bare SeekSource, so the inherent FlacDecoder::seek_to doesn't apply).
            AnyDecoder::OggFlac(d) => SeekableDecoder::seek_to(d, sample),
            AnyDecoder::Alac(d) => d.seek_to(sample),
            AnyDecoder::Aac(d) => d.seek_to(sample),
            AnyDecoder::Opus(d) => d.seek_to(sample),
            // Surround Opus seeking (per-stream pre-roll) is not implemented.
            AnyDecoder::OpusMs(_) => Err(DecodeError::Unsupported),
            // Matroska restart + forward decode-discard (Opus/FLAC/AAC).
            AnyDecoder::Matroska(d) => d.seek_to(sample),
            // WavPack rewind + forward decode-discard (block-index O(log n) is a
            // follow-up).
            AnyDecoder::WavPack(d) => d.seek_to(sample),
            // CAF: PCM is a direct byte offset; ALAC rewinds + decode-discards.
            AnyDecoder::Caf(d) => d.seek_to(sample),
        }
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to`]).
    ///
    /// # Errors
    /// As [`Self::seek_to`].
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        match self {
            AnyDecoder::Wav(d) => d.seek_to_time(secs),
            AnyDecoder::Aiff(d) => d.seek_to_time(secs),
            AnyDecoder::Flac(d) => d.seek_to_time(secs),
            AnyDecoder::Mp3(d) => d.seek_to_time(secs),
            AnyDecoder::OggFlac(d) => {
                let rate = d.stream_info().sample_rate;
                SeekableDecoder::seek_to(d, crate::decoder::secs_to_samples(secs, rate))
            }
            AnyDecoder::Alac(d) => d.seek_to_time(secs),
            AnyDecoder::Aac(d) => d.seek_to_time(secs),
            // Opus always decodes at 48 kHz; convert seconds at that rate.
            AnyDecoder::Opus(d) => d.seek_to(crate::decoder::secs_to_samples(secs, 48_000)),
            AnyDecoder::OpusMs(_) => Err(DecodeError::Unsupported),
            AnyDecoder::Matroska(d) => d.seek_to_time(secs),
            AnyDecoder::WavPack(d) => d.seek_to_time(secs),
            AnyDecoder::Caf(d) => d.seek_to_time(secs),
        }
    }
}

impl<S: crate::io::SeekSource> crate::decoder::SeekableDecoder for AnyDecoder<S> {
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        AnyDecoder::seek_to(self, sample)
    }
    fn total_frames(&self) -> Option<u64> {
        AnyDecoder::total_frames(self)
    }
}

impl<S: crate::io::Source> Decoder for AnyDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        match self {
            AnyDecoder::Wav(d) => d.stream_info(),
            AnyDecoder::Aiff(d) => d.stream_info(),
            AnyDecoder::Flac(d) => d.stream_info(),
            AnyDecoder::Mp3(d) => d.stream_info(),
            AnyDecoder::OggFlac(d) => d.stream_info(),
            AnyDecoder::Alac(d) => d.stream_info(),
            AnyDecoder::Aac(d) => d.stream_info(),
            AnyDecoder::Opus(d) => d.stream_info(),
            AnyDecoder::OpusMs(d) => d.stream_info(),
            AnyDecoder::Matroska(d) => d.stream_info(),
            AnyDecoder::WavPack(d) => d.stream_info(),
            AnyDecoder::Caf(d) => d.stream_info(),
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        match self {
            AnyDecoder::Wav(d) => d.decode_into(out),
            AnyDecoder::Aiff(d) => d.decode_into(out),
            AnyDecoder::Flac(d) => d.decode_into(out),
            AnyDecoder::Mp3(d) => d.decode_into(out),
            AnyDecoder::OggFlac(d) => d.decode_into(out),
            AnyDecoder::Alac(d) => d.decode_into(out),
            AnyDecoder::Aac(d) => d.decode_into(out),
            AnyDecoder::Opus(d) => d.decode_into(out),
            AnyDecoder::OpusMs(d) => d.decode_into(out),
            AnyDecoder::Matroska(d) => d.decode_into(out),
            AnyDecoder::WavPack(d) => d.decode_into(out),
            AnyDecoder::Caf(d) => d.decode_into(out),
        }
    }

    /// Gapless encoder delay (leading priming, per-channel samples). Non-zero
    /// for the MP4 codecs with an `elst` edit list (AAC/ALAC) and for MP3 with a
    /// LAME/Lavc info tag (its encoder delay + the MPEG decoder delay); every
    /// other variant uses the `Decoder` default of 0 (lossless, or Ogg pre-skip
    /// applied in-decoder).
    fn encoder_delay(&self) -> u64 {
        match self {
            AnyDecoder::Alac(d) => d.encoder_delay(),
            AnyDecoder::Aac(d) => d.encoder_delay(),
            AnyDecoder::Mp3(d) => d.encoder_delay(),
            AnyDecoder::Wav(_)
            | AnyDecoder::Aiff(_)
            | AnyDecoder::Flac(_)
            | AnyDecoder::OggFlac(_)
            | AnyDecoder::Opus(_)
            | AnyDecoder::OpusMs(_)
            | AnyDecoder::Matroska(_)
            | AnyDecoder::WavPack(_)
            | AnyDecoder::Caf(_) => 0,
        }
    }

    /// Gapless playable length (frames to emit after the `encoder_delay` skip).
    /// Non-`None` for MP3 (LAME/Lavc padding) and MP4 AAC/ALAC (the `elst`
    /// presented length); every other variant uses the `Decoder` default
    /// (`None` — emit to the decoder's end).
    fn playable_frames(&self) -> Option<u64> {
        match self {
            AnyDecoder::Mp3(d) => d.playable_frames(),
            AnyDecoder::Alac(d) => d.playable_frames(),
            AnyDecoder::Aac(d) => d.playable_frames(),
            AnyDecoder::Wav(_)
            | AnyDecoder::Aiff(_)
            | AnyDecoder::Flac(_)
            | AnyDecoder::OggFlac(_)
            | AnyDecoder::Opus(_)
            | AnyDecoder::OpusMs(_)
            | AnyDecoder::Matroska(_)
            | AnyDecoder::WavPack(_)
            | AnyDecoder::Caf(_) => None,
        }
    }
}

/// Probe `src`, then construct the matching PCM decoder.
///
/// # Errors
/// - [`DecodeError::Unsupported`] if the container is unknown, or is a DSD
///   container (which needs an explicit mode — see [`crate::dsd`]).
/// - Whatever the chosen decoder's `new` reports.
pub fn open<S: BufSource>(mut src: S) -> Result<AnyDecoder<S>, DecodeError> {
    match probe(&mut src)? {
        Format::Wav => WavDecoder::new(src).map(AnyDecoder::Wav),
        Format::Aiff => AiffDecoder::new(src).map(AnyDecoder::Aiff),
        Format::Flac => FlacDecoder::new(src).map(AnyDecoder::Flac),
        Format::Mp3 => Mp3Decoder::new(src).map(AnyDecoder::Mp3),
        Format::OggFlac => FlacDecoder::new(OggReader::new(src)?).map(AnyDecoder::OggFlac),
        // One MP4 container carries either codec; the `stsd` sample entry
        // decides. Parse the header once, then branch on it: `alac`→ALAC,
        // `mp4a`(AAC-LC)→AAC.
        Format::Mp4 => {
            let reader = Mp4Reader::new(src)?;
            match reader.codec() {
                Mp4Codec::Alac(_) => AlacDecoder::from_reader(reader).map(AnyDecoder::Alac),
                Mp4Codec::Aac(_) => AacDecoder::new_mp4(reader).map(AnyDecoder::Aac),
            }
        }
        // Raw ADTS (`.aac`) AAC-LC elementary stream.
        Format::Adts => AdtsReader::new(src)
            .and_then(AacDecoder::new_adts)
            .map(AnyDecoder::Aac),
        // DSD containers need an explicit DsdMode (native / DoP / decimate);
        // they are not routed through the PCM `open`.
        Format::Dsf | Format::Dff => Err(DecodeError::Unsupported),
        // Vorbis needs a caller-provided arena (`VorbisDecoder<S, A>`), and
        // `AnyDecoder`'s by-value variants don't carry a storage type-param —
        // threading one through here would ripple into `Deck`/`Engine` for
        // every other format. Vorbis is consumer-dispatched instead: build
        // `VorbisDecoder` directly (design spec, "Dispatch: per-platform, NOT
        // in `AnyDecoder`").
        Format::Vorbis => Err(DecodeError::Unsupported),
        // Opus has no per-file setup (every RFC 6716 table is fixed-capacity),
        // so — unlike Vorbis — it needs no caller-provided arena. Parse the
        // header once and branch on the channel-mapping family: 0 = mono/stereo
        // (the single-stream decoder), 1/255 = multichannel surround.
        Format::Opus => {
            let (ogg, head) = open_opus_headers(src)?;
            if head.mapping_family == 0 {
                OpusDecoder::from_parts(ogg, head).map(AnyDecoder::Opus)
            } else {
                MultistreamOpusDecoder::from_parts(ogg, head).map(AnyDecoder::OpusMs)
            }
        }
        // Matroska / WebM: parse the container, then build the contained codec's
        // decoder (Opus/FLAC in v1) — see `MatroskaDecoder::new`.
        Format::Matroska => MatroskaDecoder::new(src).map(AnyDecoder::Matroska),
        // WavPack lossless (.wv).
        Format::WavPack => WavpackDecoder::new(src).map(AnyDecoder::WavPack),
        // Apple CAF (lpcm / alac).
        Format::Caf => CafDecoder::new(src).map(AnyDecoder::Caf),
    }
}

/// Like [`open`], but for a **seekable** source — so MP4/`.m4a` with `moov`
/// *after* `mdat` (ffmpeg's default, non-faststart) is supported, not just the
/// faststart layout [`open`]'s forward walk can handle. Every other format is
/// constructed identically (they only need forward reads). Prefer this whenever
/// the source can seek (files, slices); use [`open`] only for a forward-only
/// stream (where non-faststart MP4 is inherently unreadable).
pub fn open_seekable<S: BufSource + crate::io::SeekSource>(
    mut src: S,
) -> Result<AnyDecoder<S>, DecodeError> {
    // An ID3v2 tag can front a raw ADTS (`.aac`) stream (iTunes/FAAC tag it this
    // way); the plain `probe` calls a leading `ID3` an MP3. If ADTS frames follow
    // the tag, skip it and decode the ADTS beneath.
    if let Some(start) = adts_after_id3(&mut src)? {
        let hint = adts_total_frames(&mut src, start);
        src.seek(embedded_io::SeekFrom::Start(start))
            .map_err(|_| DecodeError::Io)?;
        // `new_at(start)` so a later seek rewinds to the first ADTS frame, not to
        // byte 0 (the ID3 tag) — the latter decodes ID3 bytes as ADTS → Malformed.
        let mut dec = AacDecoder::new_adts(AdtsReader::new_at(src, start)?)?;
        dec.set_length_hint(hint);
        return Ok(AnyDecoder::Aac(dec));
    }
    match probe(&mut src)? {
        Format::Wav => WavDecoder::new(src).map(AnyDecoder::Wav),
        Format::Aiff => AiffDecoder::new(src).map(AnyDecoder::Aiff),
        Format::Flac => FlacDecoder::new(src).map(AnyDecoder::Flac),
        Format::Mp3 => Mp3Decoder::new(src).map(AnyDecoder::Mp3),
        Format::OggFlac => {
            // FLAC-in-Ogg STREAMINFO often declares `total_samples = 0` (ffmpeg's
            // streamed mux), so the decoder's length would be unknown. The real
            // length is the last Ogg page's granule — peek it (seekable source)
            // and hand it to the decoder as a fallback so `total_frames` (the
            // progress-bar length) is right. Restores `src` to the start first.
            let hint = ogg_last_granule(&mut src);
            let mut dec = FlacDecoder::new(OggReader::new(src)?)?;
            dec.set_length_hint(hint);
            Ok(AnyDecoder::OggFlac(dec))
        }
        Format::Mp4 => {
            // The only difference from `open`: accept any `moov`/`mdat` order.
            let reader = Mp4Reader::new_seekable(src)?;
            match reader.codec() {
                Mp4Codec::Alac(_) => AlacDecoder::from_reader(reader).map(AnyDecoder::Alac),
                Mp4Codec::Aac(_) => AacDecoder::new_mp4(reader).map(AnyDecoder::Aac),
            }
        }
        Format::Adts => {
            // Raw ADTS declares no length. On a seekable source, scan the frame
            // headers for a total (frames × 1024, as the tag reader does) so the
            // playback progress bar has a length. Restores `src` to the start.
            let hint = adts_total_frames(&mut src, 0);
            let mut dec = AacDecoder::new_adts(AdtsReader::new(src)?)?;
            dec.set_length_hint(hint);
            Ok(AnyDecoder::Aac(dec))
        }
        Format::Dsf | Format::Dff => Err(DecodeError::Unsupported),
        Format::Vorbis => Err(DecodeError::Unsupported),
        Format::Opus => {
            let (ogg, head) = open_opus_headers(src)?;
            if head.mapping_family == 0 {
                OpusDecoder::from_parts(ogg, head).map(AnyDecoder::Opus)
            } else {
                MultistreamOpusDecoder::from_parts(ogg, head).map(AnyDecoder::OpusMs)
            }
        }
        Format::Matroska => MatroskaDecoder::new(src).map(AnyDecoder::Matroska),
        Format::WavPack => WavpackDecoder::new(src).map(AnyDecoder::WavPack),
        Format::Caf => CafDecoder::new(src).map(AnyDecoder::Caf),
    }
}

/// Count raw-ADTS frames on a seekable source and return total per-channel
/// samples (`frames × 1024`, matching the tag reader's duration), restoring
/// `src` to the start. Each frame's 13-bit `aac_frame_length` (header bytes
/// 3..6) is the stride; a non-sync/short header ends the count. `None` if no
/// frame is found.
fn adts_total_frames<S: crate::io::SeekSource>(src: &mut S, start: u64) -> Option<u64> {
    use embedded_io::SeekFrom;
    src.seek(SeekFrom::Start(start)).ok()?;
    let mut frames: u64 = 0;
    let mut h = [0u8; 7];
    loop {
        let mut off = 0;
        while off < 7 {
            match src.read(&mut h[off..]).ok()? {
                0 => break,
                n => off += n,
            }
        }
        if off < 7 {
            break; // clean end (or truncated tail)
        }
        // ADTS syncword 0xFFF + layer 00 (the 0x06 mask rules out an MP3 frame).
        if h[0] != 0xFF || (h[1] & 0xF6) != 0xF0 {
            break;
        }
        let frame_len =
            ((u32::from(h[3]) & 0x03) << 11) | (u32::from(h[4]) << 3) | (u32::from(h[5]) >> 5);
        if frame_len < 7 {
            break;
        }
        frames += 1;
        if src
            .seek(SeekFrom::Current(i64::from(frame_len) - 7))
            .is_err()
        {
            break;
        }
    }
    src.seek(SeekFrom::Start(start)).ok()?;
    (frames > 0).then_some(frames * 1024)
}

/// If `src` starts with an ID3v2 tag immediately followed by a raw ADTS stream
/// (the iTunes/FAAC way of tagging `.aac`), return the byte offset of the first
/// ADTS frame (past the tag). Restores `src` to the start. `None` otherwise.
fn adts_after_id3<S: crate::io::SeekSource>(src: &mut S) -> Result<Option<u64>, DecodeError> {
    use embedded_io::SeekFrom;
    src.seek(SeekFrom::Start(0)).map_err(|_| DecodeError::Io)?;
    let mut hdr = [0u8; 10];
    let n = read_up_to(src, &mut hdr)?;
    let mut result = None;
    if n == 10 && &hdr[0..3] == b"ID3" {
        let size = (u32::from(hdr[6] & 0x7F) << 21)
            | (u32::from(hdr[7] & 0x7F) << 14)
            | (u32::from(hdr[8] & 0x7F) << 7)
            | u32::from(hdr[9] & 0x7F);
        let start = 10u64 + u64::from(size);
        if src.seek(SeekFrom::Start(start)).is_ok() {
            let mut s = [0u8; 2];
            if read_up_to(src, &mut s)? == 2 && s[0] == 0xFF && (s[1] & 0xF6) == 0xF0 {
                result = Some(start);
            }
        }
    }
    src.seek(SeekFrom::Start(0)).map_err(|_| DecodeError::Io)?;
    Ok(result)
}

/// Read up to `buf.len()` bytes; returns how many were read (< len at EOF).
fn read_up_to<S: crate::io::Source>(src: &mut S, buf: &mut [u8]) -> Result<usize, DecodeError> {
    let mut off = 0;
    while off < buf.len() {
        match embedded_io::Read::read(src, &mut buf[off..]).map_err(|_| DecodeError::Io)? {
            0 => break,
            k => off += k,
        }
    }
    Ok(off)
}

/// Granule position of the last real Ogg page — for FLAC-in-Ogg the decoded
/// per-channel sample count at end of stream. Scans only a bounded tail window
/// (an Ogg page tops out ~64 KiB) backward for the final `OggS` whose granule
/// is a real end-of-packet value (not `0` = a header page, nor `u64::MAX` = no
/// packet finished). Restores `src` to the start so the caller's header parse
/// sees it unmoved. `None` on any I/O failure or if no such page is found —
/// the length then simply stays unknown rather than erroring.
fn ogg_last_granule<S: crate::io::SeekSource>(src: &mut S) -> Option<u64> {
    use embedded_io::SeekFrom;
    const WINDOW: usize = 65536;
    let len = src.seek(SeekFrom::End(0)).ok()?;
    let mut found = None;
    let window = len.min(WINDOW as u64) as usize;
    if window >= 14 {
        src.seek(SeekFrom::Start(len - window as u64)).ok()?;
        let mut buf = [0u8; WINDOW];
        let mut off = 0;
        while off < window {
            match src.read(&mut buf[off..window]).ok()? {
                0 => break,
                n => off += n,
            }
        }
        let b = &buf[..off];
        if b.len() >= 14 {
            let mut idx = b.len() - 14;
            loop {
                if &b[idx..idx + 4] == b"OggS" {
                    let g = u64::from_le_bytes(b[idx + 6..idx + 14].try_into().unwrap());
                    if g != u64::MAX && g != 0 {
                        found = Some(g);
                        break;
                    }
                }
                if idx == 0 {
                    break;
                }
                idx -= 1;
            }
        }
    }
    // Rewind for the subsequent `OggReader::new` header parse (probe left us at 0).
    src.seek(SeekFrom::Start(0)).ok()?;
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// A minimal valid 16-bit mono WAV carrying `samples`.
    fn wav16(samples: &[i16]) -> Vec<u8> {
        let mut data = Vec::new();
        for s in samples {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&8000u32.to_le_bytes());
        v.extend_from_slice(&16000u32.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);
        v
    }

    #[test]
    fn open_routes_and_decodes_wav() {
        let wav = wav16(&[100, -100, 32767]);
        let mut dec = open(SliceSource::new(&wav)).expect("probe + construct");
        assert!(matches!(dec, AnyDecoder::Wav(_))); // routed to the WAV variant
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &[100, -100, 32767]); // real PCM, delegated through AnyDecoder
    }

    /// A minimal valid FLAC: signature + a STREAMINFO block (44.1 kHz stereo 16-bit).
    fn flac_min() -> Vec<u8> {
        let mut si = [0u8; 34];
        let v: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36); // ch-1=1, bps-1=15
        si[10..18].copy_from_slice(&v.to_be_bytes());
        let mut b = Vec::new();
        b.extend_from_slice(b"fLaC");
        b.push(0x80); // last block + STREAMINFO
        b.extend_from_slice(&[0, 0, 34]);
        b.extend_from_slice(&si);
        b
    }

    #[test]
    fn open_routes_flac_with_real_stream_info() {
        let bytes = flac_min();
        let dec = open(SliceSource::new(&bytes)).expect("probe + construct");
        assert_eq!(dec.stream_info().sample_rate, 44_100);
        assert_eq!(dec.stream_info().channels, 2);
        assert!(matches!(dec, AnyDecoder::Flac(_)));
    }

    #[test]
    fn open_rejects_dsd_containers_without_a_mode() {
        // DSF is a valid container but needs an explicit DsdMode, so `open` (the
        // PCM path) declines it.
        assert_eq!(
            open(SliceSource::new(b"DSD \0\0\0\0")).err(),
            Some(DecodeError::Unsupported)
        );
    }

    #[test]
    fn any_decoder_reports_total_frames_and_seeks() {
        // 8 mono frames; total is known from the data-chunk size, and seek lands
        // exactly (WAV is sample-accurate).
        let wav = wav16(&[10, 20, 30, 40, 50, 60, 70, 80]);
        let mut dec = open(SliceSource::new(&wav)).expect("open");
        assert_eq!(dec.total_frames(), Some(8));
        assert_eq!(dec.seek_to(4).unwrap(), 4);
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &[50, 60, 70, 80]);
        // seek_to_time on a 8 kHz stream: 0.0005 s = frame 4.
        assert_eq!(dec.seek_to_time(0.0005).unwrap(), 4);
    }

    #[test]
    fn open_rejects_vorbis_not_being_an_anydecoder_variant() {
        // Vorbis is consumer-dispatched (needs a caller-provided arena that
        // `AnyDecoder`'s by-value variants don't carry) — `open()` must
        // decline it cleanly, not panic or silently misdetect it.
        let mut packet = vec![1u8];
        packet.extend_from_slice(b"vorbis");
        packet.extend_from_slice(&[0u8; 23]);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"OggS");
        bytes.push(0);
        bytes.push(0x02);
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.push(1);
        bytes.push(packet.len() as u8);
        bytes.extend_from_slice(&packet);
        assert_eq!(
            open(SliceSource::new(&bytes)).err(),
            Some(DecodeError::Unsupported)
        );
    }

    #[test]
    fn open_rejects_unknown() {
        assert_eq!(
            open(SliceSource::new(b"XXXXYYYY")).err(),
            Some(DecodeError::Unsupported)
        );
    }

    #[test]
    fn open_routes_opus_with_48k_stream_info() {
        // The real fixture (CELT-only fullband, per the design spec's fixture
        // naming): `open()` must route it to `AnyDecoder::Opus` and report
        // 48 kHz/stereo/Int16 (Opus always decodes at 48 kHz regardless of the
        // container's informational input rate).
        let bytes = include_bytes!("../../../testdata/tone.opus");
        let mut dec = open(SliceSource::new(bytes)).expect("probe + construct Opus");
        assert!(matches!(dec, AnyDecoder::Opus(_)));
        assert_eq!(dec.stream_info().sample_rate, 48_000);
        assert_eq!(dec.stream_info().channels, 2);
        assert_eq!(
            dec.stream_info().format,
            crate::model::SampleFormat::Int { bits: 16 }
        );
        // Task 8: CELT decode is live — the fixture yields real interleaved
        // PCM (bit-level fidelity vs ffmpeg is the golden in tests/decode.rs).
        let mut out = [7i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 8, "a full CELT frame fills the whole 8-sample request");
        assert!(
            out.iter().all(|&s| (-32768..=32767).contains(&s)),
            "decoded samples are 16-bit range"
        );
        // Opus seeking is implemented (Task 13): seek-to-0 returns to the start
        // (output sample 0), routed through `AnyDecoder::Opus`.
        assert_eq!(dec.seek_to(0).unwrap(), 0);
    }

    #[test]
    fn open_routes_and_decodes_alac() {
        let bytes = include_bytes!("../testdata/alac.m4a");
        let mut dec = open(crate::io::SliceSource::new(bytes)).expect("probe + construct");
        assert!(matches!(dec, AnyDecoder::Alac(_)));
        assert_eq!(dec.stream_info().sample_rate, 44_100);
        assert_eq!(dec.total_frames(), Some(13_230));
        let mut buf = [0i32; 16];
        let n = dec.decode_into(&mut buf).unwrap();
        assert!(n > 0);
        // ALAC seeking is sample-accurate (routed through `AnyDecoder::Alac`).
        assert_eq!(dec.seek_to(0).unwrap(), 0, "seek-to-0 lands at the start");
        assert_eq!(
            dec.seek_to(4_000).unwrap(),
            4_000,
            "sample-accurate landing"
        );
        assert_eq!(
            dec.seek_to(1_000_000).unwrap(),
            13_230,
            "out-of-range clamps"
        );
    }
}
