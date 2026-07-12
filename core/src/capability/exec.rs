//! Per-layer and stack-level exec policy evaluation.
//!
//! Two internal types encode the per-layer and whole-stack verdicts;
//! `evaluate_exec` folds the stack, `layer_exec_verdict` decides one
//! layer.  Within a layer the unified exec map admits commands two
//! ways: by literal key match (bare name or absolute path), or by
//! subpath-prefix match (a key ending in `/` covering anything under
//! it).  Literal beats subpath; deeper subpath beats shallower.

use crate::path;
use crate::types::{Context, ExecDir, ExecMap, ExecPolicy, Meet};
use std::collections::{BTreeMap, BTreeSet};

/// What an admitted command may run: any arguments, or only a fixed
/// set of first-argument subcommands.
pub(super) enum Admit {
    Any,
    Subcommands(BTreeSet<String>),
}

/// `Admit` is [`ExecPolicy`] with the `Deny` bottom removed: a `Deny`
/// can never reach an admitted verdict, so only the two authority
/// points that survive admission remain.  Meet delegates to
/// [`ExecPolicy::meet`] over those points, never producing `Deny`.
impl Meet for Admit {
    fn meet(self, other: Self) -> Self {
        Self::from_policy(self.into_policy().meet(other.into_policy()))
    }
}

impl Admit {
    fn into_policy(self) -> ExecPolicy {
        match self {
            Self::Any => ExecPolicy::Allow,
            Self::Subcommands(s) => ExecPolicy::Subcommands(s),
        }
    }

    fn from_policy(policy: ExecPolicy) -> Self {
        match policy {
            ExecPolicy::Allow => Self::Any,
            ExecPolicy::Subcommands(s) => Self::Subcommands(s),
            ExecPolicy::Deny => unreachable!("meet of two admitted verdicts is never Deny"),
        }
    }
}

/// One opining capability layer's vote on a candidate command.
pub(super) enum LayerExec {
    /// Layer has exec restrictions and the command matches none.
    Denied,
    /// Layer admits the command with this allowance.
    Allowed(Admit),
}

/// The two identity sets a command carries into the exec gate.
///
/// A command has two identities — the bare name and the resolved
/// absolute path — and the basename of either form.  These widen the
/// VETO surface but must not widen the ADMISSION surface: a planted
/// `/tmp/evil/rg` invoked by absolute path must not inherit the bare
/// `rg` admission of an outer grant.  So the gate carries both sets and
/// consults them asymmetrically — deny-broad, allow-narrow.
///
/// * `deny` — broad: `policy_names` ∪ basenames of the resolved and
///   as-invoked forms.  Any hit on a literal `Deny`, or any absolute
///   `deny` name landing in a `Deny` dir, vetoes.
/// * `allow` — narrow: exactly `policy_names`.  Only a hit here on a
///   literal `Allow`/`Subcommands`, or an absolute `allow` name landing
///   in an `Allow` dir, admits.
#[derive(Clone, Copy)]
pub(super) struct ExecNames<'a> {
    pub(super) deny: &'a [&'a str],
    pub(super) allow: &'a [&'a str],
}

/// Folded verdict across the whole capability stack.
pub(super) enum ExecVerdict {
    /// No layer has any exec opinion.
    Unrestricted,
    /// At least one layer denies; the call is rejected.
    Denied,
    /// Every opining layer allowed; effective allowance is the
    /// intersection of those layers' allowances.
    Allowed(Admit),
}

/// Walk the stack and combine per-layer verdicts.
///
/// Any layer that denies → command denied.  Allowed opinions
/// intersect.  If the stack declared exec policy but no layer admitted
/// the command, deny; only a stack with no exec policy at all is
/// unrestricted.
pub(super) fn evaluate_exec(ctx: &Context, names: ExecNames) -> ExecVerdict {
    let mut admit: Option<Admit> = None;
    let mut saw = false;
    for exec in ctx.grants.exec() {
        saw = true;
        match layer_exec_verdict(exec, names) {
            LayerExec::Denied => return ExecVerdict::Denied,
            LayerExec::Allowed(a) => admit = admit.meet(Some(a)),
        }
    }
    if let Some(a) = admit {
        ExecVerdict::Allowed(a)
    } else if saw {
        ExecVerdict::Denied
    } else {
        ExecVerdict::Unrestricted
    }
}

