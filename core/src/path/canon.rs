//! The names one object answers to: its firmlink twins ([`twins`]), for the
//! path matcher and the kernel sandboxes alike.  All lexical: what the object
//! is, and where a surface reaches it, is the walk's to say.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// macOS firmlinks bridging the read-only system volume to the data volume,
/// as `(firm, canon)` pairs.  A MAC hook may be handed either form depending
/// on which API the caller used, so a grant has to name both.  Empty off
/// macOS — firmlinks are an APFS feature.
#[cfg(target_os = "macos")]
const FIRMLINKS: &[(&str, &str)] = &[
    ("/var", "/private/var"),
    ("/tmp", "/private/tmp"),
    ("/etc", "/private/etc"),
];
#[cfg(not(target_os = "macos"))]
const FIRMLINKS: &[(&str, &str)] = &[];

/// `p`, then, on macOS, its firmlink toggle: every path string by which the
/// host, or a kernel sandbox MAC hook, might present the object at `p`.  Just
/// `p` elsewhere, or where `p` starts at no firmlink root.
///
/// The motivation is empirical.  A Seatbelt rule `(subpath "/private/var/select")`
/// does not match an `lstat` of `/var/select/developer_dir` on every macOS
/// version, but `(subpath "/var/select")` does; other syscalls behave the
/// inverse way.  Granting both removes the guess without enlarging the trust
/// surface — the two names already reach the same inode.
pub(crate) fn twins(p: &Path) -> impl Iterator<Item = Cow<'_, Path>> + Clone {
    std::iter::once(Cow::Borrowed(p)).chain(firmlink_toggle(p).map(Cow::Owned))
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

    #[cfg(target_os = "macos")]
    #[test]
    fn twins_toggle_macos_firmlinks() {
        let v: Vec<_> = twins(Path::new("/var/select")).collect();
        assert!(v.iter().any(|p| p == Path::new("/var/select")));
        assert!(v.iter().any(|p| p == Path::new("/private/var/select")));

        let v: Vec<_> = twins(Path::new("/private/var/select")).collect();
        assert!(v.iter().any(|p| p == Path::new("/var/select")));
        assert!(v.iter().any(|p| p == Path::new("/private/var/select")));

        let v: Vec<_> = twins(Path::new("/private/var/folders/X/T")).collect();
        assert!(v.iter().any(|p| p == Path::new("/var/folders/X/T")));
    }

    #[test]
    fn twins_pass_unrelated_paths_through() {
        let v: Vec<_> = twins(Path::new("/Users/nobody/projects/foo")).collect();
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
}
