//! Interpreter state.
//!
//! [`Shell`] is the central interpreter state, partitioned into four fields
//! by lifetime — the field name *is* the invariant:
//!
//! - [`Mobile`] — the persistable bundle (scope + control + context) that
//!   crosses evaluation boundaries and thread spawns.
//! - [`TurnState`] — the dynamic frame a top-level turn installs (IO, surface
//!   sink, foreground cancel scope, source cursor) and restores on teardown.
//! - [`SessionState`] — state that survives every turn (durable cancel root,
//!   source registry, exit hints, builtin table).
//! - [`LocalState`] — host-local scratch with its own flow rules (audit, REPL).
//!
//! The methods on [`Shell`] live in submodules grouped by concern:
//!
//! - `init` — construction and the startup env-var seeding pass.
//! - `context` — the [`Context`] impl: dynamic-context verbs.
//! - `scope` — `with_*` scope guards (`within` / `grant`), alias
//!   frames, audit-subtree combinators.
//! - `checks` — capability-check forwarders to the
//!   `capability::check_*(&Context, …)` decisions.
//! - `cwd` — logical working directory and path resolution.
//! - `inherit` — state transfer between parent and child Shells
//!   ([`Shell::with_thunk_body`] for same-thread bodies;
//!   [`Shell::spawn_thread`], [`Shell::child_of`], [`Shell::child_from`]
//!   for forks, plus [`Shell::inherit_from`] / [`Shell::return_to`]).
//!
//! The small primitives that don't fit a concern — error
//! construction, stdout writes, status writes, `$env` / `$args`
//! resolution, closure-capture snapshot — live directly on this
//! module.

pub(crate) mod bindings;
mod checks;
mod context;
pub(crate) mod control;
pub(crate) mod cwd;
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
use crate::diagnostic::LocationCursor;
use crate::io::Io;
use crate::process::{DurableRoot, ForegroundScope};
use crate::source::FileId;
use crate::source::{Source, SourceDb};
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
/// into a child; the source cursor and builtin table are deliberately *not*
/// here (they are turn- and session-state respectively), so a `Context` clone
/// carries no render registry and no host dispatch.
///
/// Deliberately excluded (each handled separately):
/// - `scope` (sibling on [`Mobile`]): closure-captured per child; not
///   bulk-inherited from parent.
/// - `control` (sibling on [`Mobile`]): per-field flow rules in
///   [`ControlState::inherit_from`]; `last_status` reset on a spawned thread.
/// - the [`TurnState`] substates (`io`, `surface`, `cancel`, `loc`): installed
///   afresh per turn, flowed per their own manifest, never clone-shared blind.
#[derive(Debug, Clone, Default)]
pub struct Context {
    // ── attenuable by within / grant ─────────────────────────────────────
    /// Process env-var overrides set by `within [shell: …]`.
    /// `pub(crate)` so two privileged callers — the
    /// [`Shell::with_env`] restore step and the child-eval mobile
    /// reconstruction (`WireMobile::into_runtime`) — can install a
    /// vetted whole map.  Per-key mutation
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

    /// Hook dispatch table — session-lived named turn-entry
    /// points (rc-declared prompt, startup block, plugin hooks, …).
    /// A separate namespace from both the user lexical scope and the
    /// handler stack: hooks are turn roots, never commands or
    /// readable variables.
    pub hooks: std::collections::HashMap<hooks::HookName, hooks::Hook>,

