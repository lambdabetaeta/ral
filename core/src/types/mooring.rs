//! The run-invariant frame: what a run fixes once and never changes.
//!
//! [`Mooring`] and its satellites — the structured-event sink, the enquiry
//! desk, the session-fork [`Nursery`], and this run's [`TerminalAccess`].
//! [`Mooring`]'s own doc explains why it is deliberately not [`Shell`] state.

use super::shell::Shell;
use super::shell::workers::WorkerLease;
use super::value::Value;
use crate::process::{DurableRoot, ForegroundScope};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A sink for structured host events: the value-typed dual of the byte
/// [`Io`](crate::io::Io) sinks.
///
/// A *synchronous* trait taking a borrowed
/// [`Value`]; the `surface` builtin forwards its argument to "the current
/// run's sink", and is the identity when none is installed.  Core names no
/// host runtime type — the host decides whether `emit` prints, blocks,
/// coalesces, or crosses a channel.  `Send + Sync` so a same-thread thunk body
/// may share it alongside the rest of the child subtree; a *deferred* worker
/// does not receive the live sink (it buffers into bounded deferred storage and
/// replays on `await`) so a clone of it can never define run completion.
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: &crate::serial::FOValue);
}

/// The no-op surface: a host with no structured-event rail installs `()`,
/// and an absent surface (`None`) behaves identically.
impl EventSink for () {
    fn emit(&self, _ev: &crate::serial::FOValue) {}
}

/// Shared handle to the run-local structured-event sink.
///
/// Run-scoped, not a
/// persistent `Shell` capability: installed only by a run door
/// ([`Shell::run`](crate::Shell::run)), it has no liveness
/// role, so a clone can never decide that a run is over.
pub type SurfaceSink = Arc<dyn EventSink>;

/// The host's answer desk for the engine→host answered channel. `enquire`
/// is blocking and short by contract (§3): a start receipt, a ledger read,
/// a confirmation, a verdict — never a long-running result.
pub trait EnquiryDesk: Send + Sync {
    /// Answer one enquiry synchronously.
    ///
    /// `cancel` is the enquiring run's own scope: a desk that parks waiting
    /// for its answer polls it, so the park unwinds on the same causes every
    /// other poll point observes — an interrupt, a reaper deadline, a
    /// cancelled handle.
    ///
    /// # Errors
    /// Returns `Err` when the host cannot answer `req` — a malformed or
    /// unrecognised request, or a failure while producing the answer.
    fn enquire(
        &self,
        req: crate::serial::FOValue,
        cancel: &crate::process::CancelScope,
    ) -> Result<crate::serial::FOValue, crate::types::Error>;
}

/// Shared handle to the run-local enquiry desk. Run-scoped like `SurfaceSink`,
/// no liveness role. `None` outside a host that answers enquiries.
pub type Desk = std::sync::Arc<dyn EnquiryDesk>;

/// Identifier for a shell session parked in a [`Nursery`] by
/// [`Shell::fork_into_nursery`](crate::types::Shell::fork_into_nursery), and
/// redeemed once by [`Nursery::adopt`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NurseryId(pub u64);

/// [`Nursery`]'s locked interior: the parked forks plus the monotonic
/// counter [`Nursery::park`] mints ids from — a length-derived id would
/// repeat once `adopt`/`clear` free a slot, so the counter must outlive
/// individual entries rather than be read off the map.
#[derive(Default)]
struct NurseryState {
    next: u64,
    parked: HashMap<u64, Shell>,
}

/// Run-local holding pen for engine-side session forks.
///
/// A desk handler is forbidden from taking the session lock (§3's
/// reentrancy law), so it cannot itself hold `&mut Shell` to fork one; the
/// fork happens in the builtin body — the one place lawfully holding
/// `&mut Shell` — via
/// [`Shell::fork_into_nursery`](crate::types::Shell::fork_into_nursery), and
/// crosses to the handler as a [`NurseryId`] the handler later
/// [`adopt`](Self::adopt)s.  Run-scoped like [`Desk`]: shared into
/// same-thread bodies by the run's [`Mooring`] borrow, left `None` on a
/// spawned worker, and emptied by [`NurseryGuard`] so a fork nobody
/// adopted — an enquiry that was refused, or a run that ended before the
/// handler ran — never survives past its run.
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

