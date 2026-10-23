//! [`Mooring`]: the frame a run fixes once and never changes — its
//! structured-event sink, its enquiry desk, its session-[`Fork`] handle, and
//! its terminal-foreground authority.

use super::flow::Settled;
use super::shell::Bequest;
use super::value::Value;
use crate::process::{DurableRoot, ScopeNode};
use crate::sync::LockExt as _;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A sink for structured host events: the value-typed dual of the byte
/// [`Io`](crate::io::Io) sinks.
///
/// `Send + Sync` so a same-thread thunk body may share it with the rest of
/// the child subtree; a deferred worker never gets the live sink, only a
/// bounded buffering one that delivers at completion.
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: &crate::first_order::FOValue);
}

/// The no-op surface, for a caller with no rail to speak into; an absent
/// sink (`None`) behaves identically.
impl EventSink for () {
    fn emit(&self, _ev: &crate::first_order::FOValue) {}
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

/// The status that refusal carries. Not a failure of the request — the host
/// simply has no desk — but a raise all the same, since there is no answer.
pub const NO_DESK_STATUS: i32 = 1;

/// The hint that refusal carries, spelled once alongside it: host-independent,
/// since the wording must not reveal which host refused.
pub const NO_DESK_HINT: &str =
    "only a foreground run has a desk: call it directly, not in `spawn` or a pipeline stage";

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
        req: crate::first_order::FOValue,
        scope: &crate::process::Scope,
    ) -> Settled<crate::first_order::FOValue>;
}

/// Shared handle to the run-local enquiry desk.  Run-scoped like
/// [`SurfaceSink`]; `None` outside a host that answers enquiries.
pub type Desk = std::sync::Arc<dyn EnquiryDesk>;

/// Identifier for a shell session parked in a [`Nursery`], redeemed once by
/// [`IdentityTransport::adopt_parked`](crate::carrier::IdentityTransport::adopt_parked).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NurseryId(pub u64);

/// The parked bequests plus the counter [`Nursery::park`] mints ids from: a
/// length-derived id would repeat once `adopt`/`clear` free a slot.
#[derive(Default)]
struct NurseryState {
    next: u64,
    parked: HashMap<u64, Bequest>,
}

/// Run-local holding pen for engine-side session forks, reached only through
/// [`Fork::Park`] — an in-process desk and nothing else.
///
/// That arm exists for one reason: the reentrancy law bars a same-process desk
/// handler from holding `&mut Shell`, so it cannot fork a session itself. The
/// builtin body bequeaths through
/// [`Shell::bequeath`](crate::types::Shell::bequeath) and the bequest reaches
/// the handler as a [`NurseryId`] its transport adopts.
/// [`NurseryGuard`] empties it, so an unadopted bequest dies with its run.
#[derive(Clone, Default)]
pub struct Nursery(Arc<Mutex<NurseryState>>);

impl Nursery {
    /// Park `bequest` under a freshly minted id.
    pub fn park(&self, bequest: Bequest) -> NurseryId {
        let id = {
            let mut state = self.0.lock_ignore_poison();
            let id = state.next;
            state.next += 1;
            state.parked.insert(id, bequest);
            id
        };
        NurseryId(id)
    }

    /// Remove and return the bequest parked under `id`, if one is still there.
    pub(crate) fn adopt(&self, id: NurseryId) -> Option<Bequest> {
        self.0.lock_ignore_poison().parked.remove(&id.0)
    }

    /// Drop every still-parked bequest.
    pub fn clear(&self) {
        self.0.lock_ignore_poison().parked.clear();
    }
}

/// How a forked session reaches the desk that will adopt it.
#[derive(Clone)]
pub enum Fork {
    /// In-process: the builtin parks a bequest, the handler adopts it by id.
    Park(Nursery),
    /// Across a wire: the builtin seals its bequest into its enquiry; the host
    /// that answers makes the child engine and hands it the seal in its
    /// `Attach`.
    Seal,
}

/// Host-installed destination for deferred workers' surface.
///
/// Session-lived, unlike [`SurfaceSink`], and cloned into spawned workers. It
/// receives a settled worker's batch once and a `watch` worker's lines one at
/// a time; where absent, a worker's surface is dropped.
pub trait DeferredSink: Send + Sync {
    /// Deliver surfaced values: a settled worker's whole batch, or one watched line.
    fn deliver(&self, batch: Vec<crate::first_order::FOValue>);
}

