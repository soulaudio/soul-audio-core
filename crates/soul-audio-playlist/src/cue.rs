//! `.cue` sheet parsing: a single-file album as individual, sample-accurate
//! tracks.
//!
//! A cue sheet is the index of a CD rip kept as one audio file: it names the
//! file(s), the tracks inside them, and each track's start as a CD timestamp.
//! Ripping a disc to `album.flac` + `album.cue` is a standard audiophile
//! workflow (it keeps the disc's exact gapless stream intact — no per-track
//! re-encoding, no boundary guessing), and a player that cannot read the cue
//! shows one 50-minute "track".
//!
//! This module is the parse and the arithmetic:
//!
//! - [`parse`] reads the sheet into a [`CueSheet`] — disc-level metadata, each
//!   `FILE`, and each `TRACK` with its own title/performer/ISRC and index
//!   points.
//! - [`CueTime`] is a CD timestamp (`MM:SS:FF`, 75 frames per second) and
//!   converts to **audio frames** at the file's own sample rate, exactly where
//!   the rate allows ([`CueTime::to_frames`]).
//! - [`CueFile::ranges`] turns the tracks of one file into playable
//!   `(start, length)` ranges: each track runs to the next one's start, the
//!   last to the end of the file. Feed those to a sub-range decoder
//!   (`soul_audio_core::SubRange`) and the existing gapless engine
//!   plays the album with sample-exact boundaries.
//!
//! Lives in the playlist crate because a cue sheet *is* a playlist over one
//! file: `no_std` + `alloc`, no filesystem access, shared by the desktop host
//! and the DAP firmware.
//!
//! # Encoding
//!
//! [`parse`] takes `&str`: cue sheets in the wild are UTF-8, Windows-1252 or
//! UTF-16, and deciding which is the caller's job (it has the bytes and, often,
//! the user's locale). Parsing is ASCII-driven — commands, numbers and
//! separators are all ASCII — so a mis-decoded sheet still splits the audio
//! correctly, it just carries mojibake in the titles.

use alloc::string::String;
use alloc::vec::Vec;

/// CD frames per second — the resolution of every cue timestamp (a "frame"
/// here is a CD sector: 588 stereo samples at 44.1 kHz).
pub const CD_FRAMES_PER_SEC: u64 = 75;

/// The largest timestamp [`CueTime::from_mm_ss_ff`] accepts, in CD frames —
/// about 115 years of audio.
///
/// Not a limitation anyone meets: a day-long single file is 6.5 million
/// frames, four orders of magnitude under this. It exists because a corrupt or
/// hand-edited sheet can carry a 15-digit minute field, and a number that large
/// is not a timestamp — refusing it (the sheet is then reported as malformed and
/// the album imports as its plain file) is better than splitting the audio
/// somewhere nobody meant.
pub const MAX_CUE_FRAMES: u64 = 1 << 38;

/// A cue timestamp, as a count of CD frames from the start of the file.
///
/// `MM:SS:FF` with 75 frames per second. Minutes are not capped at 99: a
/// sheet for a long single file legitimately runs past it, and clamping would
/// silently mis-split the audio.
///
/// `frames` is public, so a value can also arrive from a caller or a
/// deserialized profile rather than from the parser — every conversion below is
/// therefore **total**, saturating rather than overflowing for any `u64`. An
/// arithmetic panic in a library scan would take out someone's whole collection
/// over one bad file.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CueTime {
    /// CD frames (1/75 s) from the start of the file.
    pub frames: u64,
}

impl CueTime {
    /// From `MM:SS:FF` components. `None` if seconds or frames are out of
    /// range (`SS > 59`, `FF > 74`) — that is a malformed sheet, not a long
    /// one, and accepting it would place the split somewhere else than the
    /// ripper meant — or if the total exceeds [`MAX_CUE_FRAMES`].
    #[must_use]
    pub fn from_mm_ss_ff(minutes: u64, seconds: u64, frames: u64) -> Option<Self> {
        if seconds > 59 || frames > CD_FRAMES_PER_SEC - 1 {
            return None;
        }
        // Checked throughout: a corrupt sheet's 15-digit minute field must be
        // refused, not wrapped into a plausible-looking offset.
        let total = minutes
            .checked_mul(60)?
            .checked_add(seconds)?
            .checked_mul(CD_FRAMES_PER_SEC)?
            .checked_add(frames)?;
        (total <= MAX_CUE_FRAMES).then_some(Self { frames: total })
    }

