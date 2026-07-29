//! Lexical path resolution: a sigil-expanded string becomes an absolute,
//! `.`/`..`-free path against a scoped cwd, with no filesystem access.
//!
//! The alias-aware containment the grant matcher folds over the result lives
//! here too.  The firmlink table both sides share lives in [`super::canon`],
//! so matcher and canonicaliser can never see different aliases.

use std::path::{Component, Path, PathBuf};

use super::process_cwd;

/// `p` plus any alternate spelling the host treats as the same file
/// (`/tmp/foo` ↔ `/private/tmp/foo` on macOS; just `[p]` elsewhere).
///
/// The matcher needs this because `canonicalize` cannot always bridge the
/// two forms — under Seatbelt `realpath(3)` can fail on `/tmp` itself — so a
/// grant authored in one spelling still covers an access in the other.
pub fn path_aliases(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![p.to_path_buf()];
    out.extend(super::canon::firmlink_toggle(p));
    out
}

/// True iff some alias of `path` starts with some alias of `prefix`: `path`
/// lies inside `prefix` modulo firmlinks and, on Windows, modulo the path
/// identity [`starts_with_identity`] applies.
///
/// The runtime grant gate (`capability::enforce`) and the prefix intersector
/// (`super::prefix_set`) both decide containment through it.
pub fn path_within(path: &Path, prefix: &Path) -> bool {
    let ps = path_aliases(path);
    let qs = path_aliases(prefix);
    if cfg!(windows) {
        ps.iter().any(|p| {
            qs.iter()
                .any(|q| starts_with_identity(&p.to_string_lossy(), &q.to_string_lossy(), true))
        })
    } else {
        // Compare the `&Path`s, not their `to_string_lossy` forms: two
        // distinct non-UTF-8 paths can decode to the same
        // replacement-character string, and the matcher would call them one.
        ps.iter().any(|p| qs.iter().any(|q| p.starts_with(q)))
    }
}

/// Component-wise prefix test: byte-exact off Windows (via
/// [`Path::starts_with`], so `/tmp` never matches `/tmpx`), and under Windows
/// path identity when `windows` is set — `/` ≡ `\`, case-insensitive
/// components, a `\\?\`-verbatim prefix equivalent to its plain spelling, so
/// a grant that went through `canonicalize` still matches a candidate that
/// did not.
///
/// String logic rather than `std::path::Path`, whose separator and prefix
/// parsing are fixed at compile time to the build target, and `windows` is a
/// parameter rather than a `cfg!` read — together they let the Windows rule
/// be unit-tested on every host.  The platform gate sits at the sole call
/// site, [`path_within`].
#[allow(clippy::disallowed_methods)]
pub(crate) fn starts_with_identity(path: &str, prefix: &str, windows: bool) -> bool {
    if !windows {
        return Path::new(path).starts_with(Path::new(prefix));
    }
    let path = windows_identity_components(path);
    let prefix = windows_identity_components(prefix);
    path.len() >= prefix.len() && path[..prefix.len()] == prefix[..]
}

