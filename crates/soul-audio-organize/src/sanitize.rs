//! Filesystem-safe path segment sanitization.
//!
//! Produces a single path segment that is safe on the strictest common
//! denominator (Windows / NTFS, and the DAP's FAT card), so one destination
//! layout works unchanged across every body. Each Windows-illegal character
//! (`< > : " / \ | ? *`) and every ASCII control character (`0x00`–`0x1F`) is
//! replaced with `_`; surrounding spaces and *trailing* dots (which Windows
//! silently strips) are removed, while leading dots are kept (so a title like
//! "...And Justice for All" survives); a reserved device name (`CON`, `COM1`,
//! …) gets a `_` appended so it can never name a real device; and an empty
//! result becomes `_`. All other Unicode is preserved verbatim.

/// Windows-reserved device names, checked case-insensitively. `COM1`..=`COM9`
/// and `LPT1`..=`LPT9` are handled separately (a single trailing digit `1`–`9`).
const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL"];

/// Windows-illegal filename characters.
const ILLEGAL: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Is `stem` (the part of a segment before its first `.`) a reserved device name?
fn is_reserved_stem(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    if RESERVED.contains(&upper.as_str()) {
        return true;
    }
    if let Some(rest) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    {
        // Exactly one digit, 1–9 (COM0, COM10, COM are all fine).
        let mut chars = rest.chars();
        if let (Some(d), None) = (chars.next(), chars.next()) {
            return d.is_ascii_digit() && d != '0';
        }
    }
    false
}

/// Make `s` safe to use as a single path segment on the host filesystem.
pub(crate) fn sanitize_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ILLEGAL.contains(&c) || matches!(c, '\u{0}'..='\u{1f}') {
            out.push('_');
        } else {
            out.push(c);
        }
    }

    // Windows strips trailing dots/spaces; also drop surrounding spaces. Leading
    // dots are preserved on purpose.
    let trimmed = out.trim_start_matches(' ').trim_end_matches([' ', '.']);
    if trimmed.is_empty() {
        return "_".to_owned();
    }

    // Reserved-name check on the stem (before any `.`), so `CON.foo` is caught too.
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    if is_reserved_stem(stem) {
        let mut res = String::with_capacity(trimmed.len() + 1);
        res.push_str(trimmed);
        res.push('_');
        res
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_segment;

    #[test]
    fn replaces_path_separator() {
        assert_eq!(sanitize_segment("AC/DC"), "AC_DC");
    }

    #[test]
    fn replaces_each_windows_illegal_char() {
        assert_eq!(sanitize_segment("a:b*c?"), "a_b_c_");
    }

    #[test]
    fn appends_underscore_to_reserved_device_name() {
        assert_eq!(sanitize_segment("CON"), "CON_");
    }

    #[test]
    fn reserved_name_check_is_case_insensitive() {
        assert_eq!(sanitize_segment("con"), "con_");
    }

    #[test]
    fn com_and_lpt_ports_are_reserved() {
        assert_eq!(sanitize_segment("COM1"), "COM1_");
        assert_eq!(sanitize_segment("lpt9"), "lpt9_");
        // not reserved: no digit / out-of-range / extra digits.
        assert_eq!(sanitize_segment("COM"), "COM");
        assert_eq!(sanitize_segment("COM0"), "COM0");
        assert_eq!(sanitize_segment("COM10"), "COM10");
    }

    #[test]
    fn trims_trailing_dots_and_spaces() {
        assert_eq!(sanitize_segment("  trail. "), "trail");
    }

    #[test]
    fn empty_becomes_underscore() {
        assert_eq!(sanitize_segment(""), "_");
    }

    #[test]
    fn preserves_other_unicode() {
        assert_eq!(sanitize_segment("Café"), "Café");
    }

    #[test]
    fn replaces_ascii_control_char() {
        assert_eq!(sanitize_segment("a\u{07}b"), "a_b");
    }

    #[test]
    fn leading_dots_are_preserved() {
        // Windows strips only *trailing* dots; a leading-dot title such as
        // Metallica's "...And Justice for All" must survive.
        assert_eq!(
            sanitize_segment("...And Justice for All"),
            "...And Justice for All"
        );
    }
}
