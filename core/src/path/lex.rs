//! Lexical path resolution: a sigil-expanded string becomes an absolute,
//! `.`/`..`-free path against a scoped cwd, with no filesystem access.
//!
//! Containment and name identity live in [`super::identity`]; device names in
//! [`super::device`].

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use super::PathRules;
use crate::host;

/// How a raw path string is rooted, judged on the string alone.  A UNC or
/// verbatim-UNC root is recognised only with a server and a share both named:
/// bare, its component list folds empty, a prefix of every path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Root {
    /// `/x`: rooted by one separator, no drive.
    Posix,
    /// `\\server\share` with either separator, both names non-empty.
    Unc,
    /// `\\?\C:\x`, with either separator.
    VerbatimDrive,
    /// `\\?\UNC\server\share`, with either separator, both names non-empty.
    VerbatimUnc,
    /// `C:\x` or `C:/x`.
    Drive,
    /// Opens with two separators but is no well-formed UNC or verbatim root:
    /// a missing server or share, a `?` or `.` server, a device path.
    Malformed,
    /// Opens with two separators of different spellings, `/\` or `\/`, and
    /// no verbatim head.  Windows reads either as a separator, so to it this
    /// is a UNC or device head like any doubled one; POSIX reads `/\x` as
    /// rooted at `/` and `\/x` as relative.  One spelling, two paths: kept
    /// apart from [`Unc`](Self::Unc) and [`Malformed`](Self::Malformed),
    /// whose spellings each platform reads alike, so a grant can refuse it.
    MixedHead,
    /// `\x`: rooted by a backslash, but on no drive, so Windows anchors it
    /// to the current one.
    Driveless,
    /// `C:x`: a drive but no root on it, so Windows anchors it to that
    /// drive's current directory.
    DriveRelative,
    /// Neither root nor drive: `x`, `./x`, the empty string.
    Relative,
}

/// The [`Root`] of `path`.  String logic rather than `std::path::Path`, whose
/// absoluteness rule is fixed at compile time to the build target, so a grant
/// can be classified from any host.
pub(crate) fn root(path: &str) -> Root {
    if let Some((Namespace::Verbatim, tail)) = namespace_head(path, Heads::Written) {
        return verbatim_root(tail);
    }
    if let Some(rest) = path.strip_prefix(r"\\").or_else(|| path.strip_prefix("//")) {
        return if server_share(rest.split(|c| PathRules::Windows.is_separator(c))) {
            Root::Unc
        } else {
            Root::Malformed
        };
    }
    match path.as_bytes() {
        [b'/', b'\\', ..] | [b'\\', b'/', ..] => Root::MixedHead,
        [b'/', ..] => Root::Posix,
        [b'\\', ..] => Root::Driveless,
        [d, b':', b'/' | b'\\', ..] if d.is_ascii_alphabetic() => Root::Drive,
        [d, b':', ..] if d.is_ascii_alphabetic() => Root::DriveRelative,
        _ => Root::Relative,
    }
}

/// Whether `path` is absolute under `rules`: what that platform's kernel
/// would answer, judged on the raw string as [`root`] is, so either
/// platform's answer can be read from any host.
///
/// POSIX: whatever opens with `/`, `//`-headed spellings included, which
/// the kernel anchors at the root like any other.  Windows: a drive root, a
/// UNC share, or a namespace head, `\\?\` or `\\.\`; every path opening
/// with two separators, of either spelling, is UNC or device to Windows, so
/// a [`Root::Malformed`] or [`Root::MixedHead`] one is absolute there too.
/// Whether such a root is *well formed*, and read alike on both platforms,
/// is a separate question, those variants', which a grant asks first.
///
/// [`anchor`] and [`path_dirs_absolute`](super::which::path_dirs_absolute),
/// through [`is_absolute`], and [`join_under`], rely on a
/// [`Root::Malformed`] or [`Root::MixedHead`] path counting as absolute
/// under Windows: the kernel never reads one against a cwd, so neither may
/// they, joining it onto one or dropping it as relative.
pub(crate) fn absolute_under(path: &str, rules: PathRules) -> bool {
    match rules {
        PathRules::Posix => path.starts_with('/'),
        PathRules::Windows => matches!(
            root(path),
            Root::Unc
                | Root::VerbatimDrive
                | Root::VerbatimUnc
                | Root::Drive
                | Root::Malformed
                | Root::MixedHead
        ),
    }
}

/// Whether `path` opens with a drive, rooted on it or not: `C:\x`, `C:x`.
pub(crate) fn has_drive(path: &str) -> bool {
    matches!(root(path), Root::Drive | Root::DriveRelative)
}

/// Whether `path` names a network share: `\\server\share`, or its verbatim
/// spelling `\\?\UNC\server\share`.
#[cfg(any(windows, test))]
#[allow(
    clippy::disallowed_methods,
    reason = "native: a path the OS spelled, classified by its head, which is ASCII and so survives a lossy decode"
)]
pub(crate) fn is_network(path: &Path) -> bool {
    matches!(root(&path.to_string_lossy()), Root::Unc | Root::VerbatimUnc)
}

/// The namespace a head opens: past one, Windows takes the rest of a path
/// without its usual normalisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Namespace {
    /// `\\?\`: verbatim, the rest handed on uninterpreted.
    Verbatim,
    /// `\\.\`: the Win32 device namespace.
    Device,
    /// `\??\`: the NT object namespace's DOS-device directory.
    Object,
}

/// Which spellings of a [namespace head](namespace_head) a reading accepts:
/// the two readings Windows itself gives a head, by who spelled the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Heads {
    /// A path the OS handed back: backslashes only, and all three heads.
    /// Windows normalises a `//?/` head's separators as it does any path's,
    /// so no such spelling comes back, and `/` past a head is no separator.
    OsReturned,
    /// A path ral was given, a grant or an access: each separator of a head
    /// read as Windows rules read one, `/` or `\`, so `//?/` and every mix
    /// between open a verbatim path too; the Win32 heads `\\?\` and `\\.\`
    /// alone.  The one reading [`root`], the identity fold and the discard
    /// device share, so a grant and an access spelled with different
    /// separators cannot part company over it.
    Written,
}

