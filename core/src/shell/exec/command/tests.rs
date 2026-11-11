//! The operation's path, tested stage by stage: a submodule per former file.

mod request {
    use super::super::*;
    use crate::authority::policy::{Baseline, Capabilities, ExecGrant, ExecKey, GrantStack};
    use crate::path::FrozenPath;
    use std::path::Path;

    fn admits_command(ctx: &Context, command: Command<Request>) -> bool {
        !matches!(
            command.lookup(&ctx.grants, &Baseline::default()).state,
            Err(LookupFailure::Refused { .. })
        )
    }

    fn looked_up(command: Command<Request>) -> Command<Lookup> {
        let command = command.lookup(&GrantStack::root(), &Baseline::default());
        assert!(!matches!(command.state, Err(LookupFailure::Refused { .. })));
        command
    }

    #[test]
    fn a_tilde_path_is_shown_expanded_against_env_home() {
        let mut shell = crate::test_helper::core_shell();
        shell.context.set_env_var("HOME", "/tmp/home").unwrap();
        let command = Command::from_name(
            &CommandName::TildePath(crate::path::TildePath {
                suffix: Some("/.local/bin/claude".into()),
            }),
            &shell.context,
        );
        assert_eq!(command.shown, "/tmp/home/.local/bin/claude");
    }

    /// A `~` path with no home directory is not found, and says why.
    #[test]
    fn a_tilde_path_with_no_home_is_not_found_for_want_of_one() {
        let command = Command {
            shown: "~/bin/tool".to_owned(),
            origin: Origin::Path,
            state: Request::NoHome,
        }
        .lookup(&GrantStack::root(), &Baseline::default());
        let Err(LookupFailure::Missing(missing @ Missing::NoHome)) = command.state else {
            panic!("a `~` path with no home has no program");
        };
        let error = missing.into_error(&command.shown);
        assert_eq!(
            error.reason,
            crate::fact::Reason::process("~/bin/tool", crate::fact::Failure::NotFound)
        );
        assert_eq!(error.hints(), [Box::from("no home directory is set")]);
    }

    /// A context whose sole grant layer is `exec`.
    fn under(exec: ExecGrant) -> Context {
        let mut grants = GrantStack::root();
        grants.push(crate::test_helper::layer(Capabilities {
            exec: Some(exec),
            ..Capabilities::default()
        }));
        Context::under_grants(grants)
    }

    #[test]
    fn path_exhaustion_keeps_the_first_fact_unchanged_and_context_separate() {
        use crate::fact::{Denial, SingleOrNested, Withholding};
        let first = Denial::exec_run(
            "tool".into(),
            Withholding::MissingAllowance(SingleOrNested::Single),
        );
        let failure = |at: &str| LookupFailure::Refused {
            at: at.into(),
            denial: Box::new(first.clone()),
            exhaustion: None,
        };
        let selected = search_error(vec![
            LookupFailure::ABSENT,
            failure("first"),
            failure("later"),
        ]);
        let LookupFailure::Refused {
            at,
            denial,
            exhaustion,
        } = selected
        else {
            panic!("first refusal");
        };
        assert_eq!(at, "first");
        assert_eq!(*denial, first);
        assert!(matches!(
            exhaustion,
            Some(PathExhaustion::NoAllowedDirectory)
        ));
        let blocked = search_error(vec![
            failure("first"),
            LookupFailure::Missing(Missing::NotRunnable(NotRunnable {
                found: "later".into(),
                why: Unrunnable::NotExecutable,
            })),
        ]);
        assert!(matches!(
            blocked,
            LookupFailure::Refused {
                exhaustion: None,
                ..
            }
        ));
    }

    fn path_name(file: &Path) -> CommandName {
        CommandName::Path(file.to_string_lossy().into_owned())
    }

    fn file_of(command: &Command<Lookup>) -> (&Path, &crate::path::RealPath) {
        match command.state.as_ref().map(Admitted::program) {
            Ok(policy::Program::File { arg0, located }) => (arg0, located.real()),
            Ok(other) => panic!("expected a host file, got {other:?}"),
            Err(LookupFailure::Missing(Missing::NotFound { .. } | Missing::NoHome)) => {
                panic!("expected a host file, found none")
            }
            Err(LookupFailure::Missing(Missing::NotRunnable(details))) => {
                panic!("expected a host file, got {details:?}");
            }
            Err(LookupFailure::Refused { at, denial, .. }) => {
                panic!("expected a host file, refused {at}: {}", denial.describe());
            }
        }
    }

    fn canonical(path: &Path) -> crate::path::RealPath {
        crate::test_helper::trace(path).real().clone()
    }

