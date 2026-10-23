//! An exclusion holds the object it was fixed to: by identity through every
//! name the object takes, by the walk's own descriptor against the
//! identity's reuse, and by position where the object is gone.

use super::super::*;
use super::unix::{entered, root, spelled};
use super::{Around, LINK_READ};
use crate::path::{Depth, MatchedBy, Subject};
use std::os::unix::fs::{MetadataExt, symlink};

fn deny(path: &Path) -> Excluded {
    walk_excluded(&spelled(path)).unwrap()
}

fn trace(path: &Path, mode: LeafMode) -> Trace {
    walk_lenient(&spelled(path), mode).unwrap()
}

/// How `deny` holds an access to `path` within it, if it does.
fn by(deny: &Excluded, path: &Path) -> Option<MatchedBy> {
    deny.matches(
        Subject::Access(&trace(path, LeafMode::Follow)),
        Depth::Within,
    )
}

fn mkdirs(dir: &Path, names: &[&str]) {
    for name in names {
        std::fs::create_dir_all(dir.join(name)).unwrap();
    }
}

/// A file `d/secret` denied, with `elsewhere` beside `d`.
fn secret() -> (tempfile::TempDir, PathBuf, Excluded) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkdirs(&dir, &["d", "elsewhere"]);
    std::fs::write(dir.join("d/secret"), "s").unwrap();
    let excluded = deny(&dir.join("d/secret"));
    (tmp, dir, excluded)
}

#[test]
fn a_deny_holds_its_file_after_a_rename_in_its_directory() {
    let (_tmp, dir, excluded) = secret();
    std::fs::rename(dir.join("d/secret"), dir.join("d/other")).unwrap();
    assert_eq!(
        by(&excluded, &dir.join("d/other")),
        Some(MatchedBy::Identity)
    );
    assert_eq!(by(&excluded, &dir.join("d/secret")), Some(MatchedBy::Name));
}

#[test]
fn a_deny_holds_its_file_moved_out_of_its_directory() {
    let (_tmp, dir, excluded) = secret();
    std::fs::rename(dir.join("d/secret"), dir.join("elsewhere/x")).unwrap();
    assert_eq!(
        by(&excluded, &dir.join("elsewhere/x")),
        Some(MatchedBy::Identity)
    );
}

/// A deny on a directory holds what the directory holds and nothing that
/// left it, or was linked from it by a process that could write it.
#[test]
fn a_deny_on_a_directory_does_not_follow_what_left_it() {
    let (_tmp, dir, _) = secret();
    let excluded = deny(&dir.join("d"));
    std::fs::hard_link(dir.join("d/secret"), dir.join("elsewhere/h")).unwrap();
    std::fs::rename(dir.join("d/secret"), dir.join("elsewhere/x")).unwrap();
    assert_eq!(by(&excluded, &dir.join("elsewhere/x")), None);
    assert_eq!(by(&excluded, &dir.join("elsewhere/h")), None);
}

#[test]
fn a_deny_on_an_absent_name_holds_it_after_its_directory_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkdirs(&dir, &["d"]);
    let excluded = deny(&dir.join("d/.env"));
    std::fs::rename(dir.join("d"), dir.join("d2")).unwrap();
    assert_eq!(
        by(&excluded, &dir.join("d2/.env")),
        Some(MatchedBy::Identity)
    );
    assert_eq!(by(&excluded, &dir.join("d2/other")), None);
}

#[test]
fn a_deny_on_an_absent_name_holds_it_through_a_recreated_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkdirs(&dir, &["d/a"]);
    let excluded = deny(&dir.join("d/a/b"));
    std::fs::remove_dir(dir.join("d/a")).unwrap();
    std::fs::rename(dir.join("d"), dir.join("d2")).unwrap();
    mkdirs(&dir, &["d2/a"]);
    assert_eq!(
        by(&excluded, &dir.join("d2/a/b")),
        Some(MatchedBy::Identity)
    );
}

/// The tail is compared under `collision_key`: a case-insensitive volume
/// answers `.ENV` with the file `.env` would be.
#[test]
fn a_moved_absent_name_is_held_under_every_spelling_of_its_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkdirs(&dir, &["d"]);
    let excluded = deny(&dir.join("d/.env"));
    std::fs::rename(dir.join("d"), dir.join("d2")).unwrap();
    assert_eq!(
        by(&excluded, &dir.join("d2/.ENV")),
        Some(MatchedBy::Identity)
    );
}

