//! Pre-spawn vetting: a [`Head`] and its argv values become a [`SpawnPlan`],
//! or a focused refusal.
//!
//! Existence, then argv shape, then grant policy through `Shell::check_exec`
//! — the order is diagnostic priority: "does this command exist?" outranks
//! "is this argument the right shape?".  Both single-command exec and
//! pipeline stages consume the plan, so the rules live in exactly one place.

use crate::capability::Admitted;
use crate::types::{Break, Error, RefusedArg, Settled, Shell, Value};

use super::head::Head;

/// A vetted call, ready for [`super::process::build_command`].  `shown` is the
/// name diagnostics and audit report; `admitted` is what runs, with what.
pub(crate) struct SpawnPlan {
    pub(crate) shown: String,
    pub(crate) admitted: Admitted,
}

/// Vet a resolved [`Head`] for spawn: 127 when it names nothing, 126 when
/// the walk stopped at a file lacking `+x` — read off the head's own walk,
/// never re-probed.
pub(crate) fn vet(head: &Head, args: &[Value], shell: &mut Shell) -> Settled<SpawnPlan> {
    let program = head
        .program
        .as_ref()
        .map_err(|missing| Break::Error(missing.error(&head.shown, &shell.context)))?;
    let args = validate_argv(&head.shown, args)?;
    let admitted = shell.check_exec(&head.shown, program.clone(), args)?;
    Ok(SpawnPlan {
        shown: head.shown.clone(),
        admitted,
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
    let refusal = RefusedArg::of_value(arg)?;
    Some(
        Error::new(
            format!(
                "cannot pass {} to external command '{cmd}'",
                arg.type_name()
            ),
            1,
        )
        .with_hint(refusal.remedy(cmd))
        .into(),
    )
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::capability::Program;
    use crate::ir::CommandName;
    use std::path::Path;

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
    fn refused(head: &Head, shell: &mut Shell) -> Error {
        match vet(head, &[], shell) {
            Err(Break::Error(e)) => e,
            Err(other) => panic!("expected a refusal, got {other:?}"),
            Ok(_) => panic!("expected a refusal, got a plan"),
        }
    }

    /// The headline regression: a file sitting in the directory the user just
    /// `cd`'d into is not on `PATH`, and a `PATH` ending in `;` does not put it
    /// there.  Naming it bare is 127 — "no such command" — never 126.
    #[cfg(windows)]
    #[test]
    fn bare_name_of_a_cwd_file_is_127_not_126() {
        crate::path::forget_located_commands();
        let here = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let name = plant(here.path(), "zzcwdonly");

        let mut shell = Shell::default();
        shell.seed_cwd(here.path().to_path_buf());
        shell
            .context
            .set_env_var("PATH", format!("{};", elsewhere.path().to_string_lossy()));

        let head = Head::resolve(&CommandName::Bare(name.into()), &shell.context);
        let e = refused(&head, &mut shell);
        assert_eq!(e.exit_code(), 127);
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
        let mut shell = Shell::default();
        shell.seed_cwd(here.path().to_path_buf());

        let head = Head::resolve(&CommandName::Path(r".\zztool".into()), &shell.context);
        let e = refused(&head, &mut shell);
        assert_eq!(e.exit_code(), 127);
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

        let mut shell = Shell::default();
        shell.context.set_env_var("PATH", "./bin");
        shell.seed_cwd(tmp.path().to_path_buf());
        let head = Head::resolve(&CommandName::Bare(name.as_str().into()), &shell.context);
        assert!(
            matches!(&head.program, Ok(Program::File { path, .. }) if *path == bin.join(&name))
        );
        vet(&head, &[], &mut shell).expect("a resolved name must vet");

        shell.seed_cwd(elsewhere.path().to_path_buf());
        let head = Head::resolve(&CommandName::Bare(name.into()), &shell.context);
        assert_eq!(refused(&head, &mut shell).exit_code(), 127);
    }

    /// Bundled names short-circuit the disk probe: `ls` vets on an empty
    /// `PATH`.  Gated because without `coreutils` the bundled set is empty.
    #[cfg(feature = "coreutils")]
    #[test]
    fn bundled_name_vets_with_empty_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::default();
        shell
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let head = Head::resolve(&CommandName::Bare("ls".into()), &shell.context);
        let plan = vet(&head, &[], &mut shell).expect("bundled name must not 127");
        assert_eq!(plan.shown, "ls");
    }

    /// A path head naming no file is a missing command, and the refusal names
    /// the path.
    #[test]
    fn a_path_head_naming_no_file_produces_127() {
        let mut shell = Shell::default();
        let head = std::env::temp_dir().join("no-such-dir").join("tool");
        let head = Head::resolve(
            &CommandName::Path(head.to_string_lossy().into_owned()),
            &shell.context,
        );
        let e = refused(&head, &mut shell);
        assert_eq!(e.exit_code(), 127);
        assert!(e.message.contains("no-such-dir"), "{}", e.message);
    }

    /// A bare name that is neither bundled nor on `PATH` surfaces 127.
    #[test]
    fn missing_non_bundled_name_produces_127() {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::default();
        shell
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let head = Head::resolve(
            &CommandName::Bare("definitely-not-a-real-tool-xyz".into()),
            &shell.context,
        );
        assert_eq!(refused(&head, &mut shell).exit_code(), 127);
    }
}
