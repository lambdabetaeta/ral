use super::{Carving, carve};
use crate::path::{RealPath, shown};
use std::path::{Path, PathBuf};

/// A tree spelled one path per line, under a fresh real root that `/` stands
/// for: a trailing `/` is a directory, a trailing `@` a link, anything else a
/// file. Every parent is a directory.
struct Tree {
    _dir: tempfile::TempDir,
    root: RealPath,
}

impl Tree {
    fn new(paths: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let tree = Self {
            root: crate::test_helper::real(dir.path()),
            _dir: dir,
        };
        for spelled in paths {
            let at = tree.path(spelled.trim_end_matches(['/', '@']));
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            match spelled.chars().last() {
                Some('/') => std::fs::create_dir_all(&at).unwrap(),
                Some('@') => link(&at),
                _ => std::fs::write(&at, b"").unwrap(),
            }
        }
        tree
    }

    /// Where `spelled` lies on disk under the root.
    fn path(&self, spelled: &str) -> PathBuf {
        let names = spelled.split('/').filter(|name| !name.is_empty());
        names.fold(self.root.as_path().to_path_buf(), |at, name| at.join(name))
    }

    /// The position `spelled` names under the root, present or not.
    fn at(&self, spelled: &str) -> RealPath {
        crate::test_helper::place(self.path(spelled))
    }

    /// `real` as `at` spells it.
    fn spelled(&self, real: &RealPath) -> String {
        let names = real.relative_to(&self.root).expect("beneath the root");
        names.iter().fold(String::new(), |out, name| {
            format!("{out}/{}", shown(name.as_os_str()))
        })
    }
}

/// A link that leads nowhere in particular.
fn link(at: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink("elsewhere", at).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file("elsewhere", at).unwrap();
}

/// The carving's two sets, spelled and sorted.
fn plan(tree: &Tree, allow: &[&str], deny: &[&str]) -> (Vec<String>, Vec<String>) {
    let reals = |spelled: &[&str]| spelled.iter().map(|p| tree.at(p)).collect::<Vec<_>>();
    let Carving { whole, entries } = carve(&reals(allow), &reals(deny)).unwrap();
    let spelled = |paths: Vec<RealPath>| {
        let mut out: Vec<String> = paths.iter().map(|p| tree.spelled(p)).collect();
        out.sort();
        out
    };
    (spelled(whole), spelled(entries))
}

