//! Git directory discovery, for the `gitdir:` sigil that `authority::grant::freeze`
//! expands at policy freeze, and the working tree's directory chain, for the
//! files a tool reads at every level of it.
//!
//! A `.git` *file* names its git directory in the working tree's own words, so
//! following it asks the other direction too: the git directory must name this
//! working tree back ([`claims`]), which is settled by walking both names and
//! comparing the positions the walks reached, never their identities.  A
//! pointer that does not, or cannot be read, is a [`BadPointer`]; why that is a
//! refusal and not a wider grant, and how it is worded, is the freeze's.  Every
//! file is read through the caller's `read`, which judges it: the freeze's
//! reads nothing governs, the tree walk's refuses what its region refuses.

use crate::path::lex::{anchor_on, fold_dots, parent_or_cwd};
use crate::path::site::Silent;
use crate::path::{CwdPath, LeafMode, LexicalPath, shown, walk};
use crate::site::site;
use std::io;
use std::path::{Path, PathBuf};

/// The first `.git` at or above `cwd` — a directory in a plain clone,
/// a file in a worktree or submodule.
fn find_git_entry(cwd: &Path) -> Option<PathBuf> {
    work_tree_root(cwd).map(|root| root.join(".git"))
}

/// The first ancestor of `cwd`, itself included, holding a `.git` entry —
/// file or directory, so a worktree or submodule is a root of its own.
fn work_tree_root(cwd: &Path) -> Option<&Path> {
    site!(Silent::GitRoot, cwd => ancestors())
        .find(|dir| site!(Silent::GitRoot, dir.join(".git") => exists()))
}

/// The directories from the working tree's root down to `cwd`, root first.
/// Outside a working tree, `cwd` alone.
pub fn work_tree_chain(cwd: &Path) -> Vec<&Path> {
    let mut chain: Vec<&Path> = match work_tree_root(cwd) {
        Some(root) => site!(Silent::GitWorkTreeChain, cwd => ancestors())
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
pub(crate) fn discover_git_dir(
    cwd: &Path,
    mut read: impl FnMut(&LexicalPath) -> io::Result<String>,
) -> Result<Option<PathBuf>, BadPointer> {
    let Some(dot_git) = find_git_entry(cwd) else {
        return Ok(None);
    };
    if site!(Silent::GitDotGit, &dot_git => is_dir()) {
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
        .and_then(|line| against(git_dir, line))
        .map_or_else(|| git_dir.to_path_buf(), |named| fold_dots(&named))
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
    read(&LexicalPath::absolute(path).ok_or_else(|| not_absolute(path))?)
}

fn not_absolute(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        crate::fact::PathIoLeaf::AbsoluteRequired(shown(path)),
    )
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
    let target = against(parent_or_cwd(dot_git), pointer)
        .ok_or_else(|| Pointer::Unreadable(not_absolute(dot_git)))?;
    Ok(fold_dots(&target))
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
        return against(gitdir, back.trim()).is_some_and(|named| same_path(&named, dot_git));
    }
    core_worktree(&gitdir.join("config"), read).is_some_and(|tree| {
        against(gitdir, &tree).is_some_and(|named| same_path(&named, parent_or_cwd(dot_git)))
    })
}

/// `path`, written relative to the directory `dir` (a git pointer or
/// back-pointer), as the host reads a name there ([`anchor_on`]); `None`
/// where `path` is relative and `dir` is no directory a name joins under.
fn against(dir: &Path, path: &str) -> Option<PathBuf> {
    anchor_on(
        CwdPath::new(dir.to_path_buf()).ok().as_ref(),
        &PathBuf::from(path),
    )
}

/// Two spellings of one place: both walked, and the positions their
/// transcripts reached compared, firmlink twins as one.  Not their
/// identities: a transcript's is read once, and a held handle does not keep
/// it the object's alone (an unmount revokes a macOS event-only descriptor
/// and a remount may reuse the id; a FAT id is a directory slot).  Two absent
/// names are not the same file.
fn same_path(a: &Path, b: &Path) -> bool {
    let reached = |p: &Path| {
        let located = walk(&LexicalPath::absolute(p)?, LeafMode::Follow).ok()?;
        located.trace().kind().map(|_| located.real().clone())
    };
    let (Some(a), Some(b)) = (reached(a), reached(b)) else {
        return false;
    };
    a.contains(&b) && b.contains(&a)
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
#[allow(clippy::disallowed_methods, reason = "test code")]
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

    /// Why a `.git` file in a fresh directory, read through `read`, is refused.
    fn bad_pointer(read: impl FnMut(&LexicalPath) -> io::Result<String>) -> Pointer {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".git"), "").unwrap();
        match discover_git_dir(tmp.path(), read) {
            Err(bad) => bad.why,
            Ok(_) => panic!("a bad pointer must be refused"),
        }
    }

    #[test]
    fn a_pointer_that_cannot_be_read_is_unreadable() {
        let denied =
            |_: &LexicalPath| -> io::Result<String> { Err(io::ErrorKind::PermissionDenied.into()) };
        assert!(matches!(bad_pointer(denied), Pointer::Unreadable(_)));
    }

    #[test]
    fn a_pointer_file_without_a_gitdir_line_names_nothing() {
        let body = |_: &LexicalPath| -> io::Result<String> { Ok("not a pointer\n".into()) };
        assert!(matches!(bad_pointer(body), Pointer::NoGitdirLine));
    }

    #[test]
    fn a_target_that_claims_nothing_is_unclaimed_and_named() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".git"), "gitdir: elsewhere\n").unwrap();
        let read = |p: &LexicalPath| std::fs::read_to_string(p.as_path());
        let Err(BadPointer {
            why: Pointer::Unclaimed(target),
            ..
        }) = discover_git_dir(tmp.path(), read)
        else {
            panic!("a target with no back-pointer must be unclaimed");
        };
        assert_eq!(target, tmp.path().join("elsewhere"));
    }

    #[test]
    fn common_dir_reads_the_named_directory_against_the_git_dir() {
        let root = std::env::temp_dir();
        let repo = root.join("r").join(".git");
        let git_dir = repo.join("worktrees").join("w");
        let common = root.join("common");
        let abs = format!("{}\n", common.display());
        for (commondir, expect) in [
            (Some("../..\n"), &repo),
            (Some(abs.as_str()), &common),
            (Some("\n"), &git_dir),
            (None, &git_dir),
        ] {
            let read = |_: &LexicalPath| -> io::Result<String> {
                commondir
                    .map(str::to_string)
                    .ok_or_else(|| io::ErrorKind::NotFound.into())
            };
            assert_eq!(common_dir(&git_dir, read), *expect, "{commondir:?}");
        }
    }
}
