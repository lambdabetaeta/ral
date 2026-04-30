//! `PATH` search: locate a bare command name on disk.
//!
//! Sibling to the four-stage grant pipeline: command resolution
//! shares the "given a string, find the absolute file" question
//! but skips the sigil/lex/canon stages — `$PATH` is a colon-
//! separated list of directories and we just look in each.
//!
//! Both head-classification (`Shell::classify_command_head`) and
//! the exec gate (`evaluator::exec::exec_policy_names`) need the
//! same resolution so a path-keyed grant matches at both points.
//!
//! Two entry points:
//!
//!   * [`resolve_in_path`] — pure-string PATH walker; bare names only.
//!     Used by exec-gate keying where `name` is already known to be
//!     bare and there is no shell-context cwd to anchor against.
//!
//!   * [`locate`] — full command resolution: handles names that
//!     contain a separator (cwd-anchored or absolute) as well as bare
//!     names, takes an explicit `cwd` so relative `PATH` entries
//!     resolve consistently with the caller's notion of "here."
//!     Used by `which`, by the dispatch error path (so a deny message
//!     can distinguish "exists and denied" from "not installed"), and
//!     anywhere else that needs the same answer the OS would give.

use std::path::{Path, PathBuf};

/// Walk `path` (a colon-separated `PATH` string) looking for an
/// executable file named `name`.  Returns the absolute path of
/// the first hit, or `None` if none of `path`'s directories
/// contain an executable `name`.
///
/// Returns `None` immediately if `name` contains a separator —
/// that is treated as a path, not a bare command, and is not the
/// business of `PATH` lookup.  Thin wrapper over [`locate`] for
/// the common case where the caller has a `PATH` string in hand
/// and no shell context to anchor relative entries against.
pub fn resolve_in_path(name: &str, path: &str) -> Option<String> {
    if name_has_separator(name) {
        return None;
    }
    locate(name, Some(path), None).map(|p| p.to_string_lossy().into_owned())
}

/// Resolve a command head — bare name or path — to its executable
/// target on disk, using `path_value` as the colon-separated `PATH`
/// and `cwd` to anchor relative paths and relative `PATH` entries.
///
/// - Names containing a separator are treated as paths: an absolute
///   name is checked as-is; a relative one is anchored against `cwd`
///   (or returned unchanged when `cwd` is `None`).
/// - Bare names are walked against `path_value`.  Relative `PATH`
///   entries (rare but legal, e.g. `./bin`) are anchored against
///   `cwd` so the walk has the same notion of "here" as the caller.
///
/// Returns the resolved [`PathBuf`] when the candidate is a regular
/// file with the executable bit set; otherwise `None`.
pub fn locate(name: &str, path_value: Option<&str>, cwd: Option<&Path>) -> Option<PathBuf> {
    if name_has_separator(name) {
        let candidate = anchor_to_cwd(PathBuf::from(name), cwd);
        return is_executable_file(&candidate).then_some(candidate);
    }
    let path_value = path_value?;
    for dir in std::env::split_paths(&std::ffi::OsString::from(path_value)) {
        let candidate = anchor_to_cwd(dir, cwd).join(name);
        #[cfg(windows)]
        for c in windows_command_candidates(&candidate) {
            if is_executable_file(&c) {
                return Some(c);
            }
        }
        #[cfg(not(windows))]
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn name_has_separator(name: &str) -> bool {
    name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') || name.contains('\\')
}

fn anchor_to_cwd(p: PathBuf, cwd: Option<&Path>) -> PathBuf {
    if p.is_absolute() {
        return p;
    }
    match cwd {
        Some(c) => c.join(p),
        None => p,
    }
}

fn is_executable_file(p: &Path) -> bool {
    if !p.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Windows PATHEXT expansion.  When invoked without an explicit
/// extension, the Windows command resolver tries each suffix in
/// `%PATHEXT%` (defaulting to `.COM;.EXE;.BAT;.CMD`).  We mirror the
/// same fallback so `locate("python")` finds `python.exe`.
#[cfg(windows)]
fn windows_command_candidates(base: &Path) -> Vec<PathBuf> {
    use std::ffi::OsStr;
    let mut out = Vec::new();
    if base.extension().is_some() {
        out.push(base.to_path_buf());
    }
    let pathext = std::env::var_os("PATHEXT")
        .unwrap_or_else(|| OsStr::new(".COM;.EXE;.BAT;.CMD").to_os_string());
    for ext in pathext
        .to_string_lossy()
        .split(';')
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        let ext = ext.trim_start_matches('.');
        out.push(base.with_extension(ext));
    }
    out
}
