//! A command name as a launch reads it: a path, or a bare name for `PATH`
//! to search, and the extensions Windows runs a bare name through.

#[cfg(any(windows, test))]
use std::ffi::OsStr;

use crate::path::PathRules;

/// Whether a command name is a path rather than a name for `PATH` to find:
/// it bears a separator under the host's rules.
pub(crate) fn names_a_path(name: &str) -> bool {
    names_a_path_for(name, PathRules::HOST)
}

/// [`names_a_path`] under given rules: so `a\b` is a path under Windows
/// and a bare name, searched on `PATH`, under Posix.
pub(crate) fn names_a_path_for(name: &str, rules: PathRules) -> bool {
    name.contains(|c| rules.is_separator(c))
}

/// The extensions Windows runs a bare name through.  `.bat` and `.cmd` belong
/// here even though `process::launch` refuses to spawn them: that is a later
/// refusal of the image, not of the name.
pub(crate) const WINDOWS_EXEC_EXTENSIONS: &[&str] = &["com", "exe", "bat", "cmd"];

/// The [`WINDOWS_EXEC_EXTENSIONS`] Windows runs through `cmd.exe`, which
/// re-parses the command line by its own escaping rules.
#[cfg(any(windows, test))]
pub(super) const WINDOWS_BATCH_EXTENSIONS: &[&str] = &["bat", "cmd"];

/// Whether `program` names a batch image as Windows will run it: once
/// [Win32 has trimmed](crate::path::device::win32_trimmed) its trailing dots and
/// blanks, it ends in `.` and one of [`WINDOWS_BATCH_EXTENSIONS`], in any
/// case, so `run.bat.` and `run.bat ` are batch images too, and `.bat`
/// alone is one, as `std`'s own check reads it.  Trimmed under a verbatim
/// head as well, where Win32 would not: a refusal that over-reads fails
/// closed.
///
/// Read on the name's encoded bytes, so nothing is decoded and an ill-formed
/// name is judged by the suffix it has.
#[cfg(any(windows, test))]
pub(crate) fn is_batch_image(program: &OsStr) -> bool {
    let trimmed = crate::path::device::win32_trimmed(program.as_encoded_bytes());
    trimmed.iter().rposition(|&b| b == b'.').is_some_and(|dot| {
        let ext = &trimmed[dot + 1..];
        WINDOWS_BATCH_EXTENSIONS
            .iter()
            .any(|batch| ext.eq_ignore_ascii_case(batch.as_bytes()))
    })
}
