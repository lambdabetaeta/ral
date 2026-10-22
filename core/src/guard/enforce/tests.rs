//! Pins for [`GrantStack::admits_fs`] and the refusals' words: containment is judged on resolved
//! forms on both sides, so a symlink-spelled grant covers its target, in
//! both directions.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] test fs scaffolding: tempdir trees and symlinks for containment pins"
)]

use super::FsOp;
use crate::capability::{Capabilities, Floor, FsPolicy, GrantStack};
use crate::path::FrozenPath;
use crate::test_helper::{Tree, layer, located, trace};

fn stack(fs: FsPolicy) -> GrantStack {
    crate::test_helper::stack([layer(Capabilities {
        fs: Some(fs),
        ..Capabilities::default()
    })])
}

fn admits_read(grants: &GrantStack, path: &std::path::Path) -> bool {
    grants.admits_fs(&Floor::default(), &FsOp::Read, &trace(path))
}

#[test]
fn a_symlink_spelled_read_prefix_admits_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("SKILL.md"), "x").unwrap();
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let grants = stack(FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(crate::path::utf8(&link).unwrap())],
        ..FsPolicy::default()
    });
    assert!(
        admits_read(&grants, &real.join("SKILL.md")),
        "a prefix granted through a symlink must cover the resolved target"
    );
    assert!(
        !admits_read(&grants, &tmp.path().join("outside")),
        "the region is still the prefix, not the world"
    );
}

/// The threat is a stack with no `fs` opinion at all — the one shape
/// the fold answers `Ok` before consulting anything — so the
/// guard is tested on exactly that stack, through the typed door, and by
/// inode: a hard link is the same file under another name.
#[test]
fn a_write_onto_a_pinned_binary_is_guarded_before_any_grant_is_consulted() {
    use crate::capability::Refused;
    use crate::path::{LeafMode, LexicalPath, Reading, Writing, walk};
    crate::sandbox::pin();
    let (open, floor) = (GrantStack::root(), Floor::default());
    let held = |path: &std::path::Path| {
        let spelling = LexicalPath::absolute(path).expect("a test names an absolute path");
        walk(&spelling, LeafMode::Follow).expect("a test path walks")
    };
    let own = std::env::current_exe().expect("own path");

    assert!(
        matches!(
            open.judge::<Writing>(&floor, held(&own)),
            Err(Refused::Guarded("ral"))
        ),
        "a write onto the pinned executable must be guarded under an open stack"
    );
    assert!(
        open.judge::<Reading>(&floor, held(&own)).is_ok(),
        "the guard is over rewriting, not reading"
    );
    let tmp = tempfile::tempdir().unwrap();
    let other = tmp.path().join("other");
    std::fs::write(&other, "x").unwrap();
    assert!(
        open.judge::<Writing>(&floor, held(&other)).is_ok(),
        "an unpinned file is the stack's to decide"
    );
    let link = tmp.path().join("link-to-own");
    if std::fs::hard_link(&own, &link).is_ok() {
        assert!(
            matches!(
                open.judge::<Writing>(&floor, held(&link)),
                Err(Refused::Guarded("ral"))
            ),
            "a hard link names the pinned inode too"
        );
        assert!(
            open.judge::<Reading>(&floor, held(&link)).is_ok(),
            "a hard link to the executable is as readable as the executable"
        );
    }
}

#[test]
fn a_symlink_spelled_deny_covers_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    let secret = real.join("secret");
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::write(secret.join("SKILL.md"), "x").unwrap();
    let link = tmp.path().join("link-secret");
    std::os::unix::fs::symlink(&secret, &link).unwrap();

    let grants = stack(FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(crate::path::utf8(&real).unwrap())],
        deny_paths: vec![FrozenPath::from_surface(crate::path::utf8(&link).unwrap())],
        ..FsPolicy::default()
    });
    assert!(
        !admits_read(&grants, &secret.join("SKILL.md")),
        "a deny spelled through a symlink must cover the resolved target"
    );
    assert!(
        admits_read(&grants, &real.join("SKILL.md")),
        "the deny is the entry, not the whole read region"
    );
}

