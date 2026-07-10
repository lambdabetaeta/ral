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
//! reason.  The macOS firmlink table and its toggle live in
//! [`super::canon`]; the matcher reuses them so it and the
//! canonicaliser see the same view.

use std::path::{Component, Path, PathBuf};

use super::process_cwd;

/// Return `p` together with any alternate lexical forms that the
/// host filesystem treats as identical (e.g. `/tmp/foo` ↔
/// `/private/tmp/foo` on macOS).  Pure: no filesystem access.
/// macOS-only — the firmlink table is empty elsewhere, so this is
/// `[p]` on Linux and Windows.
///
/// The matcher uses this so a grant authored as one form still
/// covers an access expressed as the other, even when
/// `canonicalize` cannot be relied on to bridge them — notably
/// under Seatbelt, where `realpath(3)` can fail on `/tmp` itself.
/// The toggle is [`super::canon::firmlink_toggle`], the same
/// primitive the canonicaliser uses, so the matcher's view of the
/// firmlink rewrite can never diverge from the canonicaliser's.
pub fn path_aliases(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![p.to_path_buf()];
    out.extend(super::canon::firmlink_toggle(p));
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

/// Resolve `path` against `cwd`, normalising `.` and `..`
/// components.  If `path` is already absolute it is normalised in
/// place; otherwise it is joined to `cwd` (or to
/// `std::env::current_dir` when `cwd` is `None`).  Purely
/// lexical — no symlink resolution — so the result may differ
/// from `canonicalize`.
#[allow(clippy::disallowed_methods)]
pub fn resolve_path(cwd: Option<&Path>, path: &str) -> PathBuf {
    let input = PathBuf::from(path);
    let joined = if input.is_absolute() {
        input
    } else if let Some(cwd) = cwd {
        cwd.join(input)
    } else if let Some(cwd) = process_cwd() {
        cwd.join(input)
    } else {
        input
    };

    let normalized = fold_dots(&joined);
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

/// Fold `.`/`..` components lexically, without touching the filesystem or
/// the cwd: `CurDir` drops, `ParentDir` pops (or is kept when there is
/// nothing to pop, so a leading `..` survives), every other component is
/// pushed.  The shared kernel of [`resolve_path`] (which joins a cwd
/// first) and [`super::canon::canonicalise_lenient`] (which folds cwd-free
/// before its existing-ancestor walk).
pub(crate) fn fold_dots(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(comp.as_os_str());
                }
            }
            _ => normalized.push(comp.as_os_str()),
        }
    }
    normalized
}

/// Like [`resolve_path`], but takes the cwd as a string.  Thin
/// wrapper for cross-crate callers (exarch policy loading) that
/// hold the cwd as a `&str` and would otherwise reach for
/// `Path::new` themselves.
#[allow(clippy::disallowed_methods)]
pub fn resolve_str(cwd: Option<&str>, path: &str) -> PathBuf {
    resolve_path(cwd.map(Path::new), path)
}

/// Like [`path_within`], but takes both arguments as strings.
/// Saves callers a `Path::new(p)` pair at every call site.
#[allow(clippy::disallowed_methods)]
pub fn path_within_str(path: &str, prefix: &str) -> bool {
    path_within(Path::new(path), Path::new(prefix))
}

/// Intersect two prefix sets, keeping every element covered by some
/// element of the other set — i.e. the deeper prefix of each
/// overlapping pair survives.  `key` projects each item to the path
/// string overlap is judged on.  Module-private to `path`: the sole
/// caller is [`PrefixSet`](super::PrefixSet)'s `Meet`, which always keys
/// on the symlink-resolved form — so a confinement meet can never fall
/// back to lexical (surface-string) overlap.  The result is unsorted and
/// may contain duplicates; the caller applies its own dedup/ordering.
pub(in crate::path) fn meet_prefix_sets_by<T: Clone>(
    a: &[T],
    b: &[T],
    key: impl Fn(&T) -> &str,
) -> Vec<T> {
    let covered = |x: &T, others: &[T]| others.iter().any(|o| path_within_str(key(x), key(o)));
    a.iter()
        .filter(|x| covered(x, b))
        .cloned()
        .chain(b.iter().filter(|y| covered(y, a)).cloned())
        .collect()
}

/// Resolve `path` relative to the directory containing `script`.
/// If `path` is absolute it is returned unchanged.  If `script` is
/// empty or starts with `<` (the synthetic-source convention used
/// by the REPL, `eval`, etc.) the input is returned unchanged so
/// the caller can fall back to cwd-relative resolution.
///
/// The third anchor in the resolver lattice, after cwd-relative
/// (most builtins) and HOME-relative (`~` expansion): a module
/// importing a sibling file wants paths to resolve against *its
/// own* directory, not whoever invoked it.
#[allow(clippy::disallowed_methods)]
pub fn resolve_relative_to_script(path: &str, script: &str) -> PathBuf {
    let input = PathBuf::from(path);
    if input.is_absolute() {
        return input;
    }
    if script.is_empty() || script.starts_with('<') {
        return input;
    }
    let base = Path::new(script).parent().unwrap_or_else(|| Path::new("."));
    base.join(input)
}

/// `path.parent()`, or the literal current directory (`.`) when
/// `path` has no parent (i.e. is a bare filename) or the parent is
/// the empty path.  The fallback exists so callers that immediately
/// pass the result to a `*_in(parent)` API (`tempfile::Builder::tempfile_in`,
/// `fs::File::open` of a directory for fsync, …) don't blow up on
/// bare filenames.
#[allow(clippy::disallowed_methods)]
pub fn parent_or_cwd(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// `Path::new(path).exists()` behind a named helper so call sites
/// that already hold the path as a string don't reach into the
/// stdlib path constructors directly.  Pure existence probe —
/// follows symlinks, does not canonicalise.
#[allow(clippy::disallowed_methods)]
pub fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// True iff `path` is an absolute path under the host's
/// platform rules.  Wraps `Path::new(path).is_absolute()` so
/// callers stop reaching for `Path::new` themselves just to ask
/// the question.
#[allow(clippy::disallowed_methods)]
pub fn is_absolute(path: &str) -> bool {
    Path::new(path).is_absolute()
}

/// Final path component of `path`, decoded as UTF-8 with a fallback
/// to the original string when the path has no file name or the
/// name is not valid UTF-8.  Used by callers that key on a command
/// basename (exit hint lookup, login-shell detection).
#[allow(clippy::disallowed_methods)]
pub fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// Proper ancestors of `paths`, dedup'd, root excluded.  For each
/// input path, walk `Path::ancestors()` upward stopping above `/` and
/// collect every intermediate directory.  Output is sorted (`BTreeSet`
/// iteration order) and free of duplicates across inputs.
///
/// Used by the macOS Seatbelt builder to emit `file-read-metadata`
/// allows on the parents of each grant prefix (Seatbelt checks
/// parent-directory metadata during path lookup).  Generic enough to
/// live next to the path lattice rather than alongside the SBPL
/// renderer.
#[allow(clippy::disallowed_methods)]
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
#[allow(clippy::disallowed_methods)]
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
        for s in [
            "/tmp/foo",
            "/private/tmp/foo",
            "/var/folders/xy",
            "/etc/passwd",
        ] {
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
        assert!(path_within(
            Path::new("/tmp/foo"),
            Path::new("/private/tmp")
        ));
        assert!(path_within(
            Path::new("/private/tmp/foo"),
            Path::new("/tmp")
        ));
    }
}
