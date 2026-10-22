//! The tree walk, on a real filesystem: its options, its ignore rules and
//! what a grant takes from it.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] fixtures laid, linked and removed by name"
)]

use super::*;
use crate::capability::{Capabilities, Floor, FsPolicy, GrantStack};
use crate::path::{FrozenPath, relative_key, utf8};
use crate::process::{CancelCause, DurableRoot};
use crate::test_helper::{layer, stack};
use std::path::PathBuf;

/// A tempdir and its real path, which is what the walk names.
pub(super) fn tmp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let real = crate::test_helper::real(dir.path());
    let root = real.as_path().to_path_buf();
    (dir, root)
}

/// Files and directories laid under `root`: a directory where the name ends
/// in `/`, else a file holding the text.
pub(super) fn lay(root: &Path, files: &[(&str, &str)]) {
    for (name, text) in files {
        let at = root.join(name);
        if name.ends_with('/') {
            std::fs::create_dir_all(&at).unwrap();
        } else {
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(&at, text).unwrap();
        }
    }
}

fn link(target: &Path, at: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, at).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(target, at).unwrap();
}

pub(super) fn lexical(path: &Path) -> LexicalPath {
    LexicalPath::absolute(path).expect("a test path is absolute")
}

/// No grant: the root stack's region, which withholds nothing.
pub(super) fn open() -> ReadRegion {
    GrantStack::root().read_region(&Floor::default())
}

/// A grant that reads under `allow` but for `deny`.
fn reading(allow: &Path, deny: &[&Path]) -> ReadRegion {
    let frozen = |path: &Path| FrozenPath::from_surface(utf8(path).unwrap());
    let policy = FsPolicy {
        read_prefixes: vec![frozen(allow)],
        deny_paths: deny.iter().map(|path| frozen(path)).collect(),
        ..FsPolicy::default()
    };
    let caps = Capabilities {
        fs: Some(policy),
        ..Capabilities::default()
    };
    stack([layer(caps)]).read_region(&Floor::default())
}

pub(super) fn scope() -> Scope {
    Scope::clone(&DurableRoot::default())
}

fn run(walk: Walk, region: ReadRegion) -> Vec<WalkEntry> {
    let entries = walk.readable_by(region).entries(scope());
    entries
        .map(|entry| entry.expect("no scope is cancelled"))
        .collect()
}

/// The names of `entries` beneath `root`, root excluded, in walk order.
fn named(entries: &[WalkEntry], root: &Path) -> Vec<String> {
    let root = lexical(root);
    (entries.iter().filter(|entry| entry.depth > 0))
        .map(|entry| relative_key(&entry.shown, &root).expect("an entry lies beneath its root"))
        .collect()
}

/// What `walk` yields beneath `root` under no grant, sorted.
fn seen(root: &Path, walk: Walk) -> Vec<String> {
    let mut names = named(&run(walk, open()), root);
    names.sort();
    names
}

/// What the ignore rules leave of `root`, under no grant, sorted.
fn kept(root: &Path) -> Vec<String> {
    seen(root, Walk::respecting_ignores(&lexical(root)))
}

fn found<'a>(entries: &'a [WalkEntry], root: &Path, name: &str) -> &'a Found {
    let root = lexical(root);
    let entry = entries
        .iter()
        .find(|entry| relative_key(&entry.shown, &root).as_deref() == Some(name));
    &entry
        .unwrap_or_else(|| panic!("{name} was not yielded"))
        .found
}

#[test]
fn entries_come_root_first_then_depth_first_in_name_order() {
    let (_tmp, root) = tmp();
    lay(&root, &[("b/z", ""), ("b/y", ""), ("a", "")]);
    let entries = run(Walk::all(&lexical(&root)), open());
    assert_eq!(
        entries.iter().map(|e| e.depth).collect::<Vec<_>>(),
        [0, 1, 1, 2, 2]
    );
    let mut names = vec![String::new()];
    names.extend(named(&entries, &root));
    assert_eq!(names, ["", "a", "b", "b/y", "b/z"]);
    assert!(entries[1].is_file() && !entries[2].is_file());
}

