//! Path identity: which spellings of a name are one name, and the
//! alias-aware containment the grant matcher folds over it.
//!
//! The firmlink table both sides share lives in [`super::canon`], so matcher
//! and canonicaliser can never see different aliases.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use icu_casemap::CaseMapper;
use icu_locale_core::LanguageIdentifier;
use icu_normalizer::DecomposingNormalizerBorrowed;

use super::PathRules;

/// `p` plus any alternate spelling the host treats as the same file
/// (`/tmp/foo` ↔ `/private/tmp/foo` on macOS; just `[p]` elsewhere).
///
/// The matcher needs this because `canonicalize` cannot always bridge the
/// two forms: under Seatbelt `realpath(3)` can fail on `/tmp` itself, so a
/// grant authored in one spelling still covers an access in the other.
fn path_aliases(p: &Path) -> Vec<PathBuf> {
    let mut out = vec![p.to_path_buf()];
    out.extend(super::canon::firmlink_toggle(p));
    out
}

/// Which spellings of a name count as one name.  An allow is judged under
/// [`Stored`](Self::Stored), a deny under [`Collision`](Self::Collision): a
/// rule's [`Polarity`] picks, never a caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Identity {
    /// The name as stored: bytes off Windows, ASCII case on Windows, where
    /// the walk does not spell stored names.
    Stored,
    /// Every name some filesystem takes for the same: [`collision_key`] per
    /// component.
    Collision,
}

/// Which way a rule speaks, and so which [`Identity`] it reads names under.
pub(crate) trait Polarity {
    const IDENTITY: Identity;
}

/// An allow holds a name as stored.
pub(crate) enum Allow {}

/// A deny holds every spelling of a name.
pub(crate) enum Deny {}

impl Polarity for Allow {
    const IDENTITY: Identity = Identity::Stored;
}

impl Polarity for Deny {
    const IDENTITY: Identity = Identity::Collision;
}

/// The name a filesystem may take `name` for: compatibility caseless
/// matching (Unicode D147) of the uppercase image of its compatibility
/// decomposition.  A name that is not UTF-8 is its own key; no filesystem
/// folds one.
///
/// Coarser than or equal to every identity in play (case-insensitive APFS
/// and Seatbelt, NTFS `$UpCase`, Linux casefold, ZFS under every
/// `casesensitivity` and `normalization`) and so fail-closed where a volume
/// is finer.  NFKD comes first: `𝚤` decomposes to ı, which ZFS upcases to I,
/// yet has no uppercase of its own.
pub(super) fn collision_key(name: &OsStr) -> Cow<'_, OsStr> {
    let Some(s) = name.to_str() else {
        return Cow::Borrowed(name);
    };
    if s.is_ascii() {
        return Cow::Owned(s.to_ascii_lowercase().into());
    }
    let nfd = DecomposingNormalizerBorrowed::new_nfd();
    let nfkd = DecomposingNormalizerBorrowed::new_nfkd();
    let case = CaseMapper::new();
    let upper = case
        .uppercase_to_string(&nfkd.normalize(s), &LanguageIdentifier::UNKNOWN)
        .into_owned();
    let once = nfkd
        .normalize(&case.fold_string(&nfd.normalize(&upper)))
        .into_owned();
    Cow::Owned(nfkd.normalize(&case.fold_string(&once)).into_owned().into())
}

/// True iff some alias of `path` starts with some alias of `prefix`: `path`
/// lies inside `prefix` modulo firmlinks and `identity`, on Windows the
/// path identity [`starts_with_identity`] applies at the least.
///
/// Every authority judgment, fs and exec, decides containment through it.
///
/// `pub(super)`: the kernel is form-blind, and *which* of a prefix's two
/// forms a containment question is asked of, under which identity, is
/// settled inside `path` — by a [`Polarity`] through
/// [`FrozenPath::contains`](super::FrozenPath::contains) for fs and
/// [`RealPath::within`](super::RealPath::within) for exec — and never by a
/// caller holding two bare paths.
pub(super) fn path_within(path: &Path, prefix: &Path, identity: Identity) -> bool {
    let rules = PathRules::HOST;
    let ps = path_aliases(path);
    let qs = path_aliases(prefix);
    ps.iter().any(|p| {
        qs.iter().any(|q| match identity {
            // The identity fold is defined on strings, so this arm
            // necessarily accepts the lossy form: two distinct non-UTF-8
            // paths that decode alike compare equal here.  `FrozenPath`
            // already freezes to `to_string_lossy` strings, so nothing is
            // closed by fixing only the matcher, and failing closed here
            // would make a *deny* prefix fail open.
            Identity::Stored if rules == PathRules::Windows => {
                starts_with_identity(&p.to_string_lossy(), &q.to_string_lossy(), rules)
            }
            // Compare the `&Path`s, not their `to_string_lossy` forms: two
            // distinct non-UTF-8 paths can decode to the same
            // replacement-character string, and the matcher would call them
            // one.
            Identity::Stored => p.starts_with(q),
            Identity::Collision => starts_with_collision(p, q, rules),
        })
    })
}