/// The [`Namespace`] `path` opens with under `heads`, and what follows its
/// head.
pub(crate) fn namespace_head(path: &str, heads: Heads) -> Option<(Namespace, &str)> {
    let sep = |b: &u8| match heads {
        Heads::OsReturned => *b == b'\\',
        Heads::Written => PathRules::Windows.is_separator(char::from(*b)),
    };
    let namespace = match path.as_bytes() {
        [a, b, b'?', c, ..] if [a, b, c].into_iter().all(sep) => Namespace::Verbatim,
        [a, b, b'.', c, ..] if [a, b, c].into_iter().all(sep) => Namespace::Device,
        [b'\\', b'?', b'?', b'\\', ..] if heads == Heads::OsReturned => Namespace::Object,
        _ => return None,
    };
    Some((namespace, &path[4..]))
}

/// Whether `path` is a volume GUID path, `\\?\Volume{…}\` under any
/// [namespace head](namespace_head) the OS returns, as a volume mounted in a
/// folder is the reparse target of that folder.
#[cfg(any(windows, test))]
#[allow(
    clippy::disallowed_methods,
    reason = "native: a path the OS spelled, classified by its head, which is ASCII and so survives a lossy decode"
)]
pub(crate) fn is_volume_guid(path: &Path) -> bool {
    namespace_head(&path.to_string_lossy(), Heads::OsReturned)
        .and_then(|(_, rest)| rest.get(..7))
        .is_some_and(|v| v.eq_ignore_ascii_case("Volume{"))
}

/// What follows the `UNC` and separator opening a [`Heads::Written`]
/// verbatim tail: the `server\share\…` of `\\?\UNC\server\share\…`, `UNC` in
/// any case.
pub(crate) fn verbatim_unc_tail(tail: &str) -> Option<&str> {
    match tail.split_at_checked(4) {
        Some((unc, rest))
            if unc.as_bytes()[..3].eq_ignore_ascii_case(b"UNC")
                && unc.ends_with(|c| PathRules::Windows.is_separator(c)) =>
        {
            Some(rest)
        }
        _ => None,
    }
}

/// [`root`] of a [`Heads::Written`] verbatim tail.
fn verbatim_root(tail: &str) -> Root {
    if let Some(rest) = verbatim_unc_tail(tail) {
        return if server_share(rest.split(|c| PathRules::Windows.is_separator(c))) {
            Root::VerbatimUnc
        } else {
            Root::Malformed
        };
    }
    let mut parts = tail.split(|c| PathRules::Windows.is_separator(c));
    match parts.next() {
        Some(d) if is_drive(d) && parts.next().is_some() => Root::VerbatimDrive,
        _ => Root::Malformed,
    }
}

fn is_drive(s: &str) -> bool {
    matches!(s.as_bytes(), [d, b':'] if d.is_ascii_alphabetic())
}

/// A non-empty server that is not `?` or `.`, then a non-empty share.
fn server_share<'a>(mut parts: impl Iterator<Item = &'a str>) -> bool {
    parts
        .next()
        .is_some_and(|server| !matches!(server, "" | "?" | "."))
        && parts.next().is_some_and(|share| !share.is_empty())
}

/// Resolve `path` against `cwd`, or against the process cwd when `cwd` is
/// `None`, folding `.` and `..`.  Purely lexical — no symlink resolution —
/// so the answer can differ from `canonicalize`.
pub(crate) fn resolve_path(cwd: Option<&Path>, path: &str) -> PathBuf {
    anchor(cwd, &PathBuf::from(path))
}

