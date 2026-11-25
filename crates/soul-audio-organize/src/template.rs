//! Path-template renderer: turn a `{AlbumArtist}/{Album}/{TrackNo} - {Title}`
//! style template plus a file's [`Tags`] into a relative, filesystem-safe
//! destination path.
//!
//! # Tokens
//! The supported tokens are, case-sensitive and exact: `{AlbumArtist}`,
//! `{Artist}`, `{Album}`, `{Year}`, `{Disc}`, `{DiscPrefix}`, `{DiscDir}`,
//! `{DiscTotal}`, `{TrackNo}`, `{Title}`, `{Genre}`. An **unknown** `{...}`
//! token renders as the empty string.
//!
//! # Disc tokens
//! The album's disc count is `max(disc_total, disc, 1)` from the tags (the
//! planner sets both to the album's effective values). `{DiscPrefix}` is `""`
//! for a single-disc album, else the disc number zero-padded to the width of
//! the disc count plus `-` (`1-`, `01-` for >= 10 discs) - so
//! `{DiscPrefix}{TrackNo}` is `1-05`. `{DiscDir}` is `""` for a single-disc
//! album, else `Disc N` (same padding). `{DiscTotal}` is the disc count.
//!
//! # Empty segments
//! A **directory** segment that renders empty (or whitespace) - `{DiscDir}` of
//! a single-disc album, `{Year}` without a date - is dropped, not turned into
//! `_`, so `{Album}/{DiscDir}/{TrackNo}` gives `Album/05` for one disc and
//! `Album/Disc 1/05` for several. The file-name segment still becomes `_`.
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
        self.render_inner(tags, src, false)
    }

    /// The folder a track renders into with `{DiscDir}` forced empty — relative
    /// to the destination root. The planner takes the common ancestor of these
    /// over an album's tracks as the album's own folder (where side files go).
    pub(crate) fn render_album_dir(&self, tags: &Tags, src: &Path) -> PathBuf {
        self.render_inner(tags, src, true)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    /// Whether the template lays discs out as folders (`{DiscDir}`).
    pub(crate) fn has_disc_dir(&self) -> bool {
        self.raw.contains("{DiscDir}")
    }

    /// [`render`](Self::render), optionally forcing `{DiscDir}` empty (the
    /// planner uses that to find an album's own folder).
    pub(crate) fn render_inner(&self, tags: &Tags, src: &Path, neutral_disc_dir: bool) -> PathBuf {
        let mut path = PathBuf::new();
        let mut segments = self.raw.split('/').peekable();
        while let Some(template_segment) = segments.next() {
            let substituted = substitute(template_segment, tags, src, neutral_disc_dir);
            let is_file_name = segments.peek().is_none();
            if !is_file_name && substituted.trim().is_empty() {
                continue; // an empty directory level is dropped, not `_`
            }
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
fn substitute(segment: &str, tags: &Tags, src: &Path, neutral_disc_dir: bool) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut rest = segment;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                if let Some(value) = token_value(&after[..close], tags, src, neutral_disc_dir) {
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
fn token_value(name: &str, tags: &Tags, src: &Path, neutral_disc_dir: bool) -> Option<String> {
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
        "DiscPrefix" => {
            let (disc, count) = disc_and_count(tags);
            if count > 1 {
                format!("{}-", disc_label(disc, count))
            } else {
                String::new()
            }
        }
        "DiscDir" => {
            let (disc, count) = disc_and_count(tags);
            if count > 1 && !neutral_disc_dir {
                disc_dir_name(disc, count)
            } else {
                String::new()
            }
        }
        "DiscTotal" => disc_and_count(tags).1.to_string(),
        // First 4 chars of `date` parsed as a year, else "".
        "Year" => year(tags).map(|y| y.to_string()).unwrap_or_default(),
        _ => return None,
    };
    Some(value)
}

/// `(disc, disc_count)` for the disc tokens: the disc number (missing or 0 ->
/// 1) and `max(disc_total, disc, 1)`.
fn disc_and_count(tags: &Tags) -> (u32, u32) {
    let disc = tags.disc.filter(|&d| d > 0).unwrap_or(1);
    (disc, tags.disc_total.unwrap_or(0).max(disc))
}

/// `disc` zero-padded to the digit width of `disc_count` (`2` of 3 -> `"2"`,
/// of 12 -> `"02"`).
pub(crate) fn disc_label(disc: u32, disc_count: u32) -> String {
    let width = disc_count.max(disc).to_string().len();
    format!("{disc:0width$}")
}

/// The `{DiscDir}` folder name: `Disc N`, padded like [`disc_label`].
pub(crate) fn disc_dir_name(disc: u32, disc_count: u32) -> String {
    format!("Disc {}", disc_label(disc, disc_count))
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

/// Built-in named templates: `(name, template)`. Every album preset is
/// disc-safe (`{DiscPrefix}` - identical output for single-disc albums) and
/// has a `+disc-folders` twin that puts each disc in a `Disc N/` folder.
pub fn presets() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "artist-album",
            "{AlbumArtist}/{Album}/{DiscPrefix}{TrackNo} - {Title}",
        ),
        (
            "artist-album+disc-folders",
            "{AlbumArtist}/{Album}/{DiscDir}/{TrackNo} - {Title}",
        ),
        (
            "artist-year-album",
            "{AlbumArtist}/{Year} - {Album}/{DiscPrefix}{TrackNo} - {Title}",
        ),
        (
            "artist-year-album+disc-folders",
            "{AlbumArtist}/{Year} - {Album}/{DiscDir}/{TrackNo} - {Title}",
        ),
        (
            "genre-artist-album",
            "{Genre}/{AlbumArtist}/{Album}/{DiscPrefix}{TrackNo} - {Title}",
        ),
        (
            "genre-artist-album+disc-folders",
            "{Genre}/{AlbumArtist}/{Album}/{DiscDir}/{TrackNo} - {Title}",
        ),
        ("flat", "{Artist} - {Title}"),
    ]
}

/// Look up a built-in template by name (e.g. `"artist-album+disc-folders"`).
pub fn preset(name: &str) -> Option<&'static str> {
    presets().iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

#[cfg(test)]
mod tests {
    use super::{Template, preset, presets};
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
    fn neutral_disc_dir_renders_empty_and_gives_the_album_folder() {
        let tags = disc_tags(2, Some(3));
        let t = Template::new(preset("artist-album+disc-folders").unwrap());
        assert_eq!(
            t.render(&tags, Path::new("x.flac")),
            expect(&["Unknown Artist", "Alb", "Disc 2", "05 - T.flac"])
        );
        assert_eq!(
            t.render_inner(&tags, Path::new("x.flac"), true),
            expect(&["Unknown Artist", "Alb", "05 - T.flac"]),
            "{{DiscDir}} neutralized: the segment is dropped"
        );
        assert_eq!(
            t.render_album_dir(&tags, Path::new("x.flac")),
            expect(&["Unknown Artist", "Alb"])
        );
        assert!(t.has_disc_dir());
        let flat_prefix = Template::new(preset("artist-album").unwrap());
        assert!(!flat_prefix.has_disc_dir());
        assert_eq!(
            flat_prefix.render_album_dir(&tags, Path::new("x.flac")),
            expect(&["Unknown Artist", "Alb"])
        );
    }

    #[test]
    fn as_str_returns_raw() {
        assert_eq!(
            Template::new("{Artist} - {Title}").as_str(),
            "{Artist} - {Title}"
        );
    }

    fn disc_tags(disc: u32, total: Option<u32>) -> Tags {
        Tags {
            title: Some("T".into()),
            album: Some("Alb".into()),
            track: Some(5),
            disc: Some(disc),
            disc_total: total,
            ..Default::default()
        }
    }

    #[test]
    fn disc_prefix_is_empty_for_a_single_disc_album() {
        let t = Template::new("{DiscPrefix}{TrackNo} - {Title}");
        assert_eq!(
            t.render(&disc_tags(1, Some(1)), Path::new("x.flac")),
            expect(&["05 - T.flac"])
        );
        let untagged = Tags {
            title: Some("T".into()),
            track: Some(5),
            ..Default::default()
        };
        assert_eq!(
            t.render(&untagged, Path::new("x.flac")),
            expect(&["05 - T.flac"]),
            "untagged = single disc"
        );
    }

    #[test]
    fn disc_prefix_pads_to_the_disc_count_width() {
        let t = Template::new("{DiscPrefix}{TrackNo}");
        assert_eq!(
            t.render(&disc_tags(2, Some(3)), Path::new("x.flac")),
            expect(&["2-05.flac"])
        );
        assert_eq!(
            t.render(&disc_tags(3, Some(12)), Path::new("x.flac")),
            expect(&["03-05.flac"])
        );
        // No total tag: the count is inferred from the disc number itself.
        assert_eq!(
            t.render(&disc_tags(2, None), Path::new("x.flac")),
            expect(&["2-05.flac"])
        );
    }

    #[test]
    fn disc_dir_segment_is_dropped_for_a_single_disc_album() {
        let t = Template::new("{Album}/{DiscDir}/{TrackNo} - {Title}");
        assert_eq!(
            t.render(&disc_tags(1, Some(1)), Path::new("x.flac")),
            expect(&["Alb", "05 - T.flac"])
        );
        assert_eq!(
            t.render(&disc_tags(2, Some(2)), Path::new("x.flac")),
            expect(&["Alb", "Disc 2", "05 - T.flac"])
        );
        assert_eq!(
            t.render(&disc_tags(2, Some(10)), Path::new("x.flac")),
            expect(&["Alb", "Disc 02", "05 - T.flac"])
        );
    }

    #[test]
    fn disc_total_token_is_the_disc_count() {
        let t = Template::new("{Disc} of {DiscTotal}");
        assert_eq!(
            t.render(&disc_tags(2, Some(3)), Path::new("x.flac")),
            expect(&["2 of 3.flac"])
        );
        assert_eq!(
            t.render(&Tags::default(), Path::new("x.flac")),
            expect(&["1 of 1.flac"])
        );
    }

    #[test]
    fn empty_directory_segments_are_dropped_but_an_empty_file_name_is_not() {
        let tags = Tags {
            album: Some("Alb".into()),
            title: Some("T".into()),
            ..Default::default()
        };
        // `{Year}` with no date renders empty -> the directory level disappears.
        assert_eq!(
            Template::new("{Year}/{Album}/{Title}").render(&tags, Path::new("x.flac")),
            expect(&["Alb", "T.flac"])
        );
        // The final (file-name) segment still sanitizes to `_`.
        assert_eq!(
            Template::new("{Album}/{Year}").render(&tags, Path::new("x.flac")),
            expect(&["Alb", "_.flac"])
        );
    }

    #[test]
    fn presets_are_byte_identical_to_the_old_ones_for_single_disc_albums() {
        let old = [
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
        ];
        let single = Tags {
            title: Some("Hells Bells".into()),
            artist: Some("AC/DC".into()),
            album: Some("Back in Black".into()),
            date: Some("1980".into()),
            genre: Some("Rock".into()),
            track: Some(1),
            disc: Some(1),
            disc_total: Some(1),
            ..Default::default()
        };
        let src = Path::new("x.flac");
        for (name, old_tpl) in old {
            let want = Template::new(old_tpl).render(&single, src);
            assert_eq!(
                Template::new(preset(name).unwrap()).render(&single, src),
                want,
                "{name}"
            );
            if name != "flat" {
                let df = preset(&format!("{name}+disc-folders")).unwrap();
                assert_eq!(
                    Template::new(df).render(&single, src),
                    want,
                    "{name}+disc-folders"
                );
            }
        }
    }

    #[test]
    fn multi_disc_presets_render_both_layouts() {
        let tags = Tags {
            title: Some("Intro".into()),
            album_artist: Some("HOYO-MiX".into()),
            album: Some("Box".into()),
            track: Some(1),
            disc: Some(2),
            disc_total: Some(2),
            ..Default::default()
        };
        let src = Path::new("x.flac");
        assert_eq!(
            Template::new(preset("artist-album").unwrap()).render(&tags, src),
            expect(&["HOYO-MiX", "Box", "2-01 - Intro.flac"])
        );
        assert_eq!(
            Template::new(preset("artist-album+disc-folders").unwrap()).render(&tags, src),
            expect(&["HOYO-MiX", "Box", "Disc 2", "01 - Intro.flac"])
        );
        assert_eq!(preset("no-such-preset"), None);
    }

    #[test]
    fn presets_catalog_is_exact() {
        assert_eq!(
            presets(),
            &[
                (
                    "artist-album",
                    "{AlbumArtist}/{Album}/{DiscPrefix}{TrackNo} - {Title}"
                ),
                (
                    "artist-album+disc-folders",
                    "{AlbumArtist}/{Album}/{DiscDir}/{TrackNo} - {Title}"
                ),
                (
                    "artist-year-album",
                    "{AlbumArtist}/{Year} - {Album}/{DiscPrefix}{TrackNo} - {Title}"
                ),
                (
                    "artist-year-album+disc-folders",
                    "{AlbumArtist}/{Year} - {Album}/{DiscDir}/{TrackNo} - {Title}"
                ),
                (
                    "genre-artist-album",
                    "{Genre}/{AlbumArtist}/{Album}/{DiscPrefix}{TrackNo} - {Title}"
                ),
                (
                    "genre-artist-album+disc-folders",
                    "{Genre}/{AlbumArtist}/{Album}/{DiscDir}/{TrackNo} - {Title}"
                ),
                ("flat", "{Artist} - {Title}"),
            ]
        );
    }
}
