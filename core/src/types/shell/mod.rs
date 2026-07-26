//! Interpreter state.
//!
//! [`Shell`] is the central interpreter state, partitioned into four fields
//! by lifetime — the field name *is* the invariant:
//!
//! - [`Mobile`] — the persistable bundle (scope + control + context) that
//!   crosses evaluation boundaries and thread spawns.
//! - [`Disposition`] — the mutable residue of the frame a top-level run
//!   installs (IO, call site, terminal authority) and restores on
//!   teardown.  Its run-invariant other half, [`Mooring`], is deliberately
//!   *not* a field here: it lives on the run's Rust stack frame and reaches
//!   callees as `&Mooring`, disjoint from `&mut Shell`.
//! - [`SessionState`] — state that survives every run (durable cancel root,
//!   the anchor top-level runs nest under, source registry, exit hints,
//!   builtin table).
//! - [`LocalState`] — host-local scratch with its own flow rules (audit, REPL).
//!
//! The methods on [`Shell`] live in submodules grouped by concern:
//!
//! - [`init`] — construction and the startup env-var seeding pass.
//! - [`context`] — the [`Context`] impl: dynamic-context verbs.
//! - [`scope`] — `with_*` scope guards (`within` / `grant`), alias
//!   frames, audit-subtree combinators.
//! - [`checks`] — capability-check forwarders to the
//!   `capability::check_*(&Context, …)` decisions.
//! - [`cwd`] — logical working directory and path resolution.
//! - [`hooks`] — the hook table's types and the session's registration
//!   surface ([`Shell::register_hook`] and its inverses).
//! - [`inherit`] — state transfer between parent and child Shells
//!   ([`Shell::with_thunk_body`] for same-thread bodies;
//!   [`Shell::spawn_thread`], [`Shell::child_of`], [`Shell::child_from`]
//!   for forks, plus [`Shell::inherit_from`] / [`Shell::return_to`]).
//!
//! The small primitives that don't fit a concern — error
//! construction, stdout writes, status writes, `$ENV` / `$ARGS`
//! resolution, closure-capture snapshot — live directly on this
//! module.

pub(crate) mod bindings;
mod checks;
mod context;
pub(crate) mod control;
pub(crate) mod cwd;
pub(crate) mod detached;
pub(crate) mod hooks;
mod host;
mod inherit;
mod init;
pub(crate) mod modules;
pub(crate) mod repl;
mod scope;
pub(crate) mod workers;

pub use host::TerminalLoan;
pub(crate) use inherit::ThunkBody;

use self::bindings::BindingLedger;
use self::control::ControlState;
use self::cwd::Cwd;
use self::detached::DetachPolicy;
use self::modules::Modules;
use self::repl::ReplScratch;
use self::workers::{WorkerLease, WorkerRegistry};
use super::audit::Audit;
use super::builtin::BuiltinTable;
use super::capability::GrantStack;
use super::env::Env;
use super::env::EnvVars;
use super::error::Error;
use super::handler::HandlerStack;
use super::value::Value;
use crate::diagnostic::CallSite;
use crate::io::Io;
use crate::process::{DurableRoot, ForegroundScope};
use crate::source::{FileId, Source, SourceDb, Span};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Default cap on non-tail closure-call depth.
///
/// Insurance against
/// stack-guard SIGABRT from runaway recursion the typechecker can't
/// catch.  Tail calls are landed in the trampoline loop and don't
/// count.  Overridable via rc / CLI; in practice never tuned.
pub const DEFAULT_RECURSION_LIMIT: usize = 1024;

