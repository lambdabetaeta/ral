//! Point-of-use capability gates.
//!
//! Every runtime yes/no asked as an action is attempted — exec and its
//! argv, an fs read or write, the editor and shell flags, head admission
//! — folds the whole dynamic [`GrantStack`], so a verdict is authority
//! intersected across every layer, never one frame.  Only the exec and fs
//! gates reach a real OS resource, and only they audit.  The sibling
//! [`super::sandbox`] projects the same authority onto the OS sandbox.

use super::exec::{Admit, ExecNames, ExecVerdict, evaluate_exec};
use crate::path::{NormalizedPrefix, Resolver, path_within};
use crate::types::{
    Audit, CallSite, Capabilities, Context, ExecNode, FsPolicy, GrantStack, Map, Settled, Value,
    sig, sig_hint,
};

/// Gate a command and its argv against the stack's exec opinions.  Audits
/// only when some layer holds such an opinion, as the fs gate does.
pub(crate) fn check_exec_args(
    ctx: &Context,
    display_name: &str,
    deny_names: &[&str],
    policy_names: &[&str],
    args: &[String],
    audit: &mut Audit,
    site: CallSite,
) -> Settled<()> {
    let names = ExecNames {
        deny: deny_names,
        allow: policy_names,
    };
    let result: Settled<()> = match evaluate_exec(&ctx.grants, names) {
        ExecVerdict::Unrestricted | ExecVerdict::Allowed(Admit::Any) => Ok(()),
        ExecVerdict::Denied => Err(sig_hint(
            format!("command '{display_name}' denied by active grant"),
            "add the command to the grant exec map \
             (or its directory, keyed with a trailing '/') to allow it",
        )),
        ExecVerdict::Allowed(Admit::Subcommands(allowed)) => {
            let hint = || {
                format!(
                    "allowed subcommands (matched against the command's first argument): {}",
                    allowed.iter().cloned().collect::<Vec<_>>().join(", ")
                )
            };
            match args.first() {
                Some(first) if allowed.contains(first) => Ok(()),
                Some(first) => Err(sig_hint(
                    format!("command '{display_name}' subcommand '{first}' denied by active grant"),
                    hint(),
                )),
                None => Err(sig_hint(
                    format!(
                        "command '{display_name}' requires an allowed subcommand \
                         under the active grant"
                    ),
                    hint(),
                )),
            }
        }
    };

    if ctx.grants.exec().next().is_some() {
        emit_capability_audit(ctx, "exec", result.is_ok(), audit, site, |m| {
            m.insert("name".into(), Value::String(display_name.into()));
            if let Some(resolved_name) = policy_names
                .iter()
                .find(|candidate| **candidate != display_name)
            {
                m.insert("resolved".into(), Value::String((*resolved_name).into()));
            }
            let args_val: Vec<Value> = args.iter().map(|a| Value::String(a.clone())).collect();
            m.insert("args".into(), Value::list(args_val));
        });
    }

    result
}

/// Which fs region a check consults: the read or the write prefix set.
pub(crate) enum FsOp {
    Read,
    Write,
}

impl FsOp {
    fn label(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    fn prefixes<'a>(&self, fs: &'a FsPolicy) -> &'a [NormalizedPrefix] {
        match self {
            Self::Read => &fs.read_prefixes,
            Self::Write => &fs.write_prefixes,
        }
    }
}

/// The pure half of [`check_fs_op`]'s decision: does the stack admit `op`
/// on the resolved path, and under which prefix?  `Unrestricted` exactly
/// when no layer held an `fs` opinion, so there is nothing to audit.
pub(super) enum FsVerdict {
    Unrestricted,
    /// The innermost matching prefix, for the audit record.
    Granted(String),
    Denied,
}

/// Fold the stack's `fs` opinions over a resolved path: it passes when, at
/// every opining layer, the path lies inside some prefix of the op's region
/// and outside every `deny_paths` entry — one deny region per layer covers
/// both reads and writes.  Membership is region containment,
/// alias-aware, via [`path_within`].
///
/// Prefixes are re-resolved against the live disk on every check rather
/// than read off the frozen policy: composition is a statement about the
/// policy, enforcement one about the world.
pub(super) fn fs_verdict(
    grants: &GrantStack,
    resolver: &Resolver,
    resolved: &std::path::Path,
    op: &FsOp,
) -> FsVerdict {
    let mut granted: Option<String> = None;
    let mut saw = false;
    for fs in grants.fs() {
        saw = true;
        let in_deny = fs
            .deny_paths
            .iter()
            .any(|d| path_within(resolved, &resolver.check(d.as_str())));
        if in_deny {
            return FsVerdict::Denied;
        }
        match op
            .prefixes(fs)
            .iter()
            .find(|prefix| path_within(resolved, &resolver.check(prefix.as_str())))
        {
            Some(prefix) => granted = Some(prefix.as_str().to_string()),
            None => return FsVerdict::Denied,
        }
    }
    match (saw, granted) {
        (true, Some(prefix)) => FsVerdict::Granted(prefix),
        _ => FsVerdict::Unrestricted,
    }
}