#[test]
fn a_dangling_link_is_an_entry_of_kind_symlink() {
    let (_tmp, root) = tmp();
    link(&root.join("nowhere"), &root.join("dangling"));
    for walk in [
        Walk::all(&lexical(&root)),
        Walk::all(&lexical(&root)).follow_links(),
    ] {
        let entries = run(walk, open());
        assert!(matches!(
            found(&entries, &root, "dangling"),
            Found::Seen(stat) if stat.kind == Kind::Symlink
        ));
    }
}

#[test]
fn a_dangling_root_link_is_the_only_entry_and_a_link() {
    let (_tmp, root) = tmp();
    let at = root.join("dangling");
    for target in [root.join("nowhere"), root.join("gone").join("x")] {
        link(&target, &at);
        for walk in [
            Walk::all(&lexical(&at)),
            Walk::all(&lexical(&at)).follow_links(),
        ] {
            let entries = run(walk, open());
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].shown, lexical(&at));
            assert!(matches!(
                &entries[0].found,
                Found::Seen(stat) if stat.kind == Kind::Symlink
            ));
        }
        #[cfg(unix)]
        std::fs::remove_file(&at).unwrap();
        #[cfg(windows)]
        std::fs::remove_dir(&at).unwrap();
    }
}

#[test]
fn a_directory_the_region_refuses_is_not_entered() {
    let (_tmp, root) = tmp();
    lay(&root, &[("open/a", ""), ("secret/b", "")]);
    link(&root.join("secret"), &root.join("open/via"));
    let secret = root.join("secret");
    let walk = Walk::all(&lexical(&root)).follow_links();
    let entries = run(walk, reading(&root, &[&secret]));
    let mut names = named(&entries, &root);
    names.sort();
    assert_eq!(names, ["open", "open/a", "open/via"]);
    assert!(matches!(
        found(&entries, &root, "open/via"),
        Found::Unentered { why: Unentered::Refused(e), .. }
            if e.kind() == io::ErrorKind::PermissionDenied
    ));
}

#[test]
fn a_denied_gitignore_is_not_applied_and_its_directory_is_entered() {
    let (_tmp, root) = tmp();
    let repo = root.join("repo");
    lay(
        &repo,
        &[
            (".git/", ""),
            (".gitignore", "skip.txt\n"),
            ("skip.txt", ""),
            ("keep.txt", ""),
        ],
    );
    assert_eq!(kept(&repo), ["keep.txt"]);
    let walk = Walk::respecting_ignores(&lexical(&repo));
    let region = reading(&root, &[&repo.join(".gitignore")]);
    let mut names = named(&run(walk, region), &repo);
    names.sort();
    assert_eq!(names, ["keep.txt", "skip.txt"]);
}

#[test]
fn a_denied_dot_git_makes_its_directory_no_repository() {
    let (_tmp, root) = tmp();
    let repo = root.join("repo");
    lay(
        &repo,
        &[
            (".git", "gitdir: /nowhere\n"),
            (".gitignore", "skip.txt\n"),
            ("skip.txt", ""),
            ("keep.txt", ""),
        ],
    );
    assert_eq!(kept(&repo), ["keep.txt"]);
    let walk = Walk::respecting_ignores(&lexical(&repo));
    let region = reading(&root, &[&repo.join(".git")]);
    let mut names = named(&run(walk, region), &repo);
    names.sort();
    assert_eq!(names, ["keep.txt", "skip.txt"]);
}

#[test]
fn an_ancestors_denied_ignore_is_not_applied_and_the_walk_proceeds() {
    let (_tmp, root) = tmp();
    let work = root.join("work");
    lay(
        &root,
        &[
            (".ignore", "skip.txt\n"),
            ("work/skip.txt", ""),
            ("work/keep.txt", ""),
        ],
    );
    assert_eq!(kept(&work), ["keep.txt"]);
    let walk = Walk::respecting_ignores(&lexical(&work));
    let mut names = named(&run(walk, reading(&work, &[])), &work);
    names.sort();
    assert_eq!(names, ["keep.txt", "skip.txt"]);
}