/// A deny spelled through a name that is not there, folded away, is fixed to
/// the object the spelling reaches, and holds it under another name.
#[test]
fn a_deny_through_a_name_folded_away_holds_its_object_by_identity() {
    let (_tmp, dir, _) = secret();
    let excluded = deny(&dir.join("d/missing/../secret"));
    std::fs::hard_link(dir.join("d/secret"), dir.join("elsewhere/h")).unwrap();
    assert_eq!(
        by(&excluded, &dir.join("elsewhere/h")),
        Some(MatchedBy::Identity)
    );
}

/// A deny spelled through a name folded away to a link holds the link and
/// what it reached, as a deny on the link's own name does.
#[test]
fn a_deny_through_a_name_folded_away_to_a_link_holds_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("tgt"), "t").unwrap();
    symlink("tgt", dir.join("dlink")).unwrap();
    let excluded = deny(&dir.join("missing/../dlink"));
    assert_eq!(excluded, deny(&dir.join("dlink")));
    assert_eq!(excluded.target().unwrap().real().as_path(), dir.join("tgt"));
    std::fs::rename(dir.join("tgt"), dir.join("moved")).unwrap();
    assert_eq!(by(&excluded, &dir.join("moved")), Some(MatchedBy::Identity));
}

/// A listing's record of an entry is the parent's trace plus its one step.
#[test]
fn a_listed_entry_is_held_by_its_identity() {
    let (_tmp, dir, excluded) = secret();
    std::fs::hard_link(dir.join("d/secret"), dir.join("elsewhere/h")).unwrap();
    let listed = entered(&dir.join("elsewhere")).unwrap();
    let entries: Vec<_> = listed.read_dir().unwrap().collect();
    let entry = entries.first().expect("the link is listed");
    let record = entry.listed().expect("it was seen");
    assert_eq!(
        excluded.matches(Subject::Listed(&record), Depth::Within),
        Some(MatchedBy::Identity)
    );
}

/// An exact deny holds the object, or the absent name, and not what lies
/// beneath it: its remaining names are the whole rest of the access's.
#[test]
fn an_exact_deny_holds_an_absent_name_after_its_directory_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkdirs(&dir, &["d"]);
    let excluded = deny(&dir.join("d/tool"));
    std::fs::rename(dir.join("d"), dir.join("e")).unwrap();
    let exact = |path: &str| {
        let access = trace(&dir.join(path), LeafMode::Follow);
        excluded.matches(Subject::Access(&access), Depth::Exact)
    };
    assert_eq!(exact("e/tool"), Some(MatchedBy::Identity));
    assert_eq!(exact("e/tool/x"), None);
}

/// The least way is cited: a hard link is the object under another name, and
/// a case variant of an absent name another spelling of it.
#[test]
fn a_hard_link_is_matched_by_identity_and_a_case_variant_by_folded_name() {
    let (_tmp, dir, excluded) = secret();
    std::fs::hard_link(dir.join("d/secret"), dir.join("elsewhere/h")).unwrap();
    assert_eq!(
        by(&excluded, &dir.join("elsewhere/h")),
        Some(MatchedBy::Identity)
    );
    assert_eq!(by(&excluded, &dir.join("d/secret")), Some(MatchedBy::Name));
    let absent = deny(&dir.join("d/Fresh"));
    assert_eq!(by(&absent, &dir.join("d/Fresh")), Some(MatchedBy::Name));
    assert_eq!(
        by(&absent, &dir.join("d/FRESH")),
        Some(MatchedBy::FoldedName)
    );
}

