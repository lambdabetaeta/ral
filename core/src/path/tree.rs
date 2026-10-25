//! Directory trees: the one walk over them, glob patterns enumerated by it, and their size.

use crate::authority::policy::Ungoverned;
use crate::path::LexicalPath;
use crate::path::site::Silent;
use crate::process::{DurableRoot, Scope};
use crate::site::site;
use std::path::Path;
use walk::{Found, Walk};

pub(crate) mod glob;
mod ignore;
pub mod walk;

/// Total bytes of every regular file under `root`, recursively.
///
/// Symlinks are not followed (their target may leave the tree) and an
/// unreadable entry counts zero rather than failing the fold.  A host-side
/// sizing of its own scratch and log directories, judged by no grant, for the
/// `path-bytes` probe and exarch `/resources`.
#[must_use]
pub fn bytes(root: &Path) -> u64 {
    let Some(root) = LexicalPath::absolute(root) else {
        return 0;
    };
    Walk::all(&root)
        .readable_by(site!(Silent::OwnTreeRegion, Ungoverned::region()))
        .entries(Scope::clone(&DurableRoot::default()))
        .filter_map(Result::ok)
        .filter(|entry| entry.depth > 0)
        .map(|entry| match entry.found {
            Found::Seen(stat) if stat.kind == crate::path::Kind::File => stat.len,
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
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