    // ── dynamic context, not attenuable ─────────────────────────────────
    /// Invocation positional args (`$args`, `$1`, …) passed on the
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
    /// `types/control.rs`.
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
/// turn's sink", and is the identity when none is installed.  Core names no
/// host runtime type — the host decides whether `emit` prints, blocks,
/// coalesces, or crosses a channel.  `Send + Sync` so a same-thread thunk body
/// may share it alongside the rest of the child subtree; a *deferred* worker
/// does not receive the live sink (it buffers into bounded deferred storage and
/// replays on `await`) so a clone of it can never define turn completion.
pub trait EventSink: Send + Sync {
    fn emit(&self, ev: &crate::serial::FOValue);
}

/// The no-op surface: a host with no structured-event rail installs `()`,
/// and an absent surface (`None`) behaves identically.
impl EventSink for () {
    fn emit(&self, _ev: &crate::serial::FOValue) {}
}

/// Shared handle to the turn-local structured-event sink.
///
/// Turn-scoped, not a
/// persistent `Shell` capability: installed only by a turn door
/// ([`Shell::run_turn`](crate::Shell::run_turn)), it has no liveness
/// role, so a clone can never decide that a turn is over.
pub type SurfaceSink = Arc<dyn EventSink>;

/// The host's answer desk for the engine→host answered channel. `enquire`
/// is blocking and short by contract (§3): a start receipt, a ledger read,
/// a confirmation, a verdict — never a long-running result.
pub trait EnquiryDesk: Send + Sync {
    /// Answer one enquiry synchronously.
    ///
    /// # Errors
    /// Returns `Err` when the host cannot answer `req` — a malformed or
    /// unrecognised request, or a failure while producing the answer.
    fn enquire(
        &self,
        req: crate::serial::FOValue,
    ) -> Result<crate::serial::FOValue, crate::types::Error>;
}

/// Shared handle to the turn-local enquiry desk. Turn-scoped like `SurfaceSink`,
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

/// Turn-local holding pen for engine-side session forks.
///
/// A desk handler is forbidden from taking the session lock (§3's
/// reentrancy law), so it cannot itself hold `&mut Shell` to fork one; the
/// fork happens in the builtin body — the one place lawfully holding
/// `&mut Shell` — via [`Shell::fork_into_nursery`], and crosses to the
/// handler as a [`NurseryId`] the handler later [`adopt`](Self::adopt)s.
/// Turn-scoped like [`Desk`]: cloned into same-thread bodies via
/// [`TurnState::inherit_from`], left `None` on a spawned worker, and
/// emptied by the turn guard at teardown so a fork nobody adopted — an
/// enquiry that was refused, or a turn that ended before the handler ran —
/// never survives past its turn.
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
/// turn boundary.
///
/// `None` outside an agent host: a bare REPL installs none,
/// so a deferred worker's surface still reaches a sink only via
/// `await`/`race`.  Carried on the turn beside [`SurfaceSink`] and cloned
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

/// This turn's authority to hand the controlling terminal to a child.
///
/// The internal (per-turn) form of the host-facing
/// [`RequestedTerminalAccess`](crate::driver::RequestedTerminalAccess): it carries
/// the extra `ExplicitLoan` state that `_ed-tui` raises mid-turn and that a host
/// cannot request at a turn door. Read by
/// [`Shell::terminal_lease`](Shell::terminal_lease), which yields the session's
/// `&TerminalLease` only when this is `Leased` or `ExplicitLoan`. `Denied` is
/// the default — the safe element — so a frame with no stated policy can never
/// reach the foreground handoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum TerminalAccess {
    /// No child/job foreground handoff in this turn (exarch tool turns, a
    /// backgrounded launch, the boot frame before the first turn).
    #[default]
    Denied,
    /// This turn may foreground terminal-bound children (the interactive REPL,
    /// a terminal-launched script).
    Leased,
    /// A within-turn elevation of a `Leased` turn: a foreground handoff fires
    /// even though stdout is a buffer, because the body (`_ed-tui`'s `fzf`)
    /// draws on `/dev/tty` and must own the foreground pgid. Set by the host
    /// loan token, never by a `TurnRequest`.
    ExplicitLoan,
}

