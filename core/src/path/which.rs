//! `PATH` search: locate a bare command name on disk.
//!
//! Sibling to the grant pipeline, but `$PATH` is only a colon-separated
//! list walked in turn, so the sigil/lex/canon stages do not apply.
//! Dispatch arrives via `runtime::command::identity` and completion via
//! [`commands_on_path`], both onto the same walk and the same
//! executable-bit rule.

use std::path::{Path, PathBuf};

/// First executable named `name` on the colon-separated `path`, with
/// relative `PATH` entries anchored to `cwd`; `None` when `name` bears a
/// separator, which is a path, not `PATH`'s business.
pub fn resolve_in_path(name: &str, path: &str, cwd: Option<&Path>) -> Option<String> {
    if name_has_separator(name) {
        return None;
    }
    locate(name, Some(path), cwd).map(|p| p.to_string_lossy().into_owned())
}

/// Resolve a command head to its executable on disk: a separator-bearing
/// name is a path anchored against `cwd`, a bare name is walked against
/// `path_value`.  `Some` only for a regular file carrying the executable
/// bit — the check the OS would apply at spawn.
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

/// The one directory list behind [`locate`], [`commands_on_path`], and
/// [`file_exists_on_path`]; relative entries (`./bin`) anchor to `cwd`.
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

/// Names of the executables reachable through `path_value`, in `PATH`
/// order; unreadable entries are skipped.  Unsorted, and a name repeats
/// once per directory holding it — completion sorts and dedupes its own.
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

/// A regular file named `name` on `PATH`, executable bit ignored, so
/// `runtime::command::vet` can answer 126 (permission denied) where a
/// plain miss would answer 127.
pub fn file_exists_on_path(name: &str, path: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    if name_has_separator(name) {
        return None;
    }
    path_dirs(path, cwd)
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Mirror the Windows resolver's `%PATHEXT%` fallback, so
/// `locate("python")` finds `python.exe`.  `capability::exec` keeps its
/// own copy of the default list to strip suffixes off grant keys.
#[cfg(windows)]
#[allow(
    clippy::disallowed_methods,
    reason = "PATHEXT is the Windows resolver's suffix list, not an XDG basedir — a PATH-probe env read, allowed at the call site like the other which/PATH probes here"
)]
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
        // Against the supplied cwd, not the process cwd: otherwise the
        // prompt stops reflecting the shell's notion of "here".
        let names = commands_on_path("./bin", Some(tmp.path()));
        assert!(names.contains(&"runme".to_string()), "got {names:?}");
    }

    #[test]
    fn resolve_in_path_anchors_relative_entries_to_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        touch(&bin.join("runme"), 0o755);
        // Against the supplied cwd, not the process cwd: dispatch and the
        // exec gate must land on the binary the shell's own `./bin` names.
        let hit = resolve_in_path("runme", "./bin", Some(tmp.path())).unwrap();
        assert_eq!(
            std::fs::canonicalize(&hit).unwrap(),
            std::fs::canonicalize(bin.join("runme")).unwrap(),
        );
    }

    #[test]
    fn commands_on_path_skips_missing_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let absent = tmp.path().join("does-not-exist");
        let names = commands_on_path(absent.to_str().unwrap(), None);
        assert!(names.is_empty(), "got {names:?}");
    }
}