    /// The `(minutes, seconds, frames)` this timestamp displays as.
    #[must_use]
    pub fn mm_ss_ff(self) -> (u64, u64, u64) {
        let secs = self.frames / CD_FRAMES_PER_SEC;
        (secs / 60, secs % 60, self.frames % CD_FRAMES_PER_SEC)
    }

    /// This timestamp in **audio frames** at `sample_rate`:
    /// `frames · rate / 75`.
    ///
    /// Exact for every rate that is a multiple of 75 Hz — which is every rate
    /// a CD rip can be in (44 100 = 588 per CD frame, 48 000 = 640, 96 000 =
    /// 1280, …). For an exotic rate that is not, the result is rounded to
    /// nearest; see [`Self::divides_evenly`].
    #[must_use]
    pub fn to_frames(self, sample_rate: u32) -> u64 {
        // `u128` intermediate: `frames` is a public field, so this has to be
        // total for every `u64` — and the product of a large timestamp and a
        // high rate leaves `u64` long before it stops being meaningful.
        // Rounds to nearest, so a non-multiple-of-75 rate cannot drift a whole
        // sample per track, and saturates rather than wrapping.
        let num =
            u128::from(self.frames) * u128::from(sample_rate) + u128::from(CD_FRAMES_PER_SEC) / 2;
        u64::try_from(num / u128::from(CD_FRAMES_PER_SEC)).unwrap_or(u64::MAX)
    }

    /// Whether `sample_rate` divides evenly into CD frames, i.e. whether
    /// [`Self::to_frames`] is exact rather than rounded.
    #[must_use]
    pub fn divides_evenly(sample_rate: u32) -> bool {
        u64::from(sample_rate) % CD_FRAMES_PER_SEC == 0
    }

    /// This timestamp in milliseconds (for a duration display).
    #[must_use]
    pub fn as_millis(self) -> u64 {
        let ms = u128::from(self.frames) * 1000 / u128::from(CD_FRAMES_PER_SEC);
        u64::try_from(ms).unwrap_or(u64::MAX)
    }
}

/// One `TRACK` of a cue sheet.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CueTrack {
    /// The number the sheet gives it (`TRACK 01 AUDIO` → 1). Not necessarily
    /// its position in [`CueFile::tracks`] — a sheet may start at 2 (a
    /// continued disc) or skip a number.
    pub number: u16,
    /// `AUDIO`, `MODE1/2352`, … Uppercased as written. A non-`AUDIO` track is
    /// data (a mixed-mode disc's first track) and carries no playable audio.
    pub kind: String,
    /// `TITLE`, if the track names one.
    pub title: Option<String>,
    /// `PERFORMER`, if the track names one (else the sheet's).
    pub performer: Option<String>,
    /// `SONGWRITER`, if present.
    pub songwriter: Option<String>,
    /// `ISRC`, if present.
    pub isrc: Option<String>,
    /// `INDEX 01` — where the track's audio starts. The one index that
    /// matters for playback; `None` only in a malformed sheet (such a track is
    /// skipped by [`CueFile::ranges`]).
    pub start: Option<CueTime>,
    /// `INDEX 00` — the pregap's start, when the sheet marks one. The gap
    /// belongs to the *previous* track's tail by convention, so playback
    /// starts at [`Self::start`]; this is kept for a player that wants to
    /// offer "include the pregap".
    pub pregap_start: Option<CueTime>,
    /// Any further index points (`INDEX 02`+), in sheet order — sub-index
    /// markers a few rips carry.
    pub extra_indexes: Vec<(u16, CueTime)>,
}

impl CueTrack {
    /// Whether this track carries audio (`TRACK nn AUDIO`). A data track on a
    /// mixed-mode disc does not, and must not be played.
    #[must_use]
    pub fn is_audio(&self) -> bool {
        self.kind.eq_ignore_ascii_case("AUDIO")
    }
}

/// One `FILE` of a cue sheet, with the tracks inside it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CueFile {
    /// The file name as written in the sheet, relative to the cue's folder.
    pub name: String,
    /// The declared type (`WAVE`, `FLAC`, `MP3`, `BINARY`, …), as written.
    /// Rippers lie about this constantly (`WAVE` for a FLAC), so a player
    /// should sniff the file rather than trust it.
    pub format: String,
    /// The tracks inside this file, in sheet order.
    pub tracks: Vec<CueTrack>,
}

/// A playable slice of one audio file: an audio-frame offset and length,
/// paired with the track it came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CueRange {
    /// Index into [`CueFile::tracks`].
    pub track: usize,
    /// First audio frame of the track, from the start of the file.
    pub start_frame: u64,
    /// Length in audio frames, or `None` for "to the end of the file" (the
    /// last track, when the file's length is not known).
    pub frames: Option<u64>,
}