/// Host-installed destination for a deferred worker's surface batch,
/// delivered when the worker settles and rendered by the host at the next
/// run boundary.
///
/// `None` outside an agent host: a bare REPL installs none,
/// so a deferred worker's surface still reaches a sink only via
/// `await`/`race`.  Carried on the run beside [`SurfaceSink`] and cloned
/// into spawned workers (so a nested `spawn` delivers at its own
/// completion); unlike `surface` it is session-lived.
pub trait DeferredSink: Send + Sync {
    /// Deliver a settled deferred worker's surfaced values as one batch.
    /// Deliver-once is the *caller's* discipline, not the sink's: the worker's
    /// completion path wins the test-and-set on the `joined` latch it shares
    /// with the eliminators (`await`/`race`) before invoking this, so an
    /// implementation only says where the batch goes and cannot forget the
    /// latch.
    fn deliver(&self, batch: Vec<crate::serial::FOValue>);
}

/// This run's authority to hand the controlling terminal to a child.
///
/// The internal (per-run) form of the host-facing
/// [`RequestedTerminalAccess`](crate::run::RequestedTerminalAccess): it carries
/// the extra `ExplicitLoan` state that `_ed-tui` raises mid-run and that a host
/// cannot request at a run door. Read by
/// [`Shell::terminal_lease`](crate::types::Shell::terminal_lease), which
/// yields the session's `&TerminalLease` only when this is `Leased` or
/// `ExplicitLoan`. `Denied` is the default — the safe element — so a
/// mooring with no stated policy can never reach the foreground handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum TerminalAccess {
    /// No child/job foreground handoff in this run (exarch tool runs, a
    /// backgrounded launch, the boot frame before the first run).
    #[default]
    Denied,
    /// This run may foreground terminal-bound children (the interactive REPL,
    /// a terminal-launched script).
    Leased,
    /// A within-run elevation of a `Leased` run: a foreground handoff fires
    /// even though stdout is a buffer, because the body (`_ed-tui`'s `fzf`)
    /// draws on `/dev/tty` and must own the foreground pgid. Set only by
    /// [`Mooring::lend_terminal`], never by a
    /// [`RunRequest`](crate::run::RunRequest).
    ExplicitLoan,
}

