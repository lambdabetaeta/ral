//! The uniform agent node: canonical event log, persistent shell, capability
//! set, an owned hot-swappable provider, and the shared turn driver every node
//! runs.  An exarch run is a *fleet* of these arranged in a tree
//! ([[decisions/260624_uniform-agent-nodes]]); the [`Fleet`](crate::fleet::Fleet)
//! holds what is shared (the registry, the one event bus, the focused-agent
//! handle, whether a human is attached), and each `Agent` is one node.
//!
//! [`Agent::apply`] runs one round-trip against the provider;
//! [`Agent::drive`] is the one loop — every node alike — that pulls
//! turn-boundary messages from the node's own [`Inbox`], runs one `apply`
//! per message, and turns a nudge-worthy outcome into a self-posted
//! [`InboxMsg::Nudge`].  No node is privileged by special-case code; the
//! distinctions reduce to *position*: the parent-less **trunk** publishes its
//! cancel token for the OS-signal path, holding `reply` falls out of the
//! `interactive` predicate (via the agent's tool view from `tools_for`), and
//! parking out of `interactive`/`focus` via `park_mode`.  A child's single
//! result is delivered up its parent's mailbox by the spawn site, not
//! here, so `drive` itself is identical for all.

use crate::bootstrap::Scratch;
use crate::bus::{
    AgentId, AgentOutcome, Emitter, Inbox, InboxMsg, Kind, Mailbox, ParkMode, Turn,
    WORKER_PANIC_PREFIX,
};
use crate::cancel;
use crate::digest::{
    AGENT_REPLY_CAP, COMPACT_THRESHOLD, OPAQUE_CAP, SUMMARY_CAP_FALLBACK_TOKENS, clip,
    compaction_due, render, suffix_keep_budget, summary_cap_tokens,
};
use crate::event::{AgentLog, QuiesceReason, ToolResult as SessionToolResult};
use crate::fleet::NO_FOCUS;
use crate::nudge;
use crate::provider::{
    CutShort, Provider, ProviderError, ProviderKind, StepOut, StopReason, ToolCall,
};
use crate::shell_eval;
use crate::transcript::Transcript;
use ral_core::Shell;
use ral_core::transport::Transport;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub struct Agent {
    pub id: AgentId,
    pub(crate) system: String,
    log: AgentLog,
    /// This session's operational trace (`transcript.jsonl`), written at the
    /// emit seam through the [`Emitter`](crate::bus::Emitter) the frontend
    /// builds from [`Self::transcript`].  The sibling of [`Self::log`]: the
    /// model view (`events.json`) is what the model saw; the transcript is
    /// what the agent did.
    transcript: Transcript,
    transport: ral_core::transport::IdentityTransport,
    wire: Option<ral_core::transport::WireTransport>,
    caps: ral_core::types::Capabilities,
    /// This agent's parent, or `None` for the **trunk**.  The sole structural
    /// distinction: the trunk publishes its cancel token for the OS-signal path
    /// and, when `interactive`, converses (withholding `reply` and parking
    /// indefinitely); every asymmetry is read from this field plus
    /// `interactive`/`focus`, never an `is_root` branch.
    parent: Option<AgentId>,
    /// Remaining spawn budget, decremented by one at each [`Self::fork`].
    /// Zero clears [`Gate::Spawns`](crate::tools::Gate::Spawns) from this
    /// agent's [`tools_for`](crate::tools::tools_for) view, so a delegation
    /// chain terminates by tool absence rather than recursing forever. The
    /// trunk starts at [`SPAWN_FUEL`]; a fork hands its child one less.
    fuel: u32,
    /// This agent's own hot-swappable provider.  A `/model` on the focused
    /// agent swaps *its* handle alone; a `fork` seeds the child's own handle
    /// from this one's current provider, so neither disturbs the other.  The
    /// drive loop reads it once per turn.
    provider: ProviderHandle,
    /// The fleet's focused-agent handle, shared with the frontend and every
    /// node.  The drive loop reads it in [`Self::park_mode`]: a focused agent
    /// parks for the human like the conversing trunk.  Off the TUI it holds
    /// [`NO_FOCUS`] and never changes.
    focus: Arc<AtomicU64>,
    /// Whether a human is attached to the fleet (the TUI).  With `parent =
    /// None` it makes this the *conversing* trunk; off the TUI (`false`) the
    /// trunk is a one-shot headless agent.  Inherited by every fork.
    interactive: bool,
    /// This agent's own inbox — the consumer side of its message queue.
    /// [`Agent::drive`] pulls turn-boundary deliverables from here;
    /// [`Agent::apply`] drains tool-boundary steering from it mid-round-trip.
    /// Self-nudges and self-armed wakeups land here; a child's result lands in
    /// its *parent's* inbox, never reaching across into a sibling's.
    inbox: Inbox,
    /// Per-agent nudge state — the retry budget and the one-shot completion
    /// gates.  Its latches reset on a genuine turn-boundary message, never on
    /// a self-[`InboxMsg::Nudge`], so a multi-step nudge sequence runs to
    /// completion within one turn.
    nudges: nudge::Registry,
    /// This agent's cancellation token — one sticky token for its life,
    /// registered in the fleet so the subtree cascade (`agent_cancel`, the
    /// ceiling, an Esc on the focused subtree) always reaches the live turn.
    /// The drive loop [`reset`](cancel::Token::reset)s its flag at each genuine
    /// turn boundary, so an Esc on one turn never bleeds into the next; the
    /// trunk additionally [`publish`](cancel::publish)es it for OS signals.
    cancel: cancel::Token,
    /// This agent's view into the tool registry — the single source of truth
    /// read by `provider.complete` (advertisement) and [`Self::stage`]
    /// (dispatch).  Membership is the gate: a tool absent here is neither
    /// advertised nor callable.  Every agent spawns; the views differ only by
    /// `reply` (withheld from the conversing trunk) and the self-wakeup family
    /// (the `--allow-schedule` grant).  See [`crate::tools::tools_for`].
    tools: Vec<&'static dyn crate::tools::Tool>,
    /// A returning agent's staged return value, set by the `reply` tool's
    /// dispatch (via [`Self::set_reply`]) and lifted by [`Self::apply`] into a
    /// [`TurnOutcome::Replied`] once the current tool-call batch finishes
    /// draining — never mid-batch, so the session reaches a clean boundary
    /// with every `call_id` answered.  Held as the faithful
    /// [`serde_json::Value`] the model passed, so the consuming edge renders it
    /// (a null or empty value settles as
    /// [`AgentOutcome`](crate::bus::AgentOutcome)`::Empty`).  Every returning
    /// agent sets it — a peer and the headless root; `reply` is withheld only
    /// from the interactive root.
    reply: Option<serde_json::Value>,
    /// Durable [`MobileSnapshot`](ral_core::types::MobileSnapshot) of the
    /// shell as of the last clean tool-call boundary.  Refreshed inside
    /// `run_shell` right before each dispatch, so it always holds the dynamic
    /// context completed calls left behind.  Read by [`Self::drive`] after a
    /// caught worker panic to roll the panicking call's dynamic-context
    /// effects back; it lives on `Agent` precisely so it survives the
    /// `catch_unwind` boundary.
    durable: ral_core::types::MobileSnapshot,
    /// The **fleet's** agent registry — one shared map, cloned to every node,
    /// so "all live agents" is its contents.  Every agent registers itself here
    /// and registers its own children, so the registry is the spawn tree at any
    /// depth.  Survives `/clear`: [`clear_subtree`](crate::agent_registry::AgentRegistry::clear_subtree)
    /// bumps the generation and reaps the focused agent's descendants, so a
    /// worker that settles after the clear drops its result.
    pub(crate) agents: crate::agent_registry::AgentRegistry,
    /// Live scheduled wakeups (cron / after).  A peer may self-schedule (it
    /// posts wakeups into its own inbox), so this is no longer root-only; it
    /// is still gated by the [`Gate::Schedules`](crate::tools::Gate) axis
    /// (`--allow-schedule`), which decides whether the self-wakeup tools are in
    /// the agent's view at all.
    pub(crate) schedules: crate::schedule::ScheduleRegistry,
    /// Input tokens the model saw on the most recent completion — the live
    /// numerator for the context-pressure compaction trigger
    /// ([`Self::compact`]), the same signal the TUI's fidelity gauge reads.
    /// Set from each step's usage; reset to `0` on `/clear`.
    last_input: u64,
    /// Current pinned-state digests (`key → one-line summary`), mirrored from
    /// the surface sink as the model `` `pin ``s/`` `unpin ``s state.  Pins
    /// otherwise flow past the session to the frontend; this small copy lets
    /// the periodic [`nudge`] reminder describe what the model has pinned.
    /// Cleared on `/clear` like the rest of the session's generation state.
    pins: shell_eval::PinDigests,
    /// The agent's ral-call epoch: incremented once at the top of every
    /// [`Self::run_shell`] call — success or error alike, a failed eval is
    /// still a call.  The integer clock every per-call ledger reads: the
    /// settled-worker retention sweep today
    /// ([`advance_worker_epoch`](ral_core::Shell::advance_worker_epoch)),
    /// the binding leases of `decisions/260629_agent-binding-reaping` next,
    /// on this same counter.  Starts at 0, and a fork's child starts its
    /// own at 0; deliberately NOT reset by `/clear` — the registry is
    /// emptied there anyway, and a monotone counter is simpler to reason
    /// about than one that rewinds.
    ral_epoch: u64,
}

/// Outcome of one [`Agent::apply`].  Degenerate cases (`Empty`,
/// `Stopped`) become nudges; `Cancelled` and `Capped` do not; hard
/// failures travel through [`ProviderError`].
#[derive(Debug)]
pub enum TurnOutcome {
    Complete(String),
    /// A returning agent called `reply`: the carried payload is its deliberate
    /// return value, a faithful [`serde_json::Value`] (a JSON null when the
    /// argument was absent or null).  Carried as a value, not pre-rendered
    /// text, so each consumer renders it at its own edge — prose for a model
    /// parent, the structure itself for the headless harness.  Distinct from
    /// [`Self::Complete`] precisely so the nudge layer can tell "already
    /// returned" from "stopped without returning" and not re-nudge an agent
    /// that replied.  Terminal: it ends the drive loop.
    Replied(serde_json::Value),
    Empty,
    Stopped {
        reason: String,
    },
    Cancelled,
    /// The round-trip loop hit [`MAX_STEPS`] without the model ever
    /// emitting a tool-call-free reply.  Terminal: it carries no nudge
    /// (re-driving would just spend another `MAX_STEPS`).
    Capped,
}

/// Hard ceiling on provider round-trips in one [`Agent::apply`].  The
/// interactive frontend has Esc to halt a runaway turn; headless and
/// autonomous sub-agent runs have nothing, so a model that keeps
/// emitting tool calls would loop until the token budget or the wall
/// runs out.  Bounding the step count keeps benchmark and headless runs
/// terminating.  Generous enough that no genuine interactive turn ever
/// reaches it.
const MAX_STEPS: u32 = 250;

/// Spawn budget the trunk starts with; each [`Agent::fork`] hands its child
/// one less, and a `fuel == 0` agent loses the spawn tools from its view
/// ([`tools_for`](crate::tools::tools_for)). Covers a short legitimate
/// delegation chain — `/discuss`'s chair and partner, a couple of hops of
/// sub-agent fan-out — while bounding a runaway spawn-calling loop to a fixed
/// number of generations instead of the process exhausting threads.
const SPAWN_FUEL: u32 = 3;

