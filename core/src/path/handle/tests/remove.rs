//! `remove_all` on every host: what it judges, what it skips, and what a
//! failure names.

use std::convert::Infallible;
use std::io;
use std::path::Path;

use super::super::{Located, Writing};
use super::writing;
use crate::authority::policy::{Baseline, Capabilities, FsPolicy, GrantStack, Refused};
use crate::fact::PinnedBinary;
use crate::path::FrozenPath;
use crate::test_helper::located;

fn held(path: &Path) -> Located<Writing> {
    writing(located(path))
}

/// A denied descendant, or a baseline-pinned binary beneath a writable
/// directory, refuses the removal and stays.
#[test]
fn remove_all_refuses_denied_and_pinned_descendants() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    let secret = tree.join("sub").join("secret");
    std::fs::create_dir_all(tree.join("sub")).unwrap();
    std::fs::write(&secret, b"kept").unwrap();

    let layer = crate::test_helper::layer(Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![FrozenPath::root()],
            write_prefixes: vec![FrozenPath::root()],
            deny_paths: vec![FrozenPath::from_surface(
                located(&secret).real().as_path().to_str().unwrap(),
            )],
        }),
        ..Capabilities::default()
    });
    let mut shell = crate::test_helper::core_shell();
    shell.with_layer(layer, |shell| {
        let target = shell.judge::<Writing>(located(&tree)).unwrap();
        assert!(
            (target.remove_all(|child| shell.judge::<Writing>(child))).is_err(),
            "a denied descendant refuses the removal"
        );
    });
    assert_eq!(std::fs::read(&secret).unwrap(), b"kept");

    let pinned = tmp.path().join("pinned");
    let tool = pinned.join("tool");
    std::fs::create_dir(&pinned).unwrap();
    std::fs::write(&tool, b"pinned").unwrap();
    let id = (located(&tool).trace().leaf_id()).expect("a file has an identity");
    let baseline = Baseline::of_host()
        .unwrap()
        .with_images(None, vec![(id, PinnedBinary::Ral)]);
    let open = GrantStack::root();
    let target = (open.judge::<Writing, _>(&baseline, located(&pinned))).unwrap();
    assert!(matches!(
        target.remove_all(|child| open.judge::<Writing, _>(&baseline, child)),
        Err(Refused::Guarded(PinnedBinary::Ral))
    ));
    assert_eq!(std::fs::read(&tool).unwrap(), b"pinned");
}

/// An entry gone before its turn is skipped: here a sibling another process
/// removed while the first entry was judged.
#[test]
fn remove_all_skips_an_entry_gone_before_its_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("a"), b"").unwrap();
    std::fs::write(tree.join("b"), b"").unwrap();

    // Names are taken in order: `a` is judged before `b` is located.
    let Ok(removed) = held(&tree).remove_all(|child| {
        if child.real().as_path().ends_with("a") {
            std::fs::remove_file(tree.join("b")).unwrap();
        }
        Ok::<_, Infallible>(writing(child))
    });
    removed.unwrap();
    assert!(!tree.exists());
}

/// A failure names the entry it happened at, not the tree: here a directory
/// another process wrote into after it was listed.
#[test]
fn remove_all_names_the_entry_it_failed_at() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    let sub = tree.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("listed"), b"").unwrap();

    let Ok(removed) = held(&tree).remove_all(|child| {
        if child.real().as_path().ends_with("listed") {
            std::fs::write(sub.join("unlisted"), b"").unwrap();
        }
        Ok::<_, Infallible>(writing(child))
    });
    let (at, e) = removed.unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::DirectoryNotEmpty, "{e}");
    assert_eq!(at, *located(&sub).real());
    assert!(sub.join("unlisted").exists());
}
