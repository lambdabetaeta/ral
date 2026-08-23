#![allow(clippy::disallowed_methods)]
// Unix-only: the fixtures are Unix path shapes, and the no-repository
// fallback anchors at `/`.
#![cfg(unix)]

//! The `gitdir:` sigil, over the shapes a `.git` entry can take.
//!
//! It is a grant-shaping input — exarch's shipped `reasonable.exarch.ral`
//! grants `'gitdir:'` read *and* write — and a `.git` file is written inside
//! the working tree, which under that grant the agent may write.  So the
//! pointer alone never decides the grant: the git directory it names must claim
//! this working tree back, and a pointer nothing claims is a policy error
//! rather than a wider grant.

use ral_core::path::sigil::{FreezeCtx, freeze_one};
use ral_core::types::PolicyError;
use std::path::{Path, PathBuf};

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ral-gitdir-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn freeze(entry: &str, cwd: &Path) -> Result<String, PolicyError> {
    freeze_one(
        entry,
        &FreezeCtx {
            home: Some("/h"),
            cwd,
        },
    )
    .map(|p| p.as_str().to_string())
}

fn frozen(entry: &str, cwd: &Path) -> String {
    freeze(entry, cwd).expect("the pointer is claimed, so the grant freezes")
}

fn refused(entry: &str, cwd: &Path) -> PolicyError {
    freeze(entry, cwd).expect_err("an unclaimed pointer must refuse")
}

/// A worktree: `.git` is a *file* holding `gitdir: <path>`, and the git
/// directory holds a `gitdir` file naming that very file.  The pointer is read,
/// joined against the file's parent, folded, and the sigil lands in the real
/// git directory — not in the worktree the cwd sits in.
#[test]
fn gitdir_follows_a_pointer_the_git_directory_claims() {
    let root = root("worktree");
    let real = root.join("main/.git/worktrees/wt");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(root.join("wt")).unwrap();
    std::fs::write(root.join("wt/.git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();
    std::fs::write(
        real.join("gitdir"),
        root.join("wt/.git").display().to_string(),
    )
    .unwrap();

    let out = frozen("gitdir:/index", &root.join("wt"));
    assert_eq!(out, real.join("index").display().to_string());
    assert!(
        !out.starts_with(root.join("wt").to_str().unwrap()),
        "the grant must not fall back to the worktree: {out}"
    );
    assert!(!out.contains(".."), "the pointer must be folded: {out}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The other record git keeps: a repository split off with
/// `--separate-git-dir`, and an absorbed submodule, claim their tree with
/// `core.worktree` in the config rather than with a `gitdir` file.
#[test]
fn gitdir_follows_a_pointer_claimed_by_core_worktree() {
    let root = root("separate");
    let real = root.join("repo.git");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(root.join("tree")).unwrap();
    std::fs::write(root.join("tree/.git"), "gitdir: ../repo.git\n").unwrap();
    std::fs::write(
        real.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tworktree = ../tree\n",
    )
    .unwrap();

    assert_eq!(
        frozen("gitdir:", &root.join("tree")),
        real.display().to_string()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The attack the claim rule answers: a `.git` file naming a directory holding
/// the user's secrets.  Nothing there claims the tree, so the session refuses
/// instead of freezing a grant over it.
#[test]
fn gitdir_refuses_an_absolute_pointer_at_an_unclaiming_directory() {
    let root = root("absolute-escape");
    let secrets = root.join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    std::fs::create_dir_all(root.join("tree")).unwrap();
    std::fs::write(
        root.join("tree/.git"),
        format!("gitdir: {}\n", secrets.display()),
    )
    .unwrap();

    let err = refused("gitdir:", &root.join("tree"));
    assert!(
        err.message.contains(&secrets.display().to_string()),
        "{err:?}"
    );
    assert!(
        err.message.contains("does not name this working tree back"),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A relative pointer climbs out of the tree just as far, and is refused on the
/// resolved path rather than on its spelling.
#[test]
fn gitdir_refuses_a_relative_pointer_climbing_out_of_the_tree() {
    let root = root("relative-escape");
    std::fs::create_dir_all(root.join("secrets")).unwrap();
    std::fs::create_dir_all(root.join("tree")).unwrap();
    std::fs::write(root.join("tree/.git"), "gitdir: ../secrets\n").unwrap();

    let err = refused("gitdir:", &root.join("tree"));
    assert!(
        err.message
            .contains(&root.join("secrets").display().to_string()),
        "the refusal names the resolved target: {err:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A git directory that claims *another* tree does not claim this one — the
/// shape a moved worktree leaves behind, and the hint names its repair.
#[test]
fn gitdir_refuses_a_git_directory_claiming_another_tree() {
    let root = root("stale");
    let real = root.join("main/.git/worktrees/wt");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(root.join("moved")).unwrap();
    std::fs::write(
        root.join("moved/.git"),
        "gitdir: ../main/.git/worktrees/wt\n",
    )
    .unwrap();
    std::fs::write(
        real.join("gitdir"),
        root.join("elsewhere/wt/.git").display().to_string(),
    )
    .unwrap();

    let err = refused("gitdir:", &root.join("moved"));
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains("git worktree repair"),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A `.git` file that names no git directory at all is a separate refusal, and
/// says so rather than reporting an unclaimed path.
#[test]
fn gitdir_refuses_a_git_file_with_no_pointer_line() {
    let root = root("no-pointer");
    std::fs::create_dir_all(root.join("tree")).unwrap();
    std::fs::write(root.join("tree/.git"), "this is not a pointer\n").unwrap();

    let err = refused("gitdir:", &root.join("tree"));
    assert!(err.message.contains("no `gitdir:` line"), "{err:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A plain clone's `.git` is a directory, and is the answer as it stands: there
/// is no pointer to distrust.
#[test]
fn gitdir_takes_a_plain_clone_directory_as_it_stands() {
    let root = root("clone");
    std::fs::create_dir_all(root.join(".git/objects")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();

    let out = frozen("gitdir:", &root.join("src"));
    assert_eq!(out, root.join(".git").display().to_string());
    let _ = std::fs::remove_dir_all(&root);
}

/// Outside a repository there is nothing to discover, and `freeze_one`'s
/// documented fallback is the cwd itself.
#[test]
fn gitdir_outside_a_repository_falls_back_to_the_cwd() {
    // The walk climbs every ancestor, so the fallback is only reachable from
    // a cwd with no `.git` anywhere above it — `/` is the one such directory.
    assert_eq!(frozen("gitdir:", Path::new("/")), "/");
}
