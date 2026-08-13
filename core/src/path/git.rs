//! Git directory discovery, for the `gitdir:` sigil that
//! [`freeze_one`](super::sigil::freeze_one) expands at policy freeze.
//!
//! A `.git` *file* names its git directory in the working tree's own words, so
//! a grant that followed the pointer alone would be authored by whoever wrote
//! the tree — and the sigil is granted for read *and* write.  Following it
//! therefore asks the other direction too: the git directory must name this
//! working tree back, as a linked worktree's `gitdir` file does and as
//! `core.worktree` does for a repository split off with `--separate-git-dir`.
//! A pointer no git directory claims is a [`PolicyError`], never a wider grant.

use crate::path::canon::canonicalise_lenient;
use crate::path::lex::{fold_dots, parent_or_cwd, path_within};
use crate::types::PolicyError;
use std::path::{Path, PathBuf};

/// The first `.git` at or above `cwd` — a directory in a plain clone,
/// a file in a worktree or submodule.
pub fn find_git_entry(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find_map(|dir| {
        let dg = dir.join(".git");
        if dg.exists() { Some(dg) } else { None }
    })
}

/// The git directory `cwd` belongs to.
///
/// A plain clone's `.git` as it stands, or the target of a `.git` pointer file
/// that claims this working tree.  `None` when no `.git` lies at or above `cwd`,
/// which leaves the sigil's fallback to its caller.
///
/// # Errors
/// A `.git` file that names no git directory (unreadable, or no `gitdir:`
/// line), or one whose target does not claim the working tree — each naming
/// both paths, with the repair as the hint.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:git-dir-discovery] reads the .git worktree pointer and the git directory's answering claim at session startup, to discover the actual git directory; not turn-time I/O"
)]
pub fn discover_git_dir(cwd: &Path) -> Result<Option<PathBuf>, PolicyError> {
    let Some(dot_git) = find_git_entry(cwd) else {
        return Ok(None);
    };
    if dot_git.is_dir() {
        return Ok(Some(dot_git));
    }
    let target = read_pointer(&dot_git)?;
    if claims(&target, &dot_git) {
        return Ok(Some(target));
    }
    Err(unclaimed_message(&dot_git, &target))
}

/// A worktree's `.git` is a file whose body is `gitdir: <path>`, relative to
/// the file's own directory when it is not absolute.  Folded through the same
/// kernel that mints a `NormalizedPrefix`, so what comes back can be matched
/// against grant prefixes.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:git-dir-discovery]"
)]
fn read_pointer(dot_git: &Path) -> Result<PathBuf, PolicyError> {
    let contents = std::fs::read_to_string(dot_git).map_err(|e| unreadable_message(dot_git, &e))?;
    let pointer = contents
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .ok_or_else(|| no_pointer_message(dot_git))?;
    Ok(fold_dots(&against(parent_or_cwd(dot_git), pointer)))
}

/// Whether `gitdir` names `dot_git`'s working tree as its own, by whichever of
/// the two records git keeps is there: a linked worktree's `gitdir` file, holding
/// the path of the very `.git` file that points here, or `core.worktree` in
/// `config`, which `git init --separate-git-dir` and an absorbed submodule write
/// instead.  A back-pointer that exists is the answer, whether it names this tree
/// or another one.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:git-dir-discovery]"
)]
fn claims(gitdir: &Path, dot_git: &Path) -> bool {
    if let Ok(back) = std::fs::read_to_string(gitdir.join("gitdir")) {
        return same_path(&against(gitdir, back.trim()), dot_git);
    }
    core_worktree(&gitdir.join("config"))
        .is_some_and(|tree| same_path(&against(gitdir, &tree), parent_or_cwd(dot_git)))
}

/// `path` as an absolute path: itself when it already is one, else joined onto
/// `base` — the directory a git pointer or back-pointer is written relative to.
fn against(base: &Path, path: &str) -> PathBuf {
    if crate::path::is_absolute(path) {
        return PathBuf::from(path);
    }
    base.join(path)
}

