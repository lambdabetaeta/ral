//! Directory trees: the one walk over them, glob patterns enumerated by it, and their size.

use std::path::Path;

pub(crate) mod glob;
pub mod walk;

/// Total bytes of every regular file under `root`, recursively.
///
/// Symlinks are not followed (their target may leave the tree) and an
/// unreadable entry counts zero rather than failing the fold. Sizes come per
/// path from `symlink_metadata`: on Windows a directory entry's own figure is
/// a cached size a still-open file has not updated.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:tree-bytes] a read-only metadata walk sizing a host's own scratch and log directories, for the `path-bytes` probe and exarch `/resources`; operator diagnostics, not model I/O"
)]
#[must_use]
pub fn bytes(root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => bytes(&path),
                Ok(meta) if meta.is_file() => meta.len(),
                _ => 0,
            }
        })
        .sum()
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures write scratch files"
)]
mod tests {
    use super::*;

    /// A missing directory reads zero rather than failing the fold.
    #[test]
    fn bytes_sums_files_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("a"), b"12345").unwrap();
        std::fs::write(root.join("sub/b"), b"123").unwrap();
        assert_eq!(bytes(root), 8);
        assert_eq!(bytes(&root.join("missing")), 0);
    }
}
