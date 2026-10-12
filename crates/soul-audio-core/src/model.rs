//! The format-independent data model: [`StreamInfo`], [`SampleFormat`],
//! [`DsdRate`], [`Format`], [`DsdMode`].
//!
//! These types *describe* a stream and the decode pipeline's shape. They carry
//! no behaviour beyond small helpers, and they are decoupled from both the byte
//! [`Source`](crate::Source) and the concrete decoders — the two axes meet only
//! at the decoder traits and the wiring (`probe` / `AnyDecoder`).

/// DSD bit-rate family, expressed as a multiple of the 44.1 kHz base rate.
///
/// DSD is 1-bit sigma-delta clocked in the MHz range; the "rate" is how many
/// bits per second the stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsdRate {
    /// 2.8224 MHz (44.1 kHz × 64).
    Dsd64,
    /// 5.6448 MHz (44.1 kHz × 128).
    Dsd128,
    /// 11.2896 MHz (44.1 kHz × 256).
    Dsd256,
    /// 22.5792 MHz (44.1 kHz × 512).
    Dsd512,
}

impl DsdRate {
    /// The 1-bit stream rate in Hz (44.1 kHz base).
    #[must_use]
    pub const fn hz(self) -> u32 {
        const BASE: u32 = 44_100;
        match self {
            DsdRate::Dsd64 => BASE * 64,
            DsdRate::Dsd128 => BASE * 128,
            DsdRate::Dsd256 => BASE * 256,
            DsdRate::Dsd512 => BASE * 512,
        }
    }
}

/// What kind of samples a decoder emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SampleFormat {
    /// Signed integer PCM with `bits` significant bits, widened into `i32`.
    /// Covers WAV (integer), FLAC, and MP3.
    Int { bits: u8 },
    /// IEEE float PCM with `bits` bits (32/64). Modelled now; float decode is a
    /// later slice.
    Float { bits: u8 },
    /// Native 1-bit DSD, packed 8 samples per byte.
    Dsd { rate: DsdRate },
}

impl SampleFormat {
    /// Significant bits per sample. For DSD this is `1`.
    #[must_use]
    pub const fn bits_per_sample(self) -> u8 {
        match self {
            SampleFormat::Int { bits } | SampleFormat::Float { bits } => bits,
            SampleFormat::Dsd { .. } => 1,
        }
    }

    /// Whether this stream is native DSD (as opposed to multi-bit PCM).
    #[must_use]
    pub const fn is_dsd(self) -> bool {
        matches!(self, SampleFormat::Dsd { .. })
    }
}

/// Format-independent description of a decoded stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    /// PCM frame rate (samples per second per channel). For DSD, the base-family
    /// rate (e.g. 44_100); the 1-bit rate is `format`'s [`DsdRate::hz`].
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// What the decoder emits.
    pub format: SampleFormat,
}

impl StreamInfo {
    /// Convenience: significant bits per sample of [`Self::format`].
    #[must_use]
    pub const fn bits_per_sample(&self) -> u8 {
        self.format.bits_per_sample()
    }
}

/// Container identity, as returned by [`crate::probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    /// RIFF/WAVE.
    Wav,
    /// AIFF / AIFF-C (`FORM`…`AIFF`/`AIFC`). Big-endian PCM, plus AIFF-C `sowt`
    /// (little-endian), `fl32`/`fl64` (IEEE float), and `ulaw`/`alaw`. A normal
    /// [`AnyDecoder`](crate::dispatch::AnyDecoder) variant (see
    /// `formats::aiff::AiffDecoder`).
    Aiff,
    /// Native FLAC.
    Flac,
    /// MPEG-1/2 Audio Layer III.
    Mp3,
    /// DSF (Sony DSD stream file).
    Dsf,
    /// DFF / DSDIFF.
    Dff,
    /// FLAC in an Ogg container.
    OggFlac,
    /// MP4/M4A container (ALAC audio in v1).
    Mp4,
    /// Raw ADTS (`.aac`) elementary stream (AAC-LC).
    Adts,
    /// Vorbis I audio in an Ogg container. **Consumer-dispatched**, not an
    /// [`AnyDecoder`](crate::dispatch::AnyDecoder) variant: `open()` declines
    /// it with [`crate::DecodeError::Unsupported`] (see
    /// `formats::vorbis::VorbisDecoder`, which needs a caller-provided arena
    /// that `open()`'s by-value decoders don't carry).
    Vorbis,
    /// Opus audio in an Ogg container (RFC 7845). Unlike Vorbis, every Opus
    /// table is RFC-fixed (no per-file setup header), so it needs no
    /// caller-provided arena and **is** a normal
    /// [`AnyDecoder`](crate::dispatch::AnyDecoder) variant (see
    /// `formats::opus::OpusDecoder`).
    Opus,
}

/// Explicit DSD output mode chosen by the caller. Selects which path a native
/// DSD stream takes to reach playback (see the DSD adapters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsdMode {
    /// Emit the raw 1-bit stream (fed to a DSD-capable DAC via SAI).
    Native,
    /// Wrap the 1-bit stream in DoP frames (still DSD; needs a DoP DAC).
    Dop,
    /// Decimate to real multi-bit PCM (plays on any device).
    Decimate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsd_rate_hz_scales_with_family() {
        assert_eq!(DsdRate::Dsd64.hz(), 2_822_400);
        assert_eq!(DsdRate::Dsd128.hz(), 5_644_800);
        assert_eq!(DsdRate::Dsd256.hz(), 11_289_600);
        assert_eq!(DsdRate::Dsd512.hz(), 22_579_200);
    }

    #[test]
    fn bits_per_sample_reads_through_format() {
        let pcm = StreamInfo {
            sample_rate: 44_100,
            channels: 2,
            format: SampleFormat::Int { bits: 16 },
        };
        assert_eq!(pcm.bits_per_sample(), 16);

        let dsd = StreamInfo {
            sample_rate: 44_100,
            channels: 2,
            format: SampleFormat::Dsd {
                rate: DsdRate::Dsd64,
            },
        };
        assert_eq!(dsd.bits_per_sample(), 1);
        assert!(dsd.format.is_dsd());
        assert!(!pcm.format.is_dsd());
    }
}
