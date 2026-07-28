//! Git directory discovery, for the `gitdir:` sigil that
//! [`freeze_one`](super::sigil::freeze_one) expands at policy freeze.

use std::path::{Path, PathBuf};

/// The first `.git` at or above `cwd` — a directory in a plain clone,
/// a file in a worktree or submodule.
pub fn find_git_entry(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find_map(|dir| {
        let dg = dir.join(".git");
        if dg.exists() { Some(dg) } else { None }
    })
}

/// The real git directory for `cwd`, following the `gitdir:` pointer
/// when `.git` is a file rather than a directory.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:git-dir-discovery] reads the .git worktree pointer at session startup to discover the actual git directory; not turn-time I/O"
)]
pub fn discover_git_dir(cwd: &Path) -> Option<PathBuf> {
    let dot_git = find_git_entry(cwd)?;

    if dot_git.is_dir() {
        return Some(dot_git);
    }

    // A worktree's `.git` is a file whose body is `gitdir: <path>`.
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir_path = contents
        .lines()
        .find(|l| l.starts_with("gitdir:"))?
        .strip_prefix("gitdir:")?
        .trim();

    let resolved = if crate::path::is_absolute(gitdir_path) {
        PathBuf::from(gitdir_path)
    } else {
        dot_git.parent()?.join(gitdir_path)
    };

    // Same fold-dots kernel that mints a `NormalizedPrefix`, so what comes
    // back here can be matched against grant prefixes.
    Some(crate::path::lex::fold_dots(&resolved))
}
