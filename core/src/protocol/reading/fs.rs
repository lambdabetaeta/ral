//! Readings of the engine's own filesystem: read-only metadata walks a host
//! asks for at a run boundary — operator diagnostics and completion, not
//! model I/O.

use super::rows::PathEntry;
use std::path::Path;

/// Total bytes of every regular file under `root`, recursively. Symlinks are
/// not followed, and an unreadable entry counts zero rather than failing the
/// fold. Sizes come per path from `symlink_metadata`: on Windows a directory
/// entry's own figure is a cached size a still-open file has not updated.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:probe-path-bytes] the `path-bytes` probe: a read-only metadata walk a host asks for at a run boundary to size its own scratch and log directories; operator diagnostics, not model I/O"
)]
pub(super) fn tree_bytes(root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => tree_bytes(&path),
                Ok(meta) if meta.is_file() => meta.len(),
                _ => 0,
            }
        })
        .sum()
}

/// `dir`'s entries with a UTF-8 name; none, if it cannot be read. `exec`
/// follows a symlink, as `PATH` lookup does.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:probe-path-entries] the `path-entries` probe: a directory listing a host asks for at a run boundary to complete paths and scan `PATH`; not turn-time model I/O"
)]
pub(super) fn entries(dir: &Path) -> Vec<PathEntry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            Some(PathEntry {
                dir: entry.file_type().is_ok_and(|t| t.is_dir()),
                exec: crate::path::is_executable_file(&entry.path()),
                name: entry.file_name().into_string().ok()?,
            })
        })
        .collect()
}