/// The Send+Clone dynamic context carried by every child computation — a
/// thunk body, a spawned thread (`spawn`, `par`, pipeline stage), or a REPL
/// aside.
///
/// Lives as the `context` field of [`Mobile`] and clones wholesale
/// into a child; the call site and builtin table are deliberately *not*
/// here (they are run- and session-state respectively), so a `Context` clone
/// carries no render registry and no host dispatch.
///
/// Deliberately excluded (each handled separately):
/// - `scope` (sibling on [`Mobile`]): closure-captured per child; not
///   bulk-inherited from parent.
/// - `control` (sibling on [`Mobile`]): per-field flow rules in
///   [`ControlState::inherit_from`]; `last_status` reset on a spawned thread.
/// - the run's own frame ([`Mooring`], [`Disposition`]): installed afresh per
///   run, flowed per their own manifest, never clone-shared blind.
#[derive(Debug, Clone, Default)]
pub struct Context {
    // ── attenuable by within / grant ─────────────────────────────────────
    /// Process env-var overrides set by `within [shell: …]`.
    /// `pub(crate)` so two privileged callers — the
    /// [`Shell::with_env`] restore step and the child-eval mobile
    /// reconstruction
    /// ([`WireMobile::into_runtime`](crate::subprocess::WireMobile::into_runtime)) —
    /// can install a vetted whole map.  Per-key mutation
    /// goes through [`Context::set_env_var`] (and friends).  `PWD` /
    /// `OLDPWD` are excluded: those keys live on `context.cwd`, and a
    /// copy here would shadow the canonical pair and drift on the next
    /// `cd`.
    pub(crate) env_overrides: EnvVars,
    /// Working directory override set by `within [dir: …]`.  Rolled
    /// back at scope exit; distinct from [`Cwd::current`] (the
    /// `cd`-mutated persistent cwd).
    pub dir: Option<PathBuf>,
    /// Capability restriction stack — innermost last.
    pub grants: GrantStack,
    /// `within [handlers: …, handler: …]` effect-handler stack —
    /// innermost last.
    pub handlers: HandlerStack,

    /// Hook dispatch table — session-lived named run-entry
    /// points (rc-declared prompt, startup block, plugin hooks, …).
    /// A separate namespace from both the user lexical scope and the
    /// handler stack: hooks are run roots, never commands or
    /// readable variables.
    pub hooks: std::collections::HashMap<hooks::HookName, hooks::Hook>,

    // ── dynamic context, not attenuable ─────────────────────────────────
    /// Invocation positional args (`$ARGS`, `$1`, …) passed on the
    /// command line or by `source`.  Inherits with caller; not
    /// modified by `within` / `grant`.
    pub args: Vec<String>,
    /// Module-loader state (stack, depth).
    pub modules: Modules,
    /// Shell-owned logical cwd pair (`cd`-mutated current + `OLDPWD`
    /// companion).  Snapshotted so spawned threads see the current
    /// logical cwd at the spawn point; flowed back on same-thread
    /// thunk return so a `cd` inside a thunk persists.
    pub cwd: Cwd,
}

// Capability checks operate on this dynamic-context cluster — the
// type system thereby prevents policy code from reading lexical scope,
// REPL scratch, control state, or exit hints.  The decisions themselves
// are the `capability::check_*(&Context, …)` functions, each of which
// borrows this Context and folds the whole stack from there.

/// The persistable half of a [`Shell`]: lexical scope, control
/// counters, dynamic [`Context`].
///
/// Cloned on every
/// [`Shell::inherit_from`] / [`Shell::spawn_thread`] and snapshotted
/// across evaluation boundaries via [`Shell::mobile`] /
/// [`Shell::install_mobile`].
#[derive(Clone)]
pub struct Mobile {
    /// Lexical scope chain.  Closure-captured; doesn't flow through
    /// `inherit_from` / `spawn_thread`.  See `types/env.rs`.
    pub scope: Env,
    /// Evaluator control-flow counters: `last_status`, `call_depth`,
    /// `recursion_limit`.  Different
    /// fields obey different flow rules — see
    /// [`Shell::inherit_from`] / [`Shell::return_to`] and
    /// `types/shell/control.rs`.
    pub control: ControlState,
    /// The clone-into-child dynamic context: env/cwd/grants, handlers,
    /// args, and module-loader state.  No source cursor, no builtin table.
    /// See [`Context`] and [`Cwd`] for the cwd-pair semantics.
    pub context: Context,
}

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
/// [`Shell::fork_into_nursery`], and redeemed once by [`Nursery::adopt`].
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
/// `&mut Shell` — via [`Shell::fork_into_nursery`], and crosses to the
/// handler as a [`NurseryId`] the handler later [`adopt`](Self::adopt)s.
/// Run-scoped like [`Desk`]: shared into same-thread bodies by the run's
/// [`Mooring`] borrow, left `None` on a spawned worker, and emptied by that
/// mooring's [`Drop`] so a fork nobody adopted — an enquiry that was
/// refused, or a run that ended before the handler ran — never survives
/// past its run.
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
/// [`Shell::terminal_lease`](Shell::terminal_lease), which yields the session's
/// `&TerminalLease` only when this is `Leased` or `ExplicitLoan`. `Denied` is
/// the default — the safe element — so a frame with no stated policy can never
/// reach the foreground handoff.
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
    /// draws on `/dev/tty` and must own the foreground pgid. Set by the host
    /// loan token, never by a [`RunRequest`](crate::run::RunRequest).
    ExplicitLoan,
}

