//! `.cue` sheet `FILE`-line rewriting.
//!
//! When the organizer renames or moves the audio files a cue sheet references,
//! the cue's planned copy gets its `FILE` lines pointed at the new names,
//! relative to the cue's **new** folder (`Disc 1/01 - Intro.flac` when the cue
//! stays at the album root and the tracks move into disc folders). Every other
//! byte — a UTF-8 BOM, CRLF/LF line endings, `REM` lines and every other
//! command, a `FILE` line's indentation and file type — is kept as it was. A
//! cue that cannot be rewritten without risking mojibake is left
//! byte-identical and reported ([`CueNotRewritten`]). This is the **only**
//! side file the organizer ever modifies: a `.log` (whose checksum must stay
//! valid) and everything else is copied/moved byte for byte.
#![allow(dead_code)] // wired into plan.rs by the next task, which removes this

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::paths;

/// Why a `.cue` side file was planned **unchanged** although it may reference
/// audio this plan renames (reported in `OrganizePlan::cues_not_rewritten`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CueNotRewritten {
    /// The cue is not UTF-8 (a legacy code page such as Windows-1252, or
    /// UTF-16) and a `FILE` name — as written, or the new one — is not plain
    /// ASCII: it can be neither matched nor written without risking mojibake.
    UnsupportedEncoding,
    /// A new name cannot be written into a `FILE` line (it is not UTF-8, or it
    /// holds a `"` or a line break).
    Unrepresentable,
    /// The cue could not be read while planning, or is larger than 1 MiB (no
    /// real cue sheet is).
    Unreadable,
}

/// A larger `.cue` is not a cue sheet; it is copied as-is.
const MAX_CUE_BYTES: u64 = 1 << 20;

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// Whether `path` is a cue sheet (`.cue`, any case).
pub(crate) fn is_cue(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cue"))
}

/// What [`rewrite`] made of a cue sheet.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// No `FILE` line needs a new name: copy the cue as-is.
    Unchanged,
    /// The cue with its `FILE` lines rewritten.
    Rewritten(Vec<u8>),
    /// Some `FILE` line could not be handled safely: copy the cue as-is and
    /// report why.
    NotRewritten(CueNotRewritten),
}

/// Rewrite the `FILE` lines of the cue sheet `bytes`. `target` gets each
/// `FILE` name as written and returns its new path relative to the cue's
/// destination folder, or `None` to keep the line. See the module docs for
/// what is kept and when a cue is [`NotRewritten`](Outcome::NotRewritten).
pub(crate) fn rewrite(bytes: &[u8], mut target: impl FnMut(&str) -> Option<PathBuf>) -> Outcome {
    if bytes.starts_with(b"\xFF\xFE") || bytes.starts_with(b"\xFE\xFF") {
        return Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding);
    }
    let (bom, body): (&[u8], &[u8]) = match bytes.strip_prefix(UTF8_BOM) {
        Some(rest) => (UTF8_BOM, rest),
        None => (b"", bytes),
    };
    let utf8 = std::str::from_utf8(body).is_ok();
    let mut out = Vec::with_capacity(bytes.len() + 64);
    out.extend_from_slice(bom);
    let mut changed = false;
    for (content, eol) in lines(body) {
        let Some(line) = parse_file_line(content) else {
            out.extend_from_slice(content);
            out.extend_from_slice(eol);
            continue;
        };
        let old = match std::str::from_utf8(line.name) {
            Ok(s) if utf8 || s.is_ascii() => s,
            _ => return Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding),
        };
        let new = match target(old) {
            None => None,
            Some(rel) => match cue_name(&rel) {
                None => return Outcome::NotRewritten(CueNotRewritten::Unrepresentable),
                Some(n) if !utf8 && !n.is_ascii() => {
                    return Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding);
                }
                Some(n) => Some(n),
            },
        };
        match new {
            Some(n) if n != old => {
                out.extend_from_slice(line.head);
                out.push(b'"');
                out.extend_from_slice(n.as_bytes());
                out.push(b'"');
                out.extend_from_slice(line.tail);
                changed = true;
            }
            _ => out.extend_from_slice(content),
        }
        out.extend_from_slice(eol);
    }
    if changed {
        Outcome::Rewritten(out)
    } else {
        Outcome::Unchanged
    }
}