pub(crate) fn fresh_id() -> AgentId {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// A shared, swappable handle to the active provider.  The drive loop reads
/// the current provider once per turn through [`Self::current`]; a `/model`
/// switch on the frontend's UI thread swaps it through [`Self::swap`].  Live
/// provider replacement across the UI / drive thread boundary needs a shared
/// cell rather than an owned `Arc` the worker would hold privately; a `Mutex`
/// suffices (the cell is touched at most once per turn and on the rare
/// switch).  A peer wraps a *snapshot* of the provider at spawn, so a later
/// root `/model` never disturbs an already-running child.
#[derive(Clone)]
pub struct ProviderHandle(Arc<Mutex<Arc<Provider>>>);

impl ProviderHandle {
    pub fn new(provider: Arc<Provider>) -> Self {
        Self(Arc::new(Mutex::new(provider)))
    }

    /// The provider in force for the next turn.
    pub fn current(&self) -> Arc<Provider> {
        self.0.lock().expect("provider handle poisoned").clone()
    }

    /// Replace the active provider (a `/model` switch).  Takes effect on the
    /// drive loop's next turn; an in-flight turn finishes on the provider it
    /// started with, and any running peer keeps its own snapshot.
    pub fn swap(&self, provider: Arc<Provider>) {
        *self.0.lock().expect("provider handle poisoned") = provider;
    }
}

/// What [`Agent::drive`] should do after handing a session-affecting slash
/// command to [`Control`].
pub enum ControlFlow {
    Continue,
    Quit,
}

/// The frontend hook the drive loop calls for a session-affecting slash
/// command ([`Turn::Command`]) drained at the turn boundary — the drive thread
/// owns the session the command mutates, so it cannot run on the UI thread.
/// Only the non-interactive session commands route here (`/clear`, `/compact`,
/// `/discuss`, `/quit`); `/model` swaps the [`ProviderHandle`] directly on the
/// UI thread, and view-only commands (`/help`, `/copy`, …) are handled
/// frontend-side.
/// Off the TUI there are no such commands, so [`NoControl`] handles none.
pub trait Control {
    /// Run `raw` (a slash-command line) against the session the drive loop
    /// owns, returning whether the loop should quit.
    fn command(&mut self, raw: &str, session: &mut Agent, emit: &Emitter) -> ControlFlow;
}

/// The no-op control for drivers with no slash commands (headless, peers,
/// tests): a [`Turn::Command`] should never reach them, so it is a no-op.
pub struct NoControl;

impl Control for NoControl {
    fn command(&mut self, _raw: &str, _session: &mut Agent, _emit: &Emitter) -> ControlFlow {
        ControlFlow::Continue
    }
}

/// The trunk's registry title.  The trunk lists its descendants, never itself,
/// so this is never shown — it only fills the entry the frontend looks up by id
/// for the trunk's mailbox and provider.
const TRUNK_TITLE: &str = "main";

/// The launch configuration threaded into [`Agent::assemble`] — bundled so
/// the one constructor reads at the call site rather than as a wall of bare
/// fields.
struct Build {
    system: String,
    caps: ral_core::types::Capabilities,
    shell: Shell,
    log: AgentLog,
    parent: Option<AgentId>,
    fuel: u32,
    provider: ProviderHandle,
    focus: Arc<AtomicU64>,
    interactive: bool,
    tools: Vec<&'static dyn crate::tools::Tool>,
    /// The fleet's shared registry — fresh for the trunk, the parent's clone
    /// for a fork — so every node registers into one map.
    agents: crate::agent_registry::AgentRegistry,
}

impl Agent {
    fn assemble(b: Build) -> io::Result<Self> {
        let Build {
            system,
            caps,
            mut shell,
            log,
            parent,
            fuel,
            provider,
            focus,
            interactive,
            tools,
            agents,
        } = b;
        seed_session_dir(&mut shell, &log);
        let durable = shell.mobile_snapshot();
        #[cfg(unix)]
        let wire = if std::env::var("RAL_WIRE").is_ok() {
            Some(
                ral_core::transport::WireTransport::new()
                    .expect("RAL_WIRE: failed to create WireTransport"),
            )
        } else {
            None
        };
        #[cfg(not(unix))]
        let wire: Option<ral_core::transport::WireTransport> = None;
        let transport = ral_core::transport::IdentityTransport::new(shell);
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        transport.attach(
            ral_core::transport::TerminalEndpoint {
                lease: None,
                state: ral_core::io::TerminalState::probe_from_env().1,
            },
            cwd,
            std::path::PathBuf::from(&home),
            None, // rc_path
        );
        // Every agent — the trunk and each fork, both modes — owns its trace,
        // born in the same dir as its `events.json`.
        let transcript = Transcript::create(&log.dir().join("transcript.jsonl"))?;
        Ok(Self {
            id: log.id(),
            system,
            log,
            transcript,
            transport,
            wire,
            caps,
            parent,
            fuel,
            provider,
            focus,
            interactive,
            inbox: Inbox::new(),
            nudges: nudge::Registry::new(),
            cancel: cancel::Token::new(),
            tools,
            reply: None,
            durable,
            agents,
            schedules: crate::schedule::ScheduleRegistry::new(),
            last_input: 0,
            pins: Default::default(),
            ral_epoch: 0,
        })
    }

    /// Register this agent in the fleet registry — under its `parent` (`None`
    /// for the trunk).  The trunk calls it at construction so the frontend sees
    /// it from the start; a child is registered by its spawn site (which also
    /// arms the ceiling).  Idempotent enough: a re-register overwrites in place.
    fn register_self(&self) {
        self.agents.register(
            self.id,
            self.parent,
            TRUNK_TITLE.to_string(),
            self.log.dir().to_path_buf(),
            self.cancel.clone(),
            self.parent.map(|_| self.eval_root()),
            self.inbox.mailbox(),
            self.provider.clone(),
        );
    }

    fn replace_shell(&mut self, shell: Shell) {
        let mut shell = shell;
        seed_session_dir(&mut shell, &self.log);
        self.durable = shell.mobile_snapshot();
        self.transport = ral_core::transport::IdentityTransport::new(shell);
        #[cfg(unix)]
        if std::env::var("RAL_WIRE").is_ok() {
            self.wire = Some(
                ral_core::transport::WireTransport::new()
                    .expect("RAL_WIRE: failed to create WireTransport"),
            );
        }
    }

    /// The trunk — the parent-less root of a fresh fleet.  Creates the fleet's
    /// shared registry and focused-agent handle, wraps the initial `provider`,
    /// and registers itself, so the frontend builds its [`Fleet`](crate::fleet::Fleet)
    /// by reading these handles back.  `interactive` makes it the *conversing*
    /// trunk (TUI); off it, a one-shot headless trunk.
    #[allow(clippy::too_many_arguments)] // launch config, threaded once at boot
    pub(crate) fn root(
        system: String,
        caps: ral_core::types::Capabilities,
        scratch: &Scratch,
        run_dir: &std::path::Path,
        model: &str,
        provider_label: &str,
        allow_schedule: bool,
        interactive: bool,
        provider: Arc<Provider>,
    ) -> io::Result<Self> {
        let shell = boot_root_shell(scratch);
        let sessions_root = run_dir.join("sessions");
        let id = fresh_id();
        let log = AgentLog::root(&sessions_root, id, model, provider_label, system.len())?;
        let agent = Self::assemble(Build {
            system,
            caps,
            shell,
            log,
            parent: None,
            fuel: SPAWN_FUEL,
            provider: ProviderHandle::new(provider),
            // The focus handle is fleet-shared; off the TUI it never moves, so
            // it starts at the no-focus sentinel.  The conversing trunk parks
            // via `interactive`, not via focus, so the trunk need not name
            // itself here.
            focus: Arc::new(AtomicU64::new(NO_FOCUS)),
            interactive,
            // The interactive trunk converses and never returns, so it withholds
            // `reply`; a headless trunk is a returning agent.  Either way the
            // session's self-wakeup grant shapes the view.
            tools: crate::tools::tools_for(!interactive, allow_schedule, SPAWN_FUEL > 0),
            agents: crate::agent_registry::AgentRegistry::new(),
        })?;
        agent.register_self();
        Ok(agent)
    }

    /// The fleet's focused-agent handle, shared with the frontend and every
    /// fork — the TUI binds its `App` to this and mutates it on `TAB`.
    pub(crate) fn focus_handle(&self) -> Arc<AtomicU64> {
        self.focus.clone()
    }

    /// Whether a human is attached to the fleet.
    pub(crate) fn interactive(&self) -> bool {
        self.interactive
    }

    /// This agent's own provider handle — read by the spawn site to register it
    /// (so a `/model` on the child's tab swaps the child alone) and by the
    /// frontend to build the [`Fleet`](crate::fleet::Fleet).
    pub(crate) fn provider_handle(&self) -> ProviderHandle {
        self.provider.clone()
    }

    /// The provider in force for this agent's next turn — for a slash command
    /// (`/compact`) the drive loop runs against the agent's own handle.
    pub(crate) fn current_provider(&self) -> Arc<Provider> {
        self.provider.current()
    }

    pub(crate) fn clear(&mut self, scratch: &Scratch) -> io::Result<()> {
        // `boot_root_shell` owns the signal ceremony: stale ral interrupts
        // are discarded before embedded library evaluation, and the exarch
        // cancel chain is restored over ral's freshly installed handlers.
        let shell = boot_root_shell(scratch);
        self.log.clear(self.system.len())?;
        // Cancel the outgoing shell's registered workers before it is
        // replaced — `/clear` outranks every lease, the durable class
        // included — while it is still unambiguously *this* shell reachable
        // through the transport; once `replace_shell` swaps the transport
        // below, there is no way back to it.
        self.transport.shell_mut().shell.cancel_workers();
        self.replace_shell(shell);
        // A rebuilt context starts empty: drop the stale pressure reading so
        // the next turn's usage sets it afresh.
        self.last_input = 0;
        // Retire the subtree, then empty the queue — in that order.  The
        // generation bump must precede the inbox sweep so a worker settling
        // mid-clear either lands its result before the sweep (and is swept
        // with the rest) or posts after the bump (and is dropped by
        // generation admission); no interleaving lets a pre-clear result
        // survive into the rebuilt context.  The workers themselves are
        // already cancelled above, on the shell being retired; what the
        // generation bump guards against is a straggler's late boundary
        // flush (`InboxBoundary`, shell_eval.rs) reaching the rebuilt
        // context before it notices the cancellation.  This agent itself
        // stays registered — `/clear` rebuilds its context, it does not
        // tear it down.
        self.agents.clear_subtree(self.id);
        // Schedules are producers too: disarm them before the sweep, for the
        // same reason.  A rebuilt agent carries no pending wakeups.
        self.schedules.clear();
        // Drop every queued message: a rebuilt context carries neither stale
        // user steering nor non-human deliveries across the clear.
        self.inbox.clear();
        // A rebuilt context wears no pinned state: the frontend wipes its
        // register on `/clear`, so the session's mirror must follow.
        if let Ok(mut m) = self.pins.lock() {
            m.clear();
        }
        Ok(())
    }

    pub(crate) fn fork(&self, caps: ral_core::types::Capabilities) -> io::Result<Agent> {
        // The child is an independent fork of the parent: it snapshots the
        // parent's scope (prelude, agent library, accumulated bindings),
        // dynamic context (cwd, env, grants), and installed builtin table (the
        // host's `view-text`/`grep-files`/`edit` and the rest), and starts
        // fresh in control counters and per-agent state — its own inbox, its own
        // (fresh) cancellation token, no terminal authority, no flow-back.
        // Core owns the flow matrix, so the builtin table can't be silently
        // dropped here the way a hand-copied field once was.  Its capabilities
        // are supplied by the spawn site: the parent's verbatim, or the
        // parent's narrowed to a requested base (`parent ⊓ base`).
        let shell = self.transport.shell_mut().shell.fork_session();
        let child_id = fresh_id();
        let log = self.log.fork(child_id, self.system.len())?;
        // One less than the parent's — the child's ceiling on how many more
        // generations it may itself spawn before the tools disappear.
        let fuel = self.fuel.saturating_sub(1);
        Self::assemble(Build {
            system: self.system.clone(),
            caps,
            shell,
            log,
            // The spawning agent is the child's parent — the tree edge that
            // makes the child a node at this depth and carries the cascade.
            parent: Some(self.id),
            fuel,
            // The child seeds its own handle from the parent's current provider,
            // so a later `/model` on either never disturbs the other.
            provider: ProviderHandle::new(self.provider.current()),
            // The fleet's focus and human-attachment are shared, not re-derived:
            // a child becomes parkable the instant the human `TAB`s to it.
            focus: self.focus.clone(),
            interactive: self.interactive,
            // Every agent spawns while its fuel lasts; a sub-agent returns
            // through `reply`.  Self-scheduling authority is inherited: a
            // `--allow-schedule` trunk grants its descendants the same right
            // to wake themselves.
            tools: crate::tools::tools_for(true, self.grants_schedule(), fuel > 0),
            // One shared fleet registry: the child registers into the same map,
            // so the tree is whole at any depth.
            agents: self.agents.clone(),
        })
    }

    /// A child that inherits this agent's model-visible context.  The launch
    /// prompt is seeded through the child's inbox by the spawn site, so it
    /// enters the log through the same turn path as any other user prompt.
    /// The shell/provider/capability fork is identical to [`Self::fork`];
    /// only the model log is seeded differently.
    pub(crate) fn fork_remembering(
        &self,
        caps: ral_core::types::Capabilities,
    ) -> io::Result<Agent> {
        let mut child = self.fork(caps)?;
        child
            .log
            .import_context(self.log.inherited_context_messages())
            .map_err(io::Error::other)?;
        Ok(child)
    }

    pub(crate) fn log_dir(&self) -> &std::path::Path {
        self.log.dir()
    }

    /// A handle to this session's [`Transcript`] — for the frontend to attach
    /// to the emitter it drives this session through (the root) and for the
    /// spawn site to hand a forked child its own trace.
    pub(crate) fn transcript(&self) -> Transcript {
        self.transcript.clone()
    }

    /// This session's mailbox — for the `schedule` tool to arm wakeups into
    /// its *own* inbox, and the spawn site to capture the parent's box as a
    /// child's upward result edge.
    pub(crate) fn mailbox(&self) -> Mailbox {
        self.inbox.mailbox()
    }

    /// A handle to this session's inbox — for a frontend to build the bus
    /// (whose emitters mint mailboxes onto this same queue) over the session's
    /// own queue, so producers and the drive loop share one inbox.
    pub(crate) fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    /// This session's cancellation token (read by the spawn site to register
    /// a peer's sticky token in the parent's [`Self::agents`]).
    pub(crate) fn cancel_token(&self) -> &cancel::Token {
        &self.cancel
    }

    /// The eval-layer cancel handle on this agent's own `Shell` — registered
    /// alongside the [`Token`](cancel::Token) so the subtree cascade can
    /// unwind an in-flight `ral` eval, not merely flag a drive loop that
    /// only reads its token between steps.
    pub(crate) fn eval_root(&self) -> ral_core::process::DurableRoot {
        self.transport.shell_mut().shell.cancel_handle()
    }

    /// Seed this session's inbox with its launch prompt — the spawn site calls
    /// it once on a freshly forked child, then drops its handle, so the only
    /// downward edge is this one write.
    pub(crate) fn seed(&self, prompt: String) {
        self.inbox.push_user(prompt);
    }

    /// Build a trunk against a throwaway sessions root under `dir`, with
    /// default (unrestricted) capabilities, a baked shell, and an empty
    /// scripted provider (tests that drive set their own).  The harness in
    /// `tests/` uses this to drive [`Self::apply`] and [`Self::drive`] through
    /// a [`Provider::scripted`] backend.  Non-interactive, so it terminates at
    /// quiescence like any returning agent and `drive` never blocks.
    pub fn for_test(dir: &std::path::Path, system: &str) -> io::Result<Self> {
        let shell = crate::bootstrap::boot_shell();
        let id = fresh_id();
        let log = AgentLog::root(
            &dir.join("sessions"),
            id,
            "test-model",
            "test",
            system.len(),
        )?;
        let provider = ProviderHandle::new(Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            crate::provider::scripted::Script::new(),
        )));
        let agent = Self::assemble(Build {
            system: system.to_string(),
            caps: ral_core::types::Capabilities::default(),
            shell,
            log,
            parent: None,
            fuel: SPAWN_FUEL,
            provider,
            focus: Arc::new(AtomicU64::new(NO_FOCUS)),
            interactive: false,
            tools: crate::tools::tools_for(true, false, SPAWN_FUEL > 0),
            agents: crate::agent_registry::AgentRegistry::new(),
        })?;
        agent.register_self();
        Ok(agent)
    }

    /// Whether the session is at a settled turn boundary — every turn
    /// must hand it back here.  Exposed for the harness to assert the
    /// transcript-admission invariant after a turn.
    pub fn is_ready(&self) -> bool {
        self.log.is_ready()
    }

    /// The model-view messages the next request would carry.  Exposed
    /// for the harness to assert no malformed / empty message survives
    /// into the committed transcript.
    pub fn rendered_messages(&self) -> Vec<genai::chat::ChatMessage> {
        self.log.history_messages()
    }

    /// Serialised model-view byte count — the compaction-threshold
    /// input.  Exposed so the harness can assert compaction fired.
    pub fn history_bytes(&self) -> usize {
        self.log.history_bytes()
    }

    /// Drain the shell's lease-chain reap notices and emit one compact
    /// transcript/TUI event per entry — a worker the lease chain removed by
    /// policy (unobserved past its idle bound, or past its absolute
    /// backstop) rather than one an eliminator observed away. Transcript +
    /// TUI only: never model-facing, so this posts nothing to the inbox and
    /// writes no `events.json` line — model-visible reap delivery is
    /// deferred (`decisions/260705_leases-and-budgets`). Called once per
    /// pass through the ready boundary in [`Self::drive`], the one site
    /// both the TUI and headless drives share.
    fn drain_worker_reaps(&mut self, emit: &Emitter) {
        let notices = self.transport.shell_mut().shell.take_worker_reap_notices();
        for notice in notices {
            let card = crate::card::reap_card(&notice.cmd, notice.cause);
            emit.emit(Kind::WorkerReaped {
                cmd: notice.cmd,
                cause: notice.cause,
                card,
            });
        }
    }

    /// Assemble this agent's half of the `/resources` probe fold — one
    /// [`ProbeRow`](crate::resources::ProbeRow) per session-lived
    /// accumulator this drive thread may legally read: the shell's worker
    /// registry (running and settled counts by class, with the nearest
    /// time-to-reap), the inbox's depth per source, the event log's mirror
    /// length and history bytes, the shell's binding count, the log-dir
    /// and scratch disk footprint (walked at invocation, never
    /// periodically), and the sub-agent ceiling's lease row.  A pure
    /// survey: nothing is mutated and no lease is renewed — enumeration is
    /// not observation — so `/resources` can never immortalise the zombies
    /// it exists to reveal.  The frontend appends the rows for the
    /// accumulators *it* owns (viewports, views, the bus) at render time;
    /// neither half reaches across a thread for the other's figures.
    fn resource_rows(&self) -> Vec<crate::resources::ProbeRow> {
        use crate::resources::{ProbeRow, terse_duration};
        use ral_core::types::{HandleState, LeaseClass};

        let mut rows = Vec::new();

        // ── the worker registry: running and settled, by class ──────────
        let entries = self.transport.shell_mut().shell.workers();
        let mut running_worker = 0u64;
        let mut running_durable = 0u64;
        let mut settled = 0u64;
        let mut nearest_reap: Option<std::time::Duration> = None;
        let mut nearest_expiry: Option<u64> = None;
        for entry in &entries {
            let running = *entry.handle.state.lock().unwrap() == HandleState::Running;
            if running {
                match entry.class {
                    LeaseClass::Worker => {
                        running_worker += 1;
                        // The nearer of the entry's two lease margins: idle
                        // remaining off the shared last-observed cell, and
                        // backstop remaining off its (display-only, close
                        // enough for a probe) wall-clock start.
                        let idle_left = shell_eval::DETACHED_WORKER_CEILING
                            .saturating_sub(entry.handle.last_observed.lock().unwrap().elapsed());
                        let age = entry.started.elapsed().unwrap_or_default();
                        let backstop_left =
                            shell_eval::DETACHED_WORKER_BACKSTOP.saturating_sub(age);
                        let left = idle_left.min(backstop_left);
                        nearest_reap = Some(nearest_reap.map_or(left, |m| m.min(left)));
                    }
                    LeaseClass::Durable => running_durable += 1,
                }
            } else {
                settled += 1;
                // Retention remaining in ral calls; an unstamped entry has
                // its whole retention ahead — the sweep stamps it next call.
                let left = match entry.settled_epoch {
                    Some(s) => shell_eval::SETTLED_WORKER_RETENTION
                        .saturating_sub(self.ral_epoch.saturating_sub(s)),
                    None => shell_eval::SETTLED_WORKER_RETENTION,
                };
                nearest_expiry = Some(nearest_expiry.map_or(left, |m| m.min(left)));
            }
        }
        rows.push(ProbeRow::new(
            "workers.running",
            running_worker + running_durable,
            Some(shell_eval::LIVE_WORKER_CAP as u64),
            "reject",
            None,
        ));
        rows.push(ProbeRow::new(
            "workers.running[worker]",
            running_worker,
            None,
            "reap",
            nearest_reap.map(|d| format!("nearest reap in {}", terse_duration(d))),
        ));
        rows.push(ProbeRow::new(
            "workers.running[durable]",
            running_durable,
            None,
            "none (unbounded)",
            Some("durable — dies by cancel, /clear, or process exit".to_string()),
        ));
        rows.push(ProbeRow::new(
            "workers.settled",
            settled,
            None,
            "reap",
            nearest_expiry.map(|n| format!("nearest expiry in {n} ral calls")),
        ));

        // ── the inbox, one row per source ────────────────────────────────
        for (source, depth) in self.inbox.source_depths() {
            // The ADR's split: idempotent sources coalesce, non-idempotent
            // facts are accepted or rejected — never silently dropped.
            let policy = match source {
                "user" | "schedule" | "nudge" => "coalesce",
                _ => "reject",
            };
            rows.push(ProbeRow::new(
                format!("inbox[{source}]"),
                depth,
                None,
                policy,
                Some("cap lands with enforcement".to_string()),
            ));
        }

        // ── the event log ────────────────────────────────────────────────
        rows.push(ProbeRow::new(
            "log.events",
            self.log.event_count() as u64,
            None,
            "evict",
            Some("prefix drops with compaction".to_string()),
        ));
        rows.push(ProbeRow::new(
            "log.bytes",
            self.log.history_bytes() as u64,
            Some(COMPACT_THRESHOLD as u64),
            "evict",
            Some("auto-compaction threshold".to_string()),
        ));

        // ── the lexical scope ────────────────────────────────────────────
        rows.push(ProbeRow::new(
            "bindings.count",
            self.transport.shell_mut().shell.binding_count() as u64,
            None,
            "reap",
            Some("lease lands with the binding reaper".to_string()),
        ));

        // ── disk, walked at invocation ───────────────────────────────────
        let log_dir = self.log.dir().to_path_buf();
        rows.push(ProbeRow::new(
            "disk.log_dir",
            crate::resources::dir_size(&log_dir),
            None,
            "warn",
            Some(log_dir.display().to_string()),
        ));
        if let Some(scratch) = self.transport.shell_mut().shell.env_var("EXARCH_SCRATCH") {
            rows.push(ProbeRow::new(
                "disk.scratch",
                crate::resources::dir_size(&std::path::PathBuf::from(&scratch)),
                None,
                "warn",
                Some(scratch),
            ));
        }

        // ── the sub-agent ceiling, as a lease row ────────────────────────
        rows.push(ProbeRow::new(
            "agents.ceiling",
            crate::agent_registry::AGENT_CEILING.as_secs(),
            None,
            "reap",
            Some("1 h fixed by design — push-delivery children renew nothing".to_string()),
        ));

        rows
    }

    /// Emit the `/resources` fold as one [`Kind::Resources`] bus event: the
    /// agent rows beside the card rendering them.  Called from the TUI's
    /// `Control` at the turn boundary the command drains at, exactly where
    /// `/clear` runs; transcript and TUI only, never model-facing.
    pub(crate) fn emit_resources(&self, emit: &Emitter) {
        let rows = self.resource_rows();
        let card = crate::resources::resources_card(&rows);
        emit.emit(Kind::Resources { rows, card });
    }

    /// The one shared turn loop — every node alike.  Pull a turn-boundary
    /// deliverable from the inbox, run one [`Self::apply`], let the nudge
    /// registry post any retry as a self-[`InboxMsg::Nudge`], and repeat.  An
    /// empty inbox parks or terminates by [`Self::park_mode`]: a present human
    /// (the conversing trunk or the focused agent) holds it; a live schedule
    /// holds it until cancelled; otherwise it terminates at quiescence.
    ///
    /// Per-`apply` panic recovery lives here — the `catch_unwind` rolls the
    /// dynamic context back to the last clean tool-call boundary and continues
    /// the loop, so one crashing turn never sinks the agent.  Returns the
    /// final turn's `(outcome, payload)` digest, where `payload` is the
    /// faithful [`serde_json::Value`] a `reply` carried (else `None`); the
    /// interactive trunk ignores it, a child's spawn site renders it to text,
    /// and the headless sink writes it faithfully.
    ///
    /// The provider is the agent's own [`ProviderHandle`], read once per turn,
    /// so a `/model` swap on this agent takes effect next turn; `control`
    /// handles agent-affecting slash commands ([`NoControl`] off the TUI).
    pub fn drive(
        &mut self,
        control: &mut dyn Control,
        emit: &Emitter,
    ) -> (AgentOutcome, Option<serde_json::Value>) {
        // The trunk publishes its sticky token for the OS-signal path, held for
        // the whole drive; a sub-agent's token is reached through the registry
        // cascade, never the slot, so it publishes nothing.
        let _slot = self.parent.is_none().then(|| cancel::publish(&self.cancel));
        let mut digest = (AgentOutcome::Empty, None);
        loop {
            // Every pass back to the loop top is a settled ready boundary
            // (the per-iteration quiesce below, or the invariant `drive` is
            // entered under); drain the lease chain's reap notices here so a
            // worker abandoned between turns still leaves a paper trail
            // before the loop parks or picks up the next deliverable.
            self.drain_worker_reaps(emit);
            // The park verdict ([`Self::park_mode`]) is recomputed on every wake
            // inside `next_or_idle`, so a `TAB` that de-focuses this agent (and
            // wakes its inbox) flips it from `Held` to `Quiesce` and it reaps.
            let Some(turn) = self.inbox.next_or_idle(|| self.park_mode(), &self.cancel) else {
                break;
            };
            // Generation admission: a worker delivers before it retires, so a
            // result that settled across a `/clear` can reach this queue; it
            // is dropped here, before it can reset latches or enter the log.
            if !self.admits(&turn) {
                continue;
            }
            // A genuine turn boundary resets the nudge latches and clears the
            // sticky cancel token, so a prior turn's Esc cannot carry into this
            // one.  A self-nudge is the same turn continuing and resets neither.
            if turn.resets_turn() {
                self.nudges.reset();
                self.cancel.reset();
            }
            // Agent-affecting slash commands run against the owned agent,
            // never the model.
            if let Turn::Command(raw) = &turn {
                match control.command(raw, self, emit) {
                    ControlFlow::Quit => break,
                    ControlFlow::Continue => continue,
                }
            }
            self.settle_commitment(&turn, emit);
            announce(&turn, emit);
            // Read the provider in force for this turn; a `/model` swap on this
            // agent lands here next turn, never mid-turn.
            let active = self.provider.current();
            let token = self.cancel.clone();
            let prompt = Some(turn.text());
            let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.apply(&active, prompt, &token, emit)
            }));
            let outcome = match attempt {
                Ok(o) => o,
                Err(p) => {
                    // A worker panic mid-eval: roll the dynamic state — grant
                    // frames, env/cwd overrides, the handler stack, half-applied
                    // bindings — back to the last clean tool-call boundary.
                    // Completed calls' effects live in the snapshot and survive;
                    // the panicking call's do not.  The IO frame self-heals in
                    // ral_core's own run_turn guard; this is the dynamic half.
                    self.transport
                        .shell_mut()
                        .shell
                        .restore_mobile(self.durable.clone());
                    let msg = panic_msg(&p);
                    self.note_error(format!("{WORKER_PANIC_PREFIX}{msg}"), emit);
                    digest = (
                        AgentOutcome::Failed(format!("worker panicked: {msg}")),
                        None,
                    );
                    if !self.log.is_ready() {
                        self.log.quiesce(QuiesceReason::Aborted);
                    }
                    continue;
                }
            };
            // A provider error or step cap can leave the session
            // mid-protocol (e.g. AwaitingAssistantAfterToolResults).
            // The post-loop guard does the same but only on loop exit;
            // quiesce per-iteration so the next prompt — nudge or user —
            // is admissible (turn-ends-ready invariant, X12).
            if !self.log.is_ready() {
                self.log.quiesce(QuiesceReason::Aborted);
            }
            digest = agent_digest(&outcome);
            let waiting_on_children = self.has_live_children();
            let ctx = nudge::NudgeCtx {
                // Every returning agent (a sub-agent or a headless trunk) hands
                // its value back through `reply`; an un-replied finish is
                // re-nudged within budget, then fails.  Only the interactive
                // trunk, which converses and never returns, is exempt — and so,
                // transiently, is any agent with live children: an un-replied
                // finish there is not a dropped return but a legitimate wait on
                // its fleet, and `park_mode` holds it (`HeldByChildren`) until a
                // child's result wakes it.  Nagging it toward `reply` now would
                // push it to return before its agents land; once they have all
                // settled the nudge resumes and insists on `reply` as before.
                must_reply: self.returns() && !waiting_on_children,
                pinned: self.pinned_digest(),
                waiting_on_children,
            };
            if let Some(msg) = self.nudges.react(&outcome, ctx, emit, &mut self.log) {
                self.inbox.push(InboxMsg::Nudge(msg));
            }
            // `reply` hard-terminates: end the drive loop regardless of focus or
            // a self-armed schedule — the agent returns its value and is gone.
            if matches!(outcome, Ok(TurnOutcome::Replied(_))) {
                break;
            }
        }
        // Safety net: per-iteration quiescing (above) handles mid-loop
        // errors, but a `break` from `Replied` or a future exit path could
        // still bypass it.  This catch-all ensures the turn-ends-ready
        // invariant holds regardless of how the loop exits.
        if !self.log.is_ready() {
            self.log.quiesce(QuiesceReason::Aborted);
        }
        debug_assert!(
            self.log.is_ready(),
            "drive must leave the agent ReadyForUser"
        );
        // The trunk removes itself at termination (a child is removed by its
        // spawn site through `settle`), so the fleet empties when the last
        // agent is gone.
        if self.parent.is_none() {
            self.agents.deregister(self.id);
        }
        digest
    }

    pub fn apply(
        &mut self,
        provider: &Arc<Provider>,
        prompt: Option<String>,
        token: &cancel::Token,
        emit: &Emitter,
    ) -> Result<TurnOutcome, ProviderError> {
        // Auto-compaction runs here, at the one boundary where `can_compact()`
        // actually holds — `apply` is entered `ReadyForUser` (the
        // turn-ends-ready invariant), before the prompt is committed.  Every
        // provider round-trip — each user turn, each nudge iteration — passes
        // through here, so long autonomous and headless runs stay bounded.
        self.compact(provider, emit, false, token);
        let mut last_text = String::new();
        if let Some(p) = prompt {
            self.log
                .append_user(p)
                .map_err(|e| ProviderError::Other(e.to_string()))?;
        }
        let mut n = 0u32;
        loop {
            n += 1;
            if n > MAX_STEPS {
                return self.capped(emit);
            }
            self.log
                .record_step(n, provider.tuning().clone())
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            emit.emit(Kind::Step {
                n,
                tuning: provider.tuning().clone(),
            });
            last_text.clear();
            #[cfg(debug_assertions)]
            let t_render = std::time::Instant::now();
            let messages = self
                .log
                .render_messages()
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            ral_core::dbg_trace!(
                "turn",
                "render_messages: {} msgs in {:?}",
                messages.len(),
                t_render.elapsed()
            );
            #[cfg(debug_assertions)]
            let t_req = std::time::Instant::now();
            #[cfg(debug_assertions)]
            let mut first_token: Option<std::time::Duration> = None;
            emit.emit(Kind::Phase("awaiting model".into()));
            let step_out = {
                let token_emit = emit.clone();
                let reasoning_emit = emit.clone();
                provider.complete(
                    &self.system,
                    messages,
                    &self.tools,
                    &mut |t: &str| {
                        #[cfg(debug_assertions)]
                        if first_token.is_none() {
                            first_token = Some(t_req.elapsed());
                        }
                        last_text.push_str(t);
                        token_emit.emit(Kind::Token(t.to_string()));
                    },
                    &mut |r: &str| {
                        reasoning_emit.emit(Kind::Thinking(r.to_string()));
                    },
                    token,
                )
            };
            ral_core::dbg_trace!(
                "turn",
                "provider.complete: first token {first_token:?}, full {:?}",
                t_req.elapsed()
            );
            if token.is_cancelled() {
                emit.emit(Kind::Boundary);
                return self.cancelled(emit);
            }
            let StepOut {
                mut assistant_message,
                tool_calls,
                reasoning,
                usage,
                stop_reason,
                cut_short,
            } = match step_out {
                Ok(s) => s,
                Err(ProviderError::Cancelled(_)) => {
                    emit.emit(Kind::Boundary);
                    return self.cancelled(emit);
                }
                Err(e) => {
                    emit.emit(Kind::Boundary);
                    return Err(e);
                }
            };
            // Commit reasoning before the boundary so the TUI can land the
            // `∴` block ahead of the answer's separate markdown rail. A pure
            // tool-call step may still show a thinking block; the captured
            // reasoning also round-trips on `assistant_message` below.
            if let Some(reasoning) = reasoning.as_deref()
                && !reasoning.trim().is_empty()
            {
                emit.emit(Kind::Reasoning {
                    text: reasoning.to_string(),
                    answer_chars: last_text.chars().count() as u32,
                });
            }
            emit.emit(Kind::Boundary);
            // The tokens the model just saw — the live numerator for the
            // context-pressure compaction trigger at this turn's boundary.
            self.last_input = usage.input;
            self.log
                .record_usage(usage.into())
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            emit.emit(Kind::Usage(usage));
            // Surface non-trivial stop reasons; the routine boundaries
            // and `MaxTokens` (handled below) stay silent.
            if let Some(reason) = &stop_reason {
                match reason {
                    StopReason::Completed(_)
                    | StopReason::ToolCall(_)
                    | StopReason::MaxTokens(_) => {}
                    _ => emit.emit(Kind::StopReason(reason.raw().to_string())),
                }
            }
            // Admission boundary: repair non-object tool-call arguments
            // (X2) and never commit an empty assistant message (X7) before
            // it enters the transcript.
            admit_assistant(&mut assistant_message);
            let tool_ids: Vec<String> = tool_calls.iter().map(|tc| tc.call_id.clone()).collect();
            self.log
                .append_assistant(
                    assistant_message,
                    tool_ids,
                    stop_reason.as_ref().map(|r| r.raw().to_string()),
                )
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            let truncated = cut_short.is_some();
            // The assistant turn was cut short — the output cap or a
            // mid-stream stall.  With no captured tool call there is nothing
            // to dispatch and the assistant turn is final, so commit the
            // partial reply (done above) and surface a `Truncated` so the
            // nudge re-drives the turn with `continue`.  With captured tool
            // calls (only the output cap leaves any) the assistant message
            // carries `tool_ids` and the session is now `AwaitingToolResults`;
            // returning here would strand it there and the nudge's
            // `append_user` would fail "tool results pending" (X6).  Instead
            // fall through to dispatch the calls and continue the loop — the
            // next round-trip resumes the truncated turn with the results in
            // hand.
            if truncated && tool_calls.is_empty() {
                let reason = match cut_short.as_ref().expect("truncated implies cut_short") {
                    CutShort::OutputCap => {
                        let reason = stop_reason
                            .as_ref()
                            .map(|r| r.raw().to_string())
                            .unwrap_or_else(|| "max_tokens".into());
                        self.note_error(
                            format!(
                                "turn truncated (stop_reason={reason}): output cap reached. \
                                 re-run with `--max-tokens N` for a larger ceiling, \
                                 or ask the agent to split the work into smaller turns.",
                            ),
                            emit,
                        );
                        reason
                    }
                    CutShort::Stalled(cause) => {
                        // A transient transport hiccup we recovered from, not
                        // a misconfiguration: an operational note, not an error.
                        self.note(
                            format!(
                                "[Stream stalled: {}]",
                                cause.replace('\n', " | ")
                            ),
                            emit,
                        );
                        cause.clone()
                    }
                };
                return Err(ProviderError::Truncated { reason });
            }
            if tool_calls.is_empty() {
                return Ok(match &stop_reason {
                    Some(r) if !matches!(r, StopReason::Completed(_) | StopReason::ToolCall(_)) => {
                        TurnOutcome::Stopped {
                            reason: r.raw().to_string(),
                        }
                    }
                    _ if last_text.is_empty() => TurnOutcome::Empty,
                    _ => TurnOutcome::Complete(last_text),
                });
            }
            if truncated {
                self.note(
                    "[Truncated mid-tool-call; continuing]"
                        .into(),
                    emit,
                );
            }
            let Dispatch { results, injected } = self.dispatch(provider, tool_calls, token, emit);
            self.log
                .append_tool_results(results)
                .map_err(|e| ProviderError::Other(e.to_string()))?;
            // Everything that arrived during the batch lands now, mid-turn:
            // each source renders its own chrome (a `↘` block for a subagent, a
            // marked wakeup, a `spawn`'s cards), and their texts coalesce into
            // the single steering message the protocol admits after a batch.
            if !injected.is_empty() {
                let mut text = String::new();
                for turn in &injected {
                    announce(turn, emit);
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&turn.text());
                }
                self.log
                    .append_steering(text)
                    .map_err(|e| ProviderError::Other(e.to_string()))?;
            }
            if token.is_cancelled() {
                return self.cancelled(emit);
            }
            // The batch has fully drained — every call dispatched, every
            // `call_id` answered.  If one of them was `reply`, end the run now
            // with its payload rather than looping for another round-trip.
            if let Some(payload) = self.reply.take() {
                return self.replied(payload);
            }
        }
    }

    pub(crate) fn compact(
        &mut self,
        provider: &Arc<Provider>,
        emit: &Emitter,
        requested: bool,
        token: &cancel::Token,
    ) {
        if !self.log.can_compact() {
            if requested {
                self.note_error("cannot compact while tool results are pending".into(), emit);
            }
            return;
        }
        // Auto-compaction tracks real context pressure: `last_input` (the
        // tokens the model last saw) against the model's context window,
        // firing once it grows into the reserve (oh-my-pi's trigger).  An
        // unknown window (native provider, or the catalog not yet fetched)
        // falls back to the absolute byte heuristic.  A manual `/compact`
        // (`requested`) overrides the gate: the user is compacting on
        // purpose.  `summary_cap` scales the summary with the window —
        // exarch keeps a recent suffix verbatim and summarises only the
        // older prefix, so the summary stays concise either way.
        let used = self.last_input;
        let window = provider.context_window();
        let (due, detail, summary_cap) = match window {
            Some(w) if w > 0 => (
                compaction_due(used, w),
                format!("{used} of {w} tokens"),
                summary_cap_tokens(w),
            ),
            _ => {
                let bytes = self.log.history_bytes();
                (
                    bytes >= COMPACT_THRESHOLD,
                    format!("{} KB", bytes / 1024),
                    SUMMARY_CAP_FALLBACK_TOKENS,
                )
            }
        };
        if !requested && !due {
            return;
        }
        // A turn-boundary Esc must not kick off a summarize request we'd
        // only instantly cancel; bail before the work and let `apply`'s
        // post-compact check return to the prompt.
        if token.is_cancelled() {
            return;
        }
        // Keep the recent half verbatim; summarise the older prefix.
        let keep = suffix_keep_budget(self.log.history_bytes());
        let Some(plan) = self.log.plan_compaction(keep) else {
            // No turn old enough to summarise.  This is a no-op, not an event:
            // the absence of a `compacted` note already says nothing happened,
            // and the worker has no honest way to draw view-only chrome.
            return;
        };
        self.note(format!("[Compacting history: {detail} → summary]"), emit);
        emit.emit(Kind::Phase("compacting".into()));
        match provider.summarize(&self.system, plan.prefix_messages, summary_cap, token) {
            Ok(summary) => {
                if let Err(e) = self.log.record_usage(summary.usage.into()) {
                    self.note_error(format!("compact failed: {e}"), emit);
                    return;
                }
                emit.emit(Kind::Usage(summary.usage));
                if let Err(e) = self
                    .log
                    .apply_compaction(summary.summary, plan.suffix_start)
                {
                    self.note_error(format!("compact failed: {e}"), emit);
                    return;
                }
                self.note(
                    format!("[Compacted: now {} KB]", self.log.history_bytes() / 1024),
                    emit,
                );
            }
            Err(e) => self.note_error(format!("compact failed: {e}"), emit),
        }
    }

    // --- private helpers ---

    /// Dispatch a batch of tool calls in order, short-circuiting the rest to
    /// cancelled results the instant the token trips.  Every tool returns its
    /// result synchronously now — the spawn tools launch a detached peer and
    /// return a start receipt — so there is no join phase and no
    /// `thread::scope`.
    fn dispatch(
        &mut self,
        provider: &Arc<Provider>,
        tool_calls: Vec<ToolCall>,
        token: &cancel::Token,
        emit: &Emitter,
    ) -> Dispatch {
        let mut results = Vec::with_capacity(tool_calls.len());
        let mut it = tool_calls.into_iter();
        for call in it.by_ref() {
            if token.is_cancelled() {
                results.push(cancelled_result(call.call_id));
                results.extend(it.map(|r| cancelled_result(r.call_id)));
                break;
            }
            results.push(self.stage(provider, call, emit));
        }
        // The tool-boundary drain: every message that arrived during the
        // batch — barged-in user steering, a settled subagent's result, a
        // fired wakeup, a `spawn`'s surface — tagged with its source. A
        // slash command is the lone exception, held for the turn boundary.
        // Generation admission applies here as at the turn boundary: a
        // result that settled across a `/clear` is dropped, not injected.
        let injected = self
            .inbox
            .drain_tool()
            .into_iter()
            .filter(|t| self.admits(t))
            .collect();
        Dispatch { results, injected }
    }

    /// Generation admission for a drained deliverable.  An
    /// [`AgentResult`](crate::bus::AgentResult) is stamped with its worker's
    /// birth generation, and the worker posts it *before* retiring its
    /// registry entry (deliver-then-retire, so a parked parent can never
    /// observe "no live child" without the result already queued) — which
    /// means the worker cannot check staleness itself.  The consuming edge
    /// decides instead: a result whose generation predates the live registry
    /// generation settled across a `/clear` and belongs to a context that no
    /// longer exists.  Every other turn source is generation-free and
    /// admitted.
    fn admits(&self, turn: &Turn) -> bool {
        match turn {
            Turn::Agent(r) => r.generation == self.agents.generation(),
            _ => true,
        }
    }

    /// A returning agent — every node but the *conversing* (interactive) trunk
    /// — hands a value back through `reply` and terminates.  Only the conversing
    /// trunk converses across turns and never returns.  Read in one place from
    /// position: the inverse of `parent = None ∧ interactive`.
    fn returns(&self) -> bool {
        !(self.parent.is_none() && self.interactive)
    }

    /// The `should_park` predicate ([`ParkMode`]), recomputed on every wake.  A
    /// present human holds this agent parked — the *conversing* trunk (`parent
    /// = None ∧ interactive`) or the agent the human has `TAB`bed to (`focus =
    /// id`); live children it launched hold it until they settle
    /// ([`ParkMode::HeldByChildren`]) so a headless root waiting on its fleet
    /// stays alive to receive their results; a live self-schedule holds it
    /// until cancelled; otherwise it terminates at quiescence.
    fn park_mode(&self) -> ParkMode {
        let conversing = self.parent.is_none() && self.interactive;
        if conversing || self.focus.load(Ordering::Relaxed) == self.id {
            ParkMode::Held
        } else if self.has_live_children() {
            ParkMode::HeldByChildren
        } else if self.schedules.armed() {
            ParkMode::UntilCancelled
        } else {
            ParkMode::Quiesce
        }
    }

    /// Whether this agent has a live child still running — an async child it
    /// launched that has not yet settled its result up this inbox.  Read by
    /// [`Self::park_mode`] (so a root waiting on its fleet parks rather than
    /// quiescing) and by [`Self::drive`] (so the `reply` nudge is not raised
    /// against a finish that is a legitimate wait, not a dropped return).
    fn has_live_children(&self) -> bool {
        self.agents.has_children(self.id)
    }

    fn stage(
        &mut self,
        provider: &Arc<Provider>,
        call: ToolCall,
        emit: &Emitter,
    ) -> SessionToolResult {
        // Dispatch searches this agent's own view, so a tool the agent does not
        // hold is simply not found — there is no separate permission check.  A
        // withheld tool (`reply` on the conversing trunk, the self-wakeup family
        // without `--allow-schedule`) is also unadvertised, so a well-behaved
        // model never names one here.
        match self
            .tools
            .iter()
            .find(|t| t.name() == call.fn_name)
            .copied()
        {
            Some(t) => t.dispatch(call.call_id, call.fn_arguments, self, provider, emit),
            None => {
                let msg = format!("unknown tool `{}`", call.fn_name);
                self.note_error(msg.clone(), emit);
                SessionToolResult {
                    id: call.call_id,
                    content: msg,
                }
            }
        }
    }

    pub(crate) fn cwd(&self) -> std::path::PathBuf {
        self.transport.shell_mut().shell.cwd()
    }

    /// This session's ambient authority — read by the spawn site to compute a
    /// child's capabilities, either inherited verbatim or narrowed to a base.
    pub(crate) fn caps(&self) -> &ral_core::types::Capabilities {
        &self.caps
    }

    /// This agent's remaining spawn budget — read by `/discuss` to refuse
    /// seating a chair that could not itself spawn a partner.
    pub(crate) fn fuel(&self) -> u32 {
        self.fuel
    }

    /// Whether this agent holds the self-wakeup family — read by [`Self::fork`]
    /// so a child inherits the parent's `--allow-schedule` grant.  Derived from
    /// the tool view itself, keeping it the one source of truth.
    fn grants_schedule(&self) -> bool {
        self.tools
            .iter()
            .any(|t| matches!(t.gate(), crate::tools::Gate::Schedules))
    }

    /// Best-effort dual-write: log the chrome line, then forward it
    /// through `emit`.  A log write-failure must not block the user line.
    pub(crate) fn note_error(&mut self, msg: String, emit: &Emitter) {
        let _ = self.log.record_error(msg.clone());
        emit.emit(Kind::Error(msg));
    }

    /// Emit an operational system note — a truncation recovery, a compaction
    /// step.  Recorded in `transcript.jsonl` at the emit seam and surfaced on
    /// the display dim; never written to the model-view `events.json`, since it
    /// is not a message the model saw.
    pub(crate) fn note(&self, text: String, emit: &Emitter) {
        emit.emit(Kind::SystemNote(text));
    }

    pub(crate) fn run_shell(
        &mut self,
        id: String,
        cmd: &str,
        timeout_secs: u64,
        emit: &Emitter,
    ) -> SessionToolResult {
        // One ral call, one epoch tick — counted at entry so a call that
        // fails to evaluate still advances the retention clock.
        self.ral_epoch += 1;
        // Refresh the durable snapshot at this clean boundary: the
        // dynamic context here reflects every prior tool call that
        // returned, and none that this one is about to mutate.  If the
        // eval below panics, `drive` rebuilds the live context from
        // this snapshot, rolling the panicking call's effects back.
        self.durable = self.transport.shell_mut().shell.mobile_snapshot();
        // The deferred-surface destination: a detached `spawn` worker flushes
        // its buffered batch here at completion, posted into this session's
        // own inbox (via `emit`'s mailbox) and guarded by the agent registry's
        // generation (so a `/clear` drops a stale batch).
        self.transport
            .set_boundary(shell_eval::boundary_sink(emit, self.id, &self.agents));
        let content = match shell_eval::run_shell(
            #[cfg(unix)]
            if let Some(ref wire) = self.wire {
                wire as &dyn ral_core::transport::Transport
            } else {
                &self.transport
            },
            #[cfg(not(unix))]
            &self.transport,
            &self.caps,
            cmd,
            timeout_secs,
            emit,
            Some(self.pins.clone()),
        ) {
            shell_eval::Outcome::Ran(r) => render(&r),
            shell_eval::Outcome::Static(s) => clip(&s, OPAQUE_CAP),
        };
        // Advance the settled-retention sweep to this call's epoch: stamp
        // entries first observed settled, expire those whose unclaimed
        // result has sat a full retention of calls.  Expiry notices ride
        // the existing `drain_worker_reaps` at the next ready boundary —
        // no plumbing of their own.
        self.transport
            .shell_mut()
            .shell
            .advance_worker_epoch(self.ral_epoch, shell_eval::SETTLED_WORKER_RETENTION);
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
    }

    /// Read a protected commitment pin for host-owned verification.  Model
    /// code cannot write this prefix through `surface`; verifier orchestration
    /// reads the saved card here and treats it as data.
    pub(crate) fn commitment_card(&self, key: &str) -> Result<crate::card::Card, String> {
        if !shell_eval::is_commitment_pin(key) {
            return Err(format!(
                "`{key}` is not a protected commitment pin; expected `{}<id>`",
                shell_eval::COMMITMENT_PIN_PREFIX
            ));
        }
        let m = self
            .pins
            .lock()
            .map_err(|_| "commitment pin register is unavailable".to_string())?;
        match m.get(key) {
            Some(pin) if pin.kind == shell_eval::PinKind::Commitment => Ok(pin.card.clone()),
            Some(_) => Err(format!(
                "`{key}` is pinned, but not as protected commitment state"
            )),
            None => Err(format!(
                "no live commitment pin named `{key}`; did the verifier already pass, or was the session cleared?"
            )),
        }
    }

    /// Host projection for a writer's formalized commitment: set the
    /// protected pin in the session mirror.  Pure state — projecting it to
    /// the viewport is the caller's separate step
    /// ([`Self::settle_commitment`]).  Refused if the key is already live: a
    /// commitment, once open, can only be closed by a verifier, never
    /// silently replaced.
    fn set_commitment_pin(&mut self, key: &str, card: crate::card::Card) -> Result<(), String> {
        if !shell_eval::is_commitment_pin(key) {
            return Err(format!(
                "`{key}` is not a protected commitment pin; expected `{}<id>`",
                shell_eval::COMMITMENT_PIN_PREFIX
            ));
        }
        let mut m = self
            .pins
            .lock()
            .map_err(|_| "commitment pin register is unavailable".to_string())?;
        if m.contains_key(key) {
            return Err(format!(
                "`{key}` is already a live commitment; verify or clear it before opening a new one"
            ));
        }
        m.insert(
            key.to_string(),
            shell_eval::PinDigest {
                kind: shell_eval::PinKind::Commitment,
                card,
            },
        );
        Ok(())
    }

    /// Host projection for a verifier pass: clear the protected pin in the
    /// session mirror.  Pure state — projecting the clear to the viewport is
    /// the caller's separate step ([`Self::settle_commitment`]).  The model
    /// cannot reach this path; ordinary `surface` unpins for the same prefix
    /// are rejected in `shell_eval`.
    fn unset_commitment_pin(&mut self, key: &str) -> Result<bool, String> {
        if !shell_eval::is_commitment_pin(key) {
            return Err(format!(
                "`{key}` is not a protected commitment pin; expected `{}<id>`",
                shell_eval::COMMITMENT_PIN_PREFIX
            ));
        }
        let mut m = self
            .pins
            .lock()
            .map_err(|_| "commitment pin register is unavailable".to_string())?;
        Ok(m.remove(key).is_some())
    }

    /// A settled `commit`/`verify_commitment` child tags its result with
    /// what the parent should do to the pin register
    /// ([`spawn_async`](crate::tools::agent::spawn_async)), decided on the
    /// worker thread that drove it since only that thread ever holds the raw
    /// reply.  This applies the tag on the parent's own thread, at drain, then
    /// forwards whatever [`Self::apply_commitment_settle`] (the pinning half)
    /// says actually changed to the viewport (the rendering half).
    fn settle_commitment(&mut self, turn: &Turn, emit: &Emitter) {
        let Turn::Agent(r) = turn else { return };
        if let Some(kind) = self.apply_commitment_settle(&r.commitment_settle) {
            emit.emit(kind);
        }
    }

    /// Pinning, in isolation: apply a settled `commit`/`verify_commitment`
    /// child's tag to the pin register — `set`/`unset` — and report which
    /// [`Kind`] (if any) that change is to the caller. Pure state plus a
    /// description of it; it never touches an [`Emitter`], so it needs no bus
    /// to test. An untagged settle (every ordinary `amnemon`/`mnemon`) and a
    /// clear of a key that was not actually live both report `None`.
    fn apply_commitment_settle(
        &mut self,
        settle: &Option<shell_eval::CommitmentSettle>,
    ) -> Option<Kind> {
        match settle {
            Some(shell_eval::CommitmentSettle::Open { key, card }) => {
                self.set_commitment_pin(key, card.clone()).ok()?;
                Some(Kind::Pin {
                    key: key.clone(),
                    card: card.clone(),
                })
            }
            Some(shell_eval::CommitmentSettle::Clear(key)) => {
                self.unset_commitment_pin(key)
                    .ok()
                    .filter(|&cleared| cleared)?;
                Some(Kind::Unpin { key: key.clone() })
            }
            None => None,
        }
    }

    /// Non-blocking pop of the next deliverable inbox message, for a test
    /// polling for an async spawn's settle without driving a full turn (and
    /// its provider round-trip) on the parent session.
    #[cfg(test)]
    pub(crate) fn drain_turn_for_test(&self) -> Option<Turn> {
        self.inbox.drain_turn()
    }

    #[cfg(test)]
    pub(crate) fn insert_commitment_pin_for_test(&mut self, key: &str, card: crate::card::Card) {
        assert!(shell_eval::is_commitment_pin(key));
        self.pins.lock().expect("pin register poisoned").insert(
            key.to_string(),
            shell_eval::PinDigest {
                kind: shell_eval::PinKind::Commitment,
                card,
            },
        );
    }

    /// The current pinned state as a one-line description for the periodic
    /// nudge reminder, or `None` when nothing is pinned.  Joins each slot's
    /// digest (`tasks 3/8`) — the model's labels already name them — so the
    /// reminder reads as the user sees the rail.
    fn pinned_digest(&self) -> Option<String> {
        let m = self.pins.lock().ok()?;
        if m.is_empty() {
            return None;
        }
        Some(
            m.values()
                .map(|pin| crate::card::summary_line(&pin.card))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Stash a returning agent's deliberate return value — called from the
    /// `reply` tool's dispatch with the faithful [`serde_json::Value`] the
    /// model passed.  [`Self::apply`] lifts it into a [`TurnOutcome::Replied`]
    /// once the tool-call batch drains.
    pub(crate) fn set_reply(&mut self, payload: serde_json::Value) {
        self.reply = Some(payload);
    }

    /// Reached when the batch carried a `reply`: wind the session back to
    /// `ReadyForUser` with the dedicated breadcrumb (the last round-trip
    /// dispatched the reply but never asked for a final assistant message, so
    /// the protocol sits in `AwaitingAssistantAfterToolResults`), cancel any
    /// live descendants, and return the payload.  `drive` then breaks the loop
    /// — `reply` hard-terminates.
    fn replied(&mut self, payload: serde_json::Value) -> Result<TurnOutcome, ProviderError> {
        self.agents.cancel_descendants(self.id);
        self.log.quiesce(QuiesceReason::Replied);
        Ok(TurnOutcome::Replied(payload))
    }

    fn cancelled(&mut self, emit: &Emitter) -> Result<TurnOutcome, ProviderError> {
        self.log.quiesce(QuiesceReason::Cancelled);
        // The canonical log already carries `Cancelled` from quiesce;
        // this is the user-facing companion only.
        emit.emit(Kind::Error("cancelled".into()));
        Ok(TurnOutcome::Cancelled)
    }

    /// Reached at the top of the round-trip loop once the step count
    /// would exceed [`MAX_STEPS`].  The history is mid-protocol (the last
    /// step appended its tool results); `drive`'s single exit winds it
    /// back to `ReadyForUser`.  `note_error` is the user-facing line and
    /// the forensic breadcrumb; the `StopReason` surfaces in the headless
    /// JSON result so a benchmark harness can tell a capped run from a
    /// completed one.
    fn capped(&mut self, emit: &Emitter) -> Result<TurnOutcome, ProviderError> {
        self.note_error(
            format!("step cap reached ({MAX_STEPS} provider round-trips); ending turn"),
            emit,
        );
        emit.emit(Kind::StopReason("step_cap".into()));
        Ok(TurnOutcome::Capped)
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.log.record_session_ended();
    }
}

struct Dispatch {
    results: Vec<SessionToolResult>,
    injected: Vec<Turn>,
}

/// The model-facing chrome a turn's source emits as it enters context —
/// whether it opens a fresh turn at the boundary or lands mid-turn at a tool
/// boundary: a human prompt and a wakeup echo as their (possibly marked) text,
/// an agent result renders as the dialable `↘` subagent block the sink already
/// knows.  A self-nudge and a command are quiet — a nudge is an internal
/// continuation, a command never reaches the model.
fn announce(turn: &Turn, emit: &Emitter) {
    match turn {
        Turn::Human(s) | Turn::Wakeup(s) => emit.emit(Kind::UserPromptEcho(s.clone())),
        Turn::Message(_) => emit.emit(Kind::UserPromptEcho(turn.text())),
        Turn::Agent(r) => emit.emit(Kind::SubagentDone {
            title: r.title.clone(),
            outcome: r.outcome.clone(),
            text: r.text.clone(),
            elapsed: r.elapsed,
        }),
        // A detached `spawn` worker's deferred surface batch: decode each value
        // and emit it as the live foreground decode would, so its cards/io land
        // on the rail.  The emitter's id is this session's, which is exactly the
        // id the batch was stamped with (a session's boundary sink stamps with
        // its own id and posts to its own inbox), so the cards reach the right
        // viewport.  The model is then woken with `turn.text()`'s notice.
        Turn::Surface { values, .. } => {
            for v in values {
                if let Some(kind) = shell_eval::decode_surface(v) {
                    emit.emit(kind);
                }
            }
        }
        // A self-nudge is a quiet continuation; a command never reaches the model.
        Turn::Nudge(_) | Turn::Command(_) => {}
    }
}

/// Reduce a finished turn's outcome to the `(tag, payload)` digest a returning
/// agent's result carries — the one place the reduction happens.  Only a
/// deliberate `reply` carries a value up, and it travels as the faithful
/// [`serde_json::Value`] the model passed; the consuming edge renders it (a
/// null or empty-string value settles `Empty`).  A tool-call-free prose finish
/// is **not** harvested — there is no scrape — and because `reply` is the sole
/// return path, a returning agent that never replied did not complete its
/// contract: it (like a genuinely empty turn) settles `Failed`.  Every other
/// shape carries only its tag.
fn agent_digest(
    r: &Result<TurnOutcome, ProviderError>,
) -> (AgentOutcome, Option<serde_json::Value>) {
    match r {
        Ok(TurnOutcome::Replied(v)) => match shell_eval::json_to_text(v) {
            // Any value that renders to text carries content; a null or
            // empty-string reply (renders to nothing) is a deliberate empty
            // return.
            Some(s) if !s.is_empty() => (AgentOutcome::Complete, Some(v.clone())),
            _ => (AgentOutcome::Empty, None),
        },
        // A finish without `reply`.  There is no scrape: a returning agent that
        // never replied did not complete its contract, so it fails honestly
        // rather than handing up a trailing fragment that masquerades as the
        // answer.  Re-nudged within budget first (see [`nudge`]).
        Ok(TurnOutcome::Complete(_)) | Ok(TurnOutcome::Empty) => (
            AgentOutcome::Failed("ended without calling `reply`".into()),
            None,
        ),
        Ok(TurnOutcome::Stopped { reason }) => (AgentOutcome::Stopped(reason.clone()), None),
        Ok(TurnOutcome::Cancelled) => (AgentOutcome::Cancelled, None),
        Ok(TurnOutcome::Capped) => (AgentOutcome::Stopped("step cap reached".into()), None),
        Err(e) => (AgentOutcome::Failed(e.summary()), None),
    }
}

/// Render a reply payload to the text a model parent reads: the shared
/// value→text rule ([`shell_eval::json_to_text`]), clipped to the reply cap.
/// The peer edge (the spawn tool settle site) and the `drive_peer` test
/// helper share it; the headless edge instead writes the value faithfully.
pub(crate) fn render_reply(v: &serde_json::Value) -> String {
    clip(
        &shell_eval::json_to_text(v).unwrap_or_default(),
        AGENT_REPLY_CAP,
    )
}

/// The message of a recovered panic payload, for either string shape.
fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&'static str>().map(|s| (*s).into()))
        .unwrap_or_else(|| "non-string payload".into())
}

