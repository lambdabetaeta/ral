//! An exclusion holds the object it was fixed to: by identity through every
//! name the object takes, by the walk's own descriptor against the
//! identity's reuse, and by position where the object is gone.

use super::super::super::exclusion::{Excluded, Hold};
use super::super::super::trace::Trace;
use super::super::resolve::{LeafMode, walk_excluded, walk_lenient};
use super::unix::{entered, root, spelled};
use super::{Around, LINK_READ};
use crate::path::{Depth, MatchedBy, Subject};
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};

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
fn deny_while(dir: &Path, step: impl FnMut(Around) + 'static) -> std::io::Result<Excluded> {
    symlink("tgt", dir.join("dlink")).unwrap();
    LINK_READ.with(|hook| *hook.borrow_mut() = Some(Box::new(step)));
    let excluded = walk_excluded(&spelled(&dir.join("dlink")));
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
            std::fs::rename(at.join("dlink"), at.join("away")).unwrap();
            symlink("other", at.join("dlink")).unwrap();
        }
    })
    .unwrap();
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
    })
    .unwrap();
    assert_eq!(excluded.target().unwrap().real().as_path(), dir.join("tgt"));
    assert_eq!(by(&excluded, &dir.join("other")), None);
}

#[test]
fn an_unlinked_link_cannot_anchor_a_deny_to_its_replacement() {
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
    match excluded {
        Ok(excluded) => {
            assert_eq!(excluded.target().unwrap().real().as_path(), dir.join("tgt"));
            assert_eq!(by(&excluded, &dir.join("other")), None);
        }
        #[cfg(target_os = "macos")]
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::EINVAL)),
        #[cfg(not(target_os = "macos"))]
        Err(e) => panic!("the held link could not be read: {e}"),
    }
}

/// A file removed and made again at its name is another object, met by the
/// deny's position; the old object's identity is held, so no file made while
/// the deny stands is given it.
#[test]
fn the_held_identity_is_not_reused_while_the_deny_stands() {
    let (_tmp, dir, excluded) = secret();
    let old = excluded.named().last_id();
    std::fs::remove_file(dir.join("d/secret")).unwrap();
    let held = excluded
        .holds()
        .next()
        .unwrap()
        .handle_for_test()
        .expect("the leaf is held");
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

#[test]
fn rehold_keeps_a_live_handle_after_rename_and_unlink() {
    let (_tmp, dir, mut excluded) = secret();
    let recorded = excluded.named().clone();
    std::fs::rename(dir.join("d/secret"), dir.join("elsewhere/x")).unwrap();
    excluded.rehold();
    assert_eq!(excluded.held_ids(), [Some(recorded.last_id())]);
    std::fs::remove_file(dir.join("elsewhere/x")).unwrap();
    excluded.rehold();
    assert_eq!(excluded.held_ids(), [Some(recorded.last_id())]);
    assert_eq!(excluded.named(), &recorded);
}

#[test]
fn rehold_of_an_absent_name_keeps_its_directory_after_the_name_is_made() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkdirs(&dir, &["d"]);
    let mut excluded = deny(&dir.join("d/.env"));
    let held = excluded.held_ids();
    excluded.rehold();
    assert_eq!(excluded.held_ids(), held);
    std::fs::write(dir.join("d/.env"), "").unwrap();
    excluded.rehold();
    assert_eq!(excluded.held_ids(), held);
    assert_eq!(by(&excluded, &dir.join("d/.env")), Some(MatchedBy::Name));
}

#[test]
fn an_unheld_hold_is_reacquired_only_at_its_recorded_identity() {
    let (_tmp, dir, excluded) = secret();
    let wire = serde_json::to_value(&excluded).unwrap();
    let mut present: Excluded = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(present.held_ids(), [None]);
    assert!(present.holds().next().unwrap().current().is_none());
    present.rehold();
    assert_eq!(present.held_ids(), excluded.held_ids());
    std::fs::rename(dir.join("d/secret"), dir.join("elsewhere/x")).unwrap();
    let mut absent: Excluded = serde_json::from_value(wire.clone()).unwrap();
    absent.rehold();
    assert_eq!(absent.held_ids(), [None]);
    std::fs::write(dir.join("d/secret"), "replacement").unwrap();
    let mut replaced: Excluded = serde_json::from_value(wire).unwrap();
    replaced.rehold();
    assert_eq!(replaced.held_ids(), [None]);
}

#[test]
fn current_follows_a_held_file_within_and_across_directories() {
    let (_tmp, dir, excluded) = secret();
    let held = excluded.holds().next().unwrap();
    assert_eq!(held.current().unwrap().as_path(), dir.join("d/secret"));
    assert_eq!(excluded.positions(), [excluded.named().real().clone()]);
    std::fs::rename(dir.join("d/secret"), dir.join("d/x")).unwrap();
    assert_eq!(held.current().unwrap().as_path(), dir.join("d/x"));
    std::fs::rename(dir.join("d/x"), dir.join("elsewhere/x")).unwrap();
    assert_eq!(held.current().unwrap().as_path(), dir.join("elsewhere/x"));
    let positions = excluded.positions();
    assert_eq!(positions.len(), 2);
    assert!(positions.contains(excluded.named().real()));
    assert!(positions.contains(&held.current().unwrap()));
    std::fs::remove_file(dir.join("elsewhere/x")).unwrap();
    assert_eq!(held.current(), None);
}

#[test]
fn current_does_not_find_a_hard_link_after_the_held_name_is_unlinked() {
    let (_tmp, dir, excluded) = secret();
    std::fs::hard_link(dir.join("d/secret"), dir.join("elsewhere/h")).unwrap();
    std::fs::remove_file(dir.join("d/secret")).unwrap();
    assert_eq!(excluded.holds().next().unwrap().current(), None);
}

#[test]
fn current_follows_a_directory_and_preserves_an_absent_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = root(&tmp);
    mkdirs(&dir, &["d"]);
    let directory = deny(&dir.join("d"));
    let absent = deny(&dir.join("d/.env"));
    std::fs::rename(dir.join("d"), dir.join("e")).unwrap();
    assert_eq!(
        directory
            .holds()
            .next()
            .unwrap()
            .current()
            .unwrap()
            .as_path(),
        dir.join("e")
    );
    assert_eq!(
        absent.holds().next().unwrap().current().unwrap().as_path(),
        dir.join("e/.env")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn current_keeps_a_literal_deleted_suffix_and_rejects_a_deleted_objects_namesake() {
    let (_tmp, dir, excluded) = secret();
    let name = dir.join("d/x (deleted)");
    std::fs::rename(dir.join("d/secret"), &name).unwrap();
    assert_eq!(
        excluded
            .holds()
            .next()
            .unwrap()
            .current()
            .unwrap()
            .as_path(),
        name
    );
    std::fs::remove_file(&name).unwrap();
    std::fs::write(dir.join("d/x (deleted) (deleted)"), "namesake").unwrap();
    assert_eq!(excluded.holds().next().unwrap().current(), None);
}

/// A hold crosses the wire as its trace and arrives unheld.
#[test]
fn a_hold_round_trips_as_its_trace_and_arrives_unheld() {
    let (_tmp, _dir, excluded) = secret();
    let named = excluded.holds().next().unwrap();
    let wire = serde_json::to_value(named).unwrap();
    assert_eq!(wire, serde_json::to_value(named.trace()).unwrap());
    let back: Hold = serde_json::from_value(wire).unwrap();
    assert!(back.handle_for_test().is_none());
    assert_eq!(&back, named);
    assert!(named.handle_for_test().is_some());
}
