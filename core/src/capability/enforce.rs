//! Point-of-use capability gates.
//!
//! Every runtime yes/no asked at the moment an action is attempted — an
//! exec invocation and its argv, an fs read or write, the editor and
//! shell feature flags, and head admission — is a function over a
//! borrowed [`Context`] that folds the whole dynamic [`GrantStack`]
//! (`ctx.grants`), so a verdict reflects authority intersected across
//! every layer, never a single frame.  The exec and fs gates touch a
//! real OS resource, so they emit an audit node when auditing is on; the
//! editor/shell/head gates are coarse feature flags and do not.  The
//! OS-renderable [`SandboxProjection`](crate::types::SandboxProjection)
//! these gates share an authority model with is built in the sibling
//! [`super::sandbox`].

use super::exec::{Admit, ExecNames, ExecVerdict, evaluate_exec};
use crate::path::{NormalizedPrefix, Resolver, path_within};
use crate::types::{
    Audit, CallSite, Capabilities, Context, ExecNode, FsPolicy, GrantStack, Map, Settled, Value,
    sig, sig_hint,
};

/// Validate an exec capability check against the active stack and emit
/// an audit node if auditing is on.
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
             (or its directory to exec_dirs) to allow it",
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
/// Carries both the audit label and the prefix accessor, so the
/// read / write distinction is named once and lives in one place.
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

/// The pure half of [`check_fs_op`]'s decision: does the grant stack admit
/// `op` on `resolved`, and under which prefix?  No layer with an opinion
/// is `Unrestricted`; every opining layer's deny-then-prefix test is
/// `Denied` or `Granted` with the last (innermost) hit prefix, matching a
/// grant stack's overriding-inward semantics.
pub(super) enum FsVerdict {
    /// No layer expressed an `fs` opinion — nothing to audit or check.
    Unrestricted,
    /// Every opining layer granted; carries the prefix the audit trail
    /// records.
    Granted(String),
    Denied,
}

/// Fold the grant stack's `fs` opinions over a resolved path: the access
/// succeeds when, at every layer with an `fs` opinion, the path falls
/// inside some prefix in the op's region and outside every entry in
/// `deny_paths`.  Region membership is alias-aware containment via
/// [`path_within`], so a deny on `/etc/secrets` covers `/etc/secrets/foo`
/// and a grant on `~/.local` (post-freeze: `/Users/.../.local`) covers
/// everything underneath.
///
/// Reads and writes consult the same deny set — there is one deny region
/// per layer, not two.  See SPEC §11.2.
///
/// Pure over its inputs: `resolved` is a value, `resolver` and `grants`
/// are borrowed policy and the `Resolver`'s bound `home`/`cwd`, not a
/// live filesystem consultation of *this* call's environment — the
/// canonicalisation `resolver.check` performs is the one place this
/// still touches disk (see `design/260727_policy_kernel_purity.md` §0:
/// enforcement is a statement about the world and must re-resolve at the
/// point of use).
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

/// Decide an `op` (read / write) on a single resolved path, emit the
/// audit node, and mint the `Break` on denial.  The decision itself is
/// [`fs_verdict`]; this is the reporting layer around it — the only part
/// of the check that touches `Value`, `Audit`, or the error type.
///
/// `path` is the sole input: a [`ResolvedPath`] the caller already
/// minted through [`Shell::resolve`](crate::types::Shell::resolve), so
/// the gate cannot see an un-resolved string.  `/dev/null` is a literal
/// contract — always permitted — and a `ResolvedPath` of `/dev/null` is
/// that absolute literal, so the short-circuit reads it off `path`
/// directly.  Symlink-following is lenient on both sides: the access
/// path canonicalises, each [`NormalizedPrefix`] canonicalises
/// (idempotent on its already-folded form), so a grant on `/tmp/foo`
/// still covers an access resolving through `/tmp` → `/private/tmp`.
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

/// Head-only admission: does the active stack permit a call whose head
/// matches one of the [`CommandIdentity`](crate::runtime::command::CommandIdentity)'s
/// policy keys?  Pre-args judgment that classification and the `which`
/// inspector consult to short-circuit a denied head with a focused error.
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

/// Check `editor.read` capability is available.
pub(crate) fn check_editor_read(ctx: &Context, subcmd: &str) -> Settled<()> {
    check_grant_bool(
        ctx,
        || format!("denied: _ed-{subcmd} requires editor.read"),
        |caps| caps.editor.as_ref().map(|e| e.read),
    )
}

/// Check `editor.write` capability is available.
pub(crate) fn check_editor_write(ctx: &Context, subcmd: &str) -> Settled<()> {
    check_grant_bool(
        ctx,
        || format!("denied: _ed-{subcmd} requires editor.write"),
        |caps| caps.editor.as_ref().map(|e| e.write),
    )
}

/// Check `editor.tui` capability is available.
pub(crate) fn check_editor_tui(ctx: &Context) -> Settled<()> {
    check_grant_bool(
        ctx,
        || "denied: _ed-tui requires editor.tui".into(),
        |caps| caps.editor.as_ref().map(|e| e.tui),
    )
}

/// Check `shell.chdir` capability is available.
pub(crate) fn check_shell_chdir(ctx: &Context) -> Settled<()> {
    check_grant_bool(
        ctx,
        || "denied: cd requires shell.chdir".into(),
        |caps| caps.shell.as_ref().map(|s| s.chdir),
    )
}

/// Walk the capabilities stack; if any layer with a relevant policy
/// votes `false`, return a denial error.  `test` returns
/// `Some(allowed)` when the layer has an opinion, `None` to abstain.
///
/// Shared by the editor/shell bool gates; lives here because it's the
/// reduction step those checks share.
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