/// The run-invariant half of the frame a run installs: where its events go,
/// who answers it, and what stops it.
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
/// field.  A same-thread body shares the parent's by borrow.
///
/// Not [`Clone`], and that is load-bearing: [`Drop`] empties the nursery, so
/// a second owner would empty it while the first still ran.
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
    /// this type's [`Drop`], so a fork parked and never adopted before the
    /// run ends leaks nothing.
    pub(crate) nursery: Option<Nursery>,
    /// The run's foreground work scope.
    /// [`signal::check`](crate::process::signal::check) consults it between
    /// effectful steps; a foreground cancel (run timeout, Ctrl-C) unwinds
    /// the same-thread work that shares it.  Always a descendant of
    /// [`SessionState::root`] by construction.  The one `pub` member: a host
    /// clones a deadline child of it, or cancels it to interrupt the
    /// foreground work.
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
}

/// Empty the nursery when the run's mooring goes out of scope: a fork parked
/// during the run and never adopted (a refused enquiry, or a run that ended
/// before its handler ran) must not survive the run that parked it.  Drop of
/// an owned local fires on unwind too, so a panicking run leaks no fork
/// either.  A worker's or a stage's mooring carries `nursery: None`, so its
/// drop is inert.
impl Drop for Mooring {
    fn drop(&mut self) {
        if let Some(nursery) = self.nursery.as_ref() {
            nursery.clear();
        }
    }
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
    /// a SIGTERM still reaches it, a Ctrl-C does not.
    pub(crate) fn for_worker(parent: &Self, root: &DurableRoot, surface: SurfaceSink) -> Self {
        Self {
            surface: Some(surface),
            deferred: parent.deferred.clone(),
            desk: None,
            nursery: None,
            cancel: root.worker(),
            deferred_lease: parent.deferred_lease,
            worker_cap: parent.worker_cap,
        }
    }

    /// The mooring a cross-process pipeline stage runs under, in the helper
    /// process ([`crate::child_eval`]).  Nothing crosses the process
    /// boundary, so there is no parent to rebuild from: the stage answers no
    /// enquiries, adopts no forks, defers to no rail, and surfaces into
    /// `surface` — the no-op `()` sink, which discards the `surface` calls a
    /// stage body may still make.
    pub(crate) fn for_stage(root: &DurableRoot, surface: SurfaceSink) -> Self {
        Self {
            surface: Some(surface),
            deferred: None,
            desk: None,
            nursery: None,
            cancel: root.worker(),
            deferred_lease: None,
            worker_cap: None,
        }
    }

    /// A mooring tied to no run: no surface, no rail, no desk, no nursery,
    /// and a scope under a root nothing else holds.
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
}

/// The mutable residue of the frame a run installs: what genuinely differs
/// between one step of a run and the next.
///
/// A run builds one, swaps it into `shell.run`, runs, and restores the
/// previous one on teardown — the save/restore the run-invariant
/// [`Mooring`] no longer needs.  Same-thread bodies flow these through
/// [`Disposition::inherit_from`] / [`Disposition::return_to`].
pub struct Disposition {
    /// Pipeline-stage IO: streams, value channel, terminal state, flags.
    pub(crate) io: Io,
    /// Span of the node that dispatched the command now running — the
    /// user-visible position audit nodes and capability checks name, held
    /// across prelude wrappers so they name the user's call rather than the
    /// wrapper's body.  `None` before the first dispatch of a run; resolved
    /// through [`SessionState::sources`] by [`Shell::site_of`].
    pub(crate) call_site: Option<Span>,
    /// This run's terminal-foreground authority. Gates whether a
    /// child/job foreground handoff can borrow the session's
    /// [`TerminalLease`](crate::process::TerminalLease); see
    /// [`Shell::terminal_lease`].  Mutates within a run — a loan token
    /// raises it and surrenders it back ([`Shell::begin_terminal_loan`]) —
    /// which is why it is residue and not mooring.  Restored with the rest
    /// by the run guard; flows into same-thread bodies (so a pipeline
    /// launched inside an `_ed-tui` loan still foregrounds) and is left
    /// `Denied` on a spawned worker.
    pub(crate) terminal_access: TerminalAccess,
}