/// `body` as `(content, terminator)` pairs: split after `\r\n`, `\n` or a
/// lone `\r`; the last line's terminator may be empty. Concatenating every
/// pair gives `body` back exactly.
fn lines(body: &[u8]) -> Vec<(&[u8], &[u8])> {
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < body.len() {
        let end = match body[i] {
            b'\n' => i + 1,
            b'\r' if body.get(i + 1) == Some(&b'\n') => i + 2,
            b'\r' => i + 1,
            _ => {
                i += 1;
                continue;
            }
        };
        out.push((&body[start..i], &body[i..end]));
        start = end;
        i = end;
    }
    if start < body.len() {
        out.push((&body[start..], &body[body.len()..]));
    }
    out
}

/// One `FILE` line, split around its file name.
struct FileLine<'a> {
    /// Everything before the name (indentation, `FILE`, whitespace).
    head: &'a [u8],
    /// The name, without quotes.
    name: &'a [u8],
    /// Everything after the name (closing quote excluded), e.g. ` WAVE`.
    tail: &'a [u8],
}

/// `line` (no terminator) as a `FILE` command, or `None`.
fn parse_file_line(line: &[u8]) -> Option<FileLine<'_>> {
    let is_ws = |b: &u8| *b == b' ' || *b == b'\t';
    let start = line.iter().position(|b| !is_ws(b))?;
    let rest = &line[start..];
    if rest.len() < 5 || !rest[..4].eq_ignore_ascii_case(b"FILE") || !is_ws(&rest[4]) {
        return None;
    }
    let name_at = start + 4 + rest[4..].iter().position(|b| !is_ws(b))?;
    if line[name_at] == b'"' {
        let close = name_at + 1 + line[name_at + 1..].iter().position(|&b| b == b'"')?;
        Some(FileLine {
            head: &line[..name_at],
            name: &line[name_at + 1..close],
            tail: &line[close + 1..],
        })
    } else {
        let end = line[name_at..]
            .iter()
            .position(is_ws)
            .map_or(line.len(), |n| name_at + n);
        Some(FileLine {
            head: &line[..name_at],
            name: &line[name_at..end],
            tail: &line[end..],
        })
    }
}

/// `rel` as a `FILE` name: components joined with `/`. `None` when it cannot
/// be written into a quoted `FILE` argument.
fn cue_name(rel: &Path) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?),
            Component::ParentDir => parts.push(".."),
            _ => return None,
        }
    }
    let name = parts.join("/");
    (!name.is_empty() && !name.contains(['"', '\r', '\n'])).then_some(name)
}

/// `to` relative to the folder `from_dir` (both under one destination root),
/// walking up with `..` where needed.
pub(crate) fn relative(from_dir: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component<'_>> = from_dir.components().collect();
    let to: Vec<Component<'_>> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to[common..] {
        out.push(c.as_os_str());
    }
    out
}

/// `dir` joined with a `FILE` name as written (`/` or `\` separated; empty and
/// `.` parts dropped).
fn join_cue_name(dir: &Path, name: &str) -> PathBuf {
    let mut p = dir.to_path_buf();
    for part in name
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
    {
        p.push(part);
    }
    p
}

/// Plan the cue side file `src` → `dst`: read it and point each `FILE` line
/// that references (relative to the cue's own folder, compared as
/// [`paths::key`]s — case-insensitive on Windows/macOS) an audio source in
/// `audio_to` (`key(src)` → its planned destination) at that destination,
/// relative to `dst`'s folder. `Ok(None)`: copy as-is; `Ok(Some(bytes))`:
/// write these instead; `Err(why)`: copy as-is and report.
pub(crate) fn plan_rewrite(
    src: &Path,
    dst: &Path,
    audio_to: &HashMap<PathBuf, PathBuf>,
) -> Result<Option<Vec<u8>>, CueNotRewritten> {
    match std::fs::metadata(src) {
        Ok(m) if m.len() <= MAX_CUE_BYTES => {}
        _ => return Err(CueNotRewritten::Unreadable),
    }
    let bytes = std::fs::read(src).map_err(|_| CueNotRewritten::Unreadable)?;
    let src_dir = src.parent().unwrap_or(Path::new(""));
    let dst_dir = dst.parent().unwrap_or(Path::new(""));
    let outcome = rewrite(&bytes, |name| {
        audio_to
            .get(&paths::key(&join_cue_name(src_dir, name)))
            .map(|to| relative(dst_dir, to))
    });
    match outcome {
        Outcome::Unchanged => Ok(None),
        Outcome::Rewritten(b) => Ok(Some(b)),
        Outcome::NotRewritten(why) => Err(why),
    }
}