/// This run's authority to hand the controlling terminal to a child: the
/// internal form of
/// [`RequestedTerminalAccess`](crate::process::RequestedTerminalAccess), carrying
/// the extra `ExplicitLoan` no host can request at a run.  Read by
/// [`Shell::terminal_lease`](crate::types::Shell::terminal_lease); `Denied` is
/// the safe [`Default`], so an unstated policy cannot reach the handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum TerminalAccess {
    #[default]
    Denied,
    Leased,
    /// A within-run elevation of `Leased`: the handoff fires even though stdout
    /// is a buffer, because the body (`_ed-tui`'s `fzf`) draws on `/dev/tty`
    /// and must own the foreground pgid.  Raised only by
    /// [`Mooring::lend_terminal`].
    ExplicitLoan,
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
/// cleanup's shield), so it is [`Shell::scope`](crate::types::Shell).
///
/// Nothing here changes between a run's first step and its last, so nothing
/// needs saving and restoring: it is an owned local on the run's own stack
/// frame ([`Shell::run`](crate::Shell::run)), lent onward as `&Mooring`, and an
/// outer run's is restored by the unwinding rather than by a guard.
/// Deliberately *not* a [`Shell`](crate::Shell) field — `&shell.mooring` would borrow the
/// whole shell while every callee wants `&mut Shell`, whereas `&Mooring` and
/// `&mut Shell` are disjoint.  Not [`Clone`]: [`Self::lend_terminal`] is the
/// one lawful derivation, so raise-never-mint lives in a single place.
pub struct Mooring {
    /// `None` outside a host, which makes [`Self::surface`] the identity.
    pub(crate) surface: Option<SurfaceSink>,
    pub(crate) deferred: Option<Arc<dyn DeferredSink>>,
    /// Never given to a deferred worker: a worker outlives its run's Report, so
    /// one that could enquire would break the ordering law.
    pub(crate) desk: Option<Desk>,
    /// `None`: this host adopts no forked sessions.
    pub(crate) fork: Option<Fork>,
    /// The owner's node: the base of this machine's tree (a run's, a
    /// worker's, a stage's).  What a key the terminal tenant heard is struck
    /// on, what a stage's group membership is judged by, what a host
    /// cancels a worker through.  Not what is polled: that is `Shell::scope`,
    /// a descendant swapped by frames, which coincides with this at the base.
    pub home: ScopeNode,
    /// What a `guard` cleanup gets, from the strike, before the stop ends
    /// it: the host's policy, as `wall` is.
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
    /// sink, cleanup grace, worker cap and title policy carry over, while desk,
    /// fork and terminal authority are denied.
    pub(crate) fn for_thread(parent: &Self, node: ScopeNode, surface: Option<SurfaceSink>) -> Self {
        Self {
            surface,
            deferred: parent.deferred.clone(),
            desk: None,
            fork: None,
            home: node,
            cleanup_grace: parent.cleanup_grace,
            worker_cap: parent.worker_cap,
            worker_title: parent.worker_title,
            terminal_access: TerminalAccess::Denied,
        }
    }

    /// A mooring tied to no run: nothing installed, and a node under a root
    /// nothing else holds.  For a builtin body invoked outside a run — a
    /// host-embedding test, above all — which a host stops through
    /// [`Shell::cancel_handle`](crate::Shell::cancel_handle).
    pub fn adrift() -> Self {
        Self {
            surface: None,
            deferred: None,
            desk: None,
            fork: None,
            home: DurableRoot::default().worker(),
            cleanup_grace: Duration::from_secs(5),
            worker_cap: None,
            worker_title: TitlePolicy::Optional,
            terminal_access: TerminalAccess::Denied,
        }
    }

    /// How a forked session reaches this run's desk, if it has one.
    pub fn fork(&self) -> Option<&Fork> {
        self.fork.as_ref()
    }

    /// Whether anything is on the other end — what lets a caller skip building
    /// an observation nobody would receive.
    pub(crate) fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    /// The sink entry for a runtime [`Value`], such as a host builtin with no
    /// vocabulary of its own hands in.  Encoded here into the first-order
    /// [`FOValue`](crate::first_order::FOValue) the sink carries; inert with no
    /// sink installed.  Total: a `Handle` or a closure reachable through `ev`
    /// crosses as its `opaque` placeholder rather than being dropped.
    pub fn surface(&self, ev: &Value) {
        if let Some(sink) = self.surface.as_ref() {
            sink.emit(&crate::first_order::FOValue::scrubbed(ev));
        }
    }

    /// [`Self::surface`] for a host that already holds data.  Every entry core
    /// owns reaches it through `Shell::observe_stamped` with
    /// [`crate::types::Observation::to_surface`].
    pub fn surface_data(&self, ev: &crate::first_order::FOValue) {
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
            surface: self.surface.clone(),
            deferred: self.deferred.clone(),
            desk: self.desk.clone(),
            fork: self.fork.clone(),
            home: self.home.clone(),
            cleanup_grace: self.cleanup_grace,
            worker_cap: self.worker_cap,
            worker_title: self.worker_title,
            terminal_access: TerminalAccess::ExplicitLoan,
        })
    }
}

/// Empties the run's nursery on `Drop`, so a fork never adopted — a refused
/// enquiry, a run that ended before its handler ran — cannot outlive the run
/// that parked it.  Holds an `Arc`-shared clone of the run's nursery, so it
/// clears the same state every callee sees, and lives inside the run's
/// `catch_unwind`, so it fires on the panic path too.
pub(crate) struct NurseryGuard(pub(crate) Option<Fork>);

impl Drop for NurseryGuard {
    fn drop(&mut self) {
        if let Some(Fork::Park(nursery)) = self.0.as_ref() {
            nursery.clear();
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::first_order::{FOValue, OPAQUE_TAG};
    use crate::types::idle_handle;
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

        mooring.surface(&Value::map(vec![("value".to_string(), idle_handle())]));

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

    /// A first-order value crosses unchanged: totality does not tax the
    /// common case.
    #[test]
    fn surface_leaves_a_first_order_value_untouched() {
        let recorder = Arc::new(RecordingSink::default());
        let mooring = mooring_with(recorder.clone());

        mooring.surface(&Value::Int(7));

        let emitted = recorder.0.lock().unwrap().clone();
        assert_eq!(emitted.as_slice(), [FOValue::Int { value: 7 }]);
    }
}
