//! Pins for [`GrantStack::admits_fs`] and the refusals' words: containment is judged on resolved
//! forms on both sides, so a symlink-spelled grant covers its target, in
//! both directions.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] test fs scaffolding: tempdir trees and symlinks for containment pins"
)]

use super::FsOp;
use crate::capability::{Capabilities, FsPolicy, GrantStack};
use crate::path::{FrozenPath, Resolver};

fn stack(fs: FsPolicy) -> GrantStack {
    GrantStack::of(Capabilities {
        fs: Some(fs),
        ..Capabilities::default()
    })
}

fn admits_read(grants: &GrantStack, path: &std::path::Path) -> bool {
    let cwd = crate::test_helper::process_cwd();
    let rp = Resolver::at(None, &cwd).anchor(path);
    grants.admits_fs(&FsOp::Read, &rp)
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
        read_prefixes: vec![FrozenPath::from_surface(&link)],
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
/// guard is tested on exactly that stack, and by inode: a hard link is
/// the same file under another name.
#[test]
fn a_write_onto_a_pinned_binary_is_guarded_before_any_grant_is_consulted() {
    use super::{FsRefusal, fs_judge};
    crate::sandbox::pin();
    let open = GrantStack::of(Capabilities::default());
    let own = std::env::current_exe().expect("own path");
    let verdict = |path: &std::path::Path, op: &FsOp| fs_judge(&open, path, op);

    assert!(
        matches!(verdict(&own, &FsOp::Write), Err(FsRefusal::Guarded("ral"))),
        "a write onto the pinned executable must be guarded under an open stack"
    );
    assert!(
        verdict(&own, &FsOp::Read).is_ok(),
        "the guard is over rewriting, not reading"
    );
    let tmp = tempfile::tempdir().unwrap();
    let other = tmp.path().join("other");
    std::fs::write(&other, "x").unwrap();
    assert!(
        verdict(&other, &FsOp::Write).is_ok(),
        "an unpinned file is the stack's to decide"
    );
    let link = tmp.path().join("link-to-own");
    if std::fs::hard_link(&own, &link).is_ok() {
        assert!(
            matches!(verdict(&link, &FsOp::Write), Err(FsRefusal::Guarded("ral"))),
            "a hard link names the pinned inode too"
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
        read_prefixes: vec![FrozenPath::from_surface(&real)],
        deny_paths: vec![FrozenPath::from_surface(&link)],
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

/// The refusal cites a respelling only when no deny holds the access as
/// stored, though the deepest deny holds it only by a fold.
#[test]
fn a_deny_holding_the_stored_name_refuses_plainly() {
    use super::{FsRefusal, fs_judge};
    use crate::capability::Withheld;
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let under = |denies: &[&str]| {
        stack(FsPolicy {
            read_prefixes: vec![FrozenPath::from_surface(&root)],
            deny_paths: (denies.iter())
                .map(|d| FrozenPath::from_surface(root.join(d)))
                .collect(),
            ..FsPolicy::default()
        })
    };
    let respelled = |grants: &GrantStack| {
        let access = root.join("d/secrets/f");
        match fs_judge(grants, &access, &FsOp::Read) {
            Err(FsRefusal::Withheld(Withheld::Excluded { respelled, .. })) => respelled,
            _ => panic!("an exclusion holds the access"),
        }
    };
    assert!(respelled(&under(&["d/Secrets"])));
    assert!(!respelled(&under(&["d", "d/Secrets"])));
}

mod words {
    use super::*;
    use crate::capability::{Evidence, ExecGrant, ExecKey, Program};
    use crate::guard::enforce::{check_exec, check_fs_exact};
    use crate::path::{RealPath, SearchCwd};
    use crate::types::Context;

    fn ctx(layers: impl IntoIterator<Item = Capabilities>) -> Context {
        let mut ctx = Context::under_grants(GrantStack::root());
        layers.into_iter().for_each(|l| ctx.grants.push(l));
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
            path: real.into(),
            real: RealPath::assumed(real),
        }
    }

    fn dir(path: &str) -> ExecKey {
        ExecKey::Dir(FrozenPath::for_test(path, path))
    }

    fn refusal(ctx: &Context, real: &str) -> crate::types::Error {
        match check_exec(ctx, "t", file(real), vec![]) {
            Ok(_) => panic!("{real} should be refused"),
            Err(denial) => denial.error,
        }
    }

    /// A missing allowance says so and mentions an enclosing grant only where
    /// more than one layer supplies the axis.
    #[test]
    fn a_missing_allowance_mentions_an_enclosing_grant_only_when_nested() {
        let only = exec(&[(dir("/other"), Evidence::ALLOW)]);
        let one = refusal(&ctx([only.clone()]), "/x/t");
        assert!(one.message.contains("lets it run"), "{}", one.message);
        assert!(!one.hint.unwrap().contains("enclosing grant"));
        let outer = exec(&[(dir("/x"), Evidence::ALLOW)]);
        let two = refusal(&ctx([outer, only]), "/x/t");
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
        let both = exec(&[(dir("/x"), Evidence::ALLOW), (dir("/x"), Evidence::DENY)]);
        let e = refusal(&ctx([both]), "/x/t");
        assert!(e.message.contains("`dir `absolute /x"), "{}", e.message);
        assert!(
            e.message.contains("cannot override an exclusion"),
            "{}",
            e.message
        );
        assert!(!e.hint.unwrap().contains("--extend"));
    }

    /// A name deny is cited as the author wrote it, not as the file it found.
    #[test]
    fn a_name_deny_is_named_as_written() {
        let path = crate::path::locate(
            "sh",
            std::env::var("PATH").ok().as_deref(),
            SearchCwd::nowhere(),
        )
        .expect("sh on PATH");
        let real = RealPath::of(&path).unwrap();
        let deny = exec(&[
            (ExecKey::Name("sh".into()), Evidence::ALLOW),
            (ExecKey::Name("sh".into()), Evidence::DENY),
        ]);
        let e = refusal(&ctx([deny]), &real.to_string());
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
        let runs = exec(&[(dir("/x"), Evidence::ALLOW)]);
        let reads = Capabilities {
            fs: Some(FsPolicy::default()),
            ..Capabilities::default()
        };
        let e = refusal(&ctx([reads, runs.clone()]), "/x/t");
        assert!(
            e.message.contains("may run, but it is not readable"),
            "{}",
            e.message
        );
        let unrestricted = check_exec(&ctx([runs]), "t", file("/x/t"), vec![]);
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
                deny_paths: vec![FrozenPath::from_surface(own.parent().unwrap())],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        };
        let ctx = ctx([excluding]);
        let Err(denial) = check_exec(&ctx, "ls", Program::Tool("ls".into()), vec![]) else {
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
        let read = check_fs_exact(&ctx, std::path::Path::new("/nowhere/x"), &FsOp::Read);
        assert!(read.err().unwrap().error.message.contains("reading"));
        let write = check_fs_exact(&ctx, std::path::Path::new("/nowhere/x"), &FsOp::Write);
        assert!(write.err().unwrap().error.message.contains("writing"));
    }
}

/// Default APFS answers `SECRET` with `secret`: the walk must spell it as
/// stored, or the deny on `secret` never meets the access.
#[cfg(target_os = "macos")]
#[test]
fn a_case_variant_spelling_meets_the_deny() {
    use crate::path::walk::{Leaf, walk};
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let secret = root.join("secret");
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(secret.join("key"), "x").unwrap();
    if !root.join("SECRET").exists() {
        return;
    }
    let grants = stack(FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(&root)],
        deny_paths: vec![FrozenPath::from_surface(&secret)],
        ..FsPolicy::default()
    });
    let cwd = crate::test_helper::process_cwd();
    let rp = Resolver::at(None, &cwd).anchor(&root.join("SECRET/key"));
    let located = walk(&rp, Leaf::Resolve).unwrap();
    assert_eq!(located.real(), secret.join("key"));
    assert!(!admits_read(&grants, located.real()));
}
