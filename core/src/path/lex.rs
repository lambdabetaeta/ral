//! Lexical path resolution: a sigil-expanded string becomes an absolute,
//! `.`/`..`-free path against a scoped cwd, with no filesystem access.
//!
//! Containment and name identity live in [`super::identity`]; device names in
//! [`super::device`].

use std::path::{Component, Path, PathBuf};

use super::PathRules;
use crate::host;

/// True iff `path` is absolute under Windows rules: a drive-letter prefix or
/// a UNC/verbatim root.  String logic rather than `std::path::Path`, whose
/// absoluteness rule is fixed at compile time to the build target, so
/// [`is_foreign_rooted`] can ask the question from any host.
fn is_windows_absolute(path: &str) -> bool {
    if path.starts_with(r"\\") || path.starts_with("//") {
        return true;
    }
    let b = path.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\')
}

/// True iff `path` is rooted but not Windows-absolute: a Unix-absolute grant
/// (`/tmp`, `/usr/local/bin`) frozen on a Windows build, where it resolves
/// nowhere.  Either leading separator counts: the freeze pass runs
/// [`fold_dots`] first, which re-renders the POSIX root as a native `\`.
/// Always `false` under POSIX rules, where rooted and absolute coincide.
///
/// The gate is `guard::freeze::FreezeCtx::absolute`, which drops this
/// class as a dead grant instead of erroring on it as it does on a genuinely
/// relative entry.
pub(crate) fn is_foreign_rooted(path: &str, rules: PathRules) -> bool {
    rules == PathRules::Windows
        && matches!(path.as_bytes().first(), Some(b'/' | b'\\'))
        && !is_windows_absolute(path)
}

/// Resolve `path` against `cwd`, or against the process cwd when `cwd` is
/// `None`, folding `.` and `..`.  Purely lexical — no symlink resolution —
/// so the answer can differ from `canonicalize`.
pub fn resolve_path(cwd: Option<&Path>, path: &str) -> PathBuf {
    let input = PathBuf::from(path);
    let joined = if input.is_absolute() {
        input
    } else if let Some(cwd) = cwd {
        cwd.join(input)
    } else if let Some(cwd) = host::cwd() {
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

/// True iff `path` is the filesystem root and names no drive: `/` on either
/// host, and `\` as [`fold_dots`] re-renders it on Windows.
///
/// Windows has no one root.  `\` there is *drive-relative*, so resolving or
/// canonicalising it anchors it to whichever drive the process happens to be
/// running from — while a grant prefix naming the root means "everywhere",
/// the ceiling a policy attenuates down from.  The two readings part company
/// on exactly this path and nowhere else, so the doors that must not anchor
/// it ask here rather than matching a root apiece.
///
/// String logic, as [`is_windows_absolute`] and [`is_foreign_rooted`] are,
/// and exact rather than "all separators": `\\` opens a UNC name and `X:\`
/// carries a prefix component, and neither is universal.  A drive root must
/// keep anchoring as it always did, or a grant over one volume would widen
/// to all of them.
pub(crate) fn is_bare_root(path: &str) -> bool {
    matches!(path.as_bytes(), [b'/' | b'\\'])
}

/// Fold `.`/`..` lexically, touching neither filesystem nor cwd.  A `..` that
/// cannot pop survives only on a *relative* path; on a rooted one it is
/// dropped, since `/` has no parent (`/a/../../x` folds to `/x`).  The kernel
/// [`resolve_path`] and [`super::canon::canonicalise_lenient`] share.
pub(crate) fn fold_dots(path: &Path) -> PathBuf {
    let rooted = path.has_root();
    let mut normalized = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !rooted {
                    normalized.push(comp.as_os_str());
                }
            }
            _ => normalized.push(comp.as_os_str()),
        }
    }
    normalized
}