/// The run-invariant half of the frame a run installs: where its events go,
/// who answers it, what stops it, and its terminal-foreground authority.
///
/// Nothing here changes between the run's first and last step, so nothing
/// here needs saving and restoring.  It is an owned local on the run's own
/// Rust stack frame ([`Shell::run`](crate::Shell::run)) and reaches every
/// callee as `&Mooring` — scoped sharing, which a borrow gives for free —
/// so an outer run's mooring is restored by the stack unwinding rather than
/// by a guard.  Deliberately *not* a field of [`Shell`]: `&shell.mooring`
/// would borrow the whole shell immutably while every callee wants
/// `&mut Shell`, whereas `&Mooring` and `&mut Shell` are disjoint.
///
/// A child that is a genuine fork — a deferred worker, a cross-process
/// pipeline stage — never shares its parent's: it rebuilds through
/// [`Self::for_worker`] / [`Self::for_stage`], which differ in nearly every
/// field.  A same-thread body shares the parent's by borrow, or by
/// [`Self::lend_terminal`] when its terminal authority must rise for the
/// call.
///
/// Not [`Clone`]: nothing should duplicate a mooring wholesale — a lend is
/// the one lawful derivation, and it goes through [`Self::lend_terminal`]
/// rather than a bulk copy so the raise-never-mint rule on
/// [`TerminalAccess`] lives in one door.  The nursery is emptied by
/// [`NurseryGuard`], an owned local on
/// [`Shell::run_built`](crate::Shell::run_built)'s stack.
pub struct Mooring {
    /// Host-installed sink for structured events surfaced by the `surface`
    /// builtin.  `None` outside a host (e.g. a bare REPL), in which case
    /// [`Self::surface`] is the identity.
    pub(crate) surface: Option<SurfaceSink>,
    /// Host-installed destination for a deferred worker's surface batch.
    /// `None` outside an agent host (e.g. a bare REPL).  Carried into
    /// spawned workers by [`Self::for_worker`] so a nested `spawn` delivers
    /// at its own completion.
    pub(crate) deferred: Option<Arc<dyn DeferredSink>>,
    /// Host-installed desk answering this run's enquiries (§3 of the
    /// enquiry-channel ADR).  `None` outside a host that answers enquiries
    /// (a bare REPL, or any host before the migration installs its desk),
    /// and never given to a deferred worker — a worker outlives its run's
    /// Report, so a worker that could enquire would break the ordering law
    /// and the deferred rail's attenuation.
    pub(crate) desk: Option<Desk>,
    /// The run-local nursery for engine-side session forks, crossing the
    /// reentrancy boundary the same way `desk` does (§"Core additions: the
    /// nursery" of the tools-to-builtins migration). `None` outside a host
    /// that installs one, and never given to a deferred worker.  Emptied by
    /// [`NurseryGuard`], so a fork parked and never adopted before the run
    /// ends leaks nothing.
    pub(crate) nursery: Option<Nursery>,
    /// The run's foreground work scope.
    /// [`signal::check`](crate::process::signal::check) consults it between
    /// effectful steps; a foreground cancel (run timeout, Ctrl-C) unwinds
    /// the same-thread work that shares it.  Always a descendant of
    /// [`SessionState::root`](crate::types::SessionState) by construction.
    /// The one `pub` member: a host clones a deadline child of it, or
    /// cancels it to interrupt the foreground work.
    pub cancel: ForegroundScope,
    /// The lease governing workers this run defers at the durable root.
    /// `None` (an interactive host) leaves a worker until `cancel`, root
    /// abort, or session exit; `Some(lease)` (an agent host) reaps a
    /// still-running worker once it has gone `lease.idle` unobserved —
    /// every `poll` and `await`/`race` sweep renews it — under the
    /// `lease.backstop` absolute ceiling no observation extends.  Carried
    /// into a spawned worker so a `spawn` nested in one sees the same lease.
    pub(crate) deferred_lease: Option<WorkerLease>,
    /// Admission cap on concurrently *running* workers, enforced at the
    /// spawn door: with `Some(cap)`, a birth is refused while `cap` entries
    /// of any class are still `Running` — settled entries lingering under
    /// retention never block admission.  `None` (an interactive host)
    /// admits freely.  Carried exactly as `deferred_lease` is, so a nested
    /// `spawn` cannot evade the cap its run set.
    pub(crate) worker_cap: Option<usize>,
    /// This run's terminal-foreground authority. Gates whether a
    /// child/job foreground handoff can borrow the session's
    /// [`TerminalLease`](crate::process::TerminalLease); see
    /// [`Shell::terminal_lease`](crate::types::Shell::terminal_lease).
    /// Invariant for the mooring's whole life — nothing mutates it in place;
    /// [`Self::lend_terminal`] derives a *new* mooring with it raised
    /// instead. `Denied` on a spawned worker or a stage; flows into
    /// same-thread bodies by the borrow itself, for free.
    pub(crate) terminal_access: TerminalAccess,
}

