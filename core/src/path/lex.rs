//! Lexical path resolution: a sigil-expanded string becomes an absolute,
//! `.`/`..`-free path against a scoped cwd, with no filesystem access.
//!
//! Containment and name identity live in [`super::identity`]; device names in
//! [`super::device`].

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use super::{CwdPath, PathRules};

/// Whether `path` is absolute under `rules`: answers both whether `path` is
/// fully qualified under `rules` and whether [`anchor`] must not join `path`
/// onto `cwd`, judged on the raw bytes via [`parse_bytes`] so either
/// platform's answer can be read from any host.
///
/// POSIX: whatever opens with `/`, `//`-headed spellings included, which
/// the kernel anchors at the root like any other.  Windows: a drive root, a
/// UNC share, or a device or malformed head ([`HeadKind::Device`],
/// [`HeadKind::Malformed`]); every path opening with two separators, of
/// either spelling, is UNC or device to Windows, so device and malformed
/// heads count as absolute there too.  Whether such a head is eligible as a
/// grant is a separate question, which `guard::freeze` asks first.
///
/// [`anchor`] and [`path_dirs_absolute`](super::which::path_dirs_absolute),
/// through [`is_absolute`], and [`join_under`], rely on a
/// [`HeadKind::Device`] or [`HeadKind::Malformed`] path counting as absolute
/// under Windows: the kernel never reads one against a cwd, so neither may
/// they, joining it onto one or dropping it as relative.
pub(crate) fn absolute_under(path: impl AsRef<OsStr>, rules: PathRules) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), rules).is_absolute()
}

/// Whether `path` opens with a drive, rooted on it or not: `C:\x`, `C:x`.
pub(crate) fn has_drive(path: impl AsRef<OsStr>) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), PathRules::Windows).has_drive()
}

/// Whether `path` names a network share: `\\server\share`, or its verbatim
/// spelling `\\?\UNC\server\share`.
#[cfg(any(windows, test))]
pub(crate) fn is_network(path: impl AsRef<OsStr>) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), PathRules::Windows).is_network()
}

/// Whether `path` is a verbatim drive path (`\\?\C:\x`).
#[cfg(any(windows, test))]
pub(crate) fn is_verbatim_drive(path: impl AsRef<OsStr>) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), PathRules::Windows).is_verbatim_drive()
}

/// Whether `path` is a volume GUID path, `\\?\Volume{…}\` under any
/// exact backslash namespace head (`\\?\`, `\\.\`, `\??\`) the OS returns, as
/// a volume mounted in a folder is the reparse target of that folder.
#[cfg(any(windows, test))]
pub(crate) fn is_volume_guid(path: impl AsRef<OsStr>) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), PathRules::Windows).is_volume_guid()
}

/// The 4-byte Windows namespace prefix a path opens with.
///
/// Device (`\\.\`) and exact verbatim (`\\?\`) share no normalisation rule:
/// Win32 normalises device paths and hands exact `\\?\` paths to the kernel
/// uninterpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Namespace {
    /// `\\?\` (any `/` or `\` separator spelling): verbatim prefix.
    /// `exact` is true when all three separator bytes are `b'\\'`.
    Verbatim { exact: bool },
    /// `\\.\` (any `/` or `\` separator spelling): Win32 device namespace.
    /// `exact` is true when all three separator bytes are `b'\\'`.
    Device { exact: bool },
    /// `\??\` (backslashes only): NT object manager DOS-device directory.
    Object,
}

impl Namespace {
    /// Whether all three separator bytes of the 4-byte namespace prefix are `b'\\'`.
    pub(crate) const fn is_exact(self) -> bool {
        match self {
            Self::Verbatim { exact } | Self::Device { exact } => exact,
            Self::Object => true,
        }
    }
}

/// Separator and namespace spelling of a parsed head, recorded once from the
/// opening bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeadSpelling {
    /// Present when the path opens with a 4-byte namespace prefix (`\\?\`,
    /// `\\.\`, `\??\`) under `PathRules::Windows`. Always `None` under
    /// `PathRules::Posix`.
    pub(crate) namespace: Option<Namespace>,
    /// True when the first two bytes are different separators (`/\\` or `\\/`).
    pub(crate) mixed_opening: bool,
    /// True when the first byte is `b'/'`.
    pub(crate) opens_with_slash: bool,
}

/// Structural head classification under a given [`PathRules`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadKind<'a> {
    /// `/x` or `/`: any path starting with `/` under `PathRules::Posix`, or a
    /// single leading `/` under `PathRules::Windows`.
    Posix,
    /// `C:\x`, `c:/x`, or verbatim `\\?\C:\x`, `//?/C:/x`, `/\?\C:\x`.
    /// `volume` is the slice through the drive colon (`C:` or `\\?\C:`).
    Drive { letter: u8, volume: &'a [u8] },
    /// `\\srv\sh`, `//srv/sh`, or verbatim `\\?\UNC\srv\sh`, `//?/UNC/srv/sh`.
    /// Both `server` and `share` are non-empty, neither `.` nor `..`, and
    /// `server != "?"`. `volume` is the slice through `share` (`\\srv\sh` or
    /// `\\?\UNC\srv\sh`).
    Unc { volume: &'a [u8] },
    /// `\x` or `\`: single leading backslash under `PathRules::Windows`
    /// (`\??\...` included, which carries `spelling.namespace == Some(Namespace::Object)`).
    Driveless,
    /// `C:x` or `C:`: ASCII drive letter and colon with no following separator
    /// under `PathRules::Windows`.
    DriveRelative { letter: u8 },
    /// `x`, `./x`, `""`: neither root nor drive.
    Relative,
    /// `\\.\...` (with any `/` or `\` separators): Win32 device namespace.
    Device,
    /// Opens with two separators (or a `\\?\` prefix) that is no well-formed
    /// `Drive`, `Unc`, or `Device`.
    Malformed,
}

/// A path slice parsed once under `rules`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Parsed<'a> {
    pub(crate) raw: &'a [u8],
    pub(crate) rules: PathRules,
    pub(crate) kind: HeadKind<'a>,
    pub(crate) spelling: HeadSpelling,
    /// Byte offset where the head ends and the foldable component tail begins.
    /// Equal to `raw.len()` when the path is opaque to lexical folding.
    pub(crate) head_len: usize,
}