/// Two spellings of one directory through a bind mount: the mount's root has
/// the identity of the directory it shows, so the path under the mount is
/// the denied object's.  Run in a user and mount namespace of its own, where
/// `mount --bind` is allowed; the namespace re-runs this test with
/// `RAL_TEST_BIND` set.
#[cfg(target_os = "linux")]
#[test]
fn a_deny_holds_a_file_under_a_bind_mount_of_its_directory() {
    use std::process::Command;
    const ME: &str = "a_deny_holds_a_file_under_a_bind_mount_of_its_directory";
    if let Some(dir) = std::env::var_os("RAL_TEST_BIND") {
        let dir = PathBuf::from(dir);
        let excluded = deny(&dir.join("d/secret"));
        assert_eq!(
            by(&excluded, &dir.join("bound/secret")),
            Some(MatchedBy::Identity)
        );
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkdirs(&dir, &["d", "bound"]);
    std::fs::write(dir.join("d/secret"), "s").unwrap();
    let script =
        "mount --bind \"$DIR/d\" \"$DIR/bound\" && \"$TEST\" --exact \"$NAME\" --nocapture";
    let module = module_path!().split_once("::").map_or("", |(_, rest)| rest);
    let status = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--mount",
            "--propagation",
            "private",
        ])
        .args(["sh", "-c", script])
        .env("RAL_TEST_BIND", &dir)
        .env("DIR", &dir)
        .env("TEST", std::env::current_exe().unwrap())
        .env("NAME", format!("{module}::{ME}"))
        .status();
    match status.map(|s| s.code()) {
        Ok(Some(0)) => {}
        Ok(Some(101)) => panic!("the deny did not hold the file under the bind mount"),
        _ => eprintln!("skip: no user and mount namespace with `mount --bind` is available here"),
    }
}

/// A deny on a link holds the link and what it reached; once the link is
/// replaced at its name, a `NoFollow` probe of the name is still held, as is
/// the old target, and a `Follow` access through it, now reaching `other`, is
/// not.
#[test]
fn a_deny_on_a_link_holds_the_link_and_the_target_it_had() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("tgt"), "t").unwrap();
    std::fs::write(dir.join("other"), "o").unwrap();
    symlink("tgt", dir.join("dlink")).unwrap();
    let excluded = deny(&dir.join("dlink"));
    std::fs::remove_file(dir.join("dlink")).unwrap();
    symlink("other", dir.join("dlink")).unwrap();

    let probe = trace(&dir.join("dlink"), LeafMode::NoFollow);
    assert_eq!(
        excluded.matches(Subject::Access(&probe), Depth::Within),
        Some(MatchedBy::Name)
    );
    assert_eq!(by(&excluded, &dir.join("tgt")), Some(MatchedBy::Name));
    assert_eq!(by(&excluded, &dir.join("other")), None);
    assert_eq!(by(&excluded, &dir.join("dlink")), None);
}

/// The link is held by its own handle and the target by its own: renamed,
/// each is the same object under another name.
#[test]
fn a_deny_on_a_link_holds_both_objects_by_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    std::fs::write(dir.join("tgt"), "t").unwrap();
    symlink("tgt", dir.join("dlink")).unwrap();
    let excluded = deny(&dir.join("dlink"));
    std::fs::rename(dir.join("tgt"), dir.join("moved")).unwrap();
    std::fs::rename(dir.join("dlink"), dir.join("dlink2")).unwrap();

    assert_eq!(by(&excluded, &dir.join("moved")), Some(MatchedBy::Identity));
    let named = trace(&dir.join("dlink2"), LeafMode::NoFollow);
    assert_eq!(
        excluded.matches(Subject::Access(&named), Depth::Within),
        Some(MatchedBy::Identity)
    );
}

/// What a test does to `dlink`, a link to `tgt` that is being denied, between
/// the walk's hold of it and its read of the target, and after.
fn deny_while(dir: &Path, step: impl FnMut(Around) + 'static) -> Excluded {
    symlink("tgt", dir.join("dlink")).unwrap();
    LINK_READ.with(|hook| *hook.borrow_mut() = Some(Box::new(step)));
    let excluded = deny(&dir.join("dlink"));
    LINK_READ.with(|hook| *hook.borrow_mut() = None);
    excluded
}

#[test]
fn a_link_replaced_before_its_target_is_read_holds_the_target_the_held_link_named() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkdirs(&dir, &["tgt", "other"]);
    let at = dir.clone();
    let excluded = deny_while(&dir, move |around| {
        if around == Around::Before {
            std::fs::remove_file(at.join("dlink")).unwrap();
            symlink("other", at.join("dlink")).unwrap();
        }
    });
    assert_eq!(excluded.target().unwrap().real().as_path(), dir.join("tgt"));
    assert_eq!(by(&excluded, &dir.join("other")), None);
}