#[test]
fn the_filter_and_the_region_compose() {
    let (_tmp, root) = tmp();
    lay(&root, &[("a/x", ""), ("a/y", ""), ("b/z", "")]);
    let walk = Walk::all(&lexical(&root))
        .filter_entry(|shown| shown.as_path().file_name() != Some("b".as_ref()));
    let mut names = named(&run(walk, reading(&root, &[&root.join("a/y")])), &root);
    names.sort();
    assert_eq!(names, ["a", "a/x"]);
}

#[test]
fn a_link_followed_elsewhere_splices_dotdot_from_where_it_landed() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[("a/b/", ""), ("a/c/t.txt", ""), ("c/wrong.txt", "")],
    );
    link(&root.join("a/b"), &root.join("link"));
    link(Path::new("../c"), &root.join("a/b/next"));
    let names = seen(&root, Walk::all(&lexical(&root)).follow_links());
    assert!(names.contains(&"link/next/t.txt".to_string()), "{names:?}");
    assert!(names.contains(&"a/b/next/t.txt".to_string()), "{names:?}");
    assert!(
        !names.iter().any(|name| name.starts_with("link/next/wrong")),
        "{names:?}"
    );
}

#[test]
fn an_ignore_file_that_links_above_its_directory_applies() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[
            ("rules", "skip.txt\n"),
            ("sub/skip.txt", ""),
            ("sub/keep.txt", ""),
        ],
    );
    link(Path::new("../rules"), &root.join("sub/.ignore"));
    assert_eq!(kept(&root.join("sub")), ["keep.txt"]);
}

#[test]
fn following_links_stops_at_a_directory_already_on_the_way_down() {
    let (_tmp, root) = tmp();
    lay(&root, &[("d/f", ""), ("other/o.txt", "")]);
    link(&root, &root.join("d/up"));
    link(&root.join("other"), &root.join("d/onward"));
    let entries = run(Walk::all(&lexical(&root)).follow_links(), open());
    assert!(matches!(
        found(&entries, &root, "d/up"),
        Found::Unentered {
            why: Unentered::Pruned,
            ..
        }
    ));
    let names = named(&entries, &root);
    assert!(names.contains(&"d/onward/o.txt".to_string()), "{names:?}");
    assert!(
        !names.iter().any(|name| name.starts_with("d/up/")),
        "{names:?}"
    );
}

/// `/dev` is a mount of its own on every Unix this runs on.
#[cfg(unix)]
#[test]
fn staying_on_one_file_system_stops_at_a_mount() {
    let slash = lexical(Path::new("/"));
    let walk = Walk::all(&slash)
        .same_file_system()
        .filter_entry(|shown| shown.as_path() == Path::new("/dev"));
    let entries = run(walk, open());
    assert!(matches!(
        found(&entries, Path::new("/"), "dev"),
        Found::Unentered {
            why: Unentered::Pruned,
            ..
        }
    ));
    assert_eq!(entries.len(), 2);
}

#[test]
fn the_depth_limit_prunes_what_it_does_not_enter() {
    let (_tmp, root) = tmp();
    lay(&root, &[("a/b/c.txt", "")]);
    let entries = run(Walk::all(&lexical(&root)).max_depth(Some(2)), open());
    assert_eq!(named(&entries, &root), ["a", "a/b"]);
    assert!(matches!(found(&entries, &root, "a"), Found::Seen(_)));
    assert!(matches!(
        found(&entries, &root, "a/b"),
        Found::Unentered {
            why: Unentered::Pruned,
            ..
        }
    ));
}

#[test]
fn a_hidden_root_and_an_ignored_root_asked_for_by_name_are_searched() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[
            (".hidden/x", ""),
            ("repo/.git/", ""),
            ("repo/.gitignore", "build/\n"),
            ("repo/build/out.txt", ""),
        ],
    );
    assert_eq!(kept(&root.join(".hidden")), ["x"]);
    assert_eq!(kept(&root.join("repo/build")), ["out.txt"]);
    assert_eq!(kept(&root.join("repo")), Vec::<String>::new());
}

