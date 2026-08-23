//! The audit collector: a flat trail of [`Observation`]s.
//!
//! `within`, `grant`, `guard`, `try`, and `audit` are collection boundaries,
//! not observations themselves — none of them owns or wraps one; the real
//! commands, writes, reads, and capability checks their bodies produce land
//! flat in whichever trail is open.  A sandboxed subprocess or a
//! pipeline-stage helper only *transports* its fragment back to the parent
//! process; nothing decides where an observation "belongs" beyond that flat
//! merge.

use super::observation::Observation;
use super::value::Value;
use crate::source::Span;
use serde::{Deserialize, Serialize};

/// Cap on one command observation's recorded `stderr`; `evaluator::audit`
/// truncates to it.
pub const STDERR_CAP_BYTES: usize = 64 * 1024;

/// Bytes captured for one command under `CapturePolicy::Bytes`, empty
/// otherwise.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuditIo {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Whether per-command bytes are teed into audit observations.  `Off` lets fd
/// 1 and fd 2 stream live, unbuffered; `Bytes` installs the tee that
/// `evaluator::capture` wraps each command in.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapturePolicy {
    #[default]
    Off,
    Bytes,
}

/// Backing storage for the in-flight trail; reachable only through [`Audit`].
#[derive(Default, Debug)]
struct AuditTrail {
    observations: Vec<Observation>,
}

/// Observations detached from a trail — a sandboxed child or a pipeline
/// helper hands some up across a process boundary, and the receiving side
/// merges them into the surrounding trail.
///
/// Same shape as [`AuditTrail`], but in transit.
#[derive(Default, Debug, Clone)]
pub struct AuditFragment {
    observations: Vec<Observation>,
}

impl AuditFragment {
    pub fn empty() -> Self {
        Self::default()
    }
    pub fn from_observations(observations: Vec<Observation>) -> Self {
        Self { observations }
    }
    pub fn into_observations(self) -> Vec<Observation> {
        self.observations
    }
    pub fn is_empty(&self) -> bool {
        self.observations.is_empty()
    }
}

/// A claim on the trail returned by [`Audit::open`] and consumed by
/// [`Audit::close`]. Not `Clone`, not `Copy`: exactly one close ends the
/// scope it opened.
pub struct TrailScope {
    opened: bool,
    mark: usize,
}

/// Audit collector — one per `Shell`, collecting exactly while `trail` is
/// `Some`.
#[derive(Default, Debug)]
pub struct Audit {
    trail: Option<AuditTrail>,
    capture: CapturePolicy,
    /// Where the command now running was dispatched from — the register every
    /// observation resolves its site against, `None` before a run's first
    /// dispatch.  `run_call` in `runtime::command_call` skips the write for
    /// `_`-prefixed names, so a prelude wrapper's observations name the
    /// user's call rather than the wrapper's; `IoLoan` in `crate::run` clears
    /// the register per run and restores it on drop.
    pub(crate) call_site: Option<Span>,
}

impl Audit {
    /// True when a scope is collecting.
    pub fn active(&self) -> bool {
        self.trail.is_some()
    }

    /// True when the tee should record each command's bytes.
    pub fn captures_bytes(&self) -> bool {
        matches!(self.capture, CapturePolicy::Bytes)
    }

    /// Overwrite the capture policy.  A scope wants `delimited` in
    /// [`crate::evaluator::audit`], whose merge is monotonic: an inner `try`
    /// must not silence an outer `audit`.
    pub fn set_capture(&mut self, policy: CapturePolicy) {
        self.capture = policy;
    }

    /// The current capture policy.
    pub fn capture_policy(&self) -> CapturePolicy {
        self.capture
    }

    /// The policy to inherit across a process boundary, `Some` iff a scope is
    /// collecting — a helper learns in one answer whether to open a trail and
    /// which policy to install.  Rides in `ChildEvalRequest`'s own
    /// `audit_policy` field: it instructs the child, it is not snapshot state.
    pub fn active_policy(&self) -> Option<CapturePolicy> {
        self.active().then_some(self.capture)
    }

    /// Inverse of [`Self::active_policy`]: open a trail and set the policy on
    /// `Some`, stay inactive on `None`.  An already-open trail keeps its
    /// observations.
    pub fn install_active_policy(&mut self, policy: Option<CapturePolicy>) {
        if let Some(policy) = policy {
            self.trail.get_or_insert_default();
            self.capture = policy;
        }
    }

    /// Append an observation; no-op when inactive, so the emission door need
    /// not ask.
    pub fn push(&mut self, obs: Observation) {
        if let Some(trail) = self.trail.as_mut() {
            trail.observations.push(obs);
        }
    }

    /// Open a delimited scope on the trail: install one if none is open, or
    /// mark the open one's current length. `opened` records which happened,
    /// so the matching [`Self::close`] knows whether it owns the trail or is
    /// only reading a suffix of an outer scope's.
    pub fn open(&mut self) -> TrailScope {
        let opened = self.trail.is_none();
        let mark = self.trail.get_or_insert_default().observations.len();
        TrailScope { opened, mark }
    }