/// Parse `raw`'s head and separator spelling once under `rules`.
pub(crate) fn parse_bytes(raw: &[u8], rules: PathRules) -> Parsed<'_> {
    let (kind, spelling) = match rules {
        PathRules::Posix => parse_posix(raw),
        PathRules::Windows => parse_windows(raw),
    };
    let head_len = match kind {
        HeadKind::Posix | HeadKind::Driveless => 1,
        HeadKind::DriveRelative { .. } => 2,
        HeadKind::Relative => 0,
        HeadKind::Device | HeadKind::Malformed => raw.len(),
        HeadKind::Drive { .. } | HeadKind::Unc { .. }
            if spelling.namespace.is_some_and(Namespace::is_exact) =>
        {
            raw.len()
        }
        HeadKind::Drive { volume, .. } | HeadKind::Unc { volume } => {
            (volume.len() + 1).min(raw.len())
        }
    };
    Parsed {
        raw,
        rules,
        kind,
        spelling,
        head_len,
    }
}

fn parse_posix(raw: &[u8]) -> (HeadKind<'_>, HeadSpelling) {
    let opens_with_slash = raw.first() == Some(&b'/');
    let kind = if opens_with_slash {
        HeadKind::Posix
    } else {
        HeadKind::Relative
    };
    (
        kind,
        HeadSpelling {
            namespace: None,
            mixed_opening: false,
            opens_with_slash,
        },
    )
}

fn parse_windows(raw: &[u8]) -> (HeadKind<'_>, HeadSpelling) {
    let opens_with_slash = raw.first() == Some(&b'/');
    let mixed_opening = matches!(raw, [b'/', b'\\', ..] | [b'\\', b'/', ..]);
    match raw {
        [b'\\', b'?', b'?', b'\\', ..] => (
            HeadKind::Driveless,
            HeadSpelling {
                namespace: Some(Namespace::Object),
                mixed_opening: false,
                opens_with_slash: false,
            },
        ),
        [a, b, b'.', c, ..] if is_win_sep(*a) && is_win_sep(*b) && is_win_sep(*c) => (
            HeadKind::Device,
            HeadSpelling {
                namespace: Some(Namespace::Device {
                    exact: [*a, *b, *c] == [b'\\'; 3],
                }),
                mixed_opening,
                opens_with_slash,
            },
        ),
        [a, b, b'?', c, tail @ ..] if is_win_sep(*a) && is_win_sep(*b) && is_win_sep(*c) => {
            let spelling = HeadSpelling {
                namespace: Some(Namespace::Verbatim {
                    exact: [*a, *b, *c] == [b'\\'; 3],
                }),
                mixed_opening,
                opens_with_slash,
            };
            let kind = if let Some(unc_rest) = verbatim_unc_tail_bytes(tail)
                && server_share_bytes(unc_rest.split(|&b| is_win_sep(b)))
            {
                HeadKind::Unc {
                    volume: &raw[..share_end(raw, r"\\?\UNC\".len())],
                }
            } else if let [d, b':', sep, ..] = tail
                && d.is_ascii_alphabetic()
                && is_win_sep(*sep)
            {
                HeadKind::Drive {
                    letter: *d,
                    volume: &raw[..r"\\?\C:".len()],
                }
            } else {
                HeadKind::Malformed
            };
            (kind, spelling)
        }
        [a, b, rest @ ..] if is_win_sep(*a) && is_win_sep(*b) => {
            let spelling = HeadSpelling {
                namespace: None,
                mixed_opening,
                opens_with_slash,
            };
            let kind = if !mixed_opening && server_share_bytes(rest.split(|&b| is_win_sep(b))) {
                HeadKind::Unc {
                    volume: &raw[..share_end(raw, r"\\".len())],
                }
            } else {
                HeadKind::Malformed
            };
            (kind, spelling)
        }
        _ => {
            let spelling = HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash,
            };
            let kind = match raw {
                [b'/', ..] => HeadKind::Posix,
                [b'\\', ..] => HeadKind::Driveless,
                [d, b':', sep, ..] if d.is_ascii_alphabetic() && is_win_sep(*sep) => {
                    HeadKind::Drive {
                        letter: *d,
                        volume: &raw[..2],
                    }
                }
                [d, b':', ..] if d.is_ascii_alphabetic() => HeadKind::DriveRelative { letter: *d },
                _ => HeadKind::Relative,
            };
            (kind, spelling)
        }
    }
}

fn is_sep_byte(rules: PathRules, b: u8) -> bool {
    rules.is_separator(char::from(b))
}

fn is_win_sep(b: u8) -> bool {
    is_sep_byte(PathRules::Windows, b)
}

fn verbatim_unc_tail_bytes(tail: &[u8]) -> Option<&[u8]> {
    let [u, n, c, sep, rest @ ..] = tail else {
        return None;
    };
    ([*u, *n, *c].eq_ignore_ascii_case(b"UNC") && is_win_sep(*sep)).then_some(rest)
}

fn server_share_bytes<'a>(mut parts: impl Iterator<Item = &'a [u8]>) -> bool {
    let named = |name: &[u8]| !matches!(name, b"" | b"." | b"..");
    parts
        .next()
        .is_some_and(|server| named(server) && server != b"?")
        && parts.next().is_some_and(named)
}

