#![allow(clippy::disallowed_methods, reason = "test code")]
// Unix-only: the fixtures are Unix path shapes, and the no-repository
// fallback anchors at `/`.
#![cfg(unix)]

//! The `gitdir` place, over the shapes a `.git` entry can take.
//!
//! It is a grant-shaping input — exarch's shipped `reasonable.exarch.ral`
//! grants `gitdir` read *and* write — and a `.git` file is written inside
//! the working tree, which under that grant the agent may write.  So the
//! pointer alone never decides the grant: the git directory it names must claim
//! this working tree back, and a pointer nothing claims is a policy error
//! rather than a wider grant.

use ral_core::authority::grant::freeze::{FreezeCtx, Place, PolicyError};
use ral_core::host::EnvVars;
use ral_core::path::CwdPath;
use std::path::{Path, PathBuf};

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ral-gitdir-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

fn freeze(cwd: &Path) -> Result<String, PolicyError> {
    let cwd = CwdPath::new(cwd.to_path_buf()).expect("a tempdir is absolute");
    FreezeCtx::at(EnvVars::new(), cwd)
        .place(&Place::Gitdir(".".into()))
        .map(|p| {
            p.expect("a gitdir place is never dead")
                .as_str()
                .to_string()
        })
}

fn frozen(cwd: &Path) -> String {
    freeze(cwd).expect("the pointer is claimed, so the grant freezes")
}

fn refused(cwd: &Path) -> PolicyError {
    freeze(cwd).expect_err("an unclaimed pointer must refuse")
}

/// A `.git` file that names no git directory at all is a separate refusal, and
/// says so rather than reporting an unclaimed path.
#[test]
fn gitdir_refuses_a_git_file_with_no_pointer_line() {
    let root = root("no-pointer");
    std::fs::create_dir_all(root.join("tree")).unwrap();
    std::fs::write(root.join("tree/.git"), "this is not a pointer\n").unwrap();

    let err = refused(&root.join("tree"));
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

    let out = frozen(&root.join("src"));
    assert_eq!(out, root.join(".git").display().to_string());
    let _ = std::fs::remove_dir_all(&root);
}

/// Outside a repository there is nothing to discover, and `FreezeCtx::place`'s
/// documented fallback is the cwd itself.
#[test]
fn gitdir_outside_a_repository_falls_back_to_the_cwd() {
    // The walk climbs every ancestor, so the fallback is only reachable from
    // a cwd with no `.git` anywhere above it — `/` is the one such directory.
    assert_eq!(frozen(Path::new("/")), "/");
}

/// A linked worktree's git directory names the working tree's `.git` file back
/// in its `gitdir` file.
#[test]
fn gitdir_takes_a_linked_worktrees_git_directory_that_claims_it_back() {
    let root = root("linked");
    let main = root.join("main/.git/worktrees/w");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::create_dir_all(root.join("wt")).unwrap();
    std::fs::write(main.join("gitdir"), format!("{}/wt/.git\n", root.display())).unwrap();
    std::fs::write(
        root.join("wt/.git"),
        format!("gitdir: {}\n", main.display()),
    )
    .unwrap();

    assert_eq!(frozen(&root.join("wt")), main.display().to_string());
    let _ = std::fs::remove_dir_all(&root);
}

/// A split repository names its tree in `core.worktree`.
#[test]
fn gitdir_takes_a_split_repository_whose_config_claims_the_tree() {
    let root = root("split");
    std::fs::create_dir_all(root.join("gd")).unwrap();
    std::fs::create_dir_all(root.join("wt")).unwrap();
    std::fs::write(
        root.join("gd/config"),
        format!("[core]\n\tworktree = {}/wt\n", root.display()),
    )
    .unwrap();
    std::fs::write(
        root.join("wt/.git"),
        format!("gitdir: {}/gd\n", root.display()),
    )
    .unwrap();

    assert_eq!(
        frozen(&root.join("wt")),
        root.join("gd").display().to_string()
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A back-pointer that exists and names another tree claims nothing here.
#[test]
fn gitdir_refuses_a_pointer_whose_target_claims_another_tree() {
    let root = root("other");
    let main = root.join("main/.git/worktrees/w");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::create_dir_all(root.join("wt")).unwrap();
    std::fs::create_dir_all(root.join("other")).unwrap();
    std::fs::write(root.join("other/.git"), "").unwrap();
    std::fs::write(
        main.join("gitdir"),
        format!("{}/other/.git\n", root.display()),
    )
    .unwrap();
    std::fs::write(
        root.join("wt/.git"),
        format!("gitdir: {}\n", main.display()),
    )
    .unwrap();

    let err = refused(&root.join("wt"));
    assert!(
        err.message.contains("does not name this working tree back"),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A tree the walk states and cannot hold still has a position, and the
/// claim is by position: it stands.
#[cfg(target_os = "macos")]
#[test]
fn gitdir_takes_a_claim_whose_tree_the_walk_only_states() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        return;
    }
    let root = root("unheld");
    std::fs::create_dir_all(root.join("gd")).unwrap();
    std::fs::create_dir_all(root.join("wt")).unwrap();
    std::fs::write(
        root.join("gd/config"),
        format!("[core]\n\tworktree = {}/wt\n", root.display()),
    )
    .unwrap();
    std::fs::write(
        root.join("wt/.git"),
        format!("gitdir: {}/gd\n", root.display()),
    )
    .unwrap();
    let wt = root.join("wt");
    std::fs::set_permissions(&wt, PermissionsExt::from_mode(0o300)).unwrap();

    let found = freeze(&wt);
    std::fs::set_permissions(&wt, PermissionsExt::from_mode(0o700)).unwrap();
    assert_eq!(found.unwrap(), root.join("gd").display().to_string());
    let _ = std::fs::remove_dir_all(&root);
}

/// A back-pointer reached through a link names the place the link resolves
/// to: the walks' positions agree.
#[test]
fn gitdir_takes_a_back_pointer_spelled_through_a_link() {
    let root = root("aliased");
    let main = root.join("main/.git/worktrees/w");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::create_dir_all(root.join("wt")).unwrap();
    std::os::unix::fs::symlink(root.join("wt"), root.join("alias")).unwrap();
    std::fs::write(
        main.join("gitdir"),
        format!("{}/alias/.git\n", root.display()),
    )
    .unwrap();
    std::fs::write(
        root.join("wt/.git"),
        format!("gitdir: {}\n", main.display()),
    )
    .unwrap();

    assert_eq!(frozen(&root.join("wt")), main.display().to_string());
    let _ = std::fs::remove_dir_all(&root);
}

/// A back-pointer naming a hard link of this tree's `.git` file names another
/// place: the claim is by position, never by the object's identity.
#[test]
fn gitdir_refuses_a_back_pointer_to_a_hard_link_of_the_pointer() {
    let root = root("hardlinked");
    let main = root.join("main/.git/worktrees/w");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::create_dir_all(root.join("wt")).unwrap();
    std::fs::create_dir_all(root.join("other")).unwrap();
    std::fs::write(
        root.join("wt/.git"),
        format!("gitdir: {}\n", main.display()),
    )
    .unwrap();
    std::fs::hard_link(root.join("wt/.git"), root.join("other/.git")).unwrap();
    std::fs::write(
        main.join("gitdir"),
        format!("{}/other/.git\n", root.display()),
    )
    .unwrap();

    let err = refused(&root.join("wt"));
    assert!(
        err.message.contains("does not name this working tree back"),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
