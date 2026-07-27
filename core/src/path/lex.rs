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
//! canonicaliser see the same view.  [`starts_with_identity`] is the
//! per-pair comparison `path_within` folds over every alias pair,
//! carrying Windows path identity (case, separator, `\\?\`-verbatim).

use std::path::{Component, Path, PathBuf};

use super::process_cwd;

/// Return `p` together with any alternate lexical forms that the
/// host filesystem treats as identical (e.g. `/tmp/foo` ↔
/// `/private/tmp/foo` on macOS).
///
/// Pure: no filesystem access.
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

/// True iff some alias of `path` starts with some alias of `prefix`.
///
/// I.e. `path` lies inside `prefix` modulo the host's known firmlinks
/// and, under Windows path identity, modulo case, separator spelling,
/// and a `\\?\`-verbatim prefix — see [`starts_with_identity`]. Pure
/// helper used by both the runtime grant matcher and the nested grant
/// intersector.
pub fn path_within(path: &Path, prefix: &Path) -> bool {
    let ps = path_aliases(path);
    let qs = path_aliases(prefix);
    if cfg!(windows) {
        ps.iter().any(|p| {
            qs.iter()
                .any(|q| starts_with_identity(&p.to_string_lossy(), &q.to_string_lossy(), true))
        })
    } else {
        // Off Windows, compare the original `&Path`s directly rather than
        // through `to_string_lossy`: two distinct non-UTF-8 paths can both
        // lossy-decode to the same replacement-character string, which
        // would make the matcher treat them as identical.
        ps.iter().any(|p| qs.iter().any(|q| p.starts_with(q)))
    }
}

/// Component-wise prefix test under one of two path-identity rulesets.
///
/// Off Windows (`windows == false`): the existing byte-exact rule,
/// delegated to [`Path::starts_with`] (component-aware — `/tmp` does
/// not spuriously match `/tmpx`).
///
/// Under Windows path identity (`windows == true`): `/` and `\` are
/// the same separator, components compare case-insensitively (`git`
/// grants must admit `C:\WORK` a grant wrote as `c:\work`), and a
/// `\\?\`-verbatim prefix — what `std::fs::canonicalize` returns on
/// Windows — is equivalent to its non-verbatim spelling, so a grant
/// resolved through `canonicalise_lenient` still matches a candidate
/// that was never canonicalized. `\\?\UNC\server\share` likewise folds
/// to `\\server\share`.
///
/// Pure string logic rather than `std::path::Path` — whose separator
/// and prefix parsing is fixed at compile time to the build target,
/// not switchable at runtime — so the Windows rule is exercised by a
/// unit test on every host regardless of which platform is compiling.
/// `windows` is a parameter rather than a `cfg!(windows)` read buried
/// in this function so that test is possible; the real platform gate
/// lives at the one call site, [`path_within`]. Mirrors
/// `capability::exec::names_match` and
/// `path::sigil::{unix,windows}_tool_roots`, the same pattern applied
/// to command-name and tool-root comparisons.
#[allow(clippy::disallowed_methods)]
pub(crate) fn starts_with_identity(path: &str, prefix: &str, windows: bool) -> bool {
    if !windows {
        return Path::new(path).starts_with(Path::new(prefix));
    }
    let path = windows_identity_components(path);
    let prefix = windows_identity_components(prefix);
    path.len() >= prefix.len() && path[..prefix.len()] == prefix[..]
}

