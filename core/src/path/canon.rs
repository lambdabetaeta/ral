//! Filesystem canonicalisation, named by intent.
//!
//! Two canonicalisers, distinguished by what they do when the
//! input path does not exist:
//!
//!   * [`canonicalise_strict`] — `realpath(3)` direct.  Errors if
//!     the path or any intermediate component is missing.  Use
//!     when the caller really needs the file to be there: module
//!     loaders keying caches by realpath, plugin discovery,
//!     user-facing `resolve` builtins.
//!
//!   * [`canonicalise_lenient`] — walks up to the nearest existing
//!     ancestor, canonicalises that, then re-appends the unresolved
//!     tail.  Infallible: returns the input as-is when no ancestor
//!     exists.  Use for grant prefixes (a write may target a path
//!     that does not yet exist) and for the cwd/tmp injection that
//!     `runtime_fs_policy` performs.
//!
//! Plus one path-equivalence helper:
//!
//!   * [`match_variants`] — every string form by which a kernel
//!     sandbox MAC hook might present the same VFS object.  Combines
//!     the lenient canonical form with macOS firmlink toggling
//!     (`/var` ↔ `/private/var`, `/tmp` ↔ `/private/tmp`, `/etc`
//!     ↔ `/private/etc`), since whether Seatbelt sees the firmlinked
//!     or canonical form at a given check varies by syscall and we
//!     can't reliably predict which.
//!
//! Every other module that needs canonicalisation goes through
//! one of these so the choice is visible at the call site.

use std::path::{Path, PathBuf};

/// macOS firmlinks bridge the read-only system volume to the data
/// volume at these well-known mount points.  Each pair is
/// `(firm, canon)` — the firm-side mountpoint and its canonical
/// `/private/...` form.  The kernel may surface either form to a MAC
/// hook (Seatbelt, sandbox) depending on which API the caller used,
/// and `realpath(3)` itself can fail on `/tmp` under Seatbelt, so
/// both the canonicaliser ([`firmlink_toggle`]) and the lexical alias
/// generator (see [`super::lex::path_aliases`]) need this list.
/// Empty on non-macOS — firmlinks are an APFS feature.
#[cfg(target_os = "macos")]
pub(crate) const FIRMLINKS: &[(&str, &str)] = &[
    ("/var", "/private/var"),
    ("/tmp", "/private/tmp"),
    ("/etc", "/private/etc"),
];
#[cfg(not(target_os = "macos"))]
pub(crate) const FIRMLINKS: &[(&str, &str)] = &[];

/// Strict realpath: errors when the file or an intermediate
/// directory is missing.  One-line wrapper over `fs::canonicalize`,
/// existing so call sites name their intent — and so the
/// workspace-wide `disallowed_methods` lint can keep
/// `std::fs::canonicalize` itself caged inside this file.
///
/// Crate-internal: the public canonicalisation surface is
/// [`ResolvedPath::canonicalise_strict`](super::ResolvedPath::canonicalise_strict),
/// so no caller outside `path` can `realpath` a path that was not first
/// resolved through [`Resolver::resolve`](super::Resolver::resolve).
#[allow(clippy::disallowed_methods)]
pub(crate) fn canonicalise_strict(p: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p)
}

/// Lenient canonicalisation: resolves the longest existing prefix
/// of `p` and re-appends the unresolved tail.  Always returns
/// something — falls back to the lexically normalised `p` when no
/// ancestor exists.  The input is `.`/`..`-folded cwd-free first, so a
/// `..` in the unresolved tail cannot smuggle past the existing-ancestor
/// walk (`/a/x/../y` resolves to `canon(/a)/y`, never `canon(/a)/x/y`).
///
/// Needed for grant prefixes that may name not-yet-created
/// targets (e.g. a `fs.write` grant against a build output path),
/// and so that a grant authored as `/tmp/foo` still matches when
/// `/tmp` is a symlink and the access path resolves through the
/// symlink to `/private/tmp/foo`.
///
/// Crate-internal: the public surface is
/// [`ResolvedPath::canonicalise_lenient`](super::ResolvedPath::canonicalise_lenient).
/// The `fold_dots` first-step stays as documented defence for the
/// remaining internal callers that still pass a bare `Path`
/// ([`match_variants`]).
#[allow(clippy::disallowed_methods)]
pub(crate) fn canonicalise_lenient(p: &Path) -> PathBuf {
    let folded = super::lex::fold_dots(p);
    let folded = if folded.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        folded
    };
    let p = folded.as_path();
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let mut trail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = p;
    loop {
        if let Ok(c) = std::fs::canonicalize(cursor) {
            let mut resolved = c;
            for seg in trail.iter().rev() {
                resolved.push(seg);
            }
            return resolved;
        }
        match cursor.parent() {
            Some(parent) => {
                if let Some(name) = cursor.file_name() {
                    trail.push(name.to_os_string());
                }
                if parent.as_os_str().is_empty() {
                    return p.to_path_buf();
                }
                cursor = parent;
            }
            None => return p.to_path_buf(),
        }
    }
}