/// The whole dynamic frame a top-level turn installs.
///
/// A turn builds one,
/// swaps it into `shell.turn`, runs, and restores the previous one on
/// teardown; the field is the invariant "the turn-local part" used to be a
/// scattered save/restore.  Same-thread bodies flow these through
/// [`TurnState::inherit_from`] / [`TurnState::return_to`]; a spawned worker
/// builds a fresh one under the durable root.
pub struct TurnState {
    /// Pipeline-stage IO: streams, value channel, terminal state, flags.
    pub(crate) io: Io,
    /// Host-installed sink for structured events surfaced by the `surface`
    /// builtin.  `None` outside a host (e.g. a bare REPL), in which case
    /// `surface` is the identity.  Cloned into thunk bodies and spawned
    /// stages — the `Arc` is shared, never folded back.
    pub(crate) surface: Option<SurfaceSink>,
    /// Host-installed destination for a deferred worker's surface batch.
    /// `None` outside an agent host (e.g. a bare REPL).  Cloned into thunk
    /// bodies and spawned workers so a nested `spawn` delivers at its own
    /// completion; like `surface` it never folds back.
    pub(crate) deferred: Option<Arc<dyn DeferredSink>>,
    /// Host-installed desk answering this turn's enquiries (§3 of the
    /// enquiry-channel ADR).  Turn-local like `surface`: cloned into
    /// same-thread bodies so a nested call enquires through the same desk,
    /// `None` outside a host that answers enquiries (a bare REPL, or any
    /// host before the migration installs its desk) and left `None` on a
    /// deferred worker — a worker outlives its turn's Report, so a worker
    /// that could enquire would break the ordering law and the deferred
    /// rail's attenuation.  Never folds back, like `surface`.
    pub(crate) desk: Option<Desk>,
    /// The turn-local nursery for engine-side session forks, crossing the
    /// reentrancy boundary the same way `desk` does (§"Core additions: the
    /// nursery" of the tools-to-builtins migration). Turn-local like `desk`:
    /// cloned into same-thread bodies, `None` outside a host that installs
    /// one, and left `None` on a deferred worker. Unlike `desk` it is not
    /// merely restored at turn teardown — the turn guard also empties it
    /// ([`Nursery::clear`]), so a fork parked and never adopted before the
    /// turn ends leaks nothing. Never folds back, like `surface`.
    pub(crate) nursery: Option<Nursery>,
    /// The turn's foreground work scope.  `signal::check` consults it between
    /// effectful steps; a foreground cancel (turn timeout, Ctrl-C) unwinds
    /// the same-thread work that shares it.  Always a descendant of
    /// [`SessionState::root`] by construction.
    pub(crate) cancel: ForegroundScope,
    /// Turn-local source cursor: every non-registry source-position field.
    /// Read when building a [`SourceLoc`](crate::diagnostic::SourceLoc);
    /// resolved at render time against [`SessionState::sources`].
    pub(crate) loc: LocationCursor,
    /// The lease governing workers this turn defers at the durable root.
    /// `None` (an interactive host) leaves a worker until `cancel`, root
    /// abort, or session exit; `Some(lease)` (an agent host) reaps a
    /// still-running worker once it has gone `lease.idle` unobserved —
    /// every `poll` and `await`/`race` sweep renews it — under the
    /// `lease.backstop` absolute ceiling no observation extends.  Supplied
    /// by the frame and flowed into same-thread bodies and spawned workers
    /// so a `spawn` nested in a thunk sees the same lease.
    pub(crate) deferred_lease: Option<WorkerLease>,
    /// Admission cap on concurrently *running* workers, enforced at the
    /// spawn door: with `Some(cap)`, a birth is refused while `cap` entries
    /// of any class are still `Running` — settled entries lingering under
    /// retention never block admission.  `None` (an interactive host)
    /// admits freely.  Flows exactly as `deferred_lease` does — into
    /// same-thread bodies and spawned workers — so a nested `spawn` cannot
    /// evade the cap its frame set.
    pub(crate) worker_cap: Option<usize>,
    /// This turn's terminal-foreground authority. Gates whether a
    /// child/job foreground handoff can borrow the session's
    /// [`TerminalLease`](crate::process::TerminalLease); see
    /// [`Shell::terminal_lease`]. Restored with the rest of the frame by the
    /// turn guard; flows into same-thread bodies (so a pipeline launched inside
    /// an `_ed-tui` loan still foregrounds) and is left `Denied` on a spawned
    /// worker.
    pub(crate) terminal_access: TerminalAccess,
}

