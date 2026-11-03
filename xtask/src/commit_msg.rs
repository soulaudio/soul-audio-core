//! Commit-message policy check, run by lefthook's `commit-msg` hook as
//! `cargo xtask check-commit-msg <path-to-message-file>`.
//!
//! Pure Rust (no grep/sh) so it behaves identically on Windows, macOS, Linux,
//! and CI — the same reason the rest of `xtask` exists. It enforces two rules:
//!
//! 1. **Conventional Commits** header: `type(scope)!: description`.
//! 2. **No AI / attribution** anywhere in the message (Claude, Anthropic, a
//!    bare "AI", or any co-author / "Generated with" trailer). Project policy:
//!    the public history stays free of tool attribution.

use std::fs;

/// Allowed Conventional Commits types.
const TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore", "revert",
];

/// Substrings forbidden anywhere in the message (matched case-insensitively).
const FORBIDDEN: &[&str] = &[
    "claude",
    "anthropic",
    "co-author",
    "coauthor",
    "generated with",
    "noreply@anthropic.com",
    "\u{1F916}", // 🤖
];

/// Subject prefixes that are legitimately *not* Conventional Commits.
const EXEMPT_PREFIXES: &[&str] = &["Merge ", "Revert ", "fixup! ", "squash! ", "amend! "];

/// Entry point for `cargo xtask check-commit-msg <path>`.
pub fn check(path: Option<String>) -> Result<(), String> {
    let path = path.ok_or("check-commit-msg: missing commit-message file path")?;
    let raw = fs::read_to_string(&path).map_err(|e| format!("reading {path}: {e}"))?;
    check_message(&strip_git_noise(&raw))
}

/// Drop everything git itself would drop before storing the message: the
/// verbose-diff section after the scissors line, and comment (`#`) lines.
fn strip_git_noise(raw: &str) -> String {
    raw.lines()
        .take_while(|l| !l.trim_start().starts_with("# ------------------------ >8"))
        .filter(|l| !l.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Validate the cleaned message body. Kept separate from [`check`] so it is
/// unit-testable without touching the filesystem.
fn check_message(body: &str) -> Result<(), String> {
    check_no_ai(body)?;
    check_conventional(body)?;
    Ok(())
}

fn check_no_ai(body: &str) -> Result<(), String> {
    let lower = body.to_lowercase();
    if let Some(pat) = FORBIDDEN.iter().find(|p| lower.contains(**p)) {
        return Err(ai_error(pat));
    }
    // A standalone "ai" token — without false-positiving "AIFF", "detail",
    // "chain", etc. (those are single alphanumeric tokens, not equal to "ai").
    if lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| tok == "ai")
    {
        return Err(ai_error("ai"));
    }
    Ok(())
}

fn ai_error(pat: &str) -> String {
    format!(
        "commit message contains a forbidden reference ({pat:?}).\n\
         Project policy: the history must not mention Claude, Anthropic, AI, or any\n\
         co-author / \"Generated with\" trailer. Remove that text and commit again."
    )
}

fn check_conventional(body: &str) -> Result<(), String> {
    let subject = body
        .lines()
        .find(|l| !l.trim().is_empty())
        .ok_or("commit message is empty")?
        .trim();

    if EXEMPT_PREFIXES.iter().any(|p| subject.starts_with(p)) {
        return Ok(());
    }

    parse_header(subject).map_err(|e| {
        format!(
            "commit subject is not a valid Conventional Commit:\n  {subject:?}\n  -> {e}\n\n\
             Expected:  <type>(<scope>)!: <description>\n\
             Types:     {}\n\
             Examples:  feat(core): add WavPack seeking\n           \
             fix(tags): correct ID3v2.4 size semantics\n           \
             refactor(engine)!: drop the legacy cursor API",
            TYPES.join(", ")
        )
    })
}

/// Parse `type(scope)!: description`, erroring with a specific reason.
fn parse_header(subject: &str) -> Result<(), String> {
    let (prefix, desc) = subject
        .split_once(": ")
        .ok_or("missing the \"<type>: \" prefix (a colon followed by a space)")?;
    if desc.trim().is_empty() {
        return Err("the description after the colon is empty".into());
    }

    // An optional trailing "!" marks a breaking change.
    let prefix = prefix.strip_suffix('!').unwrap_or(prefix);

    let type_ = match prefix.find('(') {
        Some(open) => {
            if !prefix.ends_with(')') {
                return Err("the scope must be wrapped in parentheses, e.g. feat(core):".into());
            }
            if prefix[open + 1..prefix.len() - 1].trim().is_empty() {
                return Err("the scope parentheses are empty".into());
            }
            &prefix[..open]
        }
        None => prefix,
    };

    if !TYPES.contains(&type_) {
        return Err(format!(
            "\"{type_}\" is not an allowed type ({})",
            TYPES.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check_message;

    #[test]
    fn accepts_valid_headers() {
        for ok in [
            "feat(core): add WavPack seeking",
            "fix: handle empty input",
            "refactor(engine)!: drop the legacy cursor API",
            "test(core): real-music fixtures across all codecs (#99)",
            "feat(core): AIFF / AIFF-C container support", // "AIFF" must not trip the "ai" rule
            "docs: explain the detail of the chain",       // "detail"/"chain" must not trip "ai"
        ] {
            assert!(check_message(ok).is_ok(), "should accept: {ok:?}");
        }
    }

    #[test]
    fn rejects_bad_headers() {
        for bad in [
            "add a thing",          // no type
            "feat - add a thing",   // no colon-space
            "wip(core): something", // bad type
            "feat(): empty scope",  // empty scope
            "feat(core): ",         // empty description
        ] {
            assert!(check_message(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn rejects_ai_attribution() {
        for bad in [
            "feat: x\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
            "fix: y\n\n\u{1F916} Generated with Claude Code",
            "feat: add ai inference",
            "chore: bump anthropic sdk",
        ] {
            assert!(check_message(bad).is_err(), "should reject: {bad:?}");
        }
    }

    #[test]
    fn exempts_merge_and_revert() {
        assert!(check_message("Merge branch 'main' into feature").is_ok());
        assert!(check_message("Revert \"feat: something\"").is_ok());
    }

    #[test]
    fn missing_path_errors() {
        assert!(super::check(None).is_err());
    }

    #[test]
    fn strips_comment_lines() {
        // git-supplied comment lines must be dropped, not validated or scanned.
        let raw = "feat(core): real subject\n\n# Please enter the commit message\n# anthropic";
        assert!(check_message(&super::strip_git_noise(raw)).is_ok());
    }
}
