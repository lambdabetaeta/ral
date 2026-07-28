//! Pre-spawn vetting: a [`CommandIdentity`] and its argv values become a
//! [`SpawnPlan`], or a focused refusal.
//!
//! Existence, then argv shape, then grant policy through
//! `Shell::check_exec_call` — the order is diagnostic priority: "does this
//! command exist?" outranks "is this argument the right shape?".  Both
//! single-command exec and pipeline stages consume the plan, so the rules
//! live in exactly one place.

use crate::ir::CommandName;
use crate::types::{Break, Context, Error, Settled, Shell, Value};

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

/// 127 when a bare name misses on `PATH`, 126 when a file of that name is
/// there but lacks `+x`.  Bundled tools short-circuit ahead of the probe, so
/// the in-binary implementation wins on hosts that ship a same-named system
/// binary; path and tilde heads are left to the kernel's ENOENT at spawn.
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

/// Stringify `args`, refusing any shape the syscall boundary cannot carry.
fn validate_argv(id: &CommandIdentity, args: &[Value], shell: &Shell) -> Settled<Vec<String>> {
    for arg in args {
        if let Some(sig) = reject_exec_arg(id, arg, shell) {
            return Err(sig);
        }
    }
    Ok(args.iter().map(std::string::ToString::to_string).collect())
}

/// Per-argument shape gate: nothing here can reach `execve(2)`, so each
/// refusal names the idiom that lowers it — `...$xs` for a list, `to-bytes`
/// or a decode for bytes.
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

    /// Bundled names short-circuit the disk probe: `ls` passes on an empty
    /// `PATH`.  Gated because without `coreutils` the bundled set is empty.
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

    /// A bare name that is neither bundled nor on `PATH` surfaces 127.
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
