//! Disc-folder names: `CD1`, `cd 01`, `Disc 2 - Liyue`, `Disc02`, `Disk 3`,
//! `DVD 1`, `Side A`. The rule (case-insensitive, anchored):
//!
//! ```text
//! ^(cd|disc|disk|dvd|side)[\s._-]*(\d{1,3}|[a-d])\b.*$
//! ```
//!
//! `a..d` count as discs `1..4` (vinyl sides). Whatever follows the number,
//! trimmed of separators, is a **disc subtitle** fallback (`Disc 1 - Mondstadt`
//! → `Mondstadt`). `\b` is an ASCII word boundary (`[A-Za-z0-9_]`), so a CJK
//! subtitle glued to the number still matches. `no_std`, no-alloc: the
//! subtitle borrows from `name`.

/// A parsed disc-folder name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscFolder<'a> {
    /// The disc number (`Side A` → 1, `B` → 2, …).
    pub number: u16,
    /// The text after the number (`Disc 1 - Mondstadt` → `Mondstadt`), if any.
    pub subtitle: Option<&'a str>,
}

const PREFIXES: [&str; 5] = ["disc", "disk", "side", "dvd", "cd"];

/// `[\s._-]` — the separators allowed between the prefix and the number.
fn is_sep(c: char) -> bool {
    c.is_whitespace() || matches!(c, '.' | '_' | '-')
}

/// An ASCII `\w` byte — what a `\b` after the number must NOT be followed by.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Parse a folder name as a disc folder. `None` when it is not one.
pub fn parse(name: &str) -> Option<DiscFolder<'_>> {
    let bytes = name.as_bytes();
    let plen = PREFIXES
        .iter()
        .find(|p| bytes.len() >= p.len() && bytes[..p.len()].eq_ignore_ascii_case(p.as_bytes()))?
        .len();
    // The matched prefix is ASCII, so `plen` is a char boundary.
    let rest = name[plen..].trim_start_matches(is_sep);
    let rb = rest.as_bytes();
    let (number, used) = if rb.first().is_some_and(u8::is_ascii_digit) {
        let n = rb.iter().take_while(|b| b.is_ascii_digit()).count();
        if n > 3 {
            return None; // \d{1,3} then \b can never match a 4+ digit run
        }
        let mut v: u16 = 0;
        for &b in &rb[..n] {
            v = v * 10 + u16::from(b - b'0');
        }
        (v, n)
    } else {
        match rb.first().map(u8::to_ascii_lowercase) {
            Some(c @ b'a'..=b'd') => (u16::from(c - b'a') + 1, 1),
            _ => return None,
        }
    };
    if rb.get(used).is_some_and(|&b| is_word_byte(b)) {
        return None; // no word boundary after the number/letter
    }
    let tail =
        rest[used..].trim_matches(|c: char| is_sep(c) || matches!(c, ':' | '(' | ')' | '[' | ']'));
    Some(DiscFolder {
        number,
        subtitle: (!tail.is_empty()).then_some(tail),
    })
}

#[cfg(test)]
mod tests {
    use super::{DiscFolder, parse};

    fn df(number: u16, subtitle: Option<&str>) -> Option<DiscFolder<'_>> {
        Some(DiscFolder { number, subtitle })
    }

    #[test]
    fn matches_disc_folder_names() {
        assert_eq!(parse("CD1"), df(1, None));
        assert_eq!(parse("cd 01"), df(1, None));
        assert_eq!(parse("Disc 2 - Liyue"), df(2, Some("Liyue")));
        assert_eq!(parse("Disc02"), df(2, None));
        assert_eq!(parse("Disk 3"), df(3, None));
        assert_eq!(parse("DVD 1"), df(1, None));
        assert_eq!(parse("Side A"), df(1, None));
        assert_eq!(parse("Side B"), df(2, None));
        assert_eq!(parse("side d"), df(4, None));
        assert_eq!(parse("Disc 1 - Mondstadt"), df(1, Some("Mondstadt")));
        assert_eq!(parse("CD.2"), df(2, None));
        assert_eq!(parse("CD_3"), df(3, None));
        assert_eq!(parse("Disc 1 (Bonus)"), df(1, Some("Bonus")));
        assert_eq!(parse("DISC 12: Finale"), df(12, Some("Finale")));
        assert_eq!(parse("CD 100"), df(100, None));
        // ASCII word boundary: a CJK subtitle glued to the number still matches.
        assert_eq!(parse("Disc 1 中文"), df(1, Some("中文")));
    }

    #[test]
    fn rejects_non_disc_folders() {
        for name in [
            "Discography",
            "CDs",
            "Disc",
            "2CD",
            "CD Collection",
            "CD1234",
            "Album",
            "",
            "Sidewinder",
            "Disco 1",
            "Disc 2_Liyue", // `_` is a word char: no boundary after the number
            " CD1",         // anchored at the start: no leading whitespace
        ] {
            assert_eq!(parse(name), None, "{name:?} must not be a disc folder");
        }
    }
}
