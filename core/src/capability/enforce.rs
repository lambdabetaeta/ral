//! Point-of-use capability gates.
//!
//! Every runtime yes/no asked as an action is attempted — exec and its
//! argv, an fs read or write, the editor and shell flags, head admission
//! — folds the whole dynamic [`GrantStack`], so a verdict is authority
//! intersected across every layer, never one frame.  Only the exec and fs
//! gates reach a real OS resource, and only they audit.  The sibling
//! [`super::sandbox`] projects the same authority onto the OS sandbox, and
//! does so off the same per-dimension folds this module tests against —
//! [`super::fs::allow_region`] and [`super::exec::evaluate_exec`] — so the
//! two cannot drift.

use super::exec::{Admit, ExecNames, ExecVerdict, evaluate_exec};
use super::fs::{FsOp, allow_region, deny_region};
use crate::path::Resolver;
use crate::types::{
    Audit, CallSite, Capabilities, Context, Decision, GrantStack, Observation, Observed, Settled,
    sig, sig_hint,
};
use std::collections::BTreeMap;

/// Gate a command and its argv against the stack's exec opinions.  A refusal
/// is recorded on an open trail, as the fs gate's is.
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

    if result.is_err() {
        emit_capability_denial(ctx, "exec", audit, site, |f| {
            f.insert("name".into(), display_name.into());
            if let Some(resolved_name) = policy_names
                .iter()
                .find(|candidate| **candidate != display_name)
            {
                f.insert("resolved".into(), (*resolved_name).into());
            }
            f.insert("args".into(), args.join(" "));
        });
    }

    result
}

/// The pure half of [`check_fs_op`]'s decision: does the stack admit `op`
/// on the resolved path?  `Unrestricted` exactly when no layer held an `fs`
/// opinion.
pub(super) enum FsVerdict {
    Unrestricted,
    Granted,
    Denied,
}

/// Test the resolved path against the regions [`super::fs`] folds: it passes
/// when the path lies inside `op`'s allow region and outside the deny
/// region.  Membership is region containment, alias-aware, via
/// [`PrefixSet::covering`](crate::path::PrefixSet::covering).
///
/// The fold runs against a live [`Resolver`] on every check, so the regions
/// this decides against are the ones the disk describes now — where the
/// sibling [`super::sandbox`] folds once, at spawn, because that is when the
/// OS profile is written.  That freshness is the whole difference between
/// the two consumers of the fold.
pub(super) fn fs_verdict(
    grants: &GrantStack,
    resolver: &Resolver,
    resolved: &std::path::Path,
    op: &FsOp,
) -> FsVerdict {
    let Some(allowed) = allow_region(grants, resolver, op) else {
        return FsVerdict::Unrestricted;
    };
    if deny_region(grants, resolver).covering(resolved).is_some() {
        return FsVerdict::Denied;
    }
    match allowed.covering(resolved) {
        Some(_) => FsVerdict::Granted,
        None => FsVerdict::Denied,
    }
}

impl GrantStack {
    /// The fs gate's verdict as a plain bool, for callers with no [`Context`]
    /// to audit through — exarch's boot-time skill discovery asks it of a
    /// one-frame [`GrantStack::of`].  The same [`fs_verdict`] the gate runs,
    /// canonicalising leniently inside as [`check_fs_op`] does, so there is no
    /// surface-form spelling of the question.  The
    /// [`ResolvedPath::is_discard`](crate::path::ResolvedPath::is_discard)
    /// exemption is [`check_fs_op`]'s alone: it excuses a discard device from
    /// an *access*, and this door decides membership, not access.
    pub fn admits_fs(
        &self,
        op: &FsOp,
        resolver: &Resolver,
        path: &crate::path::ResolvedPath,
    ) -> bool {
        !matches!(
            fs_verdict(self, resolver, &path.canonicalise_lenient(), op),
            FsVerdict::Denied
        )
    }
}

