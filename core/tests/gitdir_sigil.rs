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