impl Disposition {
    /// Same-thread child flow-in: clone the byte sinks, move the read-once
    /// stdin out of the parent, and adopt its call site.  Mirror of
    /// [`Self::return_to`].
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.io.inherit_from(&mut parent.io);
        self.call_site = parent.call_site;
        // Terminal access flows in so a pipeline launched inside an `_ed-tui`
        // loan (or any same-thread body of a Leased run) sees the parent's
        // authority. It does not flow back in `return_to`: the parent retains
        // its own access (it set the loan and will end it).
        self.terminal_access = parent.terminal_access;
    }

    /// Same-thread child flow-out: return the read-once stdin to the parent
    /// so sibling calls see the unconsumed pipe.  The call site does not flow
    /// back — the asymmetry is the point.
    pub fn return_to(&mut self, parent: &mut Self) {
        self.io.return_to(&mut parent.io);
    }
}

/// State that outlives every run's teardown.
pub struct SessionState {
    /// The session's durable cancel root.  Detached workers (`spawn`,
    /// `watch`, `par`) parent under this rather than under the swappable
    /// foreground [`Mooring::cancel`], so a foreground cancel never reaches
    /// them.  Only a [`RootAbort`](crate::process::CancelCause::RootAbort) on
    /// the root, or a cancel on the worker's own scope, stops such a worker.
    /// Minted deaf to the ambient causes; [`Shell::face_signals`] re-mints
    /// it facing, for the one host that owns the process's signals.  Shared,
    /// not re-minted, by an aside ([`Shell::join_session`]).
    pub(crate) root: DurableRoot,
    /// The scope a *top-level* run nests its foreground frame under — the
    /// role the boot frame's `run.cancel` played while the frame lived on
    /// the `Shell`.  Re-minted wherever the root is
    /// ([`Shell::new`], [`Shell::face_signals`], [`Shell::join_session`]),
    /// and never afterwards: a run entered through
    /// [`Shell::run`](crate::Shell::run) hangs off it, while one entered
    /// through [`Shell::run_nested`](crate::Shell::run_nested) hangs off the
    /// mooring of the run it nests in, so the tree is the LIFO extent it
    /// claims to be.
    pub(crate) anchor: ForegroundScope,
    /// Durable source registry, keyed by [`FileId`](crate::source::FileId).
    /// A run resets and seeds it at run start, module loads append to it,
    /// and hosts read it after the run returns to render runtime errors.
    /// Durable across a run's teardown, not across all future runs.
    /// Skipped by IPC/serde — it is render state, not mobile.
    pub(crate) sources: SourceDb,
    /// Exit-code hint table — loaded once at startup from the data directory.
    pub(crate) exit_hints: crate::exit_hints::ExitHints,
    /// Host-installed command dispatch table.  Builtin bodies are Rust fn
    /// pointers or captured host closures — process-local dispatch state, not
    /// serialised ral values, so the receiver of a wire mobile supplies its
    /// own rather than shipping it in a [`WireMobile`](crate::subprocess::WireMobile).
    pub(crate) builtins: BuiltinTable,
    /// Host-installed `name -> doc` entries for a sourced closure library
    /// (exarch's agent helpers) that `help`/`explain` cannot otherwise see —
    /// see [`Shell::install_library_docs`](super::Shell::install_library_docs).
    pub(crate) library_docs: std::collections::HashMap<String, String>,
    /// The session's terminal-foreground witness, minted once at construction
    /// from the startup `tcgetpgrp == getpgrp` predicate. `Some` when ral owns
    /// the controlling terminal's foreground (interactive REPL, terminal-
    /// launched script), `None` otherwise (piped/backgrounded/tty-less, and
    /// every non-Unix platform). Lent — never moved or cloned — to the
    /// foreground handoff via [`Shell::terminal_lease`], and only when the
    /// installed run's [`TerminalAccess`] permits.
    pub(crate) terminal_lease: Option<crate::process::TerminalLease>,
    /// The guest process jail, installed once by
    /// [`crate::engine::run_engine`] when `RAL_GUEST` is set — never on an
    /// ordinary host run. Shared by `Arc`, never cloned-and-reset, across
    /// every fork and spawned worker, so concurrent spawns from sibling
    /// Shells still mint distinct uids and cgroups off the one counter.
    pub(crate) guest_jail: Option<std::sync::Arc<crate::process::jail::GuestJail>>,
}