/// Every path string by which a kernel sandbox MAC hook might
/// present the same VFS object as `p`.
///
/// Always includes `p` itself; on macOS also includes the lenient
/// canonical form and any firmlink-toggled variant.  Unsorted and
/// possibly repeating — the sole caller [`match_variants_list`] owns
/// ordering and uniqueness.
///
/// The motivation is empirical: a Seatbelt rule written
/// `(subpath "/private/var/select")` does not match an `lstat` of
/// `/var/select/developer_dir` on every macOS version, but the
/// twin rule `(subpath "/var/select")` does.  Other syscalls
/// behave the inverse way.  Granting both forms removes the
/// guessing without enlarging the trust surface — both names
/// already point to the same inode.
pub(crate) fn match_variants(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![p.to_path_buf(), canonicalise_lenient(p)];
    let toggles: Vec<PathBuf> = out.iter().filter_map(|q| firmlink_toggle(q)).collect();
    out.extend(toggles);
    out
}

/// List-shaped [`match_variants`]: expand every entry to its firmlink
/// equivalents, flatten, dedupe, and render to strings for the
/// Seatbelt profile text.
///
/// A grant for `/tmp/work` produces
/// `[/tmp/work, /private/tmp/work]`, since Seatbelt may present either
/// form to the MAC hook depending on the syscall.  Used by the macOS
/// sandbox profile builder when laying out subpath rules.  Accepts the
/// grant-side [`NormalizedPrefix`](super::NormalizedPrefix)es and the
/// renderer's bare strings alike — both are backed by `String`, so
/// always valid UTF-8 on the way in.
///
/// The `Err` case handles the way out: [`canonicalise_lenient`]'s
/// `realpath(3)` call can surface a symlink target or mount whose name
/// is not valid Unicode, even though every input string was.  A
/// Seatbelt rule is a string literal, so a variant that is not valid
/// UTF-8 cannot be rendered without changing which inode it names —
/// silently substituting a lossy string would grant or deny the wrong
/// path.  Fail-closed here matches the sandbox's posture everywhere
/// else: a grant the renderer cannot express faithfully is refused,
/// not approximated.  See [`match_variants_paths`] for the byte-level
/// dedup and the point the error is raised.
///
/// # Errors
///
/// Returns `Err` when a firmlink/canonical expansion of one of the
/// inputs is not valid UTF-8 (see above) rather than rendering a lossy
/// approximation.
#[allow(clippy::disallowed_methods)]
pub fn match_variants_list<S: AsRef<str>>(paths: &[S]) -> Result<Vec<String>, String> {
    match_variants_paths(paths.iter().map(|p| Path::new(p.as_ref())))
}

/// Path-level engine behind [`match_variants_list`]: dedupes on the
/// exact path bytes (`BTreeSet<PathBuf>`, never a `to_string_lossy`
/// rendering — two distinct non-UTF-8 paths can lossy-decode to the
/// same U+FFFD-substituted string, which would misattribute one
/// path's grant to the other), then commits each surviving variant to
/// a string once, failing closed on the first one that is not valid
/// UTF-8.
///
/// Factored out from [`match_variants_list`] so this logic can be
/// exercised directly with genuine non-UTF-8 [`Path`]s in tests —
/// [`match_variants_list`]'s `S: AsRef<str>` bound means it can never
/// be handed one.
fn match_variants_paths<'a>(paths: impl Iterator<Item = &'a Path>) -> Result<Vec<String>, String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for p in paths {
        for v in match_variants(p) {
            if seen.insert(v.clone()) {
                // `{v:?}` deliberately, not `.display()`: the error exists
                // because `v` is not valid UTF-8, and `Display` would
                // paper over that with the very U+FFFD lossy substitution
                // this function refuses to render into a Seatbelt rule.
                #[allow(clippy::unnecessary_debug_formatting)]
                let s = v.to_str().ok_or_else(|| {
                    format!(
                        "ral: sandbox grant path is not valid UTF-8, refusing to render \
                         it into a Seatbelt rule that could name the wrong path: {v:?}"
                    )
                })?;
                out.push(s.to_string());
            }
        }
    }
    Ok(out)
}