#[test]
fn a_root_that_is_a_link_is_entered_through_it() {
    let (_tmp, root) = tmp();
    lay(&root, &[("real/a", "")]);
    let at = root.join("link");
    link(&root.join("real"), &at);
    let entries = run(Walk::respecting_ignores(&lexical(&at)), open());
    assert_eq!(named(&entries, &at), ["a"]);
    assert_eq!(entries[0].shown, lexical(&at));
    assert_eq!(entries[1].shown, lexical(&at.join("a")));
}

#[test]
fn a_root_that_is_a_file_is_the_only_entry() {
    let (_tmp, root) = tmp();
    lay(&root, &[("f.txt", "")]);
    let entries = run(
        Walk::respecting_ignores(&lexical(&root.join("f.txt"))),
        open(),
    );
    assert_eq!(entries.len(), 1);
    assert!(entries[0].is_file());
}

#[test]
fn a_gitignore_is_honoured_within_a_repository_only() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[
            ("repo/.git/", ""),
            ("repo/.gitignore", "*.log\n"),
            ("repo/a.log", ""),
            ("repo/a.txt", ""),
            ("loose/.gitignore", "*.log\n"),
            ("loose/a.log", ""),
        ],
    );
    assert_eq!(kept(&root.join("repo")), ["a.txt"]);
    assert_eq!(kept(&root.join("loose")), ["a.log"]);
}

#[test]
fn an_ignore_file_is_honoured_and_an_ancestors_outside_any_repository() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[
            (".ignore", "*.tmp\n"),
            ("work/.ignore", "*.log\n"),
            ("work/a.tmp", ""),
            ("work/a.log", ""),
            ("work/a.txt", ""),
        ],
    );
    assert_eq!(kept(&root.join("work")), ["a.txt"]);
}

#[test]
fn info_exclude_applies_through_a_dot_git_pointer_file() {
    let (_tmp, root) = tmp();
    let git = root.join("gitdir");
    lay(
        &root,
        &[
            ("gitdir/info/exclude", "skip.txt\n"),
            ("work/.git", &format!("gitdir: {}\n", shown(&git))),
            ("work/skip.txt", ""),
            ("work/keep.txt", ""),
        ],
    );
    assert_eq!(kept(&root.join("work")), ["keep.txt"]);
}

#[test]
fn an_info_exclude_keeps_the_lines_before_one_that_is_not_utf8() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[
            ("repo/.git/", ""),
            ("repo/skip.txt", ""),
            ("repo/keep.txt", ""),
        ],
    );
    std::fs::create_dir_all(root.join("repo/.git/info")).unwrap();
    std::fs::write(
        root.join("repo/.git/info/exclude"),
        b"skip.txt\n\xff\xfekeep.txt\n",
    )
    .unwrap();
    assert_eq!(kept(&root.join("repo")), ["keep.txt"]);
}

#[test]
fn a_linked_worktree_reads_info_exclude_through_its_commondir() {
    let (_tmp, root) = tmp();
    let linked = root.join("main/.git/worktrees/w");
    lay(
        &root,
        &[
            ("main/.git/info/exclude", "skip.txt\n"),
            ("main/.git/worktrees/w/commondir", "../..\n"),
            ("wt/.git", &format!("gitdir: {}\n", shown(&linked))),
            ("wt/skip.txt", ""),
            ("wt/keep.txt", ""),
        ],
    );
    assert_eq!(kept(&root.join("wt")), ["keep.txt"]);
}

#[test]
fn a_gitignore_with_a_byte_order_mark_applies() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[
            (".git/", ""),
            (".gitignore", "\u{feff}skip.txt\r\n"),
            ("skip.txt", ""),
            ("keep.txt", ""),
        ],
    );
    assert_eq!(kept(&root), ["keep.txt"]);
}