    /// An explicit path runs the file it names from the shell's cwd, and is
    /// judged by that file's real path.
    #[test]
    fn a_relative_path_is_the_file_under_the_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "configure");
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(tmp.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );

        let command = looked_up(Command::from_name(
            &CommandName::Path(format!("./{name}")),
            &shell.context,
        ));
        let (path, real) = file_of(&command);
        let file = canonical(&tmp.path().join(&name));
        assert_eq!(real, &file);
        assert_eq!(canonical(path), file);
    }

    /// `link/../tool` runs `real/tool` when `link` points at `real/sub`: the
    /// kernel walks the `..` after the link, so the launcher is handed the
    /// spelling unfolded and the guard judges what it reaches.
    #[cfg(unix)]
    #[test]
    fn a_dotdot_after_a_link_is_judged_where_the_kernel_lands() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        plant(&real, "tool");
        std::os::unix::fs::symlink(real.join("sub"), tmp.path().join("link")).unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(tmp.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );

        let command = looked_up(Command::from_name(
            &CommandName::Path("link/../tool".into()),
            &shell.context,
        ));
        let (path, judged) = file_of(&command);
        assert_eq!(judged, &canonical(&real.join("tool")));
        assert_eq!(path, tmp.path().join("link/../tool"));
    }

    #[test]
    fn a_path_naming_no_file_has_no_program() {
        let shell = crate::test_helper::core_shell();
        let path = std::env::temp_dir().join("no-such-dir").join("configure");
        let command = looked_up(Command::from_name(&path_name(&path), &shell.context));
        assert!(matches!(
            command.state,
            Err(LookupFailure::Missing(Missing::NotFound {
                suggested_extension: None,
            }))
        ));
    }

    /// The kernel refuses `file/` with ENOTDIR, so a trailing slash must not
    /// be folded away into a runnable spelling.
    #[cfg(unix)]
    #[test]
    fn a_trailing_slash_on_a_file_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "tool");
        let shell = crate::test_helper::core_shell();
        let path = tmp.path().join(name).to_string_lossy().into_owned() + "/";
        let command = looked_up(Command::from_name(&CommandName::Path(path), &shell.context));
        assert!(matches!(
            command.state,
            Err(LookupFailure::Missing(Missing::NotFound {
                suggested_extension: None,
            }))
        ));
    }

    #[test]
    fn an_empty_exec_allow_list_refuses_a_nonexistent_path() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = under(ExecGrant::default());
        for file in [tmp.path().join("missing"), tmp.path().join("absent/tool")] {
            let command = Command::from_name(&path_name(&file), &ctx);
            let Err(LookupFailure::Refused { at, denial, .. }) =
                command.lookup(&ctx.grants, &Baseline::default()).state
            else {
                panic!("an empty exec allowance refuses even an absent name");
            };
            assert_eq!(at, path::shown(&file));
            assert!(denial.describe().contains("exec `allow` list"));
        }
    }

    #[test]
    fn read_withheld_commands_are_refused_like_present_programs() {
        use crate::authority::policy::FsPolicy;
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join(plant(tmp.path(), "zzhidden"));
        #[cfg(unix)]
        let looping = {
            let looping = tmp.path().join("loop.bat");
            std::os::unix::fs::symlink("loop.bat", &looping).unwrap();
            looping
        };
        let ctx = Context::under_grants(crate::test_helper::stack([crate::test_helper::layer(
            Capabilities {
                fs: Some(FsPolicy::default()),
                ..Capabilities::default()
            },
        )]));
        let denied = |file: &Path| {
            let Err(LookupFailure::Refused { denial, .. }) =
                Command::from_name(&path_name(file), &ctx)
                    .lookup(&ctx.grants, &Baseline::default())
                    .state
            else {
                panic!("running a read-withheld name is refused");
            };
            denial
        };
        let cases = [
            tmp.path().join("missing.bat"),
            tmp.path().join("absent/tool.bat"),
            #[cfg(unix)]
            looping.clone(),
        ]
        .map(|file| {
            let refusal = denied(&file);
            assert!(refusal.describe().contains("not readable"));
            (file, refusal)
        });
        #[cfg(unix)]
        std::fs::rename(&looping, tmp.path().join("saved-loop")).unwrap();
        for (file, before) in cases {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::copy(&source, &file).unwrap();
            let after = denied(&file);
            assert_eq!(before.describe(), after.describe(), "{}", file.display());
            assert_eq!(before.advice(), after.advice(), "{}", file.display());
            assert_eq!(
                before.audit_check(),
                after.audit_check(),
                "{}",
                file.display()
            );
        }
    }

    #[test]
    fn bare_search_skips_refused_candidates_including_memo_hits() {
        for prime_memo in [false, true] {
            let d1 = tempfile::tempdir().unwrap();
            let d2 = tempfile::tempdir().unwrap();
            let name = plant(d1.path(), "zzadmitted");
            plant(d2.path(), "zzadmitted");
            let path = std::env::join_paths([d1.path(), d2.path()])
                .unwrap()
                .into_string()
                .unwrap();
            let mut ctx = under(ExecGrant::of([dir_key(d2.path())], []));
            ctx.set_env_var("PATH", path.clone()).unwrap();
            let command = Command::from_name(&CommandName::Bare(name.as_str().into()), &ctx);
            if prime_memo {
                assert_eq!(
                    path::locate(&name, Some(&path), Some(ctx.cwd())),
                    Some(d1.path().join(&name))
                );
            }
            let command = command.lookup(&ctx.grants, &Baseline::default());
            assert_eq!(file_of(&command).0, d2.path().join(&name));
        }
    }

    #[cfg(unix)]
    #[test]
    fn path_order_decides_between_refusal_and_non_executability() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let admitted = tmp.path().join("admitted");
        let denied = tmp.path().join("denied");
        std::fs::create_dir(&admitted).unwrap();
        std::fs::create_dir(&denied).unwrap();
        let plain = admitted.join("zzplain");
        std::fs::write(&plain, "").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let absent = admitted.join("absent");
        let lookup = |dirs: [&Path; 3]| {
            let mut ctx = under(ExecGrant::of([dir_key(&admitted)], []));
            let path = std::env::join_paths(dirs).unwrap().into_string().unwrap();
            ctx.set_env_var("PATH", path).unwrap();
            Command::from_name(&CommandName::Bare("zzplain".into()), &ctx)
                .lookup(&ctx.grants, &Baseline::default())
        };
        let Err(LookupFailure::Refused { at, .. }) =
            lookup([absent.as_path(), denied.as_path(), admitted.as_path()]).state
        else {
            panic!("the first non-absence is a refusal");
        };
        assert_eq!(at, path::shown(denied.join("zzplain")));
        let first = lookup([absent.as_path(), admitted.as_path(), denied.as_path()]);
        assert!(matches!(
            first.state,
            Err(LookupFailure::Missing(Missing::NotRunnable(NotRunnable {
                found,
                why: Unrunnable::NotExecutable,
            }))) if found == path::shown(&plain)
        ));
    }

    /// Each `PATH` candidate is walked once, the remembered hit included
    /// where the grant refuses it and the search goes on.
    #[cfg(unix)]
    #[test]
    fn each_path_candidate_is_walked_once() {
        let dirs = [(); 3].map(|()| tempfile::tempdir().unwrap());
        let name = plant(dirs[1].path(), "zzonce");
        plant(dirs[2].path(), "zzonce");
        let path = std::env::join_paths(dirs.iter().map(tempfile::TempDir::path))
            .unwrap()
            .into_string()
            .unwrap();
        let mut ctx = under(ExecGrant::of([dir_key(dirs[2].path())], []));
        ctx.set_env_var("PATH", path.clone()).unwrap();
        assert_eq!(
            path::locate(&name, Some(&path), Some(ctx.cwd())),
            Some(dirs[1].path().join(&name)),
        );
        let steps = crate::test_access::walk_steps;
        let walk_of = |dir: &tempfile::TempDir| {
            let before = steps();
            drop(Candidate::walk(dir.path().join(&name)));
            steps() - before
        };
        let once: usize = dirs.iter().map(walk_of).sum();

        let before = steps();
        let command = Command::from_name(&CommandName::Bare(name.as_str().into()), &ctx)
            .lookup(&ctx.grants, &Baseline::default());
        assert_eq!(steps() - before, once);
        assert_eq!(file_of(&command).0, dirs[2].path().join(&name));
    }

    /// A bare name runs what the walk found, never a same-named file in the
    /// cwd.
    #[test]
    fn a_bare_name_missing_from_path_is_not_found_beside_a_cwd_file() {
        crate::path::forget_located_commands();
        let here = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let name = plant(here.path(), "zzcwdfile");

        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(here.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        shell
            .context
            .set_env_var("PATH", elsewhere.path().to_string_lossy().into_owned())
            .unwrap();

        let command = looked_up(Command::from_name(
            &CommandName::Bare(name.into()),
            &shell.context,
        ));
        assert!(matches!(
            command.state,
            Err(LookupFailure::Missing(Missing::NotFound {
                suggested_extension: None,
            }))
        ));
    }

    /// The 126 case, which only a platform with an executable bit has.
    #[cfg(unix)]
    #[test]
    fn a_non_executable_path_hit_is_not_executable() {
        use std::os::unix::fs::PermissionsExt;
        crate::path::forget_located_commands();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("zznoexec");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned())
            .unwrap();

        let command = looked_up(Command::from_name(
            &CommandName::Bare("zznoexec".into()),
            &shell.context,
        ));
        assert!(matches!(
            command.state,
            Err(LookupFailure::Missing(Missing::NotRunnable(NotRunnable {
                found,
                why: Unrunnable::NotExecutable,
            }))) if found == path::shown(&file)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_non_runnable_path_reports_the_typed_name() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(tmp.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        for (shown, why) in [
            ("./plain", Unrunnable::NotExecutable),
            ("./", Unrunnable::Directory),
        ] {
            let command = looked_up(Command::from_name(
                &CommandName::Path(shown.into()),
                &shell.context,
            ));
            let Err(LookupFailure::Missing(Missing::NotRunnable(details))) = command.state else {
                panic!("{shown} cannot run");
            };
            assert_eq!(details.found, shown);
            assert_eq!(details.why, why);
        }
    }

    /// A bundled name is its tool whatever `PATH` holds: no walk is taken.
    #[cfg(feature = "coreutils")]
    #[test]
    fn a_bundled_name_is_its_tool() {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned())
            .unwrap();
        let command = Command::from_name(&CommandName::Bare("ls".into()), &shell.context);
        assert!(matches!(command.state, Request::Tool(tool) if tool == "ls"));
    }

    /// A bare key is the file the host `PATH` finds, so a scoped `PATH` that
    /// plants another `git` does not inherit `git: allow`.
    #[test]
    fn a_scoped_path_cannot_redirect_a_bare_key() {
        crate::path::forget_located_commands();
        let dir = tempfile::tempdir().unwrap();
        let name = plant(dir.path(), "git");
        let mut ctx = under(ExecGrant::of([ExecKey::Name("git".into())], []));
        ctx.set_env_var("PATH", dir.path().to_string_lossy().into_owned())
            .unwrap();

        let command = Command::from_name(&CommandName::Bare("git".into()), &ctx);
        assert_eq!(
            file_of(&looked_up(Command::from_name(
                &CommandName::Bare("git".into()),
                &ctx
            )))
            .1,
            &canonical(&dir.path().join(name)),
        );
        assert!(!admits_command(&ctx, command));
    }

    /// `bin/bash` real, `planted/b` a symlink to it: the shape the evasion
    /// takes.  Returns both directories and the link.
    #[cfg(unix)]
    fn planted_symlink(tmp: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let bin = tmp.join("bin");
        let planted = tmp.join("planted");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir(&planted).unwrap();
        plant(&bin, "bash");
        let link = planted.join("b");
        std::os::unix::fs::symlink(bin.join("bash"), &link).unwrap();
        (bin, planted, link)
    }

    fn dir_key(dir: &Path) -> ExecKey {
        ExecKey::Dir(FrozenPath::from_surface(crate::path::utf8(dir).unwrap()))
    }

    /// A policy that denies `bash` by bare name yet allows all of its bin
    /// dir: the file that runs is `bash` whatever the command's spelling, so the
    /// veto beats the dir.
    #[test]
    fn a_bare_deny_beats_the_allow_dir_an_explicit_path_sits_in() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "bash");
        let ctx = under(ExecGrant::of(
            [dir_key(tmp.path())],
            [ExecKey::Name("bash".into())],
        ));

        let command = Command::from_name(&path_name(&tmp.path().join(name)), &ctx);
        assert!(!admits_command(&ctx, command));
    }

    /// The mirror: a bare `rg: allow` is the host's `rg`, so a planted
    /// path `evil/rg` does not inherit it.
    #[test]
    fn a_bare_allow_does_not_admit_a_planted_path() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "rg");
        let ctx = under(ExecGrant::of([ExecKey::Name("rg".into())], []));

        let command = Command::from_name(&path_name(&tmp.path().join(name)), &ctx);
        assert!(!admits_command(&ctx, command));
    }

    /// The symlink half of the deny-by-name evasion: `planted/b` wears a name
    /// under the allow dir and runs a file the grant denies.  The program is
    /// that file, so the link is denied by its name.
    #[cfg(unix)]
    #[test]
    fn a_symlink_under_an_allow_dir_meets_the_bare_deny_of_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin, planted, link) = planted_symlink(tmp.path());
        let ctx = under(ExecGrant::of(
            [dir_key(&planted)],
            [ExecKey::Name("bash".into())],
        ));

        let command = Command::from_name(&path_name(&link), &ctx);
        assert_eq!(
            file_of(&looked_up(Command::from_name(&path_name(&link), &ctx))).1,
            &canonical(&bin.join("bash")),
        );
        assert!(!admits_command(&ctx, command));
    }

    /// The same link, judged by a denied *region* its program lies in.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_deny_dir_is_denied_by_the_file_it_names() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin, planted, link) = planted_symlink(tmp.path());
        let ctx = under(ExecGrant::of([dir_key(&planted)], [dir_key(&bin)]));

        let command = Command::from_name(&path_name(&link), &ctx);
        assert!(!admits_command(&ctx, command));
    }

    /// A path key names a file, so a link to it runs the admitted file.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_admitted_by_the_path_key_on_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin, _planted, link) = planted_symlink(tmp.path());
        let ctx = under(ExecGrant::of(
            [ExecKey::Path(FrozenPath::from_surface(
                crate::path::utf8(&bin.join("bash")).unwrap(),
            ))],
            [],
        ));

        let command = Command::from_name(&path_name(&link), &ctx);
        assert!(admits_command(&ctx, command));
    }

    /// Where the scheme stops, pinned rather than left to be rediscovered: a
    /// *copy* is a different file, holding no trace of the name that was
    /// denied, and an allow dir admits it.  No resolution can close this, and
    /// nothing needs to: the copy runs under the projection that admitted the
    /// write, so it reaches nothing its author could not.
    #[test]
    fn a_copy_under_a_new_name_is_a_different_file_and_is_admitted() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let planted = tmp.path().join("planted");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir(&planted).unwrap();
        let name = plant(&bin, "bash");
        let copy = planted.join("b");
        std::fs::copy(bin.join(&name), &copy).unwrap();

        let ctx = under(ExecGrant::of(
            [dir_key(&planted)],
            [ExecKey::Name("bash".into())],
        ));

        let command = Command::from_name(&path_name(&copy), &ctx);
        assert!(admits_command(&ctx, command));
    }

    /// An executable a *bare-name* walk finds in `dir`: off Unix a bare name
    /// only resolves through `%PATHEXT%`, so the file needs a suffix from it.
    fn plant(dir: &Path, stem: &str) -> String {
        let name = if cfg!(windows) {
            format!("{stem}.bat")
        } else {
            stem.to_owned()
        };
        std::fs::write(dir.join(&name), b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.join(&name);
            let mut perms = std::fs::metadata(&p).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&p, perms).unwrap();
        }
        name
    }

    /// A `./bin` on `PATH` follows the shell's cwd, the one "here" every
    /// other consumer reads.
    #[test]
    fn walk_anchors_relative_path_entries_to_the_cwd() {
        crate::path::forget_located_commands();
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let name = plant(&bin, "zzwalk");

        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(tmp.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        shell.context.set_env_var("PATH", "./bin").unwrap();

        let command = looked_up(Command::from_name(
            &CommandName::Bare(name.as_str().into()),
            &shell.context,
        ));
        assert_eq!(file_of(&command).0, bin.join(&name));
    }

    #[cfg(unix)]
    impl policy::Program {
        /// The present file `path` names, walked but unjudged.
        pub(crate) fn file(path: PathBuf) -> Option<Self> {
            let Candidate { arg0, walked, .. } = Candidate::walk(path)?;
            let located = walked.ok()?;
            located.trace().kind()?;
            Some(Self::File {
                arg0,
                located: Box::new(located),
            })
        }
    }
}

