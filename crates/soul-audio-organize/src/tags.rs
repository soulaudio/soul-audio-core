//! Per-file tag reader: open a path and collect its owned [`Tags`], degrading to
//! [`Tags::default`] on any IO or parse error so planning never panics on an
//! unreadable or untagged file.

use std::path::Path;

use soul_audio_tags::Tags;

/// Read a file's tags, returning [`Tags::default`] when the file cannot be
/// opened or its container cannot be parsed. Mirrors `soul-audio-import`'s
/// private `read_meta`, discarding the
/// [`StreamFacts`](soul_audio_tags::StreamFacts) the organizer does not need.
// Wired into `plan` in the next layer; only the test harness exercises it today.
#[allow(dead_code)]
pub(crate) fn read_tags(path: &Path) -> Tags {
    match std::fs::File::open(path) {
        Ok(f) => {
            let mut src = embedded_io_adapters::std::FromStd::new(f);
            soul_audio_tags::read_owned(&mut src)
                .map(|(t, _facts)| t)
                .unwrap_or_default()
        }
        Err(_) => Tags::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::read_tags;
    use std::path::{Path, PathBuf};

    /// A committed fixture under the repo's shared `testdata/` dir.
    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata")
            .join(name)
    }

    #[test]
    fn missing_path_returns_default_tags_without_panicking() {
        let tags = read_tags(Path::new("no/such/file/exists.flac"));
        assert_eq!(tags, soul_audio_tags::Tags::default());
    }

    #[test]
    fn reads_title_artist_album_from_real_fixture() {
        let tags = read_tags(&testdata("tagged.flac"));
        assert_eq!(tags.title.as_deref(), Some("Soul Test Track"));
        assert_eq!(tags.artist.as_deref(), Some("Test Artist"));
        assert_eq!(tags.album.as_deref(), Some("Test Album"));
    }
}
