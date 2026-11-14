//! Path-template renderer: turn a `{AlbumArtist}/{Album}/{TrackNo} - {Title}`
//! style template plus a file's [`Tags`] into a relative, filesystem-safe
//! destination path.
//!
//! # Tokens
//! The supported tokens are, case-sensitive and exact: `{AlbumArtist}`,
//! `{Artist}`, `{Album}`, `{Year}`, `{Disc}`, `{TrackNo}`, `{Title}`, `{Genre}`.
//! An **unknown** `{...}` token renders as the empty string.
//!
//! # Separators vs. tag values
//! Only the literal `/` characters *in the template* delimit directory levels.
//! The template is split on `/` **before** substitution, so a tag value that
//! itself contains a slash (e.g. the artist "AC/DC") can never introduce a stray
//! directory level — its slash is sanitized to `_` inside one segment.

use std::path::{Path, PathBuf};

use soul_audio_tags::Tags;

use crate::sanitize::sanitize_segment;

/// A destination path template, e.g. `{AlbumArtist}/{Album}/{TrackNo} - {Title}`.
pub struct Template {
    raw: String,
}

impl Template {
    /// Build a template from its raw string form.
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    /// The template's raw string form.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Render a **relative**, sanitized destination path from `tags`. The path
    /// uses native separators, each segment is run through
    /// [`sanitize_segment`](crate::sanitize), and the original extension of
    /// `src` is preserved (keeping its case; none is appended if `src` has no
    /// extension). The caller joins the result onto the destination root.
    pub fn render(&self, tags: &Tags, src: &Path) -> PathBuf {
        let mut path = PathBuf::new();
        for template_segment in self.raw.split('/') {
            let substituted = substitute(template_segment, tags, src);
            path.push(sanitize_segment(&substituted));
        }
        // Preserve the source extension (original case) by appending it to the
        // final component — `set_extension` would mis-handle a name with dots.
        if let Some(ext) = src.extension() {
            let mut os = path.into_os_string();
            os.push(".");
            os.push(ext);
            path = PathBuf::from(os);
        }
        path
    }
}