/// Host-local scratch whose members carry their own flow rules — not a
/// lifetime category, the residue left once run and session state are named.
pub struct LocalState {
    /// Audit collector: the in-flight execution tree plus the current
    /// byte-capture policy.  Scope-introducing builtins (`grant`, `within`,
    /// `guard`, `try`, `audit`) own the children their bodies produce; the
    /// dispatcher posts one node per command via the command lifecycle
    /// in [`crate::evaluator::audit`].
    pub(crate) audit: Audit,
    /// REPL-only scratch state (editor plugin context + queued chpwd
    /// notification).  Doesn't flow across threads or IPC; moved on
    /// cross-process pipeline-stage boundary.  See `types/shell/repl.rs`.
    pub(crate) repl: ReplScratch,
    /// Directory of every worker (`spawn`, `watch`, `service`) detached
    /// from this shell, keyed by nothing but the handle it registered with — see
    /// `types/shell/workers.rs`.  One registry per `Shell`, i.e. one per
    /// agent; [`Shell::spawn_thread`] shares it into a spawned worker's own
    /// shell (so a nested `spawn` registers alongside its parent), but a
    /// sub-agent fork starts with a fresh one.
    pub(crate) workers: WorkerRegistry,
    /// The binding-lease ledger (`types/shell/bindings.rs`,
    /// `decisions/260629_agent-binding-reaping`): inert until a host arms
    /// it, then tracks the idle-call age of every non-baseline top-level
    /// name so an agent host can prune scratch that has gone unused for too
    /// long. Unlike [`Self::workers`] this needs no lock — it has exactly
    /// one writer, the thread that owns `&mut Shell` for every run, install,
    /// and prune (verified in `bindings.rs`'s module doc). One ledger per
    /// `Shell`, i.e. one per agent; a sub-agent fork or spawned worker starts
    /// with a fresh, inert one — nothing shares it, nothing flows back.
    pub(crate) bindings: BindingLedger,
    /// This session's authority to birth processes that outlive it
    /// (`types/shell/detached.rs`): `None` until a host arms it, and armed in
    /// the same act that installs the `detach` builtin, so an unarmed shell
    /// lacks the verb rather than owning one it cannot budget. `Arc`-shared
    /// into a spawned worker's shell like [`Self::workers`], so a `detach`
    /// nested in a `spawn { }` body spends the owning session's budget.
    pub(crate) detach: Option<Arc<DetachPolicy>>,
    /// Whether this state owns its worker registry. True everywhere but a
    /// [`Shell::spawn_thread`] child, which shares its *parent's* registry
    /// by `Arc` clone — a worker's own shell dropping must not cancel its
    /// parent's whole roster. Read by the [`Drop`] below: a session's
    /// workers die when the session's shell is torn down, the ownership
    /// edge the session-ledger ADR calls for, closed once here rather than
    /// at every call site that can end a session's life.
    pub(crate) workers_owned: bool,
}

impl Default for LocalState {
    fn default() -> Self {
        Self {
            audit: Audit::default(),
            repl: ReplScratch::default(),
            workers: WorkerRegistry::default(),
            bindings: BindingLedger::default(),
            detach: None,
            workers_owned: true,
        }
    }
}

/// A session's workers die with the session's shell: cancelling on drop
/// covers every teardown path — an agent's ordinary end, a `/clear`'s shell
/// replacement, a wire session's detach — without a host call site.
impl Drop for LocalState {
    fn drop(&mut self) {
        if self.workers_owned {
            self.workers.cancel_all();
        }
    }
}

/// The runtime, partitioned by lifetime.
///
/// A field either changes within a run
/// ([`Disposition`]), survives a run ([`SessionState`]), crosses evaluation
/// boundaries ([`Mobile`]), or stays as host scratch ([`LocalState`]).  What
/// a run fixes once and never changes is not here at all: it is the
/// [`Mooring`] the run door owns on its stack.
///
/// Every field is `pub(crate)`: the partition that encodes run safety,
/// capability attenuation, and mobile framing is core's invariant, not a
/// public API a host can reach past.  Hosts drive a session through the
/// intent verbs — the [`mod@host`] accessors and the scope/context verbs —
/// each of which is a complete operation rather than a raw field poke.
/// Durability is core's own: the run door ([`Shell::run`])
/// checkpoints and rolls back the [`Mobile`] around every run.
pub struct Shell {
    pub(crate) mobile: Mobile,
    pub(crate) run: Disposition,
    pub(crate) session: SessionState,
    pub(crate) local: LocalState,
}

impl Shell {
    /// Construct an unspanned [`Error`]: the break path stamps the span of
    /// the innermost node it unwinds through.  A caller already holding a
    /// better span attaches it with [`Error::at_span`].
    pub fn err(&self, msg: impl Into<String>, status: i32) -> Error {
        Error::new(msg, status)
    }