impl TurnState {
    /// Same-thread child flow-in: clone the byte sinks, move the read-once
    /// stdin out of the parent, and share the foreground scope and source
    /// cursor.  Mirror of [`Self::return_to`].
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.io.inherit_from(&mut parent.io);
        self.surface = parent.surface.clone();
        self.deferred = parent.deferred.clone();
        self.desk = parent.desk.clone();
        self.nursery = parent.nursery.clone();
        self.cancel = parent.cancel.clone();
        self.loc = parent.loc.clone();
        self.deferred_lease = parent.deferred_lease;
        self.worker_cap = parent.worker_cap;
        // Terminal access flows in so a pipeline launched inside an `_ed-tui`
        // loan (or any same-thread body of a Leased turn) sees the parent's
        // authority. It does not flow back in `return_to`: the parent retains
        // its own access (it set the loan and will end it).
        self.terminal_access = parent.terminal_access;
    }

    /// Same-thread child flow-out: return the read-once stdin to the parent
    /// so sibling calls see the unconsumed pipe.  The cursor and foreground
    /// do not flow back — the asymmetry is the point.
    pub fn return_to(&mut self, parent: &mut Self) {
        self.io.return_to(&mut parent.io);
    }
}

/// State that outlives every turn's teardown.
pub struct SessionState {
    /// The session's durable cancel root.  Detached workers (`spawn`,
    /// `watch`, `par`) parent under this rather than under the swappable
    /// foreground [`TurnState::cancel`], so a foreground cancel never reaches
    /// them.  Only a [`RootAbort`](crate::process::CancelCause::RootAbort) on
    /// the root, or a cancel on the worker's own scope, stops such a worker.
    pub(crate) root: DurableRoot,
    /// Whether this session's turn doors publish its scopes into the
    /// process-global signal slots — the reach-in path for OS signals and a
    /// front-end's cancel key.  The slots' save/restore-previous discipline
    /// is sound only for LIFO publication on a single thread, so at most one
    /// session per process may publish: the signal-facing one.  `true` at
    /// construction; a forked child session ([`Shell::fork_session`]) never
    /// publishes — its host stops it through a cancel handle on
    /// [`Self::root`] ([`Shell::cancel_handle`]), not through signals.
    pub(crate) publishes_signal_slots: bool,
    /// Durable source registry, keyed by [`FileId`](crate::source::FileId).
    /// A turn resets and seeds it at turn start, module loads append to it,
    /// and hosts read it after the turn returns to render runtime errors.
    /// Durable across the teardown of [`TurnState::loc`], not across all
    /// future turns.  Skipped by IPC/serde — it is render state, not mobile.
    pub(crate) sources: SourceDb,
    /// Exit-code hint table — loaded once at startup from the data directory.
    pub(crate) exit_hints: crate::exit_hints::ExitHints,
    /// Host-installed command dispatch table.  Builtin bodies are Rust fn
    /// pointers or captured host closures — process-local dispatch state, not
    /// serialised ral values, so the receiver of a wire mobile supplies its
    /// own rather than shipping it in a `WireMobile`.
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
    /// installed turn's [`TerminalAccess`] permits.
    pub(crate) terminal_lease: Option<crate::process::TerminalLease>,
}

