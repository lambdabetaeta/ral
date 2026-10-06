//! Lexical path resolution: a sigil-expanded string becomes an absolute,
//! `.`/`..`-free path against a scoped cwd, with no filesystem access.
//!
//! The alias-aware containment the grant matcher folds over the result lives
//! here too.  The firmlink table both sides share lives in [`super::canon`],
//! so matcher and canonicaliser can never see different aliases.

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use icu_casemap::CaseMapper;
use icu_locale_core::LanguageIdentifier;
use icu_normalizer::DecomposingNormalizerBorrowed;

use super::process_cwd;

/// `p` plus any alternate spelling the host treats as the same file
/// (`/tmp/foo` ↔ `/private/tmp/foo` on macOS; just `[p]` elsewhere).
///
/// The matcher needs this because `canonicalize` cannot always bridge the
/// two forms — under Seatbelt `realpath(3)` can fail on `/tmp` itself — so a
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

/// The name a filesystem may take `name` for: canonical caseless matching
/// (Unicode D145) of its uppercase image.  A name that is not UTF-8 is its
/// own key; no filesystem folds one.
///
/// Coarser than or equal to every identity in play — case-insensitive APFS
/// and Seatbelt, NTFS `$UpCase`, Linux casefold — and so fail-closed where a
/// volume is finer.  The uppercase image adds exactly {ı, i, I} to D145,
/// which `$UpCase` merges.
pub(super) fn collision_key(name: &OsStr) -> Cow<'_, OsStr> {
    let Some(s) = name.to_str() else {
        return Cow::Borrowed(name);
    };
    if s.is_ascii() {
        return Cow::Owned(s.to_ascii_lowercase().into());
    }
    let nfd = DecomposingNormalizerBorrowed::new_nfd();
    let case = CaseMapper::new();
    let upper = case.uppercase_to_string(s, &LanguageIdentifier::UNKNOWN);
    let decomposed = nfd.normalize(&upper);
    let folded = case.fold_string(&decomposed);
    Cow::Owned(nfd.normalize(&folded).into_owned().into())
}

/// True iff some alias of `path` starts with some alias of `prefix`: `path`
/// lies inside `prefix` modulo firmlinks and `identity` — on Windows, the
/// path identity [`starts_with_identity`] applies, at the least.
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
    let ps = path_aliases(path);
    let qs = path_aliases(prefix);
    ps.iter().any(|p| {
        qs.iter().any(|q| match identity {
            // The identity fold is defined on strings, so this arm
            // necessarily accepts the lossy form: two distinct non-UTF-8
            // paths that decode alike compare equal here.  `FrozenPath`
            // already freezes to `to_string_lossy` strings, so nothing is
            // closed by fixing only the matcher — and failing closed here
            // would make a *deny* prefix fail open.
            Identity::Stored if cfg!(windows) => {
                starts_with_identity(&p.to_string_lossy(), &q.to_string_lossy(), true)
            }
            // Compare the `&Path`s, not their `to_string_lossy` forms: two
            // distinct non-UTF-8 paths can decode to the same
            // replacement-character string, and the matcher would call them
            // one.
            Identity::Stored => p.starts_with(q),
            Identity::Collision => starts_with_collision(p, q, cfg!(windows)),
        })
    })
}