    /// Like [`Self::err`], with an additional hint.
    pub fn err_hint(&self, msg: impl Into<String>, hint: impl Into<String>, status: i32) -> Error {
        Error::new(msg, status).with_hint(hint)
    }

    /// Resolve `span` to the value-typed [`CallSite`] audit nodes and
    /// capability checks carry: the name of its source in this session's
    /// registry plus the 1-indexed line and column of its start.
    /// [`CallSite::default`] when there is no span, or its source is not
    /// registered here.
    pub(crate) fn site_of(&self, span: Option<Span>) -> CallSite {
        let resolved = span.and_then(|s| self.session.sources.get(s.file).map(|src| (s, src)));
        let Some((span, source)) = resolved else {
            return CallSite::default();
        };
        let (line, col) = source.byte_to_line_col(span.start as usize);
        CallSite {
            script: source.name().to_string(),
            line,
            col,
        }
    }

    /// This run's call site as a [`CallSite`] — [`Self::site_of`] applied to
    /// `run.call_site`.  Taken by value so the caller may hold `&mut audit`
    /// alongside it.
    pub(crate) fn call_site(&self) -> CallSite {
        self.site_of(self.run.call_site)
    }

    /// Display name of the currently-executing script — the name of the
    /// source this run's call site belongs to.  `None` in the REPL, before
    /// the first dispatch, and whenever that source is unregistered.
    pub(crate) fn script_name(&self) -> Option<&str> {
        self.session
            .sources
            .get(self.run.call_site?.file)
            .map(Source::name)
    }

    /// Put one enquiry to `mooring`'s host desk and block for the answer.
    /// The absent-desk error is the honest answer of a host that answers none
    /// (the bare REPL, and any host before the migration installs its desk).
    ///
    /// # Errors
    /// Returns `Err` if no desk is installed on this run, or if the
    /// installed desk's [`EnquiryDesk::enquire`] returns one.
    pub fn enquire(
        &self,
        mooring: &Mooring,
        req: crate::serial::FOValue,
    ) -> Result<crate::serial::FOValue, crate::types::Error> {
        match mooring.desk.as_ref() {
            Some(desk) => desk.enquire(req, mooring.cancel.as_scope()),
            None => Err(self.err("this host answers no enquiries", 1)),
        }
    }

    /// Fork this shell ([`Self::fork_session`]) and park the fork in
    /// `mooring`'s nursery, returning the [`NurseryId`] a desk handler — barred
    /// by the reentrancy law from holding `&mut Shell` itself — later
    /// redeems with [`Nursery::adopt`]. The absent-nursery error is the
    /// honest answer of a host that adopts no forked sessions (the bare
    /// REPL, and any host before it installs a nursery).
    ///
    /// # Errors
    /// Returns `Err` if no nursery is installed on this run.
    pub fn fork_into_nursery(&self, mooring: &Mooring) -> crate::types::Settled<NurseryId> {
        match mooring.nursery.as_ref() {
            Some(nursery) => Ok(nursery.park(self.fork_session())),
            None => Err(crate::types::Break::Error(
                self.err("this host adopts no forked sessions", 1),
            )),
        }
    }

    /// Atomically overwrite `path` with `bytes` through core's full
    /// `>`-redirect write recipe — symlink-resolved, mode-preserving,
    /// fsync-durable — while emitting no io event.  The public write door for a
    /// host builtin (exarch's `edit-hash`/`edit-replace`) that must write *below* the
    /// redirect frame and speak its own surface: it shares the one atomic
    /// recipe instead of forking a weaker temp-file write that silently narrows
    /// the target's mode, follows symlinks by replacing them, and skips the
    /// durability flush.
    ///
    /// # Errors
    /// Returns `Err` if the target cannot be opened for writing, if the
    /// write fails, or if the atomic commit (temp-file rename and fsync)
    /// fails.
    pub fn atomic_write(&mut self, path: &str, bytes: &[u8]) -> crate::types::Settled<()> {
        crate::runtime::command::atomic_write(path, bytes, self)
    }

    /// Install a script context: register `text` under display `name` in the
    /// session registry, returning the [`FileId`] the compiled program's
    /// spans must carry so they resolve to it.
    pub fn install_script_context(&mut self, name: String, text: &str) -> FileId {
        self.session
            .sources
            .register(Source::from_text(&name, text))
    }

