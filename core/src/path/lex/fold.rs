//! The `.`/`..` fold: a path's logical reading, its head meaning what it did.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::head::{is_sep_byte, parse_bytes};
use crate::path::PathRules;

/// Fold `.`/`..` lexically under the host's rules, touching neither
/// filesystem nor cwd: [`fold_dots_under`] on a path the host holds.  The
/// logical reading of a name, which [`LexicalPath::folded`](crate::path::LexicalPath::folded)
/// gives its consumers.
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
/// still [`Drive`](super::HeadKind::Drive) or [`Unc`](super::HeadKind::Unc) under
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
/// the head kept in.
///
/// [`HeadKind`]: super::HeadKind
/// [`HeadKind::Device`]: super::HeadKind::Device
/// [`HeadKind::Driveless`]: super::HeadKind::Driveless
/// [`HeadKind::Drive`]: super::HeadKind::Drive
/// [`HeadKind::Malformed`]: super::HeadKind::Malformed
/// [`HeadKind::Posix`]: super::HeadKind::Posix
/// [`HeadKind::Unc`]: super::HeadKind::Unc
/// [`Namespace::Verbatim`]: super::head::Namespace::Verbatim
pub(crate) fn fold_dots_under(path: &str, rules: PathRules) -> String {
    String::from_utf8(fold_bytes(path.as_bytes(), rules))
        .expect("a fold cuts UTF-8 only at ASCII separators and joins it with them")
}

/// [`fold_dots_under`] on a path's encoded bytes.
pub(super) fn fold_bytes(path: &[u8], rules: PathRules) -> Vec<u8> {
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
                    rules.separator()
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
            folded.push(rules.separator());
        }
        folded.extend_from_slice(name);
    }
    folded
}
