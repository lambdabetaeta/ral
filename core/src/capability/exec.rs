//! Per-layer and stack-level exec policy evaluation.
//!
//! Two internal types encode the per-layer and whole-stack verdicts;
//! `evaluate_exec` folds the stack, `layer_exec_verdict` decides one layer.

use crate::types::{Capabilities, Dynamic, ExecPolicy};
use crate::path;
use std::path::Path;

/// One capability layer's vote on a candidate command.
pub(super) enum LayerExec {
    /// Layer has no exec/exec_dirs opinion.
    NoOpinion,
    /// Layer has exec restrictions and the command matches none.
    Denied,
    /// Layer admits the command with this policy.
    Allowed(ExecPolicy),
}

/// Folded verdict across the whole capability stack.
pub(super) enum ExecVerdict {
    /// No layer has any exec opinion.
    Unrestricted,
    /// At least one layer denies; the call is rejected.
    Denied,
    /// Every opining layer allowed; effective policy is the
    /// intersection of those layers' allowed policies.
    Allowed(ExecPolicy),
}

/// Walk the stack and combine per-layer verdicts.
///
/// Any layer that denies → command denied.  Allowed opinions
/// intersect.  If the stack declared exec policy but no layer admitted
/// the command, deny; only a stack with no exec policy at all is
/// unrestricted.
pub(super) fn evaluate_exec(dynamic: &Dynamic, names: &[&str]) -> ExecVerdict {
    let mut policy: Option<ExecPolicy> = None;
    let mut any_opinion = false;
    let mut saw_exec_policy = false;
    for ctx in &dynamic.capabilities_stack {
        saw_exec_policy |= ctx.exec.is_some() || ctx.exec_dirs.is_some();
        match layer_exec_verdict(ctx, names) {
            LayerExec::NoOpinion => {}
            LayerExec::Denied => return ExecVerdict::Denied,
            LayerExec::Allowed(p) => {
                any_opinion = true;
                policy = Some(match policy.take() {
                    None => p,
                    Some(prev) => intersect_exec_policy(prev, p),
                });
            }
        }
    }
    if any_opinion {
        ExecVerdict::Allowed(policy.unwrap_or(ExecPolicy::Allow))
    } else if saw_exec_policy {
        ExecVerdict::Denied
    } else {
        ExecVerdict::Unrestricted
    }
}

/// Decide a single layer's verdict on a command.
///
/// Two routes match a layer: (a) name in the layer's `exec` map,
/// (b) resolved absolute path under one of the layer's `exec_dirs`.
/// Name match wins if both are present (takes the named policy).
///
/// `None` on either field is "no opinion" for that route.  A layer
/// that declared only one route and missed it abstains, letting another
/// layer admit the command by a different route.
pub(super) fn layer_exec_verdict(ctx: &Capabilities, names: &[&str]) -> LayerExec {
    let exec_set = ctx.exec.is_some();
    let dirs_set = ctx.exec_dirs.is_some();
    if !exec_set && !dirs_set {
        return LayerExec::NoOpinion;
    }
    if let Some(exec) = &ctx.exec {
        let mut matched = names.iter().filter_map(|n| exec.get(*n));
        if let Some(first) = matched.next() {
            let policy = matched.fold(first.clone(), |acc, p| intersect_exec_policy(acc, p.clone()));
            // Name match wins over `exec_dirs`: an explicit `Deny`
            // here vetos even when a directory route would admit
            // the resolved path.
            if matches!(policy, ExecPolicy::Deny) {
                return LayerExec::Denied;
            }
            return LayerExec::Allowed(policy);
        }
    }
    if let Some(dirs) = &ctx.exec_dirs
        && dirs.iter().any(|d| {
            names.iter().any(|n| {
                let p = Path::new(n);
                p.is_absolute() && path::path_within(p, Path::new(d))
            })
        })
    {
        return LayerExec::Allowed(ExecPolicy::Allow);
    }
    // Both routes declared, neither matched → strict deny.
    // Otherwise abstain so an outer layer's opinion can decide.
    if exec_set && dirs_set {
        LayerExec::Denied
    } else {
        LayerExec::NoOpinion
    }
}

/// Meet two exec policies: the intersection of their granted authority.
pub(super) fn intersect_exec_policy(outer: ExecPolicy, inner: ExecPolicy) -> ExecPolicy {
    match (outer, inner) {
        (ExecPolicy::Deny, _) | (_, ExecPolicy::Deny) => ExecPolicy::Deny,
        (ExecPolicy::Allow, inner) => inner,
        (outer, ExecPolicy::Allow) => outer,
        (ExecPolicy::Subcommands(a), ExecPolicy::Subcommands(b)) => {
            ExecPolicy::Subcommands(a.into_iter().filter(|s| b.contains(s)).collect())
        }
    }
}