#[test]
fn ignore_beats_gitignore_in_one_directory() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[
            (".git/", ""),
            (".ignore", "!keep.txt\n"),
            (".gitignore", "keep.txt\n"),
            ("keep.txt", ""),
        ],
    );
    assert_eq!(kept(&root), ["keep.txt"]);
}

#[test]
fn an_ancestors_ignore_beats_a_deeper_gitignores_whitelist() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[
            (".git/", ""),
            (".ignore", "secret.txt\n"),
            ("sub/.gitignore", "!secret.txt\n"),
            ("sub/secret.txt", ""),
            ("sub/open.txt", ""),
        ],
    );
    assert_eq!(kept(&root), ["sub", "sub/open.txt"]);
}

#[test]
fn a_deeper_file_beats_an_ancestors_within_one_class() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[
            (".git/", ""),
            (".gitignore", "*.log\n"),
            ("sub/.gitignore", "!keep.log\n"),
            ("a.log", ""),
            ("sub/keep.log", ""),
            ("sub/b.log", ""),
        ],
    );
    assert_eq!(kept(&root), ["sub", "sub/keep.log"]);
}

#[test]
fn an_outer_gitignore_stops_at_a_nested_repository_and_an_outer_ignore_does_not() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[
            (".git/", ""),
            (".gitignore", "secret.txt\n"),
            (".ignore", "hush.txt\n"),
            ("nested/.git/", ""),
            ("nested/secret.txt", ""),
            ("nested/hush.txt", ""),
            ("secret.txt", ""),
        ],
    );
    assert_eq!(kept(&root), ["nested", "nested/secret.txt"]);
}

#[test]
fn a_whitelist_admits_a_dotfile_the_hidden_rule_would_skip() {
    let (_tmp, root) = tmp();
    lay(
        &root,
        &[
            (".git/", ""),
            (".gitignore", "!.visible\n"),
            (".visible", ""),
            (".other", ""),
        ],
    );
    assert_eq!(kept(&root), [".visible"]);
}

#[cfg(windows)]
#[test]
fn an_entry_with_the_hidden_attribute_is_hidden() {
    let (_tmp, root) = tmp();
    lay(&root, &[("shy.txt", ""), ("plain.txt", "")]);
    let status = std::process::Command::new("attrib")
        .arg("+h")
        .arg(root.join("shy.txt"))
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(kept(&root), ["plain.txt"]);
}

#[cfg(unix)]
#[test]
fn a_directory_the_open_refuses_is_unentered_with_its_error() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("skip: root enters every directory");
        return;
    }
    let (_tmp, root) = tmp();
    lay(&root, &[("closed/inside", ""), ("open", "")]);
    let closed = root.join("closed");
    std::fs::set_permissions(&closed, PermissionsExt::from_mode(0o000)).unwrap();
    let entries = run(Walk::all(&lexical(&root)), open());
    std::fs::set_permissions(&closed, PermissionsExt::from_mode(0o700)).unwrap();
    assert!(matches!(
        found(&entries, &root, "closed"),
        Found::Unentered { why: Unentered::Refused(e), .. }
            if e.kind() == io::ErrorKind::PermissionDenied
    ));
    assert_eq!(named(&entries, &root), ["closed", "open"]);
}

#[cfg(target_os = "macos")]
#[test]
fn a_root_the_walk_cannot_enter_is_unentered_with_the_walks_error() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("skip: root enters every directory");
        return;
    }
    let (_tmp, root) = tmp();
    lay(&root, &[("closed/inside", "")]);
    let closed = root.join("closed");
    for mode in [0o000, 0o400] {
        std::fs::set_permissions(&closed, PermissionsExt::from_mode(mode)).unwrap();
        let entries = run(Walk::all(&lexical(&closed)), open());
        std::fs::set_permissions(&closed, PermissionsExt::from_mode(0o700)).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].depth, 0);
        assert!(matches!(
            &entries[0].found,
            Found::Unentered { stat: Stat { kind: Kind::Dir, .. }, why: Unentered::Refused(e) }
                if e.kind() == io::ErrorKind::PermissionDenied
        ));
    }
}

