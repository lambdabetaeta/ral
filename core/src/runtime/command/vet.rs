//! Pre-spawn vetting: turn a [`CommandIdentity`] plus argv values into
//! a [`SpawnPlan`] or refuse with a focused diagnostic.
//!
//! Three judgments run in order so the diagnostic priority layers
//! correctly:
//!
//!   1. **Existence** — a bare name that doesn't resolve on PATH
//!      surfaces 127 (or 126 when the file is on disk but lacks +x).
//!      Bundled uutils tools and path / tilde literals skip the
//!      probe; the kernel produces an adequate ENOENT for the latter
//!      at spawn time.
//!   2. **Argv shape** — list / map / lambda / block / handle / bytes
//!      arguments are rejected with `...$xs` / `to-bytes` hints.
//!      Requires existence to have settled first: "does this command
//!      exist?" is a higher-priority diagnostic than "is this
//!      argument the right shape?".
//!   3. **Grant policy** — the full `(head, args)` call clears the
//!      active grant lattice via `check_exec_args`.
//!
//! The result is a [`SpawnPlan`] both single-command exec and pipeline
//! stages consume, so the vetting rules live in exactly one place.

use crate::ir::CommandName;
use crate::types::{Break, Context, Error, Settled, Shell, Value};

use super::identity::CommandIdentity;

/// The executable image a vetted call resolves to.  A host program is
/// run by path (or bare name) via `Command::new`; a bundled
/// coreutils/diffutils/ripgrep tool is run as a child placement of ral
/// itself (`ral --ral-bundled-tool <tool> …`) so it goes through the
/// same spawn/stdio/`RunningChild`/audit machinery as a host external.
pub(crate) enum ExecImage {
    Host(String),                 // resolved path (or bare name) for `Command::new`
    BundledTool { tool: String }, // run as `ral --ral-bundled-tool <tool>`
}

/// A vetted call, ready to feed [`super::process::build_command`].
/// `shown` is the surface name (diagnostics, audit), `image` is the
/// resolved executable image, and `args` is the stringified argv.
pub(crate) struct SpawnPlan {
    pub(crate) shown: String,
    pub(crate) image: ExecImage,
    pub(crate) args: Vec<String>,
}

/// Vet a pre-built [`CommandIdentity`] for spawn.
pub(crate) fn vet(id: &CommandIdentity, args: &[Value], shell: &mut Shell) -> Settled<SpawnPlan> {
    check_existence(id, &shell.mobile.context)?;
    let arg_strs = validate_argv(id, args, shell)?;
    let policy_names = id.policy_names(&shell.mobile.context);
    let policy_refs: Vec<&str> = policy_names.iter().map(String::as_str).collect();
    let deny_names = id.deny_names_from(policy_names.clone());
    let deny_refs: Vec<&str> = deny_names.iter().map(String::as_str).collect();
    shell.check_exec_call(&id.shown, &deny_refs, &policy_refs, &arg_strs)?;
    let image = match &id.name {
        CommandName::Bare(b) if crate::builtins::uutils::is_uutils_tool(b) => {
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

/// 127 when a bare name doesn't resolve on PATH, 126 when it's on
/// disk but lacks `+x`.  Path / tilde literals reach the OS unchanged
/// — kernel ENOENT at spawn is a perfectly good diagnostic.  Bundled
/// uutils tools live inside the ral binary, so they short-circuit
/// before the disk probe and stay bundled-authoritative even on
/// hosts that ship a same-named system binary.
fn check_existence(id: &CommandIdentity, ctx: &Context) -> Settled<()> {
    let CommandName::Bare(bare) = &id.name else {
        return Ok(());
    };
    if crate::builtins::uutils::is_uutils_tool(bare) {
        return Ok(());
    }
    if id.resolved != id.shown {
        return Ok(());
    }
    let path = ctx.env_overrides().get_or_host("PATH").unwrap_or_default();
    if crate::path::file_exists_on_path(bare, &path, ctx.cwd_chain()).is_some() {
        return Err(Break::Error(Error::new(
            format!("{}: permission denied", id.shown),
            126,
        )));
    }
    Err(Break::Error(Error::new(
        crate::process::not_found_hint(&id.shown),
        127,
    )))
}

/// Stringify `args`, refusing any shape the syscall boundary cannot
/// accept.  Takes `&CommandIdentity` rather than `&str` because
/// [`reject_exec_arg`] needs `id.shown` for its hints.  Diagnostic
/// ordering — existence before shape — is a property of [`vet`]'s call
/// sequence, not of this signature.
fn validate_argv(id: &CommandIdentity, args: &[Value], shell: &Shell) -> Settled<Vec<String>> {
    for arg in args {
        if let Some(sig) = reject_exec_arg(id, arg, shell) {
            return Err(sig);
        }
    }
    Ok(args.iter().map(std::string::ToString::to_string).collect())
}

/// Per-argument shape gate.  Lists / maps / lambdas / blocks /
/// handles / bytes never reach `execve(2)` — each carries its own
/// hint pointing at the idiom that lowers it (`...$xs` for lists,
/// `to-bytes` / decode for bytes).  Everything else stringifies
/// through [`Value::to_string`] at the caller.
fn reject_exec_arg(id: &CommandIdentity, arg: &Value, shell: &Shell) -> Option<Break> {
    let cmd = id.shown.as_str();
    match arg {
        Value::List(_)
        | Value::Map(_)
        | Value::Lambda { .. }
        | Value::Block { .. }
        | Value::Handle(_) => Some(
            shell
                .err_hint(
                    format!(
                        "cannot pass {} to external command '{cmd}'",
                        arg.type_name()
                    ),
                    format!("use '...' to spread a list into arguments: {cmd} ...$var"),
                    1,
                )
                .into(),
        ),
        Value::Bytes(_) => Some(
            shell
                .err_hint(
                    format!("cannot pass Bytes as argument to external command '{cmd}'"),
                    "pipe binary data via stdin with to-bytes, or decode to string first",
                    1,
                )
                .into(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Existence is bundled-first: a bare uutils name passes even
    /// when `PATH` is empty.  `ls` lives inside the ral binary, so
    /// the disk probe never fires for it.  The `coreutils` feature
    /// is what populates the bundled set; without it
    /// `is_uutils_tool` returns false for every name, so the test
    /// is feature-gated.
    #[cfg(feature = "coreutils")]
    #[test]
    fn bundled_name_passes_existence_with_empty_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::default();
        shell
            .mobile
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let id = CommandIdentity::resolve(CommandName::Bare("ls".into()), &shell.mobile.context);
        check_existence(&id, &shell.mobile.context).expect("bundled name must not 127");
        assert_eq!(id.shown, "ls");
    }

    /// A bare name that is neither bundled nor on `PATH` surfaces
    /// 127.  Confirms the bundled-first short-circuit is the only
    /// escape from the disk probe.
    #[test]
    fn missing_non_bundled_name_produces_127() {
        let dir = tempfile::tempdir().unwrap();
        let mut shell = Shell::default();
        shell
            .mobile
            .context
            .set_env_var("PATH", dir.path().to_string_lossy().into_owned());

        let id = CommandIdentity::resolve(
            CommandName::Bare("definitely-not-a-real-tool-xyz".into()),
            &shell.mobile.context,
        );
        match check_existence(&id, &shell.mobile.context) {
            Err(Break::Error(e)) => assert_eq!(e.exit_code(), 127),
            Err(other) => panic!("expected 127 error, got {other:?}"),
            Ok(()) => panic!("missing non-bundled name must not succeed"),
        }
    }
}