/// The refusal cites a folded name only when no deny holds the access as
/// stored, though the deepest deny holds it only by a fold.
#[test]
fn a_deny_holding_the_stored_name_refuses_plainly() {
    use super::{Refused, fs_judge};
    use crate::capability::Withheld;
    use crate::path::MatchedBy;
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let under = |denies: &[&str]| {
        stack(FsPolicy {
            read_prefixes: vec![FrozenPath::from_surface(crate::path::utf8(&root).unwrap())],
            deny_paths: (denies.iter())
                .map(|d| FrozenPath::from_surface(crate::path::utf8(&root.join(d)).unwrap()))
                .collect(),
            ..FsPolicy::default()
        })
    };
    let matched_by = |grants: &GrantStack| {
        let access = root.join("d/secrets/f");
        match fs_judge(grants, &Floor::default(), &trace(&access), &FsOp::Read) {
            Err(Refused::Withheld(Withheld::Excluded { matched_by, .. })) => matched_by,
            _ => panic!("an exclusion holds the access"),
        }
    };
    assert_eq!(matched_by(&under(&["d/Secrets"])), MatchedBy::FoldedName);
    assert_eq!(matched_by(&under(&["d", "d/Secrets"])), MatchedBy::Name);
}

mod words {
    use super::*;
    use crate::capability::{Evidence, ExecGrant, ExecKey, Program};
    use crate::guard::enforce::{check_exec, check_fs_exact};
    use crate::types::Context;

    fn ctx(layers: impl IntoIterator<Item = Capabilities>) -> Context {
        let mut ctx = Context::under_grants(GrantStack::root());
        layers.into_iter().for_each(|l| ctx.grants.push(layer(l)));
        ctx
    }

    fn exec(entries: &[(ExecKey, Evidence)]) -> Capabilities {
        Capabilities {
            exec: Some(entries.iter().cloned().collect::<ExecGrant>()),
            ..Capabilities::default()
        }
    }

    fn file(real: &str) -> Program {
        Program::File {
            arg0: real.into(),
            located: Box::new(located(real)),
        }
    }

    fn dir(path: &str) -> ExecKey {
        ExecKey::Dir(FrozenPath::from_surface(path))
    }

    fn refusal(ctx: &Context, real: &str) -> crate::types::Error {
        match check_exec(ctx, &Floor::default(), "t", file(real), vec![]) {
            Ok(_) => panic!("{real} should be refused"),
            Err(denial) => denial.error,
        }
    }

