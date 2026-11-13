//! [`Mooring`]: the frame a run fixes once and never changes — its
//! structured-event sink, its enquiry desk, and its terminal-foreground
//! authority.

use super::Error;
use super::flow::Settled;
use crate::fact::{Reason, StateFailure};
use crate::ir::ValClos;
use crate::process::{DEFAULT_CLEANUP_GRACE, DurableRoot, ScopeNode, TerminalAccess};
use std::sync::Arc;
use std::time::Duration;

/// A sink for structured host events: the value-typed dual of the byte
/// [`Io`](crate::io::Io) sinks.
///
/// `Send + Sync` so a same-thread thunk body may share it with the rest of
/// the child subtree; a deferred worker never gets the live sink, only a
/// bounded buffering one that delivers at completion.
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: &crate::data::FOValue);
}

/// The no-op surface, for a caller with no rail to speak into; an absent
/// sink (`None`) behaves identically.
impl EventSink for () {
    fn emit(&self, _ev: &crate::data::FOValue) {}
}

/// Shared handle to the run-local structured-event sink.
///
/// Installed only by a run ([`Shell::run`](crate::Shell::run)), never a
/// persistent `Shell` capability, and holding no liveness role — a clone
/// cannot decide a run is over.
pub type SurfaceSink = Arc<dyn EventSink>;

/// The refusal a host with no desk answers, spelled once.
///
/// Both transports, the engine, and every front-end reach for this rather than
/// their own copy: a run must not be able to tell which host refused it by the
/// wording.
pub const NO_DESK: &str = "this host answers no enquiries";

/// The hint that refusal carries, spelled once alongside it: host-independent,
/// since the wording must not reveal which host refused.
pub const NO_DESK_HINT: &str =
    "only a foreground run has a desk: call it directly, not in `spawn` or a pipeline stage";

/// The refusal a host with no desk answers, as an error.
pub fn no_desk() -> Error {
    Error::new(Reason::State(Box::new(StateFailure::NoDesk)))
}

/// The host's answer desk for the engine→host answered channel.  `enquire` is
/// blocking and short by contract: a receipt, a ledger read, a verdict — never
/// a long-running result.
pub trait EnquiryDesk: Send + Sync {
    /// Answer one enquiry synchronously.
    ///
    /// `scope` is the enquiring run's current scope, so a desk that parks polls
    /// it and unwinds on the same causes every other poll point observes.
    ///
    /// # Errors
    /// Returns `Err` when the host cannot answer `req`, or the stop `scope`
    /// carries.
    fn enquire(
        &self,
        req: crate::data::FOValue,
        scope: &crate::process::Scope,
    ) -> Settled<crate::data::FOValue>;
}

/// Shared handle to the run-local enquiry desk.  Run-scoped like
/// [`SurfaceSink`]; `None` outside a host that answers enquiries.
pub type Desk = std::sync::Arc<dyn EnquiryDesk>;

/// Host-installed destination for deferred workers' surface.
///
/// Session-lived, unlike [`SurfaceSink`], and cloned into spawned workers. It
/// receives a settled worker's batch once and a `watch` worker's lines one at
/// a time; where absent, a worker's surface is dropped.
pub trait DeferredSink: Send + Sync {
    /// Deliver surfaced values: a settled worker's whole batch, or one watched line.
    fn deliver(&self, batch: Vec<crate::data::FOValue>);
}

/// Whether a `spawn` must name its work: a host's legibility policy, refused
/// at the spawn step before any birth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TitlePolicy {
    Optional,
    Required,
}

/// The run-invariant half of the frame a run installs: where its events go,
/// who answers it, what stops it, and its terminal-foreground authority.
///
/// The node is here, being run-invariant; the current scope is not (a
/// cleanup's shield), so it is [`Shell::scope`](crate::shell::runtime::Shell).
///
/// Nothing here changes between a run's first step and its last, so nothing
/// needs saving and restoring: it is an owned local on the run's own stack
/// frame ([`Shell::run`](crate::Shell::run)), lent onward as `&Mooring`, and an
/// outer run's is restored by the unwinding rather than by a guard.
/// Deliberately *not* a [`Shell`](crate::Shell) field — `&shell.mooring` would borrow the
/// whole shell while every callee wants `&mut Shell`, whereas `&Mooring` and
/// `&mut Shell` are disjoint.  A clone carries the authority the original has;
/// [`Self::lend_terminal`] is the one place it is raised, never minted.
#[derive(Clone)]
pub struct Mooring {
    /// `None` outside a host, which makes [`Self::surface`] the identity.
    pub(crate) surface: Option<SurfaceSink>,
    pub(crate) deferred: Option<Arc<dyn DeferredSink>>,
    /// Never given to a deferred worker: a worker outlives its run's Report, so
    /// one that could enquire would break the ordering law.
    pub(crate) desk: Option<Desk>,
    /// The owner's node: the base of this machine's tree (a run's, a
    /// worker's, a stage's).  What a key the terminal tenant heard is struck
    /// on, what a stage's group membership is judged by, what a host
    /// cancels a worker through.  Not what is polled: that is `Shell::scope`,
    /// a descendant swapped by frames, which coincides with this at the base.
    pub home: ScopeNode,
    /// What a `guard` cleanup gets, from the strike, before the stop ends
    /// it: the run's [`Run::cleanup_grace`](crate::protocol::Run::cleanup_grace),
    /// [`DEFAULT_CLEANUP_GRACE`] when adrift; a child thread inherits it.
    pub(crate) cleanup_grace: Duration,
    /// Admission cap enforced at the spawn, counting only *running*
    /// entries — settled ones lingering under retention never block a birth.
    pub(crate) worker_cap: Option<usize>,
    pub(crate) worker_title: TitlePolicy,
    pub(crate) terminal_access: TerminalAccess,
}

