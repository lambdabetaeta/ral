//! The in-process guards.
//!
//! Every runtime yes/no asked as an action is attempted — exec and its
//! argv, an fs read or write, the editor and shell flags, head admission
//! — folds the whole dynamic [`GrantStack`], so a verdict is authority
//! intersected across every layer, never one frame.  Only the exec and fs
//! guards reach a real OS resource, and only they audit.  The sibling
//! [`super::sandbox`] projects the same authority onto the OS sandbox, and
//! does so off the same per-dimension folds this module tests against —
//! [`super::fs::allow_region`] and the table [`check_exec`] hands over in
//! its [`Admitted`] — so the two cannot drift.

use super::exec::{ExecRules, Program, rules};
use super::fs::{FsOp, allow_region, deny_region};
use crate::path::Resolver;
use crate::runtime::command::Head;
use crate::types::{
    Audit, CallSite, Capabilities, Context, Decision, GrantStack, Observation, Observed, Settled,
    Verdict, sig, sig_hint,
};
use std::collections::BTreeMap;

/// A program the in-process guard admitted with these arguments, and the table that
/// admitted it.  Only [`check_exec`] makes one, so nothing launches unjudged.
#[must_use]
pub(crate) struct Admitted {
    program: Program,
    args: Vec<String>,
    rules: Option<ExecRules>,
}

impl Admitted {
    pub(crate) fn program(&self) -> &Program {
        &self.program
    }

    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }

    /// The stack's exec table, `None` where no layer restricts exec.
    pub(crate) fn rules(&self) -> Option<&ExecRules> {
        self.rules.as_ref()
    }
}

/// Judge a program and its argv against the stack's exec rules.  A refusal
/// is recorded on an open trail, as the fs guard's is.
pub(crate) fn check_exec(
    ctx: &Context,
    shown: &str,
    program: Program,
    args: Vec<String>,
    audit: &mut Audit,
    site: Option<CallSite>,
) -> Settled<Admitted> {
    let rules = rules(&ctx.grants);
    let verdict = rules
        .as_ref()
        .map_or(Verdict::Allow, |r| r.verdict(program.subject()));
    let result: Settled<()> = match verdict {
        Verdict::Allow => Ok(()),
        Verdict::Deny => Err(sig_hint(
            format!("command '{shown}' denied by active grant"),
            "add the command to the grant exec map \
             (or its directory, keyed with a trailing '/') to allow it",
        )),
        Verdict::Only(allowed) => {
            let hint = || {
                format!(
                    "allowed subcommands (matched against the command's first argument): {}",
                    allowed.iter().cloned().collect::<Vec<_>>().join(", ")
                )
            };
            match args.first() {
                Some(first) if allowed.contains(first) => Ok(()),
                Some(first) => Err(sig_hint(
                    format!("command '{shown}' subcommand '{first}' denied by active grant"),
                    hint(),
                )),
                None => Err(sig_hint(
                    format!(
                        "command '{shown}' requires an allowed subcommand \
                         under the active grant"
                    ),
                    hint(),
                )),
            }
        }
    };

    if result.is_err() {
        emit_capability_denial(ctx, "exec", audit, site, |f| {
            f.insert("name".into(), shown.into());
            f.insert("resolved".into(), program.to_string());
            f.insert("args".into(), args.join(" "));
        });
    }

    result.map(|()| Admitted {
        program,
        args,
        rules,
    })
}

/// The decision half of [`check_fs_op`]: does the stack admit `op` on the
/// resolved path?  `Unrestricted` exactly when no layer held an `fs`
/// opinion.  `Guarded` is the one verdict no grant decides: a write onto a
/// binary the sandbox pinned at boot, refused before the stack is consulted
/// — the twin of the discard device, which is admitted before it is.
pub(super) enum FsVerdict {
    Unrestricted,
    Granted,
    Denied,
    Guarded(&'static str),
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
    // Before the stack: an empty stack is exactly the case it must catch.
    if matches!(op, FsOp::Write)
        && let Some(pinned) = crate::sandbox::pinned_binary(resolved)
    {
        return FsVerdict::Guarded(pinned);
    }
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
    /// The fs guard's verdict as a plain bool, for callers with no [`Context`]
    /// to audit through — exarch's boot-time skill discovery asks it of a
    /// one-frame [`GrantStack::of`].  The same [`fs_verdict`] the guard runs,
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
        self.admits_fs_exact(op, resolver, &path.canonicalise_lenient())
    }

    /// [`admits_fs`](Self::admits_fs) for a path already symlink-free — what
    /// [`crate::path::Located::real`] hands over — so no second walk gets to
    /// disagree with the one that located the object.
    pub fn admits_fs_exact(&self, op: &FsOp, resolver: &Resolver, real: &std::path::Path) -> bool {
        matches!(
            fs_verdict(self, resolver, real, op),
            FsVerdict::Unrestricted | FsVerdict::Granted
        )
    }
}