impl<'a> Parsed<'a> {
    /// What follows a 4-byte Windows namespace prefix (`\\?\`, `\\.\`, `\??\`),
    /// when `self.spelling.namespace` is `Some`.
    pub(crate) fn namespace_tail(&self) -> Option<&'a [u8]> {
        self.spelling.namespace.map(|_| &self.raw[4..])
    }

    /// Answers two questions that agree for all current callers: whether `self`
    /// is fully qualified under `self.rules`, and whether `anchor` must not
    /// join `self` onto `cwd` (`Device` and `Malformed` count as absolute so
    /// `anchor` never joins them onto `cwd`).
    pub(crate) fn is_absolute(&self) -> bool {
        match self.rules {
            PathRules::Posix => matches!(self.kind, HeadKind::Posix),
            PathRules::Windows => matches!(
                self.kind,
                HeadKind::Drive { .. }
                    | HeadKind::Unc { .. }
                    | HeadKind::Device
                    | HeadKind::Malformed
            ),
        }
    }

    /// Whether `self` is a valid working directory (`CwdPath`) under `self.rules`.
    pub(crate) fn is_working_directory(&self) -> bool {
        match self.rules {
            PathRules::Posix => matches!(self.kind, HeadKind::Posix),
            PathRules::Windows => {
                matches!(self.kind, HeadKind::Drive { .. } | HeadKind::Unc { .. })
            }
        }
    }

    /// Whether `self` is a single bare root separator (`/` under Posix;
    /// `/` or `\` under Windows).
    pub(crate) fn is_bare_root(&self) -> bool {
        self.raw.len() == 1 && matches!(self.kind, HeadKind::Posix | HeadKind::Driveless)
    }

    /// Whether `self` opens with an ordinary (non-verbatim) drive (`C:\x` or `C:x`).
    pub(crate) fn has_drive(&self) -> bool {
        self.spelling.namespace.is_none()
            && matches!(
                self.kind,
                HeadKind::Drive { .. } | HeadKind::DriveRelative { .. }
            )
    }

    /// Whether `self` names a UNC share (`\\srv\sh` or `\\?\UNC\srv\sh`).
    #[cfg(any(windows, test))]
    pub(crate) fn is_network(&self) -> bool {
        matches!(self.kind, HeadKind::Unc { .. })
    }

    /// Whether `self` is a verbatim drive path (`\\?\C:\x`).
    #[cfg(any(windows, test))]
    pub(crate) fn is_verbatim_drive(&self) -> bool {
        matches!(self.spelling.namespace, Some(Namespace::Verbatim { .. }))
            && matches!(self.kind, HeadKind::Drive { .. })
    }

    /// Whether `self` opens with an exact backslash namespace head
    /// (`\\?\`, `\\.\`, `\??\`).
    pub(crate) fn has_os_namespace_head(&self) -> bool {
        self.spelling.namespace.is_some_and(Namespace::is_exact)
    }

    /// Whether `self` opens with two different separators (`/\\` or `\\/`)
    /// outside the `\\?\` verbatim namespace: a head Windows reads as UNC or
    /// device and POSIX reads as `/` followed by a `\` component or a
    /// relative `\` name.
    pub(crate) fn is_mixed_head(&self) -> bool {
        matches!(self.kind, HeadKind::Device | HeadKind::Malformed)
            && self.spelling.mixed_opening
            && !matches!(self.spelling.namespace, Some(Namespace::Verbatim { .. }))
    }

    /// Whether `self` opens with `\??\`, the NT object manager's DOS-device
    /// directory.
    pub(crate) fn is_nt_object(&self) -> bool {
        self.kind == HeadKind::Driveless && self.spelling.namespace == Some(Namespace::Object)
    }

    /// Whether `self` is a volume GUID path (`\\?\Volume{...}`,
    /// `\\.\Volume{...}`, `\??\Volume{...}`).
    #[cfg(any(windows, test))]
    pub(crate) fn is_volume_guid(&self) -> bool {
        self.has_os_namespace_head()
            && self
                .namespace_tail()
                .and_then(|t| t.get(..b"Volume{".len()))
                .is_some_and(|v| v.eq_ignore_ascii_case(b"Volume{"))
    }

    /// The drive letter of a `Drive` working directory.
    pub(crate) fn drive_letter(&self) -> Option<u8> {
        match self.kind {
            HeadKind::Drive { letter, .. } => Some(letter),
            _ => None,
        }
    }

    /// The volume slice (`C:`, `\\?\C:`, `\\srv\sh`, `\\?\UNC\srv\sh`) of a
    /// `Drive` or `Unc` head.
    pub(crate) fn volume(&self) -> Option<&'a [u8]> {
        match self.kind {
            HeadKind::Drive { volume, .. } | HeadKind::Unc { volume } => Some(volume),
            _ => None,
        }
    }

    /// Whether `self`'s head is a root that absorbs leading `..` during folding.
    pub(crate) fn is_rooted(&self) -> bool {
        !matches!(
            self.kind,
            HeadKind::Relative | HeadKind::DriveRelative { .. }
        )
    }

    /// Whether `fold_bytes` may respell separators inside `&self.raw[..self.head_len]`.
    pub(crate) fn respell_head(&self) -> bool {
        match self.rules {
            PathRules::Posix => true,
            PathRules::Windows => {
                self.spelling.namespace.is_none()
                    && matches!(
                        self.kind,
                        HeadKind::Drive { .. }
                            | HeadKind::Unc { .. }
                            | HeadKind::Driveless
                            | HeadKind::DriveRelative { .. }
                            | HeadKind::Relative
                    )
            }
        }
    }

    /// `self.raw` past any Windows verbatim prefix (`\\?\` or `\\?\UNC\`), so
    /// `\\?\C:\work` yields `C:\work` and `\\?\UNC\srv\sh\x` yields `srv\sh\x`.
    pub(crate) fn without_verbatim_prefix(&self) -> &'a [u8] {
        match self.namespace_tail() {
            Some(tail) if matches!(self.spelling.namespace, Some(Namespace::Verbatim { .. })) => {
                verbatim_unc_tail_bytes(tail).unwrap_or(tail)
            }
            _ => self.raw,
        }
    }
}

/// Resolve `path` against `cwd`, folding `.` and `..`: [`anchor`] on a
/// string.
pub(crate) fn resolve_path(cwd: &CwdPath, path: &str) -> PathBuf {
    anchor(cwd, &PathBuf::from(path))
}

