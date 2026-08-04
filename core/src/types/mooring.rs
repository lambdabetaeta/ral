//! [`Mooring`]: the frame a run fixes once and never changes — its
//! structured-event sink, its enquiry desk, its session-fork [`Nursery`], and
//! its terminal-foreground authority.

use super::shell::Shell;
use super::shell::workers::WorkerLease;
use super::value::Value;
use crate::process::{DurableRoot, ForegroundScope};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A sink for structured host events: the value-typed dual of the byte
/// [`Io`](crate::io::Io) sinks.
///
/// `Send + Sync` so a same-thread thunk body may share it with the rest of
/// the child subtree; a deferred worker never gets the live sink, only a
/// bounded buffering one it replays on `await`.
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: &crate::serial::FOValue);
}

/// The no-op surface, for a caller with no rail to speak into
/// ([`Mooring::for_stage`]); an absent sink (`None`) behaves identically.
impl EventSink for () {
    fn emit(&self, _ev: &crate::serial::FOValue) {}
}

/// Shared handle to the run-local structured-event sink.
///
/// Installed only by a run door ([`Shell::run`](crate::Shell::run)), never a
/// persistent `Shell` capability, and holding no liveness role — a clone
/// cannot decide a run is over.
pub type SurfaceSink = Arc<dyn EventSink>;

/// The refusal a host with no desk answers, spelled once.
///
/// Both transports, the engine, and every front-end reach for this rather than
/// their own copy: a run must not be able to tell which seam refused it by the
/// wording.
pub const NO_DESK: &str = "this host answers no enquiries";

/// The status that refusal carries. Not a failure of the request — the host
/// simply has no desk — but a raise all the same, since there is no answer.
pub const NO_DESK_STATUS: i32 = 1;

/// The host's answer desk for the engine→host answered channel.  `enquire` is
/// blocking and short by contract: a receipt, a ledger read, a verdict — never
/// a long-running result.
pub trait EnquiryDesk: Send + Sync {
    /// Answer one enquiry synchronously.
    ///
    /// `cancel` is the enquiring run's own scope, so a desk that parks polls it
    /// and unwinds on the same causes every other poll point observes.
    ///
    /// # Errors
    /// Returns `Err` when the host cannot answer `req`.
    fn enquire(
        &self,
        req: crate::serial::FOValue,
        cancel: &crate::process::CancelScope,
    ) -> Result<crate::serial::FOValue, crate::types::Error>;
}

/// Shared handle to the run-local enquiry desk.  Run-scoped like
/// [`SurfaceSink`]; `None` outside a host that answers enquiries.
pub type Desk = std::sync::Arc<dyn EnquiryDesk>;

/// Identifier for a shell session parked in a [`Nursery`], redeemed once by
/// [`Nursery::adopt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NurseryId(pub u64);

/// The parked forks plus the counter [`Nursery::park`] mints ids from: a
/// length-derived id would repeat once `adopt`/`clear` free a slot.
#[derive(Default)]
struct NurseryState {
    next: u64,
    parked: HashMap<u64, Shell>,
}

/// Run-local holding pen for engine-side session forks.
///
/// The reentrancy law bars a desk handler from holding `&mut Shell`, so it
/// cannot fork a session itself: the builtin body forks through
/// [`Shell::fork_into_nursery`](crate::types::Shell::fork_into_nursery) and the
/// fork reaches the handler as a [`NurseryId`] to [`adopt`](Self::adopt).
/// [`NurseryGuard`] empties it, so an unadopted fork dies with its run.
#[derive(Clone, Default)]
pub struct Nursery(Arc<Mutex<NurseryState>>);

impl Nursery {
    /// Park `shell` under a freshly minted id.
    ///
    /// # Panics
    /// Panics if the lock is poisoned.
    pub fn park(&self, shell: Shell) -> NurseryId {
        let id = {
            let mut state = self.0.lock().unwrap();
            let id = state.next;
            state.next += 1;
            state.parked.insert(id, shell);
            id
        };
        NurseryId(id)
    }