/// Host-local scratch whose members carry their own flow rules — not a
/// lifetime category, the residue left once turn and session state are named.
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
    /// agent; `Shell::spawn_thread` shares it into a spawned worker's own
    /// shell (so a nested `spawn` registers alongside its parent), but a
    /// sub-agent fork starts with a fresh one.
    pub(crate) workers: WorkerRegistry,
    /// The binding-lease ledger (`types/shell/bindings.rs`,
    /// `decisions/260629_agent-binding-reaping`): inert until a host arms
    /// it, then tracks the idle-call age of every non-baseline top-level
    /// name so an agent host can prune scratch that has gone unused for too
    /// long. Unlike [`Self::workers`] this needs no lock — it has exactly
    /// one writer, the thread that owns `&mut Shell` for every turn, install,
    /// and prune (verified in `bindings.rs`'s module doc). One ledger per
    /// `Shell`, i.e. one per agent; a sub-agent fork or spawned worker starts
    /// with a fresh, inert one — nothing shares it, nothing flows back.
    pub(crate) bindings: BindingLedger,
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
/// A field either moves as a turn
/// ([`TurnState`]), survives a turn ([`SessionState`]), crosses evaluation
/// boundaries ([`Mobile`]), or stays as host scratch ([`LocalState`]).
///
/// Every field is `pub(crate)`: the partition that encodes turn safety,
/// capability attenuation, and mobile framing is core's invariant, not a
/// public API a host can reach past.  Hosts drive a session through the
/// intent verbs — the [`mod@host`] accessors and the scope/context verbs —
/// each of which is a complete operation rather than a raw field poke.
/// Durability is core's own: the turn door ([`Shell::run_turn`])
/// checkpoints and rolls back the [`Mobile`] around every turn.
pub struct Shell {
    pub(crate) mobile: Mobile,
    pub(crate) turn: TurnState,
    pub(crate) session: SessionState,
    pub(crate) local: LocalState,
}

impl Shell {
    /// Construct an [`Error`] located at the current source position.
    pub fn err(&self, msg: impl Into<String>, status: i32) -> Error {
        Error::new(msg, status).at_loc(self.turn.loc.source_loc(0))
    }

    /// Like [`Self::err`], with an additional hint.
    pub fn err_hint(&self, msg: impl Into<String>, hint: impl Into<String>, status: i32) -> Error {
        Error::new(msg, status)
            .at_loc(self.turn.loc.source_loc(0))
            .with_hint(hint)
    }

    /// Forward any structured-event [`Value`] onto this turn's surface sink, if
    /// one is installed; inert when none is.  The public door host builtins use
    /// to surface their own events — a Rust exarch `edit` raising its write
    /// card, a `grep-files` announcing its search — since `turn.surface` is `pub(crate)`
    /// and so unreachable from a host crate.  Core names no event shape here: the
    /// caller hands a fully-formed `Value`, encoded once at this door into the
    /// first-order [`FOValue`](crate::serial::FOValue) the sink actually
    /// carries.  A value that is not first-order (a closure, a handle) cannot
    /// cross the rail; core has no host vocabulary to report that in (an
    /// exarch `Kind::SystemNote` would leak host concepts into core), so the
    /// drop is a `dbg_trace`, not a silent no-op.
    pub fn surface(&self, ev: &Value) {
        match crate::serial::FOValue::try_from(ev) {
            Ok(fo) => {
                if let Some(sink) = self.turn.surface.as_ref() {
                    sink.emit(&fo);
                }
            }
            Err(_) => {
                crate::dbg_trace!("surface", "dropping non-first-order surface value");
            }
        }
    }

    /// Put one enquiry to this turn's host desk and block for the answer.
    /// The absent-desk error is the honest answer of a host that answers none
    /// (the bare REPL, and any host before the migration installs its desk).
    ///
    /// # Errors
    /// Returns `Err` if no desk is installed on this turn, or if the
    /// installed desk's [`EnquiryDesk::enquire`] returns one.
    pub fn enquire(
        &self,
        req: crate::serial::FOValue,
    ) -> Result<crate::serial::FOValue, crate::types::Error> {
        match self.turn.desk.as_ref() {
            Some(desk) => desk.enquire(req),
            None => Err(self.err("this host answers no enquiries", 1)),
        }
    }

    /// Fork this shell ([`Self::fork_session`]) and park the fork in this
    /// turn's nursery, returning the [`NurseryId`] a desk handler — barred
    /// by the reentrancy law from holding `&mut Shell` itself — later
    /// redeems with [`Nursery::adopt`]. The absent-nursery error is the
    /// honest answer of a host that adopts no forked sessions (the bare
    /// REPL, and any host before it installs a nursery).
    ///
    /// # Errors
    /// Returns `Err` if no nursery is installed on this turn.
    pub fn fork_into_nursery(&self) -> crate::types::Settled<NurseryId> {
        match self.turn.nursery.as_ref() {
            Some(nursery) => Ok(nursery.park(self.fork_session())),
            None => Err(crate::types::Break::Error(
                self.err("this host adopts no forked sessions", 1),
            )),
        }
    }