/// `input` made absolute against `cwd` as the host reads it, then folded.
/// Purely lexical — no symlink resolution — so the answer can differ from
/// `canonicalize`.
///
/// An absolute `input` is kept, and a relative one joins `cwd`.  Under
/// Windows rules two more spellings are relative to less than a whole path,
/// and are read as Win32 reads them, `cwd` standing for the process's
/// current directory, as ral's logical cwd does:
///
/// - root-relative, `\x` or `/x`, lands on `cwd`'s drive or share;
/// - drive-relative, `D:x`, joins `cwd` when `cwd` is on `D:`.  On another
///   drive it joins that drive's own current directory, which Windows keeps
///   per process, apart from ral's, in its `=D:` variable, and which only
///   Win32 can answer: [`std::path::absolute`] asks it.  Where that gives no
///   absolute answer, the drive's root stands in, as it does in Win32 when
///   `=D:` is unset.
///
/// `cwd` is on a well-formed root ([`CwdPath`]), and each reading keeps it,
/// so the result is absolute, and the fold, which keeps the root it reads,
/// keeps it so: what [`LexicalPath`](super::LexicalPath) asserts.
pub(crate) fn anchor(cwd: &CwdPath, input: &Path) -> PathBuf {
    let joined = if is_absolute(input) {
        input.to_path_buf()
    } else {
        against(cwd, input)
    };
    fold_dots(&joined)
}

/// `input`, relative under the host's rules, read against `cwd` as
/// [`anchor`] describes.
fn against(cwd: &CwdPath, input: &Path) -> PathBuf {
    let bytes = input.as_os_str().as_encoded_bytes();
    if PathRules::HOST == PathRules::Windows {
        match parse_bytes(bytes, PathRules::Windows).kind {
            HeadKind::Driveless | HeadKind::Posix => return joined(volume(cwd), bytes),
            HeadKind::DriveRelative { .. } => return drive_relative(cwd, input),
            _ => {}
        }
    }
    joined(cwd.as_path().as_os_str().as_encoded_bytes(), bytes)
}

/// `D:x` read as [`anchor`] describes, `cwd` a [`CwdPath`] under Windows
/// rules.
fn drive_relative(cwd: &CwdPath, input: &Path) -> PathBuf {
    let (drive, rest) = input.as_os_str().as_encoded_bytes().split_at(2);
    let cwd = cwd.as_path().as_os_str().as_encoded_bytes();
    if drive_letter(cwd).is_some_and(|letter| letter.eq_ignore_ascii_case(&drive[0])) {
        return joined(cwd, rest);
    }
    std::path::absolute(input)
        .ok()
        .filter(|answer| is_absolute(answer))
        .unwrap_or_else(|| joined(&[drive[0], b':', b'\\'], rest))
}

/// The drive a [`CwdPath`] under Windows rules is on, as its letter: that
/// of `C:\x` or `\\?\C:\x`, none for a share.
fn drive_letter(cwd: &[u8]) -> Option<u8> {
    parse_bytes(cwd, PathRules::Windows).drive_letter()
}

/// The drive or share a [`CwdPath`] under Windows rules is on, as spelled
/// and with no separator after it: `C:`, `\\srv\sh`, `\\?\C:`,
/// `\\?\UNC\srv\sh`.
fn volume(cwd: &CwdPath) -> &[u8] {
    parse_bytes(
        cwd.as_path().as_os_str().as_encoded_bytes(),
        PathRules::Windows,
    )
    .volume()
    .expect("CwdPath is on a drive or a share under Windows rules")
}

/// `base`, then `rest`, a separator of the host's between them unless one
/// already stands there or `rest` is empty.
fn joined(base: &[u8], rest: &[u8]) -> PathBuf {
    let is_separator = |b: Option<&u8>| b.is_some_and(|&b| is_sep_byte(PathRules::HOST, b));
    let mut out = base.to_vec();
    if !rest.is_empty() && !is_separator(base.last()) && !is_separator(rest.first()) {
        out.push(separator(PathRules::HOST));
    }
    out.extend_from_slice(rest);
    // SAFETY: `base` and `rest` are each a whole encoded path or a run of one
    // cut at an ASCII byte, which cannot land inside a multi-byte
    // UTF-8/WTF-8 sequence; and an ASCII separator stands at the seam,
    // ending `base`, opening `rest` or put between them, unless `rest` is
    // empty, so no surrogate half meets another, and `out` is encoded as
    // validly as its parts.
    PathBuf::from(unsafe { OsStr::from_encoded_bytes_unchecked(&out) })
}