/// [`fold_dots`] in the *guest's* namespace: the same law, for a path whose
/// separator is `/` no matter which host is folding it.
///
/// [`fold_dots`] rebuilds through a `PathBuf`, so on Windows a root renders
/// as `\` and `/work` comes back as `\work` — not a spelling variant but a
/// *relative* path in the namespace it claims to name, matching nothing the
/// engine inside the machine will resolve.  Hence a second kernel rather than
/// a flag on the first.  Every rule is [`fold_dots`]'s, down to the `..` that
/// survives only on a relative path: unreachable from the absolute prefixes
/// the sole caller
/// [`FrozenPath::from_guest`](super::FrozenPath::from_guest)
/// hands it, mirrored anyway, since two normalisers that agree except in the
/// dark are worse than one.
pub(crate) fn fold_dots_posix(path: &str) -> String {
    let rooted = path.starts_with('/');
    let mut folded: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            // `Path::components` yields neither empties nor `.`; splitting
            // on the separator yields both, which this arm absorbs so the
            // two iterations stay comparable.
            "" | "." => {}
            ".." => {
                if folded.pop().is_none() && !rooted {
                    folded.push("..");
                }
            }
            other => folded.push(other),
        }
    }
    let joined = folded.join("/");
    if rooted { format!("/{joined}") } else { joined }
}

/// [`resolve_path`] with the cwd as a string, for cross-crate callers
/// (exarch's policy loading) that hold it that way.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: lifts the cwd string for `resolve_path`, the lexical resolver"
)]
pub fn resolve_str(cwd: Option<&str>, path: &str) -> PathBuf {
    resolve_path(cwd.map(Path::new), path)
}

/// True if `script` names an actual compiled source — not the REPL, not
/// `-c`, not a synthetic `<...>` source.
///
/// The one rule [`resolve_relative_to_script`] and the elaborator's
/// `$SCRIPT` bake share, rather than two enumerations free to drift.
pub(crate) fn has_script_identity(script: &str) -> bool {
    !script.is_empty() && !script.starts_with('<') && script != "-c"
}

/// Resolve `path` against the directory holding `script`.
///
/// This is the third anchor after cwd-relative and HOME-relative, so a
/// module importing a sibling file resolves against *its own* directory,
/// not its caller's.
///
/// Returned unchanged when `path` is absolute or `script` has no
/// [script identity](has_script_identity), leaving the caller its cwd
/// fallback.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: the script's directory, the third anchor, is read lexically off its name"
)]
pub fn resolve_relative_to_script(path: &str, script: &str) -> PathBuf {
    let input = PathBuf::from(path);
    if input.is_absolute() {
        return input;
    }
    if !has_script_identity(script) {
        return input;
    }
    let base = Path::new(script).parent().unwrap_or_else(|| Path::new("."));
    base.join(input)
}

/// `path.parent()`, or `.` when that is absent or empty.
///
/// Callers feeding the result to a `*_in(parent)` API
/// (`tempfile::Builder::tempfile_in`, opening the directory to fsync it)
/// therefore don't choke on a bare filename.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: `.` stands in lexically for an empty parent"
)]
pub(crate) fn parent_or_cwd(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// `Path::new(path).exists()` for call sites already holding a string.
/// Follows symlinks, canonicalises nothing.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: this is the `exists` helper the ban names"
)]
pub fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// `Path::new(path).is_dir()`, the companion of [`exists`] for callers that
/// must tell a directory from a file.
///
/// Exec grants spell their two kinds of path key apart by trailing slash,
/// and `guard::decode` checks that spelling against disk.  Follows
/// symlinks; `false` for a missing path.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: this is `is_dir`, the companion of the `exists` helper the ban names"
)]
pub fn is_dir(path: &str) -> bool {
    Path::new(path).is_dir()
}

/// What stands at a path, as the shapes a mount can be laid over.  A final
/// symlink is its own shape rather than followed: the kernel refuses to mount
/// over one at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathShape {
    Dir,
    /// A regular file, or any other non-directory inode — fifo, socket,
    /// device — since they share the mount rule.
    NonDir,
    Symlink,
    Absent,
}

/// The [`PathShape`] of `path`, from one `lstat`, so a caller choosing
/// between mount kinds cannot race itself between two predicates.  An
/// unstatable path reports [`PathShape::Absent`].
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:mount-shape] sandbox mount probe: one `lstat` picking a mount kind for a denied path; a shape predicate, not model data I/O, raises no surface card."
)]
pub fn shape(path: &str) -> PathShape {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => PathShape::Symlink,
        Ok(meta) if meta.is_dir() => PathShape::Dir,
        Ok(_) => PathShape::NonDir,
        Err(_) => PathShape::Absent,
    }
}