/// Decide an `op` on one resolved path, audit it, and mint the `Break` on
/// denial: [`fs_verdict`] is the decision, this the reporting around it.
/// A [discard device](crate::path::ResolvedPath::is_discard) is exempt from
/// both regions — asked before canonicalisation, since the question is about
/// the name, not about what is on the disk under it.
pub(crate) fn check_fs_op(
    ctx: &Context,
    path: &crate::path::ResolvedPath,
    op: &FsOp,
    audit: &mut Audit,
    site: CallSite,
) -> Settled<()> {
    if path.is_discard() {
        return Ok(());
    }
    let resolved = path.canonicalise_lenient();
    let verdict = fs_verdict(&ctx.grants, &ctx.resolver(), &resolved, op);

    if matches!(verdict, FsVerdict::Denied) {
        emit_capability_denial(ctx, "fs", audit, site, |f| {
            f.insert("op".into(), op.label().into());
            f.insert("path".into(), path.display().to_string());
        });
    }

    match verdict {
        FsVerdict::Denied => Err(sig(format!(
            "fs {} denied by grant: {}",
            op.label(),
            resolved.display()
        ))),
        FsVerdict::Unrestricted | FsVerdict::Granted => Ok(()),
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

/// Record a refused capability check on an open trail.  An admitted one is
/// never recorded: a trail of every permitted read says nothing an auditor
/// asked, and the refusals are the whole story.
///
/// The trail is this door's whole audience: it has no `&Mooring` to surface
/// through. Its callers ([`check_exec_args`], [`check_fs_op`]) are reached
/// from `types/shell/checks.rs`, which fans out through
/// `builtins/{fs,modules,util}.rs`, `runtime/command/{vet,redirect}.rs`, and
/// exarch's own doors, none of which carry one. So a denial here reaches the
/// trail alone — unlike a head admission, which broadcasts. A refused
/// external command still surfaces in its own right, as the failed command
/// observation its dispatch builds.
fn emit_capability_denial(
    context: &Context,
    resource: &str,
    audit: &mut Audit,
    site: CallSite,
    fill: impl FnOnce(&mut BTreeMap<String, String>),
) {
    if !audit.active() {
        return;
    }
    let mut fields = BTreeMap::new();
    fill(&mut fields);
    let obs = Observation::instant(
        site,
        context.principal(),
        Observed::Capability {
            resource: resource.to_string(),
            decision: Decision::Denied,
            fields,
        },
    );
    audit.push(obs);
}

/// Pins for [`GrantStack::admits_fs`]: containment is judged on resolved
/// forms on both sides, so a symlink-spelled grant covers its target — the
/// divergence the retired hand-rolled skill matcher had, in both directions.
#[cfg(unix)]
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs scaffolding: tempdir trees and symlinks for containment pins"
)]
mod tests {
    use super::FsOp;
    use crate::path::{NormalizedPrefix, Resolver};
    use crate::types::{Capabilities, FsPolicy, GrantStack};

    fn stack(fs: FsPolicy) -> GrantStack {
        GrantStack::of(Capabilities {
            fs: Some(fs),
            ..Capabilities::default()
        })
    }

    fn admits_read(grants: &GrantStack, path: &std::path::Path) -> bool {
        let resolver = Resolver::shell_less();
        let rp = resolver.resolve(&path.to_string_lossy());
        grants.admits_fs(&FsOp::Read, &resolver, &rp)
    }

    #[test]
    fn a_symlink_spelled_read_prefix_admits_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("SKILL.md"), "x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let grants = stack(FsPolicy {
            read_prefixes: vec![NormalizedPrefix::from_surface(&link)],
            ..FsPolicy::default()
        });
        assert!(
            admits_read(&grants, &real.join("SKILL.md")),
            "a prefix granted through a symlink must cover the resolved target"
        );
        assert!(
            !admits_read(&grants, &tmp.path().join("outside")),
            "the region is still the prefix, not the world"
        );
    }

    #[test]
    fn a_symlink_spelled_deny_covers_its_target() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let secret = real.join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("SKILL.md"), "x").unwrap();
        let link = tmp.path().join("link-secret");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let grants = stack(FsPolicy {
            read_prefixes: vec![NormalizedPrefix::from_surface(&real)],
            deny_paths: vec![NormalizedPrefix::from_surface(&link)],
            ..FsPolicy::default()
        });
        assert!(
            !admits_read(&grants, &secret.join("SKILL.md")),
            "a deny spelled through a symlink must cover the resolved target"
        );
        assert!(
            admits_read(&grants, &real.join("SKILL.md")),
            "the deny is the entry, not the whole read region"
        );
    }
}
