//! A relative spelling made absolute against a cwd, as the host's kernel
//! reads it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use super::head::{HeadKind, is_absolute, is_sep_byte, parse_bytes};
use crate::path::{CwdPath, PathRules};

/// `input` made absolute against `cwd` as the host reads it.  Unfolded: a
/// `..` or a trailing separator is the kernel's to read, after the links
/// before it ([`fold_dots`] is the logical reading).
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
/// so the result is absolute: what [`LexicalPath`]
/// asserts.
///
/// [`fold_dots`]: super::fold_dots
/// [`LexicalPath`]: crate::path::LexicalPath
pub(crate) fn anchor(cwd: &CwdPath, input: &Path) -> PathBuf {
    if is_absolute(input) {
        input.to_path_buf()
    } else {
        against(cwd, input)
    }
}

/// [`anchor`] where there may be no cwd: an absolute `input` is kept, and a
/// relative one with none names nothing.
pub(crate) fn anchor_on(cwd: Option<&CwdPath>, input: &Path) -> Option<PathBuf> {
    match cwd {
        Some(cwd) => Some(anchor(cwd, input)),
        None => is_absolute(input).then(|| input.to_path_buf()),
    }
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
        out.push(PathRules::HOST.separator());
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