mod vet {
    use super::super::*;
    use crate::authority::policy::{
        Admitted, Capabilities, ExecGrant, ExecKey, FsPolicy, GrantLayer,
    };
    use crate::fact::{Check, FailureCode as _, Observed, Resource};
    use crate::ir::CommandName;
    use crate::path::FrozenPath;
    use crate::shell::runtime::{CapturePolicy, Mooring, Shell};
    use std::path::Path;

    /// A command admitted with no surface, then prepared with no argv.
    fn plan(command: Command<Request>, shell: &mut Shell) -> Settled<Prepared> {
        command
            .lookup(&shell.context.grants, shell.baseline())
            .admit(&Mooring::adrift(), shell)?
            .prepare(&[], Ownership::Kept, shell)
    }

    /// An executable a *bare-name* walk finds in `dir`; off Unix a bare name
    /// resolves only through `%PATHEXT%`, so the file carries a suffix from it.
    fn plant(dir: &Path, stem: &str) -> String {
        let name = if cfg!(windows) {
            format!("{stem}.bat")
        } else {
            stem.to_owned()
        };
        let p = dir.join(&name);
        std::fs::write(&p, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&p).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&p, perms).unwrap();
        }
        name
    }

    /// The error `prepare` refuses `command` with.
    fn refused(command: Command<Request>, shell: &mut Shell) -> Error {
        match plan(command, shell) {
            Err(Break::Error(e)) => e,
            Err(other) => panic!("expected a refusal, got {other:?}"),
            Ok(_) => panic!("expected a refusal, got a plan"),
        }
    }

    #[cfg(unix)]
    fn no_exec() -> GrantLayer {
        crate::test_helper::layer(Capabilities {
            exec: Some(ExecGrant::default()),
            ..Capabilities::default()
        })
    }

    fn run_from(dir: &Path) -> GrantLayer {
        run_from_excluding(dir, [])
    }

    fn run_from_excluding(dir: &Path, excluded: impl IntoIterator<Item = ExecKey>) -> GrantLayer {
        crate::test_helper::layer(Capabilities {
            exec: Some(ExecGrant::of(
                [ExecKey::Dir(FrozenPath::from_surface(
                    crate::path::utf8(dir).unwrap(),
                ))],
                excluded,
            )),
            fs: Some(FsPolicy::default()),
            ..Capabilities::default()
        })
    }

    /// The headline regression: a file sitting in the directory the user just
    /// `cd`'d into is not on `PATH`, and a `PATH` ending in `;` does not put it
    /// there.  Naming it bare is 127 ("no such command"), never 126.
    #[cfg(windows)]
    #[test]
    fn bare_name_of_a_cwd_file_is_127_not_126() {
        crate::path::forget_located_commands();
        let here = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let name = plant(here.path(), "zzcwdonly");

        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(here.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        shell
            .context
            .set_env_var("PATH", format!("{};", elsewhere.path().to_string_lossy()))
            .unwrap();

        let command = Command::from_name(&CommandName::Bare(name.into()), &shell.context);
        let e = refused(command, &mut shell);
        assert_eq!(e.failure_code().get(), 127);
        assert!(
            !format!("{e:?}").contains("permission denied"),
            "a name no walk resolved must not be refused as unexecutable: {e:?}",
        );
    }

    /// A path written without its extension runs nothing, and the refusal
    /// names the file it might have meant.
    #[cfg(windows)]
    #[test]
    fn an_extensionless_path_is_127_with_a_hint() {
        let here = tempfile::tempdir().unwrap();
        plant(here.path(), "zztool");
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(here.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );

        let command = Command::from_name(&CommandName::Path(r".\zztool".into()), &shell.context);
        let e = refused(command, &mut shell);
        assert_eq!(e.failure_code().get(), 127);
        assert_eq!(
            e.hints().first().map(|h| &**h),
            Some(r"did you mean '.\zztool.bat'?")
        );
    }

    /// Resolution and verdict are two projections of one walk, so they agree
    /// about where "here" is: the same `./bin` that resolves under one cwd is
    /// simply absent under another — 127, never 126.
    #[test]
    fn verdict_and_resolution_come_from_one_walk() {
        crate::path::forget_located_commands();
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let name = plant(&bin, "zzonewalk");
        let elsewhere = tempfile::tempdir().unwrap();

        let mut shell = crate::test_helper::core_shell();
        shell.context.set_env_var("PATH", "./bin").unwrap();
        shell.seed_cwd(
            crate::path::CwdPath::new(tmp.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        let command = Command::from_name(&CommandName::Bare(name.as_str().into()), &shell.context)
            .lookup(&shell.context.grants, shell.baseline())
            .admit(&Mooring::adrift(), &mut shell)
            .expect("an unrestricted shell admits the command");
        assert!(matches!(
            command.state.as_ref().map(Admitted::program),
            Ok(policy::Program::File { arg0, .. }) if *arg0 == bin.join(&name)
        ));
        command
            .prepare(&[], Ownership::Kept, &shell)
            .expect("a resolved name must prepare");

        shell.seed_cwd(
            crate::path::CwdPath::new(elsewhere.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        let command = Command::from_name(&CommandName::Bare(name.into()), &shell.context);
        assert_eq!(refused(command, &mut shell).failure_code().get(), 127);
    }

    /// Bundled names short-circuit the disk probe: `ls` vets on an empty
    /// `PATH`.  Gated because without `coreutils` the bundled set is empty.
    #[cfg(feature = "coreutils")]
    #[test]
    fn bundled_name_vets_with_empty_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned())
            .unwrap();

        let command = Command::from_name(&CommandName::Bare("ls".into()), &shell.context);
        let plan = plan(command, &mut shell).expect("bundled name must not 127");
        assert_eq!(plan.program.shown, "ls");
    }

    /// The 126 case of an explicit path: a file lacking `+x`, or a directory,
    /// which only the admitted program is asked about.
    #[cfg(unix)]
    #[test]
    fn a_path_that_cannot_be_run_is_not_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut shell = crate::test_helper::core_shell();
        for path in [file.as_path(), tmp.path()] {
            let command = Command::from_name(
                &CommandName::Path(path.to_string_lossy().into_owned()),
                &shell.context,
            );
            let e = refused(command, &mut shell);
            assert_eq!(e.failure_code().get(), 126, "{e:?}");
            assert!(!e.describe().contains("denied by active grant"), "{e:?}");
        }
    }

    /// A program is judged before it is looked at, so a file the grant denies
    /// is refused as denied whether or not it could have run: the refusal
    /// does not tell what a denied path is.
    #[cfg(unix)]
    #[test]
    fn a_denied_non_executable_is_refused_as_denied_not_as_non_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut shell = crate::test_helper::core_shell();
        let command = Command::from_name(
            &CommandName::Path(file.to_string_lossy().into_owned()),
            &shell.context,
        );
        let e = shell.with_layer(no_exec(), |shell| refused(command, shell));
        assert!(e.describe().contains("denied by active grant"), "{e:?}");
    }

    /// Only a refused walk is permission denied: a link cycle is reported as
    /// the OS words it.
    #[cfg(unix)]
    #[test]
    fn a_looping_path_is_not_reported_as_permission_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let looping = tmp.path().join("loop");
        std::os::unix::fs::symlink("loop", &looping).unwrap();
        let mut shell = crate::test_helper::core_shell();
        let command = Command::from_name(
            &CommandName::Path(looping.to_string_lossy().into_owned()),
            &shell.context,
        );
        let e = refused(command, &mut shell);
        assert_eq!(e.failure_code().get(), 126, "{e:?}");
        assert!(!e.describe().contains("permission denied"), "{e:?}");
    }

    /// A name in no directory the grant runs from, whether absent or found
    /// outside them, is refused by the first candidate outside, and said to
    /// be no program there.
    #[test]
    fn a_name_unfound_where_the_grant_runs_records_the_first_refusal_with_a_bare_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "zzdeniedbare");
        let dirs = ["admitted", "first-denied", "later-denied"].map(|dir| {
            let dir = tmp.path().join(dir);
            std::fs::create_dir(&dir).unwrap();
            dir
        });
        let mut shell = crate::test_helper::core_shell();
        shell
            .context
            .set_env_var(
                "PATH",
                std::env::join_paths(&dirs).unwrap().into_string().unwrap(),
            )
            .unwrap();
        for present in [false, true] {
            if present {
                for dir in &dirs[1..] {
                    std::fs::copy(tmp.path().join(&name), dir.join(&name)).unwrap();
                }
            }
            let (e, trail) = shell.with_layer(run_from(&dirs[0]), |shell| {
                let scope = shell.local.audit.open(CapturePolicy::Off);
                let command =
                    Command::from_name(&CommandName::Bare(name.as_str().into()), &shell.context);
                let e = refused(command, shell);
                (e, shell.local.audit.close(scope))
            });
            assert!(e.describe().contains("denied by active grant"), "{e:?}");
            let [bare] = e.hints() else {
                panic!("only the bare-name lead is contextual: {:?}", e.hints());
            };
            let advice = e.reason.advice();
            let [grant] = advice.as_slice() else {
                panic!("the denial supplies its remedy");
            };
            assert_eq!(
                &**bare,
                format!("'{name}' is not in any PATH directory the grant allows")
            );
            let rendered = e.render(shell.sources(), None, shell.exit_hints());
            assert!(
                rendered.find(grant).unwrap() < rendered.find(&**bare).unwrap(),
                "{rendered}"
            );
            assert_eq!(rendered.matches(grant).count(), 1, "{rendered}");
            assert_eq!(rendered.matches(&**bare).count(), 1, "{rendered}");
            assert_ne!(grant.as_str(), &**bare);
            assert!(!grant.contains("#'") && !bare.contains("#'"), "{e:?}");
            let [observation] = trail.as_slice() else {
                panic!("only the first refusal is recorded: {trail:?}");
            };
            assert_eq!(
                observation.what,
                Observed::Check(Check::new(
                    Resource::Exec,
                    [("name".into(), name.clone())].into(),
                ))
            );
        }
    }

    /// An excluded program was found, so its refusal carries the exclusion's
    /// hint alone, even when an earlier candidate outside the grant refused
    /// first.
    #[test]
    fn an_excluded_program_on_path_is_not_said_to_be_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let [outside, admitted] = ["outside", "admitted"].map(|dir| {
            let dir = tmp.path().join(dir);
            std::fs::create_dir(&dir).unwrap();
            dir
        });
        let name = plant(&admitted, "zzexcluded");
        let excluded = ExecKey::Path(FrozenPath::from_surface(
            crate::path::utf8(&admitted.join(&name)).unwrap(),
        ));
        let mut shell = crate::test_helper::core_shell();
        for dirs in [vec![&admitted], vec![&outside, &admitted]] {
            shell
                .context
                .set_env_var(
                    "PATH",
                    std::env::join_paths(&dirs).unwrap().into_string().unwrap(),
                )
                .unwrap();
            let e = shell.with_layer(run_from_excluding(&admitted, [excluded.clone()]), |shell| {
                refused(
                    Command::from_name(&CommandName::Bare(name.as_str().into()), &shell.context),
                    shell,
                )
            });
            assert!(e.describe().contains("denied by active grant"), "{e:?}");
            assert!(e.hints().is_empty(), "intrinsic advice is not cached");
            assert_eq!(e.reason.advice().len(), 1);
            if dirs.len() == 1 {
                assert!(e.describe().contains("is excluded by"), "{e:?}");
                assert_eq!(
                    e.reason.advice(),
                    [crate::authority::remedy::remove_the_exclusion().to_owned()]
                );
            }
        }
    }

    /// A `PATH` entry the OS refuses to search is reported as that refusal,
    /// as an explicit path's is: absence is only what remains when nothing else
    /// failed.
    #[cfg(unix)]
    #[test]
    fn a_refused_path_entry_is_permission_denied_not_absent() {
        use crate::fact::NotRunnable;
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::geteuid().is_root() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let [locked, empty] = ["locked", "empty"].map(|dir| {
            let dir = tmp.path().join(dir);
            std::fs::create_dir(&dir).unwrap();
            dir
        });
        let name = plant(&locked, "zzlocked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell
            .context
            .set_env_var(
                "PATH",
                std::env::join_paths([&empty, &locked])
                    .unwrap()
                    .into_string()
                    .unwrap(),
            )
            .unwrap();
        let e = refused(
            Command::from_name(&CommandName::Bare(name.as_str().into()), &shell.context),
            &mut shell,
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let found = crate::path::shown(locked.join(&name));
        assert!(
            matches!(
                e.reason.failure(),
                Some(Failure::NotRunnable(NotRunnable {
                    found: at,
                    why: Unrunnable::PermissionDenied,
                })) if *at == found
            ),
            "{e:?}"
        );
        assert_eq!(e.failure_code().get(), 126);
        assert_eq!(e.describe(), format!("{name}: {found}: permission denied"));
    }

    /// An explicit path naming no file is a missing command, and the refusal names
    /// the path.
    #[test]
    fn a_path_naming_no_file_produces_127() {
        let mut shell = crate::test_helper::core_shell();
        let path = std::env::temp_dir().join("no-such-dir").join("tool");
        let command = Command::from_name(
            &CommandName::Path(path.to_string_lossy().into_owned()),
            &shell.context,
        );
        let e = refused(command, &mut shell);
        assert_eq!(e.failure_code().get(), 127);
        assert!(e.prov.is_none());
        let description = e.describe();
        assert!(description.contains("no-such-dir"), "{description}");
    }

    #[test]
    fn all_absent_path_candidates_produce_127() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell
            .context
            .set_env_var(
                "PATH",
                std::env::join_paths([d1.path(), d2.path()])
                    .unwrap()
                    .into_string()
                    .unwrap(),
            )
            .unwrap();
        let command = Command::from_name(
            &CommandName::Bare("definitely-not-a-real-tool-xyz".into()),
            &shell.context,
        );
        assert_eq!(refused(command, &mut shell).failure_code().get(), 127);
    }

    #[test]
    fn admitted_path_failures_precede_argv_shape() {
        let tmp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let looping = {
            let looping = tmp.path().join("loop");
            std::os::unix::fs::symlink("loop", &looping).unwrap();
            looping
        };
        let mut shell = crate::test_helper::core_shell();
        shell.with_layer(run_from(tmp.path()), |shell| {
            for (file, code) in [
                (tmp.path().join("missing"), 127),
                (tmp.path().join("absent/tool"), 127),
                (tmp.path().to_path_buf(), 126),
                #[cfg(unix)]
                (looping, 126),
            ] {
                let command = Command::from_name(
                    &CommandName::Path(file.to_string_lossy().into_owned()),
                    &shell.context,
                )
                .lookup(&shell.context.grants, shell.baseline())
                .admit(&Mooring::adrift(), shell)
                .expect("the grant runs this directory");
                let Err(Break::Error(e)) = command.prepare(&[Value::Unit], Ownership::Kept, shell)
                else {
                    panic!("the admitted path cannot be run");
                };
                assert_eq!(e.failure_code().get(), code, "{e:?}");
                assert!(!e.describe().contains("cannot pass"), "{e:?}");
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn an_admitted_non_executable_candidate_is_126_naming_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("zzplain");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell
            .context
            .set_env_var("PATH", tmp.path().to_string_lossy().into_owned())
            .unwrap();
        shell.with_layer(run_from(tmp.path()), |shell| {
            let command = Command::from_name(&CommandName::Bare("zzplain".into()), &shell.context)
                .lookup(&shell.context.grants, shell.baseline())
                .admit(&Mooring::adrift(), shell)
                .expect("the grant admits the non-executable file");
            let Err(Break::Error(e)) = command.prepare(&[Value::Unit], Ownership::Kept, shell)
            else {
                panic!("a non-executable candidate cannot be run");
            };
            assert_eq!(e.failure_code().get(), 126, "{e:?}");
            assert!(e.describe().contains(&crate::path::shown(&file)), "{e:?}");
        });
    }

    #[test]
    fn rejected_argv_keeps_the_argument_index_and_head() {
        let error =
            check_argv("tool", &[Value::string("okay"), Value::List(vec![].into())]).unwrap_err();
        let Reason::Invalid(failure) = &error.reason else {
            panic!("typed argv shape");
        };
        assert_eq!(failure.path, [crate::fact::DataStep::Index(1usize.into())]);
        assert!(matches!(
            &failure.cause,
            crate::fact::InvalidCause::Shape(crate::fact::ShapeFailure {
                found: crate::fact::Found::List,
                ..
            })
        ));
        assert_eq!(error.hints(), []);
        assert_eq!(
            error.describe(),
            "cannot pass a list as argument at index 1 to `tool`"
        );
        assert_eq!(error.reason.advice(), [RefusedArg::List.remedy("tool")]);
        assert!(error.plain().contains(&RefusedArg::List.remedy("tool")));
    }

    #[test]
    fn a_nul_in_argv_is_refused_with_its_argument_number() {
        use crate::fact::{LaunchFailure, PrepareCause, PrepareFailure, PrepareInvalid};
        let error =
            check_argv("tool", &[Value::string("okay"), Value::string("a\0b")]).unwrap_err();
        let Reason::Launch(failure) = &error.reason else {
            panic!("a NUL argument is a launch refusal");
        };
        assert!(matches!(
            **failure,
            LaunchFailure::Prepare(PrepareFailure {
                step: PrepareStep::Argv,
                subject: PrepareSubject::Argument(n),
                cause: PrepareCause::Invalid(PrepareInvalid::Nul),
            }) if n == 2usize.into()
        ));
    }

    /// A value and its type earn one verdict: the property the shared
    /// classification exists to hold.  The `Thunk` and `Handle` pairs need an
    /// `Env`, and are paired end to end instead
    /// (`tests/reject/external-arg-*.ral`, `core/tests/argv_convention.rs`).
    #[test]
    fn a_value_and_its_type_earn_the_same_verdict() {
        use crate::ty::{Row, Ty};
        let of_value = |v: &Value| v.heads().first().copied().and_then(RefusedArg::of_head);
        let pairs: [(Value, Ty); 9] = [
            (Value::Unit, Ty::unit()),
            (Value::Bool(true), Ty::bool()),
            (Value::Int(1), Ty::int()),
            (
                Value::Float(crate::data::Finite::new(1.0).expect("finite")),
                Ty::float(),
            ),
            (Value::string("x"), Ty::string()),
            (Value::bytes(vec![1]), Ty::bytes()),
            (Value::List(vec![].into()), Ty::list(Ty::string())),
            (Value::map(vec![]), Ty::map(Ty::int())),
            // A tagged value renders, so neither side refuses it.
            (Value::variant("ok", None), Ty::variant(Row::empty())),
        ];
        for (value, ty) in pairs {
            assert_eq!(
                of_value(&value),
                RefusedArg::of_ty(&ty),
                "{value:?} and {ty:?} must earn the same verdict"
            );
        }
        // A record is a map at run time, so it is refused as the map it is.
        assert_eq!(
            RefusedArg::of_ty(&Ty::record(Row::empty())),
            Some(RefusedArg::Map)
        );
        // A variable is not a shape: the runtime keeps that question.
        assert_eq!(RefusedArg::of_ty(&Ty::var(crate::ty::TyVar(0))), None);
    }
}

mod stdio {
    use crate::io::{Source, SourceReader};
    #[cfg(unix)]
    use crate::process::{DurableRoot, Scope};
    use crate::process::{StdioSpec, TerminalInput};

    #[cfg(unix)]
    #[test]
    fn stream_connection_failure_names_preparation_not_duplication_of_the_program() {
        use super::super::*;
        let mut shell = crate::test_helper::core_shell();
        let prepared = Command::from_name(&CommandName::Path("/bin/sh".into()), &shell.context)
            .lookup(&shell.context.grants, shell.baseline())
            .admit(&Mooring::adrift(), &mut shell)
            .unwrap()
            .prepare(&[], Ownership::Kept, &shell)
            .unwrap();
        let cause = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        let error = prepared.connection_error(&cause);
        assert_eq!(
            error.reason,
            Reason::Launch(Box::new(crate::fact::LaunchFailure::Prepare(
                crate::fact::PrepareFailure {
                    step: crate::fact::PrepareStep::Stdio,
                    subject: crate::fact::PrepareSubject::Program("/bin/sh".into()),
                    cause: crate::fact::PrepareCause::Io(Box::new(crate::io::failure(&cause))),
                }
            )))
        );
        assert!(
            error
                .describe()
                .starts_with("ral could not connect standard streams for program '/bin/sh':")
        );
    }

    #[test]
    fn offered_withholds_only_terminal_input_and_borrows_other_sources() {
        let terminal = Source::Terminal;
        let empty = Source::Empty;
        let file = Source::Reader(SourceReader::file(tempfile::tempfile().expect("reader")));
        #[cfg(unix)]
        let (pipe, _writer) = crate::process::cloexec_pipe().expect("data pipe");
        let sources = [
            empty,
            file,
            #[cfg(unix)]
            Source::Reader(SourceReader::pipe(pipe)),
        ];
        for input in [TerminalInput::Readable, TerminalInput::Withheld] {
            match input {
                TerminalInput::Readable => {
                    assert!(std::ptr::eq(terminal.offered(input), &raw const terminal));
                }
                TerminalInput::Withheld => {
                    assert!(matches!(terminal.offered(input), Source::Empty));
                }
            }
            for source in &sources {
                assert!(std::ptr::eq(source.offered(input), source));
                if matches!(source, Source::Reader(_)) {
                    assert!(matches!(source.child_stdio(), Ok(StdioSpec::Owned(_))));
                    assert!(
                        source
                            .reader()
                            .expect("source keeps its descriptor")
                            .is_some()
                    );
                }
            }
        }
    }

    /// The source is authoritative: `Empty` wires `/dev/null` and `Terminal`
    /// inherits fd 0, whatever fd 0 is.
    #[test]
    fn the_childs_source_decides_its_stdin() {
        assert!(matches!(Source::Empty.child_stdio(), Ok(StdioSpec::Null)));
        assert!(matches!(
            Source::Terminal.child_stdio(),
            Ok(StdioSpec::Inherit)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_stage_shaped_source_is_borrowed() {
        let (r, _w) = crate::process::cloexec_pipe().expect("data pipe");
        let stdin = Source::Reader(
            SourceReader::pipe(r).interruptible(Scope::clone(&DurableRoot::default())),
        );

        let stdio = stdin.child_stdio().expect("child stdin");
        assert!(matches!(stdio, StdioSpec::Owned(_)));

        // The source is borrowed, never taken, so it still has a reader.
        assert!(stdin.reader().expect("reader").is_some());
    }
}

#[cfg(unix)]
mod foreground {
    use super::super::*;
    use crate::io::{Sink, Source};
    use crate::process::{Launch, TerminalAccess, TerminalLease};
    use crate::test_helper::terminal::{self, Mode};
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::FileExt as _;
    use std::os::unix::process::CommandExt as _;
    use std::sync::{Arc, Mutex, mpsc};

    fn tenant_shell() -> Shell {
        let mut shell = crate::test_helper::core_shell();
        shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
        shell.io.stdin = Source::Terminal;
        shell.io.stdout = Sink::Stdout;
        shell
    }

    /// Both records precede exec, hence precede the parent's spawn return.
    /// No script scheduling or child-side readiness race can lose the marker.
    fn marked_sleep(pid_file: &std::fs::File, marker: &'static [u8]) -> Launch {
        let pid_file = pid_file.try_clone().expect("child pid file");
        let mut command = std::process::Command::new("/bin/sleep");
        command.arg("30");
        // SAFETY: the hook only calls getpid/write on prepared descriptors;
        // all bytes live on the stack or in static storage. No locks or allocation.
        unsafe {
            command.pre_exec(move || {
                let pid = libc::getpid().to_ne_bytes();
                for (fd, mut bytes) in [
                    (pid_file.as_raw_fd(), pid.as_slice()),
                    (libc::STDERR_FILENO, marker),
                ] {
                    while !bytes.is_empty() {
                        let n = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
                        if n < 0 {
                            let error = std::io::Error::last_os_error();
                            if error.kind() == ErrorKind::Interrupted {
                                continue;
                            }
                            return Err(error);
                        }
                        if n == 0 {
                            return Err(ErrorKind::WriteZero.into());
                        }
                        bytes = &bytes[n.unsigned_abs()..];
                    }
                }
                Ok(())
            });
        }
        Launch::from_command(command)
    }

    fn assert_child_reaped(pid_file: &std::fs::File) {
        let mut bytes = [0; std::mem::size_of::<libc::pid_t>()];
        pid_file.read_exact_at(&mut bytes, 0).expect("pre-exec pid");
        let pid = libc::pid_t::from_ne_bytes(bytes);
        let mut status = 0;
        // SAFETY: this pid was recorded by our own child before exec.
        let waited = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        let error = std::io::Error::last_os_error();
        if waited == 0 {
            // A failed assertion must not leak a live child in its own group.
            // waitpid just proved it is still our unreaped child, pinning its ID.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, &raw mut status, 0);
            }
        }
        assert_eq!(waited, -1, "the refused child must already be reaped");
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
    }

    fn assert_handoff_refusal(refusal: &Error) {
        let described = refusal.describe();
        assert!(
            described.contains("could not hand the terminal"),
            "{refusal}"
        );
        assert!(
            described.contains(&std::io::Error::from_raw_os_error(libc::ENOTTY).to_string()),
            "the handoff must name the real non-controlling-tty error: {refusal}"
        );
    }

    /// Placement cases live in the process table. Here the real handoff must
    /// return `Refused` (not `Program` or `Stopped`), having killed and reaped its child.
    #[test]
    fn a_child_the_terminal_cannot_be_lent_to_is_killed() {
        terminal::isolated(
            "shell::exec::command::tests::foreground::a_child_the_terminal_cannot_be_lent_to_is_killed",
            Mode::NonControlling,
            |_pty| {
                let shell = tenant_shell();
                let mooring = Mooring {
                    terminal_access: TerminalAccess::Leased,
                    ..Mooring::adrift()
                };
                let pid_file = tempfile::tempfile().expect("pid file");
                let mut launch = marked_sleep(&pid_file, b"");
                launch.stdout(crate::process::StdioSpec::Null);
                launch.stderr(crate::process::StdioSpec::Null);
                let placement = Placement::command(shell.group_owner(&mooring));
                let result = launch.spawn(placement, &shell.scope);
                assert_child_reaped(&pid_file);
                let Err(Unstarted::Refused(refusal)) = result else {
                    panic!("a failed handoff must refuse the start");
                };
                assert_handoff_refusal(&refusal);
            },
        );
    }

    /// An ordinary start error must not detach a drain that is still inside
    /// `Sink::write_under`. The watch callback is the actual blocking sink,
    /// and the tee's second branch retains every stderr byte after release.
    #[test]
    fn a_refused_start_waits_for_its_blocked_stderr_drain() {
        terminal::isolated(
            "shell::exec::command::tests::foreground::a_refused_start_waits_for_its_blocked_stderr_drain",
            Mode::NonControlling,
            |_pty| {
                const MARKER: &[u8] = b"stderr written before spawn returns\n";
                let mut shell = tenant_shell();
                let mooring = Mooring {
                    terminal_access: TerminalAccess::Leased,
                    ..Mooring::adrift()
                };
                let mut command =
                    Command::from_name(&CommandName::Path("/bin/sleep".into()), &shell.context)
                        .lookup(&shell.context.grants, shell.baseline())
                        .admit(&mooring, &mut shell)
                        .expect("unrestricted sleep")
                        .prepare(&[Value::string("30")], Ownership::Kept, &shell)
                        .expect("prepare sleep");
                let pid_file = tempfile::tempfile().expect("pid file");
                command.launch = ProgramLaunch::Direct(marked_sleep(&pid_file, MARKER));
                let (mut observer, blocker) = terminal::checkpoints().expect("sink checkpoints");
                let blocker = Mutex::new(blocker);
                let (capture, bytes) = crate::io::new_buffer();
                let stderr = Sink::Tee(
                    Box::new(Sink::Watch {
                        line: Arc::new(move |line| {
                            assert_eq!(line, &MARKER[..MARKER.len() - 1]);
                            let mut checkpoint = blocker.lock().unwrap();
                            checkpoint.send(b'B').expect("entered sink write");
                            checkpoint.expect(b'R').expect("release sink write");
                        }),
                        pending: Vec::new(),
                    }),
                    Box::new(capture),
                );
                let placement = Placement::command(shell.group_owner(&mooring));
                let io = Connection::new(
                    shell.io.stdin.offered(placement.input()),
                    &shell.io.stdout,
                    &stderr,
                )
                .expect("connection");
                let scope = shell.scope.clone();
                let (tx, rx) = mpsc::channel();
                let worker = std::thread::spawn(move || {
                    let result = match command
                        .start(io, placement, &scope)
                        .map_err(|failure| failure.settle(&scope))
                    {
                        Err(Break::Error(refusal)) => {
                            assert_handoff_refusal(&refusal);
                            Err(refusal.describe().into_owned())
                        }
                        Err(other) => panic!("expected handoff refusal, got {other:?}"),
                        Ok(mut started) => {
                            // If handoff refusal breaks, still settle the unexpected child.
                            started.spawned.group.owned().expect("tenant group").kill();
                            let _ = started.spawned.child.kill();
                            let _ = started.spawned.child.reap();
                            started.drains.join_under(&scope, || {});
                            Ok(())
                        }
                    };
                    tx.send(result).expect("report start result");
                });
                observer
                    .expect(b'B')
                    .expect("drain entered the blocking sink");
                let premature = rx.recv_timeout(std::time::Duration::from_millis(200));
                // Always release before asserting, including on a broken join path.
                observer.send(b'R').expect("release the blocked sink");
                assert!(
                    matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)),
                    "a start result escaped while stderr was blocked: {premature:?}"
                );
                assert!(
                    rx.recv_timeout(terminal::WAIT)
                        .expect("settled refusal")
                        .is_err()
                );
                worker.join().expect("start worker");
                assert_child_reaped(&pid_file);
                let retained = bytes.take();
                assert_eq!(retained.bytes, MARKER);
                assert_eq!(retained.total, MARKER.len() as u64);
            },
        );
    }
}

mod process {
    use super::super::*;

    mod denials {
        use super::exit_error;
        use crate::fact::{Failure, Reason};
        use std::time::Instant;

        /// No line is an unconfined child's: its failure is the raw ending, with
        /// no kernel account and no brake blamed, whatever the status.
        #[test]
        fn unconfined_failures_keep_raw_exits_without_a_hint() {
            for failure in [
                Failure::Exited(128),
                Failure::Exited(384),
                Failure::Exited(5),
                Failure::Exited(0xC000_0022_u32.cast_signed()),
                Failure::Exited(-1),
                Failure::Exited(i32::MIN),
                Failure::Signaled(31),
            ] {
                let error = exit_error("tool", failure.clone(), None, Instant::now());
                assert_eq!(error.reason, Reason::process("tool", failure));
                assert!(error.hints().is_empty(), "{:?}", error.hints());
            }
        }

        /// The brake's hint is decided from the failure itself: an exit status
        /// of 128, never a death by signal 128 nor any other status.  A claim of
        /// no pid reads no log, so the brake's is the only hint here.
        #[cfg(target_os = "macos")]
        #[test]
        fn a_confined_fork_brake_hint_reads_only_an_exit_status_of_128() {
            let braked = crate::process::limits::failure_hint(128);
            assert!(braked.is_some());
            let pids = std::collections::HashSet::new();
            for (failure, hint) in [
                (Failure::Exited(128), braked.as_deref()),
                (Failure::Exited(384), None),
                (Failure::Exited(-1), None),
                (Failure::Signaled(128), None),
            ] {
                let error = exit_error("tool", failure.clone(), Some(&pids), Instant::now());
                assert_eq!(error.reason, Reason::process("tool", failure));
                assert_eq!(error.hints().first().map(|h| &**h), hint);
            }
        }
    }

    /// What runs is the file that was judged, by its real path and under the
    /// spelling the user wrote, however the spelling moves in between.
    #[cfg(unix)]
    mod judged_program {
        use super::{Command, Prepared};
        use crate::authority::sandbox::Ownership;
        use crate::io::child::Connection;
        use crate::io::{Sink, Source};
        use crate::ir::CommandName;
        use crate::shell::runtime::{Mooring, Shell, Value};
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::path::{Path, PathBuf};

        /// A scratch directory holding `tools/`, as the shell's cwd.
        fn workdir() -> (tempfile::TempDir, Shell) {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::create_dir(tmp.path().join("tools")).unwrap();
            let mut shell = crate::test_helper::core_shell();
            shell.seed_cwd(
                crate::path::CwdPath::new(tmp.path().to_path_buf())
                    .expect("a tempdir is a working directory"),
            );
            (tmp, shell)
        }

        /// An executable that prints `who`.
        fn plant(dir: &Path, who: &str) -> PathBuf {
            let path = dir.join(who);
            std::fs::write(&path, format!("#!/bin/sh\necho {who}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        /// `tools/build`, made to point at `target`.
        fn point_build_at(dir: &Path, target: &Path) -> PathBuf {
            let link = dir.join("tools/build");
            let _ = std::fs::remove_file(&link);
            symlink(target, &link).unwrap();
            link
        }

        fn judge(shell: &mut Shell, args: &[&str]) -> Prepared {
            let command =
                Command::from_name(&CommandName::Path("tools/build".into()), &shell.context);
            let args: Vec<_> = args.iter().map(|arg| Value::string(*arg)).collect();
            command
                .lookup(&shell.context.grants, shell.baseline())
                .admit(&Mooring::adrift(), shell)
                .expect("an unrestricted shell admits the file")
                .prepare(&args, Ownership::Kept, shell)
                .expect("the file can be run")
        }

        fn stdout_of(prepared: Prepared) -> String {
            let out = prepared.launch.output().expect("the file runs");
            String::from_utf8(out.stdout).unwrap()
        }

        /// Building only describes, so a stop in force refuses no build; the
        /// start sees it, and is the run's stop with no child.
        #[test]
        fn a_stop_is_the_starts_and_never_the_builds() {
            let (tmp, mut shell) = workdir();
            point_build_at(tmp.path(), &plant(tmp.path(), "a"));
            let stopped = crate::process::DurableRoot::default().child();
            stopped.cancel(crate::process::CancelCause::Cancelled);
            shell.scope = crate::process::Scope::clone(&stopped);
            let prepared = judge(&mut shell, &[]);
            let started = prepared
                .start(
                    Connection::new(&Source::Empty, &Sink::Stdout, &Sink::Stderr).unwrap(),
                    crate::process::Placement::Inherit,
                    &shell.scope,
                )
                .map_err(|failure| failure.settle(&shell.scope));
            assert!(
                matches!(
                    started,
                    Err(crate::shell::runtime::Break::Escape(
                        crate::shell::runtime::Escape::Stopped(crate::shell::runtime::Stop {
                            cause: crate::process::CancelCause::Cancelled,
                            ..
                        })
                    ))
                ),
                "{:?}",
                started.err()
            );
        }

        /// No sandbox confines a start under no grant, so it claims no line.
        #[test]
        fn an_unconfined_start_claims_no_line() {
            use super::Started;
            let (tmp, mut shell) = workdir();
            point_build_at(tmp.path(), &plant(tmp.path(), "a"));
            let prepared = judge(&mut shell, &[]);
            let discard = Sink::File(std::sync::Arc::new(crate::path::open_discard().unwrap()));
            let Ok(Started {
                mut spawned,
                denial_pids,
                ..
            }) = prepared.start(
                Connection::new(&Source::Empty, &discard, &discard).unwrap(),
                crate::process::Placement::Inherit,
                &shell.scope,
            )
            else {
                panic!("the file starts");
            };
            let _ = spawned.child.reap();
            assert_eq!(denial_pids, None);
        }

        /// The control: the harness tells `a` from `b`, so the next test cannot
        /// pass by the two being alike.
        #[test]
        fn a_judged_link_runs_its_target() {
            for who in ["a", "b"] {
                let (tmp, mut shell) = workdir();
                point_build_at(tmp.path(), &plant(tmp.path(), who));
                assert_eq!(stdout_of(judge(&mut shell, &[])), format!("{who}\n"));
            }
        }

        /// A pipeline's earlier stage may retarget a later one's link after the
        /// command is looked up and before it launches: the judged `a` still runs.
        #[test]
        fn a_link_retargeted_after_judgment_runs_the_file_judged() {
            let (tmp, mut shell) = workdir();
            let (a, b) = (plant(tmp.path(), "a"), plant(tmp.path(), "b"));
            let link = point_build_at(tmp.path(), &a);

            let plan = judge(&mut shell, &[]);
            point_build_at(tmp.path(), &b);
            assert_eq!(
                crate::test_helper::real(&link),
                crate::test_helper::real(&b),
                "the link moved"
            );
            assert_eq!(stdout_of(plan), "a\n");
        }

        /// A `#!` script reached through a link is handed its real path by the
        /// kernel, so the lexical idiom `${0%/*}` (what `dirname "$0"` computes)
        /// finds its siblings.
        #[test]
        fn a_script_reached_through_a_link_sees_its_real_path() {
            let (tmp, mut shell) = workdir();
            let bin = tmp.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            let tool = bin.join("tool");
            std::fs::write(
                &tool,
                "#!/bin/sh\nprintf '%s\\n' \"$0\"; read -r l < \"${0%/*}/sibling\"; printf '%s\\n' \"$l\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(bin.join("sibling"), "SIBLING\n").unwrap();
            point_build_at(tmp.path(), &tool);

            let plan = judge(&mut shell, &[]);
            let real = std::fs::canonicalize(&tool).unwrap();
            assert_eq!(stdout_of(plan), format!("{}\nSIBLING\n", real.display()));
        }

        #[test]
        fn a_program_sees_the_spelling_it_was_named_by_as_argv0() {
            let (tmp, mut shell) = workdir();
            let link = point_build_at(tmp.path(), Path::new("/bin/sh"));

            let plan = judge(&mut shell, &["-c", r#"printf %s "$0""#]);
            assert_eq!(stdout_of(plan), link.to_string_lossy());
        }

        /// The residual, stated: a program is started by its real name, so a
        /// native ELF replaced at that name between `admit` and the exec is what
        /// runs.
        #[cfg(target_os = "linux")]
        #[test]
        fn a_native_elf_replaced_at_its_name_after_admit_runs_the_replacement() {
            let (tmp, mut shell) = workdir();
            let tool = tmp.path().join("tools/build");
            std::fs::copy("/bin/sh", &tool).unwrap();
            let plan = judge(&mut shell, &["-c", "echo admitted"]);
            std::fs::rename(plant(tmp.path(), "replacement"), &tool).unwrap();
            assert_eq!(stdout_of(plan), "replacement\n");
        }
    }

    #[cfg(unix)]
    mod exec_error_tests {
        use super::{Origin, Program};
        use crate::authority::sandbox::Image;
        use crate::fact::{Failure, Reason, Unrunnable};
        use std::io::{Error, ErrorKind};
        use std::path::{Path, PathBuf};

        fn not_runnable(why: Unrunnable) -> Reason {
            Reason::process("run", Failure::not_runnable("run", why))
        }

        fn file(real: crate::path::RealPath) -> Image {
            Image::File {
                real,
                arg0: "run".into(),
            }
        }

        /// `run`, typed as a path, from `image`, whose `#!` named
        /// `interpreter` when it was built.
        fn run(image: &Image, interpreter: Option<&str>) -> Program {
            Program {
                shown: "run".into(),
                origin: Origin::Path,
                image: image.clone(),
                interpreter: interpreter.map(PathBuf::from),
            }
        }

        /// `e` starting `image`, under no sandbox that withholds anything.
        fn unwithheld(e: &Error, image: &Image, interpreter: Option<&str>) -> Reason {
            run(image, interpreter).exec_error(e, || None).reason
        }

        fn enoent(image: &Image, interpreter: Option<&str>) -> Reason {
            unwithheld(&Error::from(ErrorKind::NotFound), image, interpreter)
        }

        /// The interpreter named is the one read when the image was built, never
        /// what the file says by the time it failed.
        #[test]
        fn a_file_still_there_lacks_the_interpreter_its_image_names() {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("run");
            std::fs::write(&script, "#!/bin/sh\n").unwrap();
            let real = crate::test_helper::real(&script);
            assert_eq!(
                enoent(&file(real.clone()), Some("/no/such/interpreter")),
                not_runnable(Unrunnable::MissingInterpreter(Some(
                    "/no/such/interpreter".into()
                )))
            );
            assert_eq!(
                enoent(&file(real), None),
                not_runnable(Unrunnable::MissingInterpreter(None))
            );
        }

        #[test]
        fn a_vanished_file_or_a_tool_is_not_found() {
            let dir = tempfile::tempdir().unwrap();
            let gone = file(crate::test_helper::place(dir.path().join("gone")));
            for image in [&gone, &Image::Tool("ls")] {
                assert_eq!(
                    enoent(image, Some("/bin/sh")),
                    Reason::process("run", Failure::NotFound),
                    "{image:?}"
                );
            }
            assert!(!Path::new(&dir.path().join("gone")).exists());
        }

        #[test]
        fn eacces_is_permission_denied_and_the_rest_is_io() {
            let tool = Image::Tool("ls");
            assert_eq!(
                unwithheld(&Error::from_raw_os_error(libc::EACCES), &tool, None),
                not_runnable(Unrunnable::PermissionDenied)
            );
            let e = Error::from_raw_os_error(libc::E2BIG);
            assert_eq!(
                unwithheld(&e, &tool, None),
                not_runnable(Unrunnable::Io(Box::new(crate::io::failure(&e))))
            );
        }

        /// An interpreter the launch's own sandbox withholds is denied whether
        /// Seatbelt refused it (`EACCES`) or bwrap hid it (`ENOENT`); with no
        /// such verdict, a hidden interpreter reads as missing.  The verdict is
        /// asked for only on those two.
        #[test]
        fn a_withheld_interpreter_is_denied_however_the_kernel_says_so() {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("run");
            std::fs::write(&script, "#!/opt/x/bin/python3\n").unwrap();
            let program = run(
                &file(crate::test_helper::real(&script)),
                Some("/opt/x/bin/python3"),
            );
            let hint = "its interpreter /opt/x/bin/python3 is not admitted by the grant";
            let withheld = || Some(hint.to_owned());

            for errno in [libc::ENOENT, libc::EACCES] {
                let denied = program.exec_error(&Error::from_raw_os_error(errno), withheld);
                assert_eq!(
                    denied.reason,
                    not_runnable(Unrunnable::PermissionDenied),
                    "{errno}"
                );
                assert_eq!(denied.hints().first().map(|h| &**h), Some(hint), "{errno}");
            }
            let missing = program.exec_error(&Error::from_raw_os_error(libc::ENOENT), || None);
            assert_eq!(
                missing.reason,
                not_runnable(Unrunnable::MissingInterpreter(Some(
                    "/opt/x/bin/python3".into()
                )))
            );
            assert!(missing.hints().is_empty(), "{:?}", missing.hints());
            let e2big = Error::from_raw_os_error(libc::E2BIG);
            let other = program.exec_error(&e2big, || {
                panic!("only a denial or an absence asks for the verdict")
            });
            assert_eq!(
                other.reason,
                not_runnable(Unrunnable::Io(Box::new(crate::io::failure(&e2big))))
            );
            assert!(other.hints().is_empty(), "{:?}", other.hints());
        }

        /// A program found by a PATH search is named where the search found
        /// it, as lookup names it, and a missing interpreter is reported there.
        #[test]
        fn a_searched_program_fails_at_the_path_it_was_found_at() {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("run");
            std::fs::write(&script, "#!/no/such/interpreter\n").unwrap();
            let program = Program {
                shown: "run".into(),
                origin: Origin::Bare,
                image: Image::File {
                    real: crate::test_helper::real(&script),
                    arg0: script.clone(),
                },
                interpreter: Some("/no/such/interpreter".into()),
            };
            let found = crate::path::shown(&script);
            let error = program.exec_error(&Error::from(ErrorKind::NotFound), || None);
            assert_eq!(
                error.reason,
                Reason::process(
                    "run",
                    Failure::not_runnable(
                        found.clone(),
                        Unrunnable::MissingInterpreter(Some("/no/such/interpreter".into()))
                    )
                )
            );
            assert_eq!(
                error.describe(),
                format!(
                    "run: {found}: its interpreter /no/such/interpreter or its loader was not found"
                )
            );
        }
    }

    /// A confined launch that cannot start is a process failure, as an
    /// unconfined one is, never a trampoline's exit status.
    #[cfg(target_os = "macos")]
    mod confined_start {
        use super::{Command, Started};
        use crate::authority::policy::{Capabilities, ExecGrant, ExecKey, FsPolicy};
        use crate::authority::sandbox::Ownership;
        use crate::fact::{Failure, Reason, Unrunnable};
        use crate::io::child::Connection;
        use crate::io::{Sink, Source};
        use crate::ir::CommandName;
        use crate::path::FrozenPath;
        use crate::process::Placement;
        use crate::shell::runtime::{Break, Error, Mooring, Settled};
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;

        /// `body` as an executable `run` in a fresh directory.
        fn script(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let script = std::fs::canonicalize(dir.path()).unwrap().join("run");
            std::fs::write(&script, body).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            (dir, script)
        }

        /// `script` started under a grant reading its directory and admitting
        /// `exec`, with `between` run after the launch is built.
        fn launched(
            script: &Path,
            exec: Option<ExecGrant>,
            between: impl FnOnce(),
        ) -> (String, Settled<Started>) {
            let mut shell = crate::test_helper::core_shell();
            let layer = crate::test_helper::layer(Capabilities {
                exec,
                fs: Some(FsPolicy {
                    read_prefixes: vec![FrozenPath::freeze(script.parent().unwrap()).unwrap()],
                    ..FsPolicy::default()
                }),
                ..Capabilities::default()
            });
            shell.with_layer(layer, |shell| {
                let command = Command::from_name(
                    &CommandName::Path(script.to_string_lossy().into_owned()),
                    &shell.context,
                )
                .lookup(&shell.context.grants, shell.baseline())
                .admit(&Mooring::adrift(), shell)
                .expect("the grant admits the script");
                let prepared = command
                    .prepare(&[], Ownership::Kept, shell)
                    .expect("the launch builds");
                let shown = prepared.program.shown.clone();
                let placement = Placement::Inherit;
                between();
                (
                    shown,
                    prepared
                        .start(
                            Connection::new(&Source::Empty, &Sink::Stdout, &Sink::Stderr).unwrap(),
                            placement,
                            &shell.scope,
                        )
                        .map_err(|failure| failure.settle(&shell.scope)),
                )
            })
        }

        /// [`launched`], which must not start: the error.
        fn started(
            script: &Path,
            exec: Option<ExecGrant>,
            between: impl FnOnce(),
        ) -> (String, Error) {
            let (shown, settled) = launched(script, exec, between);
            let Err(Break::Error(error)) = settled else {
                panic!("the confined launch must not start");
            };
            (shown, error)
        }

        /// A confined direct child is its program, `execve`d in place: it claims
        /// its own pid, and no other.
        #[test]
        fn a_confined_start_claims_exactly_its_childs_pid() {
            let (_dir, script) = script("#!/bin/sh\nexit 0\n");
            let (_, settled) = launched(&script, None, || ());
            let Ok(Started {
                mut spawned,
                denial_pids,
                ..
            }) = settled
            else {
                panic!("the confined script starts");
            };
            let pid = spawned.child.id();
            let _ = spawned.child.reap();
            assert_eq!(denial_pids, Some(std::collections::HashSet::from([pid])));
        }

        fn not_runnable(shown: &str, why: Unrunnable) -> Reason {
            Reason::process(shown, Failure::not_runnable(shown, why))
        }

        #[test]
        fn a_confined_script_whose_interpreter_is_missing_is_missing_interpreter() {
            let (_dir, script) = script("#!/no/such/interpreter\n");
            let (shown, error) = started(&script, None, || ());
            assert_eq!(
                error.reason,
                not_runnable(
                    &shown,
                    Unrunnable::MissingInterpreter(Some("/no/such/interpreter".into()))
                )
            );
        }

        #[test]
        fn a_confined_file_that_lost_its_execute_bit_is_not_runnable() {
            let (_dir, script) = script("#!/bin/sh\nexit 0\n");
            let (shown, error) = started(&script, None, || {
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
            });
            assert_eq!(
                error.reason,
                not_runnable(&shown, Unrunnable::PermissionDenied)
            );
            assert!(error.hints().is_empty(), "{:?}", error.hints());
        }

        /// The hint names the interpreter read through the judged handle before
        /// the spawn, not what the script names by the time the kernel refuses.
        #[test]
        fn a_refused_interpreters_hint_names_the_one_read_before_the_spawn() {
            let (_dir, script) = script("#!/bin/sh\n");
            let exec = ExecGrant::of([ExecKey::Path(FrozenPath::freeze(&script).unwrap())], []);
            let (shown, error) = started(&script, Some(exec), || {
                std::fs::write(&script, "#!/usr/bin/false\n").unwrap();
            });
            assert_eq!(
                error.reason,
                not_runnable(&shown, Unrunnable::PermissionDenied)
            );
            let hint = error.hints().first().map(|h| &**h).unwrap_or_default();
            assert!(
                hint.contains("its interpreter /bin/sh is not admitted"),
                "{hint}"
            );
            assert!(!hint.contains("/usr/bin/false"), "{hint}");
        }
    }
}

#[cfg(unix)]
mod alone {
    use super::super::*;
    use crate::authority::policy::{Capabilities, ExecGrant};
    use crate::fact::ExitCode as _;
    use crate::shell::runtime::{CapturePolicy, Observed, Prov};
    use crate::source::CallSite;
    use std::os::unix::fs::PermissionsExt;

    /// A shell whose `PATH` is a directory holding `on-path`, which exits 9
    /// unless its first argument is `one argument`, and with its second
    /// otherwise.
    fn on_path() -> (tempfile::TempDir, Shell) {
        let tmp = tempfile::tempdir().unwrap();
        let program = tmp.path().join("on-path");
        std::fs::write(
            &program,
            "#!/bin/sh\n[ \"$1\" = 'one argument' ] || exit 9\nexit \"$2\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(CwdPath::new(tmp.path().to_path_buf()).unwrap());
        shell
            .context
            .set_env_var("PATH", tmp.path().to_str().unwrap())
            .unwrap();
        (tmp, shell)
    }

    #[test]
    fn a_bare_name_and_a_path_both_run_with_argv_intact() {
        let (_tmp, mut shell) = on_path();
        for program in ["on-path", "./on-path"] {
            for code in [0, 7] {
                let result = shell.run_external(
                    program,
                    &[
                        Value::string("one argument"),
                        Value::string(code.to_string()),
                    ],
                    &Mooring::adrift(),
                );
                if code == 0 {
                    assert!(matches!(result, Ok(Value::Unit)), "{result:?}");
                } else {
                    assert_eq!(result.unwrap_err().code(), code);
                }
            }
        }
    }

    #[test]
    fn the_record_names_the_program_and_sites_the_visible_register() {
        let (_tmp, mut shell) = on_path();
        let wrapper = Prov::Resolved(Box::new((
            "wrapper".into(),
            CallSite {
                script: "wrapper.ral".into(),
                line: 2,
                col: 3,
            },
        )));
        let _site = shell.local.audit.enter_site(wrapper);
        let scope = shell.local.audit.open(CapturePolicy::Off);
        let result = shell.run_external(
            "on-path",
            &[Value::string("one argument"), Value::string("7")],
            &Mooring::adrift(),
        );
        let Err(Break::Error(error)) = result else {
            panic!("the child exits 7")
        };
        assert!(
            error.prov.is_none(),
            "an outer visible frame may still claim it"
        );
        let trail = shell.local.audit.close(scope);
        let [observation] = trail.as_slice() else {
            panic!("one command observation")
        };
        let Observed::Command(command) = &observation.what else {
            panic!("the external command")
        };
        assert_eq!(
            command.error.as_deref(),
            Some("on-path: exited with status 7")
        );
        assert_eq!(observation.site, shell.call_site());
    }

    #[test]
    fn an_empty_exec_grant_refuses_before_running() {
        let (_tmp, mut shell) = on_path();
        shell
            .context
            .grants
            .push(crate::test_helper::layer(Capabilities {
                exec: Some(ExecGrant::default()),
                ..Capabilities::default()
            }));
        let result = shell.run_external(
            "on-path",
            &[Value::string("one argument"), Value::string("0")],
            &Mooring::adrift(),
        );
        assert!(result.is_err());
    }
}

/// A non-enveloped confined startup report, independent of host sandbox support.
#[cfg(unix)]
pub(in crate::shell::exec) fn joined_startup_report(
    shell: &mut super::Shell,
    script: &str,
    report: crate::process::report::Report,
) -> super::Prepared {
    use super::*;
    use crate::process::Launch;
    let mooring = Mooring::adrift();
    let mut prepared = Command::from_name(&CommandName::Path("/bin/sh".into()), &shell.context)
        .lookup(&shell.context.grants, shell.baseline())
        .admit(&mooring, shell)
        .expect("admit shell")
        .prepare(&[], Ownership::Kept, shell)
        .expect("prepare shell");
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", script]);
    prepared.launch = ProgramLaunch::Direct(Launch::trampolined(command, report));
    prepared
}
