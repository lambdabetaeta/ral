//! A path's head, parsed once under either platform's rules, and every
//! judgment of it: absoluteness, a working directory, a bare root, and the
//! Windows components past it.

use std::ffi::OsStr;
use std::path::{Component, Path};

use crate::path::PathRules;
use crate::path::site::Silent;
use crate::site::site;

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
/// grant is a separate question, which `authority::grant::freeze` asks first.
///
/// [`anchor`], through [`is_absolute`], and [`join_under`], rely on a
/// [`HeadKind::Device`] or [`HeadKind::Malformed`] path counting as absolute
/// under Windows: the kernel never reads one against a cwd, so neither may
/// they, joining it onto one or dropping it as relative.
///
/// [`anchor`]: super::anchor()
/// [`join_under`]: super::join_under
pub(crate) fn absolute_under(path: impl AsRef<OsStr>, rules: PathRules) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), rules).is_absolute()
}

/// Whether `path` opens with a drive, rooted on it or not: `C:\x`, `C:x`.
pub(crate) fn has_drive(path: impl AsRef<OsStr>) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), PathRules::Windows).has_drive()
}

/// Whether `path` names a network share: `\\server\share`, or its verbatim
/// spelling `\\?\UNC\server\share`.
#[cfg(test)]
pub(crate) fn is_network(path: impl AsRef<OsStr>) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), PathRules::Windows).is_network()
}

/// Whether `path` is a verbatim drive path (`\\?\C:\x`).
#[cfg(test)]
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

/// Whether `path` is absolute on this host: [`absolute_under`] the host's
/// rules, the one absoluteness the path module answers with, a path the
/// host holds or a string alike.
pub fn is_absolute(path: impl AsRef<OsStr>) -> bool {
    parse_bytes(path.as_ref().as_encoded_bytes(), PathRules::HOST).is_absolute()
}

/// Whether `path` can be a working directory under the host's rules: what
/// [`CwdPath`] checks.  POSIX: absolute.  Windows: a drive root, a share, or
/// the verbatim spelling of either, the roots with an extent a name joins
/// past without changing what the root names.  A malformed or mixed head has
/// none: `\\srv` joined with `sh` would name the share `\\srv\sh`.
///
/// [`CwdPath`]: crate::path::CwdPath
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
/// on exactly this path and nowhere else, so the callers that must not anchor
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
        && !matches!(
            site!(Silent::UnparsedHead, path => components()).next(),
            Some(Component::Prefix(_))
        )
}

/// The 4-byte Windows namespace prefix a path opens with.
///
/// Device (`\\.\`) and exact verbatim (`\\?\`) share no normalisation rule:
/// Win32 normalises device paths and hands exact `\\?\` paths to the kernel
/// uninterpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::path) enum Namespace {
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
pub(in crate::path) struct HeadSpelling {
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
pub(in crate::path) enum HeadKind<'a> {
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
pub(in crate::path) struct Parsed<'a> {
    pub(crate) raw: &'a [u8],
    pub(crate) rules: PathRules,
    pub(crate) kind: HeadKind<'a>,
    pub(crate) spelling: HeadSpelling,
    /// Byte offset where the head ends and the foldable component tail begins.
    /// Equal to `raw.len()` when the path is opaque to lexical folding.
    pub(crate) head_len: usize,
}

/// Parse `raw`'s head and separator spelling once under `rules`.
pub(in crate::path) fn parse_bytes(raw: &[u8], rules: PathRules) -> Parsed<'_> {
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

pub(super) fn is_sep_byte(rules: PathRules, b: u8) -> bool {
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
    #[cfg(test)]
    pub(crate) fn is_network(&self) -> bool {
        matches!(self.kind, HeadKind::Unc { .. })
    }

    /// Whether `self` is a verbatim drive path (`\\?\C:\x`).
    #[cfg(test)]
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

/// A path's components under Windows path identity, past its
/// [head](windows_head), in the encoding it came in: the bytes of a `str`, or
/// of an [`OsStr`] as the host encodes it: the one split every Windows
/// identity of a path takes ([`crate::path::identity`]).
///
/// Lossless on either encoding: a separator is ASCII, and an ASCII byte in
/// either stands for that character alone; a head is ASCII, so slicing at its
/// boundary preserves the encoding of the rest.
pub(crate) fn windows_components(p: &[u8]) -> impl Iterator<Item = &[u8]> {
    parse_bytes(p, PathRules::Windows)
        .without_verbatim_prefix()
        .split(|&b| PathRules::Windows.is_separator(char::from(b)))
        .filter(|c| !c.is_empty())
}

/// A path string past its Windows head: a verbatim prefix stripped, and a
/// verbatim UNC head's `UNC` with it, so `\\?\UNC\server\share` splits as
/// `\\server\share` does.  The step [`windows_components`] takes before it
/// splits, shared with the prompt's home-strip (`crate::path::tilde`), which needs
/// the head fold without the case fold.  One copy, because a head the two
/// folded differently would silently unfold a `~`.
pub(crate) fn windows_head(p: &str) -> &str {
    let rest = parse_bytes(p.as_bytes(), PathRules::Windows).without_verbatim_prefix();
    // SAFETY: `without_verbatim_prefix` slices `p` at an ASCII prefix boundary,
    // which cannot land inside a multi-byte UTF-8 sequence.
    unsafe { std::str::from_utf8_unchecked(rest) }
}