fn strings(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

fn cfg() -> Tree {
    Tree::new(&[
        "/r/cfg/",
        "/r/cfg/gh/",
        "/r/cfg/gh/hosts.yml",
        "/r/cfg/git/",
        "/r/cfg/git/config",
        "/r/cfg/other.txt",
    ])
}

#[test]
fn an_allowance_with_no_exclusion_beneath_is_granted_whole_unlisted() {
    for deny in [&[][..], &["/r/elsewhere"], &["/r/cfgx/gh"]] {
        let (whole, entries) = plan(&cfg(), &["/r/cfg"], deny);
        assert_eq!(whole, strings(&["/r/cfg"]), "{deny:?}");
        assert!(entries.is_empty(), "{deny:?}");
    }
}

#[test]
fn an_excluded_entry_is_withheld_and_its_siblings_granted() {
    let (whole, entries) = plan(&cfg(), &["/r/cfg"], &["/r/cfg/gh"]);
    assert_eq!(whole, strings(&["/r/cfg/git", "/r/cfg/other.txt"]));
    assert_eq!(entries, strings(&["/r/cfg"]));
}

#[test]
fn a_deeper_exclusion_carves_every_directory_on_its_way() {
    let tree = Tree::new(&[
        "/a/", "/a/b/", "/a/b/c/", "/a/b/c/k", "/a/b/d", "/a/e/", "/a/e/f",
    ]);
    let (whole, entries) = plan(&tree, &["/a"], &["/a/b/c"]);
    assert_eq!(whole, strings(&["/a/b/d", "/a/e"]));
    assert_eq!(entries, strings(&["/a", "/a/b"]));
}

#[test]
fn several_exclusions_carve_together() {
    let tree = Tree::new(&[
        "/a/", "/a/x", "/a/y", "/a/z", "/a/b/", "/a/b/c", "/a/b/d", "/a/e/", "/a/e/f",
    ]);
    let (whole, entries) = plan(&tree, &["/a"], &["/a/x", "/a/y", "/a/b/c"]);
    assert_eq!(whole, strings(&["/a/b/d", "/a/e", "/a/z"]));
    assert_eq!(entries, strings(&["/a", "/a/b"]));
}

/// The case that refusing was for: an exclusion of nothing yet. Every entry
/// there is granted, and the directory itself only carved, so a name made
/// later inherits nothing granted.
#[test]
fn an_absent_exclusion_grants_every_entry_and_carves_the_directory() {
    let (whole, entries) = plan(&cfg(), &["/r/cfg"], &["/r/cfg/op"]);
    assert_eq!(
        whole,
        strings(&["/r/cfg/gh", "/r/cfg/git", "/r/cfg/other.txt"])
    );
    assert_eq!(entries, strings(&["/r/cfg"]));
}

/// An absent directory on the way to the exclusion is not granted: there is
/// nothing there, and a grant would reach what is made there later.
#[test]
fn an_absent_directory_on_the_way_is_not_granted() {
    let (whole, entries) = plan(&cfg(), &["/r/cfg"], &["/r/cfg/op/session"]);
    assert_eq!(
        whole,
        strings(&["/r/cfg/gh", "/r/cfg/git", "/r/cfg/other.txt"])
    );
    assert_eq!(entries, strings(&["/r/cfg"]));
}

/// The deny reading of names: an exclusion withholds every spelling a
/// filesystem could take for its name.
#[test]
fn an_exclusion_withholds_every_spelling_of_its_name() {
    let (whole, _) = plan(&cfg(), &["/r/cfg"], &["/r/cfg/GH"]);
    assert_eq!(whole, strings(&["/r/cfg/git", "/r/cfg/other.txt"]));
}

#[test]
fn a_link_is_never_granted_nor_followed() {
    let tree = Tree::new(&[
        "/r/cfg/",
        "/r/cfg/gh/",
        "/r/cfg/l@",
        "/r/cfg/m@",
        "/r/cfg/k",
    ]);
    let (whole, entries) = plan(&tree, &["/r/cfg"], &["/r/cfg/gh", "/r/cfg/m/x"]);
    assert_eq!(whole, strings(&["/r/cfg/k"]));
    assert_eq!(entries, strings(&["/r/cfg"]));
}

/// Nothing can be carved from what is not a directory, so it is withheld.
#[test]
fn a_non_directory_with_an_exclusion_beneath_is_withheld() {
    let (whole, entries) = plan(&cfg(), &["/r/cfg"], &["/r/cfg/other.txt/x"]);
    assert_eq!(whole, strings(&["/r/cfg/gh", "/r/cfg/git"]));
    assert_eq!(entries, strings(&["/r/cfg"]));
    let (whole, entries) = plan(&cfg(), &["/r/cfg/other.txt"], &["/r/cfg/other.txt/x"]);
    assert!(whole.is_empty() && entries.is_empty());
}

#[test]
fn an_absent_allowance_grants_nothing() {
    let (whole, entries) = plan(&cfg(), &["/r/gone"], &["/r/gone/x"]);
    assert!(whole.is_empty() && entries.is_empty());
}

/// An exclusion on an allowance or around it is no carving's to answer: the
/// allowance passes whole, unlisted, for the backend to refuse — even with
/// another exclusion beneath it.
#[test]
fn an_exclusion_on_or_around_an_allowance_passes_it_whole() {
    for deny in [
        &["/r/cfg"][..],
        &["/r"],
        &["/r/cfg", "/r/cfg/gh"],
        &["/R/CFG"],
    ] {
        let (whole, entries) = plan(&cfg(), &["/r/cfg"], deny);
        assert_eq!(whole, strings(&["/r/cfg"]), "{deny:?}");
        assert!(entries.is_empty(), "{deny:?}");
    }
}

#[test]
fn each_allowance_is_carved_on_its_own() {
    let (whole, entries) = plan(
        &cfg(),
        &["/r/cfg", "/r/cfg/git", "/r/other"],
        &["/r/cfg/gh"],
    );
    assert_eq!(
        whole,
        strings(&["/r/cfg/git", "/r/cfg/git", "/r/cfg/other.txt", "/r/other"])
    );
    assert_eq!(entries, strings(&["/r/cfg"]));
}

/// What cannot be listed cannot be carved, and is not granted whole instead.
/// Skipped where permissions do not bind the process: it lists anything.
#[cfg(unix)]
#[test]
fn an_unlistable_directory_ends_the_plan() {
    use std::os::unix::fs::PermissionsExt;
    let tree = Tree::new(&["/a/", "/a/b/", "/a/b/c", "/a/d"]);
    let locked = tree.path("/a/b");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let refused = carve(&[tree.at("/a")], &[tree.at("/a/b/c")]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if let Err(unlisted) = refused {
        assert_eq!(tree.spelled(&unlisted.at), "/a/b");
    }
}