impl Mooring {
    /// The mooring a child thread (a spawned worker or a pipeline stage) runs
    /// under: a rebuild, not a share.
    ///
    /// `node` is the child's own scope node; `surface` is selected explicitly
    /// by the caller (a session worker's buffered surface, or the parent's live
    /// surface for a gather child or pipeline stage).  The session's deferred
    /// sink, cleanup grace, worker cap and title policy carry over, while desk
    /// and terminal authority are denied.
    pub(crate) fn for_thread(parent: &Self, node: ScopeNode, surface: Option<SurfaceSink>) -> Self {
        Self {
            surface,
            deferred: parent.deferred.clone(),
            desk: None,
            home: node,
            cleanup_grace: parent.cleanup_grace,
            worker_cap: parent.worker_cap,
            worker_title: parent.worker_title,
            terminal_access: TerminalAccess::Denied,
        }
    }

    /// A mooring tied to no run: nothing installed, `home` a node under a
    /// root nothing holds, so nothing can cancel it, and the default cleanup
    /// grace.  For a builtin body invoked outside a run, a host-embedding
    /// test above all.
    pub fn adrift() -> Self {
        Self {
            surface: None,
            deferred: None,
            desk: None,
            home: DurableRoot::default().child(),
            cleanup_grace: DEFAULT_CLEANUP_GRACE,
            worker_cap: None,
            worker_title: TitlePolicy::Optional,
            terminal_access: TerminalAccess::Denied,
        }
    }

    /// Whether this run has an enquiry desk.
    pub fn has_desk(&self) -> bool {
        self.desk.is_some()
    }

    /// Whether anything is on the other end — what lets a caller skip building
    /// an observation nobody would receive.
    pub(crate) fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    /// The sink entry for a runtime [`ValClos`], such as a host builtin with no
    /// vocabulary of its own hands in.  Encoded here into the first-order
    /// [`FOValue`](crate::data::FOValue) the sink carries; inert with no
    /// sink installed.  Total: a `Handle` or a closure reachable through `ev`
    /// crosses as its `opaque` placeholder rather than being dropped.
    pub fn surface(&self, ev: &ValClos) {
        if let Some(sink) = self.surface.as_ref() {
            sink.emit(&crate::data::FOValue::scrubbed(ev.into()));
        }
    }

    /// [`Self::surface`] for a host that already holds data.  Every entry core
    /// owns reaches it through `Shell::observe_stamped` with
    /// [`crate::shell::runtime::Observation::to_surface`].
    pub fn surface_data(&self, ev: &crate::data::FOValue) {
        if let Some(sink) = self.surface.as_ref() {
            sink.emit(ev);
        }
    }

    /// The same mooring with `terminal_access` raised to `ExplicitLoan`, for
    /// the call that borrows it (`_ed-tui`'s `fzf`, which must own the
    /// foreground pgid to draw on `/dev/tty` even though stdout is captured).
    /// `Some` only from `Leased`: a `Denied` run has no terminal to lend and an
    /// `ExplicitLoan` one has already lent it.  The parent is untouched.
    pub fn lend_terminal(&self) -> Option<Self> {
        matches!(self.terminal_access, TerminalAccess::Leased).then(|| Self {
            terminal_access: TerminalAccess::ExplicitLoan,
            ..self.clone()
        })
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::data::{FOValue, OPAQUE_TAG};
    use crate::handle::idle_handle;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingSink(Mutex<Vec<FOValue>>);

    impl EventSink for RecordingSink {
        fn emit(&self, ev: &FOValue) {
            self.0.lock().unwrap().push(ev.clone());
        }
    }

    fn mooring_with(recorder: Arc<RecordingSink>) -> Mooring {
        let mut mooring = Mooring::adrift();
        mooring.surface = Some(recorder);
        mooring
    }

    /// A value whose `Handle` reaches the sink as its tagged placeholder
    /// rather than the whole emission being dropped.
    #[test]
    fn surface_scrubs_a_handle_instead_of_dropping() {
        let recorder = Arc::new(RecordingSink::default());
        let mooring = mooring_with(recorder.clone());

        mooring.surface(&ValClos::map(vec![("value".to_string(), idle_handle())]));

        let emitted = recorder.0.lock().unwrap().clone();
        assert_eq!(emitted.len(), 1, "the emission must not be dropped");
        let FOValue::Map { entries } = &emitted[0] else {
            panic!("a map surfaces as a map")
        };
        let (_, value) = entries
            .iter()
            .find(|(k, _)| k == "value")
            .expect("the value key survives");
        assert_eq!(
            *value,
            FOValue::Variant {
                label: OPAQUE_TAG.to_string(),
                payload: Some(Box::new(FOValue::Map {
                    entries: vec![(
                        "type".to_string(),
                        FOValue::String {
                            value: "handle".to_string()
                        }
                    )]
                }))
            }
        );
    }
}