/// Split a path string into lower-cased components under Windows path
/// identity: a leading verbatim prefix (`\\?\`, or the all-forward-slash
/// spelling `//?/`) is stripped, a verbatim UNC form
/// `UNC\server\share` (either separator) right after it is folded to
/// `\server\share`, and `/` and `\` are both treated as separators for
/// everything else.
///
/// The verbatim-prefix strip recognises both separator spellings for
/// the same reason every other rule here does: this function's whole
/// premise is that `/` and `\` are interchangeable under Windows path
/// identity, so special-casing the verbatim-prefix token to only the
/// backslash spelling would carve out one silent exception to that
/// premise — a `//?/C:/work`-spelled deny prefix, for instance, would
/// silently fail to fold to the same components as a `\\?\C:\work`
/// grant, missing the match a deny needs to close.  (Real Windows
/// itself only recognises the backslash spelling as a genuine
/// verbatim escape at the `CreateFileW` boundary — this is purely an
/// internal-matcher normalisation, not a claim about OS behaviour.)
///
/// The case fold is ASCII-only (`to_ascii_lowercase`), not full
/// Unicode `to_lowercase` — matching
/// `capability::exec::names_match`'s fold, so path components and
/// command names apply one Windows case-insensitivity rule rather
/// than two different ones.  Neither is the real NTFS `$UpCase`
/// table (a pinned, Unicode-aware mapping baked into the driver);
/// ASCII-only is the conservative approximation, since it can never
/// claim a non-ASCII equivalence `$UpCase` wouldn't honour, at the
/// cost of missing non-ASCII folds `$UpCase` would make.
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

/// Strip a leading verbatim prefix — two separators, `?`, a separator
/// — under either slash spelling (`\\?\` or `//?/`, and the mixed
/// forms in between).  See [`windows_identity_components`] for why
/// both spellings are recognised here.
fn strip_verbatim_prefix(p: &str) -> &str {
    let b = p.as_bytes();
    let is_sep = |c: u8| c == b'/' || c == b'\\';
    if b.len() >= 4 && is_sep(b[0]) && is_sep(b[1]) && b[2] == b'?' && is_sep(b[3]) {
        &p[4..]
    } else {
        p
    }
}

/// True iff `path` would be absolute under Windows path rules: a
/// drive-letter prefix (`C:\`, `c:/`) or a UNC/verbatim form (`\\`,
/// `//`) followed by a root.  Pure string logic rather than
/// `std::path::Path` — whose absoluteness rule is fixed at compile
/// time to the build target — for the same reason
/// [`windows_identity_components`] is: it lets [`is_foreign_rooted`]
/// classify a path's Windows-absoluteness from any host.
fn is_windows_absolute(path: &str) -> bool {
    if path.starts_with(r"\\") || path.starts_with("//") {
        return true;
    }
    let b = path.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\')
}

/// True iff `path` has a root but is not absolute under Windows rules
/// — a Unix-absolute path (`/tmp`, `/usr/local/bin`) frozen on a build
/// where it has a root but no drive letter, so it resolves nowhere.
/// Both separator spellings are recognised: the freeze pass folds the
/// entry through `fold_dots` first, which re-renders the POSIX root as
/// a native `\` (`/tmp` → `\tmp`), so by the time this check runs the
/// leading slash may face either way.  A `\\`/`//` prefix is UNC and
/// therefore absolute, excluded by the `is_windows_absolute` arm.
/// Always `false` when `windows` is `false`: off Windows, "rooted" and
/// "absolute" coincide (`NormalizedPrefix::is_absolute` already covers
/// that case), so there is no foreign-rooted class to detect.
///
/// `windows` is a parameter rather than a `cfg!(windows)` read so this
/// classification has a fixed-outcome unit test on every host —
/// mirrors [`starts_with_identity`] and
/// `capability::exec::names_match`; the real platform gate lives at
/// the one call site, `capability::decode`'s absoluteness check.
pub(crate) fn is_foreign_rooted(path: &str, windows: bool) -> bool {
    windows && matches!(path.as_bytes().first(), Some(b'/' | b'\\')) && !is_windows_absolute(path)
}