/// Decide an `op` on one resolved path, audit it, and mint the `Break` on
/// denial.  The path is canonicalised leniently, so this is for the *reads
/// by name* — predicates, listings, module loading — where nothing is
/// written through the name.  A write goes through `Shell::locate`, which
/// judges the located object with [`check_fs_exact`].
/// A [discard device](crate::path::ResolvedPath::is_discard) is exempt from
/// both regions — asked before canonicalisation, since the question is about
/// the name, not about what is on the disk under it.
pub(crate) fn check_fs_op(
    ctx: &Context,
    path: &crate::path::ResolvedPath,
    op: &FsOp,
    audit: &mut Audit,
    site: Option<CallSite>,
) -> Settled<()> {
    if path.is_discard() {
        return Ok(());
    }
    check_fs_exact(ctx, &path.canonicalise_lenient(), op, audit, site)
}

/// Decide an `op` on a symlink-free path: [`fs_verdict`] is the decision,
/// this the reporting around it.
pub(crate) fn check_fs_exact(
    ctx: &Context,
    resolved: &std::path::Path,
    op: &FsOp,
    audit: &mut Audit,
    site: Option<CallSite>,
) -> Settled<()> {
    let verdict = fs_verdict(&ctx.grants, &ctx.resolver(), resolved, op);

    if let FsVerdict::Denied | FsVerdict::Guarded(_) = verdict {
        emit_capability_denial(ctx, "fs", audit, site, |f| {
            f.insert("op".into(), op.label().into());
            f.insert("path".into(), resolved.display().to_string());
            if let FsVerdict::Guarded(pinned) = verdict {
                f.insert("pinned".into(), pinned.into());
            }
        });
    }

    match verdict {
        FsVerdict::Denied => Err(sig(format!(
            "fs {} denied by grant: {}",
            op.label(),
            resolved.display()
        ))),
        FsVerdict::Guarded(pinned) => Err(sig_hint(
            format!(
                "fs write refused: {} is the {pinned} binary ral pinned at startup, which \
                 confines every command run under a grant",
                resolved.display()
            ),
            "ral never writes into the binary that enforces its grants, whatever the \
             grant allows. Install a new one by replacing the file rather than \
             rewriting it: a rename leaves this session's pinned copy intact",
        )),
        FsVerdict::Unrestricted | FsVerdict::Granted => Ok(()),
    }
}

/// Head-only admission, before any argv is known: classification and the
/// `which` inspector consult it to refuse a denied head with a focused
/// error rather than let the call reach [`check_exec`].  A head with no
/// program passes: `vet` reports the missing command.
pub(crate) fn admits_head(ctx: &Context, head: &Head) -> bool {
    let Ok(program) = &head.program else {
        return true;
    };
    rules(&ctx.grants).is_none_or(|r| !r.verdict(program.subject()).is_denied())
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
/// through. Its callers ([`check_exec`], [`check_fs_op`]) are reached
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
    site: Option<CallSite>,
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
/// forms on both sides, so a symlink-spelled grant covers its target, in
/// both directions.
#[cfg(unix)]
#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs scaffolding: tempdir trees and symlinks for containment pins"
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

    /// The threat is a stack with no `fs` opinion at all — the one shape
    /// the fold answers `Unrestricted` before consulting anything — so the
    /// guard is tested on exactly that stack, and by inode: a hard link is
    /// the same file under another name.
    #[test]
    fn a_write_onto_a_pinned_binary_is_guarded_before_any_grant_is_consulted() {
        use super::{FsVerdict, fs_verdict};
        crate::sandbox::early_init(&crate::Invocation::Shell);
        let open = GrantStack::of(Capabilities::default());
        let resolver = Resolver::shell_less();
        let own = std::env::current_exe().expect("own path");
        let verdict = |path: &std::path::Path, op: &FsOp| fs_verdict(&open, &resolver, path, op);

        assert!(
            matches!(verdict(&own, &FsOp::Write), FsVerdict::Guarded("ral")),
            "a write onto the pinned executable must be guarded under an open stack"
        );
        assert!(
            matches!(verdict(&own, &FsOp::Read), FsVerdict::Unrestricted),
            "the guard is over rewriting, not reading"
        );
        let tmp = tempfile::tempdir().unwrap();
        let other = tmp.path().join("other");
        std::fs::write(&other, "x").unwrap();
        assert!(
            matches!(verdict(&other, &FsOp::Write), FsVerdict::Unrestricted),
            "an unpinned file is the stack's to decide"
        );
        let link = tmp.path().join("link-to-own");
        if std::fs::hard_link(&own, &link).is_ok() {
            assert!(
                matches!(verdict(&link, &FsOp::Write), FsVerdict::Guarded("ral")),
                "a hard link names the pinned inode too"
            );
        }
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

    /// Default APFS answers `SECRET` with `secret`: the walk must spell it as
    /// stored, or the deny on `secret` never meets the access.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_case_variant_spelling_meets_the_deny() {
        use crate::path::walk::{Leaf, walk};
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let secret = root.join("secret");
        std::fs::create_dir(&secret).unwrap();
        std::fs::write(secret.join("key"), "x").unwrap();
        if !root.join("SECRET").exists() {
            return;
        }
        let grants = stack(FsPolicy {
            read_prefixes: vec![NormalizedPrefix::from_surface(&root)],
            deny_paths: vec![NormalizedPrefix::from_surface(&secret)],
            ..FsPolicy::default()
        });
        let resolver = Resolver::shell_less();
        let rp = resolver.resolve(&root.join("SECRET/key").to_string_lossy());
        let located = walk(&rp, Leaf::Resolve).unwrap();
        assert_eq!(located.real(), secret.join("key"));
        assert!(!admits_read(&grants, located.real()));
    }
}