/// A path string as lower-cased components under Windows path identity: a
/// verbatim prefix stripped, a verbatim UNC head folded to `\server\share`,
/// `/` and `\` alike as separators.
///
/// The verbatim prefix is recognised in either slash spelling, though real
/// Windows honours only `\\?\` at the `CreateFileW` boundary: this is an
/// internal normalisation, and folding `//?/C:/work` differently from
/// `\\?\C:\work` would leave a deny that a differently-spelled access slips
/// past.  The case fold is ASCII-only, matching
/// `capability::exec::names_match` so paths and command names fold alike, and
/// erring below the real NTFS `$UpCase` table rather than above it — missing
/// non-ASCII folds sooner than claiming equivalences the driver would refuse.
fn windows_identity_components(p: &str) -> Vec<String> {
    let s = strip_verbatim_prefix(p);
    let s = s
        .strip_prefix("UNC\\")
        .or_else(|| s.strip_prefix("UNC/"))
        .map_or_else(|| s.to_string(), |rest| format!(r"\{rest}"));
    s.split(['/', '\\'])
        .filter(|c| !c.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Strip a leading verbatim prefix — two separators, `?`, a separator — under
/// either slash spelling and the mixed forms between.
fn strip_verbatim_prefix(p: &str) -> &str {
    let b = p.as_bytes();
    let is_sep = |c: u8| c == b'/' || c == b'\\';
    if b.len() >= 4 && is_sep(b[0]) && is_sep(b[1]) && b[2] == b'?' && is_sep(b[3]) {
        &p[4..]
    } else {
        p
    }
}

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

/// True iff `path` is rooted but not Windows-absolute — a Unix-absolute grant
/// (`/tmp`, `/usr/local/bin`) frozen on a Windows build, where it resolves
/// nowhere.  Either leading separator counts: the freeze pass runs
/// [`fold_dots`] first, which re-renders the POSIX root as a native `\`.
/// Always `false` off Windows, where rooted and absolute coincide.
///
/// `windows` is a parameter rather than a `cfg!` read, as in
/// [`starts_with_identity`], so the table is pinned on every host; the gate is
/// `capability::decode`'s `freeze_absolute`, which drops this class as a dead
/// grant instead of erroring on it as it does on a genuinely relative entry.
pub(crate) fn is_foreign_rooted(path: &str, windows: bool) -> bool {
    windows && matches!(path.as_bytes().first(), Some(b'/' | b'\\')) && !is_windows_absolute(path)
}

/// Resolve `path` against `cwd`, or against the process cwd when `cwd` is
/// `None`, folding `.` and `..`.  Purely lexical — no symlink resolution —
/// so the answer can differ from `canonicalize`.
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
/// [`NormalizedPrefix::from_guest`](super::NormalizedPrefix::from_guest)
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
#[allow(clippy::disallowed_methods)]
pub fn resolve_str(cwd: Option<&str>, path: &str) -> PathBuf {
    resolve_path(cwd.map(Path::new), path)
}

/// [`path_within`] on strings.
#[allow(clippy::disallowed_methods)]
pub fn path_within_str(path: &str, prefix: &str) -> bool {
    path_within(Path::new(path), Path::new(prefix))
}

/// Depth of `dir` in components, folded through the same identity
/// [`path_within`] matches with: a firmlink alias collapsed to its canonical
/// (longer) spelling, then split under Windows path identity when `windows`
/// is set, by [`Path::components`] otherwise.
///
/// `capability::exec::longest_dir_match` ranks competing directory prefixes
/// by depth, and a character count is a depth proxy only within one spelling:
/// `/tmp/a/b` nests deeper than `/private/tmp` yet is shorter, so counting
/// characters ranks spelling and lets a shallow alias outrank the directory
/// it sits above.
pub(crate) fn identity_depth(dir: &str, windows: bool) -> usize {
    let original = PathBuf::from(dir);
    let canonical = match super::canon::firmlink_toggle(&original) {
        Some(alt) if alt.as_os_str().len() > original.as_os_str().len() => alt,
        _ => original,
    };
    if windows {
        windows_identity_components(&canonical.to_string_lossy()).len()
    } else {
        canonical.components().count()
    }
}

/// True if `script` names an actual compiled source — not the REPL, not
/// `-c`, not a synthetic `<...>` source.
///
/// The one rule [`resolve_relative_to_script`] and the elaborator's
/// `$SCRIPT` bake share, rather than two enumerations free to drift.
pub fn has_script_identity(script: &str) -> bool {
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
#[allow(clippy::disallowed_methods)]
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
#[allow(clippy::disallowed_methods)]
pub fn parent_or_cwd(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// `Path::new(path).exists()` for call sites already holding a string.
/// Follows symlinks, canonicalises nothing.
#[allow(clippy::disallowed_methods)]
pub fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// `Path::new(path).is_dir()`, the companion of [`exists`] for callers that
/// must tell a directory from a file.
///
/// Exec grants spell their two kinds of path key apart by trailing slash,
/// and `capability::decode` checks that spelling against disk.  Follows
/// symlinks; `false` for a missing path.
#[allow(clippy::disallowed_methods)]
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
    reason = "[io-door:silent:mount-shape] sandbox mount probe: one `lstat` picking a mount kind for a denied path; a shape predicate, not model data I/O, raises no surface card."
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
#[allow(clippy::disallowed_methods)]
pub fn is_absolute(path: &str) -> bool {
    Path::new(path).is_absolute()
}

/// Final component of `path`, falling back to `path` itself when there is no
/// file name or it is not UTF-8.  For callers that key on a command basename
/// (exit hints, login-shell detection).
#[allow(clippy::disallowed_methods)]
pub fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// Proper ancestors of `paths`, sorted, dedup'd across inputs, root excluded.
///
/// The macOS Seatbelt builder emits `file-read-metadata` allows on them,
/// since Seatbelt checks parent-directory metadata during path lookup and a
/// grant on a deep prefix is unreachable without its chain.
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
        assert!(is_foreign_rooted("/tmp", true));
        assert!(is_foreign_rooted(r"\tmp", true));
        assert!(is_foreign_rooted("/usr/local/bin", true));
        assert!(!is_foreign_rooted(r"C:\work", true));
        assert!(!is_foreign_rooted("c:/work", true));
        assert!(!is_foreign_rooted(r"\\server\share", true));
        assert!(!is_foreign_rooted("//server/share", true));
        // Genuinely relative paths stay in the strict-error class.
        assert!(!is_foreign_rooted("proj", true));
        assert!(!is_foreign_rooted("./a", true));
        assert!(!is_foreign_rooted("/tmp", false));
        assert!(!is_foreign_rooted(r"\tmp", false));
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

    /// Security regression: two distinct non-UTF-8 byte sequences can decode
    /// to the same U+FFFD-substituted string, so comparing lossy forms would
    /// let an unrelated path match a grant prefix.
    #[cfg(unix)]
    #[test]
    fn path_within_does_not_collide_distinct_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // `/opt` (unlike `/tmp`, `/var`, `/etc`) is no firmlink source, so
        // these never reach `firmlink_toggle`: what is under test is
        // `path_within`'s comparison, not the aliasing.
        let prefix_bytes: &[u8] = b"/opt/\xFFsecret";
        let candidate_bytes: &[u8] = b"/opt/\xFEsecret";
        // Sanity: the two invalid bytes really do lossy-collide, or this
        // test proves nothing.
        assert_eq!(
            Path::new(OsStr::from_bytes(prefix_bytes)).to_string_lossy(),
            Path::new(OsStr::from_bytes(candidate_bytes)).to_string_lossy(),
        );

        let prefix = Path::new(OsStr::from_bytes(prefix_bytes));
        let candidate_path = PathBuf::from(OsStr::from_bytes(candidate_bytes)).join("file");
        assert!(
            !path_within(&candidate_path, prefix),
            "distinct non-UTF-8 paths must not collide via lossy string comparison"
        );
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

    // The Windows rules below pass `windows: true` directly rather than
    // hiding behind `cfg(windows)`, so they are pinned on every host.

    #[test]
    fn windows_identity_ignores_case() {
        assert!(starts_with_identity(r"C:\WORK\sub", r"c:\work", true));
        assert!(starts_with_identity(r"c:\Work\Sub", r"C:\WORK", true));
    }

    #[test]
    fn windows_identity_unifies_forward_and_back_slashes() {
        assert!(starts_with_identity(r"c:/work/sub", r"C:\work", true));
        assert!(starts_with_identity(r"C:\work\sub", "c:/work", true));
    }

    #[test]
    fn windows_identity_strips_verbatim_prefix() {
        assert!(starts_with_identity(r"\\?\C:\work\sub", r"C:\work", true));
        assert!(starts_with_identity(r"C:\work\sub", r"\\?\C:\work", true));
        assert!(starts_with_identity(
            r"\\?\C:\work\sub",
            r"\\?\c:\WORK",
            true
        ));
    }

    #[test]
    fn windows_identity_folds_verbatim_unc() {
        assert!(starts_with_identity(
            r"\\?\UNC\server\share\sub",
            r"\\server\share",
            true
        ));
    }

    /// Without this, a `//?/`-spelled access path bypasses a `\\?\`- or
    /// plain-spelled deny: real Windows accepts only the backslash form, but
    /// this matcher holds `/` and `\` interchangeable, so a deny authored in
    /// one spelling must catch an access spelled the other way.
    #[test]
    fn windows_identity_strips_forward_slash_verbatim_prefix() {
        assert!(starts_with_identity(r"//?/C:/work/sub", r"C:\work", true));
        assert!(starts_with_identity(r"C:\work\sub", "//?/c:/work", true));
        assert!(starts_with_identity(
            r"//?/C:/work/sub",
            r"\\?\c:\WORK",
            true
        ));
        assert!(starts_with_identity(r"//?\C:\work/sub", r"C:\work", true));
    }

    #[test]
    fn windows_identity_folds_forward_slash_verbatim_unc() {
        assert!(starts_with_identity(
            "//?/UNC/server/share/sub",
            r"\\server\share",
            true
        ));
    }

    #[test]
    fn windows_identity_respects_component_boundaries() {
        assert!(!starts_with_identity(r"C:\workshop", r"C:\work", true));
    }

    #[test]
    fn windows_identity_rejects_unrelated_drive() {
        assert!(!starts_with_identity(r"D:\work\sub", r"C:\work", true));
    }

    #[test]
    fn windows_identity_off_flag_is_byte_exact() {
        // Byte-exact regardless of build target, which is what makes the
        // flag, not the host, the thing under test.
        assert!(!starts_with_identity(r"C:\WORK", r"C:\work", false));
    }

    /// Three spellings of one directory must report one depth, or the
    /// deepest-prefix ranking picks by spelling.
    #[test]
    fn identity_depth_windows_folds_case_separator_and_verbatim() {
        assert_eq!(identity_depth(r"C:\work\sub", true), 3);
        assert_eq!(identity_depth(r"c:/WORK/SUB", true), 3);
        assert_eq!(identity_depth(r"\\?\C:\work\sub", true), 3);
    }
}