/// Substitute every `{token}` in one template segment. Known tokens expand to
/// their (tag-derived) value; an unknown token expands to the empty string;
/// an unterminated `{` is treated as literal text.
fn substitute(segment: &str, tags: &Tags, src: &Path) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut rest = segment;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                if let Some(value) = token_value(&after[..close], tags, src) {
                    out.push_str(&value);
                }
                rest = &after[close + 1..];
            }
            None => {
                // No closing brace: keep the rest verbatim.
                out.push('{');
                out.push_str(after);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The value a known token expands to, or `None` for an unknown token.
fn token_value(name: &str, tags: &Tags, src: &Path) -> Option<String> {
    let value = match name {
        "AlbumArtist" => non_empty(&tags.album_artist)
            .or_else(|| non_empty(&tags.artist))
            .unwrap_or_else(|| "Unknown Artist".to_owned()),
        "Artist" => non_empty(&tags.artist).unwrap_or_else(|| "Unknown Artist".to_owned()),
        "Album" => non_empty(&tags.album).unwrap_or_else(|| "Unknown Album".to_owned()),
        "Title" => non_empty(&tags.title).unwrap_or_else(|| src_stem(src)),
        "Genre" => non_empty(&tags.genre).unwrap_or_else(|| "Unknown Genre".to_owned()),
        // Zero-padded width 2 (track 3 -> "03"; missing -> "00").
        "TrackNo" => format!("{:02}", tags.track.unwrap_or(0)),
        // Plain decimal (missing -> "1").
        "Disc" => tags.disc.unwrap_or(1).to_string(),
        // First 4 chars of `date` parsed as a year, else "".
        "Year" => year(tags).map(|y| y.to_string()).unwrap_or_default(),
        _ => return None,
    };
    Some(value)
}

/// `Some(clone)` if the tag is present and non-empty, else `None`.
fn non_empty(tag: &Option<String>) -> Option<String> {
    tag.as_deref().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// The leading 4-digit year of the `date` tag (mirrors `soul-audio-import`).
fn year(tags: &Tags) -> Option<u16> {
    tags.date
        .as_deref()
        .and_then(|d| d.get(0..4))
        .and_then(|y| y.parse::<u16>().ok())
}

/// The source file's stem (name without extension), or `""`.
fn src_stem(src: &Path) -> String {
    src.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_owned()
}

/// Built-in named templates: `(name, template)`.
pub fn presets() -> &'static [(&'static str, &'static str)] {
    &[
        ("artist-album", "{AlbumArtist}/{Album}/{TrackNo} - {Title}"),
        (
            "artist-year-album",
            "{AlbumArtist}/{Year} - {Album}/{TrackNo} - {Title}",
        ),
        (
            "genre-artist-album",
            "{Genre}/{AlbumArtist}/{Album}/{TrackNo} - {Title}",
        ),
        ("flat", "{Artist} - {Title}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::{Template, presets};
    use soul_audio_tags::Tags;
    use std::path::{Path, PathBuf};

    fn expect(parts: &[&str]) -> PathBuf {
        let mut p = PathBuf::new();
        for s in parts {
            p.push(s);
        }
        p
    }

    #[test]
    fn renders_fully_tagged_and_sanitizes_album_artist() {
        // Artist "AC/DC" proves a tag value's own slash can't spawn a stray
        // directory level — it sanitizes to "AC_DC" inside one segment.
        let tags = Tags {
            title: Some("Hells Bells".into()),
            artist: Some("AC/DC".into()),
            album: Some("Back in Black".into()),
            track: Some(1),
            ..Default::default()
        };
        let got = Template::new("{AlbumArtist}/{Album}/{TrackNo} - {Title}")
            .render(&tags, Path::new("x.flac"));
        assert_eq!(
            got,
            expect(&["AC_DC", "Back in Black", "01 - Hells Bells.flac"])
        );
    }

    #[test]
    fn renders_all_defaults_with_src_stem_as_title() {
        let got = Template::new("{AlbumArtist}/{Album}/{TrackNo} - {Title}")
            .render(&Tags::default(), Path::new("song.mp3"));
        assert_eq!(
            got,
            expect(&["Unknown Artist", "Unknown Album", "00 - song.mp3"])
        );
    }

    #[test]
    fn sanitizes_colon_in_title() {
        let tags = Tags {
            title: Some("Hells: Bells".into()),
            artist: Some("AC/DC".into()),
            album: Some("Back in Black".into()),
            track: Some(1),
            ..Default::default()
        };
        let got = Template::new("{AlbumArtist}/{Album}/{TrackNo} - {Title}")
            .render(&tags, Path::new("x.flac"));
        assert_eq!(
            got,
            expect(&["AC_DC", "Back in Black", "01 - Hells_ Bells.flac"])
        );
    }

    #[test]
    fn year_token_takes_first_four_chars_of_date() {
        let tags = Tags {
            album: Some("Alb".into()),
            date: Some("2012-05-29".into()),
            ..Default::default()
        };
        let got = Template::new("{Year} - {Album}").render(&tags, Path::new("x.flac"));
        assert_eq!(got, expect(&["2012 - Alb.flac"]));
    }

    #[test]
    fn unknown_token_renders_empty() {
        let got = Template::new("x{Foo}y").render(&Tags::default(), Path::new("s.mp3"));
        assert_eq!(got, expect(&["xy.mp3"]));
    }

    #[test]
    fn disc_defaults_to_one_and_genre_falls_back() {
        let tags = Tags {
            title: Some("T".into()),
            artist: Some("A".into()),
            ..Default::default()
        };
        let got = Template::new("{Genre}/Disc {Disc}/{Title}").render(&tags, Path::new("x.flac"));
        assert_eq!(got, expect(&["Unknown Genre", "Disc 1", "T.flac"]));
    }

    #[test]
    fn src_without_extension_adds_no_extension() {
        let tags = Tags {
            title: Some("T".into()),
            artist: Some("A".into()),
            track: Some(3),
            ..Default::default()
        };
        let got = Template::new("{Artist}/{TrackNo} - {Title}").render(&tags, Path::new("noext"));
        assert_eq!(got, expect(&["A", "03 - T"]));
    }

    #[test]
    fn as_str_returns_raw() {
        assert_eq!(
            Template::new("{Artist} - {Title}").as_str(),
            "{Artist} - {Title}"
        );
    }

    #[test]
    fn presets_catalog_is_exact() {
        assert_eq!(
            presets(),
            &[
                ("artist-album", "{AlbumArtist}/{Album}/{TrackNo} - {Title}"),
                (
                    "artist-year-album",
                    "{AlbumArtist}/{Year} - {Album}/{TrackNo} - {Title}"
                ),
                (
                    "genre-artist-album",
                    "{Genre}/{AlbumArtist}/{Album}/{TrackNo} - {Title}"
                ),
                ("flat", "{Artist} - {Title}"),
            ]
        );
    }
}
