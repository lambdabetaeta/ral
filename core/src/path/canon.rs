//! Stage 3 of path resolution: `realpath(3)`, in two flavours named by what
//! they do when the path does not exist.
//!
//! [`canonicalise_strict`] errors, [`canonicalise_lenient`] resolves the
//! longest existing ancestor and re-appends the tail.  Every module needing
//! canonicalisation comes through one of them, so the choice is visible at
//! the call site.  [`match_variants`] is the sandbox-side companion: the
//! several names one VFS object answers to.

use std::path::{Path, PathBuf};

/// macOS firmlinks bridging the read-only system volume to the data volume,
/// as `(firm, canon)` pairs.  A MAC hook may be handed either form depending
/// on which API the caller used, so a grant has to name both.  Empty off
/// macOS — firmlinks are an APFS feature.
#[cfg(target_os = "macos")]
pub(crate) const FIRMLINKS: &[(&str, &str)] = &[
    ("/var", "/private/var"),
    ("/tmp", "/private/tmp"),
    ("/etc", "/private/etc"),
];
#[cfg(not(target_os = "macos"))]
pub(crate) const FIRMLINKS: &[(&str, &str)] = &[];

/// `realpath(3)`: errors when the file or an intermediate directory is
/// missing.  Wraps `fs::canonicalize` so the workspace `disallowed_methods`
/// lint can cage that call inside this file; the door outside `path` is
/// [`ResolvedPath::canonicalise_strict`](super::ResolvedPath::canonicalise_strict),
/// so nothing can `realpath` a path that was not first resolved through
/// [`Resolver::resolve`](super::Resolver::resolve).
#[allow(clippy::disallowed_methods)]
pub(crate) fn canonicalise_strict(p: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p)
}

/// Resolves the longest existing prefix of `p` and re-appends the unresolved
/// tail, falling back to `p` itself when no ancestor resolves.  Grant prefixes
/// need this: a `fs.write` grant may name a build output that does not exist
/// yet, and a grant authored `/tmp/foo` must still match an access that
/// resolves through the symlink to `/private/tmp/foo`.
///
/// The `fold_dots` first step is load-bearing, not tidiness: it stops a `..`
/// in the unresolved tail from smuggling past the ancestor walk, so `/a/x/../y`
/// gives `canon(/a)/y` and never `canon(/a)/x/y`.  Callers arriving through
/// [`ResolvedPath::canonicalise_lenient`](super::ResolvedPath::canonicalise_lenient)
/// are already folded; [`match_variants`] hands over a bare `Path`.
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

/// Every path string by which a kernel sandbox MAC hook might present the same
/// VFS object as `p`: `p` itself, its lenient canonical form, and on macOS the
/// firmlink toggle of each.  Unsorted and possibly repeating — the sole caller,
/// [`match_variants_paths`], owns order and uniqueness.
///
/// The motivation is empirical.  A Seatbelt rule `(subpath "/private/var/select")`
/// does not match an `lstat` of `/var/select/developer_dir` on every macOS
/// version, but `(subpath "/var/select")` does; other syscalls behave the
/// inverse way.  Granting both removes the guess without enlarging the trust
/// surface — the two names already reach the same inode.
pub(crate) fn match_variants(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![p.to_path_buf(), canonicalise_lenient(p)];
    let toggles: Vec<PathBuf> = out.iter().filter_map(|q| firmlink_toggle(q)).collect();
    out.extend(toggles);
    out
}