#[cfg(test)]
mod tests {
    use super::{CueNotRewritten, Outcome, is_cue, relative, rewrite};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    /// `rewrite` against a fixed `old name → new relative path` table.
    fn run(bytes: &[u8], table: &[(&str, &str)]) -> Outcome {
        let map: HashMap<&str, PathBuf> = table
            .iter()
            .map(|(old, new)| (*old, PathBuf::from(new)))
            .collect();
        rewrite(bytes, |name| map.get(name).cloned())
    }

    fn rewritten(o: Outcome) -> Vec<u8> {
        match o {
            Outcome::Rewritten(b) => b,
            other => panic!("expected Rewritten, got {other:?}"),
        }
    }

    #[test]
    fn crlf_lines_and_every_other_byte_are_kept() {
        let src = b"REM GENRE Game\r\nREM COMMENT \"ExactAudioCopy v1.6\"\r\n\
PERFORMER \"HOYO-MiX\"\r\nFILE \"01 Intro.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n\
    INDEX 01 00:00:00\r\n";
        let want = b"REM GENRE Game\r\nREM COMMENT \"ExactAudioCopy v1.6\"\r\n\
PERFORMER \"HOYO-MiX\"\r\nFILE \"1-01 - Intro.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n\
    INDEX 01 00:00:00\r\n";
        assert_eq!(
            rewritten(run(src, &[("01 Intro.flac", "1-01 - Intro.flac")])),
            want.to_vec()
        );
    }

    #[test]
    fn lf_lone_cr_and_an_unterminated_last_line_are_kept() {
        assert_eq!(
            rewritten(run(
                b"TITLE \"x\"\nFILE \"a.flac\" WAVE\rTRACK 01 AUDIO\nFILE \"b.flac\" WAVE",
                &[("a.flac", "A.flac"), ("b.flac", "B.flac")]
            )),
            b"TITLE \"x\"\nFILE \"A.flac\" WAVE\rTRACK 01 AUDIO\nFILE \"B.flac\" WAVE".to_vec()
        );
    }

    #[test]
    fn a_utf8_bom_is_kept() {
        assert_eq!(
            rewritten(run(
                b"\xEF\xBB\xBFFILE \"a.flac\" WAVE\r\n",
                &[("a.flac", "Disc 1/01 - A.flac")]
            )),
            b"\xEF\xBB\xBFFILE \"Disc 1/01 - A.flac\" WAVE\r\n".to_vec()
        );
    }

    #[test]
    fn several_file_lines_and_an_unrelated_one() {
        let src = b"FILE \"01 a.flac\" WAVE\n  TRACK 01 AUDIO\n\
FILE \"Album Image.wav\" WAVE\n  TRACK 02 AUDIO\n\
REM FILE \"01 a.flac\"\nFILE \"02 b c.flac\" WAVE\n  TRACK 03 AUDIO\n";
        let want = b"FILE \"1-01 - a.flac\" WAVE\n  TRACK 01 AUDIO\n\
FILE \"Album Image.wav\" WAVE\n  TRACK 02 AUDIO\n\
REM FILE \"01 a.flac\"\nFILE \"1-02 - b c.flac\" WAVE\n  TRACK 03 AUDIO\n";
        assert_eq!(
            rewritten(run(
                src,
                &[
                    ("01 a.flac", "1-01 - a.flac"),
                    ("02 b c.flac", "1-02 - b c.flac")
                ]
            )),
            want.to_vec(),
            "the image no track maps to and the REM line stay as they are"
        );
    }

    #[test]
    fn unquoted_names_and_an_indented_lowercase_command() {
        assert_eq!(
            rewritten(run(
                b"\tfile 01.flac WAVE\n",
                &[("01.flac", "Disc 1/01 - A.flac")]
            )),
            b"\tfile \"Disc 1/01 - A.flac\" WAVE\n".to_vec()
        );
    }