#[test]
fn a_link_swapped_out_and_back_around_the_read_holds_the_held_links_target() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkdirs(&dir, &["tgt", "other"]);
    let at = dir.clone();
    let excluded = deny_while(&dir, move |around| match around {
        Around::Before => {
            std::fs::rename(at.join("dlink"), at.join("away")).unwrap();
            symlink("other", at.join("dlink")).unwrap();
        }
        Around::After => {
            std::fs::remove_file(at.join("dlink")).unwrap();
            std::fs::rename(at.join("away"), at.join("dlink")).unwrap();
        }
    });
    assert_eq!(excluded.target().unwrap().real().as_path(), dir.join("tgt"));
    assert_eq!(by(&excluded, &dir.join("other")), None);
}

/// A file removed and made again at its name is another object, met by the
/// deny's position; the old object's identity is held, so no file made while
/// the deny stands is given it.
#[test]
fn the_held_identity_is_not_reused_while_the_deny_stands() {
    let (_tmp, dir, excluded) = secret();
    let old = excluded.named.trace.last_id();
    std::fs::remove_file(dir.join("d/secret")).unwrap();
    let held = excluded.named.handle.as_ref().expect("the leaf is held");
    assert_eq!(held.metadata().unwrap().nlink(), 0, "the inode is kept");
    std::fs::write(dir.join("d/secret"), "new").unwrap();

    let met = trace(&dir.join("d/secret"), LeafMode::Follow);
    assert_ne!(met.last_id(), old);
    assert_eq!(by(&excluded, &dir.join("d/secret")), Some(MatchedBy::Name));
    for n in 0..500 {
        let path = dir.join(format!("fresh{n}"));
        std::fs::write(&path, "").unwrap();
        assert_ne!(trace(&path, LeafMode::Follow).last_id(), old);
    }
}

/// An object found again at its position is held again; one gone, or
/// replaced there, leaves the exclusion unheld, its trace unchanged, and
/// holding by position and by the identities it recorded.
#[test]
fn rehold_keeps_the_object_found_and_lets_go_of_another() {
    let (_tmp, dir, mut excluded) = secret();
    let recorded = excluded.named.trace.clone();
    excluded.rehold();
    let held = excluded
        .named
        .handle
        .as_ref()
        .expect("still there, so held");
    assert_eq!(ObjectId::of(held).unwrap(), recorded.last_id());

    std::fs::hard_link(dir.join("d/secret"), dir.join("elsewhere/h")).unwrap();
    std::fs::remove_file(dir.join("d/secret")).unwrap();
    excluded.rehold();
    assert!(excluded.named.handle.is_none(), "gone");
    std::fs::write(dir.join("d/secret"), "new").unwrap();
    excluded.rehold();
    assert!(excluded.named.handle.is_none(), "replaced");

    assert_eq!(excluded.named.trace, recorded);
    assert_eq!(by(&excluded, &dir.join("d/secret")), Some(MatchedBy::Name));
    assert_eq!(
        by(&excluded, &dir.join("elsewhere/h")),
        Some(MatchedBy::Identity)
    );
}

/// The deepest object an absent name's walk opened is its directory, and it
/// is held until the name exists.
#[test]
fn rehold_of_an_absent_name_holds_its_directory_until_the_name_is_made() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkdirs(&dir, &["d"]);
    let mut excluded = deny(&dir.join("d/.env"));
    assert!(excluded.named.handle.is_some());
    excluded.rehold();
    assert!(excluded.named.handle.is_some());
    std::fs::write(dir.join("d/.env"), "").unwrap();
    excluded.rehold();
    assert!(excluded.named.handle.is_none());
    assert_eq!(by(&excluded, &dir.join("d/.env")), Some(MatchedBy::Name));
}

/// A hold crosses the wire as its trace and arrives unheld.
#[test]
fn a_hold_round_trips_as_its_trace_and_arrives_unheld() {
    let (_tmp, _dir, excluded) = secret();
    let wire = serde_json::to_value(&excluded.named).unwrap();
    assert_eq!(wire, serde_json::to_value(&excluded.named.trace).unwrap());
    let back: Hold = serde_json::from_value(wire).unwrap();
    assert!(back.handle.is_none());
    assert_eq!(back, excluded.named);
    assert!(excluded.named.handle.is_some());
}
