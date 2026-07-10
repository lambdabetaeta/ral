//! Git directory discovery for the `gitdir:` sigil.
//!
//! Walks up from a directory to find the first `.git` entry, then
//! resolves worktree pointers (`gitdir: <path>`) to the actual
//! git directory.  Used by [`freeze_one`](super::sigil::freeze_one)
//! to expand `gitdir:` tokens.

use std::path::{Path, PathBuf};

/// Walk up from `cwd` and return the first `.git` entry found (file or
/// directory).
///
/// Returns `None` when `cwd` is not inside a git repository.
pub fn find_git_entry(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find_map(|dir| {
        let dg = dir.join(".git");
        if dg.exists() { Some(dg) } else { None }
    })
}

/// Discover the real git directory for `cwd`, resolving the `gitdir:`
/// pointer when `.git` is a worktree file rather than a directory.
///
/// Returns `None` when `cwd` is not inside a git repository.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:git-dir-discovery] reads the .git worktree pointer at session startup to discover the actual git directory; not turn-time I/O"
)]
pub fn discover_git_dir(cwd: &Path) -> Option<PathBuf> {
    let dot_git = find_git_entry(cwd)?;

    if dot_git.is_dir() {
        return Some(dot_git);
    }

    // Worktree: .git is a file containing "gitdir: <path>"
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir_path = contents
        .lines()
        .find(|l| l.starts_with("gitdir:"))?
        .strip_prefix("gitdir:")?
        .trim();

    let resolved = if crate::path::is_absolute(gitdir_path) {
        PathBuf::from(gitdir_path)
    } else {
        // Relative to the .git file's parent directory
        dot_git.parent()?.join(gitdir_path)
    };

    // Normalize through the same fold-dots kernel the grant side uses.
    Some(crate::path::lex::fold_dots(&resolved))
}