impl CueFile {
    /// The playable ranges of this file's **audio** tracks, in sheet order, at
    /// `sample_rate`.
    ///
    /// Each track runs from its `INDEX 01` to the next audio track's
    /// `INDEX 01`; the last runs to `total_frames` when the caller knows the
    /// file's length (from the decoder), else to the end of the file
    /// (`frames: None`). Tracks with no `INDEX 01`, and data tracks, are
    /// skipped — they have no audio to play.
    ///
    /// A start past `total_frames`, or a sheet whose starts are not ascending
    /// (a malformed or mis-decoded cue), yields a zero-length range rather
    /// than a wrapped one: a bad sheet must not be able to make the player
    /// read backwards or seek past the end.
    #[must_use]
    pub fn ranges(&self, sample_rate: u32, total_frames: Option<u64>) -> Vec<CueRange> {
        let playable: Vec<(usize, u64)> = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.is_audio())
            .filter_map(|(i, t)| Some((i, t.start?.to_frames(sample_rate))))
            .collect();
        let mut out = Vec::with_capacity(playable.len());
        for (n, &(track, start)) in playable.iter().enumerate() {
            let start = match total_frames {
                Some(total) => start.min(total),
                None => start,
            };
            let end = match playable.get(n + 1) {
                Some(&(_, next)) => Some(match total_frames {
                    Some(total) => next.min(total),
                    None => next,
                }),
                None => total_frames,
            };
            out.push(CueRange {
                track,
                start_frame: start,
                // saturating_sub: a non-ascending sheet gives an empty track,
                // never a wrapped length.
                frames: end.map(|e| e.saturating_sub(start)),
            });
        }
        out
    }
}

/// A parsed cue sheet.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CueSheet {
    /// Disc `TITLE` — the album title.
    pub title: Option<String>,
    /// Disc `PERFORMER` — the album artist.
    pub performer: Option<String>,
    /// Disc `SONGWRITER`.
    pub songwriter: Option<String>,
    /// `CATALOG` (the disc's UPC/EAN).
    pub catalog: Option<String>,
    /// `REM GENRE`.
    pub genre: Option<String>,
    /// `REM DATE` (usually a year).
    pub date: Option<String>,
    /// `REM COMMENT`.
    pub comment: Option<String>,
    /// The files the sheet indexes, in sheet order. Usually one (a single-file
    /// album); a per-track rip with one cue has one `FILE` per track.
    pub files: Vec<CueFile>,
}

impl CueSheet {
    /// Total number of audio tracks across every file.
    #[must_use]
    pub fn audio_track_count(&self) -> usize {
        self.files
            .iter()
            .map(|f| f.tracks.iter().filter(|t| t.is_audio()).count())
            .sum()
    }
}

/// Why a cue sheet could not be parsed.
///
/// Deliberately few: real sheets carry commands no player knows, in orders no
/// spec blesses, and rejecting those would lose albums. Only two things are
/// fatal — a sheet with no tracks at all (it indexes nothing), and a malformed
/// timestamp (it would place a split somewhere the ripper did not mean, which
/// is worse than refusing).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum CueError {
    /// A `TRACK` appeared before any `FILE`, so it indexes nothing.
    TrackBeforeFile {
        /// 1-based line number.
        line: usize,
    },
    /// An `INDEX` timestamp is not a valid `MM:SS:FF`.
    BadTimestamp {
        /// 1-based line number.
        line: usize,
    },
    /// The sheet declares no tracks.
    NoTracks,
}

/// Split a cue line into its command and the rest, both trimmed. `None` for a
/// blank line.
fn split_command(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match line.split_once(char::is_whitespace) {
        Some((cmd, rest)) => Some((cmd, rest.trim_start())),
        None => Some((line, "")),
    }
}

/// The first argument of a cue line: a `"quoted string"` (quotes stripped, the
/// content kept verbatim — cue has no escape mechanism), else everything up to
/// the next whitespace. Returns the value and the remainder.
fn take_arg(rest: &str) -> (&str, &str) {
    let rest = rest.trim_start();
    if let Some(body) = rest.strip_prefix('"') {
        match body.split_once('"') {
            Some((value, tail)) => (value, tail.trim_start()),
            // An unterminated quote: take the rest of the line. A truncated
            // sheet still names its file/title as far as it goes.
            None => (body, ""),
        }
    } else {
        match rest.split_once(char::is_whitespace) {
            Some((value, tail)) => (value, tail.trim_start()),
            None => (rest, ""),
        }
    }
}

