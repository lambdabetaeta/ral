//! The names one object answers to: its firmlink twins ([`twins`]), for the
//! path matcher and the kernel sandboxes alike.  All lexical: what the object is, and where a
//! surface reaches it, is the walk's to say.

#[cfg(unix)]
use super::site::Silent;
#[cfg(unix)]
use crate::site::site;
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

/// `p` and, on macOS, its firmlink toggle: every path string by which the
/// host, or a kernel sandbox MAC hook, might present the object at `p`.  Just
/// `[p]` elsewhere, or where `p` starts at no firmlink root.  Unsorted; the
/// caller owns order and uniqueness.
///
/// The motivation is empirical.  A Seatbelt rule `(subpath "/private/var/select")`
/// does not match an `lstat` of `/var/select/developer_dir` on every macOS
/// version, but `(subpath "/var/select")` does; other syscalls behave the
/// inverse way.  Granting both removes the guess without enlarging the trust
/// surface — the two names already reach the same inode.
pub(crate) fn twins(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![p.to_path_buf()];
    out.extend(firmlink_toggle(p));
    out
}

/// The [`twins`] of each path, deduped and refused unless UTF-8.  Dedup keys
/// on the exact path bytes, never a lossy rendering: two distinct non-UTF-8
/// paths decode to the same U+FFFD string, and collapsing them would
/// attribute one path's grant to the other.
#[cfg(unix)]
pub(crate) fn spelled<'a>(paths: impl Iterator<Item = &'a Path>) -> Result<Vec<String>, String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for p in paths {
        for v in twins(p) {
            if seen.insert(v.clone()) {
                // `{v:?}`, not `.display()`: `Display` would paper over the
                // fault with the very U+FFFD substitution being refused.
                #[allow(clippy::unnecessary_debug_formatting)]
                let s = site!(Silent::RuleText, &v => to_str()).ok_or_else(|| {
                    format!(
                        "ral: sandbox grant path is not valid UTF-8, refusing to render \
                         it into a sandbox rule that could name the wrong path: {v:?}"
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
fn firmlink_toggle(p: &Path) -> Option<PathBuf> {
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

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn twins_always_include_input() {
        let v = twins(Path::new("/some/non/existent/path"));
        assert!(v.iter().any(|p| p == Path::new("/some/non/existent/path")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn twins_toggle_macos_firmlinks() {
        let v = twins(Path::new("/var/select"));
        assert!(v.iter().any(|p| p == Path::new("/var/select")));
        assert!(v.iter().any(|p| p == Path::new("/private/var/select")));

        let v = twins(Path::new("/private/var/select"));
        assert!(v.iter().any(|p| p == Path::new("/var/select")));
        assert!(v.iter().any(|p| p == Path::new("/private/var/select")));

        let v = twins(Path::new("/private/var/folders/X/T"));
        assert!(v.iter().any(|p| p == Path::new("/var/folders/X/T")));
    }

    #[test]
    fn twins_pass_unrelated_paths_through() {
        let v = twins(Path::new("/Users/nobody/projects/foo"));
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
    #[test]
    fn spelled_does_not_collide_distinct_non_utf8_paths() {
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
        let err = spelled([pa.as_path(), pb.as_path()].into_iter())
            .expect_err("non-UTF-8 paths must fail closed, not render lossily");
        assert!(err.contains("not valid UTF-8"), "got {err:?}");
    }

    /// A path the renderer cannot express faithfully must produce an `Err`,
    /// never a lossy `(subpath …)` rule naming the wrong inode.  Touching no
    /// firmlink root isolates this from the toggle above.
    #[test]
    fn spelled_fails_closed_on_non_utf8_variant() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let p = PathBuf::from(OsStr::from_bytes(
            b"/this/should/not/exist/anywhere/\xFFghost",
        ));
        let err = spelled([p.as_path()].into_iter())
            .expect_err("non-UTF-8 grant path must be refused, not lossily rendered");
        assert!(err.contains("not valid UTF-8"), "got {err:?}");
    }
}
