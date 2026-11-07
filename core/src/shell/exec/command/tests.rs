//! The operation's path, tested stage by stage: a submodule per former file.

mod head {
    use super::super::*;
    use crate::authority::policy::{Baseline, Capabilities, ExecGrant, ExecKey, GrantStack};
    #[cfg(unix)]
    use crate::fact::NotRunnable;
    use crate::path::FrozenPath;
    use std::path::Path;

    fn admits_head(ctx: &Context, head: Head) -> bool {
        head.select(&ctx.grants, &Baseline::default()).is_ok()
    }

    fn selected(head: Head) -> Head<Admission> {
        head.select(&GrantStack::root(), &Baseline::default())
            .unwrap_or_else(|_| panic!("an unrestricted stack admits the head"))
    }

    #[test]
    fn a_tilde_head_is_shown_expanded_against_env_home() {
        let mut shell = crate::test_helper::core_shell();
        shell.context.set_env_var("HOME", "/tmp/home");
        let head = Head::resolve(
            &CommandName::TildePath(crate::path::TildePath {
                suffix: Some("/.local/bin/claude".into()),
            }),
            &shell.context,
        );
        assert_eq!(head.shown, "/tmp/home/.local/bin/claude");
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

    fn path_head(file: &Path) -> CommandName {
        CommandName::Path(file.to_string_lossy().into_owned())
    }

    #[cfg(unix)]
    fn is_not_runnable_at(error: &Error, at: &str) -> bool {
        matches!(
            error.reason.failure(),
            Some(Failure::NotRunnable(NotRunnable { found, .. })) if found == at
        )
    }

    fn file_of(head: &Head<Admission>) -> (&Path, &crate::path::RealPath) {
        match head.program.as_ref().map(Admitted::program) {
            Ok(Program::File { arg0, located }) => (arg0, located.real()),
            other => panic!("expected a host file, got {other:?}"),
        }
    }

    fn canonical(path: &Path) -> crate::path::RealPath {
        crate::test_helper::trace(path).real().clone()
    }

    /// A path head runs the file it names from the shell's cwd, and is
    /// judged by that file's real path.
    #[test]
    fn a_relative_path_head_is_the_file_under_the_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "configure");
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(tmp.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );

        let head = selected(Head::resolve(
            &CommandName::Path(format!("./{name}")),
            &shell.context,
        ));
        let (path, real) = file_of(&head);
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