    /// Remove and return the shell parked under `id`, if one is still there.
    ///
    /// # Panics
    /// Panics if the lock is poisoned.
    pub fn adopt(&self, id: NurseryId) -> Option<Shell> {
        self.0.lock().unwrap().parked.remove(&id.0)
    }

    /// Drop every still-parked shell.
    ///
    /// # Panics
    /// Panics if the lock is poisoned.
    pub fn clear(&self) {
        self.0.lock().unwrap().parked.clear();
    }
}

/// Host-installed destination for a deferred worker's surface batch, delivered
/// when the worker settles.
///
/// Session-lived, unlike [`SurfaceSink`], and cloned into spawned workers so a
/// nested `spawn` delivers at its own completion. `None` outside an agent
/// host, where the batch reaches a sink only through `await`/`race`.
pub trait DeferredSink: Send + Sync {
    /// Deliver a settled worker's surfaced values as one batch.
    ///
    /// Deliver-once is the *caller's* discipline: the completion path wins the
    /// test-and-set on the `joined` latch it shares with the eliminators before
    /// invoking this, so an implementation cannot forget the latch.
    fn deliver(&self, batch: Vec<crate::serial::FOValue>);
}

/// This run's authority to hand the controlling terminal to a child: the
/// internal form of
/// [`RequestedTerminalAccess`](crate::run::RequestedTerminalAccess), carrying
/// the extra `ExplicitLoan` no host can request at a run door.  Read by
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

/// The run-invariant half of the frame a run installs: where its events go,
/// who answers it, what stops it, and its terminal-foreground authority.
///
/// Nothing here changes between a run's first step and its last, so nothing
/// needs saving and restoring: it is an owned local on the run's own stack
/// frame ([`Shell::run`](crate::Shell::run)), lent onward as `&Mooring`, and an
/// outer run's is restored by the unwinding rather than by a guard.
/// Deliberately *not* a [`Shell`] field — `&shell.mooring` would borrow the
/// whole shell while every callee wants `&mut Shell`, whereas `&Mooring` and
/// `&mut Shell` are disjoint.  Not [`Clone`]: [`Self::lend_terminal`] is the
/// one lawful derivation, so raise-never-mint lives in a single door.
pub struct Mooring {
    /// `None` outside a host, which makes [`Self::surface`] the identity.
    pub(crate) surface: Option<SurfaceSink>,
    pub(crate) deferred: Option<Arc<dyn DeferredSink>>,
    /// Never given to a deferred worker: a worker outlives its run's Report, so
    /// one that could enquire would break the ordering law.
    pub(crate) desk: Option<Desk>,
    pub(crate) nursery: Option<Nursery>,
    /// The run's foreground work scope, consulted between effectful steps by
    /// [`signal::check`](crate::process::signal::check) and always a descendant
    /// of [`SessionState`](crate::types::SessionState)'s durable root.  The one
    /// `pub` member: a host clones a deadline child of it, or cancels it to
    /// interrupt the foreground work.
    pub cancel: ForegroundScope,
    pub(crate) deferred_lease: Option<WorkerLease>,
    /// Admission cap enforced at the spawn door, counting only *running*
    /// entries — settled ones lingering under retention never block a birth.
    pub(crate) worker_cap: Option<usize>,
    pub(crate) terminal_access: TerminalAccess,
}

impl Mooring {
    /// The mooring a detached worker runs under: a rebuild, not a share.
    ///
    /// `parent`'s cancel scope would let a foreground cancel reach a worker
    /// that must outlive the run, and desk and nursery are barred to one; rail,
    /// lease, and cap carry over, so a nested `spawn` is governed alike.  Its
    /// scope is a [`worker`](DurableRoot::worker) of the session root: a
    /// SIGTERM reaches it, a Ctrl-C does not.
    pub(crate) fn for_worker(parent: &Self, root: &DurableRoot, surface: SurfaceSink) -> Self {
        Self {
            surface: Some(surface),
            deferred: parent.deferred.clone(),
            desk: None,
            nursery: None,
            cancel: root.worker(),
            deferred_lease: parent.deferred_lease,
            worker_cap: parent.worker_cap,
            terminal_access: TerminalAccess::Denied,
        }
    }