/// Component-wise prefix test under [`collision_key`]: split as
/// [`starts_with_identity`] splits under `windows`, by [`Path::components`]
/// otherwise, then compared key by key, so no fold crosses a component
/// boundary (`/w/secretsx` is not within `/w/Secrets`).  `windows` is a
/// parameter for the reason [`starts_with_identity`]'s is.
fn starts_with_collision(path: &Path, prefix: &Path, windows: bool) -> bool {
    let keys = |p: &Path| -> Vec<OsString> {
        if windows {
            windows_components(&p.to_string_lossy(), |c| {
                collision_key(OsStr::new(c)).into_owned()
            })
        } else {
            p.components()
                .map(|c| collision_key(c.as_os_str()).into_owned())
                .collect()
        }
    };
    let (path, prefix) = (keys(path), keys(prefix));
    path.len() >= prefix.len() && path[..prefix.len()] == prefix[..]
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
/// `which`'s `name_key_on` so paths and command names fold alike, and
/// erring below the real NTFS `$UpCase` table rather than above it — missing
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

/// Strip a leading verbatim prefix — two separators, `?`, a separator — under
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
fn is_sep(c: u8) -> bool {
    matches!(c, b'/' | b'\\')
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

/// True iff `path` names the discard device: the sink that swallows every
/// byte and keeps none.  `/dev/null` under POSIX rules; under Windows', the
/// device-namespace spelling `\\.\NUL`, in either slash spelling and any case,
/// and nothing else — every other path ending in a reserved name is turned
/// away by [`reserved_device_refusal`].
///
/// `windows` is a parameter rather than a `cfg!` read, as in
/// [`starts_with_identity`], so both tables are pinned on every host; the
/// platform gate sits at the sole call site,
/// [`LexicalPath::is_discard`](super::LexicalPath::is_discard).
pub(crate) fn is_discard_device(path: &str, windows: bool) -> bool {
    if !windows {
        return path == "/dev/null";
    }
    match path.as_bytes() {
        [a, b, b'.', c, name @ ..] => {
            [a, b, c].into_iter().all(|&s| is_sep(s)) && name.eq_ignore_ascii_case(b"nul")
        }
        _ => false,
    }
}

/// The refusal owed `path` under Windows rules when its last component is a
/// DOS reserved device name — `NUL`, `CON`, `PRN`, `AUX`, `COM1`–`COM9`,
/// `LPT1`–`LPT9`, in any case, with or without an extension, trailing dots and
/// blanks as Win32 trims them — and `None` for every other path, the discard
/// device's own spelling included.
///
/// Such a name is no file: most Windows tools read it as the device, so a file
/// made under it is unusable to them, and ral does not take the device meaning
/// either — the one discard it offers is [`is_discard_device`]'s.
///
/// `windows` is a parameter for the reason [`is_discard_device`]'s is; the
/// gate is
/// [`LexicalPath::reserved_device_refusal`](super::LexicalPath::reserved_device_refusal).
pub(crate) fn reserved_device_refusal(path: &str, windows: bool) -> Option<String> {
    if !windows || is_discard_device(path, true) {
        return None;
    }
    let last = path.rsplit(['/', '\\']).find(|c| !c.is_empty())?;
    let stem = last
        .trim_end_matches(['.', ' '])
        .split('.')
        .next()?
        .trim_end_matches(' ');
    if !is_reserved_device_stem(stem) {
        return None;
    }
    Some(if stem.eq_ignore_ascii_case("nul") {
        format!(
            "`{last}` is a DOS device name, which ral does not treat as a device. \
             Did you mean `\\\\.\\NUL`? \
             (A file named `{last}` would be unusable from most Windows tools.)"
        )
    } else {
        format!(
            "`{last}` is a reserved DOS device name, \
             and ral refuses reserved device names as file names"
        )
    })
}

fn is_reserved_device_stem(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "NUL" | "CON" | "PRN" | "AUX")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9']))
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
#[allow(clippy::disallowed_methods)]
pub fn resolve_str(cwd: Option<&str>, path: &str) -> PathBuf {
    resolve_path(cwd.map(Path::new), path)
}

/// [`path_within`] on strings, and `pub(super)` for the same reason.
#[allow(clippy::disallowed_methods)]
pub(super) fn path_within_str(path: &str, prefix: &str, identity: Identity) -> bool {
    path_within(Path::new(path), Path::new(prefix), identity)
}

/// Depth of `dir` in components, folded through the same identity
/// [`path_within`] matches with: a firmlink alias collapsed to its canonical
/// (longer) spelling, then split under Windows path identity when `windows`
/// is set, by [`Path::components`] otherwise.
///
/// `capability::exec` ranks competing directory rules by depth, and a
/// character count is a depth proxy only within one spelling:
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
pub(crate) fn parent_or_cwd(path: &Path) -> &Path {
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
#[allow(clippy::disallowed_methods)]
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
            ("f\u{131}le", "file"),
            ("f\u{131}le", "FILE"),
        ] {
            assert_eq!(key(a), key(b), "{a:?} and {b:?} must collide");
        }
        for (a, b) in [("\u{ff21}", "a"), ("abc", "abd"), ("secret", "secrets")] {
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
        let within = |p: &str, q: &str| starts_with_collision(Path::new(p), Path::new(q), true);
        assert!(within(r"\\?\C:\w\éclair\x", "c:/W/ÉCLAIR"));
        assert!(!within(r"C:\w\eclair", r"C:\w\Éclair"));
        assert!(!starts_with_identity(r"C:\w\éclair", r"C:\w\Éclair", true));
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

    // Only the device-namespace spelling is the discard under Windows rules;
    // `windows` is passed directly, so both tables are pinned on every host.
    #[test]
    fn discard_device_table() {
        for p in [r"\\.\NUL", r"\\.\nul", "//./NUL", r"\/.\Nul"] {
            assert!(is_discard_device(p, true), "{p}");
        }
        for p in [
            "NUL",
            "nul",
            "nul.txt",
            "NULL",
            r"C:\denied\nul.txt",
            r"\\?\C:\denied\nul.txt",
            r"C:\x\NUL",
            r"\\?\C:\x\nul",
            r"\\?\NUL",
            r"\\.\NUL\x",
            r"\\.\NUL.txt",
            "/dev/null",
        ] {
            assert!(!is_discard_device(p, true), "{p}");
        }
        assert!(is_discard_device("/dev/null", false));
        for p in ["/dev/null/x", "/tmp/nul", "NUL", r"\\.\NUL"] {
            assert!(!is_discard_device(p, false), "{p}");
        }
    }

    #[test]
    fn reserved_device_names_are_refused_under_windows_rules() {
        for p in [
            "NUL",
            "nul",
            "NUL.txt",
            "con",
            "COM1.log",
            "LPT9",
            "prn.",
            "AUX ",
            r"C:\x\NUL",
            r"C:\x\nul .txt",
            "C:/x/aux",
            r"\\?\C:\denied\nul.txt",
        ] {
            assert!(reserved_device_refusal(p, true).is_some(), "{p}");
        }
    }

    #[test]
    fn ordinary_names_and_the_discard_are_not_refused() {
        for p in [
            r"\\.\NUL",
            "//./nul",
            "null",
            "nullable.txt",
            "CONFIG",
            "COM10",
            "COM0",
            "LPTA",
            r"C:\x\nul\file",
            r"\\.\NUL\test-agent",
            r"C:\",
            "...",
            "",
        ] {
            assert_eq!(reserved_device_refusal(p, true), None, "{p}");
        }
    }

    #[test]
    fn nothing_is_refused_off_windows() {
        for p in ["NUL", "con", "/tmp/aux.c"] {
            assert_eq!(reserved_device_refusal(p, false), None, "{p}");
        }
    }

    #[test]
    fn nul_refusal_names_the_component_and_the_discard() {
        assert_eq!(
            reserved_device_refusal(r"C:\x\NUL", true).as_deref(),
            Some(
                r"`NUL` is a DOS device name, which ral does not treat as a device. Did you mean `\\.\NUL`? (A file named `NUL` would be unusable from most Windows tools.)"
            )
        );
        let verbatim = reserved_device_refusal(r"\\?\C:\denied\nul.txt", true).unwrap();
        assert!(
            verbatim.starts_with("`nul.txt` is a DOS device name"),
            "{verbatim}"
        );
    }

    #[test]
    fn other_reserved_names_suggest_no_discard() {
        assert_eq!(
            reserved_device_refusal("COM1.log", true).as_deref(),
            Some(
                "`COM1.log` is a reserved DOS device name, and ral refuses reserved device names as file names"
            )
        );
    }
}
