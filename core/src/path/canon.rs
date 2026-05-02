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

/// Top-level macOS firmlinks, expressed as `(firmlink, canonical)`.
/// Firmlinks bridge the read-only system volume to the data volume
/// at these well-known mount points; userland sees either form
/// transparently, but the kernel sandbox layer may present either to
/// a MAC hook depending on which API the caller used.
///
/// Empty on non-macOS — firmlinks are an APFS feature.
#[cfg(target_os = "macos")]
const FIRMLINK_PAIRS: &[(&str, &str)] = &[
    ("/var", "/private/var"),
    ("/tmp", "/private/tmp"),
    ("/etc", "/private/etc"),
];
#[cfg(not(target_os = "macos"))]
const FIRMLINK_PAIRS: &[(&str, &str)] = &[];

/// Strict realpath: errors when the file or an intermediate
/// directory is missing.  One-line wrapper over `fs::canonicalize`,
/// existing so call sites name their intent — and so the
/// workspace-wide `disallowed_methods` lint can keep
/// `std::fs::canonicalize` itself caged inside this file.
#[allow(clippy::disallowed_methods)]
pub fn canonicalise_strict(p: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(p)
}

/// Lenient canonicalisation: resolves the longest existing prefix
/// of `p` and re-appends the unresolved tail.  Always returns
/// something — falls back to `p` itself when no ancestor exists.
///
/// Needed for grant prefixes that may name not-yet-created
/// targets (e.g. a `fs.write` grant against a build output path),
/// and so that a grant authored as `/tmp/foo` still matches when
/// `/tmp` is a symlink and the access path resolves through the
/// symlink to `/private/tmp/foo`.
#[allow(clippy::disallowed_methods)]
pub fn canonicalise_lenient(p: &Path) -> PathBuf {
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
/// present the same VFS object as `p`.  Always includes `p`
/// itself; on macOS also includes the lenient canonical form
/// and any firmlink-toggled variant.  Sorted, deduped.
///
/// The motivation is empirical: a Seatbelt rule written
/// `(subpath "/private/var/select")` does not match an `lstat` of
/// `/var/select/developer_dir` on every macOS version, but the
/// twin rule `(subpath "/var/select")` does.  Other syscalls
/// behave the inverse way.  Granting both forms removes the
/// guessing without enlarging the trust surface — both names
/// already point to the same inode.
pub fn match_variants(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![p.to_path_buf(), canonicalise_lenient(p)];
    let toggles: Vec<PathBuf> = out.iter().filter_map(|q| firmlink_toggle(q)).collect();
    out.extend(toggles);
    out.sort();
    out.dedup();
    out
}

/// List-shaped [`match_variants`]: expand every entry to its firmlink
/// equivalents, flatten, dedupe.  A grant for `/tmp/work` produces
/// `[/tmp/work, /private/tmp/work]`, since Seatbelt may present either
/// form to the MAC hook depending on the syscall.  Used by the macOS
/// sandbox profile builder when laying out subpath rules.
pub fn match_variants_list(paths: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for p in paths {
        for v in match_variants(Path::new(p)) {
            let s = v.to_string_lossy().into_owned();
            if seen.insert(s.clone()) {
                out.push(s);
            }
        }
    }
    out
}

/// If `p` begins with one of the macOS firmlink roots (in either
/// direction), return the toggled variant; otherwise `None`.
/// Pure string operation — no filesystem access — so it works on
/// non-existent paths and inside a sandbox where canonicalise calls
/// would fail.
fn firmlink_toggle(p: &Path) -> Option<PathBuf> {
    let s = p.to_string_lossy();
    for (firm, canon) in FIRMLINK_PAIRS {
        for (from, to) in [(*canon, *firm), (*firm, *canon)] {
            if let Some(rest) = s.strip_prefix(from) {
                if rest.is_empty() || rest.starts_with('/') {
                    return Some(PathBuf::from(format!("{to}{rest}")));
                }
            }
        }
    }
    None
}

#[cfg(test)]
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

    #[test]
    fn match_variants_passes_unrelated_paths_through() {
        // Path that doesn't touch any firmlink root and has no
        // resolvable symlinks: only the input is returned.
        let v = match_variants(Path::new("/Users/nobody/projects/foo"));
        assert_eq!(v, vec![PathBuf::from("/Users/nobody/projects/foo")]);
    }
}
