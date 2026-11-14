//! Path-template renderer. Placeholder scaffold — the real token substitution,
//! tag fallbacks, and presets land in Step 3.

use std::path::{Path, PathBuf};

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

    /// Render a relative, sanitized destination path. Placeholder — Step 3 fills
    /// in token substitution and extension handling.
    pub fn render(&self, _tags: &soul_audio_tags::Tags, _src: &Path) -> PathBuf {
        PathBuf::from(sanitize_segment(&self.raw))
    }
}

/// Built-in named templates. Placeholder — Step 3 fills in the catalog.
pub fn presets() -> &'static [(&'static str, &'static str)] {
    &[]
}
