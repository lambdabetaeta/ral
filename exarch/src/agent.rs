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
/// `/quit`); `/model` swaps the [`ProviderHandle`] directly on the UI thread,
/// and view-only commands (`/help`, `/copy`, …) are handled frontend-side.
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
            tools: crate::tools::tools_for(!interactive, allow_schedule),
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
        self.replace_shell(shell);
        // A rebuilt context starts empty: drop the stale pressure reading so
        // the next turn's usage sets it afresh.
        self.last_input = 0;
        // Drop every queued message: a rebuilt context carries neither stale
        // user steering nor non-human deliveries across the clear.
        self.inbox.clear();
        // Cancel and reap this agent's subtree and advance the generation, so
        // any worker that settles after this clear drops its result rather than
        // delivering it into the rebuilt context.  This agent itself stays
        // registered — `/clear` rebuilds its context, it does not tear it down.
        self.agents.clear_subtree(self.id);
        // Drop every schedule too: a rebuilt agent carries no pending wakeups.
        self.schedules.clear();
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
        Self::assemble(Build {
            system: self.system.clone(),
            caps,
            shell,
            log,
            // The spawning agent is the child's parent — the tree edge that
            // makes the child a node at this depth and carries the cascade.
            parent: Some(self.id),
            // The child seeds its own handle from the parent's current provider,
            // so a later `/model` on either never disturbs the other.
            provider: ProviderHandle::new(self.provider.current()),
            // The fleet's focus and human-attachment are shared, not re-derived:
            // a child becomes parkable the instant the human `TAB`s to it.
            focus: self.focus.clone(),
            interactive: self.interactive,
            // Every agent spawns now; a sub-agent returns through `reply`.
            // Self-scheduling authority is inherited: a `--allow-schedule` trunk
            // grants its descendants the same right to wake themselves.
            tools: crate::tools::tools_for(true, self.grants_schedule()),
            // One shared fleet registry: the child registers into the same map,
            // so the tree is whole at any depth.
            agents: self.agents.clone(),
        })
    }

    /// A child that inherits this agent's model-visible context, then answers
    /// `prompt` as its own fresh task.  The shell/provider/capability fork is
    /// identical to [`Self::fork`]; only the model log is seeded differently.
    pub(crate) fn fork_remembering(
        &self,
        caps: ral_core::types::Capabilities,
        prompt: String,
    ) -> io::Result<Agent> {
        let mut child = self.fork(caps)?;
        child
            .log
            .import_context_then_prompt(self.log.inherited_context_messages(), prompt)
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
            provider,
            focus: Arc::new(AtomicU64::new(NO_FOCUS)),
            interactive: false,
            tools: crate::tools::tools_for(true, false),
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
            // The park verdict ([`Self::park_mode`]) is recomputed on every wake
            // inside `next_or_idle`, so a `TAB` that de-focuses this agent (and
            // wakes its inbox) flips it from `Held` to `Quiesce` and it reaps.
            let Some(turn) = self.inbox.next_or_idle(|| self.park_mode(), &self.cancel) else {
                break;
            };
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
                must_reply: self.returns() && !self.has_live_children(),
                pinned: self.pinned_digest(),
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
                                "[stream stalled mid-reply ({cause}); committed the partial \
                                 output and continuing the turn]"
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
                    "[turn truncated by the output cap mid-tool-call; dispatching the \
                     captured calls and continuing]"
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
        self.note(format!("[compacting history: {detail} → summary]"), emit);
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
                    format!("[compacted: now {} KB]", self.log.history_bytes() / 1024),
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
        Dispatch {
            results,
            // The tool-boundary drain: every message that arrived during the
            // batch — barged-in user steering, a settled subagent's result, a
            // fired wakeup, a `spawn`'s surface — tagged with its source. A
            // slash command is the lone exception, held for the turn boundary.
            injected: self.inbox.drain_tool(),
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
        emit.emit(Kind::ToolResult(content.clone()));
        SessionToolResult { id, content }
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
                .map(crate::card::summary_line)
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
    /// the protocol sits in `AwaitingAssistantAfterToolResults`), and return
    /// the payload.  `drive` then breaks the loop — `reply` hard-terminates.
    fn replied(&mut self, payload: serde_json::Value) -> Result<TurnOutcome, ProviderError> {
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
    /// just the core set a bare `Shell::new` seeds, and may itself spawn — the
    /// tree is unbounded in depth.
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
    fn remembering_fork_inherits_context_and_appends_prompt() {
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

        let child = parent
            .fork_remembering(parent.caps().clone(), "use that invariant".into())
            .expect("fork remembering child");
        let ms = child.log.render_messages().unwrap();
        let view = serde_json::to_string(&ms).unwrap();

        assert!(view.contains("what did we learn?"));
        assert!(view.contains("the invariant matters"));
        assert!(
            view.rfind("use that invariant") > view.rfind("the invariant matters"),
            "anamnesis must put the fresh prompt at the end: {view}"
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
}