/// `Path::new(path).is_absolute()` — the *host's* rule, not the Windows one
/// `is_windows_absolute` applies.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: this is the `is_absolute` helper the ban names"
)]
pub fn is_absolute(path: &str) -> bool {
    Path::new(path).is_absolute()
}

/// Final component of `path`, falling back to `path` itself when there is no
/// file name or it is not UTF-8.  For callers that key on a command basename
/// (exit hints, login-shell detection).
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: this is the `basename` helper the ban names"
)]
pub fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// Proper ancestors of `paths`, sorted, dedup'd across inputs, root excluded.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: ancestors are a lexical walk up the name; nothing is resolved"
)]
pub(crate) fn proper_ancestors<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
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
#[allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures compare literal paths"
)]
mod tests {
    use super::*;

    const W: PathRules = PathRules::Windows;

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // `/` has no parent, so a `..` that reaches the root is dropped.
    #[cfg(unix)]
    #[test]
    fn fold_dots_drops_dotdot_at_root() {
        assert_eq!(fold_dots(Path::new("/..")), pb("/"));
        assert_eq!(fold_dots(Path::new("/a/../../x")), pb("/x"));
        assert_eq!(fold_dots(Path::new("/../..")), pb("/"));
    }

    // A `..` a relative path cannot pop survives for a later cwd join.
    #[test]
    fn fold_dots_keeps_leading_dotdot_on_relative_path() {
        assert_eq!(fold_dots(Path::new("../x")), pb("../x"));
        assert_eq!(fold_dots(Path::new("a/../../x")), pb("../x"));
    }

    // The guest kernel's reason to exist, pinned on every host: a guest path
    // folds to a guest path, separator intact, where `fold_dots` on Windows
    // would answer `\work`.
    #[test]
    fn fold_dots_posix_keeps_the_guests_separator() {
        assert_eq!(fold_dots_posix("/work"), "/work");
        assert_eq!(
            fold_dots_posix("/work/./drafts/../letters"),
            "/work/letters"
        );
        assert_eq!(fold_dots_posix("/work/"), "/work");
        assert_eq!(fold_dots_posix("/"), "/");
        assert_eq!(fold_dots_posix("/.."), "/");
        assert_eq!(fold_dots_posix("/a/../../x"), "/x");
        assert_eq!(fold_dots_posix("../x"), "../x");
        assert_eq!(fold_dots_posix("a/../../x"), "../x");
    }

    // Where the host *is* the guest's namespace the two kernels must be one
    // function: any drift in the shared law surfaces here, on the platform
    // that can see both.
    #[cfg(unix)]
    #[test]
    fn fold_dots_posix_agrees_with_fold_dots_where_the_host_is_posix() {
        for input in [
            "/work",
            "/work/",
            "/work/./drafts/../letters",
            "/",
            "/..",
            "/../..",
            "/a/../../x",
            "../x",
            "a/../../x",
            "../../a",
            "a/b/c",
            "",
        ] {
            assert_eq!(
                fold_dots_posix(input),
                fold_dots(Path::new(input)).to_string_lossy(),
                "the two kernels disagree on {input:?}"
            );
        }
    }

    // The whole table pinned on every host, no Windows CI leg needed.
    #[test]
    fn foreign_rooted_classification() {
        // Either separator: the freeze pass folds `/tmp` to `\tmp` on
        // Windows before this check runs.
        assert!(is_foreign_rooted("/tmp", W));
        assert!(is_foreign_rooted(r"\tmp", W));
        assert!(is_foreign_rooted("/usr/local/bin", W));
        assert!(!is_foreign_rooted(r"C:\work", W));
        assert!(!is_foreign_rooted("c:/work", W));
        assert!(!is_foreign_rooted(r"\\server\share", W));
        assert!(!is_foreign_rooted("//server/share", W));
        // Genuinely relative paths stay in the strict-error class.
        assert!(!is_foreign_rooted("proj", W));
        assert!(!is_foreign_rooted("./a", W));
        assert!(!is_foreign_rooted("/tmp", PathRules::Posix));
        assert!(!is_foreign_rooted(r"\tmp", PathRules::Posix));
    }
}