    /// End a scope: the opener drains the trail to empty and closes it, for
    /// every exit — the caller is responsible for reaching this on a panic
    /// too. A nested scope copies its suffix and leaves the trail open,
    /// intact, for its own opener to close later.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "a scope is a claim, spent exactly once: taking it by value is the discipline"
    )]
    pub fn close(&mut self, scope: TrailScope) -> Vec<Observation> {
        let TrailScope { opened, mark } = scope;
        if opened {
            self.trail.take().map_or_else(Vec::new, |t| t.observations)
        } else {
            self.trail
                .as_ref()
                .map_or_else(Vec::new, |t| t.observations[mark..].to_vec())
        }
    }

    /// Drain the trail, leaving it open but empty — how a sandbox or pipeline
    /// child ships its audit home.  Empty fragment when inactive.
    pub fn take_fragment(&mut self) -> AuditFragment {
        match self.trail.as_mut() {
            Some(trail) => {
                AuditFragment::from_observations(std::mem::take(&mut trail.observations))
            }
            None => AuditFragment::empty(),
        }
    }

    /// STT-in for a same-thread thunk body.  The trail and policy move in and
    /// the call site is copied in, but it never flows back on
    /// [`Self::return_to`]: the asymmetry keeps a body's own dispatches from
    /// leaking their site into the caller's next one.
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.trail = parent.trail.take();
        self.capture = parent.capture;
        self.call_site = parent.call_site;
    }

    /// STT-out: the trail the body extended, and the policy, go back to the
    /// parent.  The call site does not.
    pub fn return_to(&mut self, parent: &mut Self) {
        parent.trail = self.trail.take();
        parent.capture = self.capture;
    }
}

/// Microseconds since the Unix epoch.
pub fn epoch_us() -> i64 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "microseconds-since-epoch stays below i64::MAX until year 294276"
    )]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i64
    }
}

/// The record `audit { … }` returns: the body's own outcome plus the flat
/// list of observations its dynamic extent produced.
///
/// This is not an observation itself — `audit` runs no command and owns no
/// site of its own, only the outcome of what it forced into being recorded.
/// Mirrored in the typechecker by `audit_record` in
/// `core/src/typecheck/builtins.rs`.
pub fn tree_value(
    status: i32,
    value: Value,
    error: Option<String>,
    children: &[Observation],
) -> Value {
    let children_list: Vec<Value> = children.iter().map(Observation::to_value).collect();
    Value::map(vec![
        ("status".into(), Value::Int(i64::from(status))),
        ("value".into(), value),
        ("error".into(), Value::String(error.unwrap_or_default())),
        ("children".into(), Value::list(children_list)),
    ])
}

#[cfg(test)]
mod tests {
    use super::super::observation::Observed;
    use super::*;
    use crate::diagnostic::CallSite;

    fn dummy(pattern: &str) -> Observation {
        Observation::instant(
            CallSite::default(),
            None,
            Observed::Grep {
                scope: String::new(),
                pattern: pattern.into(),
            },
        )
    }

    fn pattern_of(obs: &Observation) -> &str {
        match &obs.what {
            Observed::Grep { pattern, .. } => pattern,
            _ => unreachable!(),
        }
    }

    /// An opener's close drains the trail to `None` — the next scope starts
    /// from a clean slate rather than inheriting a stale `Some`.
    #[test]
    fn opener_close_drains_and_closes() {
        let mut audit = Audit::default();
        let scope = audit.open();
        audit.push(dummy("a"));
        audit.push(dummy("b"));
        let drained = audit.close(scope);
        assert_eq!(
            drained.iter().map(pattern_of).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(
            !audit.active(),
            "the opener's close must leave no trail open"
        );
    }

    /// A scope opened onto an already-open trail reads only its own suffix
    /// and leaves the trail open — the flat merge law: an outer scope still
    /// sees everything a nested one pushed.
    #[test]
    fn nested_close_reads_suffix_and_leaves_trail_open() {
        let mut audit = Audit::default();
        let outer = audit.open();
        audit.push(dummy("outer-1"));

        let inner = audit.open();
        audit.push(dummy("inner-1"));
        let inner_children = audit.close(inner);
        assert_eq!(
            inner_children.iter().map(pattern_of).collect::<Vec<_>>(),
            ["inner-1"]
        );
        assert!(
            audit.active(),
            "a nested close must not close the trail its opener still owns"
        );

        audit.push(dummy("outer-2"));
        let outer_children = audit.close(outer);
        assert_eq!(
            outer_children.iter().map(pattern_of).collect::<Vec<_>>(),
            ["outer-1", "inner-1", "outer-2"],
            "the outer scope sees the inner scope's entries too — the flat merge"
        );
        assert!(!audit.active());
    }
}