/// Engine behind [`render_paths`](super::render_paths).  Dedup keys on the
/// exact path bytes, never a lossy rendering: two distinct non-UTF-8 paths
/// decode to the same U+FFFD string, and collapsing them would attribute one
/// path's grant to the other.  Separate from `render_paths` because that
/// function's `S: AsRef<str>` bound means a test can never hand it a
/// non-UTF-8 [`Path`].
pub(crate) fn match_variants_paths<'a>(
    paths: impl Iterator<Item = &'a Path>,
) -> Result<Vec<String>, String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for p in paths {
        for v in match_variants(p) {
            if seen.insert(v.clone()) {
                // `{v:?}`, not `.display()`: `Display` would paper over the
                // fault with the very U+FFFD substitution being refused.
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

/// The other spelling of `p` if it starts at a [`FIRMLINKS`] root, in either
/// direction; `None` otherwise.  A pure byte splice — no filesystem, no UTF-8
/// round-trip — so it holds on paths that do not exist, inside a sandbox where
/// `realpath(3)` would fail, and on a tail that is not valid UTF-8.
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
                // Safety: an ASCII firmlink-root literal followed by `rest`,
                // an unmodified suffix of `p`'s own encoded bytes.  Splicing
                // at an ASCII boundary cannot land inside a multi-byte
                // UTF-8/WTF-8 sequence, so `out` is encoded exactly as validly
                // as `p.as_os_str()` was.
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
        // Only the suffix is asserted: where `/` canonicalises non-trivially
        // the tail is re-appended to a different head.
        assert!(out.ends_with("either"), "got {out:?}");
    }

    #[test]
    fn lenient_resolves_existing_ancestor_and_reattaches_tail() {
        // `/tmp` exists and the tail does not, so the head may come back as
        // `/private/tmp` on macOS.
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
        let v = match_variants(Path::new("/var/select"));
        assert!(v.iter().any(|p| p == Path::new("/var/select")));
        assert!(v.iter().any(|p| p == Path::new("/private/var/select")));

        let v = match_variants(Path::new("/private/var/select"));
        assert!(v.iter().any(|p| p == Path::new("/var/select")));
        assert!(v.iter().any(|p| p == Path::new("/private/var/select")));

        let v = match_variants(Path::new("/private/var/folders/X/T"));
        assert!(v.iter().any(|p| p == Path::new("/var/folders/X/T")));
    }

    // Unix-only because Windows reads `/Users/...` as relative to the current
    // drive, where `C:\Users` exists, so the ancestor walk resolves a head and
    // the path no longer passes through untouched.
    #[cfg(unix)]
    #[test]
    fn match_variants_passes_unrelated_paths_through() {
        let v = match_variants(Path::new("/Users/nobody/projects/foo"));
        assert!(
            v.iter()
                .all(|p| p == Path::new("/Users/nobody/projects/foo"))
        );
    }

    /// Security regression: a non-UTF-8 tail must survive byte-for-byte.  A
    /// U+FFFD substitution would leave the twin naming a different inode —
    /// an under-grant on the twin side.
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

        let canon = PathBuf::from(OsStr::from_bytes(b"/private/tmp/\xFFghost"));
        let back = firmlink_toggle(&canon).expect("/private/tmp is a firmlink root");
        assert_eq!(
            back.as_os_str().as_bytes(),
            b"/tmp/\xFFghost",
            "got {back:?}"
        );
    }

    /// Security regression: two distinct non-UTF-8 paths that decode to the
    /// same U+FFFD string must not collapse into one entry, which would
    /// attribute one path's grant to the other.
    #[cfg(unix)]
    #[test]
    fn match_variants_paths_does_not_collide_distinct_non_utf8_paths() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let a: &[u8] = b"/opt/\xFFsecret";
        let b: &[u8] = b"/opt/\xFEsecret";
        // Sanity: unless these two really lossy-collide, the test proves
        // nothing.
        assert_eq!(
            Path::new(OsStr::from_bytes(a)).to_string_lossy(),
            Path::new(OsStr::from_bytes(b)).to_string_lossy(),
        );

        let pa = PathBuf::from(OsStr::from_bytes(a));
        let pb = PathBuf::from(OsStr::from_bytes(b));
        let err = match_variants_paths([pa.as_path(), pb.as_path()].into_iter())
            .expect_err("non-UTF-8 paths must fail closed, not render lossily");
        assert!(err.contains("not valid UTF-8"), "got {err:?}");
    }

    /// A path the renderer cannot express faithfully must produce an `Err`,
    /// never a lossy `(subpath …)` rule naming the wrong inode.  Touching no
    /// firmlink root isolates this from the toggle above.
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
}
