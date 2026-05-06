//! Internal helpers for `EffectiveGrant`'s decision methods.
//!
//! These functions implement the actual stack walks behind exec, fs,
//! editor, and shell checks.  They are not entry points — every call
//! site goes through `EffectiveGrant` so the decision authority is
//! visible at the type level.  The audit-node emission shared by exec
//! and fs lives here too.

use super::effective::check_grant_bool;
use super::exec::{ExecVerdict, evaluate_exec};
use crate::types::{
    Audit, Dynamic, EditorPolicy, Error, EvalSignal, ExecNode, ExecPolicy, FsPolicy, Location,
    ShellPolicy, Value,
};

/// Validate an exec capability check against the active stack and emit
/// an audit node if auditing is on.
pub(super) fn check_exec_args_impl(
    dynamic: &Dynamic,
    display_name: &str,
    policy_names: &[&str],
    args: &[String],
    audit: &mut Audit,
    location: &Location,
) -> Result<(), EvalSignal> {
    let verdict = evaluate_exec(dynamic, policy_names);
    let result: Result<(), EvalSignal> = match verdict {
        ExecVerdict::Unrestricted => Ok(()),
        ExecVerdict::Denied => Err(EvalSignal::Error(
            Error::new(format!("command '{display_name}' denied by active grant"), 1)
                .with_hint(
                    "add the command to the grant exec map \
                     (or its directory to exec_dirs) to allow it",
                ),
        )),
        ExecVerdict::Allowed(ExecPolicy::Allow) => Ok(()),
        // Unreachable in practice: a layer that resolves to `Deny`
        // returns `LayerExec::Denied` upstream and short-circuits to
        // `ExecVerdict::Denied`.  Keep the arm so the match stays
        // exhaustive against the full `ExecPolicy` lattice.
        ExecVerdict::Allowed(ExecPolicy::Deny) => Err(EvalSignal::Error(
            Error::new(format!("command '{display_name}' denied by active grant"), 1),
        )),
        ExecVerdict::Allowed(ExecPolicy::Subcommands(allowed)) => match args.first() {
            None => Err(EvalSignal::Error(
                Error::new(
                    format!(
                        "command '{display_name}' requires an allowed subcommand \
                         under the active grant"
                    ),
                    1,
                )
                .with_hint(format!("allowed subcommands: {}", allowed.join(", "))),
            )),
            Some(first) => {
                if allowed.iter().any(|candidate| candidate == first) {
                    Ok(())
                } else {
                    Err(EvalSignal::Error(
                        Error::new(
                            format!(
                                "command '{display_name}' subcommand '{first}' \
                                 denied by active grant"
                            ),
                            1,
                        )
                        .with_hint(format!("allowed subcommands: {}", allowed.join(", "))),
                    ))
                }
            }
        },
    };

    let has_exec_policy = dynamic
        .capabilities_stack
        .iter()
        .any(|ctx| ctx.exec.is_some());
    if has_exec_policy {
        emit_capability_audit(dynamic, "exec", result.is_ok(), audit, location, |pairs| {
            pairs.push(("name".into(), Value::String(display_name.into())));
            if let Some(resolved_name) =
                policy_names.iter().find(|candidate| **candidate != display_name)
            {
                pairs.push(("resolved".into(), Value::String((*resolved_name).into())));
            }
            let args_val: Vec<Value> = args.iter().map(|a| Value::String(a.clone())).collect();
            pairs.push(("args".into(), Value::List(args_val)));
        });
    }

    result
}

pub(super) fn check_fs_read_impl(
    dynamic: &Dynamic,
    path: &str,
    audit: &mut Audit,
    location: &Location,
) -> Result<(), EvalSignal> {
    check_fs_op(dynamic, path, "read", |fs| &fs.read_prefixes, audit, location)
}

pub(super) fn check_fs_write_impl(
    dynamic: &Dynamic,
    path: &str,
    audit: &mut Audit,
    location: &Location,
) -> Result<(), EvalSignal> {
    check_fs_op(dynamic, path, "write", |fs| &fs.write_prefixes, audit, location)
}