/// Parse `MM:SS:FF`.
fn parse_timestamp(s: &str) -> Option<CueTime> {
    let mut parts = s.trim().split(':');
    let m = parts.next()?.trim().parse::<u64>().ok()?;
    let s_ = parts.next()?.trim().parse::<u64>().ok()?;
    let f = parts.next()?.trim().parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    CueTime::from_mm_ss_ff(m, s_, f)
}

/// Parse a `REM` line's `KEY value` into the sheet.
fn apply_rem(sheet: &mut CueSheet, rest: &str) {
    let (key, tail) = take_arg(rest);
    let (value, _) = take_arg(tail);
    if value.is_empty() {
        return;
    }
    let slot = if key.eq_ignore_ascii_case("GENRE") {
        &mut sheet.genre
    } else if key.eq_ignore_ascii_case("DATE") {
        &mut sheet.date
    } else if key.eq_ignore_ascii_case("COMMENT") {
        &mut sheet.comment
    } else {
        return; // DISCID, REPLAYGAIN_*, ripper noise — not ours to model
    };
    if slot.is_none() {
        *slot = Some(String::from(value));
    }
}

/// Parse a cue sheet.
///
/// Lenient by design: unknown commands, stray indentation, CRLF or LF endings,
/// a UTF-8 BOM, and `REM` keys this player does not model are all ignored. A
/// `TITLE`/`PERFORMER`/`SONGWRITER` before the first `TRACK` is the disc's;
/// after one, it is that track's.
///
/// # Errors
/// [`CueError`] — see its variants; only an unindexable `TRACK`, a malformed
/// timestamp, or a sheet with no tracks at all are fatal.
pub fn parse(text: &str) -> Result<CueSheet, CueError> {
    let mut sheet = CueSheet::default();
    // Which file/track the next field belongs to.
    let mut cur_file: Option<usize> = None;
    let mut cur_track: Option<usize> = None;

    for (n, raw) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        let line_no = n + 1;
        let Some((cmd, rest)) = split_command(raw) else {
            continue;
        };

        if cmd.eq_ignore_ascii_case("REM") {
            apply_rem(&mut sheet, rest);
            continue;
        }

        if cmd.eq_ignore_ascii_case("FILE") {
            let (name, tail) = take_arg(rest);
            let (format, _) = take_arg(tail);
            sheet.files.push(CueFile {
                name: String::from(name),
                format: String::from(format),
                tracks: Vec::new(),
            });
            cur_file = Some(sheet.files.len() - 1);
            cur_track = None;
            continue;
        }

        if cmd.eq_ignore_ascii_case("TRACK") {
            let file = cur_file.ok_or(CueError::TrackBeforeFile { line: line_no })?;
            let (number, tail) = take_arg(rest);
            let (kind, _) = take_arg(tail);
            let tracks = &mut sheet.files[file].tracks;
            tracks.push(CueTrack {
                number: number.trim().parse().unwrap_or(0),
                kind: {
                    let mut k = String::from(kind);
                    k.make_ascii_uppercase();
                    k
                },
                ..CueTrack::default()
            });
            cur_track = Some(tracks.len() - 1);
            continue;
        }

        if cmd.eq_ignore_ascii_case("INDEX") {
            let (number, tail) = take_arg(rest);
            let (stamp, _) = take_arg(tail);
            let time = parse_timestamp(stamp).ok_or(CueError::BadTimestamp { line: line_no })?;
            let index: u16 = number.trim().parse().unwrap_or(1);
            // An INDEX before any TRACK indexes nothing; ignore it rather
            // than failing an otherwise good sheet.
            let (Some(file), Some(track)) = (cur_file, cur_track) else {
                continue;
            };
            let t = &mut sheet.files[file].tracks[track];
            match index {
                0 => t.pregap_start = Some(time),
                1 => t.start = Some(time),
                other => t.extra_indexes.push((other, time)),
            }
            continue;
        }

        // Text fields: the disc's before the first TRACK, the track's after.
        let value = || {
            let (v, _) = take_arg(rest);
            if v.is_empty() {
                None
            } else {
                Some(String::from(v))
            }
        };
        let is_title = cmd.eq_ignore_ascii_case("TITLE");
        let is_performer = cmd.eq_ignore_ascii_case("PERFORMER");
        let is_songwriter = cmd.eq_ignore_ascii_case("SONGWRITER");
        let is_isrc = cmd.eq_ignore_ascii_case("ISRC");
        let is_catalog = cmd.eq_ignore_ascii_case("CATALOG");

        match (cur_file, cur_track) {
            (Some(file), Some(track)) => {
                let t = &mut sheet.files[file].tracks[track];
                if is_title {
                    t.title = value();
                } else if is_performer {
                    t.performer = value();
                } else if is_songwriter {
                    t.songwriter = value();
                } else if is_isrc {
                    t.isrc = value();
                }
            }
            _ => {
                if is_title {
                    sheet.title = value();
                } else if is_performer {
                    sheet.performer = value();
                } else if is_songwriter {
                    sheet.songwriter = value();
                } else if is_catalog {
                    sheet.catalog = value();
                }
            }
        }
    }

    if sheet.files.iter().all(|f| f.tracks.is_empty()) {
        return Err(CueError::NoTracks);
    }
    Ok(sheet)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    /// A realistic single-file sheet, as EAC/whipper write them (CRLF, two
    /// leading spaces of indentation, `REM` lines, a pregap on track 2).
    const ALBUM: &str = "REM GENRE Jazz\r\n\
         REM DATE 1959\r\n\
         REM DISCID 7F0A2B0C\r\n\
         REM COMMENT \"ExactAudioCopy v1.5\"\r\n\
         CATALOG 0888072000001\r\n\
         PERFORMER \"Miles Davis\"\r\n\
         TITLE \"Kind of Blue\"\r\n\
         FILE \"Kind of Blue.flac\" WAVE\r\n\
         \x20 TRACK 01 AUDIO\r\n\
         \x20   TITLE \"So What\"\r\n\
         \x20   PERFORMER \"Miles Davis\"\r\n\
         \x20   ISRC USSM15900001\r\n\
         \x20   INDEX 01 00:00:00\r\n\
         \x20 TRACK 02 AUDIO\r\n\
         \x20   TITLE \"Freddie Freeloader\"\r\n\
         \x20   SONGWRITER \"Miles Davis\"\r\n\
         \x20   INDEX 00 09:20:50\r\n\
         \x20   INDEX 01 09:22:00\r\n\
         \x20 TRACK 03 AUDIO\r\n\
         \x20   TITLE \"Blue in Green\"\r\n\
         \x20   INDEX 01 18:58:25\r\n";

    #[test]
    fn parses_a_real_single_file_sheet() {
        let sheet = parse(ALBUM).expect("parse");
        assert_eq!(sheet.title.as_deref(), Some("Kind of Blue"));
        assert_eq!(sheet.performer.as_deref(), Some("Miles Davis"));
        assert_eq!(sheet.genre.as_deref(), Some("Jazz"));
        assert_eq!(sheet.date.as_deref(), Some("1959"));
        assert_eq!(sheet.comment.as_deref(), Some("ExactAudioCopy v1.5"));
        assert_eq!(sheet.catalog.as_deref(), Some("0888072000001"));
        assert_eq!(sheet.files.len(), 1);
        assert_eq!(sheet.audio_track_count(), 3);

        let file = &sheet.files[0];
        assert_eq!(file.name, "Kind of Blue.flac");
        assert_eq!(file.format, "WAVE", "the declared type is kept as written");

        let t1 = &file.tracks[0];
        assert_eq!(t1.number, 1);
        assert_eq!(t1.title.as_deref(), Some("So What"));
        assert_eq!(t1.performer.as_deref(), Some("Miles Davis"));
        assert_eq!(t1.isrc.as_deref(), Some("USSM15900001"));
        assert_eq!(t1.start, CueTime::from_mm_ss_ff(0, 0, 0));
        assert!(t1.is_audio());

        let t2 = &file.tracks[1];
        assert_eq!(t2.title.as_deref(), Some("Freddie Freeloader"));
        assert_eq!(t2.songwriter.as_deref(), Some("Miles Davis"));
        // The pregap is recorded but playback starts at INDEX 01.
        assert_eq!(t2.pregap_start, CueTime::from_mm_ss_ff(9, 20, 50));
        assert_eq!(t2.start, CueTime::from_mm_ss_ff(9, 22, 0));
        // The track-level PERFORMER is absent, so a consumer falls back to the
        // sheet's — the parser does not invent one.
        assert_eq!(t2.performer, None);

        assert_eq!(
            file.tracks[2].start,
            CueTime::from_mm_ss_ff(18, 58, 25),
            "frames are not seconds: 25 frames is a third of a second"
        );
    }

    #[test]
    fn timestamps_convert_to_audio_frames_exactly_at_cd_rates() {
        // 44.1 kHz: 588 samples per CD frame, exactly.
        let t = CueTime::from_mm_ss_ff(9, 22, 0).unwrap();
        assert_eq!(t.frames, (9 * 60 + 22) * 75);
        assert_eq!(t.to_frames(44_100), t.frames * 588);
        // Every CD-ish rate divides evenly; 44 056 (a one-off) does not.
        for rate in [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000] {
            assert!(CueTime::divides_evenly(rate), "{rate}");
            assert_eq!(
                t.to_frames(rate),
                t.frames * (u64::from(rate) / 75),
                "{rate} must be exact"
            );
        }
        assert!(!CueTime::divides_evenly(44_056));
        // A single CD frame at 44.1 kHz is 588 samples — 13.3 ms.
        assert_eq!(CueTime { frames: 1 }.to_frames(44_100), 588);
        assert_eq!(CueTime { frames: 1 }.as_millis(), 13);
        // Round trip through the display form.
        assert_eq!(t.mm_ss_ff(), (9, 22, 0));
        assert_eq!(CueTime { frames: 1_234_567 }.mm_ss_ff(), (274, 20, 67));
        // Minutes past 99 are a long single file, not a malformed sheet.
        let long = CueTime::from_mm_ss_ff(123, 45, 67).unwrap();
        assert_eq!(long.mm_ss_ff(), (123, 45, 67));
    }

    /// A corrupt or hand-edited sheet can carry a minute field with fifteen
    /// digits. That is not a timestamp, and the arithmetic that turns it into
    /// a sample offset must not overflow: a panic here takes out the whole
    /// library scan over one bad file.
    #[test]
    fn an_absurd_timestamp_is_refused_rather_than_overflowing() {
        // Parses as a `u64` minute count, and its CD-frame total even fits —
        // but it is past the sane ceiling, so it is refused.
        assert_eq!(CueTime::from_mm_ss_ff(1_000_000_000_000_000, 0, 0), None);
        // And one large enough to overflow the frame arithmetic itself.
        assert_eq!(CueTime::from_mm_ss_ff(u64::MAX, 0, 0), None);
        assert_eq!(CueTime::from_mm_ss_ff(u64::MAX / 60, 59, 74), None);
        // Through the parser it reads as a malformed sheet, with its line.
        assert!(matches!(
            parse("FILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 1000000000000000:00:00\n"),
            Err(CueError::BadTimestamp { line: 3 })
        ));

        // The ceiling is generous: a day-long single file is nowhere near it.
        let day = CueTime::from_mm_ss_ff(24 * 60, 0, 0).expect("a day is a sane timestamp");
        assert_eq!(day.to_frames(44_100), 24 * 3600 * 44_100);
        assert_eq!(day.as_millis(), 24 * 3600 * 1000);
        assert!(CueTime::from_mm_ss_ff(MAX_CUE_FRAMES / 75 / 60, 0, 0).is_some());

        // `frames` is public, so a value can also arrive from a caller or a
        // deserialized profile: every conversion stays total, saturating.
        let absurd = CueTime { frames: u64::MAX };
        assert_eq!(absurd.to_frames(768_000), u64::MAX);
        assert_eq!(absurd.as_millis(), u64::MAX);
        assert_eq!(absurd.to_frames(0), 0);
        let (m, _, _) = absurd.mm_ss_ff();
        assert!(m > 0, "the display form must not panic either");
    }

    /// And a file whose tracks carry such a value yields ranges rather than a
    /// panic — the importer walks these for every sheet it finds.
    #[test]
    fn ranges_of_an_absurd_timestamp_do_not_panic() {
        let file = CueFile {
            name: String::from("a.flac"),
            format: String::from("WAVE"),
            tracks: vec![
                CueTrack {
                    number: 1,
                    kind: String::from("AUDIO"),
                    start: Some(CueTime { frames: 0 }),
                    ..CueTrack::default()
                },
                CueTrack {
                    number: 2,
                    kind: String::from("AUDIO"),
                    start: Some(CueTime { frames: u64::MAX }),
                    ..CueTrack::default()
                },
            ],
        };
        // With and without a known file length.
        for total in [Some(88_200u64), None] {
            let ranges = file.ranges(192_000, total);
            assert_eq!(ranges.len(), 2);
            assert!(
                ranges
                    .iter()
                    .all(|r| r.frames.unwrap_or(0) <= 88_200 || total.is_none()),
                "{ranges:?}"
            );
        }
    }

    #[test]
    fn out_of_range_timestamp_components_are_rejected() {
        assert_eq!(CueTime::from_mm_ss_ff(0, 60, 0), None, "60 seconds");
        assert_eq!(CueTime::from_mm_ss_ff(0, 0, 75), None, "75 CD frames");
        assert!(matches!(
            parse("FILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:60:00\n"),
            Err(CueError::BadTimestamp { line: 3 })
        ));
        assert!(matches!(
            parse("FILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 nonsense\n"),
            Err(CueError::BadTimestamp { line: 3 })
        ));
        // Only three components; `00:00` is not a cue timestamp.
        assert!(matches!(
            parse("FILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00\n"),
            Err(CueError::BadTimestamp { line: 3 })
        ));
    }

    #[test]
    fn ranges_split_the_file_at_the_index_points() {
        let sheet = parse(ALBUM).expect("parse");
        let file = &sheet.files[0];
        // A 25-minute file at 44.1 kHz.
        let total = 25 * 60 * 44_100;
        let ranges = file.ranges(44_100, Some(total));
        assert_eq!(ranges.len(), 3);

        let f =
            |m: u64, s: u64, fr: u64| CueTime::from_mm_ss_ff(m, s, fr).unwrap().to_frames(44_100);
        assert_eq!(ranges[0].start_frame, 0);
        assert_eq!(ranges[0].frames, Some(f(9, 22, 0)));
        assert_eq!(ranges[1].start_frame, f(9, 22, 0));
        assert_eq!(ranges[1].frames, Some(f(18, 58, 25) - f(9, 22, 0)));
        assert_eq!(ranges[2].start_frame, f(18, 58, 25));
        assert_eq!(
            ranges[2].frames,
            Some(total - f(18, 58, 25)),
            "the last track runs to the end of the file"
        );
        // The ranges tile the file exactly — no gap, no overlap, nothing lost.
        let covered: u64 = ranges.iter().map(|r| r.frames.unwrap()).sum();
        assert_eq!(covered, total, "the ranges must tile the whole file");
        for pair in ranges.windows(2) {
            assert_eq!(
                pair[0].start_frame + pair[0].frames.unwrap(),
                pair[1].start_frame,
                "each track must abut the next (this is what makes it gapless)"
            );
        }

        // Without a known length the last track runs to EOF.
        let open = file.ranges(44_100, None);
        assert_eq!(open[2].frames, None);
        assert_eq!(open[0].frames, Some(f(9, 22, 0)));
    }

    #[test]
    fn data_tracks_and_unindexed_tracks_are_not_playable() {
        // A mixed-mode disc: a data track first, then audio.
        let sheet = parse(
            "FILE \"disc.bin\" BINARY\n\
             TRACK 01 MODE1/2352\n\
             INDEX 01 00:00:00\n\
             TRACK 02 AUDIO\n\
             TITLE \"Real Audio\"\n\
             INDEX 01 00:02:00\n\
             TRACK 03 AUDIO\n\
             TITLE \"No index\"\n",
        )
        .expect("parse");
        let file = &sheet.files[0];
        assert_eq!(file.tracks.len(), 3);
        assert!(!file.tracks[0].is_audio(), "MODE1/2352 is data");
        assert_eq!(sheet.audio_track_count(), 2);

        let ranges = file.ranges(44_100, Some(44_100 * 60));
        assert_eq!(ranges.len(), 1, "only the indexed audio track is playable");
        assert_eq!(ranges[0].track, 1);
        assert_eq!(ranges[0].start_frame, 2 * 75 * 588);
    }

    #[test]
    fn a_per_track_rip_has_one_file_per_track() {
        let sheet = parse(
            "TITLE \"Split Rip\"\n\
             FILE \"01 - One.flac\" WAVE\n\
             TRACK 01 AUDIO\n\
             TITLE \"One\"\n\
             INDEX 01 00:00:00\n\
             FILE \"02 - Two.flac\" WAVE\n\
             TRACK 02 AUDIO\n\
             TITLE \"Two\"\n\
             INDEX 01 00:00:00\n",
        )
        .expect("parse");
        assert_eq!(sheet.files.len(), 2);
        assert_eq!(sheet.audio_track_count(), 2);
        for (i, file) in sheet.files.iter().enumerate() {
            assert_eq!(file.tracks.len(), 1, "file {i}");
            let ranges = file.ranges(44_100, Some(1000));
            assert_eq!(ranges[0].start_frame, 0);
            assert_eq!(
                ranges[0].frames,
                Some(1000),
                "a single-track file is the whole file"
            );
        }
    }

    #[test]
    fn a_malformed_sheet_cannot_make_the_player_read_backwards() {
        // Descending index points (a mis-decoded or hand-edited sheet).
        let sheet = parse(
            "FILE \"a.flac\" WAVE\n\
             TRACK 01 AUDIO\n\
             INDEX 01 05:00:00\n\
             TRACK 02 AUDIO\n\
             INDEX 01 01:00:00\n",
        )
        .expect("parse");
        let ranges = sheet.files[0].ranges(44_100, Some(10 * 60 * 44_100));
        assert_eq!(ranges[0].frames, Some(0), "a backwards range is empty");
        assert!(ranges[0].start_frame > ranges[1].start_frame);
        // And a start past the end of the file is clamped, not left to seek
        // off the end.
        let short = sheet.files[0].ranges(44_100, Some(60 * 44_100));
        assert_eq!(short[0].start_frame, 60 * 44_100);
        assert_eq!(short[0].frames, Some(0));
    }

    #[test]
    fn unparseable_and_empty_sheets_are_reported() {
        assert_eq!(parse(""), Err(CueError::NoTracks));
        assert_eq!(parse("REM nothing here\n"), Err(CueError::NoTracks));
        assert_eq!(
            parse("FILE \"a.flac\" WAVE\n"),
            Err(CueError::NoTracks),
            "a FILE with no tracks indexes nothing"
        );
        assert_eq!(
            parse("TRACK 01 AUDIO\nINDEX 01 00:00:00\n"),
            Err(CueError::TrackBeforeFile { line: 1 })
        );
    }

    #[test]
    fn unknown_commands_and_odd_formatting_are_tolerated() {
        // Unknown commands, a BOM, LF endings, no indentation, an unquoted
        // title, an unterminated quote, and a FLAGS line.
        let sheet = parse(
            "\u{feff}REM RIPPER \"whipper 0.10\"\n\
             POSTGAP 00:02:00\n\
             TITLE Unquoted\n\
             FILE \"a.flac\" FLAC\n\
             TRACK 01 AUDIO\n\
             FLAGS DCP\n\
             TITLE \"Unterminated\n\
             INDEX 01 00:00:00\n\
             INDEX 02 00:30:00\n",
        )
        .expect("parse");
        assert_eq!(sheet.title.as_deref(), Some("Unquoted"));
        let t = &sheet.files[0].tracks[0];
        assert_eq!(t.title.as_deref(), Some("Unterminated"));
        assert_eq!(t.extra_indexes, [(2, CueTime { frames: 30 * 75 })]);
        assert_eq!(sheet.audio_track_count(), 1);
    }

    #[test]
    fn a_track_number_the_sheet_skips_is_kept_as_written() {
        // A continued disc starting at track 7 — the number is metadata, the
        // position in the file is what plays.
        let sheet = parse(
            "FILE \"side-b.flac\" WAVE\n\
             TRACK 07 AUDIO\n\
             INDEX 01 00:00:00\n\
             TRACK 09 AUDIO\n\
             INDEX 01 03:00:00\n",
        )
        .expect("parse");
        let file = &sheet.files[0];
        assert_eq!(file.tracks[0].number, 7);
        assert_eq!(file.tracks[1].number, 9);
        let ranges = file.ranges(44_100, Some(10 * 60 * 44_100));
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].track, 0, "ranges index tracks by position");
        assert_eq!(ranges[1].track, 1);
    }

    #[test]
    fn a_long_single_file_sheet_past_99_minutes_still_splits() {
        // Two hours of audio in one file (a live set / DJ mix).
        let mut text = String::from("FILE \"set.flac\" WAVE\n");
        for i in 0..6 {
            text.push_str(&format!(
                "TRACK {:02} AUDIO\nINDEX 01 {:02}:00:00\n",
                i + 1,
                i * 20
            ));
        }
        let sheet = parse(&text).expect("parse");
        let total = 120 * 60 * 44_100;
        let ranges = sheet.files[0].ranges(44_100, Some(total));
        assert_eq!(ranges.len(), 6);
        assert_eq!(ranges[5].start_frame, 100 * 60 * 44_100);
        assert_eq!(ranges[5].frames, Some(20 * 60 * 44_100));
        assert_eq!(
            ranges.iter().map(|r| r.frames.unwrap()).sum::<u64>(),
            total,
            "the ranges must still tile the file past 99 minutes"
        );
        // The display form keeps the real minute count.
        assert_eq!(
            sheet.files[0].tracks[5].start.unwrap().mm_ss_ff().0,
            100,
            "100 minutes in, from {} CD frames",
            sheet.files[0].tracks[5].start.unwrap().frames
        );
    }
}
