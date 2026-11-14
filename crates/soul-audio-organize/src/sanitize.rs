//! Filesystem-safe path segment sanitization. Placeholder scaffold — the real
//! Windows-safe rules land in Step 2.

/// Make `s` safe to use as a single path segment on the host filesystem.
pub(crate) fn sanitize_segment(s: &str) -> String {
    s.to_owned()
}
