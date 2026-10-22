//! Git directory discovery, for the `gitdir:` sigil that `guard::freeze`
//! expands at policy freeze, and the working tree's directory chain, for the
//! files a tool reads at every level of it.
//!
//! A `.git` *file* names its git directory in the working tree's own words, so
//! following it asks the other direction too: the git directory must name this
//! working tree back ([`claims`]), which is settled by holding both names and
//! comparing the identities held, never one the walk could not hold.  A
//! pointer that does not, or cannot be read, is a [`BadPointer`]; why that is a
//! refusal and not a wider grant, and how it is worded, is the freeze's.  Every
//! file is read through the caller's `read`, which judges it: the freeze's
//! reads nothing governs, the tree walk's refuses what its region refuses.

use crate::path::lex::{fold_dots, parent_or_cwd};
use crate::path::{LeafMode, LexicalPath, shown, walk};
use std::io;
use std::path::{Path, PathBuf};

/// The first `.git` at or above `cwd` — a directory in a plain clone,
/// a file in a worktree or submodule.
fn find_git_entry(cwd: &Path) -> Option<PathBuf> {
    work_tree_root(cwd).map(|root| root.join(".git"))
}

/// The first ancestor of `cwd`, itself included, holding a `.git` entry —
/// file or directory, so a worktree or submodule is a root of its own.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:git-root] probes each ancestor of the cwd for a .git entry at session start, before any grant governs; ral's own repository discovery, not a name the model gave"
)]
fn work_tree_root(cwd: &Path) -> Option<&Path> {
    cwd.ancestors().find(|dir| dir.join(".git").exists())
}

/// The directories from the working tree's root down to `cwd`, root first.
/// Outside a working tree, `cwd` alone.
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: the spellings from a root already found down to the cwd"
)]
pub fn work_tree_chain(cwd: &Path) -> Vec<&Path> {
    let mut chain: Vec<&Path> = match work_tree_root(cwd) {
        Some(root) => cwd
            .ancestors()
            .take_while(|dir| *dir != root)
            .chain([root])
            .collect(),
        None => vec![cwd],
    };
    chain.reverse();
    chain
}

/// Why a `.git` file cannot name a git directory.
pub(crate) enum Pointer {
    Unreadable(std::io::Error),
    NoGitdirLine,
    /// The target names no claim on this working tree.
    Unclaimed(PathBuf),
}

/// A `.git` file at `dot_git` that names no usable git directory.
pub(crate) struct BadPointer {
    pub dot_git: PathBuf,
    pub why: Pointer,
}

/// The git directory `cwd` belongs to, each file read through `read`.
///
/// A plain clone's `.git` as it stands, or the target of a `.git` pointer file
/// that claims this working tree.  `None` when no `.git` lies at or above `cwd`,
/// which leaves the sigil's fallback to its caller.
///
/// # Errors
/// A `.git` file that names no git directory (unreadable, or no `gitdir:`
/// line), or one whose target does not claim the working tree.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:git-dot-git] asks whether the .git entry session-start discovery found is a directory or a pointer file; ral's own repository discovery, not a name the model gave"
)]
pub(crate) fn discover_git_dir(
    cwd: &Path,
    mut read: impl FnMut(&LexicalPath) -> io::Result<String>,
) -> Result<Option<PathBuf>, BadPointer> {
    let Some(dot_git) = find_git_entry(cwd) else {
        return Ok(None);
    };
    if dot_git.is_dir() {
        return Ok(Some(dot_git));
    }
    let target = read_pointer(&dot_git, &mut read).map_err(|why| BadPointer {
        dot_git: dot_git.clone(),
        why,
    })?;
    if claims(&target, &dot_git, &mut read) {
        return Ok(Some(target));
    }
    Err(BadPointer {
        dot_git,
        why: Pointer::Unclaimed(target),
    })
}

/// The shared repository directory of the git directory `git_dir`: the one
/// its `commondir` names where it is a linked worktree's, else itself.  What
/// `info/exclude` lives in, not `.git/worktrees/<name>`.
pub(crate) fn common_dir(
    git_dir: &Path,
    mut read: impl FnMut(&LexicalPath) -> io::Result<String>,
) -> PathBuf {
    let named = read_at(&mut read, &git_dir.join("commondir")).ok();
    named
        .as_deref()
        .and_then(|text| text.lines().next())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map_or_else(
            || git_dir.to_path_buf(),
            |line| fold_dots(&against(git_dir, line)),
        )
}

/// The `info/exclude` file of the repository whose `.git` entry is `dot_git`,
/// a directory or a pointer file: `None` where a pointer names no directory.
pub(crate) fn info_exclude(
    dot_git: &Path,
    is_dir: bool,
    mut read: impl FnMut(&LexicalPath) -> io::Result<String>,
) -> Option<LexicalPath> {
    let shared = if is_dir {
        dot_git.to_path_buf()
    } else {
        common_dir(&read_pointer(dot_git, &mut read).ok()?, &mut read)
    };
    LexicalPath::absolute(&shared.join("info").join("exclude"))
}

/// `read` of the absolute path `path`.
fn read_at(
    read: &mut impl FnMut(&LexicalPath) -> io::Result<String>,
    path: &Path,
) -> io::Result<String> {
    let spelled = LexicalPath::absolute(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not an absolute path", shown(path)),
        )
    })?;
    read(&spelled)
}

/// A worktree's `.git` is a file whose body is `gitdir: <path>`, relative to
/// the file's own directory when it is not absolute.  Folded through the same
/// kernel that mints a `FrozenPath`, so what comes back can be matched
/// against grant prefixes.
fn read_pointer(
    dot_git: &Path,
    read: &mut impl FnMut(&LexicalPath) -> io::Result<String>,
) -> Result<PathBuf, Pointer> {
    let contents = read_at(read, dot_git).map_err(Pointer::Unreadable)?;
    let pointer = contents
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .ok_or(Pointer::NoGitdirLine)?;
    Ok(fold_dots(&against(parent_or_cwd(dot_git), pointer)))
}

/// Whether `gitdir` names `dot_git`'s working tree as its own, by whichever of
/// the two records git keeps is there: a linked worktree's `gitdir` file, holding
/// the path of the very `.git` file that points here, or `core.worktree` in
/// `config`, which `git init --separate-git-dir` and an absorbed submodule write
/// instead.  A back-pointer that exists is the answer, whether it names this tree
/// or another one.
fn claims(
    gitdir: &Path,
    dot_git: &Path,
    read: &mut impl FnMut(&LexicalPath) -> io::Result<String>,
) -> bool {
    if let Ok(back) = read_at(read, &gitdir.join("gitdir")) {
        return same_path(&against(gitdir, back.trim()), dot_git);
    }
    core_worktree(&gitdir.join("config"), read)
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

/// Two spellings of one object: both walked and both held while they are
/// compared, so the identities cannot have been recycled under the
/// comparison — identity, not spelling, so a firmlink pair is one file.
/// An operand the walk could not hold is no claim; two absent names are
/// not the same file.
fn same_path(a: &Path, b: &Path) -> bool {
    let hold = |p: &Path| walk(&LexicalPath::absolute(p)?, LeafMode::Follow).ok();
    let (Some(a), Some(b)) = (hold(a), hold(b)) else {
        return false;
    };
    matches!((a.held_id(), b.held_id()), (Some(x), Some(y)) if x == y)
}

fn core_worktree(
    config: &Path,
    read: &mut impl FnMut(&LexicalPath) -> io::Result<String>,
) -> Option<String> {
    core_worktree_of(&read_at(read, config).ok()?)
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