/// Whether `path` can be a working directory under the host's rules: what
/// [`CwdPath`] checks.  POSIX: absolute.  Windows: a drive root, a share, or
/// the verbatim spelling of either, the roots with an extent a name joins
/// past without changing what the root names.  A malformed or mixed head has
/// none: `\\srv` joined with `sh` would name the share `\\srv\sh`.
pub(crate) fn is_working_directory(path: impl AsRef<OsStr>) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), PathRules::HOST).is_working_directory()
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
/// Byte logic, as [`parse_bytes`] is,
/// and exact rather than "all separators": `\\` opens a UNC name and `X:\`
/// carries a prefix component, and neither is universal.  A drive root must
/// keep anchoring as it always did, or a grant over one volume would widen
/// to all of them.
pub(crate) fn is_bare_root(path: impl AsRef<OsStr>, rules: PathRules) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), rules).is_bare_root()
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
/// Under Windows rules the head is what [`parse_bytes`] reads, so the fold keeps
/// it: `\\srv\sh\..` stays on the share, and a [`HeadKind::Device`] or
/// [`HeadKind::Malformed`] path is head through to its end and comes back as
/// written.  Such a head has no extent ral could cut a tail from: past `\\`
/// with no share, any fold may assemble a server and a share (`\\\srv\sh`
/// would fold to `\\srv\sh`), or, past a verbatim head, a drive, so the
/// kernel reads it, not ral.  A verbatim path spelled exactly `\\?\`,
/// backslashes only, as Win32 alone reads one verbatim, is head through to
/// its end too: Win32 hands the rest to the kernel uninterpreted, `/` a byte
/// of a name and `.` and `..` names, so the fold leaves it as written
/// (`\\?\C:\a/b\..` stays so).  Spelled with a `/` anywhere in its head (`//?/C:/x`), it is a
/// device path Win32 normalises as it does any other: its [`HeadKind`] is
/// still [`Drive`](HeadKind::Drive) or [`Unc`](HeadKind::Unc) under
/// [`Namespace::Verbatim`], its head through the drive or share is kept as
/// written, and the tail past it folds.  Under Posix rules the head is the one
/// root, `/`, which a doubled `//` spells too.
///
/// A head is spelled with the separator of `rules`, as its names are,
/// wherever that keeps both its [`HeadKind`] and what Windows reads it as: a
/// non-verbatim [`HeadKind::Drive`] (`C:/` folds to `C:\`), a non-verbatim
/// [`HeadKind::Unc`] (`//srv/sh` to `\\srv\sh`) and a non-namespace
/// [`HeadKind::Driveless`] one, so such a path folds to one
/// separator throughout.  Every other head is kept as written: `/` is a
/// [`HeadKind::Posix`] head, which `\` would make [`HeadKind::Driveless`]; a
/// verbatim head is verbatim to Win32 only as `\\?\`, and `//?/` a device
/// path it normalises; a device, malformed or mixed head means what its exact
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
    let parsed = parse_bytes(path, rules);
    let rooted = parsed.is_rooted();
    let (head, tail) = path.split_at(parsed.head_len);
    let mut names: Vec<&[u8]> = Vec::new();
    for name in tail.split(|&b| is_sep_byte(rules, b)) {
        match name {
            b"" | b"." => {}
            b".." if names.last().is_some_and(|last| *last != b"..") => {
                names.pop();
            }
            b".." if rooted => {}
            _ => names.push(name),
        }
    }
    let mut folded: Vec<u8> = if parsed.respell_head() {
        head.iter()
            .map(|&b| {
                if is_sep_byte(rules, b) {
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

/// Where the share of a UNC head under Windows rules ends, its server name
/// starting at `server`: the end of the name after the server's.
fn share_end(path: &[u8], server: usize) -> usize {
    let next_separator = |from: usize| {
        path.get(from..)
            .and_then(|rest| rest.iter().position(|&b| is_win_sep(b)))
            .map_or(path.len(), |i| from + i)
    };
    next_separator(next_separator(server) + 1)
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
        [.., last] => is_sep_byte(rules, *last),
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
pub fn is_absolute(path: impl AsRef<OsStr>) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), PathRules::HOST).is_absolute()
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
    use HeadKind::*;
    use Namespace::{Device as NsDevice, Object, Verbatim};

    /// One row per head form (`Relative`, `Driveless`, `DriveRelative`,
    /// `Drive`, `Unc`, `Posix`, `Device`, `Malformed`) and separator spelling
    /// (`\\`, `//`, `/\`, `\/`; for 4-byte namespace prefixes all eight
    /// spellings of the three separator slots), plus every incomplete UNC and
    /// verbatim form.  Expectations are literal, never computed from the
    /// parser under test.
    const PARSED_WINDOWS: &[(&str, HeadKind<'static>, HeadSpelling, usize)] = &[
        // Posix
        (
            "/x",
            Posix,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            1,
        ),
        (
            "/",
            Posix,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            1,
        ),
        (
            "/??/C:/x",
            Posix,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            1,
        ),
        // Driveless (plain and NT object manager)
        (
            r"\x",
            Driveless,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            1,
        ),
        (
            r"\",
            Driveless,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            1,
        ),
        (
            r"\?\x",
            Driveless,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            1,
        ),
        (
            r"\?\UNC\srv",
            Driveless,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            1,
        ),
        (
            r"\??\C:\x",
            Driveless,
            HeadSpelling {
                namespace: Some(Object),
                mixed_opening: false,
                opens_with_slash: false,
            },
            1,
        ),
        // DriveRelative
        (
            "C:x",
            DriveRelative { letter: b'C' },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            2,
        ),
        (
            "C:",
            DriveRelative { letter: b'C' },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            2,
        ),
        // Drive (non-verbatim, both separator spellings)
        (
            r"C:\x",
            Drive {
                letter: b'C',
                volume: b"C:",
            },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            3,
        ),
        (
            r"C:\",
            Drive {
                letter: b'C',
                volume: b"C:",
            },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            3,
        ),
        (
            "c:/x",
            Drive {
                letter: b'c',
                volume: b"c:",
            },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            3,
        ),
        (
            "C:/?/x",
            Drive {
                letter: b'C',
                volume: b"C:",
            },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            3,
        ),
        // Unc (non-verbatim, `\\` and `//` openings)
        (
            r"\\srv\sh",
            Unc {
                volume: br"\\srv\sh",
            },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            8,
        ),
        (
            r"\\srv\sh\x",
            Unc {
                volume: br"\\srv\sh",
            },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            9,
        ),
        (
            r"\\srv/sh",
            Unc {
                volume: br"\\srv/sh",
            },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            8,
        ),
        (
            "//srv/sh",
            Unc {
                volume: b"//srv/sh",
            },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            8,
        ),
        (
            "//srv/sh/x",
            Unc {
                volume: b"//srv/sh",
            },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            9,
        ),
        (
            r"//srv\sh\x",
            Unc {
                volume: br"//srv\sh",
            },
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            9,
        ),
        // Verbatim Drive: all 8 spellings of the three separator slots
        (
            r"\\?\C:\x",
            Drive {
                letter: b'C',
                volume: br"\\?\C:",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            8,
        ),
        (
            r"\\?\C:\",
            Drive {
                letter: b'C',
                volume: br"\\?\C:",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            7,
        ),
        (
            r"\\?/C:\x",
            Drive {
                letter: b'C',
                volume: br"\\?/C:",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            7,
        ),
        (
            r"\/?\C:\x",
            Drive {
                letter: b'C',
                volume: br"\/?\C:",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: false,
            },
            7,
        ),
        (
            r"\/?/C:\x",
            Drive {
                letter: b'C',
                volume: br"\/?/C:",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: false,
            },
            7,
        ),
        (
            r"/\?\C:\x",
            Drive {
                letter: b'C',
                volume: br"/\?\C:",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: true,
            },
            7,
        ),
        (
            r"/\?/C:\x",
            Drive {
                letter: b'C',
                volume: br"/\?/C:",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: true,
            },
            7,
        ),
        (
            r"//?\C:\x",
            Drive {
                letter: b'C',
                volume: br"//?\C:",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: false,
                opens_with_slash: true,
            },
            7,
        ),
        (
            "//?/C:/x",
            Drive {
                letter: b'C',
                volume: b"//?/C:",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: false,
                opens_with_slash: true,
            },
            7,
        ),
        // Verbatim Unc: all 8 spellings of the three separator slots
        (
            r"\\?\UNC\srv\sh",
            Unc {
                volume: br"\\?\UNC\srv\sh",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            14,
        ),
        (
            r"\\?\UNC\srv\sh\x",
            Unc {
                volume: br"\\?\UNC\srv\sh",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            16,
        ),
        (
            r"\\?\unc\srv\sh",
            Unc {
                volume: br"\\?\unc\srv\sh",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            14,
        ),
        (
            r"\\?/UNC\srv\sh\x",
            Unc {
                volume: br"\\?/UNC\srv\sh",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            15,
        ),
        (
            r"\/?\UNC\srv\sh\x",
            Unc {
                volume: br"\/?\UNC\srv\sh",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: false,
            },
            15,
        ),
        (
            r"\/?/UNC\srv\sh\x",
            Unc {
                volume: br"\/?/UNC\srv\sh",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: false,
            },
            15,
        ),
        (
            r"/\?\UNC\srv\sh\x",
            Unc {
                volume: br"/\?\UNC\srv\sh",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: true,
            },
            15,
        ),
        (
            r"/\?/unc\srv/sh",
            Unc {
                volume: br"/\?/unc\srv/sh",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: true,
            },
            14,
        ),
        (
            r"//?\UNC\srv\sh\x",
            Unc {
                volume: br"//?\UNC\srv\sh",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: false,
                opens_with_slash: true,
            },
            15,
        ),
        (
            "//?/UNC/srv/sh",
            Unc {
                volume: b"//?/UNC/srv/sh",
            },
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: false,
                opens_with_slash: true,
            },
            14,
        ),
        // Device: all 8 spellings of the three separator slots
        (
            r"\\.\C:\x",
            Device,
            HeadSpelling {
                namespace: Some(NsDevice { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            8,
        ),
        (
            r"\\.\pipe\x",
            Device,
            HeadSpelling {
                namespace: Some(NsDevice { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            10,
        ),
        (
            r"\\.\nul",
            Device,
            HeadSpelling {
                namespace: Some(NsDevice { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            7,
        ),
        (
            r"\\./nul",
            Device,
            HeadSpelling {
                namespace: Some(NsDevice { exact: false }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            7,
        ),
        (
            r"\/.\nul",
            Device,
            HeadSpelling {
                namespace: Some(NsDevice { exact: false }),
                mixed_opening: true,
                opens_with_slash: false,
            },
            7,
        ),
        (
            r"\/./nul",
            Device,
            HeadSpelling {
                namespace: Some(NsDevice { exact: false }),
                mixed_opening: true,
                opens_with_slash: false,
            },
            7,
        ),
        (
            r"/\.\pipe\x",
            Device,
            HeadSpelling {
                namespace: Some(NsDevice { exact: false }),
                mixed_opening: true,
                opens_with_slash: true,
            },
            10,
        ),
        (
            r"/\./nul",
            Device,
            HeadSpelling {
                namespace: Some(NsDevice { exact: false }),
                mixed_opening: true,
                opens_with_slash: true,
            },
            7,
        ),
        (
            r"//.\nul",
            Device,
            HeadSpelling {
                namespace: Some(NsDevice { exact: false }),
                mixed_opening: false,
                opens_with_slash: true,
            },
            7,
        ),
        (
            "//./pipe/x",
            Device,
            HeadSpelling {
                namespace: Some(NsDevice { exact: false }),
                mixed_opening: false,
                opens_with_slash: true,
            },
            10,
        ),
        // Malformed: all 8 spellings of `\\?\x`
        (
            r"\\?\x",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            5,
        ),
        (
            r"\\?/x",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            5,
        ),
        (
            r"\/?\x",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: false,
            },
            5,
        ),
        (
            r"\/?/x",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: false,
            },
            5,
        ),
        (
            r"/\?\x",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: true,
            },
            5,
        ),
        (
            r"/\?/x",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: true,
                opens_with_slash: true,
            },
            5,
        ),
        (
            r"//?\x",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: false,
                opens_with_slash: true,
            },
            5,
        ),
        (
            "//?/x",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: false,
                opens_with_slash: true,
            },
            5,
        ),
        // Malformed: incomplete UNC & verbatim forms and mixed 2-separator openings
        (
            r"\\",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            2,
        ),
        (
            "//",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            2,
        ),
        (
            r"/\",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: true,
                opens_with_slash: true,
            },
            2,
        ),
        (
            r"\/",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: true,
                opens_with_slash: false,
            },
            2,
        ),
        (
            "///x",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            4,
        ),
        (
            r"\\srv",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            5,
        ),
        (
            r"\\srv\",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            6,
        ),
        (
            "//srv",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            5,
        ),
        (
            "//srv/",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            6,
        ),
        (
            r"\\\srv\sh",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            9,
        ),
        (
            "///srv/sh",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            9,
        ),
        (
            r"\\?\",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            4,
        ),
        (
            r"\\?",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            3,
        ),
        (
            r"\\?\UNC\",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            8,
        ),
        (
            r"\\?\UNC\srv",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            11,
        ),
        (
            r"\\?\UNC\srv\",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            12,
        ),
        (
            r"\\?\UNC\?\sh",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            12,
        ),
        (
            r"\\?\UNC\.\sh",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            12,
        ),
        (
            "//?/UNC/",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: false,
                opens_with_slash: true,
            },
            8,
        ),
        (
            "//?/UNC/srv",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: false }),
                mixed_opening: false,
                opens_with_slash: true,
            },
            11,
        ),
        (
            r"\\?\C:",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            6,
        ),
        (
            r"\\?\?\x",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            7,
        ),
        (
            r"\\..\sh",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            7,
        ),
        (
            r"\\srv\.",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            7,
        ),
        (
            r"\\srv\..\x",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            10,
        ),
        (
            "//x/../y",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: true,
            },
            8,
        ),
        (
            r"\\?\UNC\..\sh",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            13,
        ),
        (
            r"\\?\UNC\srv\..",
            Malformed,
            HeadSpelling {
                namespace: Some(Verbatim { exact: true }),
                mixed_opening: false,
                opens_with_slash: false,
            },
            14,
        ),
        (
            r"/\x",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: true,
                opens_with_slash: true,
            },
            3,
        ),
        (
            r"/\srv\sh",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: true,
                opens_with_slash: true,
            },
            8,
        ),
        (
            r"\/srv/sh",
            Malformed,
            HeadSpelling {
                namespace: None,
                mixed_opening: true,
                opens_with_slash: false,
            },
            8,
        ),
        // Relative
        (
            "x",
            Relative,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            0,
        ),
        (
            "./x",
            Relative,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            0,
        ),
        (
            "",
            Relative,
            HeadSpelling {
                namespace: None,
                mixed_opening: false,
                opens_with_slash: false,
            },
            0,
        ),
    ];

    /// Platform-independent: `parse_bytes` classifies `kind`, `spelling`,
    /// `head_len`, and `volume` on the raw byte slice under both `PathRules`.
    #[test]
    fn parse_bytes_classifies_kind_and_spelling_under_both_rules() {
        for &(raw, want_kind, want_spelling, want_head_len) in PARSED_WINDOWS {
            let win = parse_bytes(raw.as_bytes(), PathRules::Windows);
            assert_eq!(
                win,
                Parsed {
                    raw: raw.as_bytes(),
                    rules: PathRules::Windows,
                    kind: want_kind,
                    spelling: want_spelling,
                    head_len: want_head_len,
                },
                "{raw:?} windows"
            );
            let want_vol = match want_kind {
                Drive { volume, .. } | Unc { volume } => Some(volume),
                _ => None,
            };
            assert_eq!(win.volume(), want_vol, "{raw:?} volume");

            let posix = parse_bytes(raw.as_bytes(), PathRules::Posix);
            let want_posix = if want_spelling.opens_with_slash {
                Parsed {
                    raw: raw.as_bytes(),
                    rules: PathRules::Posix,
                    kind: Posix,
                    spelling: HeadSpelling {
                        namespace: None,
                        mixed_opening: false,
                        opens_with_slash: true,
                    },
                    head_len: 1,
                }
            } else {
                Parsed {
                    raw: raw.as_bytes(),
                    rules: PathRules::Posix,
                    kind: Relative,
                    spelling: HeadSpelling {
                        namespace: None,
                        mixed_opening: false,
                        opens_with_slash: false,
                    },
                    head_len: 0,
                }
            };
            assert_eq!(posix, want_posix, "{raw:?} posix");
        }
    }

    /// Platform-independent: the kernel's answer under each rules set, root by
    /// root.  POSIX roots whatever opens with `/`; Windows whatever opens with
    /// two separators, well formed or not, and a drive root.
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

    #[test]
    fn network_paths_are_the_unc_roots() {
        for raw in [r"\\srv\sh\x", r"\\?\UNC\srv\sh\x", r"\\?\unc\srv\sh"] {
            assert!(is_network(Path::new(raw)), "{raw}");
        }
        for raw in [r"\\?\C:\x", r"\\.\Volume{1234}\x", r"\\.\C:\x", r"C:\x"] {
            assert!(!is_network(Path::new(raw)), "{raw}");
        }
        assert!(is_verbatim_drive(Path::new(r"\\?\C:\x")));
        assert!(!is_verbatim_drive(Path::new(r"C:\x")));
        assert!(!is_verbatim_drive(Path::new(r"\\.\C:\x")));
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
            (r"\\srv\.\a\..", r"\\srv\.\a\.."),
            (r"\\\srv\sh\..\x", r"\\\srv\sh\..\x"),
            (r"/\x\..\y", r"/\x\..\y"),
            (r"\\.\pipe\a\..\b", r"\\.\pipe\a\..\b"),
            (r"a\..\C:\x", r".\C:\x"),
            (r".\C:\x", r".\C:\x"),
        ] {
            assert_eq!(fold_dots_under(raw, PathRules::Windows), want, "{raw}");
        }
    }

    /// Compare `before` and `after` parsed heads after folding under `rules`:
    /// `kind`, `volume` (with `/` respelled to `\` when `before.respell_head()`
    /// under Windows rules), and `spelling.namespace` are preserved — save for
    /// [`Namespace::Object`] (`\??\`), which Win32 reads as a [`Driveless`]
    /// path (`head_len == 1`) whose first component is `??`, so `..` climbing
    /// to `\??` or `\` keeps `Driveless` while dropping the 4-byte `\??\`
    /// prefix (`namespace: None`).
    fn assert_fold_preserves_head(before: Parsed<'_>, after: Parsed<'_>, ctx: &str) {
        let want_ns = if before.spelling.namespace == Some(Namespace::Object) {
            after
                .raw
                .starts_with(b"\\??\\")
                .then_some(Namespace::Object)
        } else {
            before.spelling.namespace
        };
        assert_eq!(after.spelling.namespace, want_ns, "{ctx}: namespace");
        let respell_vol = |vol: &[u8]| -> Vec<u8> {
            if before.rules == PathRules::Windows && before.respell_head() {
                vol.iter()
                    .map(|&b| if b == b'/' { b'\\' } else { b })
                    .collect()
            } else {
                vol.to_vec()
            }
        };
        match (before.kind, after.kind) {
            (
                Drive {
                    letter: l1,
                    volume: v1,
                },
                Drive {
                    letter: l2,
                    volume: v2,
                },
            ) => {
                assert_eq!(l2, l1, "{ctx}: drive letter");
                assert_eq!(v2, respell_vol(v1).as_slice(), "{ctx}: drive volume");
            }
            (Unc { volume: v1 }, Unc { volume: v2 }) => {
                assert_eq!(v2, respell_vol(v1).as_slice(), "{ctx}: unc volume");
            }
            (k1, k2) => assert_eq!(k2, k1, "{ctx}: kind"),
        }
    }

    /// The fold keeps the root its rules read, and is its own fixed point:
    /// every head in [`PARSED_WINDOWS`], bare and under each tail, folds to a
    /// path of that head, which folds to itself (for `\??\`, `Driveless` is
    /// kept while `..` climbing to `\??` or `\` drops `Namespace::Object`).  So
    /// [`LexicalPath`](super::super::LexicalPath)'s invariant, a fixed point
    /// of [`fold_dots`], holds of whatever the resolver hands it.
    #[test]
    fn a_fold_keeps_the_root_under_either_rules() {
        let tails = ["", "/", "/x", r"\.\y\..", "/../..", r"\a\..\C:\z", "/./?/x"];
        for &(raw, _, _, _) in PARSED_WINDOWS {
            for tail in tails {
                let path = format!("{raw}{tail}");
                for rules in [PathRules::Posix, PathRules::Windows] {
                    let parsed = parse_bytes(path.as_bytes(), rules);
                    let folded = fold_dots_under(&path, rules);
                    let parsed_folded = parse_bytes(folded.as_bytes(), rules);
                    assert_fold_preserves_head(
                        parsed,
                        parsed_folded,
                        &format!("{path:?} -> {folded:?} ({rules:?})"),
                    );
                    if rules == PathRules::Windows {
                        if parsed.respell_head() {
                            assert!(!folded.contains('/'), "{path:?} -> {folded:?}");
                        } else {
                            assert!(
                                folded.starts_with(&path[..parsed.head_len]),
                                "{path:?} -> {folded:?}"
                            );
                        }
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

    /// Laws 1–3 (§9): lossless parsing, host-independent explicit-rule parsing
    /// and folding, and fold stability (`P_r(F_r(s))` preserves head — with
    /// [`Namespace::Object`] `\??\` keeping `Driveless` and retaining
    /// `Some(Namespace::Object)` iff the folded path still opens with `\??\` —
    /// and `F_r(F_r(s)) == F_r(s)`, with `F_W(s) == s` on opaque heads),
    /// checked by exhaustive enumeration over the token alphabet
    /// `["a", ".", "..", ""]` to depth 4 across both separator spellings
    /// (`"/"`, `r"\"`), plus non-UTF-8 byte slices on Unix and
    /// unpaired-surrogate `OsString`s on Windows.
    #[test]
    fn laws_1_to_3_hold_across_token_alphabet_to_depth_4() {
        let check_bytes = |s: &[u8], rules: PathRules| {
            let p = parse_bytes(s, rules);
            // Law 1: lossless parsing and head/tail partition.
            assert_eq!(p.raw, s);
            assert_eq!([&p.raw[..p.head_len], &p.raw[p.head_len..]].concat(), s);

            // Law 3: fold stability and opaque preservation.
            let f = fold_bytes(s, rules);
            let ff = fold_bytes(&f, rules);
            assert_eq!(ff, f, "idempotence failed on {s:?} under {rules:?}");
            if rules == PathRules::Windows && p.head_len == s.len() && !p.respell_head() {
                assert_eq!(f, s, "opaque head modified on {s:?}");
            }
            let pf = parse_bytes(&f, rules);
            assert_fold_preserves_head(p, pf, &format!("{s:?} ({rules:?})"));
        };

        let heads = [
            "",
            "/",
            r"\",
            "C:",
            r"C:\",
            "C:/",
            r"\\srv\sh\",
            "//srv/sh/",
            r"\\?\C:\",
            "//?/C:/",
            r"\\.\pipe\",
            r"\??\C:\",
            r"\\srv\",
            r"/\x\",
        ];
        let tokens = ["a", ".", "..", ""];
        let seps = ["/", r"\"];
        for head in heads {
            for sep in seps {
                for t0 in tokens {
                    for t1 in tokens {
                        for t2 in tokens {
                            for t3 in tokens {
                                let tail = [t0, t1, t2, t3].join(sep);
                                let s = format!("{head}{tail}");
                                for rules in [PathRules::Posix, PathRules::Windows] {
                                    check_bytes(s.as_bytes(), rules);
                                }
                            }
                        }
                    }
                }
            }
        }

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            for raw in [
                &b"/a/\xff/./b/../\xfe"[..],
                &b"\\\xfe\\./..\\\xff"[..],
                &b"C:\\\xff\\..\\a"[..],
                &b"\\\\?\\C:\\\xff\\.."[..],
            ] {
                for rules in [PathRules::Posix, PathRules::Windows] {
                    check_bytes(raw, rules);
                }
                let folded = fold_dots(Path::new(OsStr::from_bytes(raw)));
                assert_eq!(fold_dots(&folded), folded);
            }
        }

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            let wide: Vec<u16> = r"C:\a\"
                .encode_utf16()
                .chain([0xD800])
                .chain(r"\.\b\..\".encode_utf16())
                .chain([0xD801])
                .collect();
            let os = std::ffi::OsString::from_wide(&wide);
            for rules in [PathRules::Posix, PathRules::Windows] {
                check_bytes(os.as_encoded_bytes(), rules);
            }
            let folded = fold_dots(Path::new(&os));
            assert_eq!(fold_dots(&folded), folded);
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
            "//?/C:/x",
            r"\\.\pipe\x",
            "/x",
            "x",
        ] {
            assert!(!is_unparsed_head(Path::new(raw)), "{raw}");
        }
    }
}
