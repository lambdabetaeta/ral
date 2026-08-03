#![allow(clippy::disallowed_methods)]
// Unix-only: the fixtures are Unix path shapes, and the no-repository
// fallback anchors at `/`.
#![cfg(unix)]

//! The `gitdir:` sigil, over the three shapes a `.git` entry can take.
//!
//! It is a grant-shaping input — exarch's shipped `reasonable.exarch.ral`
//! grants `'gitdir:'` read and write — and `freeze_one` degrades silently to
//! the cwd when discovery returns `None`.  Under a plain clone that
//! degradation is invisible, so only a worktree fixture can tell a followed
//! pointer from a grant that quietly became "the worktree".

use ral_core::path::sigil::{FreezeCtx, freeze_one};
use std::path::{Path, PathBuf};

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ral-gitdir-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn frozen(entry: &str, cwd: &Path) -> String {
    freeze_one(entry, &FreezeCtx { home: "/h", cwd })
        .expect("gitdir: never fails")
        .as_str()
        .to_string()
}

/// A worktree's `.git` is a *file* holding `gitdir: <relative path>`.  The
/// pointer is read, joined against the file's parent, folded, and the sigil
/// lands in the real git directory — not in the worktree the cwd sits in.
#[test]
fn gitdir_follows_a_worktree_pointer_file() {
    let root = root("worktree");
    let real = root.join("main/.git/worktrees/wt");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(root.join("wt")).unwrap();
    std::fs::write(root.join("wt/.git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();

    let out = frozen("gitdir:/index", &root.join("wt"));
    assert_eq!(out, real.join("index").display().to_string());
    assert!(
        !out.starts_with(root.join("wt").to_str().unwrap()),
        "the grant must not fall back to the worktree: {out}"
    );
    assert!(!out.contains(".."), "the pointer must be folded: {out}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A plain clone's `.git` is a directory, and is the answer as it stands.
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