/// [`resolve_path`] for a path already on the host's terms, which needs no
/// lifting from a string.
pub(crate) fn anchor(cwd: Option<&Path>, input: &Path) -> PathBuf {
    let joined = if is_absolute(input) {
        input.to_path_buf()
    } else if let Some(cwd) = cwd {
        cwd.join(input)
    } else if let Some(cwd) = host::cwd() {
        cwd.join(input)
    } else {
        input.to_path_buf()
    };

    let normalized = fold_dots(&joined);
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

/// True iff `path` is the filesystem root and names no drive under `rules`:
/// one separator alone, so `/` under either rules, and `\` under Windows',
/// where it is the same root spelled with the other separator.  Under POSIX
/// rules `\` is a relative file name, and reading it as the root would widen
/// a relative spelling to every path.
///
/// Windows has no one root.  `\` there is *drive-relative*, so resolving or
/// canonicalising it anchors it to whichever drive the process happens to be
/// running from — while a grant prefix naming the root means "everywhere",
/// the ceiling a policy attenuates down from.  The two readings part company
/// on exactly this path and nowhere else, so the doors that must not anchor
/// it ask here rather than matching a root apiece.
///
/// String logic, as [`root`] is,
/// and exact rather than "all separators": `\\` opens a UNC name and `X:\`
/// carries a prefix component, and neither is universal.  A drive root must
/// keep anchoring as it always did, or a grant over one volume would widen
/// to all of them.
pub(crate) fn is_bare_root(path: &str, rules: PathRules) -> bool {
    let mut chars = path.chars();
    chars.next().is_some_and(|c| rules.is_separator(c)) && chars.next().is_none()
}

/// Fold `.`/`..` lexically under the host's rules, touching neither
/// filesystem nor cwd: [`fold_dots_under`] on a path the host holds.  The
/// kernel [`resolve_path`] and [`super::canon::canonicalise_lenient`] share.
pub(crate) fn fold_dots(path: &Path) -> PathBuf {
    let folded = fold_bytes(path.as_os_str().as_encoded_bytes(), PathRules::HOST);
    // SAFETY: `folded` is `path`'s head, its ASCII separators perhaps
    // respelled as other ASCII separators, and some of its names, each an
    // unmodified run of its encoded bytes cut at an ASCII separator, joined
    // by ASCII separators.  A cut at an ASCII byte cannot land inside a
    // multi-byte UTF-8/WTF-8 sequence, and an ASCII byte between two runs
    // keeps a surrogate half from meeting another, so `folded` is encoded
    // as validly as `path` was.
    PathBuf::from(unsafe { OsStr::from_encoded_bytes_unchecked(&folded) })
}

/// Fold `.`/`..` out of `path` lexically under `rules`, its head meaning
/// what it did.
///
/// The head is the root `rules` read; past it, `.` and empty names drop,
/// each `..` takes the name before it, and the names that remain join with
/// the separator of `rules`.  A `..` with no name to take survives only on
/// a path with no root, which may yet be joined onto a cwd; past a root it
/// is dropped, since the root has no parent (`/a/../../x` folds to `/x`).
/// Under Windows rules a relative path whose first name is drive-shaped
/// keeps a `.\` before it, so `a\..\C:\x` folds to `.\C:\x`, a name under
/// the cwd as it was, not a drive.
///
/// Under Windows rules the head is what [`root`] reads, so the fold keeps
/// it: `\\srv\sh\..` stays on the share, and a [`Root::Malformed`] or
/// [`Root::MixedHead`] path is head through to its end and comes back as
/// written.  Such a head has no extent ral could cut a tail from: past `\\`
/// with no share, any fold may assemble a server and a share (`\\\srv\sh`
/// would fold to `\\srv\sh`), or, past a verbatim head, a drive, so the
/// kernel reads it, not ral.  A verbatim path spelled exactly `\\?\`,
/// backslashes only, as Win32 alone reads one verbatim, is head through to
/// its end too: Win32 hands the rest to the kernel uninterpreted, `/` a byte
/// of a name and `.` and `..` names, so the fold leaves it as written
/// (`\\?\C:\a/b\..` stays so).  Spelled with a `/` anywhere in its head (`//?/C:/x`), it is a
/// device path Win32 normalises as it does any other: its [`root`] is
/// still verbatim, its head through the drive or share is kept as written,
/// and the tail past it folds.  Under Posix rules the head is the one root,
/// `/`, which a doubled `//` spells too.
///
/// A head is spelled with the separator of `rules`, as its names are,
/// wherever that keeps both its [`root`] and what Windows reads it as: a
/// [`Root::Drive`] (`C:/` folds to `C:\`), a [`Root::Unc`] (`//srv/sh` to
/// `\\srv\sh`) and a [`Root::Driveless`] one, so such a path folds to one
/// separator throughout.  Every other head is kept as written: `/` is a
/// [`Root::Posix`] head, which `\` would make [`Root::Driveless`]; a
/// verbatim head is verbatim to Win32 only as `\\?\`, and `//?/` a device
/// path it normalises; a malformed or mixed head means what its exact
/// spelling does.
///
/// A string, read under either rules from any host: the form a test proves
/// the head kept in, and the one a guest path, `/`-separated whatever the
/// host, is folded in by
/// [`FrozenPath::from_guest`](super::FrozenPath::from_guest).
pub(crate) fn fold_dots_under(path: &str, rules: PathRules) -> String {
    String::from_utf8(fold_bytes(path.as_bytes(), rules))
        .expect("a fold cuts UTF-8 only at ASCII separators and joins it with them")
}

/// [`fold_dots_under`] on a path's encoded bytes.
fn fold_bytes(path: &[u8], rules: PathRules) -> Vec<u8> {
    let Head {
        len,
        rooted,
        respell,
    } = head(path, rules);
    let (head, tail) = path.split_at(len);
    let mut names: Vec<&[u8]> = Vec::new();
    for name in tail.split(|&b| rules.is_separator(char::from(b))) {
        match name {
            b"" | b"." => {}
            b".." if names.last().is_some_and(|last| *last != b"..") => {
                names.pop();
            }
            b".." if rooted => {}
            _ => names.push(name),
        }
    }
    let mut folded: Vec<u8> = if respell {
        head.iter()
            .map(|&b| {
                if rules.is_separator(char::from(b)) {
                    separator(rules)
                } else {
                    b
                }
            })
            .collect()
    } else {
        head.to_vec()
    };
    // A relative path whose first name opens with a drive would be read as
    // on that drive; `.\` keeps it a name, as Windows itself spells one.
    if rules == PathRules::Windows
        && head.is_empty()
        && names
            .first()
            .is_some_and(|first| matches!(first, [d, b':', ..] if d.is_ascii_alphabetic()))
    {
        folded.extend_from_slice(b".\\");
    }
    for (i, name) in names.into_iter().enumerate() {
        if i > 0 {
            folded.push(separator(rules));
        }
        folded.extend_from_slice(name);
    }
    folded
}

/// The separator names join with under `rules`.
fn separator(rules: PathRules) -> u8 {
    match rules {
        PathRules::Posix => b'/',
        PathRules::Windows => b'\\',
    }
}

/// Where a path's head ends, in bytes, whether it is a root, and whether its
/// separators may be respelled: past a root a `..` with no name to take is
/// dropped, past a drive or nothing kept.  A head that has a root separator
/// ends with it, so a name follows it directly; a share with nothing after
/// it ends at the share.
struct Head {
    len: usize,
    rooted: bool,
    /// Whether the head, spelled with the separator of its rules, keeps its
    /// [`Root`] and its meaning (see [`fold_dots_under`]).
    respell: bool,
}

/// The [`Head`] of `path` under `rules`.
fn head(path: &[u8], rules: PathRules) -> Head {
    let PathRules::Windows = rules else {
        let rooted = path.first() == Some(&b'/');
        return Head {
            len: usize::from(rooted),
            rooted,
            respell: true,
        };
    };
    // Classified on a lossy decode: every byte a kind turns on is ASCII, and
    // a replacement character is neither a separator, `?`, `.`, nor a drive
    // letter, nor empty, so the kind is the one the bytes have.  Extents are
    // measured on the bytes themselves.
    let text = String::from_utf8_lossy(path);
    // `\\?\` exactly, the backslash-only reading: verbatim to Win32, read
    // by the kernel as written.
    if let Some((Namespace::Verbatim, _)) = namespace_head(&text, Heads::OsReturned) {
        return Head {
            len: path.len(),
            rooted: true,
            respell: false,
        };
    }
    let kind = root(&text);
    let next_separator = |from: usize| {
        path.get(from..)
            .and_then(|rest| rest.iter().position(|&b| rules.is_separator(char::from(b))))
            .map_or(path.len(), |i| from + i)
    };
    // Through the share, and the separator after it if one follows.
    let through_share =
        |server: usize| (next_separator(next_separator(server) + 1) + 1).min(path.len());
    let len = match kind {
        Root::Relative => 0,
        Root::Posix | Root::Driveless => 1,
        Root::DriveRelative => 2,
        Root::Drive => 3,
        Root::VerbatimDrive => r"\\?\C:\".len(),
        Root::Unc => through_share(r"\\".len()),
        Root::VerbatimUnc => through_share(r"\\?\UNC\".len()),
        Root::Malformed | Root::MixedHead => path.len(),
    };
    Head {
        len,
        rooted: !matches!(kind, Root::Relative | Root::DriveRelative),
        respell: matches!(
            kind,
            Root::Drive | Root::Unc | Root::Driveless | Root::DriveRelative | Root::Relative
        ),
    }
}

/// Whether `path` is absolute to the host's kernel, yet `std::path` finds no
/// prefix in it: under Windows rules, a doubled-separator head in which std
/// finds neither a share nor a namespace (`\\x`, `//`, `\\\srv\sh`, `/\x`).
/// `std::path` reads such a path as rooted on the current drive, so a step
/// that descends or climbs it by its components would move it there, onto
/// a path the kernel never reads it as; such steps refuse it.  Never true
/// off Windows, where every absolute path opens with the one root.
pub(crate) fn is_unparsed_head(path: &Path) -> bool {
    PathRules::HOST == PathRules::Windows
        && is_absolute(path)
        && !matches!(path.components().next(), Some(Component::Prefix(_)))
}

/// A subpath that would not join *onto* its base but replace it, as
/// `Path::join` lets it: one naming a volume of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rerooting;

/// `sub`, split on the separators of `rules`, joined onto `base` under
/// `rules`, a string read alike from any host.
///
/// The names join with the separator of `rules`, where `base.join(sub)` on
/// Windows would leave `C:\x\ral/rc`, and where a host `Path::join` would
/// read them under the host's rules, not the given ones.  An empty
/// component, from a leading, doubled or trailing separator or an empty
/// `sub`, adds nothing, so a root of the current volume, `/x`, or `\x` under
/// Windows, joins under `base`.  `.` and `..` join as written, for the
/// resolver to fold.  A separator goes between `base` and the first name
/// unless `base` is empty, ends with one, or, under Windows rules, is a bare
/// drive: `C:` is that drive's current directory, and a name in it is
/// spelled `C:x`, as Windows spells one there.
///
/// # Errors
/// [`Rerooting`] when `sub` names a volume of its own, which only Windows
/// rules have: `sub` is [absolute](absolute_under) there, opening with two
/// separators as a share or a namespace head does (`\\srv\sh`, `//srv/sh`,
/// `\\?\UNC\srv\sh`, `\\.\pipe\x`), or a drive root; or a component at any
/// position carries a drive (`C:`, `C:x`), which re-roots a join even once
/// separators are split out.  Posix rules refuse nothing: every root there
/// is the one root, `//x` included, and `C:` a name.
pub(crate) fn join_under(base: &str, sub: &str, rules: PathRules) -> Result<String, Rerooting> {
    let mut joined = base.to_owned();
    joined.push_str(&appended(base.as_bytes(), sub, rules)?);
    Ok(joined)
}

/// [`join_under`] under the host's rules, onto a path the host holds, whose
/// name need not be UTF-8.
///
/// # Errors
/// [`join_under`]'s.
pub(crate) fn join_relative(base: impl AsRef<Path>, sub: &str) -> Result<PathBuf, Rerooting> {
    let base = base.as_ref().as_os_str();
    let mut joined = base.to_os_string();
    joined.push(appended(base.as_encoded_bytes(), sub, PathRules::HOST)?);
    Ok(PathBuf::from(joined))
}

/// What [`join_under`] appends to `base`: each name of `sub`, a separator
/// of `rules` before it where one is due.
fn appended(base: &[u8], sub: &str, rules: PathRules) -> Result<String, Rerooting> {
    let names = || {
        sub.split(|c| rules.is_separator(c))
            .filter(|c| !c.is_empty())
    };
    if rules == PathRules::Windows
        && (absolute_under(sub, PathRules::Windows) || names().any(has_drive))
    {
        return Err(Rerooting);
    }
    let mut open = match base {
        [] => true,
        [d, b':'] if rules == PathRules::Windows => d.is_ascii_alphabetic(),
        [.., last] => rules.is_separator(char::from(*last)),
    };
    let mut tail = String::new();
    for name in names() {
        if !open {
            tail.push(char::from(separator(rules)));
        }
        tail.push_str(name);
        open = false;
    }
    Ok(tail)
}

/// `sub` spelled relative, as [`join_under`] joins a subpath it accepts
/// onto a base: its leading separators under `rules` come off, so `/etc`
/// names `etc` under the base, never `/etc`.  Under Posix a leading
/// backslash is a filename byte, and stays.
pub(crate) fn relative_suffix(sub: &str, rules: PathRules) -> &str {
    sub.trim_start_matches(|c| rules.is_separator(c))
}

/// [`join_relative`] for a subpath the program spells, `/`-separated
/// whatever the host: being constant, it is relative or a bug.
///
/// # Panics
/// If `sub` re-roots, which a relative constant cannot.
pub fn join_slash(base: impl AsRef<Path>, sub: &'static str) -> PathBuf {
    join_relative(base, sub)
        .unwrap_or_else(|Rerooting| panic!("the constant subpath {sub:?} re-roots its join"))
}

/// `base` with `.ext` appended: `build.ps1` + `EXE` is `build.ps1.EXE`,
/// `ledger.json` + `tmp` is `ledger.json.tmp`.
///
/// Concatenation on the `OsStr`, never `Path::with_extension`, which replaces
/// an extension the name already has rather than adding one.
pub fn with_appended_suffix(base: &Path, ext: &str) -> PathBuf {
    let mut name = base.as_os_str().to_os_string();
    name.push(".");
    name.push(ext);
    PathBuf::from(name)
}

/// The counterpart of [`with_appended_suffix`]: `path` less a trailing
/// `.ext`, `ledger.json.tmp` less `tmp` being `ledger.json`; `None` when the
/// name does not end in `.ext` after a non-empty stem.
///
/// One extension of `ext` at a time, by `Path::extension`, so the stem is cut
/// on the `OsStr` and a name that is not UTF-8 strips like any other.
pub fn strip_appended_suffix(path: &Path, ext: &str) -> Option<PathBuf> {
    ext.rsplit('.').try_fold(path.to_path_buf(), |stem, piece| {
        (stem.extension()? == piece).then(|| stem.with_extension(""))
    })
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

/// Whether `path` is absolute on this host: [`absolute_under`] the host's
/// rules, the one absoluteness the path module answers with, a path the
/// host holds or a string alike.
#[allow(
    clippy::disallowed_methods,
    reason = "native: classified by its head, which is ASCII and so survives a lossy decode"
)]
pub fn is_absolute(path: impl AsRef<OsStr>) -> bool {
    absolute_under(&path.as_ref().to_string_lossy(), PathRules::HOST)
}

/// A path that is not UTF-8, refused where a path must travel as text: a
/// record field, a serialized source origin.  Shows as the sentence naming
/// it, for the caller to head with what it was for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotUtf8(PathBuf);

impl std::fmt::Display for NotUtf8 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is not UTF-8", super::shown(&self.0))
    }
}

