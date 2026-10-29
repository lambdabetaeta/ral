//! Pre-spawn planning: an admitted [`Head`] and its argv values become a
//! [`SpawnPlan`], or a focused refusal.
//!
//! [`Head::admit`] has decided refusal, then 127/126, before argv shape.
//! Every launch path reads that verdict here without probing again.

use crate::authority::policy::Admitted;
use crate::shell::runtime::{Break, Error, Settled, Value};
use crate::ty::RefusedArg;

use super::head::{Admission, Head};

/// A vetted call, taken by [`super::process::build_launch`].  `shown` is the
/// name diagnostics and audit report; `admitted` runs with `args`.
pub(crate) struct SpawnPlan {
    pub(crate) shown: String,
    pub(crate) admitted: Admitted,
    pub(crate) args: Vec<String>,
}

/// Plan an admitted head's launch, taking it: 127 when it names nothing,
/// 126 when the walk stopped at a file lacking `+x` or the admitted file
/// cannot be run — read off the head's own walk, never re-probed.
pub(crate) fn vet(head: Head<Admission>, args: &[Value]) -> Settled<SpawnPlan> {
    let Head { shown, program } = head;
    let admitted = program.map_err(|missing| Break::Error(missing.error(&shown)))?;
    let args = validate_argv(&shown, args)?;
    Ok(SpawnPlan {
        shown,
        admitted,
        args,
    })
}

/// Stringify `args`, refusing any shape the syscall boundary cannot carry.
fn validate_argv(cmd: &str, args: &[Value]) -> Settled<Vec<String>> {
    for arg in args {
        if let Some(sig) = reject_exec_arg(cmd, arg) {
            return Err(sig);
        }
    }
    Ok(args.iter().map(std::string::ToString::to_string).collect())
}

/// Per-argument shape gate: nothing [`RefusedArg`] names can reach `execve(2)`,
/// so each refusal carries the idiom that lowers it.
///
/// The refused set is shared with the checker, which raises the same refusal as
/// a static error wherever an argument's type is concrete.  This is the backstop
/// for what polymorphism hid from it — a `$x` whose shape only the run knows.
fn reject_exec_arg(cmd: &str, arg: &Value) -> Option<Break> {
    let refusal = arg.heads().first().copied().and_then(RefusedArg::of_head)?;
    Some(
        Error::new(format!(
            "cannot pass {} to external command '{cmd}'",
            arg.type_name()
        ))
        .with_hint(refusal.remedy(cmd))
        .into(),
    )
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::policy::{
        Capabilities, ExecGrant, ExecKey, FsPolicy, GrantLayer, Program,
    };
    use crate::fact::{Check, Observed, Resource};
    use crate::ir::CommandName;
    use crate::path::FrozenPath;
    use crate::shell::runtime::{CapturePolicy, Mooring, Shell};
    use std::path::Path;

    /// A head admitted with no surface, then planned with no argv.
    fn plan(head: Head, shell: &mut Shell) -> Settled<SpawnPlan> {
        vet(head.admit(&Mooring::adrift(), shell)?, &[])
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

    /// The exit code `vet` refuses `head` with.
    fn refused(head: Head, shell: &mut Shell) -> Error {
        match plan(head, shell) {
            Err(Break::Error(e)) => e,
            Err(other) => panic!("expected a refusal, got {other:?}"),
            Ok(_) => panic!("expected a refusal, got a plan"),
        }
    }

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
        assert_eq!(e.code(), 127);
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
        assert_eq!(e.code(), 127);
        assert_eq!(e.hint.as_deref(), Some(r"did you mean '.\zztool.bat'?"));
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
        vet(head, &[]).expect("a resolved name must vet");

        shell.seed_cwd(
            crate::path::CwdPath::new(elsewhere.path().to_path_buf())
                .expect("a tempdir is a working directory"),
        );
        let head = Head::resolve(&CommandName::Bare(name.into()), &shell.context);
        assert_eq!(refused(head, &mut shell).code(), 127);
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
            assert_eq!(e.code(), 126, "{e:?}");
            assert!(!e.message.contains("denied by active grant"), "{e:?}");
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
        assert!(e.message.contains("denied by active grant"), "{e:?}");
    }

    /// `let scratch = /tmp` runs a path, which a grant refuses; the refusal is
    /// still the grant's, and leads with the path as text without telling
    /// what lies there.
    #[test]
    fn a_denied_path_head_is_offered_as_text_ahead_of_the_grants_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let shown = tmp.path().to_string_lossy().into_owned();
        let mut shell = crate::test_helper::core_shell();
        let head = Head::resolve(&CommandName::Path(shown.clone()), &shell.context);
        let e = shell.with_layer(no_exec(), |shell| refused(head, shell));
        assert!(e.message.contains("denied by active grant"), "{e:?}");
        let hint = e.hint.as_deref().expect("a refusal carries a hint");
        let (text, grant) = hint.split_once("\n\n").expect("both hints are kept");
        assert!(text.contains(&format!("#'{shown}'#")), "{hint}");
        assert!(grant.contains("grant") && !grant.contains("#'"), "{hint}");
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
            assert!(e.message.contains("denied by active grant"), "{e:?}");
            let hint = e.hint.as_deref().expect("a refusal carries a hint");
            assert_eq!(
                hint.split_once("\n\n").expect("both hints are kept").0,
                format!("'{name}' is no program in any PATH directory the grant runs from")
            );
            assert!(!hint.contains("#'"), "{hint}");
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
        assert_eq!(e.code(), 127);
        assert!(e.message.contains("no-such-dir"), "{}", e.message);
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
        assert_eq!(refused(head, &mut shell).code(), 127);
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
                let Err(Break::Error(e)) = vet(head, &[Value::Unit]) else {
                    panic!("the admitted path cannot be run");
                };
                assert_eq!(e.code(), code, "{e:?}");
                assert!(!e.message.contains("cannot pass"), "{e:?}");
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
            let Err(Break::Error(e)) = vet(head, &[Value::Unit]) else {
                panic!("a non-executable candidate cannot be run");
            };
            assert_eq!(e.code(), 126, "{e:?}");
            assert!(e.message.contains(&crate::path::shown(&file)), "{e:?}");
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