/// Decide a single layer's verdict on a command.
///
/// The exec map admits or denies commands two ways: a literal key
/// match (bare name or absolute path) and a directory-prefix match.
/// The two identity sets are consulted asymmetrically — deny-broad,
/// allow-narrow (see [`ExecNames`]) — so a basename can close a veto
/// hole without widening admission.  Match order:
///
///   1. A literal `Deny` on any BROAD name is the strongest veto: it
///      fires even when a covering directory would admit the path.
///   2. A literal `Allow`/`Subcommands` on the NARROW names wins next.
///      Multiple narrow literal hits are meet-folded, so a bare
///      `git: Allow` paired with `/usr/bin/git: Deny` still yields
///      `Deny` (the deny side already caught it in step 1; the fold
///      confirms it).
///   3. Otherwise the deepest matching directory prefix wins.  A dir
///      `Deny` propagates as `LayerExec::Denied`; a dir `Allow` yields
///      `LayerExec::Allowed(Allow)`.  Deeper prefix beats shallower, so
///      `/usr/bin/sudo: Deny` excludes a hole inside `/usr/bin: Allow`.
///   4. Neither form fires: strict deny — the deny-by-default that
///      every opining layer carries.
///
/// Dirs match only absolute names; the basenames the broad set adds are
/// bare (no slash) and never absolute, so dir matching sees the same
/// candidates from `allow` and `deny` and needs only the narrow set.
pub(super) fn layer_exec_verdict(exec: &ExecMap, names: ExecNames) -> LayerExec {
    if literal_vetoes(&exec.literals, names.deny) {
        return LayerExec::Denied;
    }
    if let Some(policy) = match_literal_keys(&exec.literals, names.allow) {
        return match policy {
            ExecPolicy::Deny => LayerExec::Denied,
            ExecPolicy::Allow => LayerExec::Allowed(Admit::Any),
            ExecPolicy::Subcommands(s) => LayerExec::Allowed(Admit::Subcommands(s)),
        };
    }
    match longest_dir_match(exec, names.allow) {
        Some(ExecDir::Allow) => LayerExec::Allowed(Admit::Any),
        Some(ExecDir::Deny) | None => LayerExec::Denied,
    }
}

/// True iff any broad identity hits a literal `Deny`.  A literal `Deny`
/// is the strongest veto and beats a covering allow dir, so this is
/// consulted before any admission path.
fn literal_vetoes(literals: &BTreeMap<String, ExecPolicy>, deny_names: &[&str]) -> bool {
    deny_names
        .iter()
        .any(|n| matches!(literals.get(*n), Some(ExecPolicy::Deny)))
}

/// Run the stack-level exec verdict over an explicit deny/allow name
/// pair, returning whether the command is admitted (not denied).  Lets
/// a test feed the narrow set as both sets — reproducing the pre-fix
/// gate, which had no broad veto identity — against the fixed gate.
#[cfg(all(test, unix))]
pub(crate) fn admits_for_test(ctx: &Context, deny: &[&str], allow: &[&str]) -> bool {
    !matches!(
        evaluate_exec(ctx, ExecNames { deny, allow }),
        ExecVerdict::Denied
    )
}

/// Look up every candidate name as a literal key (bare names and
/// absolute paths both live in the same keyspace).  Multiple hits are
/// meet-folded so a layer that lists the same binary under both a
/// bare name and its resolved path takes the intersection of the two
/// policies.
fn match_literal_keys(
    literals: &BTreeMap<String, ExecPolicy>,
    names: &[&str],
) -> Option<ExecPolicy> {
    let mut matched = names.iter().filter_map(|n| literals.get(*n).cloned());
    let first = matched.next()?;
    Some(matched.fold(first, ExecPolicy::meet))
}

/// Find the deepest directory prefix that covers any absolute
/// candidate and return its verdict.  "Deepest" by character count of
/// the key, which is monotone with prefix depth for canonical absolute
/// paths.  Returns `None` if no directory matches.
fn longest_dir_match(exec: &ExecMap, names: &[&str]) -> Option<ExecDir> {
    let mut best: Option<(usize, ExecDir)> = None;
    for (dir, verdict) in &exec.dirs {
        let matches_any = names
            .iter()
            .any(|n| path::is_absolute(n) && path::path_within_str(n, dir));
        if !matches_any {
            continue;
        }
        let len = dir.len();
        match &best {
            Some((best_len, _)) if *best_len >= len => {}
            _ => best = Some((len, verdict.clone())),
        }
    }
    best.map(|(_, p)| p)
}