impl std::error::Error for NotUtf8 {}

/// `path`'s native spelling as text, when it is UTF-8.
///
/// The one door for a path that must travel as a string and be lifted back
/// unchanged, so a name no string can hold is refused where it enters, not
/// lost to a lossy decode or a serializer's late refusal.
///
/// # Errors
/// [`NotUtf8`], naming `path`.
#[allow(
    clippy::disallowed_methods,
    reason = "native: the UTF-8 check itself; the text is the path's native spelling, lifted back unchanged"
)]
pub fn utf8(path: &Path) -> Result<&str, NotUtf8> {
    path.to_str().ok_or_else(|| NotUtf8(path.to_path_buf()))
}

/// Final component of `path`, falling back to `path` itself when there is no
/// file name or it is not UTF-8.  For callers that key on a command basename
/// (exit hints, login-shell detection).
pub fn basename(path: &str) -> &str {
    #[allow(
        clippy::disallowed_methods,
        reason = "path-form: this is the `basename` helper the ban names"
    )]
    let name = Path::new(path).file_name();
    #[allow(
        clippy::disallowed_methods,
        reason = "native: a name that is not UTF-8 falls back to the whole path"
    )]
    let name = name.and_then(|n| n.to_str());
    name.unwrap_or(path)
}

/// The extension of `path`'s final component, as `Path::extension` reads it
/// under the host's rules: after its last `.`, `None` for a name with none
/// or a dotfile such as `.pdf`.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: an extension, read lexically off the name, and a `str` again since it is a slice of one"
)]
pub fn extension(path: &str) -> Option<&str> {
    Path::new(path).extension().and_then(|e| e.to_str())
}

