//! Lexical path resolution and alias-aware containment.
//!
//! Stage 2 (`lex`) of the grant pipeline: turns a sigil-expanded
//! string into an absolute, `.`/`..`-free `PathBuf`, joining with
//! a scoped cwd as needed.  Pure: no filesystem access.
//!
//! Also home to the alias-aware containment helpers
//! [`path_within`] and [`path_aliases`].  These are stage-4 of the
//! pipeline (the matcher), but they are pure lexical operations
//! and have always lived alongside [`resolve_path`] for that
//! reason.  The macOS firmlink table (`/tmp` ↔ `/private/tmp`,
//! etc.) lives here so the matcher and the canonicaliser see the
//! same view.

use std::path::{Component, Path, PathBuf};

use super::process_cwd;

/// Bidirectional firmlink/symlink pairs the kernel substitutes
/// transparently: a path under `a` and the same path under `b`
/// denote the same file.  macOS-only — empty elsewhere so
/// [`path_aliases`] is a no-op on Linux and Windows.
///
/// Used to keep the grant matcher correct when `canonicalize` is
/// unavailable — notably under Seatbelt, where `realpath(3)` can
/// fail on `/tmp` itself, causing comparisons to fall back to
/// lexical form on both sides.
#[cfg(target_os = "macos")]
const ALIASES: &[(&str, &str)] = &[
    ("/tmp", "/private/tmp"),
    ("/var", "/private/var"),
    ("/etc", "/private/etc"),
];

#[cfg(not(target_os = "macos"))]
const ALIASES: &[(&str, &str)] = &[];

/// Return `p` together with any alternate lexical forms that the
/// host filesystem treats as identical (e.g. `/tmp/foo` ↔
/// `/private/tmp/foo` on macOS).  Pure: no filesystem access.
///
/// The matcher uses this so a grant authored as one form still
/// covers an access expressed as the other, even when
/// `canonicalize` cannot be relied on to bridge them.
pub fn path_aliases(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![p.to_path_buf()];
    let s = p.to_string_lossy();
    for (a, b) in ALIASES {
        if let Some(rest) = strip_prefix_component(&s, a) {
            out.push(PathBuf::from(format!("{b}{rest}")));
        } else if let Some(rest) = strip_prefix_component(&s, b) {
            out.push(PathBuf::from(format!("{a}{rest}")));
        }
    }
    out
}

/// True iff some alias of `path` starts with some alias of
/// `prefix`, i.e. `path` lies inside `prefix` modulo the host's
/// known firmlinks.  Pure helper used by both the runtime grant
/// matcher and the nested grant intersector.
pub fn path_within(path: &Path, prefix: &Path) -> bool {
    let ps = path_aliases(path);
    let qs = path_aliases(prefix);
    ps.iter().any(|p| qs.iter().any(|q| p.starts_with(q)))
}

/// Like `str::strip_prefix`, but only matches when the prefix
/// ends on a path-component boundary — so `/tmp` does not
/// pseudo-match `/tmpx`.
fn strip_prefix_component<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(prefix)?;
    if rest.is_empty() || rest.starts_with('/') {
        Some(rest)
    } else {
        None
    }
}

/// Resolve `path` against `cwd`, normalising `.` and `..`
/// components.  If `path` is already absolute it is normalised in
/// place; otherwise it is joined to `cwd` (or to
/// `std::env::current_dir` when `cwd` is `None`).  Purely
/// lexical — no symlink resolution — so the result may differ
/// from `canonicalize`.
pub fn resolve_path(cwd: Option<&Path>, path: &str) -> PathBuf {
    let input = PathBuf::from(path);
    let joined = if input.is_absolute() {
        input
    } else if let Some(cwd) = cwd {
        cwd.join(input)
    } else if let Some(cwd) = process_cwd() {
        cwd.join(input)
    } else {
        PathBuf::from(path)
    };

    let mut normalized = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                let popped = normalized.pop();
                if !popped {
                    normalized.push(comp.as_os_str());
                }
            }
            _ => normalized.push(comp.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

/// Proper ancestors of `paths`, dedup'd, root excluded.  For each
/// input path, walk `Path::ancestors()` upward stopping above `/` and
/// collect every intermediate directory.  Output is sorted (BTreeSet
/// iteration order) and free of duplicates across inputs.
///
/// Used by the macOS Seatbelt builder to emit `file-read-metadata`
/// allows on the parents of each grant prefix (Seatbelt checks
/// parent-directory metadata during path lookup).  Generic enough to
/// live next to the path lattice rather than alongside the SBPL
/// renderer.
pub fn proper_ancestors<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for path in paths {
        for ancestor in Path::new(path).ancestors().skip(1) {
            if ancestor == Path::new("/") || ancestor.as_os_str().is_empty() {
                break;
            }
            out.insert(ancestor.to_string_lossy().into_owned());
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn aliases_includes_self() {
        assert!(path_aliases(Path::new("/usr/local")).contains(&pb("/usr/local")));
    }

    #[test]
    fn aliases_unrelated_path_unchanged() {
        let a = path_aliases(Path::new("/usr/bin/ls"));
        assert_eq!(a, vec![pb("/usr/bin/ls")]);
    }

    #[test]
    fn aliases_no_false_match_on_substring() {
        // `/tmp` must not pseudo-match `/tmpx`, on any platform.
        let a = path_aliases(Path::new("/tmpx/foo"));
        assert_eq!(a, vec![pb("/tmpx/foo")]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn aliases_tmp_both_directions() {
        let a = path_aliases(Path::new("/tmp/foo"));
        assert!(a.contains(&pb("/tmp/foo")));
        assert!(a.contains(&pb("/private/tmp/foo")));

        let b = path_aliases(Path::new("/private/tmp/foo"));
        assert!(b.contains(&pb("/tmp/foo")));
        assert!(b.contains(&pb("/private/tmp/foo")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn aliases_var_folders() {
        let a = path_aliases(Path::new("/var/folders/xy/abc"));
        assert!(a.contains(&pb("/private/var/folders/xy/abc")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn aliases_root_only() {
        let a = path_aliases(Path::new("/tmp"));
        assert!(a.contains(&pb("/private/tmp")));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn aliases_no_op_off_macos() {
        // Off macOS the alias table is empty, so the result is just `[p]`.
        for s in ["/tmp/foo", "/private/tmp/foo", "/var/folders/xy", "/etc/passwd"] {
            assert_eq!(path_aliases(Path::new(s)), vec![pb(s)]);
        }
    }

    #[test]
    fn path_within_self() {
        assert!(path_within(Path::new("/a/b"), Path::new("/a/b")));
    }

    #[test]
    fn path_within_strict_descendant() {
        assert!(path_within(Path::new("/a/b/c"), Path::new("/a/b")));
    }

    #[test]
    fn path_within_not_a_descendant() {
        assert!(!path_within(Path::new("/a/b"), Path::new("/a/c")));
        assert!(!path_within(Path::new("/a"), Path::new("/a/b")));
    }

    #[test]
    fn path_within_no_substring_pseudomatch() {
        assert!(!path_within(Path::new("/tmpx"), Path::new("/tmp")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn path_within_via_alias() {
        assert!(path_within(Path::new("/tmp/foo"), Path::new("/private/tmp")));
        assert!(path_within(Path::new("/private/tmp/foo"), Path::new("/tmp")));
    }
}
