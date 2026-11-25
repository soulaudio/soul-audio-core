//! Host-aware path comparison for the planner.
//!
//! Album roots, the source root, the destination root and every collision set
//! compare paths through [`key`]: a Windows verbatim prefix (`\\?\C:\…`,
//! `\\?\UNC\server\share\…` — what `std::fs::canonicalize` returns) is
//! stripped, and on Windows and macOS — whose default filesystems (NTFS, APFS)
//! are case-insensitive — names are case-folded. On Linux a key is the path
//! itself. So a canonicalized source root against plain inputs, or `Box` vs
//! `box`, never splits an album, flips the side-file decision, or lets two
//! destinations that are one file on disk both be planned.
//!
//! `soul-audio-import` applies the same rule in its `album_root` (it may not
//! depend on this crate), so album identity stays in parity.

use std::path::{Component, Path, PathBuf};

/// Whether this host's default filesystem compares names case-insensitively.
const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// `p` without a Windows verbatim prefix: `\\?\C:\x` → `C:\x`,
/// `\\?\UNC\srv\share\x` → `\\srv\share\x`. Any other path (and every path off
/// Windows, where `\\?\` is an ordinary name) is returned unchanged.
pub(crate) fn strip_verbatim(p: &Path) -> PathBuf {
    if cfg!(windows)
        && let Some(s) = p.to_str()
    {
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    p.to_path_buf()
}

/// The comparison key of `p` on this host: [`strip_verbatim`], then
/// case-folded on Windows/macOS (a non-UTF-8 path is left unfolded).
pub(crate) fn key(p: &Path) -> PathBuf {
    let p = strip_verbatim(p);
    if CASE_INSENSITIVE && let Some(s) = p.to_str() {
        return PathBuf::from(s.to_lowercase());
    }
    p
}

/// `p` is `base` or lies below it, compared by [`key`].
pub(crate) fn starts_with(p: &Path, base: &Path) -> bool {
    key(p).starts_with(key(base))
}

/// `a` and `b` name the same path, compared by [`key`].
pub(crate) fn same(a: &Path, b: &Path) -> bool {
    key(a) == key(b)
}

/// Whether `p` has a `..` component (it may escape any root it textually sits
/// under).
pub(crate) fn has_parent_dir(p: &Path) -> bool {
    p.components().any(|c| c == Component::ParentDir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_components_are_detected() {
        assert!(has_parent_dir(Path::new("lib/../etc/x.flac")));
        assert!(!has_parent_dir(Path::new("lib/..x/x.flac")));
    }

    #[test]
    fn plain_paths_compare_as_themselves() {
        assert!(starts_with(Path::new("lib/Box/CD1"), Path::new("lib/Box")));
        assert!(!starts_with(Path::new("lib/Boxed"), Path::new("lib/Box")));
        assert!(same(Path::new("lib/Box/"), Path::new("lib/Box")));
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefixes_are_stripped_on_windows() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\D:\lib\Box")),
            PathBuf::from(r"D:\lib\Box")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\nas\music\Box")),
            PathBuf::from(r"\\nas\music\Box")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"D:\lib")),
            PathBuf::from(r"D:\lib")
        );
        assert!(starts_with(
            Path::new(r"D:\lib\Box\CD1"),
            Path::new(r"\\?\D:\lib")
        ));
        assert!(same(Path::new(r"\\?\UNC\nas\m"), Path::new(r"\\nas\m")));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn names_fold_case_on_case_insensitive_hosts() {
        assert_eq!(key(Path::new("Lib/ÄBC")), key(Path::new("lib/äbc")));
        assert!(starts_with(Path::new("LIB/Box/CD1"), Path::new("lib/box")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_keys_are_the_path_itself() {
        assert_eq!(key(Path::new(r"\\?\D:\Lib")), PathBuf::from(r"\\?\D:\Lib"));
        assert!(!same(Path::new("Lib"), Path::new("lib")));
    }
}
