//! Small byte-parsing helpers shared across container/tag readers.

use crate::{Field, TagError, locate};

/// Map a ReplayGain/R128 tag key to its [`Field`], case-insensitively. The key
/// spelling is identical across VorbisComment keys, ID3 `TXXX` descriptions and
/// MP4 `----` freeform names, so all three containers share this one mapping.
/// Covers `REPLAYGAIN_{TRACK,ALBUM}_{GAIN,PEAK}` and `R128_{TRACK,ALBUM}_GAIN`.
pub(crate) fn map_replaygain_key(key: &str) -> Option<Field> {
    if key.eq_ignore_ascii_case("REPLAYGAIN_TRACK_GAIN") {
        Some(Field::ReplayGainTrackGain)
    } else if key.eq_ignore_ascii_case("REPLAYGAIN_ALBUM_GAIN") {
        Some(Field::ReplayGainAlbumGain)
    } else if key.eq_ignore_ascii_case("REPLAYGAIN_TRACK_PEAK") {
        Some(Field::ReplayGainTrackPeak)
    } else if key.eq_ignore_ascii_case("REPLAYGAIN_ALBUM_PEAK") {
        Some(Field::ReplayGainAlbumPeak)
    } else if key.eq_ignore_ascii_case("R128_TRACK_GAIN") {
        Some(Field::R128TrackGain)
    } else if key.eq_ignore_ascii_case("R128_ALBUM_GAIN") {
        Some(Field::R128AlbumGain)
    } else {
        None
    }
}

/// Parse a ReplayGain gain string like `"-7.89 dB"` (or a bare `"-7.89"`) into
/// decibels: trim whitespace, strip a trailing case-insensitive `"dB"` unit,
/// then parse the leading signed float. `None` if no finite float is present.
/// `str::parse::<f32>` is available in `core` (no `std`/`libm`).
///
/// Only the owned ([`alloc`]) snapshot layer parses tag values into typed form;
/// the no-alloc streaming [`crate::TagSink`] surfaces raw strings and leaves
/// parsing to the consumer, so these helpers are `alloc`-gated.
#[cfg(feature = "alloc")]
pub(crate) fn parse_gain_db(s: &str) -> Option<f32> {
    let t = s.trim();
    let b = t.as_bytes();
    let n = b.len();
    // Strip a trailing "dB"/"DB"/"db"/"Db". `b[n-2]` being an ASCII letter
    // guarantees `n-2` is a char boundary, so the slice can't split a codepoint.
    let num = if n >= 2 && (b[n - 1] | 0x20) == b'b' && (b[n - 2] | 0x20) == b'd' {
        t[..n - 2].trim_end()
    } else {
        t
    };
    num.parse::<f32>().ok().filter(|v| v.is_finite())
}

/// Parse a ReplayGain peak — a bare linear full-scale float like `"0.988"`
/// (1.0 = 0 dBFS). `None` on garbage. `alloc`-gated (see [`parse_gain_db`]).
#[cfg(feature = "alloc")]
pub(crate) fn parse_peak(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok().filter(|v| v.is_finite())
}

/// Parse an R128 gain tag: a signed **integer** in Q7.8 fixed-point dB
/// (`gain_db = value / 256`), referenced to -23 LUFS. Rejects non-integers.
/// `alloc`-gated (see [`parse_gain_db`]).
#[cfg(feature = "alloc")]
pub(crate) fn parse_r128_gain_db(s: &str) -> Option<f32> {
    s.trim().parse::<i32>().ok().map(|v| v as f32 / 256.0)
}

/// Read exactly `buf.len()` bytes, treating a short read (EOF) as malformed.
pub(crate) fn read_exact<S: embedded_io::Read>(
    src: &mut S,
    buf: &mut [u8],
) -> Result<(), TagError> {
    let n = locate::read_full(src, buf)?;
    if n < buf.len() {
        Err(TagError::Malformed)
    } else {
        Ok(())
    }
}

/// The longest valid UTF-8 prefix of `bytes` (never panics on a boundary cut
/// mid-codepoint, or on bytes that aren't UTF-8 at all).
pub(crate) fn utf8_prefix(bytes: &[u8]) -> &str {
    match core::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or(""),
    }
}

/// Read and discard `n` bytes from a forward-only source, in fixed-size
/// chunks (no allocation, no seek required). Shared by `vorbis` (comment
/// vector vendor string / oversized comment tails) and `ogg` (packet bytes
/// that overflow a caller-provided reassembly buffer).
pub(crate) fn discard<S: embedded_io::Read>(src: &mut S, mut n: usize) -> Result<(), TagError> {
    let mut junk = [0u8; 64];
    while n > 0 {
        let chunk = n.min(junk.len());
        read_exact(src, &mut junk[..chunk])?;
        n -= chunk;
    }
    Ok(())
}

#[cfg(all(test, feature = "alloc"))]
mod tests {
    use super::*;

    #[test]
    fn parse_gain_db_strips_db_unit() {
        assert_eq!(parse_gain_db("-7.89 dB"), Some(-7.89));
        assert_eq!(parse_gain_db("3.21 dB"), Some(3.21));
        assert_eq!(parse_gain_db("0.00 dB"), Some(0.0));
        // No unit, extra whitespace, uppercase/mixed-case unit.
        assert_eq!(parse_gain_db("  -6.00  "), Some(-6.0));
        assert_eq!(parse_gain_db("+2.5 DB"), Some(2.5));
        assert_eq!(parse_gain_db("1.0dB"), Some(1.0));
        // Garbage is rejected.
        assert_eq!(parse_gain_db("loud"), None);
        assert_eq!(parse_gain_db(""), None);
        assert_eq!(parse_gain_db("dB"), None);
    }

    #[test]
    fn parse_peak_reads_a_bare_float() {
        assert_eq!(parse_peak("0.988"), Some(0.988));
        assert_eq!(parse_peak("1.000000"), Some(1.0));
        assert_eq!(parse_peak(" 0.5 "), Some(0.5));
        assert_eq!(parse_peak("nope"), None);
    }

    #[test]
    fn parse_r128_is_q7_8_fixed_point() {
        // -2314 / 256 = -9.0390625 dB.
        let g = parse_r128_gain_db("-2314").unwrap();
        assert!((g - (-9.04)).abs() < 0.01, "got {g}");
        assert_eq!(parse_r128_gain_db("0"), Some(0.0));
        assert_eq!(parse_r128_gain_db("256"), Some(1.0));
        assert_eq!(parse_r128_gain_db("0.5"), None); // not an integer
        assert_eq!(parse_r128_gain_db("x"), None);
    }
}
