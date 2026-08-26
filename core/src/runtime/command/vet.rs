//! Pre-spawn vetting: a [`CommandIdentity`] and its argv values become a
//! [`SpawnPlan`], or a focused refusal.
//!
//! Existence, then argv shape, then grant policy through
//! `Shell::check_exec_call` — the order is diagnostic priority: "does this
//! command exist?" outranks "is this argument the right shape?".  Both
//! single-command exec and pipeline stages consume the plan, so the rules
//! live in exactly one place.

use crate::ir::CommandName;
use crate::path::PathSearch;
use crate::types::{Break, Error, RefusedArg, Settled, Shell, Value};

use super::identity::CommandIdentity;

/// The executable image a vetted call resolves to: a host program named by
/// resolved path (or by bare name, when the PATH walk missed), or a bundled
/// tool run as a child placement of ral itself (`ral --ral-bundled-tool
/// <tool> …`) so it shares a host external's spawn/stdio/audit machinery.
pub(crate) enum ExecImage {
    Host(String),
    BundledTool { tool: String },
}

/// A vetted call, ready for [`super::process::build_command`].  `shown` is the
/// name diagnostics and audit report; `image` is what actually runs.
pub(crate) struct SpawnPlan {
    pub(crate) shown: String,
    pub(crate) image: ExecImage,
    pub(crate) args: Vec<String>,
}

/// Vet a pre-built [`CommandIdentity`] for spawn.  The broad veto set is
/// widened from the narrow admission set rather than recomputed, because
/// `policy_names` may walk `PATH`.
pub(crate) fn vet(id: &CommandIdentity, args: &[Value], shell: &mut Shell) -> Settled<SpawnPlan> {
    check_existence(id)?;
    let arg_strs = validate_argv(id, args, shell)?;
    let policy_names = id.policy_names(&shell.context);
    let policy_refs: Vec<&str> = policy_names.iter().map(String::as_str).collect();
    let deny_names = id.deny_names_from(policy_names.clone());
    let deny_refs: Vec<&str> = deny_names.iter().map(String::as_str).collect();
    shell.check_exec_call(&id.shown, &deny_refs, &policy_refs, &arg_strs)?;
    let image = match &id.name {
        CommandName::Bare(b) if crate::uutils::is_uutils_tool(b) => {
            ExecImage::BundledTool { tool: b.clone() }
        }
        _ => ExecImage::Host(id.resolved.clone()),
    };
    Ok(SpawnPlan {
        shown: id.shown.clone(),
        image,
        args: arg_strs,
    })
}

/// 127 when a bare name misses on `PATH`, 126 when a file of that name is
/// there but lacks `+x`.  Bundled tools short-circuit ahead of the verdict, so
/// the in-binary implementation wins on hosts that ship a same-named system
/// binary; path and tilde heads are left to the kernel's ENOENT at spawn.
///
/// The verdict is *read off* the identity, never re-probed, so the walk and
/// the code it earns cannot disagree: two walks with two anchors is what let a
/// name no walk had resolved come back as "permission denied".
fn check_existence(id: &CommandIdentity) -> Settled<()> {
    let CommandName::Bare(bare) = &id.name else {
        return Ok(());
    };
    if crate::uutils::is_uutils_tool(bare) {
        return Ok(());
    }
    match &id.search {
        Some(PathSearch::Executable(_)) | None => Ok(()),
        // The walk kept the file it stopped at, so the refusal can name it:
        // "permission denied" alone leaves the user guessing which of several
        // `PATH` directories shadowed the one they meant.
        Some(PathSearch::FoundNotExecutable(found)) => Err(Break::Error(Error::new(
            format!(
                "{}: permission denied ({} is not executable)",
                id.shown,
                found.display()
            ),
            126,
        ))),
        Some(PathSearch::Missing) => Err(Break::Error(Error::new(
            crate::process::not_found_hint(&id.shown),
            127,
        ))),
    }
}

/// Stringify `args`, refusing any shape the syscall boundary cannot carry.
fn validate_argv(id: &CommandIdentity, args: &[Value], shell: &Shell) -> Settled<Vec<String>> {
    for arg in args {
        if let Some(sig) = reject_exec_arg(id, arg, shell) {
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
fn reject_exec_arg(id: &CommandIdentity, arg: &Value, shell: &Shell) -> Option<Break> {
    let cmd = id.shown.as_str();
    let refusal = RefusedArg::of_value(arg)?;
    Some(
        shell
            .err_hint(
                format!(
                    "cannot pass {} to external command '{cmd}'",
                    arg.type_name()
                ),
                refusal.remedy(cmd),
                1,
            )
            .into(),
    )
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
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

        let id = CommandIdentity::resolve(CommandName::Bare(name), &shell.context);
        match check_existence(&id) {
            Err(Break::Error(e)) => {
                assert_eq!(e.exit_code(), 127);
                assert!(
                    !format!("{e:?}").contains("permission denied"),
                    "a name no walk resolved must not be refused as unexecutable: {e:?}",
                );
            }
            other => panic!("expected 127, got {other:?}"),
        }
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
        let id = CommandIdentity::resolve(CommandName::Bare(name.clone()), &shell.context);
        assert_eq!(id.resolved, bin.join(&name).to_string_lossy());
        check_existence(&id).expect("a resolved name must vet");

        shell.seed_cwd(elsewhere.path().to_path_buf());
        let id = CommandIdentity::resolve(CommandName::Bare(name), &shell.context);
        match check_existence(&id) {
            Err(Break::Error(e)) => assert_eq!(e.exit_code(), 127),
            other => panic!("expected 127, got {other:?}"),
        }
    }

    /// Bundled names short-circuit the disk probe: `ls` passes on an empty
    /// `PATH`.  Gated because without `coreutils` the bundled set is empty.
    #[cfg(feature = "coreutils")]
    #[test]
    fn bundled_name_passes_existence_with_empty_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::default();
        shell
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let id = CommandIdentity::resolve(CommandName::Bare("ls".into()), &shell.context);
        check_existence(&id).expect("bundled name must not 127");
        assert_eq!(id.shown, "ls");
    }

    /// A bare name that is neither bundled nor on `PATH` surfaces 127.
    #[test]
    fn missing_non_bundled_name_produces_127() {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::default();
        shell
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let id = CommandIdentity::resolve(
            CommandName::Bare("definitely-not-a-real-tool-xyz".into()),
            &shell.context,
        );
        match check_existence(&id) {
            Err(Break::Error(e)) => assert_eq!(e.exit_code(), 127),
            Err(other) => panic!("expected 127 error, got {other:?}"),
            Ok(()) => panic!("missing non-bundled name must not succeed"),
        }
    }
}