fn boot_root_shell(scratch: &Scratch) -> Shell {
    let mut shell = crate::bootstrap::boot_shell();
    scratch.install_into(&mut shell);
    shell
}

/// Seed `EXARCH_SESSION_DIR` into `shell` from `log`'s directory.  Run
/// at construction and again after [`Agent::clear`] rebuilds the
/// shell, so the name always points at the live session's event-log
/// directory.
fn seed_session_dir(shell: &mut Shell, log: &AgentLog) {
    let dir = log.dir().to_string_lossy().into_owned();
    crate::bootstrap::seed_var(shell, "EXARCH_SESSION_DIR", &dir);
}

fn cancelled_result(id: String) -> SessionToolResult {
    SessionToolResult {
        id,
        content: "cancelled before tool execution".into(),
    }
}

/// Substituted for an assistant message that would serialise to no
/// substantive content.  Anthropic rejects an assistant turn whose
/// `content` is empty (`[]` or only an empty text block) once a later
/// message makes it non-final, so the empty-turn nudge — which appends a
/// user prompt right after — would poison every subsequent request.  A
/// short stub keeps the turn renderable; the nudge still recovers the
/// empty reply by re-prompting.
const EMPTY_ASSISTANT_STUB: &str = "(no content)";

/// Normalise an assistant message to the transcript-admission invariant
/// at the `apply` commit boundary: **every committed message serialises
/// to a request every supported provider accepts.**
///
/// - X2: a tool call whose `fn_arguments` is not a JSON object is repaired
///   to `{}`.  genai's Anthropic adapter only repairs a `null` argument, so
///   a non-object (a bare string, number, array) re-serialises verbatim and
///   strict backends 400 every later request that carries it.
/// - X7: a message with no substantive part — no tool call, no non-empty
///   text or binary — has the stub substituted, so it is never committed
///   empty.
fn admit_assistant(msg: &mut genai::chat::ChatMessage) {
    use genai::chat::ContentPart;
    for part in msg.content.iter_mut() {
        if let ContentPart::ToolCall(tc) = part
            && !tc.fn_arguments.is_object()
        {
            tc.fn_arguments = serde_json::json!({});
        }
    }
    let substantive = msg.content.iter().any(|p| match p {
        ContentPart::Text(t) => !t.trim().is_empty(),
        ContentPart::ToolCall(_) | ContentPart::Binary(_) | ContentPart::ToolResponse(_) => true,
        // Reasoning / thought signatures alone are not a renderable turn on
        // a strict backend; they ride alongside real content, never as it.
        ContentPart::ThoughtSignature(_)
        | ContentPart::ReasoningContent(_)
        | ContentPart::Custom(_) => false,
    });
    if !substantive {
        msg.content = genai::chat::MessageContent::from_text(EMPTY_ASSISTANT_STUB);
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    //! Panic-recovery integrity (A4): a worker panic mid-tool-eval must
    //! preserve the bindings completed tool calls left behind and leave the
    //! dynamic context clean.  Driven through the scripted provider and the
    //! shared `drive` loop — the real path: `drive` catches the unwind,
    //! ral_core's own frame guard self-heals the IO frame, and `drive`
    //! rebuilds the live context from the durable snapshot.

    use super::*;
    use crate::provider::scripted::{Reply, Script};
    use ral_core::Value;
    use ral_core::typecheck::builtins::{BuiltinTypeRule, mk_scheme, pure, thunk};
    use ral_core::typecheck::{Scheme, Ty, Unifier};
    use ral_core::types::{BuiltinBody, BuiltinEntry, Settled};
    use std::borrow::Cow;

    /// A scripted provider behind the `Arc` the turn driver threads.
    fn scripted(model: &str, script: Script) -> Arc<Provider> {
        Arc::new(Provider::scripted(model, ProviderKind::Openai, script))
    }

    /// A nullary builtin whose body panics — stands in for any Rust panic
    /// the evaluator can raise mid-tool-eval.
    fn builtin_panic_now(_args: &[Value], _shell: &mut Shell) -> Settled<Value> {
        panic!("a4 test: deliberate mid-eval panic");
    }

    fn scheme_panic_now(_u: &mut Unifier) -> Scheme {
        mk_scheme(&[], &[], &[], thunk(pure(Ty::Unit)))
    }

    static PANIC_BUILTINS: &[BuiltinEntry] = &[BuiltinEntry {
        name: Cow::Borrowed("a4-panic-now"),
        type_rule: BuiltinTypeRule::Scheme(Some(0), scheme_panic_now),
        doc: "test-only: panic the evaluator mid-eval.",
        body: BuiltinBody::Static(builtin_panic_now),
    }];

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("exarch-a4-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn ral_call(id: &str, cmd: &str) -> ToolCall {
        ToolCall {
            call_id: id.into(),
            fn_name: "ral".into(),
            fn_arguments: serde_json::json!({
                "cmd": cmd,
                "description": "a4 test command",
            }),
            thought_signatures: None,
        }
    }

    #[test]
    fn worker_panic_preserves_completed_bindings_and_clean_context() {
        ral_core::builtins::register_builtins(PANIC_BUILTINS);
        let dir = tmp("panic-recovery");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session
            .transport
            .shell_mut()
            .shell
            .install_builtins(PANIC_BUILTINS);
        // Refresh `durable` so the snapshot reflects the just-installed
        // builtin frame, matching the production boundary where the
        // baseline is the booted shell.
        session.durable = session.transport.shell_mut().shell.mobile_snapshot();
        let baseline_grant_depth = session.transport.shell_mut().shell.grant_depth();

        // 1st call binds `a4_x` (completes); 2nd call panics mid-eval.  Both
        // round-trips happen inside one `apply`; `drive` catches the unwind.
        let provider = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c1", "let a4_x = 7")]))
                .then(Reply::tool_calls(vec![ral_call("c2", "a4-panic-now")])),
        );
        session.provider = ProviderHandle::new(provider);
        session.seed("compute then crash".into());
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, session.id);
        let _ = session.drive(&mut NoControl, &emit);

        // The completed call's binding survives the panic.
        assert!(
            session
                .transport
                .shell_mut()
                .shell
                .scope_lookup("a4_x")
                .is_some(),
            "a binding from a completed tool call must survive a later call's panic"
        );
        // The dynamic context is rolled back to the clean boundary: no
        // leaked grant frame from the panicking call's `with_capabilities`.
        assert_eq!(
            session.transport.shell_mut().shell.grant_depth(),
            baseline_grant_depth,
            "the panicking call's grant frame must not leak into the next turn"
        );
        // The drive loop handed the session back ready for a fresh prompt.
        assert!(
            session.is_ready(),
            "drive must leave the session ReadyForUser even after a worker panic"
        );

        // The next turn is admissible and runs to completion on the
        // healed shell.
        let provider2 = scripted("test-model", Script::new().then(Reply::text("ok")));
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, session.id);
        let token = cancel::Token::new();
        let _slot = cancel::publish(&token);
        match session.apply(&provider2, Some("continue".into()), &token, &emit) {
            Ok(TurnOutcome::Complete(s)) => assert_eq!(s, "ok"),
            other => panic!("next turn on the healed shell must complete, got {other:?}"),
        }
    }

    /// A forked child inherits the parent's installed builtin surface, not
    /// just the core set a bare `Shell::new` seeds, and records its parent for
    /// the subtree cascade.
    #[test]
    fn fork_inherits_host_builtins_and_can_spawn() {
        let dir = tmp("fork-builtins");
        let session = Agent::for_test(&dir, "system").unwrap();
        assert!(
            session
                .transport
                .shell_mut()
                .shell
                .lookup_builtin("view-text")
                .is_some(),
            "the parent boot shell must carry the exarch host builtins"
        );
        let child = session
            .fork(session.caps().clone())
            .expect("fork child session");
        for name in ["view-text", "grep-files", "edit", "explore-dir"] {
            assert!(
                child
                    .transport
                    .shell_mut()
                    .shell
                    .lookup_builtin(name)
                    .is_some(),
                "the forked child must inherit the host builtin `{name}`"
            );
        }
        // The child is registered under its parent, so the cascade reaches it.
        assert_eq!(
            child.parent,
            Some(session.id),
            "a fork records its parent for the subtree cascade"
        );
    }

    /// Each fork spends one unit of the parent's spawn budget, and a `fuel ==
    /// 0` agent loses the spawn tools from its view — the chain terminates by
    /// tool absence rather than recursing forever.
    #[test]
    fn fork_chain_runs_out_of_spawn_fuel() {
        let dir = tmp("spawn-fuel");
        let mut agent = Agent::for_test(&dir, "system").unwrap();
        assert_eq!(agent.fuel, SPAWN_FUEL);
        for expected in (0..SPAWN_FUEL).rev() {
            agent = agent.fork(agent.caps().clone()).expect("fork child");
            assert_eq!(agent.fuel, expected);
            assert_eq!(
                agent.fuel > 0,
                agent
                    .tools
                    .iter()
                    .any(|t| matches!(t.gate(), crate::tools::Gate::Spawns)),
                "the spawn tools track this agent's remaining fuel exactly"
            );
        }
        assert_eq!(agent.fuel, 0, "the chain must bottom out at zero, not wrap");
    }

    /// The provider is per-agent: a `fork` seeds its own handle from the
    /// parent's *current* provider, and a later swap on either never disturbs
    /// the other — the property `/model` on the focused agent relies on.
    #[test]
    fn fork_seeds_its_own_provider_handle() {
        let dir = tmp("provider-per-agent");
        let parent = Agent::for_test(&dir, "system").unwrap();
        parent.provider.swap(scripted("p-a", Script::new()));
        let child = parent.fork(parent.caps().clone()).expect("fork child");
        assert_eq!(
            child.provider.current().model(),
            "p-a",
            "the child seeds its handle from the parent's current provider"
        );
        // A later swap on the parent leaves the child's own handle untouched.
        parent.provider.swap(scripted("p-b", Script::new()));
        assert_eq!(parent.provider.current().model(), "p-b");
        assert_eq!(
            child.provider.current().model(),
            "p-a",
            "a swap on the parent never disturbs an already-forked child"
        );
    }

    #[test]
    fn remembering_fork_imports_context_before_seeded_prompt() {
        let dir = tmp("remembering-fork");
        let mut parent = Agent::for_test(&dir, "system").unwrap();
        parent.log.append_user("what did we learn?".into()).unwrap();
        parent
            .log
            .append_assistant(
                genai::chat::ChatMessage::assistant("the invariant matters"),
                vec![],
                None,
            )
            .unwrap();

        let mut child = parent
            .fork_remembering(parent.caps().clone())
            .expect("fork remembering child");
        child.log.append_user("use that invariant".into()).unwrap();
        let ms = child.log.render_messages().unwrap();
        let view = serde_json::to_string(&ms).unwrap();

        assert!(view.contains("what did we learn?"));
        assert!(view.contains("the invariant matters"));
        assert!(
            view.rfind("use that invariant") > view.rfind("the invariant matters"),
            "mnemon must put the fresh prompt at the end: {view}"
        );
    }

    /// A `reply` tool call carrying `result`.
    fn reply_call(id: &str, result: serde_json::Value) -> ToolCall {
        ToolCall {
            call_id: id.into(),
            fn_name: "reply".into(),
            fn_arguments: serde_json::json!({ "result": result }),
            thought_signatures: None,
        }
    }

    /// Drive a forked peer to quiescence through `provider`, returning the
    /// `(outcome, text)` its parent's spawn site would deliver — it renders the
    /// faithful reply payload to text exactly as the peer edge does.
    fn drive_peer(child: &mut Agent, provider: Arc<Provider>) -> (AgentOutcome, String) {
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, child.id);
        child.provider = ProviderHandle::new(provider);
        let (outcome, payload) = child.drive(&mut NoControl, &emit);
        let text = payload.as_ref().map(render_reply).unwrap_or_default();
        (outcome, text)
    }

    /// A sub-agent returns through `reply`: its markdown payload is the value
    /// the parent receives (raw, newlines intact), it settles `Complete`, and
    /// the run hard-terminates leaving the session `ReadyForUser`.
    #[test]
    fn sub_agent_returns_through_reply() {
        let dir = tmp("reply-terminal");
        let parent = Agent::for_test(&dir, "system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.seed("write a report".into());
        let provider = scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![reply_call(
                "r1",
                serde_json::json!("# Report\nline one\nline two"),
            )])),
        );
        let (outcome, text) = drive_peer(&mut child, provider);
        assert!(
            matches!(outcome, AgentOutcome::Complete),
            "a reply settles Complete, got {outcome:?}"
        );
        assert_eq!(
            text, "# Report\nline one\nline two",
            "the markdown payload passes through raw, newlines intact"
        );
        assert!(
            child.is_ready(),
            "a replied turn must leave the session ReadyForUser"
        );
    }

    /// `reply` is a settling edge for the whole owned subtree: if a parent
    /// returns before children finish, those children are cancelled and reaped
    /// rather than left registered under a dead parent.
    #[test]
    fn reply_cancels_live_descendants() {
        let dir = tmp("reply-cancels-children");
        let parent = Agent::for_test(&dir, "system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.seed("return early".into());

        let direct = fresh_id();
        let grandchild = fresh_id();
        let sibling = fresh_id();
        let direct_token = cancel::Token::new();
        let grandchild_token = cancel::Token::new();
        let sibling_token = cancel::Token::new();
        let direct_root = ral_core::process::DurableRoot::default();
        child.agents.register(
            direct,
            Some(child.id),
            "direct".into(),
            dir.join("direct"),
            direct_token.clone(),
            Some(direct_root.clone()),
            Inbox::new().mailbox(),
            child.provider.clone(),
        );
        child.agents.register(
            grandchild,
            Some(direct),
            "grandchild".into(),
            dir.join("grandchild"),
            grandchild_token.clone(),
            Some(ral_core::process::DurableRoot::default()),
            Inbox::new().mailbox(),
            child.provider.clone(),
        );
        let sibling_generation = child.agents.register(
            sibling,
            Some(parent.id),
            "sibling".into(),
            dir.join("sibling"),
            sibling_token.clone(),
            Some(ral_core::process::DurableRoot::default()),
            Inbox::new().mailbox(),
            child.provider.clone(),
        );

        let provider = scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![reply_call(
                "r1",
                serde_json::json!("done"),
            )])),
        );
        let (outcome, text) = drive_peer(&mut child, provider);

        assert!(matches!(outcome, AgentOutcome::Complete));
        assert_eq!(text, "done");
        assert!(direct_token.is_cancelled(), "direct child is cancelled");
        assert!(
            direct_root.as_scope().is_cancelled(),
            "the reap cancels the abandoned child's eval layer too"
        );
        assert!(
            grandchild_token.is_cancelled(),
            "grandchild is cancelled recursively"
        );
        assert!(
            child.agents.list(child.id).is_empty(),
            "reply reaps the abandoned subtree"
        );
        assert!(
            !sibling_token.is_cancelled(),
            "a sibling outside the replying subtree is untouched"
        );
        assert!(
            child.agents.settle(sibling, sibling_generation),
            "reply must not bump the global generation and poison siblings"
        );
    }

    /// A structured `reply` argument (object/array) is pretty-printed, so the
    /// shape stays legible in what the parent receives.
    #[test]
    fn sub_agent_reply_renders_a_structured_payload() {
        let dir = tmp("reply-structured");
        let parent = Agent::for_test(&dir, "system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.seed("list the files".into());
        let provider = scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![reply_call(
                "r1",
                serde_json::json!({ "files": ["a.rs", "b.rs"] }),
            )])),
        );
        let (outcome, text) = drive_peer(&mut child, provider);
        assert!(matches!(outcome, AgentOutcome::Complete));
        assert!(
            text.contains("\"files\""),
            "a structured reply is pretty-printed: {text}"
        );
    }

    /// A returning agent that finishes without calling `reply` is re-nudged
    /// within budget, and if it still does not reply its run settles `Failed`
    /// — `reply` is the sole return path, so a finish without it did not
    /// complete the contract, and the final prose is **not** harvested (no
    /// scrape).
    #[test]
    fn sub_agent_without_reply_is_re_nudged_then_fails() {
        let dir = tmp("reply-missing");
        let parent = Agent::for_test(&dir, "system").unwrap();
        let mut child = parent.fork(parent.caps().clone()).expect("fork child");
        child.seed("do the thing".into());
        // More prose-only replies than the nudge budget will consume: once it
        // is spent the un-replied finish is accepted and the surplus is never
        // popped (so the test does not couple to the exact budget).
        let mut script = Script::new();
        for _ in 0..8 {
            script = script.then(Reply::text("here is prose, but no reply"));
        }
        let (outcome, text) = drive_peer(&mut child, scripted("test-model", script));
        assert!(
            matches!(outcome, AgentOutcome::Failed(_)),
            "an un-replied finish settles Failed, got {outcome:?}"
        );
        assert!(
            text.is_empty(),
            "the final prose must not be scraped: {text:?}"
        );
        assert!(child.is_ready());
    }

    /// X12: a provider error mid-turn (e.g. "stream ended without End
    /// X12: a provider error mid-turn (e.g. "stream ended without End
    /// event") must not wedge the session.  When `apply` returns `Err`
    /// with the session stranded in `AwaitingAssistantAfterToolResults`,
    /// the `drive` loop quiesces per-iteration so the next prompt is
    /// admitted — not rejected with "tool results are pending".
    #[test]
    fn provider_error_mid_turn_does_not_wedge_session() {
        let dir = tmp("x12-provider-error");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        // 1st round-trip: model requests a tool call (runs to completion,
        // leaving the session `AwaitingAssistantAfterToolResults`);
        // 2nd round-trip: stream error mid-protocol;
        // 3rd–6th: clean replies for the second turn and its nudges.
        let provider = scripted(
            "test-model",
            Script::new()
                .then(Reply::tool_calls(vec![ral_call("c1", "let x12_a = 7")]))
                .then(Reply::error(ProviderError::Other(
                    "stream ended without End event".into(),
                )))
                .then(Reply::text("ok"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok"))
                .then(Reply::text("ok")),
        );
        session.provider = ProviderHandle::new(provider);
        session.seed("first turn".into());
        session.seed("second turn after error".into());
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, session.id);
        let (outcome, _) = session.drive(&mut NoControl, &emit);
        // The session is ready for the next prompt.
        assert!(
            session.is_ready(),
            "session must be ReadyForUser after a mid-turn provider error"
        );
        // The binding from the completed tool call survived.
        assert!(
            session
                .transport
                .shell_mut()
                .shell
                .scope_lookup("x12_a")
                .is_some(),
            "a binding from a completed tool call must survive a later provider error"
        );
        // The second turn ran: the final outcome is the no-reply failure
        // from the second turn's nudges, NOT a "tool results are pending"
        // rejection from a wedged session.
        match outcome {
            AgentOutcome::Failed(msg) => {
                assert!(
                    !msg.contains("tool results are pending"),
                    "second turn must not be rejected; got: {msg}"
                );
            }
            other => panic!("expected Failed from no-reply nudges, got {other:?}"),
        }
    }

    /// A settled `verify_commitment` child tagged with a passing verdict
    /// clears the protected pin when its result drains — the host-side half
    /// of the async settle, exercised directly rather than through a real
    /// spawned thread and provider round-trip.
    #[test]
    fn settle_commitment_clear_removes_the_pin() {
        let dir = tmp("settle-commitment-clear");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let key = "commitment:abc";
        session.insert_commitment_pin_for_test(
            key,
            crate::card::Card(vec![crate::card::Mark::Text {
                spans: vec![crate::card::Span {
                    role: None,
                    text: "criteria 0/1".into(),
                }],
            }]),
        );
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, session.id);
        let turn = Turn::Agent(crate::bus::AgentResult {
            id: fresh_id(),
            title: "verify-abc".into(),
            outcome: AgentOutcome::Complete,
            text: "verifier passed".into(),
            log_dir: dir,
            elapsed: std::time::Duration::from_secs(0),
            generation: 0,
            commitment_settle: Some(shell_eval::CommitmentSettle::Clear(key.to_string())),
        });
        session.settle_commitment(&turn, &emit);
        assert!(
            session.commitment_card(key).is_err(),
            "a tagged clear must remove the pin"
        );
    }

    /// A settled `commit` child tagged with a formalized card opens the
    /// protected pin when its result drains.
    #[test]
    fn settle_commitment_open_sets_the_pin() {
        let dir = tmp("settle-commitment-open");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let key = "commitment:abc";
        let card = crate::card::Card(vec![crate::card::Mark::Text {
            spans: vec![crate::card::Span {
                role: None,
                text: "tests pass".into(),
            }],
        }]);
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, session.id);
        let turn = Turn::Agent(crate::bus::AgentResult {
            id: fresh_id(),
            title: "commit-abc".into(),
            outcome: AgentOutcome::Complete,
            text: "writer formalized the commitment".into(),
            log_dir: dir,
            elapsed: std::time::Duration::from_secs(0),
            generation: 0,
            commitment_settle: Some(shell_eval::CommitmentSettle::Open {
                key: key.to_string(),
                card,
            }),
        });
        session.settle_commitment(&turn, &emit);
        assert!(
            session.commitment_card(key).is_ok(),
            "a tagged open must set the pin"
        );
    }

    /// An ordinary settled agent — no commitment tag — never touches the pin
    /// register, whether or not one happens to be live.
    #[test]
    fn settle_commitment_no_tag_is_a_no_op() {
        let dir = tmp("settle-commitment-no-tag");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let key = "commitment:abc";
        session.insert_commitment_pin_for_test(
            key,
            crate::card::Card(vec![crate::card::Mark::Text {
                spans: vec![crate::card::Span {
                    role: None,
                    text: "criteria 0/1".into(),
                }],
            }]),
        );
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, session.id);
        let turn = Turn::Agent(crate::bus::AgentResult {
            id: fresh_id(),
            title: "sub-1".into(),
            outcome: AgentOutcome::Complete,
            text: "an ordinary sub-agent's reply".into(),
            log_dir: dir,
            elapsed: std::time::Duration::from_secs(0),
            generation: 0,
            commitment_settle: None,
        });
        session.settle_commitment(&turn, &emit);
        assert!(
            session.commitment_card(key).is_ok(),
            "an untagged settle must leave any live commitment pin alone"
        );
    }

    /// [`Agent::apply_commitment_settle`] is the pinning half on its own —
    /// exercised directly, with no [`Emitter`] in sight, to pin that it needs
    /// none: an open reports the pin event only once the register actually
    /// takes it, and a clear of a key that was never live reports nothing.
    #[test]
    fn apply_commitment_settle_reports_only_a_real_change() {
        let dir = tmp("apply-commitment-settle");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let key = "commitment:abc".to_string();
        let card = crate::card::Card(vec![crate::card::Mark::Text {
            spans: vec![crate::card::Span {
                role: None,
                text: "tests pass".into(),
            }],
        }]);

        match session.apply_commitment_settle(&Some(shell_eval::CommitmentSettle::Open {
            key: key.clone(),
            card: card.clone(),
        })) {
            Some(Kind::Pin { key: k, .. }) => assert_eq!(k, key),
            Some(_) => panic!("expected a Pin event for a fresh open, got some other Kind"),
            None => panic!("expected a Pin event for a fresh open, got none"),
        }
        assert!(
            session.commitment_card(&key).is_ok(),
            "the register must hold the opened pin"
        );

        assert!(
            session
                .apply_commitment_settle(&Some(shell_eval::CommitmentSettle::Clear(
                    "commitment:never-opened".into()
                )))
                .is_none(),
            "clearing a key that was never live changes nothing, so nothing to report"
        );

        match session
            .apply_commitment_settle(&Some(shell_eval::CommitmentSettle::Clear(key.clone())))
        {
            Some(Kind::Unpin { key: k }) => assert_eq!(k, key),
            Some(_) => panic!("expected an Unpin event for a live clear, got some other Kind"),
            None => panic!("expected an Unpin event for a live clear, got none"),
        }
        assert!(
            session.commitment_card(&key).is_err(),
            "the register must have dropped the cleared pin"
        );
    }

    /// Generation admission: a worker delivers before it retires, so a
    /// result that settled across a `/clear` can reach the inbox — the drive
    /// loop must drop it before it becomes a model turn.  The script is
    /// empty, so any admitted turn would consult the provider and fail the
    /// run; a clean `Empty` quiescence proves the result never got that far.
    #[test]
    fn stale_agent_result_is_dropped_by_generation_admission() {
        let dir = tmp("generation-admission-stale");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let stale = session.agents.generation();
        session.agents.clear_subtree(session.id);
        session.inbox.push(InboxMsg::AgentResult(crate::bus::AgentResult {
            id: fresh_id(),
            title: "late".into(),
            outcome: AgentOutcome::Complete,
            text: "settled across the clear".into(),
            log_dir: dir,
            elapsed: std::time::Duration::ZERO,
            generation: stale,
            commitment_settle: None,
        }));
        session.provider = ProviderHandle::new(scripted("test-model", Script::new()));
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, session.id);
        let (outcome, payload) = session.drive(&mut NoControl, &emit);
        assert!(
            matches!(outcome, AgentOutcome::Empty),
            "a stale result must be dropped, not driven; got {outcome:?}"
        );
        assert!(payload.is_none());
        assert!(session.is_ready());
    }

    /// The admission control's positive half: a result stamped with the live
    /// generation is delivered as a turn and drives the provider.
    #[test]
    fn current_generation_agent_result_is_delivered() {
        let dir = tmp("generation-admission-live");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session.inbox.push(InboxMsg::AgentResult(crate::bus::AgentResult {
            id: fresh_id(),
            title: "worker".into(),
            outcome: AgentOutcome::Complete,
            text: "found it".into(),
            log_dir: dir,
            elapsed: std::time::Duration::ZERO,
            generation: session.agents.generation(),
            commitment_settle: None,
        }));
        session.provider = ProviderHandle::new(scripted(
            "test-model",
            Script::new().then(Reply::tool_calls(vec![reply_call(
                "r1",
                serde_json::json!("done"),
            )])),
        ));
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::new(tx, session.id);
        let (outcome, payload) = session.drive(&mut NoControl, &emit);
        assert!(
            matches!(outcome, AgentOutcome::Complete),
            "a live-generation result must be delivered; got {outcome:?}"
        );
        assert_eq!(payload, Some(serde_json::json!("done")));
    }

    // ── worker registry: `/clear` cascade, lease-reap drain ───────────────

    /// A worker body that blocks until cancelled, so a test can catch it
    /// mid-flight. Named distinctly from `agent_builtins.rs`'s own
    /// test-only blocker (`test-block-forever`) so registering both in the
    /// same test binary never collides on name.
    fn builtin_test_clear_block_forever(_args: &[Value], shell: &mut Shell) -> Settled<Value> {
        loop {
            ral_core::process::check(shell)?;
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn scheme_test_clear_block_forever(_u: &mut Unifier) -> Scheme {
        mk_scheme(&[], &[], &[], thunk(pure(Ty::Unit)))
    }

    static WORKER_REGISTRY_TEST_BUILTINS: &[BuiltinEntry] = &[BuiltinEntry {
        name: Cow::Borrowed("test-clear-block-forever"),
        type_rule: BuiltinTypeRule::Scheme(Some(0), scheme_test_clear_block_forever),
        doc: "test-only: block until cancelled.",
        body: BuiltinBody::Static(builtin_test_clear_block_forever),
    }];

    /// `/clear` cancels every worker registered on the outgoing shell before
    /// replacing it — the durable class included: explicit destruction
    /// outranks every lease — and the rebuilt shell starts with an empty
    /// registry.  A cancelled worker settling afterward still tries to
    /// flush its deferred `done` batch through the boundary it captured
    /// before the clear — the same `InboxBoundary` generation guard that
    /// already protects a stale agent result drops it, so no late
    /// `InboxMsg::Surface` reaches the rebuilt context.
    #[test]
    fn clear_cancels_registered_workers_and_drops_their_late_surface() {
        let dir = tmp("clear-cancels-workers");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        ral_core::builtins::register_builtins(WORKER_REGISTRY_TEST_BUILTINS);
        session
            .transport
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);

        // `run_shell` wires the real boundary sink — captured with `emit`'s
        // mailbox, which must be this session's own inbox for the late-surface
        // assertion below to mean anything.
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        let _ = session.run_shell("c1".into(), "spawn { test-clear-block-forever }", 30, &emit);
        let _ = session.run_shell(
            "c2".into(),
            "service { test-clear-block-forever }",
            30,
            &emit,
        );

        let entries = session.transport.shell_mut().shell.workers();
        assert_eq!(entries.len(), 2, "one ordinary worker, one service");
        let durable = entries
            .iter()
            .find(|e| e.class == ral_core::types::LeaseClass::Durable)
            .expect("the service must register under the durable class");
        assert_eq!(durable.cmd, "<service>");
        for entry in &entries {
            assert!(
                !entry.handle.cancel.is_cancelled(),
                "freshly spawned, not yet touched by /clear"
            );
        }

        let scratch = Scratch::new().expect("scratch dir");
        session.clear(&scratch).expect("clear must succeed");

        for entry in &entries {
            assert!(
                entry.handle.cancel.is_cancelled(),
                "/clear must cancel every registered worker, the durable class included ({})",
                entry.cmd
            );
        }
        assert_eq!(
            session.transport.shell_mut().shell.worker_count(),
            0,
            "the rebuilt shell's registry must start empty"
        );

        // Let the cancelled workers actually settle: each `process::check`
        // loop observes the cancellation at its next poll, then flushes its
        // deferred batch through the boundary captured before the clear.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        for entry in &entries {
            loop {
                if *entry.handle.state.lock().unwrap() != ral_core::types::HandleState::Running {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the cancelled worker must settle within the budget"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert!(
            session.inbox.is_empty(),
            "a worker settling after /clear must not post its late surface batch"
        );
    }

    /// Run `cmd` as one dispatched turn under a millisecond-scale
    /// `detached_lease`, bypassing `shell_eval::run_shell`'s hardcoded 1 h/24 h
    /// constants so a reap test doesn't need to wait out the real policy.
    /// No boundary: a lease reap never tries to deliver anything to the
    /// inbox, so the tests using this only care about the registry side
    /// effect.
    fn dispatch_with_lease(session: &Agent, cmd: &str, lease: ral_core::types::WorkerLease) {
        use ral_core::transport::{DispatchId, ReqMirror, Turn};
        use ral_core::{RequestedTerminalAccess, TurnIo, TurnStdin};
        let req = ReqMirror {
            script_name: "<test>".to_string(),
            caps: ral_core::types::Capabilities::root(),
            turn_limit: Some(std::time::Duration::from_secs(5)),
            detached_lease: Some(lease),
            worker_cap: None,
            io: TurnIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: TurnStdin::Empty,
        };
        session.transport.dispatch(
            DispatchId(0),
            Turn::Source {
                src: cmd.to_string(),
                req,
            },
        );
    }

    /// Seed one lease-chain reap (an unpolled worker under a millisecond
    /// idle bound), then drive the drain site directly: one
    /// `Kind::WorkerReaped` event carries the cmd and cause, and a second
    /// drain — the notices ledger now empty — emits nothing.
    #[test]
    fn drain_worker_reaps_emits_once_then_nothing() {
        let dir = tmp("drain-worker-reaps");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        ral_core::builtins::register_builtins(WORKER_REGISTRY_TEST_BUILTINS);
        session
            .transport
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);

        dispatch_with_lease(
            &session,
            "spawn { test-clear-block-forever }",
            ral_core::types::WorkerLease {
                idle: std::time::Duration::from_millis(40),
                backstop: std::time::Duration::from_secs(10),
            },
        );

        // Past the idle bound, unpolled: the lease chain reaps it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while session.transport.shell_mut().shell.worker_count() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the unpolled worker must be reaped within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        session.drain_worker_reaps(&emit);
        let event = rx
            .try_recv()
            .expect("the drain must emit exactly one reap event");
        match event.kind {
            Kind::WorkerReaped { cmd, cause, .. } => {
                assert_eq!(cmd, "<block>", "the reap must name the spawned body");
                assert_eq!(
                    cause,
                    ral_core::types::ReapCause::Idle,
                    "an unpolled worker past its idle bound reaps as Idle"
                );
            }
            _ => panic!("expected Kind::WorkerReaped"),
        }
        assert!(
            rx.try_recv().is_err(),
            "the drain must emit exactly one event per reap"
        );

        session.drain_worker_reaps(&emit);
        assert!(
            rx.try_recv().is_err(),
            "a second drain with no new notices must emit nothing"
        );
    }

    /// The per-agent ral-call epoch: each `run_shell` bumps it once, the
    /// post-eval sweep stamps a settled-but-unclaimed worker's entry with
    /// it, and — the sweep fast-forwarded past the retention bound through
    /// the same host door `run_shell` uses — the expiry renders through the
    /// existing drain as a `Retention`-cause `Kind::WorkerReaped`.
    #[test]
    fn run_shell_epoch_stamps_and_retention_renders_through_the_drain() {
        let dir = tmp("ral-epoch-retention");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        // Call 1: spawn an instant worker.  The call's own sweep races the
        // worker's settling, so the stamp is asserted only after call 2,
        // by which time settling is certain.
        session.run_shell("t1".into(), "spawn { return 1 }", 5, &emit);
        assert_eq!(session.ral_epoch, 1, "one call, one tick");

        let entry = session
            .transport
            .shell_mut()
            .shell
            .workers()
            .pop()
            .expect("the spawn registered its worker");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while *entry.handle.state.lock().unwrap() != ral_core::types::HandleState::Completed {
            assert!(
                std::time::Instant::now() < deadline,
                "the instant worker must settle within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        // Call 2: a trivial eval; its sweep certainly observes the settled
        // entry, so the stamp is whichever call's sweep saw it first.
        session.run_shell("t2".into(), "let _x = 1", 5, &emit);
        assert_eq!(session.ral_epoch, 2, "two calls, two ticks");
        let stamped = session
            .transport
            .shell_mut()
            .shell
            .workers()
            .pop()
            .expect("the unclaimed entry lingers");
        let s = stamped
            .settled_epoch
            .expect("the per-call sweep stamped the settled entry");
        assert!(s == 1 || s == 2, "stamped by call 1's or call 2's sweep");

        // Fast-forward the sweep past the retention bound, expiring the
        // unclaimed entry...
        session.transport.shell_mut().shell.advance_worker_epoch(
            s + shell_eval::SETTLED_WORKER_RETENTION,
            shell_eval::SETTLED_WORKER_RETENTION,
        );
        assert_eq!(session.transport.shell_mut().shell.worker_count(), 0);

        // ...and the expiry renders through the existing drain, on a fresh
        // channel so the run_shell chatter above stays out of the assert.
        let (tx2, rx2) = std::sync::mpsc::channel();
        let emit2 = Emitter::with_mailbox(tx2, session.id, session.inbox.mailbox());
        session.drain_worker_reaps(&emit2);
        let event = rx2
            .try_recv()
            .expect("the drain must emit the retention reap");
        match event.kind {
            Kind::WorkerReaped { cmd, cause, .. } => {
                assert_eq!(cmd, "<block>", "the reap names the spawned body");
                assert_eq!(
                    cause,
                    ral_core::types::ReapCause::Retention,
                    "an unclaimed settled entry expires as Retention"
                );
            }
            _ => panic!("expected Kind::WorkerReaped"),
        }
        assert!(
            rx2.try_recv().is_err(),
            "exactly one event per retention expiry"
        );
    }

    // ── `/resources`: the probe fold's agent half ─────────────────────────

    /// The row for `name`, or a panic naming what is missing.
    fn row<'a>(
        rows: &'a [crate::resources::ProbeRow],
        name: &str,
    ) -> &'a crate::resources::ProbeRow {
        rows.iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("the fold must emit a `{name}` row"))
    }

    /// The agent half of the probe fold surveys what this thread owns: the
    /// worker registry's running/settled split with time-to-reap notes, the
    /// binding count (which a `let` increments by exactly one), the inbox's
    /// per-source depths (counted, never drained), the log figures, the
    /// disk footprint, and the sub-agent ceiling's lease row.
    #[test]
    fn resource_rows_survey_the_agents_accumulators() {
        let dir = tmp("resource-rows");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        ral_core::builtins::register_builtins(WORKER_REGISTRY_TEST_BUILTINS);
        session
            .transport
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());

        // One running worker, one settled-unclaimed worker.
        session.run_shell("c1".into(), "spawn { test-clear-block-forever }", 30, &emit);
        session.run_shell("c2".into(), "spawn { return 7 }", 30, &emit);
        let settled_entry = session
            .transport
            .shell_mut()
            .shell
            .workers()
            .into_iter()
            .find(|e| e.cmd == "<block>" && e.class == ral_core::types::LeaseClass::Worker)
            .expect("both spawns registered");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let any_settled = session
                .transport
                .shell_mut()
                .shell
                .workers()
                .iter()
                .any(|e| {
                    *e.handle.state.lock().unwrap() == ral_core::types::HandleState::Completed
                });
            if any_settled {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the instant worker must settle within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let _ = settled_entry;

        // A binding is one more row in the count — measured across a `let`.
        let before = row(&session.resource_rows(), "bindings.count").current;
        session.run_shell("c3".into(), "let probe_marker = 1", 30, &emit);
        let rows = session.resource_rows();
        assert_eq!(
            row(&rows, "bindings.count").current,
            before + 1,
            "a `let` adds exactly one binding to the probe figure"
        );

        // The registry chapter: one running worker under the admission cap,
        // with the nearest-reap note; one settled entry under retention.
        let running = row(&rows, "workers.running");
        assert_eq!(running.current, 1);
        assert_eq!(running.cap, Some(64), "the admission cap is armed");
        assert_eq!(running.policy, "reject");
        let running_worker = row(&rows, "workers.running[worker]");
        assert_eq!(running_worker.current, 1);
        assert!(
            running_worker
                .note
                .as_deref()
                .is_some_and(|n| n.starts_with("nearest reap in ")),
            "a running worker carries its time-to-reap"
        );
        let settled = row(&rows, "workers.settled");
        assert_eq!(settled.current, 1);
        assert!(
            settled
                .note
                .as_deref()
                .is_some_and(|n| n.contains("ral calls")),
            "a settled entry carries its retention remaining in ral calls"
        );

        // The log, disk, and ceiling chapters.
        assert!(row(&rows, "log.events").current > 0);
        assert_eq!(row(&rows, "log.bytes").cap, Some(COMPACT_THRESHOLD as u64));
        assert!(
            row(&rows, "disk.log_dir").current > 0,
            "a session dir with a written events.json probes nonzero"
        );
        assert_eq!(row(&rows, "agents.ceiling").current, 3600);

        // The inbox chapter counts without draining: two queued messages
        // are visible in the rows, and the whole queue reads identically
        // after the probe.  (The settled spawn's deferred `Surface` batch
        // may also sit queued — a legitimate arrival, not the probe's
        // doing — so the stability check compares snapshots rather than
        // pinning the full vector.)
        session.inbox.push(InboxMsg::UserSteering("hold".into()));
        session.inbox.push(InboxMsg::Nudge("go on".into()));
        let depths_before = session.inbox.source_depths();
        let rows = session.resource_rows();
        assert_eq!(row(&rows, "inbox[user]").current, 1);
        assert_eq!(row(&rows, "inbox[user]").policy, "coalesce");
        assert_eq!(row(&rows, "inbox[nudge]").current, 1);
        assert_eq!(
            row(&rows, "inbox[agent]").current,
            0,
            "an idle source still emits its zero row — the row set is stable"
        );
        assert_eq!(
            session.inbox.source_depths(),
            depths_before,
            "probing drained nothing"
        );

        // End the blocked worker so the test does not leak a live thread.
        for entry in session.transport.shell_mut().shell.workers() {
            entry
                .handle
                .cancel
                .cancel(ral_core::process::CancelCause::Explicit);
        }
    }

    /// Probing renews nothing: assembling the rows reads a running
    /// worker's `last_observed` cell without touching it — enumeration is
    /// not observation, so `/resources` cannot immortalise the zombies it
    /// reveals.
    #[test]
    fn resource_rows_renew_no_lease() {
        let dir = tmp("resource-rows-no-renew");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        ral_core::builtins::register_builtins(WORKER_REGISTRY_TEST_BUILTINS);
        session
            .transport
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);
        let (tx, _rx) = std::sync::mpsc::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        session.run_shell("c1".into(), "spawn { test-clear-block-forever }", 30, &emit);

        let entry = session
            .transport
            .shell_mut()
            .shell
            .workers()
            .pop()
            .expect("the spawn registered its worker");
        let before = *entry.handle.last_observed.lock().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let _ = session.resource_rows();
        let after = *entry.handle.last_observed.lock().unwrap();
        assert_eq!(after, before, "the probe must not renew the lease");

        entry
            .handle
            .cancel
            .cancel(ral_core::process::CancelCause::Explicit);
    }
}