/// Two spellings of one path, under the identity the grant matcher uses:
/// `realpath(3)` on both sides, then the alias-aware containment of
/// [`path_within`] in both directions.
fn same_path(a: &Path, b: &Path) -> bool {
    let (a, b) = (canonicalise_lenient(a), canonicalise_lenient(b));
    path_within(&a, &b) && path_within(&b, &a)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:git-dir-discovery]"
)]
fn core_worktree(config: &Path) -> Option<String> {
    core_worktree_of(&std::fs::read_to_string(config).ok()?)
}

/// `core.worktree` from the text of a git config file.
///
/// A deliberately narrow reader — one key, in the one section that may hold it,
/// with comments dropped — since the value is only ever compared against a path
/// already in hand.  Whatever it cannot read is no claim, and no claim refuses.
fn core_worktree_of(text: &str) -> Option<String> {
    let mut in_core = false;
    for line in text.lines() {
        let line = line
            .split_once(['#', ';'])
            .map_or(line, |(before, _)| before)
            .trim();
        if let Some(head) = line.strip_prefix('[') {
            in_core = head
                .trim_end_matches(']')
                .trim()
                .eq_ignore_ascii_case("core");
            continue;
        }
        if !in_core {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("worktree")
        {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn unclaimed_message(dot_git: &Path, target: &Path) -> PolicyError {
    PolicyError::new(format!(
        "the .git file at '{dot_git}' points at '{target}', but that directory \
         does not name this working tree back, so `gitdir:` will not grant it — \
         a pointer written inside the tree would otherwise choose the grant.",
        dot_git = dot_git.display(),
        target = target.display(),
    ))
    .with_hint(
        "A linked worktree's git directory holds a `gitdir` file naming this \
         very .git file, and a repository split off with `--separate-git-dir` \
         holds `core.worktree` in its config; neither names this tree.  Was the \
         worktree moved?  `git worktree repair` rewrites both ends.  If the \
         pointer is hand-written, name the git directory explicitly in the \
         policy instead of `gitdir:`.",
    )
}

fn no_pointer_message(dot_git: &Path) -> PolicyError {
    PolicyError::new(format!(
        "the .git entry at '{}' is a file with no `gitdir:` line, so there is \
         no git directory for `gitdir:` to name.",
        dot_git.display(),
    ))
    .with_hint(
        "A worktree's or submodule's `.git` file holds one line, \
         `gitdir: <path>`.  Is this file something else?  If the tree is not a \
         repository, drop `gitdir:` from the policy or replace it with an \
         explicit path.",
    )
}

fn unreadable_message(dot_git: &Path, error: &std::io::Error) -> PolicyError {
    PolicyError::new(format!(
        "the .git file at '{}' cannot be read ({error}), so `gitdir:` cannot \
         say which git directory the grant covers.",
        dot_git.display(),
    ))
    .with_hint(
        "The freeze reads the pointer as the user launching the session.  Check \
         the file's permissions, or replace `gitdir:` in the policy with an \
         explicit path.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The submodule shape git writes, `worktree` under `[core]`.
    #[test]
    fn core_worktree_reads_the_key_from_its_own_section() {
        let text = "[core]\n\trepositoryformatversion = 0\n\tworktree = ../../../sub\n";
        assert_eq!(core_worktree_of(text), Some("../../../sub".to_string()));
    }

    /// A `worktree` key belongs to `[core]`; the same word elsewhere is a
    /// different key, and reading it would let an unrelated section claim a tree.
    #[test]
    fn core_worktree_ignores_the_key_in_another_section() {
        let text = "[core]\n\tbare = false\n[submodule \"sub\"]\n\tworktree = /elsewhere\n";
        assert_eq!(core_worktree_of(text), None);
    }

    #[test]
    fn core_worktree_drops_a_commented_key() {
        let text = "[core]\n\t# worktree = /elsewhere\n\t; worktree = /also-not\n";
        assert_eq!(core_worktree_of(text), None);
    }

    #[test]
    fn core_worktree_unquotes_a_quoted_value() {
        assert_eq!(
            core_worktree_of("[core]\n\tworktree = \"/work/tree\"\n"),
            Some("/work/tree".to_string())
        );
    }
}
