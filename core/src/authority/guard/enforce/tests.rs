//! Pins for [`GrantStack::admits_fs`] and the refusals' words: containment is judged on resolved
//! forms on both sides, so a symlink-spelled grant covers its target, in
//! both directions.

use super::FsOp;
use crate::authority::policy::{Baseline, Capabilities, FsPolicy, GrantStack};
use crate::path::FrozenPath;
use crate::test_helper::{Tree, layer, located, trace};

fn stack(fs: FsPolicy) -> GrantStack {
    crate::test_helper::stack([layer(Capabilities {
        fs: Some(fs),
        ..Capabilities::default()
    })])
}

fn admits_read(grants: &GrantStack, path: &std::path::Path) -> bool {
    grants.admits_fs(&Baseline::default(), &FsOp::Read, &trace(path))
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
/// guard is tested on exactly that stack, through the typed entry, and by
/// inode: a hard link is the same file under another name.
#[test]
fn a_write_onto_a_pinned_binary_is_guarded_before_any_grant_is_consulted() {
    use crate::authority::policy::Refused;
    use crate::path::{LeafMode, LexicalPath, Reading, Writing, walk};
    crate::authority::sandbox::pin();
    let (open, baseline) = (GrantStack::root(), Baseline::of_host().unwrap());
    let held = |path: &std::path::Path| {
        let spelling = LexicalPath::absolute(path).expect("a test names an absolute path");
        walk(&spelling, LeafMode::Follow).expect("a test path walks")
    };
    let own = std::env::current_exe().expect("own path");

    assert!(
        matches!(
            open.judge::<Writing>(&baseline, held(&own)),
            Err(Refused::Guarded("ral"))
        ),
        "a write onto the pinned executable must be guarded under an open stack"
    );
    assert!(
        open.judge::<Reading>(&baseline, held(&own)).is_ok(),
        "the guard is over rewriting, not reading"
    );
    let tmp = tempfile::tempdir().unwrap();
    let other = tmp.path().join("other");
    std::fs::write(&other, "x").unwrap();
    assert!(
        open.judge::<Writing>(&baseline, held(&other)).is_ok(),
        "an unpinned file is the stack's to decide"
    );
    let link = tmp.path().join("link-to-own");
    if std::fs::hard_link(&own, &link).is_ok() {
        assert!(
            matches!(
                open.judge::<Writing>(&baseline, held(&link)),
                Err(Refused::Guarded("ral"))
            ),
            "a hard link names the pinned inode too"
        );
        assert!(
            open.judge::<Reading>(&baseline, held(&link)).is_ok(),
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
    use crate::authority::policy::Refused;
    use crate::authority::policy::Withheld;
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
        match grants.refusal_fs(&Baseline::default(), &FsOp::Read, &trace(&access)) {
            Err(Refused::Withheld(Withheld::Excluded { matched_by, .. })) => matched_by,
            _ => panic!("an exclusion holds the access"),
        }
    };
    assert_eq!(matched_by(&under(&["d/Secrets"])), MatchedBy::FoldedName);
    assert_eq!(matched_by(&under(&["d", "d/Secrets"])), MatchedBy::Name);
}

/// A hard link to a denied file is refused as the object under another name:
/// the one way the refusal is cited by identity.
#[test]
fn a_deny_holding_a_hard_link_cites_the_object_under_another_name() {
    use crate::authority::policy::Refused;
    use crate::authority::policy::Withheld;
    use crate::path::MatchedBy;
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    std::fs::write(root.join("secret"), "s").unwrap();
    std::fs::hard_link(root.join("secret"), root.join("alias")).unwrap();
    let grants = stack(FsPolicy {
        read_prefixes: vec![FrozenPath::from_surface(crate::path::utf8(&root).unwrap())],
        deny_paths: vec![FrozenPath::from_surface(
            crate::path::utf8(&root.join("secret")).unwrap(),
        )],
        ..FsPolicy::default()
    });
    let refused = grants
        .refusal_fs(
            &Baseline::default(),
            &FsOp::Read,
            &trace(root.join("alias")),
        )
        .expect_err("the alias is the denied object");
    assert!(
        matches!(
            refused,
            Refused::Withheld(Withheld::Excluded {
                matched_by: MatchedBy::Identity,
                ..
            })
        ),
        "{refused:?}"
    );
    assert!(
        refused.to_string().contains("is the denied object")
            && refused.to_string().contains("under another name"),
        "{refused}"
    );
}

mod words {
    use super::*;
    use crate::authority::guard::enforce::{Refusal, check_exec, fs_denial};
    use crate::authority::policy::{ExecGrant, ExecKey, Program};
    use crate::fact::Resource;

    fn grants(layers: impl IntoIterator<Item = Capabilities>) -> GrantStack {
        crate::test_helper::stack(layers.into_iter().map(layer))
    }

    fn exec(
        allow: impl IntoIterator<Item = ExecKey>,
        deny: impl IntoIterator<Item = ExecKey>,
    ) -> Capabilities {
        Capabilities {
            exec: Some(ExecGrant::of(allow, deny)),
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

    fn refusal(grants: &GrantStack, real: &str) -> Refusal {
        match check_exec(grants, &Baseline::default(), "t", file(real)) {
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
        let only = exec([dir(&tree.at("/other"))], []);
        let one = refusal(&grants([only.clone()]), &t);
        assert!(one.message.contains("lets it run"), "{}", one.message);
        assert!(!one.hint.unwrap().contains("enclosing grant"));
        let outer = exec([dir(&tree.at("/x"))], []);
        let two = refusal(&grants([outer, only]), &t);
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
        let both = exec([dir(&x)], [dir(&x)]);
        let e = refusal(&grants([both]), &tree.at("/x/t"));
        let named = format!("`dir `absolute {}", crate::path::shown(&x));
        assert!(e.message.contains(&named), "{}", e.message);
        assert!(
            e.message.contains("cannot override an exclusion"),
            "{}",
            e.message
        );
        assert!(!e.hint.unwrap().contains("--extend"));
    }

    #[test]
    fn an_exec_refusal_records_its_name_and_exclusion_without_the_resolved_target() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = Tree::lay(tmp.path(), &["/x/t"]);
        let x = dir(&tree.at("/x"));
        let grants = grants([exec([x.clone()], [x])]);
        let baseline = Baseline::default();
        let program = file(&tree.at("/x/t"));
        let Err(denial) = check_exec(&grants, &baseline, "t", program) else {
            panic!("an exclusion refuses the program");
        };
        assert_eq!(denial.check.resource, Resource::Exec);
        assert_eq!(denial.check.fields["name"], "t");
        assert!(!denial.check.fields.contains_key("resolved"));
        assert!(!denial.error.message.contains(&tree.at("/x/t")));
        assert!(denial.check.fields.contains_key("deny"));
        assert!(!denial.check.fields.contains_key("args"));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_under_an_admitted_folder_refuses_its_outside_target_without_disclosing_it() {
        use crate::authority::policy::{ExecDenial, Withheld};
        let tmp = tempfile::tempdir().unwrap();
        let tree = Tree::lay(tmp.path(), &["/tools/", "/elsewhere/prog"]);
        let link = tree.at("/tools/prog");
        let target = tree.at("/elsewhere/prog");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let only = exec([dir(&tree.at("/tools"))], []);
        let outer = exec([dir(&tree.at("/elsewhere"))], []);
        for layers in [vec![only.clone()], vec![outer, only]] {
            let grants = grants(layers);
            let baseline = Baseline::default();
            let refused = grants
                .admit(&baseline, file(&link))
                .expect_err("a link inside an admitted folder admits no outside object");
            assert!(matches!(
                refused.why,
                ExecDenial::Run(Withheld::MissingAllowance)
            ));
            let Err(denial) = check_exec(&grants, &baseline, &link, refused.program) else {
                panic!("the guard must refuse the same object");
            };
            assert_eq!(denial.check.resource, Resource::Exec);
            assert_eq!(
                denial.check.fields,
                std::collections::BTreeMap::from([("name".into(), link.clone())])
            );
            let Err(plain) = check_exec(&grants, &baseline, &link, file(&target)) else {
                panic!("the target is outside the admitted folder");
            };
            assert_eq!(denial.error, plain.error);
            for text in std::iter::once(&denial.error.message)
                .chain(denial.error.hint.iter())
                .chain(denial.check.fields.values())
            {
                assert!(!text.contains(&tree.at("/elsewhere")), "{text}");
            }
        }
    }

    /// A name deny is cited as the author wrote it.  The program is a native
    /// binary named `sh` that the file `sh` resolved to cannot reach, so only
    /// the name veto fires.
    #[cfg(unix)]
    #[test]
    fn a_name_deny_is_named_as_written() {
        let tmp = tempfile::tempdir().unwrap();
        let sh = tmp.path().join("sh");
        std::fs::copy("/usr/bin/true", &sh).unwrap();
        let deny = exec([ExecKey::Name("sh".into())], [ExecKey::Name("sh".into())]);
        let e = refusal(&grants([deny]), crate::path::utf8(&sh).unwrap());
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
        let runs = exec([dir(&x)], []);
        let reads = Capabilities {
            fs: Some(FsPolicy::default()),
            ..Capabilities::default()
        };
        let e = refusal(&grants([reads, runs.clone()]), &t);
        assert!(
            e.message.contains("may run, but it is not readable"),
            "{}",
            e.message
        );
        let unrestricted = check_exec(&grants([runs]), &Baseline::default(), "t", file(&t));
        assert!(unrestricted.is_ok());
    }

    /// A bundled tool is refused when an fs exclusion covers ral's binary,
    /// which the refusal names.
    #[test]
    fn a_bundled_tool_names_ral_when_its_binary_is_excluded() {
        crate::authority::sandbox::pin();
        let baseline = Baseline::of_host().unwrap();
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
        let grants = grants([excluding]);
        let Err(denial) = check_exec(&grants, &baseline, "ls", Program::Tool("ls")) else {
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

    /// An fs refusal names its op, then says of the path what the refusal
    /// alone says of "it".
    #[test]
    fn an_fs_refusal_names_its_op_in_the_refusals_one_wording() {
        let grants = grants([Capabilities {
            fs: Some(FsPolicy::default()),
            ..Capabilities::default()
        }]);
        let (baseline, nowhere) = (Baseline::default(), trace("/nowhere/x"));
        let path = crate::path::LexicalPath::absolute(nowhere.real().as_path()).unwrap();
        for op in [FsOp::Read, FsOp::Write] {
            let refused =
                (grants.refusal_fs(&baseline, &op, &nowhere)).expect_err("no allowance covers it");
            assert_eq!(
                refused.to_string(),
                "no allowance of the active grant covers it"
            );
            assert_eq!(
                fs_denial(&grants, refused, &op, &path).error.message,
                format!(
                    "fs {} denied by grant: no allowance of the active grant covers {}",
                    <&str>::from(&op),
                    path.shown()
                )
            );
        }
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
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "skip a_case_variant_spelling_meets_the_deny: the volume is case-sensitive"
        );
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
    assert!(!grants.admits_fs(&Baseline::default(), &FsOp::Read, &access));
}

/// The fact a refused fs check leaves on the trail: which op, at what path,
/// and by which exclusion or pinned binary.
mod fs_facts {
    use super::*;
    use crate::authority::guard::enforce::fs_denial;
    use crate::fact::Resource;
    use crate::path::LexicalPath;

    fn denial_at(
        grants: &GrantStack,
        baseline: &Baseline,
        op: &FsOp,
        at: &std::path::Path,
    ) -> crate::authority::guard::Denial {
        let path = LexicalPath::absolute(at).expect("an absolute path");
        match grants.refusal_fs(baseline, op, &trace(at)) {
            Ok(()) => panic!("{} should be refused", at.display()),
            Err(refused) => fs_denial(grants, refused, op, &path),
        }
    }

    #[test]
    fn a_missing_allowance_records_op_and_path_only() {
        let grants = stack(FsPolicy::default());
        for op in [FsOp::Read, FsOp::Write] {
            let at = std::path::Path::new("/nowhere/x");
            let denial = denial_at(&grants, &Baseline::default(), &op, at);
            let shown = LexicalPath::absolute(at).unwrap().shown();
            assert_eq!(denial.check.resource, Resource::Fs);
            assert_eq!(denial.check.fields["op"], <&str>::from(&op));
            assert_eq!(denial.check.fields["path"], shown);
            assert_eq!(denial.check.fields.len(), 2, "{:?}", denial.check.fields);
            assert!(denial.error.hint.is_some_and(|h| !h.is_empty()));
        }
    }

    #[test]
    fn an_exclusion_records_the_deny_beside_op_and_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let secret = root.join("secret");
        std::fs::write(&secret, "s").unwrap();
        let spell = |p: &std::path::Path| FrozenPath::from_surface(crate::path::utf8(p).unwrap());
        let grants = stack(FsPolicy {
            read_prefixes: vec![spell(&root)],
            write_prefixes: vec![spell(&root)],
            deny_paths: vec![spell(&secret)],
        });
        let denial = denial_at(&grants, &Baseline::default(), &FsOp::Read, &secret);
        let fields = &denial.check.fields;
        assert_eq!(fields["op"], <&str>::from(&FsOp::Read));
        assert_eq!(
            fields["path"],
            LexicalPath::absolute(&secret).unwrap().shown()
        );
        assert!(fields["deny"].contains("secret"), "{fields:?}");
        assert!(!fields.contains_key("pinned"));
        assert!(denial.error.hint.is_some_and(|h| !h.is_empty()));
    }

    #[test]
    fn a_pinned_binary_is_guarded_and_recorded_as_pinned() {
        crate::authority::sandbox::pin();
        let own = std::env::current_exe().expect("own path");
        let denial = denial_at(
            &GrantStack::root(),
            &Baseline::of_host().unwrap(),
            &FsOp::Write,
            &own,
        );
        let fields = &denial.check.fields;
        assert_eq!(denial.check.resource, Resource::Fs);
        assert_eq!(fields["op"], <&str>::from(&FsOp::Write));
        assert_eq!(fields["path"], LexicalPath::absolute(&own).unwrap().shown());
        assert_eq!(fields["pinned"], "ral");
        assert!(!fields.contains_key("deny"));
        assert!(denial.error.hint.is_some_and(|h| !h.is_empty()));
    }
}

#[test]
fn a_flag_is_permitted_by_silence_or_by_its_listing() {
    use crate::authority::guard::enforce::check_flag;
    use crate::authority::policy::{EditorVerb, Flag, ShellVerb};
    use std::collections::BTreeSet;
    let silent = GrantStack::root();
    let chdir_only = crate::test_helper::stack([layer(Capabilities {
        editor: Some(BTreeSet::new()),
        shell: Some(BTreeSet::from([ShellVerb::Chdir])),
        ..Capabilities::default()
    })]);
    for (grants, flag, permitted) in [
        (&silent, Flag::Shell(ShellVerb::Detach), true),
        (&silent, Flag::Editor(EditorVerb::Tui), true),
        (&chdir_only, Flag::Shell(ShellVerb::Chdir), true),
        (&chdir_only, Flag::Shell(ShellVerb::Detach), false),
        (&chdir_only, Flag::Editor(EditorVerb::Tui), false),
    ] {
        assert_eq!(
            check_flag(grants, flag, "who").is_ok(),
            permitted,
            "{flag:?}"
        );
    }
}