/// [`path_within`] on strings, and `pub(super)` for the same reason.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: lifts both strings for `path_within`; nothing is resolved"
)]
pub(super) fn path_within_str(path: &str, prefix: &str, identity: Identity) -> bool {
    path_within(Path::new(path), Path::new(prefix), identity)
}

/// Component-wise prefix test under [`collision_key`]: split as
/// [`starts_with_identity`] splits under Windows rules, by
/// [`Path::components`] otherwise, then compared key by key, so no fold
/// crosses a component boundary (`/w/secretsx` is not within `/w/Secrets`).
fn starts_with_collision(path: &Path, prefix: &Path, rules: PathRules) -> bool {
    let keys = |p: &Path| -> Vec<OsString> {
        match rules {
            PathRules::Windows => windows_components(&p.to_string_lossy(), |c| {
                collision_key(OsStr::new(c)).into_owned()
            }),
            PathRules::Posix => p
                .components()
                .map(|c| collision_key(c.as_os_str()).into_owned())
                .collect(),
        }
    };
    let (path, prefix) = (keys(path), keys(prefix));
    path.len() >= prefix.len() && path[..prefix.len()] == prefix[..]
}

/// Component-wise prefix test: byte-exact under POSIX rules (via
/// [`Path::starts_with`], so `/tmp` never matches `/tmpx`), and under Windows
/// rules by Windows path identity: `/` ≡ `\`, case-insensitive components, a
/// `\\?\`-verbatim prefix equivalent to its plain spelling, so a grant that
/// went through `canonicalize` still matches a candidate that did not.
///
/// String logic rather than `std::path::Path`, whose separator and prefix
/// parsing are fixed at compile time to the build target.  The platform gate
/// sits at the sole call site, [`path_within`].
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: POSIX identity is `Path::starts_with` itself, byte-exact per component"
)]
pub(crate) fn starts_with_identity(path: &str, prefix: &str, rules: PathRules) -> bool {
    if rules == PathRules::Posix {
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
/// `which`'s `name_key_on` so paths and command names fold alike, and
/// erring below the real NTFS `$UpCase` table rather than above it: missing
/// non-ASCII folds sooner than claiming equivalences the driver would refuse.
pub(crate) fn windows_identity_components(p: &str) -> Vec<String> {
    windows_components(p, str::to_ascii_lowercase)
}

/// A path string's components under Windows path identity, each through
/// `key`: the one split [`windows_identity_components`] and
/// [`starts_with_collision`] share.
fn windows_components<T>(p: &str, key: impl Fn(&str) -> T) -> Vec<T> {
    windows_head(p)
        .split(['/', '\\'])
        .filter(|c| !c.is_empty())
        .map(key)
        .collect()
}

/// A path string with its Windows head normalised: a verbatim prefix
/// stripped, a verbatim UNC head folded back to `\server\share`.  The step
/// [`windows_identity_components`] takes before it lower-cases and splits,
/// shared with the prompt's home-strip (`super::tilde`), which needs the head
/// fold without the case fold.  One copy, because a head the two folded
/// differently would silently unfold a `~`.
pub(crate) fn windows_head(p: &str) -> String {
    let s = strip_verbatim_prefix(p);
    s.strip_prefix("UNC\\")
        .or_else(|| s.strip_prefix("UNC/"))
        .map_or_else(|| s.to_string(), |rest| format!(r"\{rest}"))
}

/// Strip a leading verbatim prefix (two separators, `?`, a separator) under
/// either slash spelling and the mixed forms between.
fn strip_verbatim_prefix(p: &str) -> &str {
    let b = p.as_bytes();
    if b.len() >= 4 && is_sep(b[0]) && is_sep(b[1]) && b[2] == b'?' && is_sep(b[3]) {
        &p[4..]
    } else {
        p
    }
}

/// A path separator under Windows path identity: `/` and `\` alike.
pub(super) fn is_sep(c: u8) -> bool {
    matches!(c, b'/' | b'\\')
}

/// Depth of `dir` in components, folded through the same identity
/// [`path_within`] matches with: a firmlink alias collapsed to its canonical
/// (longer) spelling, then split under Windows path identity under Windows
/// rules, by [`Path::components`] otherwise.
///
/// `capability::exec` ranks competing directory rules by depth, and a
/// character count is a depth proxy only within one spelling:
/// `/tmp/a/b` nests deeper than `/private/tmp` yet is shorter, so counting
/// characters ranks spelling and lets a shallow alias outrank the directory
/// it sits above.
pub(crate) fn identity_depth(dir: &str, rules: PathRules) -> usize {
    let original = PathBuf::from(dir);
    let canonical = match super::canon::firmlink_toggle(&original) {
        Some(alt) if alt.as_os_str().len() > original.as_os_str().len() => alt,
        _ => original,
    };
    match rules {
        PathRules::Windows => windows_identity_components(&canonical.to_string_lossy()).len(),
        PathRules::Posix => canonical.components().count(),
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures compare literal paths"
)]
mod tests {
    use super::*;

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
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
        assert!(path_within(
            Path::new("/a/b"),
            Path::new("/a/b"),
            Identity::Stored
        ));
    }

    #[test]
    fn path_within_strict_descendant() {
        assert!(path_within(
            Path::new("/a/b/c"),
            Path::new("/a/b"),
            Identity::Stored
        ));
    }

    #[test]
    fn path_within_not_a_descendant() {
        assert!(!path_within(
            Path::new("/a/b"),
            Path::new("/a/c"),
            Identity::Stored
        ));
        assert!(!path_within(
            Path::new("/a"),
            Path::new("/a/b"),
            Identity::Stored
        ));
    }

    #[test]
    fn path_within_no_substring_pseudomatch() {
        assert!(!path_within(
            Path::new("/tmpx"),
            Path::new("/tmp"),
            Identity::Stored
        ));
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
            !path_within(&candidate_path, prefix, Identity::Stored),
            "distinct non-UTF-8 paths must not collide via lossy string comparison"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn path_within_via_alias() {
        assert!(path_within(
            Path::new("/tmp/foo"),
            Path::new("/private/tmp"),
            Identity::Stored
        ));
        assert!(path_within(
            Path::new("/private/tmp/foo"),
            Path::new("/tmp"),
            Identity::Stored
        ));
    }

    /// T1: the key merges every pair some filesystem merges, and nothing
    /// finer.
    #[test]
    fn collision_key_merges_what_some_filesystem_merges() {
        let key = |s: &str| collision_key(OsStr::new(s)).into_owned();
        for (a, b) in [
            ("Secrets", "secrets"),
            ("caf\u{e9}", "cafe\u{301}"),
            ("stra\u{df}e", "strasse"),
            ("\u{df}", "\u{1e9e}"),
            ("\u{fb01}le", "file"),
            ("\u{212a}ey", "key"),
            ("\u{3c3}", "\u{3c2}"),
            ("\u{130}", "i\u{307}"),
            ("f\u{131}le", "FILE"),
            ("\u{1d6a4}", "\u{131}"),
            ("\u{ff21}", "a"),
        ] {
            assert_eq!(key(a), key(b), "{a:?} and {b:?} must collide");
        }
        for (a, b) in [("abc", "abd"), ("secret", "secrets")] {
            assert_ne!(key(a), key(b), "{a:?} and {b:?} must stay distinct");
        }
    }

    /// T1: no filesystem folds a name that is not UTF-8, so the key is the
    /// name, byte for byte.
    #[cfg(unix)]
    #[test]
    fn collision_key_leaves_a_non_utf8_name_its_own() {
        use std::os::unix::ffi::OsStrExt;
        let (ff, fe) = (OsStr::from_bytes(b"\xFF"), OsStr::from_bytes(b"\xFE"));
        assert_eq!(collision_key(ff), ff);
        assert_ne!(collision_key(ff), collision_key(fe));
    }

    /// T2: a deny's relation folds within a component and never across one;
    /// an allow's does not fold at all.
    #[test]
    fn collision_folds_components_and_stored_does_not() {
        let within = |p: &str, q: &str, identity| path_within(Path::new(p), Path::new(q), identity);
        assert!(within("/w/secrets/t", "/w/Secrets", Identity::Collision));
        assert!(within("/w/SECRETS", "/w/Secrets", Identity::Collision));
        assert!(!within("/w/secretsx", "/w/Secrets", Identity::Collision));
        assert!(!within("/w/other/t", "/w/Secrets", Identity::Collision));
        if cfg!(not(windows)) {
            assert!(!within("/w/secrets/t", "/w/Secrets", Identity::Stored));
        }
    }

    /// T2 under the Windows split, pinned on every host: heads and
    /// separators fold as [`starts_with_identity`] folds them, and non-ASCII
    /// case, which the stored identity misses, collides.
    #[test]
    fn windows_collision_folds_non_ascii_case() {
        let within = |p: &str, q: &str| {
            starts_with_collision(Path::new(p), Path::new(q), PathRules::Windows)
        };
        assert!(within(r"\\?\C:\w\éclair\x", "c:/W/ÉCLAIR"));
        assert!(!within(r"C:\w\eclair", r"C:\w\Éclair"));
        assert!(!starts_with_identity(
            r"C:\w\éclair",
            r"C:\w\Éclair",
            PathRules::Windows
        ));
    }

    // The Windows rules below pass `PathRules::Windows` directly rather than
    // hiding behind `cfg(windows)`, so they are pinned on every host.
    const W: PathRules = PathRules::Windows;

    #[test]
    fn windows_identity_ignores_case() {
        assert!(starts_with_identity(r"C:\WORK\sub", r"c:\work", W));
        assert!(starts_with_identity(r"c:\Work\Sub", r"C:\WORK", W));
    }

    #[test]
    fn windows_identity_unifies_forward_and_back_slashes() {
        assert!(starts_with_identity(r"c:/work/sub", r"C:\work", W));
        assert!(starts_with_identity(r"C:\work\sub", "c:/work", W));
    }

    #[test]
    fn windows_identity_strips_verbatim_prefix() {
        assert!(starts_with_identity(r"\\?\C:\work\sub", r"C:\work", W));
        assert!(starts_with_identity(r"C:\work\sub", r"\\?\C:\work", W));
        assert!(starts_with_identity(r"\\?\C:\work\sub", r"\\?\c:\WORK", W));
    }

    #[test]
    fn windows_identity_folds_verbatim_unc() {
        assert!(starts_with_identity(
            r"\\?\UNC\server\share\sub",
            r"\\server\share",
            W
        ));
    }

    /// Without this, a `//?/`-spelled access path bypasses a `\\?\`- or
    /// plain-spelled deny: real Windows accepts only the backslash form, but
    /// this matcher holds `/` and `\` interchangeable, so a deny authored in
    /// one spelling must catch an access spelled the other way.
    #[test]
    fn windows_identity_strips_forward_slash_verbatim_prefix() {
        assert!(starts_with_identity(r"//?/C:/work/sub", r"C:\work", W));
        assert!(starts_with_identity(r"C:\work\sub", "//?/c:/work", W));
        assert!(starts_with_identity(r"//?/C:/work/sub", r"\\?\c:\WORK", W));
        assert!(starts_with_identity(r"//?\C:\work/sub", r"C:\work", W));
    }

    #[test]
    fn windows_identity_folds_forward_slash_verbatim_unc() {
        assert!(starts_with_identity(
            "//?/UNC/server/share/sub",
            r"\\server\share",
            W
        ));
    }

    #[test]
    fn windows_identity_respects_component_boundaries() {
        assert!(!starts_with_identity(r"C:\workshop", r"C:\work", W));
    }

    #[test]
    fn windows_identity_rejects_unrelated_drive() {
        assert!(!starts_with_identity(r"D:\work\sub", r"C:\work", W));
    }

    #[test]
    fn posix_identity_is_byte_exact() {
        assert!(!starts_with_identity(
            r"C:\WORK",
            r"C:\work",
            PathRules::Posix
        ));
    }

    /// Three spellings of one directory must report one depth, or the
    /// deepest-prefix ranking picks by spelling.
    #[test]
    fn identity_depth_windows_folds_case_separator_and_verbatim() {
        assert_eq!(identity_depth(r"C:\work\sub", W), 3);
        assert_eq!(identity_depth(r"c:/WORK/SUB", W), 3);
        assert_eq!(identity_depth(r"\\?\C:\work\sub", W), 3);
    }
}