/// If `p` begins with one of the macOS firmlink roots (in either
/// direction), return the toggled variant; otherwise `None`.
/// Pure byte operation — no filesystem access, no UTF-8 round-trip —
/// so it works on non-existent paths, inside a sandbox where
/// canonicalise calls would fail, and on a path whose tail is not
/// valid UTF-8.  Firmlink roots are ASCII, so splicing at their
/// boundary never lands inside a multi-byte sequence, and every tail
/// byte is copied through unchanged — on every target, not just Unix,
/// since [`FIRMLINKS`] is compiled (empty off macOS) everywhere and a
/// Windows build must still typecheck this function.
#[allow(clippy::disallowed_methods)]
pub(crate) fn firmlink_toggle(p: &Path) -> Option<PathBuf> {
    let bytes = p.as_os_str().as_encoded_bytes();
    for (firm, canon) in FIRMLINKS {
        for (from, to) in [
            (canon.as_bytes(), firm.as_bytes()),
            (firm.as_bytes(), canon.as_bytes()),
        ] {
            if let Some(rest) = bytes.strip_prefix(from)
                && (rest.is_empty() || rest[0] == b'/')
            {
                let mut out = Vec::with_capacity(to.len() + rest.len());
                out.extend_from_slice(to);
                out.extend_from_slice(rest);
                // Safety: `out` is an ASCII firmlink-root literal
                // concatenated with `rest`, an unmodified suffix of
                // `p`'s own encoded bytes.  Splicing at an ASCII
                // boundary can't land inside a multi-byte UTF-8/WTF-8
                // sequence, so `out` is exactly as validly encoded as
                // `p.as_os_str()` was.
                let os = unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&out) };
                return Some(PathBuf::from(os));
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn strict_errors_on_missing_path() {
        let r = canonicalise_strict(Path::new("/this/should/not/exist/anywhere"));
        assert!(r.is_err());
    }

    #[test]
    fn lenient_falls_back_to_input_for_missing_path() {
        let p = Path::new("/this/should/not/exist/anywhere/either");
        let out = canonicalise_lenient(p);
        // No ancestor exists below /, so we expect the input back.
        // (On platforms where / canonicalises non-trivially, the
        // tail is re-appended; the suffix is still "either".)
        assert!(out.ends_with("either"), "got {out:?}");
    }

    #[test]
    fn lenient_resolves_existing_ancestor_and_reattaches_tail() {
        // /tmp exists; /tmp/<random>/foo does not.  Lenient should
        // canonicalise /tmp (which on macOS firmlinks to
        // /private/tmp) and re-append the tail.
        let suffix = "ral-canon-lenient-probe/foo";
        let probe = Path::new("/tmp").join(suffix);
        let out = canonicalise_lenient(&probe);
        assert!(out.ends_with(suffix), "got {out:?}");
    }

    #[test]
    fn match_variants_always_includes_input() {
        let v = match_variants(Path::new("/some/non/existent/path"));
        assert!(v.iter().any(|p| p == Path::new("/some/non/existent/path")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn match_variants_toggles_macos_firmlinks() {
        // Bare firmlink root.
        let v = match_variants(Path::new("/var/select"));
        assert!(v.iter().any(|p| p == Path::new("/var/select")));
        assert!(v.iter().any(|p| p == Path::new("/private/var/select")));

        // Canonical form: should toggle back to the firmlinked form.
        let v = match_variants(Path::new("/private/var/select"));
        assert!(v.iter().any(|p| p == Path::new("/var/select")));
        assert!(v.iter().any(|p| p == Path::new("/private/var/select")));

        // Subpath under a firmlink root.
        let v = match_variants(Path::new("/private/var/folders/X/T"));
        assert!(v.iter().any(|p| p == Path::new("/var/folders/X/T")));
    }

    // Windows reinterprets `/Users/...` as relative to the current drive,
    // and `C:\Users` exists on the test host, so `canonicalise_lenient`
    // walks up to it and returns a `\\?\C:\Users\nobody\projects\foo`
    // canonical form — defeating the "unrelated path passes through" check.
    // The fall-back behaviour is already covered cross-platform by
    // `lenient_falls_back_to_input_for_missing_path` above.
    #[cfg(unix)]
    #[test]
    fn match_variants_passes_unrelated_paths_through() {
        // Path that doesn't touch any firmlink root and has no
        // resolvable symlinks: every variant is the input itself (the
        // lenient canonical form of a non-existent path is the input).
        let v = match_variants(Path::new("/Users/nobody/projects/foo"));
        assert!(
            v.iter()
                .all(|p| p == Path::new("/Users/nobody/projects/foo"))
        );
    }

    /// Security regression: `firmlink_toggle` must preserve a non-UTF-8
    /// tail byte-for-byte.  The old `to_string_lossy`-based
    /// implementation replaced it with U+FFFD, so the returned "twin"
    /// no longer named the real inode — an under-grant on the twin
    /// side, and a collision hazard if two distinct non-UTF-8 paths
    /// happened to lossy-decode to the same substituted string.
    #[cfg(target_os = "macos")]
    #[test]
    fn firmlink_toggle_preserves_non_utf8_tail_bytes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let input = PathBuf::from(OsStr::from_bytes(b"/tmp/\xFFghost"));
        let toggled = firmlink_toggle(&input).expect("/tmp is a firmlink root");
        assert_eq!(
            toggled.as_os_str().as_bytes(),
            b"/private/tmp/\xFFghost",
            "got {toggled:?}"
        );

        // And the reverse direction.
        let canon = PathBuf::from(OsStr::from_bytes(b"/private/tmp/\xFFghost"));
        let back = firmlink_toggle(&canon).expect("/private/tmp is a firmlink root");
        assert_eq!(
            back.as_os_str().as_bytes(),
            b"/tmp/\xFFghost",
            "got {back:?}"
        );
    }

    /// Security regression: two distinct non-UTF-8 paths must not
    /// collapse into one `match_variants_paths` entry.  The old
    /// `BTreeSet<String>` dedup keyed on `to_string_lossy`, under which
    /// both paths below decode to the same U+FFFD-substituted string —
    /// collapsing them would attribute one path's grant to the other.
    #[cfg(unix)]
    #[test]
    fn match_variants_paths_does_not_collide_distinct_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let a: &[u8] = b"/opt/\xFFsecret";
        let b: &[u8] = b"/opt/\xFEsecret";
        // Sanity: both really do lossy-collide, or this test proves
        // nothing.
        assert_eq!(
            Path::new(OsStr::from_bytes(a)).to_string_lossy(),
            Path::new(OsStr::from_bytes(b)).to_string_lossy(),
        );

        let pa = PathBuf::from(OsStr::from_bytes(a));
        let pb = PathBuf::from(OsStr::from_bytes(b));
        let err = match_variants_paths([pa.as_path(), pb.as_path()].into_iter())
            .expect_err("non-UTF-8 paths must fail closed, not render lossily");
        // Fail-closed, not silent collision: an error naming the
        // problem, not a one-entry `Vec` that quietly merged the two.
        assert!(err.contains("not valid UTF-8"), "got {err:?}");
    }

    /// The step-4 boundary decision: a grant path that cannot be
    /// rendered into a Seatbelt string literal without changing which
    /// inode it names must fail closed (an `Err`), never fall back to
    /// a lossy `(subpath …)` rule that would grant or deny the wrong
    /// path.  This path never touches a firmlink root, isolating the
    /// boundary behaviour from the toggle fix above.
    #[cfg(unix)]
    #[test]
    fn match_variants_paths_fails_closed_on_non_utf8_variant() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let p = PathBuf::from(OsStr::from_bytes(
            b"/this/should/not/exist/anywhere/\xFFghost",
        ));
        let err = match_variants_paths([p.as_path()].into_iter())
            .expect_err("non-UTF-8 grant path must be refused, not lossily rendered");
        assert!(err.contains("not valid UTF-8"), "got {err:?}");
    }

    /// A wholly ASCII path list still round-trips as `Ok`, so the
    /// fail-closed path doesn't fire on the common case.
    #[test]
    fn match_variants_list_ok_on_ascii_paths() {
        let v = match_variants_list(&["/some/non/existent/path"]).expect("ASCII path must be Ok");
        assert!(v.iter().any(|p| p == "/some/non/existent/path"));
    }
}
