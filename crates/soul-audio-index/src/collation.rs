//! Collation sort keys for browse ordering.
//!
//! The desktop baker sorts each `.soulidx` order table by these keys, so the
//! `no_std`/no-alloc **device reads the frozen order and never runs collation**
//! (it has no locale tables, no heap). The desktop `MultiCatalog` merge
//! recomputes the *same* key to compare heads across sources — which is why the
//! key is deterministic and byte-comparable: `key(a).cmp(&key(b))` is exactly
//! the browse order, whether compared on the desktop or baked for the device.
//!
//! The key folds the library conventions S-tier players use (see MusicBrainz
//! sort names + ICU collation keys):
//! - **leading articles** are stripped for the primary order ("The Beatles"
//!   files under **B**), but the **full name breaks ties** so "Kills" and "The
//!   Kills" keep a total order;
//! - **case** is folded (lower-cased);
//! - **numbers sort naturally** ("Track 2" before "Track 10"), leading zeros
//!   ignored ("007" == "7").
//!
//! This is a pragmatic, dependency-free pass (no full ICU). Because the device
//! only ever byte-compares the *frozen* order, the key algorithm can be enriched
//! later (fuller Unicode collation) without touching the reader or the format.

use alloc::string::String;
use alloc::vec::Vec;

/// Leading articles folded out of the primary order. Lower-cased, with the
/// trailing space (or apostrophe) that marks them an article and not a prefix of
/// a longer word ("the " strips from "The Kinks", not from "Theory").
const ARTICLES: &[&str] = &[
    "the ", "a ", "an ", "le ", "la ", "les ", "l'", "der ", "die ", "das ", "el ", "los ", "las ",
    "il ", "lo ", "gli ", "un ", "una ", "o ", "os ", "as ",
];

/// A byte-comparable collation key: `key(a).cmp(&key(b))` is the browse order.
#[must_use]
pub fn key(s: &str) -> Vec<u8> {
    let lower = s.to_lowercase();
    let primary = strip_article(&lower);
    let mut out = Vec::with_capacity(lower.len() * 2 + 2);
    natural_into(primary, &mut out);
    out.push(0x00); // section break: article-stripped primary, then full-name tiebreak
    natural_into(&lower, &mut out);
    out
}

/// Drop a single leading article, but only if a non-empty remainder follows.
fn strip_article(lower: &str) -> &str {
    for a in ARTICLES {
        if let Some(rest) = lower.strip_prefix(a) {
            let rest = rest.trim_start();
            if !rest.is_empty() {
                return rest;
            }
        }
    }
    lower
}

/// Encode `s` so maximal ASCII-digit runs sort numerically: each becomes
/// `0x01, len, digits` (leading zeros trimmed), so "2" (len 1) sorts before "10"
/// (len 2). Other scalars pass through as their UTF-8 bytes.
fn natural_into(s: &str, out: &mut Vec<u8>) {
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut digits = String::new();
            while let Some(&d) = chars.peek() {
                if d.is_ascii_digit() {
                    digits.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            let trimmed = digits.trim_start_matches('0');
            let t = if trimmed.is_empty() { "0" } else { trimmed };
            out.push(0x01);
            out.push(t.len().min(255) as u8);
            out.extend_from_slice(t.as_bytes());
        } else {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            chars.next();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::key;
    use alloc::vec;
    use alloc::vec::Vec;

    fn sorted(mut v: Vec<&str>) -> Vec<&str> {
        v.sort_by_key(|a| key(a));
        v
    }

    #[test]
    fn ignores_leading_article_but_keeps_total_order() {
        // "The Beatles" files under B; "Kills" and "The Kills" keep a total order
        // (stripped keys tie, full name breaks it: "kills" < "the kills").
        assert_eq!(
            sorted(vec!["Zz Top", "The Beatles", "ABBA", "The Kills", "Kills"]),
            vec!["ABBA", "The Beatles", "Kills", "The Kills", "Zz Top"],
        );
    }

    #[test]
    fn sorts_numbers_naturally() {
        assert_eq!(
            sorted(vec!["Track 10", "Track 2", "Track 1", "Track 20"]),
            vec!["Track 1", "Track 2", "Track 10", "Track 20"],
        );
    }

    #[test]
    fn leading_zeros_and_case_fold() {
        assert_eq!(key("Disc 007"), key("disc 7"));
        assert_eq!(key("ABBA"), key("abba"));
    }

    #[test]
    fn a_bare_article_is_not_stripped_to_empty() {
        // "The" alone (no following word) must not collapse to an empty key.
        assert!(!key("The").is_empty());
        assert_ne!(key("The"), key(""));
    }
}