    /// The mooring a cross-process pipeline stage runs under, in the helper
    /// process (`child_eval`).  Nothing crosses the process boundary, so there
    /// is no parent to rebuild from: no enquiries, no forks, no rail, no
    /// terminal authority, and the `()` sink, which discards whatever the stage
    /// body surfaces.
    pub(crate) fn for_stage(root: &DurableRoot, surface: SurfaceSink) -> Self {
        Self {
            surface: Some(surface),
            deferred: None,
            desk: None,
            nursery: None,
            cancel: root.worker(),
            deferred_lease: None,
            worker_cap: None,
            terminal_access: TerminalAccess::Denied,
        }
    }

    /// A mooring tied to no run: nothing installed, and a scope under a root
    /// nothing else holds.  What a caller invoking a builtin body outside a run
    /// reaches for — a host-embedding test, above all; cancelling its
    /// [`cancel`](Self::cancel) is how it drives the body's poll points.
    pub fn adrift() -> Self {
        Self {
            surface: None,
            deferred: None,
            desk: None,
            nursery: None,
            cancel: DurableRoot::default().worker(),
            deferred_lease: None,
            worker_cap: None,
            terminal_access: TerminalAccess::Denied,
        }
    }

    /// The one sink door.  Every door core owns reaches it through
    /// `evaluator::audit::observe_stamped`, which builds the [`Value`] from
    /// [`crate::types::Observation::to_value`] — the single map shape shared
    /// by the rail, the audit trail, and `--audit`; a host builtin with no
    /// vocabulary of its own may still hand in a fully-formed [`Value`]
    /// directly.  Encoded here into the first-order
    /// [`FOValue`](crate::serial::FOValue) the sink carries; inert with no
    /// sink installed.  A closure or a handle cannot cross the rail, and core
    /// has no host vocabulary to report that in, so the drop is a
    /// `dbg_trace`.
    /// Whether anything is on the other end — what lets a door skip building
    /// an observation nobody would receive.
    pub(crate) fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    pub fn surface(&self, ev: &Value) {
        match crate::serial::FOValue::try_from(ev) {
            Ok(fo) => {
                if let Some(sink) = self.surface.as_ref() {
                    sink.emit(&fo);
                }
            }
            Err(_) => {
                crate::dbg_trace!("surface", "dropping non-first-order surface value");
            }
        }
    }

    /// Derive a mooring identical to this one with its terminal authority
    /// raised: `Leased => ExplicitLoan`, everything else unchanged.
    /// Raise-never-mint — a `Denied` mooring lends a `Denied` one.  The parent
    /// is untouched and the derivation dies with the call that borrowed it
    /// (`_ed-tui`'s `fzf`, which must own the foreground pgid to draw on
    /// `/dev/tty` even though stdout is captured).
    pub fn lend_terminal(&self) -> Self {
        Self {
            surface: self.surface.clone(),
            deferred: self.deferred.clone(),
            desk: self.desk.clone(),
            nursery: self.nursery.clone(),
            cancel: self.cancel.clone(),
            deferred_lease: self.deferred_lease,
            worker_cap: self.worker_cap,
            terminal_access: match self.terminal_access {
                TerminalAccess::Leased => TerminalAccess::ExplicitLoan,
                other => other,
            },
        }
    }

    /// The re-entrancy guard `_ed-tui` refuses on, a nested loan being a logic
    /// error.
    pub fn in_terminal_loan(&self) -> bool {
        matches!(self.terminal_access, TerminalAccess::ExplicitLoan)
    }
}

/// Empties the run's nursery on `Drop`, so a fork never adopted — a refused
/// enquiry, a run that ended before its handler ran — cannot outlive the run
/// that parked it.  Holds an `Arc`-shared clone of the run's nursery, so it
/// clears the same state every callee sees, and lives inside the run door's
/// `catch_unwind`, so it fires on the panic path too.
pub(crate) struct NurseryGuard(pub(crate) Option<Nursery>);

impl Drop for NurseryGuard {
    fn drop(&mut self) {
        if let Some(nursery) = self.0.as_ref() {
            nursery.clear();
        }
    }
}