#[cfg(unix)]
#[test]
fn a_root_beneath_an_unenterable_directory_is_unseen() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().is_root() {
        eprintln!("skip: root enters every directory");
        return;
    }
    let (_tmp, root) = tmp();
    lay(&root, &[("closed/inner/f", "")]);
    let closed = root.join("closed");
    std::fs::set_permissions(&closed, PermissionsExt::from_mode(0o000)).unwrap();
    let entries = run(Walk::all(&lexical(&closed.join("inner"))), open());
    std::fs::set_permissions(&closed, PermissionsExt::from_mode(0o700)).unwrap();
    assert_eq!(entries.len(), 1);
    assert!(matches!(
        &entries[0].found,
        Found::Unseen(e) if e.kind() == io::ErrorKind::PermissionDenied
    ));
}

#[test]
fn an_entry_removed_between_the_listing_and_its_stat_is_unseen() {
    let (_tmp, root) = tmp();
    lay(&root, &[("gone", ""), ("here", "")]);
    let entries = Walk::all(&lexical(&root))
        .readable_by(open())
        .entries(scope());
    let dir = entries.region.judge(walk_dir(&lexical(&root)).unwrap());
    let dir = dir.unwrap_or_else(|_| panic!("the root is readable"));
    let listing = dir.read_dir().unwrap();
    std::fs::remove_file(root.join("gone")).unwrap();
    let (items, _) = entries.admit(listing).unwrap();
    let kinds: Vec<_> = (items.iter())
        .map(|item| item.seen.as_ref().map_err(io::Error::kind).map(|_| ()))
        .collect();
    assert_eq!(kinds, [Err(io::ErrorKind::NotFound), Ok(())]);
}

#[test]
fn a_cancelled_scope_stops_the_walk_at_the_next_entry() {
    let (_tmp, root) = tmp();
    lay(&root, &[("a", ""), ("b", "")]);
    let node = DurableRoot::default();
    let mut entries = (Walk::all(&lexical(&root)).readable_by(open())).entries(Scope::clone(&node));
    assert!(entries.next().expect("the root").is_ok());
    node.cancel(CancelCause::Interrupted);
    assert!(entries.next().expect("the stop").is_err());
    assert!(entries.next().is_none());
}

#[test]
fn a_cancelled_scope_stops_a_listing_between_its_entries() {
    let (_tmp, root) = tmp();
    lay(&root, &[("a", ""), ("b", "")]);
    let node = DurableRoot::default();
    let entries = (Walk::all(&lexical(&root)).readable_by(open())).entries(Scope::clone(&node));
    let dir = entries.region.judge(walk_dir(&lexical(&root)).unwrap());
    let dir = dir.unwrap_or_else(|_| panic!("the root is readable"));
    let listing = dir.read_dir().unwrap();
    node.cancel(CancelCause::Interrupted);
    assert!(entries.admit(listing).is_err());
}

#[test]
fn an_admitted_walk_that_is_cancelled_aborts() {
    let shell = crate::test_helper::core_shell();
    shell.cancel_handle().cancel(CancelCause::Interrupted);
    let root = shell.anchor(&shell.cwd());
    let mut walk = Walk::respecting_ignores(&root).admitted(&shell, "walk", ".");
    let stop = walk.next().expect_err("a cancelled scope aborts the walk");
    assert_eq!(stop.code(), 130);
}

/// A link the walk fails to follow for a reason other than what it leads to
/// — here a target name longer than any directory holds — is reported as not
/// entered, never taken for a link to a file and its subtree dropped.
#[cfg(unix)]
#[test]
fn a_link_the_walk_could_not_follow_is_reported() {
    let (_tmp, root) = tmp();
    link(Path::new(&"n".repeat(300)), &root.join("long"));
    let entries = run(Walk::all(&lexical(&root)).follow_links(), open());
    assert!(matches!(
        found(&entries, &root, "long"),
        Found::Unentered { stat, why: Unentered::Refused(_) } if stat.kind == Kind::Symlink
    ));
}