        let head = selected(Head::resolve(
            &CommandName::Path("link/../tool".into()),
            &shell.context,
        ));
        let (path, judged) = file_of(&head);
        assert_eq!(judged, &canonical(&real.join("tool")));
        assert_eq!(path, tmp.path().join("link/../tool"));
    }

    #[test]
    fn a_path_head_naming_no_file_has_no_program() {
        let shell = crate::test_helper::core_shell();
        let head = std::env::temp_dir().join("no-such-dir").join("configure");
        let head = selected(Head::resolve(&path_head(&head), &shell.context));
        assert!(head.program.as_ref().is_err_and(is_absent));
    }

    /// The kernel refuses `file/` with ENOTDIR, so a trailing slash must not
    /// be folded away into a runnable spelling.
    #[cfg(unix)]
    #[test]
    fn a_trailing_slash_on_a_file_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "tool");
        let shell = crate::test_helper::core_shell();
        let head = tmp.path().join(name).to_string_lossy().into_owned() + "/";
        let head = selected(Head::resolve(&CommandName::Path(head), &shell.context));
        assert!(head.program.as_ref().is_err_and(is_absent));
    }

    #[test]
    fn an_empty_exec_allow_list_refuses_a_nonexistent_path_head() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = under(ExecGrant::default());
        for file in [tmp.path().join("missing"), tmp.path().join("absent/tool")] {
            let head = Head::resolve(&path_head(&file), &ctx);
            let denied = head
                .select(&ctx.grants, &Baseline::default())
                .expect_err("an empty exec allowance refuses even an absent name");
            assert_eq!(denied.at, path::shown(&file));
            assert!(denied.denial.error.describe().contains("exec `allow` list"));
        }
    }

    #[test]
    fn read_withheld_heads_are_refused_like_present_programs() {
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
            Head::resolve(&path_head(file), &ctx)
                .select(&ctx.grants, &Baseline::default())
                .expect_err("running a read-withheld name is refused")
                .denial
        };
        let cases = [
            tmp.path().join("missing.bat"),
            tmp.path().join("absent/tool.bat"),
            #[cfg(unix)]
            looping.clone(),
        ]
        .map(|file| {
            let refusal = denied(&file);
            assert!(refusal.error.describe().contains("not readable"));
            (file, refusal)
        });
        #[cfg(unix)]
        std::fs::rename(&looping, tmp.path().join("saved-loop")).unwrap();
        for (file, before) in cases {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::copy(&source, &file).unwrap();
            let after = denied(&file);
            assert_eq!(
                before.error.describe(),
                after.error.describe(),
                "{}",
                file.display()
            );
            assert_eq!(
                before.error.hints(),
                after.error.hints(),
                "{}",
                file.display()
            );
            assert_eq!(before.check, after.check, "{}", file.display());
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
            ctx.set_env_var("PATH", path.clone());
            let head = Head::resolve(&CommandName::Bare(name.as_str().into()), &ctx);
            if prime_memo {
                assert_eq!(
                    path::locate(&name, Some(&path), Some(ctx.cwd())),
                    Some(d1.path().join(&name))
                );
            }
            let head = head
                .select(&ctx.grants, &Baseline::default())
                .unwrap_or_else(|_| panic!("the second PATH candidate is admitted"));
            assert_eq!(file_of(&head).0, d2.path().join(&name));
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
        let select = |dirs: [&Path; 3]| {
            let mut ctx = under(ExecGrant::of([dir_key(&admitted)], []));
            let path = std::env::join_paths(dirs).unwrap().into_string().unwrap();
            ctx.set_env_var("PATH", path);
            Head::resolve(&CommandName::Bare("zzplain".into()), &ctx)
                .select(&ctx.grants, &Baseline::default())
        };
        let first = select([absent.as_path(), denied.as_path(), admitted.as_path()])
            .expect_err("the first non-absence is a refusal");
        assert_eq!(first.at, path::shown(denied.join("zzplain")));
        let first = select([absent.as_path(), admitted.as_path(), denied.as_path()])
            .unwrap_or_else(|_| panic!("the non-executable file precedes the refusal"));
        assert!(
            matches!(first.program, Err(error) if is_not_runnable_at(&error, &path::shown(&plain)))
        );
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
        ctx.set_env_var("PATH", path.clone());
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
        let head = Head::resolve(&CommandName::Bare(name.as_str().into()), &ctx)
            .select(&ctx.grants, &Baseline::default())
            .unwrap_or_else(|_| panic!("the third PATH candidate is admitted"));
        assert_eq!(steps() - before, once);
        assert_eq!(file_of(&head).0, dirs[2].path().join(&name));
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
            .set_env_var("PATH", elsewhere.path().to_string_lossy().into_owned());

        let head = selected(Head::resolve(
            &CommandName::Bare(name.into()),
            &shell.context,
        ));
        assert!(head.program.as_ref().is_err_and(is_absent));
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
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let head = selected(Head::resolve(
            &CommandName::Bare("zznoexec".into()),
            &shell.context,
        ));
        assert!(
            matches!(head.program, Err(error) if is_not_runnable_at(&error, &path::shown(&file)))
        );
    }

    /// A bundled name is its tool whatever `PATH` holds: no walk is taken.
    #[cfg(feature = "coreutils")]
    #[test]
    fn a_bundled_name_is_its_tool() {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = crate::test_helper::core_shell();
        shell
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());
        let head = Head::resolve(&CommandName::Bare("ls".into()), &shell.context);
        assert!(matches!(head.program, Target::Tool(tool) if tool == "ls"));
    }

    /// A bare key is the file the host `PATH` finds, so a scoped `PATH` that
    /// plants another `git` does not inherit `git: allow`.
    #[test]
    fn a_scoped_path_cannot_redirect_a_bare_key() {
        crate::path::forget_located_commands();
        let dir = tempfile::tempdir().unwrap();
        let name = plant(dir.path(), "git");
        let mut ctx = under(ExecGrant::of([ExecKey::Name("git".into())], []));
        ctx.set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let head = Head::resolve(&CommandName::Bare("git".into()), &ctx);
        assert_eq!(
            file_of(&selected(Head::resolve(
                &CommandName::Bare("git".into()),
                &ctx
            )))
            .1,
            &canonical(&dir.path().join(name)),
        );
        assert!(!admits_head(&ctx, head));
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
    /// dir: the file that runs is `bash` whatever the head spelled, so the
    /// veto beats the dir.
    #[test]
    fn a_bare_deny_beats_the_allow_dir_a_path_head_sits_in() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "bash");
        let ctx = under(ExecGrant::of(
            [dir_key(tmp.path())],
            [ExecKey::Name("bash".into())],
        ));

        let head = Head::resolve(&path_head(&tmp.path().join(name)), &ctx);
        assert!(!admits_head(&ctx, head));
    }

    /// The mirror: a bare `rg: allow` is the host's `rg`, so a planted path
    /// head `evil/rg` does not inherit it.
    #[test]
    fn a_bare_allow_does_not_admit_a_planted_path_head() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "rg");
        let ctx = under(ExecGrant::of([ExecKey::Name("rg".into())], []));

        let head = Head::resolve(&path_head(&tmp.path().join(name)), &ctx);
        assert!(!admits_head(&ctx, head));
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

        let head = Head::resolve(&path_head(&link), &ctx);
        assert_eq!(
            file_of(&selected(Head::resolve(&path_head(&link), &ctx))).1,
            &canonical(&bin.join("bash")),
        );
        assert!(!admits_head(&ctx, head));
    }

    /// The same link, judged by a denied *region* its program lies in.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_deny_dir_is_denied_by_the_file_it_names() {
        let tmp = tempfile::tempdir().unwrap();
        let (bin, planted, link) = planted_symlink(tmp.path());
        let ctx = under(ExecGrant::of([dir_key(&planted)], [dir_key(&bin)]));

        let head = Head::resolve(&path_head(&link), &ctx);
        assert!(!admits_head(&ctx, head));
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

        let head = Head::resolve(&path_head(&link), &ctx);
        assert!(admits_head(&ctx, head));
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

        let head = Head::resolve(&path_head(&copy), &ctx);
        assert!(admits_head(&ctx, head));
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
        shell.context.set_env_var("PATH", "./bin");

        let head = selected(Head::resolve(
            &CommandName::Bare(name.as_str().into()),
            &shell.context,
        ));
        assert_eq!(file_of(&head).0, bin.join(&name));
    }

    #[cfg(unix)]
    impl Program {
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
        Admitted, Capabilities, ExecGrant, ExecKey, FsPolicy, GrantLayer, Program,
    };
    use crate::authority::sandbox::Ownership;
    use crate::fact::{Check, FailureCode as _, Observed, Resource};
    use crate::ir::CommandName;
    use crate::path::FrozenPath;
    use crate::shell::runtime::{CapturePolicy, Mooring, Shell};
    use std::path::Path;

    /// A head admitted with no surface, then prepared with no argv.
    fn plan(head: Head, shell: &mut Shell) -> Settled<Prepared> {
        let head = head.admit(&Mooring::adrift(), shell)?;
        prepare(head, &[], Ownership::Kept, shell)
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

    /// The error `prepare` refuses `head` with.
    fn refused(head: Head, shell: &mut Shell) -> Error {
        match plan(head, shell) {
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
        crate::test_helper::layer(Capabilities {
            exec: Some(ExecGrant::of(
                [ExecKey::Dir(FrozenPath::from_surface(
                    crate::path::utf8(dir).unwrap(),
                ))],
                [],
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
            .set_env_var("PATH", format!("{};", elsewhere.path().to_string_lossy()));

        let head = Head::resolve(&CommandName::Bare(name.into()), &shell.context);
        let e = refused(head, &mut shell);
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
    fn an_extensionless_path_head_is_127_with_a_hint() {
        let here = tempfile::tempdir().unwrap();
        plant(here.path(), "zztool");
        let mut shell = crate::test_helper::core_shell();
        shell.seed_cwd(
            crate::path::CwdPath::new(here.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );

        let head = Head::resolve(&CommandName::Path(r".\zztool".into()), &shell.context);
        let e = refused(head, &mut shell);
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
        shell.context.set_env_var("PATH", "./bin");
        shell.seed_cwd(
            crate::path::CwdPath::new(tmp.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        let head = Head::resolve(&CommandName::Bare(name.as_str().into()), &shell.context)
            .admit(&Mooring::adrift(), &mut shell)
            .expect("an unrestricted shell admits the head");
        assert!(matches!(
            head.program.as_ref().map(Admitted::program),
            Ok(Program::File { arg0, .. }) if *arg0 == bin.join(&name)
        ));
        prepare(head, &[], Ownership::Kept, &shell).expect("a resolved name must prepare");

        shell.seed_cwd(
            crate::path::CwdPath::new(elsewhere.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        let head = Head::resolve(&CommandName::Bare(name.into()), &shell.context);
        assert_eq!(refused(head, &mut shell).failure_code().get(), 127);
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
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let head = Head::resolve(&CommandName::Bare("ls".into()), &shell.context);
        let plan = plan(head, &mut shell).expect("bundled name must not 127");
        assert_eq!(plan.shown, "ls");
    }

    /// The 126 case of a path head: a file lacking `+x`, or a directory,
    /// which only the admitted program is asked about.
    #[cfg(unix)]
    #[test]
    fn a_path_head_that_cannot_be_run_is_not_executable() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain");
        std::fs::write(&file, "").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut shell = crate::test_helper::core_shell();
        for head in [file.as_path(), tmp.path()] {
            let head = Head::resolve(
                &CommandName::Path(head.to_string_lossy().into_owned()),
                &shell.context,
            );
            let e = refused(head, &mut shell);
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
        let head = Head::resolve(
            &CommandName::Path(file.to_string_lossy().into_owned()),
            &shell.context,
        );
        let e = shell.with_layer(no_exec(), |shell| refused(head, shell));
        assert!(e.describe().contains("denied by active grant"), "{e:?}");
    }

    /// Only a refused walk is permission denied: a link cycle is reported as
    /// the OS words it.
    #[cfg(unix)]
    #[test]
    fn a_looping_path_head_is_not_reported_as_permission_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let looping = tmp.path().join("loop");
        std::os::unix::fs::symlink("loop", &looping).unwrap();
        let mut shell = crate::test_helper::core_shell();
        let head = Head::resolve(
            &CommandName::Path(looping.to_string_lossy().into_owned()),
            &shell.context,
        );
        let e = refused(head, &mut shell);
        assert_eq!(e.failure_code().get(), 126, "{e:?}");
        assert!(!e.describe().contains("permission denied"), "{e:?}");
    }

    #[test]
    fn refused_and_absent_candidates_record_the_first_refusal_with_a_bare_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let name = plant(tmp.path(), "zzdeniedbare");
        let dirs = ["admitted", "first-denied", "later-denied"].map(|dir| {
            let dir = tmp.path().join(dir);
            std::fs::create_dir(&dir).unwrap();
            dir
        });
        let mut shell = crate::test_helper::core_shell();
        shell.context.set_env_var(
            "PATH",
            std::env::join_paths(&dirs).unwrap().into_string().unwrap(),
        );
        for present in [false, true] {
            if present {
                for dir in &dirs[1..] {
                    std::fs::copy(tmp.path().join(&name), dir.join(&name)).unwrap();
                }
            }
            let (e, trail) = shell.with_layer(run_from(&dirs[0]), |shell| {
                let scope = shell.local.audit.open(CapturePolicy::Off);
                let head = Head::resolve(&CommandName::Bare(name.as_str().into()), &shell.context);
                let e = refused(head, shell);
                (e, shell.local.audit.close(scope))
            });
            assert!(e.describe().contains("denied by active grant"), "{e:?}");
            let [grant, bare] = e.hints() else {
                panic!("the grant's hint, then the bare-name lead: {:?}", e.hints());
            };
            assert_eq!(
                &**bare,
                format!("'{name}' is no program in any PATH directory the grant runs from")
            );
            assert_ne!(grant, bare);
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

    /// A path head naming no file is a missing command, and the refusal names
    /// the path.
    #[test]
    fn a_path_head_naming_no_file_produces_127() {
        let mut shell = crate::test_helper::core_shell();
        let head = std::env::temp_dir().join("no-such-dir").join("tool");
        let head = Head::resolve(
            &CommandName::Path(head.to_string_lossy().into_owned()),
            &shell.context,
        );
        let e = refused(head, &mut shell);
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
        shell.context.set_env_var(
            "PATH",
            std::env::join_paths([d1.path(), d2.path()])
                .unwrap()
                .into_string()
                .unwrap(),
        );
        let head = Head::resolve(
            &CommandName::Bare("definitely-not-a-real-tool-xyz".into()),
            &shell.context,
        );
        assert_eq!(refused(head, &mut shell).failure_code().get(), 127);
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
                let head = Head::resolve(
                    &CommandName::Path(file.to_string_lossy().into_owned()),
                    &shell.context,
                )
                .admit(&Mooring::adrift(), shell)
                .expect("the grant runs this directory");
                let Err(Break::Error(e)) = prepare(head, &[Value::Unit], Ownership::Kept, shell)
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
            .set_env_var("PATH", tmp.path().to_string_lossy().into_owned());
        shell.with_layer(run_from(tmp.path()), |shell| {
            let head = Head::resolve(&CommandName::Bare("zzplain".into()), &shell.context)
                .admit(&Mooring::adrift(), shell)
                .expect("the grant admits the non-executable file");
            let Err(Break::Error(e)) = prepare(head, &[Value::Unit], Ownership::Kept, shell) else {
                panic!("a non-executable candidate cannot be run");
            };
            assert_eq!(e.failure_code().get(), 126, "{e:?}");
            assert!(e.describe().contains(&crate::path::shown(&file)), "{e:?}");
        });
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
            (Value::Unit, Ty::Unit),
            (Value::Bool(true), Ty::Bool),
            (Value::Int(1), Ty::Int),
            (
                Value::Float(crate::data::Finite::new(1.0).expect("finite")),
                Ty::Float,
            ),
            (Value::string("x"), Ty::String),
            (Value::bytes(vec![1]), Ty::Bytes),
            (Value::List(vec![].into()), Ty::list(Ty::String)),
            (Value::map(vec![]), Ty::map(Ty::Int)),
            // A tagged value renders, so neither side refuses it.
            (Value::variant("ok", None), Ty::Variant(Row::Empty)),
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
            RefusedArg::of_ty(&Ty::Record(Row::Empty)),
            Some(RefusedArg::Map)
        );
        // A variable is not a shape: the runtime keeps that question.
        assert_eq!(RefusedArg::of_ty(&Ty::Var(crate::ty::TyVar(0))), None);
    }
}

mod stdio {
    use super::super::{boundary_stdin, stdin_stdio};
    use crate::io::Source;
    use crate::process::{DurableRoot, Membership, Pgid, Scope, StdioSpec};
    use crate::shell::runtime::Shell;

    /// The terminal on the shell's stdin.
    fn at_a_tty() -> Shell {
        let mut shell = crate::test_helper::core_shell();
        shell.io.stdin = Source::Terminal;
        shell
    }

    /// The terminal survives in ral's own group or a lent one, and is EOF to
    /// any other group of its own.
    #[test]
    fn the_terminal_survives_only_where_its_group_can_read_it() {
        let shell = at_a_tty();
        assert!(matches!(boundary_stdin(&shell, true), Source::Terminal));
        assert!(matches!(boundary_stdin(&shell, false), Source::Empty));
    }

    /// A joined group reads what its owner chose, lent or not.
    #[test]
    fn a_joined_group_passes_its_owners_source_through() {
        let mut shell = at_a_tty();
        let group = Pgid::from_raw(1).expect("1 is positive");
        let stage = Scope::clone(&DurableRoot::default().child());
        shell.session.stage = Some(Membership::new(group, stage));
        assert!(matches!(boundary_stdin(&shell, false), Source::Terminal));
        shell.io.stdin = Source::Empty;
        assert!(matches!(boundary_stdin(&shell, true), Source::Empty));
    }

    /// Only the terminal is withheld: a non-tty fd 0 is a reader, and
    /// reaches every group.
    #[cfg(unix)]
    #[test]
    fn a_non_tty_stdin_reaches_every_group() {
        let mut shell = at_a_tty();
        let (r, _w) = crate::process::cloexec_pipe().expect("data pipe");
        shell.io.stdin = Source::Reader(crate::io::SourceReader::pipe(r));
        assert!(matches!(boundary_stdin(&shell, false), Source::Reader(_)));
    }

    /// The source is authoritative: `Empty` wires `/dev/null` and `Terminal`
    /// inherits fd 0, whatever fd 0 is.
    #[test]
    fn the_childs_source_decides_its_stdin() {
        assert!(matches!(stdin_stdio(&Source::Empty), Ok(StdioSpec::Null)));
        assert!(matches!(
            stdin_stdio(&Source::Terminal),
            Ok(StdioSpec::Inherit)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_stage_shaped_source_is_borrowed() {
        use crate::io::SourceReader;

        let (r, _w) = crate::process::cloexec_pipe().expect("data pipe");
        let stdin = Source::Reader(
            SourceReader::pipe(r).interruptible(Scope::clone(&DurableRoot::default())),
        );

        let stdio = stdin_stdio(&stdin).expect("stdin_stdio");
        assert!(matches!(stdio, StdioSpec::Owned(_)));

        // The source is borrowed, never taken, so it still has a reader.
        assert!(stdin.reader().expect("reader").is_some());
    }
}

mod foreground {
    use super::super::*;
    use crate::process::{DurableRoot, Membership, Pgid, TerminalLease};

    /// A session owning a terminal lease plus a `Leased` mooring: the REPL, or
    /// a terminal-launched script.  Stdout defaults to `Sink::Terminal`.
    fn leased_shell() -> (Shell, Mooring) {
        let mut shell = crate::test_helper::core_shell();
        shell.session.interactive = true;
        shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
        let mooring = Mooring {
            terminal_access: TerminalAccess::Leased,
            home: DurableRoot::default().child(),
            cleanup_grace: std::time::Duration::ZERO,
            ..Mooring::adrift()
        };
        (shell, mooring)
    }

    fn captured(shell: &mut Shell) {
        shell.io.stdout = crate::io::new_buffer().0;
    }

    /// Never foreground: no loan, and the group `interactive` leaves it in.
    fn assert_background(placed: (PgidPolicy, Option<PreparedTerminalLoan>), interactive: bool) {
        let (policy, loan) = placed;
        assert!(loan.is_none(), "a background child is lent nothing");
        if interactive {
            assert!(matches!(policy, PgidPolicy::Inherit), "{policy:?}");
        } else {
            assert!(matches!(policy, PgidPolicy::NewSession), "{policy:?}");
        }
    }

    #[cfg(unix)]
    fn assert_foreground(placed: (PgidPolicy, Option<PreparedTerminalLoan>)) {
        let (policy, loan) = placed;
        assert!(matches!(policy, PgidPolicy::NewLeader), "{policy:?}");
        assert!(loan.is_some(), "a foreground child is lent the terminal");
    }

    /// A joined group is never lent: dropping a loan restores the terminal,
    /// out from under the owner.
    #[test]
    fn a_stages_child_joins_its_group_unlent() {
        let (mut shell, mooring) = leased_shell();
        let group = Pgid::from_raw(1).expect("1 is positive");
        shell.session.stage = Some(Membership::new(group, shell.scope.clone()));
        let (policy, loan) = placement(&shell, &mooring, false);
        assert!(
            matches!(policy, PgidPolicy::Join(g) if g == group),
            "{policy:?}"
        );
        assert!(loan.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn a_leased_terminal_bound_child_foregrounds() {
        let (shell, mooring) = leased_shell();
        assert_foreground(placement(&shell, &mooring, false));
    }

    /// A capture's sink is a buffer, not the terminal, so it must not steal the
    /// foreground even under a `Leased` mooring.
    #[test]
    fn a_leased_captured_child_stays_in_the_background() {
        let (mut shell, mooring) = leased_shell();
        captured(&mut shell);
        assert_background(placement(&shell, &mooring, false), true);
    }

    /// The loan foregrounds despite the buffer sink, a lone command as a
    /// pipeline: otherwise `fzf`, drawing on `/dev/tty` from a background
    /// pgroup, raises SIGTTOU (the CTRL-R failure).
    #[test]
    #[cfg(unix)]
    fn an_explicit_loan_foregrounds_a_captured_child() {
        let (mut shell, mut mooring) = leased_shell();
        captured(&mut shell);
        mooring.terminal_access = TerminalAccess::ExplicitLoan;
        assert_foreground(placement(&shell, &mooring, false));
    }

    /// A terminal-launched script holds `Leased` exactly like the REPL: gating on
    /// interactivity would strand `claude` or `fzf` in the background on SIGTTOU.
    #[test]
    #[cfg(unix)]
    fn a_terminal_scripts_leased_child_foregrounds() {
        let (mut shell, mooring) = leased_shell();
        shell.session.interactive = false;
        assert_foreground(placement(&shell, &mooring, false));
    }

    /// A headless background child leads a session of its own.
    #[test]
    fn a_headless_captured_child_leads_a_session() {
        let (mut shell, mooring) = leased_shell();
        shell.session.interactive = false;
        captured(&mut shell);
        assert_background(placement(&shell, &mooring, false), false);
    }

    #[test]
    fn an_enveloped_child_never_foregrounds() {
        let (shell, mut mooring) = leased_shell();
        assert_background(placement(&shell, &mooring, true), true);
        mooring.terminal_access = TerminalAccess::ExplicitLoan;
        assert_background(placement(&shell, &mooring, true), true);
    }

    /// A `Denied` mooring never foregrounds even though the session owns a lease
    /// — exarch's tool runs: the borrow is unreachable, the handoff unbuildable.
    #[test]
    fn a_denied_run_never_foregrounds() {
        let (shell, mut mooring) = leased_shell();
        mooring.terminal_access = TerminalAccess::Denied;
        assert!(
            shell.session.terminal_lease.is_some(),
            "session owns a lease"
        );
        assert_background(placement(&shell, &mooring, false), true);
    }

    /// A launch that never owned the terminal foreground (backgrounded `ral … &`,
    /// a piped or tty-less eval) minted no lease, so there is nothing to borrow.
    #[test]
    fn no_lease_never_foregrounds() {
        let (mut shell, mooring) = leased_shell();
        shell.session.terminal_lease = None;
        assert_background(placement(&shell, &mooring, false), true);
    }
}

mod process {
    use super::super::*;

    mod denials {
        use super::{Claim, with_denials};
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
                let error =
                    with_denials("tool", failure.clone(), &Claim::Unconfined, Instant::now());
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
            let claim = Claim::Confined(std::collections::HashSet::new());
            for (failure, hint) in [
                (Failure::Exited(128), braked.as_deref()),
                (Failure::Exited(384), None),
                (Failure::Exited(-1), None),
                (Failure::Signaled(128), None),
            ] {
                let error = with_denials("tool", failure.clone(), &claim, Instant::now());
                assert_eq!(error.reason, Reason::process("tool", failure));
                assert_eq!(error.hints().first().map(|h| &**h), hint);
            }
        }
    }

    /// What runs is the file that was judged, by its real path and under the
    /// spelling the user wrote, however the spelling moves in between.
    #[cfg(unix)]
    mod judged_program {
        use super::{Head, Prepared, prepare};
        use crate::authority::sandbox::Ownership;
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
            let head = Head::resolve(&CommandName::Path("tools/build".into()), &shell.context);
            let args: Vec<_> = args.iter().map(|arg| Value::string(*arg)).collect();
            let admitted = head
                .admit(&Mooring::adrift(), shell)
                .expect("an unrestricted shell admits the file");
            prepare(admitted, &args, Ownership::Kept, shell).expect("the file can be run")
        }

        fn stdout_of(prepared: Prepared) -> String {
            let out = prepared.launch.output().expect("the file runs");
            String::from_utf8(out.stdout).unwrap()
        }

        /// Building only describes, so a stop in force refuses no build; the
        /// start sees it, and is the run's stop with no child.
        #[test]
        fn a_stop_is_the_starts_and_never_the_builds() {
            use super::start;
            let (tmp, mut shell) = workdir();
            point_build_at(tmp.path(), &plant(tmp.path(), "a"));
            let stopped = crate::process::DurableRoot::default().child();
            stopped.cancel(crate::process::CancelCause::Cancelled);
            shell.scope = crate::process::Scope::clone(&stopped);
            let prepared = judge(&mut shell, &[]);
            let started = start(prepared, crate::process::PgidPolicy::Inherit, &shell.scope);
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
            use super::{Born, Claim, Started, start};
            let (tmp, mut shell) = workdir();
            point_build_at(tmp.path(), &plant(tmp.path(), "a"));
            let mut prepared = judge(&mut shell, &[]);
            prepared.launch.stdout(crate::process::StdioSpec::Null);
            let Ok(Started {
                born: Born { mut spawned, claim },
                ..
            }) = start(prepared, crate::process::PgidPolicy::Inherit, &shell.scope)
            else {
                panic!("the file starts");
            };
            let _ = spawned.child.reap();
            assert_eq!(claim, Claim::Unconfined);
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
        /// head is resolved and before it launches: the judged `a` still runs.
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
    mod at_spawn_tests {
        use super::at_spawn;
        use crate::authority::sandbox::Image;
        use crate::fact::{Failure, Reason, Unrunnable};
        use std::io::{Error, ErrorKind};
        use std::path::Path;

        fn not_runnable(why: Unrunnable) -> Reason {
            Reason::process("run", Failure::not_runnable("run", why))
        }

        fn file(real: crate::path::RealPath) -> Image {
            Image::File {
                real,
                arg0: "run".into(),
            }
        }

        /// `e` starting `image`, whose `#!` named `interpreter` when it was
        /// built, under no sandbox that withholds anything.
        fn unwithheld(e: &Error, image: &Image, interpreter: Option<&str>) -> Reason {
            at_spawn(e, image, interpreter.map(Path::new), || None, "run").reason
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
                not_runnable(Unrunnable::Io(e.to_string()))
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
            let image = file(crate::test_helper::real(&script));
            let interpreter = Some(Path::new("/opt/x/bin/python3"));
            let hint = "its interpreter /opt/x/bin/python3 is not admitted by the grant";
            let withheld = || Some(hint.to_owned());

            for errno in [libc::ENOENT, libc::EACCES] {
                let denied = at_spawn(
                    &Error::from_raw_os_error(errno),
                    &image,
                    interpreter,
                    withheld,
                    "run",
                );
                assert_eq!(
                    denied.reason,
                    not_runnable(Unrunnable::PermissionDenied),
                    "{errno}"
                );
                assert_eq!(denied.hints().first().map(|h| &**h), Some(hint), "{errno}");
            }
            let missing = at_spawn(
                &Error::from_raw_os_error(libc::ENOENT),
                &image,
                interpreter,
                || None,
                "run",
            );
            assert_eq!(
                missing.reason,
                not_runnable(Unrunnable::MissingInterpreter(Some(
                    "/opt/x/bin/python3".into()
                )))
            );
            assert!(missing.hints().is_empty(), "{:?}", missing.hints());
            let e2big = Error::from_raw_os_error(libc::E2BIG);
            let other = at_spawn(
                &e2big,
                &image,
                interpreter,
                || panic!("only a denial or an absence asks for the verdict"),
                "run",
            );
            assert_eq!(
                other.reason,
                not_runnable(Unrunnable::Io(e2big.to_string()))
            );
            assert!(other.hints().is_empty(), "{:?}", other.hints());
        }
    }

    /// A confined launch that cannot start is a process failure, as an
    /// unconfined one is, never a trampoline's exit status.
    #[cfg(target_os = "macos")]
    mod confined_start {
        use super::{Born, Claim, Head, Started, prepare, start};
        use crate::authority::policy::{Capabilities, ExecGrant, ExecKey, FsPolicy};
        use crate::authority::sandbox::Ownership;
        use crate::fact::{Failure, Reason, Unrunnable};
        use crate::ir::CommandName;
        use crate::path::FrozenPath;
        use crate::process::PgidPolicy;
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
                let head = Head::resolve(
                    &CommandName::Path(script.to_string_lossy().into_owned()),
                    &shell.context,
                )
                .admit(&Mooring::adrift(), shell)
                .expect("the grant admits the script");
                let prepared =
                    prepare(head, &[], Ownership::Kept, shell).expect("the launch builds");
                let shown = prepared.shown.clone();
                between();
                (shown, start(prepared, PgidPolicy::Inherit, &shell.scope))
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
                born: Born { mut spawned, claim },
                ..
            }) = settled
            else {
                panic!("the confined script starts");
            };
            let pid = spawned.child.id();
            let _ = spawned.child.reap();
            assert_eq!(
                claim,
                Claim::Confined(std::collections::HashSet::from([pid]))
            );
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
