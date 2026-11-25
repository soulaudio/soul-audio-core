//! Small byte-parsing helpers shared across container/tag readers.

use crate::{Field, TagError, TagSink, locate};

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

/// Map a text key to the MusicBrainz **recording** MBID field, case-insensitively.
/// Recognizes the (confusingly-named) recording-id keys shared across containers —
/// VorbisComment `MUSICBRAINZ_TRACKID`, MP4 freeform `MusicBrainz Track Id`, and
/// the occasional ID3 `TXXX:MusicBrainz Recording Id`. Deliberately does NOT match
/// the *release-track* id (`MUSICBRAINZ_RELEASETRACKID` / `MusicBrainz Release
/// Track Id`), which is a different identifier. (ID3 `UFID` is owner-based, handled
/// in the id3v2 reader.)
pub(crate) fn map_musicbrainz_recording_key(key: &str) -> Option<Field> {
    let k = key.trim();
    (k.eq_ignore_ascii_case("MUSICBRAINZ_TRACKID")
        || k.eq_ignore_ascii_case("MusicBrainz Track Id")
        || k.eq_ignore_ascii_case("MusicBrainz Recording Id"))
    .then_some(Field::MusicBrainzRecordingId)
}

/// Split an `n/total` number tag (`"1/3"`, `" 05 / 12 "`) into its trimmed
/// number and, when non-empty, its total. A bare number has no total. ID3
/// `TRCK`/`TPOS`, VorbisComment and APE `TRACKNUMBER`/`DISCNUMBER` all use this
/// form.
pub(crate) fn split_num_total(s: &str) -> (&str, Option<&str>) {
    match s.split_once('/') {
        Some((n, t)) => (n.trim(), Some(t.trim()).filter(|t| !t.is_empty())),
        None => (s.trim(), None),
    }
}

/// Hand one text value to `sink`, splitting a `TrackNumber`/`DiscNumber` value
/// of the form `n/total` into the number plus a `TrackTotal`/`DiscTotal` call
/// (the total only when present). Every other field passes through verbatim.
pub(crate) fn emit_text(sink: &mut impl TagSink, field: Option<Field>, raw_key: &str, value: &str) {
    let total = match field {
        Some(Field::TrackNumber) => Field::TrackTotal,
        Some(Field::DiscNumber) => Field::DiscTotal,
        _ => {
            sink.text(field, raw_key, value);
            return;
        }
    };
    let (number, total_value) = split_num_total(value);
    sink.text(field, raw_key, number);
    if let Some(t) = total_value {
        sink.text(Some(total), raw_key, t);
    }
}

/// Parse a track/disc number or total, tolerating whitespace and a stray
/// `n/total` form (`"2/3"` -> 2). `None` on garbage. `alloc`-gated (only the
/// owned snapshot parses values; see [`parse_gain_db`]).
#[cfg(feature = "alloc")]
pub(crate) fn parse_count(s: &str) -> Option<u32> {
    split_num_total(s).0.parse().ok()
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

#[cfg(test)]
mod num_total_tests {
    use super::split_num_total;

    #[test]
    fn splits_number_and_total() {
        assert_eq!(split_num_total("1/3"), ("1", Some("3")));
        assert_eq!(split_num_total(" 2 / 4 "), ("2", Some("4")));
        assert_eq!(split_num_total("05/"), ("05", None));
        assert_eq!(split_num_total("7"), ("7", None));
        assert_eq!(split_num_total(" 01 "), ("01", None));
    }
}
