use super::{Carving, Entry, EntryKind, Listing, carve};
use crate::path::{RealPath, shown};
use std::collections::{BTreeMap, BTreeSet};

/// A tree spelled one path per line: a trailing `/` is a directory, a
/// trailing `@` a link, anything else a file. Every parent is a directory.
struct Tree {
    dirs: BTreeMap<String, Vec<Entry>>,
    others: BTreeSet<String>,
}

impl Tree {
    fn new(paths: &[&str]) -> Self {
        let mut tree = Self {
            dirs: BTreeMap::new(),
            others: BTreeSet::new(),
        };
        for spelled in paths {
            let (path, kind) = if let Some(dir) = spelled.strip_suffix('/') {
                tree.dirs.entry(dir.to_owned()).or_default();
                (dir, EntryKind::Directory)
            } else if let Some(link) = spelled.strip_suffix('@') {
                tree.others.insert(link.to_owned());
                (link, EntryKind::Link)
            } else {
                tree.others.insert((*spelled).to_owned());
                (*spelled, EntryKind::Other)
            };
            let (parent, name) = path.rsplit_once('/').unwrap();
            tree.dirs.entry(parent.to_owned()).or_default().push(Entry {
                name: name.into(),
                kind,
            });
        }
        tree
    }

    fn list(&self, dir: &RealPath) -> Listing {
        let key = shown(dir.as_path());
        match self.dirs.get(&key) {
            Some(entries) => Listing::Directory(entries.clone()),
            None if self.others.contains(&key) => Listing::NotDirectory,
            None => Listing::Missing,
        }
    }
}

fn real(paths: &[&str]) -> Vec<RealPath> {
    paths.iter().map(|p| RealPath::assumed(*p)).collect()
}

/// The carving's two sets, spelled and sorted, and the directories listed.
fn plan(tree: &Tree, allow: &[&str], deny: &[&str]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut listed = Vec::new();
    let Carving { whole, entries } = carve(&real(allow), &real(deny), &mut |dir| {
        listed.push(shown(dir.as_path()));
        Ok::<_, ()>(tree.list(dir))
    })
    .unwrap();
    let spelled = |paths: Vec<RealPath>| {
        let mut out: Vec<String> = paths.iter().map(|p| shown(p.as_path())).collect();
        out.sort();
        out
    };
    listed.sort();
    (spelled(whole), spelled(entries), listed)
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
        let (whole, entries, listed) = plan(&cfg(), &["/r/cfg"], deny);
        assert_eq!(whole, strings(&["/r/cfg"]), "{deny:?}");
        assert!(entries.is_empty() && listed.is_empty(), "{deny:?}");
    }
}

#[test]
fn an_excluded_entry_is_withheld_and_its_siblings_granted() {
    let (whole, entries, listed) = plan(&cfg(), &["/r/cfg"], &["/r/cfg/gh"]);
    assert_eq!(whole, strings(&["/r/cfg/git", "/r/cfg/other.txt"]));
    assert_eq!(entries, strings(&["/r/cfg"]));
    assert_eq!(listed, strings(&["/r/cfg"]));
}

#[test]
fn a_deeper_exclusion_carves_every_directory_on_its_way() {
    let tree = Tree::new(&[
        "/a/", "/a/b/", "/a/b/c/", "/a/b/c/k", "/a/b/d", "/a/e/", "/a/e/f",
    ]);
    let (whole, entries, listed) = plan(&tree, &["/a"], &["/a/b/c"]);
    assert_eq!(whole, strings(&["/a/b/d", "/a/e"]));
    assert_eq!(entries, strings(&["/a", "/a/b"]));
    assert_eq!(listed, strings(&["/a", "/a/b"]));
}

#[test]
fn several_exclusions_carve_together() {
    let tree = Tree::new(&[
        "/a/", "/a/x", "/a/y", "/a/z", "/a/b/", "/a/b/c", "/a/b/d", "/a/e/", "/a/e/f",
    ]);
    let (whole, entries, _) = plan(&tree, &["/a"], &["/a/x", "/a/y", "/a/b/c"]);
    assert_eq!(whole, strings(&["/a/b/d", "/a/e", "/a/z"]));
    assert_eq!(entries, strings(&["/a", "/a/b"]));
}

/// The case that refusing was for: an exclusion of nothing yet. Every entry
/// there is granted, and the directory itself only carved, so a name made
/// later inherits nothing granted.
#[test]
fn an_absent_exclusion_grants_every_entry_and_carves_the_directory() {
    let (whole, entries, _) = plan(&cfg(), &["/r/cfg"], &["/r/cfg/op"]);
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
    let (whole, entries, _) = plan(&cfg(), &["/r/cfg"], &["/r/cfg/op/session"]);
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
    let (whole, _, _) = plan(&cfg(), &["/r/cfg"], &["/r/cfg/GH"]);
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
    let (whole, entries, listed) = plan(&tree, &["/r/cfg"], &["/r/cfg/gh", "/r/cfg/m/x"]);
    assert_eq!(whole, strings(&["/r/cfg/k"]));
    assert_eq!(entries, strings(&["/r/cfg"]));
    assert_eq!(listed, strings(&["/r/cfg"]));
}

/// Nothing can be carved from what is not a directory, so it is withheld.
#[test]
fn a_non_directory_with_an_exclusion_beneath_is_withheld() {
    let (whole, entries, _) = plan(&cfg(), &["/r/cfg"], &["/r/cfg/other.txt/x"]);
    assert_eq!(whole, strings(&["/r/cfg/gh", "/r/cfg/git"]));
    assert_eq!(entries, strings(&["/r/cfg"]));
    let (whole, entries, _) = plan(&cfg(), &["/r/cfg/other.txt"], &["/r/cfg/other.txt/x"]);
    assert!(whole.is_empty() && entries.is_empty());
}

#[test]
fn an_absent_allowance_grants_nothing() {
    let (whole, entries, listed) = plan(&cfg(), &["/r/gone"], &["/r/gone/x"]);
    assert!(whole.is_empty() && entries.is_empty());
    assert_eq!(listed, strings(&["/r/gone"]));
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
        let (whole, entries, listed) = plan(&cfg(), &["/r/cfg"], deny);
        assert_eq!(whole, strings(&["/r/cfg"]), "{deny:?}");
        assert!(entries.is_empty() && listed.is_empty(), "{deny:?}");
    }
}

#[test]
fn each_allowance_is_carved_on_its_own() {
    let (whole, entries, _) = plan(
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

#[test]
fn a_lister_error_ends_the_plan() {
    let tree = Tree::new(&["/a/", "/a/b/", "/a/b/c", "/a/d"]);
    let refused = carve(&real(&["/a"]), &real(&["/a/b/c"]), &mut |dir| {
        if shown(dir.as_path()) == "/a/b" {
            Err("unlistable")
        } else {
            Ok(tree.list(dir))
        }
    });
    assert_eq!(refused, Err("unlistable"));
}
