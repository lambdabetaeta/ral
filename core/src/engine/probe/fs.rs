//! Readings of the engine's own filesystem: read-only metadata walks a host
//! asks for at a run boundary — operator diagnostics and completion, not
//! model I/O.

use crate::protocol::probe::PathEntry;
use std::path::Path;

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