/// Resolve `path` against `cwd`, normalising `.` and `..`
/// components.
///
/// If `path` is already absolute it is normalised in
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
/// the cwd: `CurDir` drops, `ParentDir` pops, every other component is
/// pushed.  A `..` that cannot pop is kept only on a *relative* path (so a
/// leading `..` survives); on a rooted path it is dropped, since `/` has no
/// parent (`/..` folds to `/`, `/a/../../x` to `/x`).  The shared kernel of
/// [`resolve_path`] (which joins a cwd first) and
/// [`super::canon::canonicalise_lenient`] (which folds cwd-free before its
/// existing-ancestor walk).
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

/// [`fold_dots`] in the *guest's* namespace: the same law, for a path
/// whose separator is `/` no matter which host is folding it.
///
/// [`fold_dots`] rebuilds its answer by pushing components into a
/// `PathBuf`, which is right for a host path — on Windows it also
/// normalises `C:/x` to `C:\x` — and wrong for a path that names
/// something inside the Linux guest.  There, `Component::RootDir` renders
/// as `\`, so `/work` comes back as `\work`: not a spelling variant but a
/// *relative* path in the namespace it claims to name, matching nothing
/// the engine inside the machine will ever resolve.  Hence a second
/// kernel rather than a flag on the first — the two answer to different
/// operating systems, and the caller always knows which one it means.
///
/// Every rule is [`fold_dots`]'s, including the one that reads like an
/// oversight: a `..` that cannot pop is dropped on a rooted path and kept
/// on a relative one, so a second leading `..` pops the first back off
/// (`../../a` folds to `a`).  Grant prefixes are absolute, so that corner
/// is unreachable from the only caller
/// ([`NormalizedPrefix::from_guest`](super::NormalizedPrefix::from_guest));
/// it is mirrored anyway, because two normalisers that agree except in
/// the dark are worse than one.
pub(crate) fn fold_dots_posix(path: &str) -> String {
    let rooted = path.starts_with('/');
    let mut folded: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            // `Path::components` yields neither empty components nor
            // `.`; splitting on the separator yields both, so this arm is
            // what makes the two iterations comparable.
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

/// Depth of `dir`, in path components, after folding it through the
/// same identity [`path_within`] matches with: a macOS firmlink alias
/// collapsed to its canonical (longer) spelling, then split under
/// Windows path identity (case/separator/`\\?\`-fold) when `windows`
/// is set, or by [`Path::components`] otherwise.
///
/// `capability::exec::longest_dir_match` ranks competing directory
/// prefixes by depth, and a character count is only a depth proxy
/// within one spelling of a path: `/tmp` and its firmlink alias
/// `/private/tmp` name the same directory at different lengths, and
/// `/tmp/a/b` (a real 3-level path) is shorter than `/private/tmp` (an
/// alias of a 1-level path) despite nesting deeper. Ranking on
/// characters ranks spelling, not depth, and lets a shallower alias or
/// a longer alias-prefix outrank the directory it is actually
/// shallower or deeper than. `windows` is a parameter, not a
/// `cfg!(windows)` read, for the same testability reason as
/// [`starts_with_identity`].
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

/// True if `script` names an actual compiled source — not the REPL,
/// not `-c`, not a synthetic `<...>` source (`<stdin>`, `<prelude>`).
///
/// The one script-identity rule that [`resolve_relative_to_script`] and
/// the elaborator's `$SCRIPT` bake both consult, instead of two drifting
/// enumerations.
pub fn has_script_identity(script: &str) -> bool {
    !script.is_empty() && !script.starts_with('<') && script != "-c"
}

/// Resolve `path` relative to the directory containing `script`.
///
/// If `path` is absolute it is returned unchanged.  If `script` has no
/// [script identity](has_script_identity) — empty, `-c`, or a synthetic
/// `<...>` source — the input is returned unchanged so the caller can
/// fall back to cwd-relative resolution.
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
    if !has_script_identity(script) {
        return input;
    }
    let base = Path::new(script).parent().unwrap_or_else(|| Path::new("."));
    base.join(input)
}