/// Editor bool gate — delegates to the shared `check_grant_bool`.
pub(super) fn check_editor_bool(
    dynamic: &Dynamic,
    msg: impl Fn() -> String,
    field: impl Fn(&EditorPolicy) -> bool,
) -> Result<(), EvalSignal> {
    check_grant_bool(dynamic, msg, |ctx| ctx.editor.as_ref().map(&field))
}

/// Shell bool gate — delegates to the shared `check_grant_bool`.
pub(super) fn check_shell_bool(
    dynamic: &Dynamic,
    msg: impl Fn() -> String,
    field: impl Fn(&ShellPolicy) -> bool,
) -> Result<(), EvalSignal> {
    check_grant_bool(dynamic, msg, |ctx| ctx.shell.as_ref().map(&field))
}

/// Decide an `op` (read / write) on a single resolved path
/// against the active capabilities stack.  An access succeeds
/// when, at every layer that has an `fs` opinion, the path falls
/// inside some prefix in `get_prefixes` and outside every entry
/// in `deny_paths`.  Region membership is alias-aware
/// containment via [`crate::path::path_within`], so a deny on
/// `/etc/secrets` covers `/etc/secrets/foo` and a grant on
/// `~/.local` (post-freeze: `/Users/.../.local`) covers everything
/// underneath.
///
/// Reads and writes consult the same deny set — there is one
/// deny region per layer, not two.  See SPEC §11.2.
fn check_fs_op(
    dynamic: &Dynamic,
    path: &str,
    op: &str,
    get_prefixes: impl Fn(&FsPolicy) -> &[String],
    audit: &mut Audit,
    location: &Location,
) -> Result<(), EvalSignal> {
    if path == "/dev/null" {
        return Ok(());
    }
    let resolver = dynamic.resolver_for_check();
    let resolved = resolver.check(path);
    let mut denied = false;
    let mut granted_prefix: Option<String> = None;
    let mut has_fs_policy = false;

    for ctx in &dynamic.capabilities_stack {
        if let Some(fs) = &ctx.fs {
            has_fs_policy = true;
            let in_deny = fs
                .deny_paths
                .iter()
                .any(|d| crate::path::path_within(&resolved, &resolver.check(d)));
            if in_deny {
                denied = true;
                break;
            }
            let prefixes = get_prefixes(fs);
            let mut hit: Option<&str> = None;
            for prefix in prefixes {
                if crate::path::path_within(&resolved, &resolver.check(prefix)) {
                    hit = Some(prefix.as_str());
                    break;
                }
            }
            match hit {
                Some(p) => granted_prefix = Some(p.to_string()),
                None => {
                    denied = true;
                    break;
                }
            }
        }
    }

    if has_fs_policy {
        emit_capability_audit(dynamic, "fs", !denied, audit, location, |pairs| {
            pairs.push(("op".into(), Value::String(op.into())));
            pairs.push(("path".into(), Value::String(path.into())));
            if !denied && let Some(gp) = granted_prefix {
                pairs.push(("granted".into(), Value::String(gp)));
            }
        });
    }

    if denied {
        Err(EvalSignal::Error(Error::new(
            format!("fs {op} denied by grant: {}", resolved.display()),
            1,
        )))
    } else {
        Ok(())
    }
}

fn emit_capability_audit(
    dynamic: &Dynamic,
    kind: &str,
    ok: bool,
    audit: &mut Audit,
    location: &Location,
    fill: impl FnOnce(&mut Vec<(String, Value)>),
) {
    if !dynamic.should_audit_capabilities(audit) {
        return;
    }
    let decision = if ok { "allowed" } else { "denied" };
    let script = location.call_site.script.clone();
    let line = location.call_site.line;
    let col = location.call_site.col;
    let principal = dynamic.env_vars().get("USER").cloned().unwrap_or_default();
    let mut node = ExecNode::capability_check(kind, decision, &script, line, col);
    node.principal = principal;
    if let Value::Map(ref mut pairs) = node.value {
        fill(pairs);
    }
    if let Some(tree) = &mut audit.tree {
        tree.push(node);
    }
}