    /// Install a top-level run's script context after clearing the session
    /// registry, so the prior run's sources are reclaimed before this run
    /// registers its own.  Used only at the interactive run boundary (REPL /
    /// shell); module loads stay on
    /// [`install_script_context`](Self::install_script_context), which appends
    /// so a run's `source`d modules remain resolvable when its error renders
    /// at end of run.
    pub fn install_root_context(&mut self, name: String, text: &str) -> FileId {
        self.session.sources.reset();
        self.install_script_context(name, text)
    }

    /// Install a script context under an already-minted [`FileId`] — the
    /// seam a re-exec'd pipeline-stage child uses to resolve its spans
    /// against the exact source and file identity its parent already
    /// registered, rather than minting a second, differently-numbered copy
    /// in its own (empty) session registry.
    pub(crate) fn install_remote_context(&mut self, name: String, file: FileId, text: &str) {
        self.session
            .sources
            .register_at(file, Source::from_text(&name, text));
    }

    /// Resolve the seven pseudo-variables (`$ENV`, `$ARGS`, `$SCRIPT`,
    /// `$NPROC`, `$CWD`, `$STATUS`, `$USER`).  All-caps names are the
    /// shell's; lowercase names are the user's.  These are computed on
    /// demand rather than stored in scope, so [`Self::lookup_value_name`]
    /// consults them after lexical scope and before the builtin table.
    /// Any other name returns `None`.
    pub fn pseudo_var(&self, name: &str) -> Option<Value> {
        match name {
            "ENV" => {
                // PWD / OLDPWD are shell-cwd-derived, not
                // env-overrides: they live on `context.cwd` and
                // change every `cd`.  Drop them at the source so
                // `$ENV` reads as the rule.
                let mut merged: HashMap<String, String> = std::env::vars()
                    .filter(|(k, _)| !matches!(k.as_str(), "PWD" | "OLDPWD"))
                    .collect();
                merged.extend(
                    self.mobile
                        .context
                        .env_overrides
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone())),
                );
                let pairs: Vec<_> = merged
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect();
                Some(Value::map(pairs))
            }
            "ARGS" => Some(Value::list(
                self.mobile
                    .context
                    .args
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            )),
            // $SCRIPT: path of the currently-executing file, i.e. the name
            // of the source the call site's span belongs to.  Absent in the
            // REPL, under `-c`, and during prelude loading.
            "SCRIPT" => match self.script_name() {
                None | Some("" | "-c" | "<prelude>") => None,
                Some(s) => Some(Value::String(s.to_string())),
            },
            "NPROC" => Some(Value::Int(std::thread::available_parallelism().map_or(
                1,
                |n| {
                    #[allow(
                        clippy::cast_possible_wrap,
                        reason = "CPU count from available_parallelism is tiny"
                    )]
                    {
                        n.get() as i64
                    }
                },
            ))),
            "CWD" => {
                let p = self.cwd();
                let home = crate::path::home_from_env();
                let cwd_str = crate::path::abbreviate_home(&p, &home);
                let cwd_str = if cwd_str.is_empty() {
                    "?".into()
                } else {
                    cwd_str
                };
                Some(Value::String(cwd_str))
            }
            "STATUS" => Some(Value::Int(i64::from(self.mobile.control.last_status))),
            "USER" => Some(Value::String(crate::path::user_name_from_env())),
            _ => None,
        }
    }

    /// Resolve `name` at value position (`$name` and other
    /// [`crate::ir::Val::Variable`] uses). Value lookup is binding-only:
    /// lexical scope, pseudo variables, and explicitly reified
    /// host/builtin bindings. User aliases and `within` handlers are
    /// operation handlers, not first-class values.
    pub fn lookup_value_name(&self, name: &str) -> Option<Value> {
        if let Some(v) = self.mobile.scope.get(name) {
            return Some(v.clone());
        }
        if let Some(v) = self.pseudo_var(name) {
            return Some(v);
        }
        let entry = self.session.builtins.get(name)?;
        crate::builtins::synthesize_builtin_value(&entry)
    }

    /// Set `last_status` from a boolean (`true` → 0, `false` → 1).
    #[inline]
    pub fn set_status_from_bool(&mut self, ok: bool) {
        self.mobile.control.last_status = i32::from(!ok);
    }

    /// Write `bytes` to the current stdout sink.
    ///
    /// `BrokenPipe` is treated as a clean shutdown: the downstream
    /// reader has closed its end of the pipe (e.g. `fzf` accepted a
    /// selection, `head` took its quota), so further writes are
    /// pointless but not an error.  This matches traditional Unix
    /// tools, which exit silently on `SIGPIPE`, and prevents the
    /// pipeline supervisor from interpreting an EPIPE on a builtin
    /// writer as a failure that warrants tearing the pgid down with
    /// `SIGKILL` — a teardown that would surface as exit status 137
    /// on sibling stages that had themselves exited cleanly.
    ///
    /// # Errors
    /// Returns `Err` if the underlying write fails with anything other than
    /// `BrokenPipe`, which is swallowed as a clean shutdown.
    pub fn write_stdout(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self.run.io.stdout.write_all(bytes) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Snapshot the current scope chain for closure capture.  Returns
    /// an `Arc<Env>` so multiple closures (e.g. a `letrec` bank)
    /// created from one snapshot share one allocation; subsequent
    /// thunk clones are refcount bumps.
    pub fn snapshot(&self) -> Arc<Env> {
        Arc::new(self.mobile.scope.clone())
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new(crate::io::TerminalState::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudo_var_cwd_is_live() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let val = shell.pseudo_var("CWD").expect("$CWD must resolve");
        match val {
            Value::String(s) => assert!(!s.is_empty(), "$CWD must be non-empty"),
            other => panic!("$CWD must be a String, got {other:?}"),
        }
    }

    #[test]
    fn pseudo_var_status_reflects_last_status() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.set_status_from_bool(false);
        let val = shell.pseudo_var("STATUS").expect("$STATUS must resolve");
        assert_eq!(val, Value::Int(1));
        shell.set_status_from_bool(true);
        let val = shell.pseudo_var("STATUS").expect("$STATUS must resolve");
        assert_eq!(val, Value::Int(0));
    }

    #[test]
    fn pseudo_var_user_is_live() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let val = shell.pseudo_var("USER").expect("$USER must resolve");
        match val {
            Value::String(s) => assert!(!s.is_empty(), "$USER must be non-empty"),
            other => panic!("$USER must be a String, got {other:?}"),
        }
    }

    #[test]
    fn pseudo_var_unknown_returns_none() {
        let shell = Shell::new(crate::io::TerminalState::default());
        assert!(shell.pseudo_var("NOSUCHVAR").is_none());
    }

    #[test]
    fn lookup_value_name_sees_pseudo_vars() {
        let shell = Shell::new(crate::io::TerminalState::default());
        assert!(shell.lookup_value_name("CWD").is_some());
        assert!(shell.lookup_value_name("STATUS").is_some());
        assert!(shell.lookup_value_name("USER").is_some());
    }

    /// The nursery twin of `enquire`'s absent-desk contract: a `Shell` with
    /// no nursery installed answers `fork_into_nursery` with the honest
    /// absence error, verbatim.
    #[test]
    fn fork_into_nursery_errors_honestly_without_a_nursery() {
        let shell = Shell::new(crate::io::TerminalState::default());
        match shell
            .fork_into_nursery(&Mooring::adrift())
            .expect_err("no nursery is installed")
        {
            crate::types::Break::Error(e) => {
                assert_eq!(e.message, "this host adopts no forked sessions");
            }
            other @ crate::types::Break::Escape(_) => {
                panic!("expected Break::Error, got {other:?}")
            }
        }
    }

    /// A nursery installed on the run round-trips a session fork:
    /// `fork_into_nursery` parks a [`Nursery::park`] entry and hands back its
    /// id, and `Nursery::adopt` redeems that id for a live child `Shell` —
    /// one that still carries the parent's whole lexical scope, exactly as
    /// `fork_session` promises (mirrors
    /// `fork_session_holds_no_terminal_authority`'s style in `inherit.rs`).
    #[test]
    fn nursery_round_trips_a_forked_session() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell
            .mobile
            .scope
            .set("parent_binding".to_string(), Value::Int(42));
        let nursery = Nursery::default();
        // `Mooring` is `Drop`, so `..base` update syntax is illegal: every
        // literal spells all seven members.
        let mooring = Mooring {
            surface: None,
            deferred: None,
            desk: None,
            nursery: Some(nursery.clone()),
            cancel: shell.session.root.worker(),
            deferred_lease: None,
            worker_cap: None,
        };

        let id = shell
            .fork_into_nursery(&mooring)
            .expect("a nursery is installed");
        let child = nursery
            .adopt(id)
            .expect("the parked fork must be adoptable");

        assert_eq!(
            child.mobile.scope.get("parent_binding"),
            Some(&Value::Int(42)),
            "the fork must carry the parent's lexical scope"
        );
    }
}
