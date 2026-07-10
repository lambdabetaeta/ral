//! `PATH` search: locate a bare command name on disk.
//!
//! Sibling to the grant pipeline — same "given a string, find the
//! absolute file" question, but `$PATH` is just a colon-separated list
//! of directories walked in turn ([`path_dirs`]), so the sigil/lex/canon
//! stages don't apply.  `runtime::command::CommandIdentity` builds on
//! [`locate`] so dispatch and grant admission see one walk per call.
//!
//! The primary entry points are [`resolve_in_path`] (pure-string walker,
//! bare names only) and [`locate`] (full resolution: separator-bearing
//! names, an explicit `cwd` to anchor relative `PATH` entries, and the
//! executable-bit check the OS would apply).

use std::path::{Path, PathBuf};

/// Walk `path` (a colon-separated `PATH` string) looking for an
/// executable file named `name`.
///
/// Returns the absolute path of
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
    for dir in path_dirs(path_value, cwd) {
        let candidate = dir.join(name);
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

/// Split a colon-separated `PATH` string into directories, anchoring each
/// relative entry against `cwd` (matching [`locate`]'s rule).  The shared
/// walk behind [`locate`], [`commands_on_path`], and [`file_exists_on_path`].
fn path_dirs(path_value: &str, cwd: Option<&Path>) -> Vec<PathBuf> {
    std::env::split_paths(&std::ffi::OsString::from(path_value))
        .map(|dir| anchor_to_cwd(dir, cwd))
        .collect()
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:which-stat] `which`/PATH probe: stats a candidate to read its executable bit; an executable-probe predicate, not turn-time model data I/O, raises no surface card."
)]
fn is_executable_file(p: &Path) -> bool {
    if !p.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Enumerate names of executable files reachable through `path_value`,
/// in PATH-entry order.
///
/// Each directory is anchored to `cwd` if it is
/// relative (matching [`locate`]'s rule).  Inaccessible or non-directory
/// entries are skipped; entries within a directory are not sorted.
///
/// Used by completion to mirror what `locate` will find: same dirs, same
/// anchor, same executable-bit rule.  The result is unsorted and may
/// repeat a name across directories; callers that want stable order or
/// deduplication apply their own.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:which-readdir] `which`/completion probe: enumerates each PATH directory to list executable names; an executable-probe scan, not turn-time model data I/O, raises no surface card."
)]
pub fn commands_on_path(path_value: &str, cwd: Option<&Path>) -> Vec<String> {
    let mut out = Vec::new();
    for dir in path_dirs(path_value, cwd) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_executable_file(&path) {
                continue;
            }
            if let Ok(name) = entry.file_name().into_string() {
                out.push(name);
            }
        }
    }
    out
}

/// Walk PATH looking for a regular file named `name`, ignoring the
/// executable bit and anchoring relative PATH entries against `cwd`
/// (like [`locate`]).
///
/// Used to improve error messages: when PATH search skips a file because
/// it lacks `+x`, we can report "permission denied" (126) instead of
/// "command not found" (127).
pub fn file_exists_on_path(name: &str, path: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    if name_has_separator(name) {
        return None;
    }
    path_dirs(path, cwd)
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
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

#[cfg(test)]
#[cfg(unix)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn touch(p: &Path, mode: u32) {
        std::fs::write(p, b"#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(p).unwrap().permissions();
        perms.set_mode(mode);
        std::fs::set_permissions(p, perms).unwrap();
    }

    #[test]
    fn commands_on_path_finds_executables() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("runme"), 0o755);
        let names = commands_on_path(tmp.path().to_str().unwrap(), None);
        assert!(names.contains(&"runme".to_string()), "got {names:?}");
    }

    #[test]
    fn commands_on_path_skips_non_executable_files() {
        let tmp = tempfile::tempdir().unwrap();
        touch(&tmp.path().join("noexec"), 0o644);
        let names = commands_on_path(tmp.path().to_str().unwrap(), None);
        assert!(!names.contains(&"noexec".to_string()), "got {names:?}");
    }

    #[test]
    fn commands_on_path_anchors_relative_entries_to_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        touch(&bin.join("runme"), 0o755);
        // Relative PATH entry resolves against the supplied cwd, not the
        // process cwd — the prompt would otherwise stop reflecting the
        // shell's notion of "here."
        let names = commands_on_path("./bin", Some(tmp.path()));
        assert!(names.contains(&"runme".to_string()), "got {names:?}");
    }

    #[test]
    fn commands_on_path_skips_missing_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("does-not-exist");
        // No panic, no error — just an empty result for the bad entry.
        let names = commands_on_path(absent.to_str().unwrap(), None);
        assert!(names.is_empty(), "got {names:?}");
    }
}