impl Mooring {
    /// The mooring a detached worker runs under.
    ///
    /// A worker *rebuilds* rather than sharing `parent`: it differs in
    /// nearly every field, and sharing would hand it the run's cancel scope
    /// (a foreground cancel would reach a worker that must outlive the run)
    /// and the run's desk and nursery (both barred to a worker, which
    /// outlives its run's Report).  It keeps `surface` — its own buffering
    /// one, supplied here — and the deferred rail, lease, and cap, so a
    /// `spawn` nested in the body delivers and is governed like its parent.
    /// Its scope is a [`worker`](DurableRoot::worker) of the session root:
    /// a SIGTERM still reaches it, a Ctrl-C does not.  `terminal_access` is
    /// `Denied`: a worker never inherits foreground authority.
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
    /// process ([`crate::child_eval`]).  Nothing crosses the process
    /// boundary, so there is no parent to rebuild from: the stage answers no
    /// enquiries, adopts no forks, defers to no rail, holds no terminal
    /// authority, and surfaces into `surface` — the no-op `()` sink, which
    /// discards the `surface` calls a stage body may still make.
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

    /// A mooring tied to no run: no surface, no rail, no desk, no nursery,
    /// no terminal authority, and a scope under a root nothing else holds.
    ///
    /// The one core does not mint at a run door, and the one a host reaches
    /// for when it calls a builtin body outside a run — a host-embedding
    /// test, above all.  Cancelling its [`cancel`](Self::cancel) is how such
    /// a caller drives a builtin's poll points.
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

    /// Forward any structured-event [`Value`] onto this run's surface sink, if
    /// one is installed; inert when none is.  The public door host builtins use
    /// to surface their own events — a Rust exarch `edit` raising its write
    /// card, a `grep-files` announcing its search.  Core names no event shape
    /// here: the caller hands a fully-formed `Value`, encoded once at this door
    /// into the first-order [`FOValue`](crate::serial::FOValue) the sink
    /// actually carries.  A value that is not first-order (a closure, a handle)
    /// cannot cross the rail; core has no host vocabulary to report that in (an
    /// exarch `Kind::SystemNote` would leak host concepts into core), so the
    /// drop is a `dbg_trace`, not a silent no-op.
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

    /// Emit a structural I/O event onto this run's surface sink, if one is
    /// installed.  This is the single door through which every redirect
    /// read/write and every exec completion announces itself to the host;
    /// with no surface installed it is inert.  The event is a plain
    /// [`Value::Map`] whose shape (`{io: …, …}`) the host decodes — core
    /// names no card type.
    pub(crate) fn emit_io(&self, ev: &Value) {
        self.surface(ev);
    }

    /// Derive a mooring identical to this one with its terminal authority
    /// raised: `Leased => ExplicitLoan`, every other value unchanged.
    /// Raise-never-mint — a `Denied` mooring lends a `Denied` mooring.  The
    /// one lawful derivation of a `Mooring`: every shared handle is cloned,
    /// the parent is untouched, and the derived value dies with the call
    /// that borrowed it (`_ed-tui`'s `fzf`, which draws on `/dev/tty` and
    /// must own the foreground pgid even though stdout is captured).
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

    /// Whether this mooring carries an explicit terminal loan — the
    /// re-entrancy guard for `_ed-tui`: a nested loan would be a logic
    /// error, so the editor builtin refuses when this is already true.
    pub fn in_terminal_loan(&self) -> bool {
        matches!(self.terminal_access, TerminalAccess::ExplicitLoan)
    }
}

/// Empties the run's nursery on `Drop`: a fork parked during the run and
/// never adopted (a refused enquiry, a run that ended before its handler
/// ran) must not survive the run that parked it.
///
/// An owned local on [`Shell::run_built`](crate::Shell::run_built)'s stack,
/// holding a clone of the run's `Option<Nursery>` (`Arc`-shared, so the
/// clone clears the same state every callee sees). Built inside `enter`'s
/// `catch_unwind`, so it fires on the panic path as surely as on the clean
/// one. A worker's or a stage's mooring carries `nursery: None`.
pub(crate) struct NurseryGuard(pub(crate) Option<Nursery>);

impl Drop for NurseryGuard {
    fn drop(&mut self) {
        if let Some(nursery) = self.0.as_ref() {
            nursery.clear();
        }
    }
}