    /// A missing allowance says so and mentions an enclosing grant only where
    /// more than one layer supplies the axis.
    #[test]
    fn a_missing_allowance_mentions_an_enclosing_grant_only_when_nested() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = Tree::lay(tmp.path(), &["/x/t", "/other/"]);
        let t = tree.at("/x/t");
        let only = exec(&[(dir(&tree.at("/other")), Evidence::ALLOW)]);
        let one = refusal(&ctx([only.clone()]), &t);
        assert!(one.message.contains("lets it run"), "{}", one.message);
        assert!(!one.hint.unwrap().contains("enclosing grant"));
        let outer = exec(&[(dir(&tree.at("/x")), Evidence::ALLOW)]);
        let two = refusal(&ctx([outer, only]), &t);
        assert!(
            two.message
                .contains("some layer's exec `allow` list does not let it run"),
            "{}",
            two.message
        );
        assert!(two.hint.unwrap().contains("enclosing grant"));
    }

    /// An exclusion is named with its `dir` constructor, and no allowance is
    /// offered against it.
    #[test]
    fn an_exclusion_is_named_and_no_allowance_is_suggested() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = Tree::lay(tmp.path(), &["/x/t"]);
        let x = tree.at("/x");
        let both = exec(&[(dir(&x), Evidence::ALLOW), (dir(&x), Evidence::DENY)]);
        let e = refusal(&ctx([both]), &tree.at("/x/t"));
        let named = format!("`dir `absolute {}", crate::path::shown(&x));
        assert!(e.message.contains(&named), "{}", e.message);
        assert!(
            e.message.contains("cannot override an exclusion"),
            "{}",
            e.message
        );
        assert!(!e.hint.unwrap().contains("--extend"));
    }

    /// A name deny is cited as the author wrote it.  The program is a native
    /// binary named `sh` that the file `sh` resolved to cannot reach, so only
    /// the name veto fires.
    #[cfg(unix)]
    #[test]
    fn a_name_deny_is_named_as_written() {
        let tmp = tempfile::tempdir().unwrap();
        let sh = tmp.path().join("sh");
        std::fs::copy("/bin/true", &sh).unwrap();
        let deny = exec(&[
            (ExecKey::Name("sh".into()), Evidence::ALLOW),
            (ExecKey::Name("sh".into()), Evidence::DENY),
        ]);
        let e = refusal(&ctx([deny]), crate::path::utf8(&sh).unwrap());
        assert!(
            e.message.contains("excluded by `` `name sh ``"),
            "{}",
            e.message
        );
    }

    /// A program that may run but that fs does not let be read is refused for
    /// reading it.  A layer reads only what it and the layers beneath it run,
    /// so the fs layer sits beneath the exec layer.
    #[test]
    fn an_unreadable_program_is_refused_for_what_it_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = Tree::lay(tmp.path(), &["/x/t"]);
        let (x, t) = (tree.at("/x"), tree.at("/x/t"));
        let runs = exec(&[(dir(&x), Evidence::ALLOW)]);
        let reads = Capabilities {
            fs: Some(FsPolicy::default()),
            ..Capabilities::default()
        };
        let e = refusal(&ctx([reads, runs.clone()]), &t);
        assert!(
            e.message.contains("may run, but it is not readable"),
            "{}",
            e.message
        );
        let unrestricted = check_exec(&ctx([runs]), &Floor::default(), "t", file(&t), vec![]);
        assert!(unrestricted.is_ok());
    }

    /// A bundled tool is refused when an fs exclusion covers ral's binary,
    /// which the refusal names.
    #[test]
    fn a_bundled_tool_names_ral_when_its_binary_is_excluded() {
        let own = std::env::current_exe().unwrap();
        let excluding = Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![FrozenPath::from_surface("/")],
                deny_paths: vec![FrozenPath::from_surface(
                    crate::path::utf8(own.parent().unwrap()).unwrap(),
                )],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        };
        let ctx = ctx([excluding]);
        let Err(denial) = check_exec(
            &ctx,
            &Floor::default(),
            "ls",
            Program::Tool("ls".into()),
            vec![],
        ) else {
            panic!("an fs exclusion over ral's binary refuses a bundled tool");
        };
        assert!(
            denial.error.message.contains("built into ral"),
            "{}",
            denial.error.message
        );
        assert!(
            denial.error.message.contains("ral's binary"),
            "{}",
            denial.error.message
        );
    }

    /// An fs refusal names the missing permission.
    #[test]
    fn an_fs_refusal_names_reading_or_writing() {
        let ctx = ctx([Capabilities {
            fs: Some(FsPolicy::default()),
            ..Capabilities::default()
        }]);
        let (floor, nowhere) = (Floor::default(), trace("/nowhere/x"));
        let read = check_fs_exact(&ctx, &floor, &nowhere, &FsOp::Read);
        assert!(read.err().unwrap().error.message.contains("reading"));
        let write = check_fs_exact(&ctx, &floor, &nowhere, &FsOp::Write);
        assert!(write.err().unwrap().error.message.contains("writing"));
    }
}

/// Default APFS answers `SECRET` with `secret`: the walk must spell it as
/// stored, or the deny on `secret` never meets the access.
#[cfg(target_os = "macos")]
#[test]
fn a_case_variant_spelling_meets_the_deny() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let secret = root.join("secret");
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(secret.join("key"), "x").unwrap();
    if !root.join("SECRET").exists() {
        return;
    }
    let grants = stack(FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(crate::path::utf8(&root).unwrap())],
        deny_paths: vec![FrozenPath::from_surface(
            crate::path::utf8(&secret).unwrap(),
        )],
        ..FsPolicy::default()
    });
    let access = trace(root.join("SECRET/key"));
    assert_eq!(access.real().as_path(), secret.join("key"));
    assert!(!grants.admits_fs(&Floor::default(), &FsOp::Read, &access));
}