/// Decide an `op` on one resolved path, audit it, and mint the `Break` on
/// denial: [`fs_verdict`] is the decision, this the reporting around it.
/// `/dev/null` is exempt from both regions as a discard device.
pub(crate) fn check_fs_op(
    ctx: &Context,
    path: &crate::path::ResolvedPath,
    op: &FsOp,
    audit: &mut Audit,
    site: CallSite,
) -> Settled<()> {
    if path.as_path().as_os_str() == "/dev/null" {
        return Ok(());
    }
    let resolved = path.canonicalise_lenient();
    let verdict = fs_verdict(&ctx.grants, &ctx.resolver(), &resolved, op);

    if !matches!(verdict, FsVerdict::Unrestricted) {
        let denied = matches!(verdict, FsVerdict::Denied);
        emit_capability_audit(ctx, "fs", !denied, audit, site, |m| {
            m.insert("op".into(), Value::String(op.label().into()));
            m.insert("path".into(), Value::String(path.display().to_string()));
            if let FsVerdict::Granted(prefix) = &verdict {
                m.insert("granted".into(), Value::String(prefix.clone()));
            }
        });
    }

    match verdict {
        FsVerdict::Denied => Err(sig(format!(
            "fs {} denied by grant: {}",
            op.label(),
            resolved.display()
        ))),
        FsVerdict::Unrestricted | FsVerdict::Granted(_) => Ok(()),
    }
}

/// Head-only admission, before any argv is known: classification and the
/// `which` inspector consult it to refuse a denied head with a focused
/// error rather than let the call reach [`check_exec_args`].
pub(crate) fn admits_head(ctx: &Context, id: &crate::runtime::command::CommandIdentity) -> bool {
    let allow = id.policy_names(ctx);
    let deny = id.deny_names_from(allow.clone());
    let allow_refs: Vec<&str> = allow.iter().map(String::as_str).collect();
    let deny_refs: Vec<&str> = deny.iter().map(String::as_str).collect();
    let names = ExecNames {
        deny: &deny_refs,
        allow: &allow_refs,
    };
    !matches!(evaluate_exec(&ctx.grants, names), ExecVerdict::Denied)
}

pub(crate) fn check_editor_read(ctx: &Context, subcmd: &str) -> Settled<()> {
    check_grant_bool(
        ctx,
        || format!("denied: _ed-{subcmd} requires editor.read"),
        |caps| caps.editor.as_ref().map(|e| e.read),
    )
}

pub(crate) fn check_editor_write(ctx: &Context, subcmd: &str) -> Settled<()> {
    check_grant_bool(
        ctx,
        || format!("denied: _ed-{subcmd} requires editor.write"),
        |caps| caps.editor.as_ref().map(|e| e.write),
    )
}

pub(crate) fn check_editor_tui(ctx: &Context) -> Settled<()> {
    check_grant_bool(
        ctx,
        || "denied: _ed-tui requires editor.tui".into(),
        |caps| caps.editor.as_ref().map(|e| e.tui),
    )
}

pub(crate) fn check_shell_chdir(ctx: &Context) -> Settled<()> {
    check_grant_bool(
        ctx,
        || "denied: cd requires shell.chdir".into(),
        |caps| caps.shell.as_ref().map(|s| s.chdir),
    )
}

/// Deny if any layer votes `false`; `test` returns `None` to abstain, so
/// silence permits.
fn check_grant_bool(
    ctx: &Context,
    msg: impl Fn() -> String,
    test: impl Fn(&Capabilities) -> Option<bool>,
) -> Settled<()> {
    for caps in &ctx.grants {
        if test(caps) == Some(false) {
            return Err(sig(msg()));
        }
    }
    Ok(())
}

fn emit_capability_audit(
    context: &Context,
    kind: &str,
    ok: bool,
    audit: &mut Audit,
    site: CallSite,
    fill: impl FnOnce(&mut Map),
) {
    if !context.should_audit_capabilities(audit) {
        return;
    }
    let decision = if ok { "allowed" } else { "denied" };
    let principal = context.principal();
    let mut fields = Map::new();
    fill(&mut fields);
    let node = ExecNode::capability_check(kind, decision, site, principal, fields);
    audit.push(node);
}