    #[test]
    fn unicode_names_in_a_utf8_cue() {
        assert_eq!(
            rewritten(run(
                "TITLE \"璃月\"\r\nFILE \"01 璃月.flac\" WAVE\r\n".as_bytes(),
                &[("01 璃月.flac", "1-01 - 璃月の夜.flac")]
            )),
            "TITLE \"璃月\"\r\nFILE \"1-01 - 璃月の夜.flac\" WAVE\r\n"
                .as_bytes()
                .to_vec()
        );
    }

    #[test]
    fn a_legacy_cue_with_a_non_ascii_name_is_not_rewritten() {
        // Windows-1252 `Café.flac` — not UTF-8, so the name cannot be matched.
        assert_eq!(
            run(
                b"FILE \"Caf\xE9.flac\" WAVE\r\n",
                &[("Café.flac", "01 - Café.flac")]
            ),
            Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding)
        );
    }

    #[test]
    fn a_legacy_cue_with_ascii_names_is_rewritten_byte_for_byte() {
        assert_eq!(
            rewritten(run(
                b"REM COMMENT \"Caf\xE9\"\r\nFILE \"01.flac\" WAVE\r\n",
                &[("01.flac", "01 - Intro.flac")]
            )),
            b"REM COMMENT \"Caf\xE9\"\r\nFILE \"01 - Intro.flac\" WAVE\r\n".to_vec()
        );
    }

    #[test]
    fn a_legacy_cue_is_never_given_a_non_ascii_name() {
        assert_eq!(
            run(
                b"REM COMMENT \"Caf\xE9\"\r\nFILE \"01.flac\" WAVE\r\n",
                &[("01.flac", "01 - Café.flac")]
            ),
            Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding)
        );
    }

    #[test]
    fn utf16_is_not_rewritten() {
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("FILE \"a.flac\" WAVE\r\n".bytes().flat_map(|b| [b, 0]))
            .collect();
        assert_eq!(
            run(&utf16, &[("a.flac", "A.flac")]),
            Outcome::NotRewritten(CueNotRewritten::UnsupportedEncoding)
        );
    }

    #[test]
    fn nothing_to_rewrite_is_unchanged() {
        assert_eq!(run(b"cue 1", &[("a.flac", "A.flac")]), Outcome::Unchanged);
        assert_eq!(
            run(b"FILE \"x.flac\" WAVE\n", &[("a.flac", "A.flac")]),
            Outcome::Unchanged
        );
        assert_eq!(
            run(b"FILE \"a.flac\" WAVE\n", &[("a.flac", "a.flac")]),
            Outcome::Unchanged,
            "a name that does not change"
        );
        assert_eq!(
            run(
                b"FILEX \"a.flac\"\nFILE \"a.flac\n",
                &[("a.flac", "A.flac")]
            ),
            Outcome::Unchanged,
            "not a FILE command / no closing quote"
        );
    }

    #[test]
    fn a_name_with_a_quote_is_unrepresentable() {
        assert_eq!(
            run(b"FILE \"a.flac\" WAVE\n", &[("a.flac", "a\"b.flac")]),
            Outcome::NotRewritten(CueNotRewritten::Unrepresentable)
        );
    }

    #[test]
    fn relative_paths_walk_down_and_up() {
        assert_eq!(
            relative(Path::new("d/Box"), Path::new("d/Box/Disc 1/01.flac")),
            Path::new("Disc 1").join("01.flac")
        );
        assert_eq!(
            relative(Path::new("d/Box/Disc 1"), Path::new("d/Box/Disc 2/01.flac")),
            Path::new("..").join("Disc 2").join("01.flac")
        );
        assert_eq!(
            relative(Path::new("d/Box"), Path::new("d/Box/01.flac")),
            PathBuf::from("01.flac")
        );
    }

    #[test]
    fn cue_extension_any_case() {
        assert!(is_cue(Path::new("a/Box.cue")) && is_cue(Path::new("a/Box.CUE")));
        assert!(!is_cue(Path::new("a/Box.log")) && !is_cue(Path::new("a/cue")));
    }
}