/// `path.parent()`, or the literal current directory (`.`) when
/// `path` has no parent (i.e. is a bare filename) or the parent is
/// the empty path.
///
/// The fallback exists so callers that immediately
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
/// stdlib path constructors directly.
///
/// Pure existence probe —
/// follows symlinks, does not canonicalise.
#[allow(clippy::disallowed_methods)]
pub fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// `Path::new(path).is_dir()` behind a named helper.
///
/// The companion of [`exists`] for call sites that must tell a
/// directory from a file — exec grants distinguish their two kinds of
/// path key by trailing slash, and check that spelling against what is
/// really on disk.
///
/// Follows symlinks; `false` for a path that does not exist.
#[allow(clippy::disallowed_methods)]
pub fn is_dir(path: &str) -> bool {
    Path::new(path).is_dir()
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
/// name is not valid UTF-8.
///
/// Used by callers that key on a command
/// basename (exit hint lookup, login-shell detection).
#[allow(clippy::disallowed_methods)]
pub fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// Proper ancestors of `paths`, dedup'd, root excluded.
///
/// For each
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

    // Rooted-path shapes: `/` has no parent, so a `..` that reaches the
    // root is dropped rather than preserved.  Unix-only: `/`-rooted
    // inputs are the shape the grant pipeline folds.
    #[cfg(unix)]
    #[test]
    fn fold_dots_drops_dotdot_at_root() {
        assert_eq!(fold_dots(Path::new("/..")), pb("/"));
        assert_eq!(fold_dots(Path::new("/a/../../x")), pb("/x"));
        assert_eq!(fold_dots(Path::new("/../..")), pb("/"));
    }

    // A relative path keeps a `..` it cannot pop, so upward references
    // survive for a later cwd join.
    #[test]
    fn fold_dots_keeps_leading_dotdot_on_relative_path() {
        assert_eq!(fold_dots(Path::new("../x")), pb("../x"));
        assert_eq!(fold_dots(Path::new("a/../../x")), pb("../x"));
    }

    // The guest kernel's whole reason to exist, pinned on every host
    // including the one it was written for: a guest path folds to a guest
    // path, separator intact.  On Windows `fold_dots` answers `\work` here
    // — see `fold_dots_posix`'s own docs — which is why synod's prefixes
    // do not go through it.
    #[test]
    fn fold_dots_posix_keeps_the_guests_separator() {
        assert_eq!(fold_dots_posix("/work"), "/work");
        assert_eq!(
            fold_dots_posix("/work/./drafts/../letters"),
            "/work/letters"
        );
        assert_eq!(fold_dots_posix("/work/"), "/work");
        assert_eq!(fold_dots_posix("/"), "/");
        // Rooted `..` at the root is dropped, as in `fold_dots`.
        assert_eq!(fold_dots_posix("/.."), "/");
        assert_eq!(fold_dots_posix("/a/../../x"), "/x");
        // And a relative `..` survives for a later join, likewise.
        assert_eq!(fold_dots_posix("../x"), "../x");
        assert_eq!(fold_dots_posix("a/../../x"), "../x");
    }

    // Where the host *is* the guest's namespace, the two kernels must be
    // one function.  This is the check that keeps `fold_dots_posix` from
    // drifting into a second, subtly different law: any divergence in the
    // shared table shows up here, on the platform that can see both.
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

    // Fixed-outcome on every host: `windows` is a parameter, so the
    // full classification table is pinned without a Windows CI leg.
    #[test]
    fn foreign_rooted_classification() {
        // Driveless-rooted, either separator: the freeze pass folds
        // `/tmp` to `\tmp` on Windows before this check runs.
        assert!(is_foreign_rooted("/tmp", true));
        assert!(is_foreign_rooted(r"\tmp", true));
        assert!(is_foreign_rooted("/usr/local/bin", true));
        // Windows-absolute forms are not foreign: drive letter, UNC.
        assert!(!is_foreign_rooted(r"C:\work", true));
        assert!(!is_foreign_rooted("c:/work", true));
        assert!(!is_foreign_rooted(r"\\server\share", true));
        assert!(!is_foreign_rooted("//server/share", true));
        // Genuinely relative paths stay in the strict-error class.
        assert!(!is_foreign_rooted("proj", true));
        assert!(!is_foreign_rooted("./a", true));
        // Off Windows there is no foreign-rooted class at all.
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

    /// Security regression: off Windows, `path_within` must compare the
    /// original `&Path`s, not their `to_string_lossy` forms — two
    /// distinct non-UTF-8 byte sequences can lossy-decode to the same
    /// U+FFFD-substituted string, which would make an unrelated path
    /// falsely match a grant prefix.
    #[cfg(unix)]
    #[test]
    fn path_within_does_not_collide_distinct_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // `/opt` (unlike `/tmp`, `/var`, `/etc`) is not a macOS
        // firmlink source, so `path_aliases` doesn't route these
        // through `firmlink_toggle`'s own `to_string_lossy` call — this
        // test isolates `path_within`'s comparison, not the aliasing.
        let prefix_bytes: &[u8] = b"/opt/\xFFsecret";
        let candidate_bytes: &[u8] = b"/opt/\xFEsecret";
        // Sanity: both single invalid bytes really do lossy-collide, or
        // this test proves nothing.
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

    // `starts_with_identity` is exercised directly with `windows: true`
    // rather than behind `cfg(windows)`, so the Windows path-identity
    // rule has a fixed-outcome unit test on every host — the pattern
    // `capability::exec::names_match` and `sigil::windows_tool_roots`
    // established.

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

    /// L1/L2 regression: the all-forward-slash spelling of the
    /// verbatim prefix (`//?/C:/work`) must fold identically to the
    /// backslash spelling (`\\?\C:\work`) — real Windows only accepts
    /// the backslash form at the `CreateFileW` boundary, but this
    /// matcher's whole premise is that `/` and `\` are interchangeable,
    /// so a deny authored in one spelling must still catch an access
    /// spelled the other way.  Without this, a `//?/`-spelled access
    /// path would bypass a `\\?\`- or plain-spelled deny.
    #[test]
    fn windows_identity_strips_forward_slash_verbatim_prefix() {
        assert!(starts_with_identity(r"//?/C:/work/sub", r"C:\work", true));
        assert!(starts_with_identity(r"C:\work\sub", "//?/c:/work", true));
        assert!(starts_with_identity(
            r"//?/C:/work/sub",
            r"\\?\c:\WORK",
            true
        ));
        // Mixed separators within the same verbatim spelling.
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
        // `C:\work` must not pseudo-match `C:\workshop`, mirroring the
        // existing substring-pseudomatch guard off Windows.
        assert!(!starts_with_identity(r"C:\workshop", r"C:\work", true));
    }

    #[test]
    fn windows_identity_rejects_unrelated_drive() {
        assert!(!starts_with_identity(r"D:\work\sub", r"C:\work", true));
    }

    #[test]
    fn windows_identity_off_flag_is_byte_exact() {
        // With `windows: false` the rule is the pre-existing byte-exact
        // one, regardless of build target — same contract as
        // `capability::exec::names_match`'s `windows: false` arm.
        assert!(!starts_with_identity(r"C:\WORK", r"C:\work", false));
    }

    /// `identity_depth` folds Windows path identity — case, separator,
    /// and a `\\?\`-verbatim prefix — before counting components, so
    /// three spellings of the same 2-level directory report the same
    /// depth. Exercised with `windows: true` directly, like
    /// `starts_with_identity`'s own tests, so this runs on every host.
    #[test]
    fn identity_depth_windows_folds_case_separator_and_verbatim() {
        assert_eq!(identity_depth(r"C:\work\sub", true), 3);
        assert_eq!(identity_depth(r"c:/WORK/SUB", true), 3);
        assert_eq!(identity_depth(r"\\?\C:\work\sub", true), 3);
    }
}
