//! Per-layer and stack-level exec policy evaluation.
//!
//! Two internal types encode the per-layer and whole-stack verdicts;
//! `evaluate_exec` folds the stack, `layer_exec_verdict` decides one
//! layer.  Within a layer the unified exec map admits commands two
//! ways: by literal key match (bare name or absolute path), or by
//! subpath-prefix match (a key ending in `/` covering anything under
//! it).  Literal beats subpath; deeper subpath beats shallower.

use crate::types::{is_subpath_key, Capabilities, Dynamic, ExecPolicy, Meet};
use crate::path;
use std::collections::BTreeMap;
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
        saw_exec_policy |= ctx.exec.is_some();
        match layer_exec_verdict(ctx, names) {
            LayerExec::NoOpinion => {}
            LayerExec::Denied => return ExecVerdict::Denied,
            LayerExec::Allowed(p) => {
                any_opinion = true;
                policy = Some(match policy.take() {
                    None => p,
                    Some(prev) => prev.meet(p),
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
/// The unified exec map admits or denies commands two ways: literal
/// key match (bare name or absolute path) and subpath-prefix match
/// (key ending in `/`).  Match order:
///
///   1. Literal hit wins.  An explicit `Deny` here vetoes even when a
///      sibling subpath would otherwise admit the path.  Multiple
///      candidate-name hits are meet-folded.
///   2. Otherwise the longest matching subpath key wins.  A subpath
///      `Deny` propagates as `LayerExec::Denied`; a subpath `Allow`
///      yields `LayerExec::Allowed(Allow)`.  Deeper prefix beats
///      shallower, so `/usr/bin/sudo/: Deny` excludes a hole inside
///      `/usr/bin/: Allow`.
///   3. Neither form fires: strict deny — the deny-by-default that
///      every opining layer carries.
///
/// A layer with no `exec` field at all has no opinion.
pub(super) fn layer_exec_verdict(ctx: &Capabilities, names: &[&str]) -> LayerExec {
    let Some(exec) = &ctx.exec else {
        return LayerExec::NoOpinion;
    };
    if let Some(policy) = match_literal_keys(exec, names) {
        if matches!(policy, ExecPolicy::Deny) {
            return LayerExec::Denied;
        }
        return LayerExec::Allowed(policy);
    }
    match longest_subpath_match(exec, names) {
        Some(ExecPolicy::Deny) => LayerExec::Denied,
        Some(ExecPolicy::Allow) => LayerExec::Allowed(ExecPolicy::Allow),
        // Subcommands is rejected on subpath keys at validation time;
        // ignore here defensively rather than panicking.
        Some(ExecPolicy::Subcommands(_)) | None => LayerExec::Denied,
    }
}

/// Look up every candidate name as a literal key (bare names and
/// absolute paths both live in the same keyspace).  Multiple hits are
/// meet-folded so a layer that lists the same binary under both a
/// bare name and its resolved path takes the intersection of the two
/// policies.  Subpath-style keys (trailing `/`) never literal-match
/// here — they're the path-prefix half of the keyspace and are
/// handled by `subpath_admits`.
fn match_literal_keys(
    exec: &BTreeMap<String, ExecPolicy>,
    names: &[&str],
) -> Option<ExecPolicy> {
    let mut matched = names.iter().filter_map(|n| exec.get(*n).cloned());
    let first = matched.next()?;
    Some(matched.fold(first, ExecPolicy::meet))
}

/// Find the longest subpath key that covers any absolute candidate
/// and return its policy.  "Longest" by character count of the key,
/// which is monotone with prefix depth for canonical absolute paths.
/// Returns `None` if no subpath matches.
fn longest_subpath_match(
    exec: &BTreeMap<String, ExecPolicy>,
    names: &[&str],
) -> Option<ExecPolicy> {
    let mut best: Option<(usize, ExecPolicy)> = None;
    for (key, policy) in exec {
        if !is_subpath_key(key) {
            continue;
        }
        let prefix = Path::new(key);
        let matches_any = names.iter().any(|n| {
            let p = Path::new(n);
            p.is_absolute() && path::path_within(p, prefix)
        });
        if !matches_any {
            continue;
        }
        let len = key.len();
        match &best {
            Some((best_len, _)) if *best_len >= len => {}
            _ => best = Some((len, policy.clone())),
        }
    }
    best.map(|(_, p)| p)
}