/// Whether the path component `name` names something besides an entry of
/// its directory under the host's rules.
///
/// On Windows a `:` makes it a drive (`C:`) or an alternate data stream of
/// some other file (`a.txt:s`); elsewhere a `:` is a filename byte, and
/// nothing does.
pub fn is_stream_or_drive(name: &OsStr) -> bool {
    PathRules::HOST == PathRules::Windows && name.as_encoded_bytes().contains(&b':')
}

/// Proper ancestors of `paths`, sorted, dedup'd across inputs, root excluded.
pub(crate) fn proper_ancestors<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut out = std::collections::BTreeSet::new();
    for path in paths {
        #[allow(
            clippy::disallowed_methods,
            reason = "path-form: ancestors are a lexical walk up the name; nothing is resolved"
        )]
        for ancestor in Path::new(path).ancestors().skip(1) {
            if ancestor == Path::new("/") || ancestor.as_os_str().is_empty() {
                break;
            }
            out.insert(super::shown(ancestor));
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
    use Root::*;

    /// One spelling of every [`Root`], and of every way to miss one.
    const ROOTS: &[(&str, Root)] = &[
        ("/x", Posix),
        ("/", Posix),
        (r"\\srv\sh", Unc),
        (r"\\srv\sh\x", Unc),
        ("//srv/sh/x", Unc),
        (r"\\?\C:\x", VerbatimDrive),
        (r"\\?\C:\", VerbatimDrive),
        ("//?/C:/x", VerbatimDrive),
        (r"\\?\UNC\srv\sh", VerbatimUnc),
        (r"\\?\UNC\srv\sh\x", VerbatimUnc),
        (r"\\?\unc\srv\sh", VerbatimUnc),
        ("//?/UNC/srv/sh", VerbatimUnc),
        (r"\/?\C:\x", VerbatimDrive),
        (r"/\?/unc\srv/sh", VerbatimUnc),
        (r"C:\x", Drive),
        ("c:/x", Drive),
        (r"\\", Malformed),
        ("//", Malformed),
        ("///x", Malformed),
        (r"\\srv", Malformed),
        (r"\\srv\", Malformed),
        ("//srv/", Malformed),
        (r"\\\srv\sh", Malformed),
        ("///srv/sh", Malformed),
        (r"\\?\", Malformed),
        (r"\\?", Malformed),
        (r"\\?\UNC\", Malformed),
        (r"\\?\UNC\srv", Malformed),
        (r"\\?\UNC\srv\", Malformed),
        (r"\\?\UNC\?\sh", Malformed),
        (r"\\?\UNC\.\sh", Malformed),
        ("//?/UNC/", Malformed),
        ("//?/UNC/srv", Malformed),
        (r"\\?\C:", Malformed),
        (r"\\?\x", Malformed),
        (r"\\.\C:\x", Malformed),
        (r"\\.\pipe\x", Malformed),
        (r"\\?\?\x", Malformed),
        (r"/\x", MixedHead),
        (r"/\srv\sh", MixedHead),
        (r"\/srv/sh", MixedHead),
        (r"/\.\pipe\x", MixedHead),
        (r"\x", Driveless),
        (r"\", Driveless),
        ("C:x", DriveRelative),
        ("C:", DriveRelative),
        ("x", Relative),
        ("./x", Relative),
        ("", Relative),
    ];

    /// Platform-independent: classification is on the raw string.
    #[test]
    fn roots_classify_on_the_raw_string() {
        for &(raw, want) in ROOTS {
            assert_eq!(root(raw), want, "{raw}");
        }
    }

    /// Platform-independent, as [`roots_classify_on_the_raw_string`] is: the
    /// kernel's answer under each rules set, root by root.  POSIX roots
    /// whatever opens with `/`; Windows whatever opens with two separators,
    /// well formed or not, and a drive root.
    #[test]
    fn absoluteness_is_read_under_either_rules() {
        for (raw, posix, windows) in [
            ("/x", true, false),
            ("/", true, false),
            ("//srv/sh/x", true, true),
            (r"\\srv\sh\x", false, true),
            ("//?/C:/x", true, true),
            (r"\\?\C:\x", false, true),
            (r"/\?\C:\x", true, true),
            ("//?/UNC/srv/sh", true, true),
            (r"\\?\UNC\srv\sh", false, true),
            (r"C:\x", false, true),
            ("c:/x", false, true),
            ("//", true, true),
            ("///x", true, true),
            ("//srv", true, true),
            (r"\\", false, true),
            (r"\\srv", false, true),
            (r"\\?\x", false, true),
            ("//?/x", true, true),
            (r"\\.\pipe\x", false, true),
            (r"\\.\C:\x", false, true),
            (r"/\x", true, true),
            (r"/\srv\sh", true, true),
            (r"\/srv/sh", false, true),
            (r"\x", false, false),
            (r"\", false, false),
            ("C:x", false, false),
            ("C:", false, false),
            ("x", false, false),
            ("./x", false, false),
            ("", false, false),
        ] {
            assert_eq!(absolute_under(raw, PathRules::Posix), posix, "{raw} posix");
            assert_eq!(
                absolute_under(raw, PathRules::Windows),
                windows,
                "{raw} windows"
            );
        }
    }

    /// The host's answer is [`absolute_under`] the host's rules, for a
    /// string and a held path alike.
    #[test]
    fn is_absolute_is_the_hosts_rule() {
        for raw in [
            "/x",
            "//?/C:/x",
            r"\\srv\sh\x",
            r"C:\x",
            r"\\.\pipe\x",
            r"\x",
            "x",
        ] {
            let host = absolute_under(raw, PathRules::HOST);
            assert_eq!(is_absolute(raw), host, "{raw}");
            assert_eq!(is_absolute(PathBuf::from(raw)), host, "{raw}");
        }
    }

    /// The written reading: two separators, the mark, a separator, each
    /// separator either spelling; the Win32 heads alone.
    #[test]
    fn written_heads_take_either_separator() {
        use Namespace::*;
        for (raw, want) in [
            (r"\\?\C:\x", Verbatim),
            ("//?/C:/x", Verbatim),
            (r"\/?/C:\x", Verbatim),
            (r"\\.\nul", Device),
            ("//./nul", Device),
            (r"/\.\nul", Device),
        ] {
            assert_eq!(
                namespace_head(raw, Heads::Written),
                Some((want, &raw[4..])),
                "{raw}"
            );
        }
        for raw in [r"\\?", r"\?\x", r"\??\C:\x", "C:/?/x"] {
            assert_eq!(namespace_head(raw, Heads::Written), None, "{raw}");
        }
    }

    /// The OS-returned reading: backslashes only, the NT object head too.
    #[test]
    fn os_returned_heads_take_backslashes_only() {
        use Namespace::*;
        for (raw, want) in [
            (r"\\?\C:\x", Verbatim),
            (r"\\.\pipe\x", Device),
            (r"\??\C:\x", Object),
        ] {
            assert_eq!(
                namespace_head(raw, Heads::OsReturned),
                Some((want, &raw[4..])),
                "{raw}"
            );
        }
        for raw in ["//?/C:/x", r"\/?/C:\x", "//./nul", "/??/C:/x", r"\\?"] {
            assert_eq!(namespace_head(raw, Heads::OsReturned), None, "{raw}");
        }
    }

    #[test]
    fn a_verbatim_unc_tail_opens_with_unc() {
        assert_eq!(verbatim_unc_tail(r"UNC\srv\sh"), Some(r"srv\sh"));
        assert_eq!(verbatim_unc_tail("unc/srv"), Some("srv"));
        assert_eq!(verbatim_unc_tail(r"UNCx\srv"), None);
        assert_eq!(verbatim_unc_tail(r"C:\x"), None);
    }

    #[test]
    fn network_paths_are_the_unc_roots() {
        for raw in [r"\\srv\sh\x", r"\\?\UNC\srv\sh\x", r"\\?\unc\srv\sh"] {
            assert!(is_network(Path::new(raw)), "{raw}");
        }
        for raw in [r"\\?\C:\x", r"\\.\Volume{1234}\x", r"\\.\C:\x", r"C:\x"] {
            assert!(!is_network(Path::new(raw)), "{raw}");
        }
    }

    #[test]
    fn volume_guid_paths_open_with_a_literal_head() {
        for raw in [
            r"\\?\Volume{1234}\",
            r"\??\volume{1234}\",
            r"\\.\Volume{1234}\x",
        ] {
            assert!(is_volume_guid(Path::new(raw)), "{raw}");
        }
        for raw in [
            r"\\?\D:\x",
            r"\\?\VolumeX",
            r"D:\Volume{1}",
            "//?/Volume{1}/",
        ] {
            assert!(!is_volume_guid(Path::new(raw)), "{raw}");
        }
    }

    #[test]
    fn an_extension_follows_the_final_dot_of_the_final_name() {
        for (raw, want) in [
            ("report.pdf", Some("pdf")),
            ("a/b/chart.Png", Some("Png")),
            ("Invoice.pdf.lnk", Some("lnk")),
            ("sub.pdf/inner", None),
            (".pdf", None),
            ("Makefile", None),
        ] {
            assert_eq!(extension(raw), want, "{raw}");
        }
    }

    #[test]
    fn only_windows_reads_a_colon_as_a_stream_or_drive() {
        for raw in ["C:", "a.txt:s"] {
            assert_eq!(is_stream_or_drive(raw.as_ref()), cfg!(windows), "{raw}");
        }
        assert!(!is_stream_or_drive("a.txt".as_ref()));
    }

    #[test]
    fn a_suffix_is_appended_never_substituted() {
        assert_eq!(
            with_appended_suffix(Path::new("ledger.json"), "tmp"),
            pb("ledger.json.tmp")
        );
        assert_eq!(with_appended_suffix(Path::new("x"), "crash"), pb("x.crash"));
    }

    #[test]
    fn a_stripped_suffix_inverts_the_appended_one() {
        for (base, ext) in [("ledger.json", "tmp"), ("x", "crash"), ("a", "tar.gz")] {
            let appended = with_appended_suffix(Path::new(base), ext);
            assert_eq!(strip_appended_suffix(&appended, ext), Some(pb(base)));
        }
        assert_eq!(strip_appended_suffix(Path::new("x.lock"), "tmp"), None);
        assert_eq!(strip_appended_suffix(Path::new(".lock"), "lock"), None);
    }

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// `b` and `parts`, joined with the separator of `rules`.
    fn under_b(parts: &[&str], rules: PathRules) -> String {
        let separator = String::from(char::from(separator(rules)));
        std::iter::once("b")
            .chain(parts.iter().copied())
            .collect::<Vec<_>>()
            .join(&separator)
    }

    /// Accepted joins under either rules: separators split, empties drop,
    /// so a single leading separator roots nothing, and `.`/`..` stay for
    /// the resolver's fold.
    #[test]
    fn a_subpath_joins_component_by_component() {
        for rules in [PathRules::Posix, PathRules::Windows] {
            for (sub, want) in [
                ("", &[][..]),
                ("x", &["x"]),
                ("/x", &["x"]),
                ("x//y/", &["x", "y"]),
                ("../x", &["..", "x"]),
                ("./x", &[".", "x"]),
            ] {
                assert_eq!(
                    join_under("b", sub, rules),
                    Ok(under_b(want, rules)),
                    "{sub:?} under {rules:?}"
                );
            }
        }
    }

    /// Off Windows a backslash and a colon are filename bytes: `C:\x` is one
    /// component, and every root is the one root, so nothing is refused.
    #[test]
    fn posix_rules_take_backslash_and_colon_as_bytes() {
        for (sub, want) in [
            (r"C:\x", &[r"C:\x"][..]),
            (r"\x", &[r"\x"]),
            (r"\\?\C:\x", &[r"\\?\C:\x"]),
            ("a/C:", &["a", "C:"]),
            ("//x", &["x"]),
            ("//srv/sh", &["srv", "sh"]),
            ("//?/UNC/srv/sh", &["?", "UNC", "srv", "sh"]),
        ] {
            assert_eq!(
                join_under("b", sub, PathRules::Posix),
                Ok(under_b(want, PathRules::Posix)),
                "{sub:?}"
            );
        }
    }

    /// Under Windows rules a drive is refused at the head or after a
    /// separator, a verbatim drive head included.
    #[test]
    fn windows_rules_refuse_a_drive_at_any_position() {
        for sub in [
            r"C:\x",
            "C:/x",
            "C:x",
            "C:",
            r"a\C:\x",
            "a/c:",
            r"\\?\C:\x",
            r"?\C:\x",
            "//?/C:/x",
        ] {
            assert_eq!(
                join_under("b", sub, PathRules::Windows),
                Err(Rerooting),
                "{sub:?}"
            );
        }
    }

    /// Under Windows rules a head of two separators names a share or a
    /// namespace, well formed or not, and is refused rather than flattened
    /// into components under the base.
    #[test]
    fn windows_rules_refuse_a_share_or_namespace_head() {
        for sub in [
            r"\\srv\sh\x",
            "//srv/sh/x",
            r"\\?\UNC\srv\sh",
            "//?/UNC/srv/sh",
            r"\\.\pipe\x",
            "//x",
            r"\\",
            r"\/srv/sh",
            r"/\srv\sh",
            r"/\x",
        ] {
            assert_eq!(
                join_under("b", sub, PathRules::Windows),
                Err(Rerooting),
                "{sub:?}"
            );
        }
    }

    /// Under Windows rules a root of the current volume, by either
    /// separator, joins under the base.
    #[test]
    fn windows_rules_keep_a_driveless_root_under_the_base() {
        for (sub, want) in [
            (r"\x", &["x"][..]),
            ("/x", &["x"]),
            (r"\?\UNC\srv", &["?", "UNC", "srv"]),
        ] {
            assert_eq!(
                join_under("b", sub, PathRules::Windows),
                Ok(under_b(want, PathRules::Windows)),
                "{sub:?}"
            );
        }
    }

    /// The join is the given rules' on every host, never the host's: under
    /// Posix rules `C:` is a name, which a host `Path::join` on Windows would
    /// read as a drive replacing the base, and `\` a byte of one.
    #[test]
    fn a_join_under_foreign_rules_is_the_same_on_every_host() {
        for (base, sub, want) in [
            ("b", "C:", "b/C:"),
            ("b", r"C:\x", r"b/C:\x"),
            ("/", "C:/x", "/C:/x"),
            (r"b\", "x", r"b\/x"),
            ("", "x", "x"),
        ] {
            assert_eq!(
                join_under(base, sub, PathRules::Posix),
                Ok(want.to_owned()),
                "{base:?} + {sub:?}"
            );
        }
        for (base, sub, want) in [
            ("b", "x/y", r"b\x\y"),
            ("b/", "x", r"b/x"),
            (r"C:\", "x", r"C:\x"),
            ("C:", "x", "C:x"),
            ("/", "x", "/x"),
            ("", "x", "x"),
        ] {
            assert_eq!(
                join_under(base, sub, PathRules::Windows),
                Ok(want.to_owned()),
                "{base:?} + {sub:?}"
            );
        }
    }

    /// A host path joins as a string joins under the host's rules.
    #[test]
    fn a_host_join_is_the_string_join_under_the_hosts_rules() {
        for sub in ["", "x", "/x", "x//y/", "../x", r"C:\x", "a/C:", "//srv/sh"] {
            assert_eq!(
                join_relative("b", sub),
                join_under("b", sub, PathRules::HOST).map(PathBuf::from),
                "{sub:?}"
            );
        }
    }

    // `/` has no parent, so a `..` that reaches the root is dropped.
    #[cfg(unix)]
    #[test]
    fn fold_dots_drops_dotdot_at_root() {
        assert_eq!(fold_dots(Path::new("/..")), pb("/"));
        assert_eq!(fold_dots(Path::new("/a/../../x")), pb("/x"));
        assert_eq!(fold_dots(Path::new("/../..")), pb("/"));
    }

    // A `..` a relative path cannot pop survives for a later cwd join, and
    // is never taken by another.
    #[test]
    fn fold_dots_keeps_leading_dotdot_on_relative_path() {
        assert_eq!(fold_dots(Path::new("../x")), pb("../x"));
        assert_eq!(fold_dots(Path::new("a/../../x")), pb("../x"));
        assert_eq!(fold_dots(Path::new("../../a")), pb("../../a"));
    }

    /// The guest's fold, pinned on every host: a guest path folds to a guest
    /// path, separator intact, where a fold under Windows rules would join
    /// with `\`.
    #[test]
    fn a_posix_fold_keeps_the_guests_separator() {
        for (raw, want) in [
            ("/work", "/work"),
            ("/work/./drafts/../letters", "/work/letters"),
            ("/work/", "/work"),
            ("/", "/"),
            ("//x", "/x"),
            ("/..", "/"),
            ("/a/../../x", "/x"),
            ("../x", "../x"),
            ("a/../../x", "../x"),
            (r"a\..\x", r"a\..\x"),
        ] {
            assert_eq!(fold_dots_under(raw, PathRules::Posix), want, "{raw}");
        }
    }

    /// Under Windows rules the names past the head join with `\`, and so
    /// does a drive, share or driveless head; a Posix or slash-spelled
    /// verbatim head is kept as written, and a malformed or mixed one, or an
    /// exact `\\?\` verbatim path, kept whole.
    #[test]
    fn a_windows_fold_respells_a_head_only_where_that_keeps_it() {
        for (raw, want) in [
            (r"C:\a\..\b", r"C:\b"),
            ("C:/a/./b/", r"C:\a\b"),
            ("C:/", r"C:\"),
            (r"C:\..", r"C:\"),
            (r"C:a\..\..\b", r"C:..\b"),
            (r"\\srv\sh\..\x", r"\\srv\sh\x"),
            (r"\\srv\sh", r"\\srv\sh"),
            ("//srv/sh/a/../x", r"\\srv\sh\x"),
            (r"\\srv/sh", r"\\srv\sh"),
            (r"\\?\C:\a\..", r"\\?\C:\a\.."),
            (r"\\?\C:\a/b", r"\\?\C:\a/b"),
            (r"\\?\C:\a\.\b\", r"\\?\C:\a\.\b\"),
            ("//?/C:/a/b", r"//?/C:/a\b"),
            ("//?/C:/a/../b/.", r"//?/C:/b"),
            (r"\/?\C:\a/..\b", r"\/?\C:\b"),
            ("//?/UNC/srv/sh/a", r"//?/UNC/srv/sh/a"),
            (r"\\?\UNC\srv\sh\a\..\..", r"\\?\UNC\srv\sh\a\..\.."),
            (r"\\?\UNC\srv\sh/a", r"\\?\UNC\srv\sh/a"),
            ("/a/../x", "/x"),
            ("/a/b", r"/a\b"),
            (r"\a\..\x", r"\x"),
            ("//x/../y", "//x/../y"),
            (r"\\\srv\sh\..\x", r"\\\srv\sh\..\x"),
            (r"/\x\..\y", r"/\x\..\y"),
            (r"\\.\pipe\a\..\b", r"\\.\pipe\a\..\b"),
            (r"a\..\C:\x", r".\C:\x"),
            (r".\C:\x", r".\C:\x"),
        ] {
            assert_eq!(fold_dots_under(raw, PathRules::Windows), want, "{raw}");
        }
    }

    /// The fold keeps the root its rules read, and is its own fixed point:
    /// every root of [`ROOTS`], bare and under each tail, folds to a path of
    /// that root, which folds to itself.  Under Windows rules the root is
    /// [`root`]'s, and the fold spells it as [`fold_dots_under`] says, root
    /// by root: with `\` alone, or with its head as written; under Posix
    /// rules it is the one root or none, since POSIX reads `//x` as `/x`.  So
    /// [`LexicalPath`](super::super::LexicalPath)'s invariant, a fixed point
    /// of [`fold_dots`], holds of whatever the resolver hands it.
    #[test]
    fn a_fold_keeps_the_root_under_either_rules() {
        let tails = ["", "/", "/x", r"\.\y\..", "/../..", r"\a\..\C:\z", "/./?/x"];
        for &(raw, _) in ROOTS {
            for tail in tails {
                let path = format!("{raw}{tail}");
                for rules in [PathRules::Posix, PathRules::Windows] {
                    let folded = fold_dots_under(&path, rules);
                    match rules {
                        PathRules::Windows => {
                            assert_eq!(root(&folded), root(&path), "{path:?} -> {folded:?}");
                            let Head { len, respell, .. } = head(path.as_bytes(), rules);
                            if respell {
                                assert!(!folded.contains('/'), "{path:?} -> {folded:?}");
                            } else {
                                assert!(folded.starts_with(&path[..len]), "{path:?} -> {folded:?}");
                            }
                        }
                        PathRules::Posix => assert_eq!(
                            absolute_under(&folded, rules),
                            absolute_under(&path, rules),
                            "{path:?} -> {folded:?}"
                        ),
                    }
                    assert_eq!(
                        fold_dots_under(&folded, rules),
                        folded,
                        "{path:?} under {rules:?}"
                    );
                }
            }
        }
    }

    /// The host's fold is the string kernel under the host's rules.
    #[test]
    fn fold_dots_is_the_kernel_under_the_hosts_rules() {
        for &(raw, _) in ROOTS {
            for tail in ["", r"\a\..\b", "/./c/../.."] {
                let path = format!("{raw}{tail}");
                assert_eq!(
                    fold_dots(Path::new(&path)),
                    pb(&fold_dots_under(&path, PathRules::HOST)),
                    "{path:?}"
                );
            }
        }
    }

    /// Off Windows a name the host holds need not be UTF-8, and folds as
    /// any other: its bytes are kept, past `..` and `.` alike.
    #[cfg(unix)]
    #[test]
    fn fold_dots_keeps_a_name_that_is_not_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let raw = Path::new(OsStr::from_bytes(b"/a/\xff/./b/../c"));
        let want = Path::new(OsStr::from_bytes(b"/a/\xff/c"));
        assert_eq!(fold_dots(raw), want);
    }

    /// `std::path` finds no prefix in a doubled-separator head that is no
    /// share or namespace, and would read it as on the current drive.
    #[test]
    fn an_unparsed_head_is_one_std_reads_on_the_current_drive() {
        for raw in [r"\\x", "//", r"\\\srv\sh", r"/\x"] {
            assert_eq!(is_unparsed_head(Path::new(raw)), cfg!(windows), "{raw}");
        }
        for raw in [
            r"C:\x",
            r"\\srv\sh\x",
            r"\\?\C:\x",
            r"\\.\pipe\x",
            "/x",
            "x",
        ] {
            assert!(!is_unparsed_head(Path::new(raw)), "{raw}");
        }
    }
}