    /// Emit a structural I/O event onto this turn's surface sink, if one is
    /// installed.  This is the single door through which every redirect
    /// read/write and every exec completion announces itself to the host;
    /// with no surface installed it is inert.  The event is a plain
    /// [`Value::Map`] whose shape (`{io: …, …}`) the host decodes — core
    /// names no card type.
    pub(crate) fn emit_io(&self, ev: &Value) {
        self.surface(ev);
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

    /// Install a script context: set the active script name on both the
    /// cursor's `script` and `call_site.script`, register the source text in
    /// the session registry, and make the registered source the active one
    /// (`current` + the `source` cache used for byte-span → (line, col)
    /// resolution).  All of these move together — diverging them produced
    /// span-to-position drift inside loaded modules (the parent's `source`
    /// was consulted for the child's spans).
    ///
    /// One-shot install: callers that need restore-on-exit semantics should
    /// use [`crate::builtins::modules::ScriptContextGuard`] instead.
    pub fn install_script_context(&mut self, name: String, text: &str) {
        let source = Source::from_text(&name, text);
        self.turn.loc.current = self.session.sources.register(source.clone());
        self.turn.loc.script = name.clone();
        self.turn.loc.call_site.script = name;
        self.turn.loc.source = Some(source);
    }

    /// Install a top-level turn's script context after clearing the session
    /// registry, so the prior turn's sources are reclaimed before this turn
    /// registers its own.  Used only at the interactive turn boundary (REPL /
    /// shell); module loads stay on
    /// [`install_script_context`](Self::install_script_context), which appends
    /// so a turn's `source`d modules remain resolvable when its error renders
    /// at end of turn.
    pub fn install_root_context(&mut self, name: String, text: &str) {
        self.session.sources.reset();
        self.install_script_context(name, text);
    }

    /// Install a script context using an already-minted [`FileId`] and its
    /// source text, without registering into the session registry — the
    /// seam a re-exec'd pipeline-stage child uses to resolve its spans
    /// against the exact source and file identity its parent already
    /// registered, rather than minting a second, differently-numbered copy
    /// in its own (empty) session registry.
    pub(crate) fn install_remote_context(&mut self, name: String, file: FileId, text: &str) {
        let source = Source::from_text(&name, text);
        self.turn.loc.current = file;
        self.turn.loc.script.clone_from(&name);
        self.turn.loc.call_site.script = name;
        self.turn.loc.source = Some(source);
    }

    /// Resolve the seven pseudo-variables (`$env`, `$args`, `$script`,
    /// `$nproc`, `$CWD`, `$STATUS`, `$USER`).  These are computed on
    /// demand rather than stored in scope, so [`Self::lookup_value_name`]
    /// consults them after lexical scope and before the builtin table.
    /// Any other name returns `None`.
    pub fn pseudo_var(&self, name: &str) -> Option<Value> {
        match name {
            "env" => {
                // PWD / OLDPWD are shell-cwd-derived, not
                // env-overrides: they live on `context.cwd` and
                // change every `cd`.  Drop them at the source so
                // `$env` reads as the rule.
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
            "args" => Some(Value::list(
                self.mobile
                    .context
                    .args
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            )),
            // $script: path of the currently-executing file.  Empty
            // in the REPL, under `-c`, and during prelude loading.
            "script" => match self.turn.loc.script.as_str() {
                "" | "-c" | "<prelude>" => None,
                s => Some(Value::String(s.to_string())),
            },
            "nproc" => Some(Value::Int(std::thread::available_parallelism().map_or(
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
        match self.turn.io.stdout.write_all(bytes) {
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
            .fork_into_nursery()
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

    /// A nursery installed on the turn round-trips a session fork:
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
        shell.turn.nursery = Some(nursery.clone());

        let id = shell.fork_into_nursery().expect("a nursery is installed");
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
