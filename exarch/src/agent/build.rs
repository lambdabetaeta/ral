//! Where every [`Agent`] comes from and how it ends.
//!
//! [`Agent::root`] builds the trunk and [`Agent::for_test`] a throwaway
//! session for the harness — both funnel through the one construction
//! step, [`Agent::assemble`], with [`Build`] bundling what it needs.
//! [`Agent::fork`] and [`Agent::branch`] are the two ways a node descends
//! into a child, sharing their core in [`Agent::fork_with`]; `/clear`
//! ([`Agent::clear`]) rebuilds a node's context in place rather than
//! ending it. `impl Drop for Agent` is the one exit every one of those
//! lives eventually takes, whatever gets it there.

use crate::agent::event::AgentLog;
use crate::agent::seat::{self, Seat};
use crate::agent::shell::LogCell;
use crate::agent::transcript::Transcript;
use crate::agent::{Agent, ProviderHandle, SPAWN_FUEL, cancel, nudge};
use crate::bootstrap::Scratch;
use crate::bus::{AgentId, Inbox};
use crate::fleet::registry::{AgentRegistry, Registration};
use crate::provider::{Provider, ProviderKind};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) fn fresh_id() -> AgentId {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

/// The trunk's registry name.  The trunk lists its descendants, never itself,
/// so this is never shown — it only fills the entry the frontend looks up by id
/// for the trunk's mailbox and provider. Since names are unique among live
/// entries ([`crate::fleet::registry::AgentRegistry::register`]), a registry
/// never holds more than one self-registered root at a time in production —
/// [`Agent::register_self`]'s one production caller per registry.
const TRUNK_NAME: &str = "main";

/// The launch configuration threaded into [`Agent::assemble`] — bundled so
/// the one constructor reads at the call site rather than as a wall of bare
/// fields.  Fields are `pub(crate)`: a desk handler assembles a spawned
/// child's [`Build`] literal directly from its captured [`crate::fleet::desk::HostServices`] (the
/// one place lawfully holding the adopted nursery shell), not through
/// [`Agent::fork`]/[`Agent::fork_with`], which stay the ordinary in-thread path.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool sets an independent, orthogonal axis on the constructed agent (interactive, returns, allow_schedule, tool_enabled); not a candidate for a combined enum"
)]
pub(crate) struct Build {
    /// The system prompt *template*: still carrying
    /// [`crate::prompt::BUILTIN_INDEX_PLACEHOLDER`] rather than a baked-in
    /// builtin index — kept as [`Agent::system_base`] for the constructed
    /// agent's own children to resolve from in turn.
    pub(crate) system: String,
    /// `system` resolved by [`crate::prompt::BuiltinIndexes::apply`] against
    /// this same [`Build`]'s shell, `returns`, and `allow_schedule` — what
    /// actually reaches the model as [`Agent::system`]. The caller resolves
    /// it because it needs the resolved length anyway, before this [`Build`]
    /// even exists: the log's [`crate::agent::event::SessionEvent::SessionStarted`] bookend records it, and the
    /// log must exist before the agent it describes does. Resolving once at
    /// the construction site keeps the bookend and the prompt the model
    /// sees structurally the same string.
    pub(crate) system_prompt: String,
    /// The fleet-shared builtin-index table [`Self::system_prompt`] was resolved
    /// from, carried on so the constructed agent's own forks resolve
    /// theirs without a shell.
    pub(crate) indexes: Arc<crate::prompt::BuiltinIndexes>,
    pub(crate) caps: ral_core::types::Capabilities,
    /// The constructed agent's seat, already through its ceremony
    /// ([`Seat::identity`]) — `assemble` seats no engines of its own, so
    /// every construction site states which seat kind it builds.
    pub(crate) seat: Seat,
    pub(crate) log: AgentLog,
    pub(crate) parent: Option<AgentId>,
    pub(crate) fuel: u32,
    pub(crate) provider: ProviderHandle,
    pub(crate) interactive: bool,
    pub(crate) returns: bool,
    pub(crate) allow_schedule: bool,
    /// Whether the constructed agent's provider requests advertise the
    /// `ral` tool at all — `false` only for a `--chat` trunk.
    pub(crate) tool_enabled: bool,
    /// The fleet's shared registry — fresh for the trunk, the parent's clone
    /// for a fork — so every node registers into one map.
    pub(crate) agents: AgentRegistry,
    /// The operator's disk-warn ceiling, threaded from [`crate::config::disk_warn_bytes`]
    /// at the trunk's construction and inherited verbatim by every fork — a
    /// host setting, not a per-agent choice.
    pub(crate) disk_warn_bytes: Option<u64>,
    /// The IT-set `fetch-url` policy, audit ledger, and rate budget,
    /// threaded from [`RootConfig::egress`] and inherited verbatim by every
    /// fork — a host setting, not a per-agent choice.
    pub(crate) egress: crate::fleet::egress::Egress,
}

/// The trunk's launch configuration.
///
/// Everything [`Agent::root`] needs that is not the seat choice or the
/// provider, bundled so the construction site reads as named facts rather
/// than a wall of positional arguments.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool sets an independent, orthogonal axis on the trunk (allow_schedule, interactive, chat); not a candidate for a combined enum"
)]
pub struct RootConfig {
    pub system: String,
    pub caps: ral_core::types::Capabilities,
    pub run_dir: std::path::PathBuf,
    pub model: String,
    pub provider_label: String,
    pub allow_schedule: bool,
    pub interactive: bool,
    pub chat: bool,
    pub disk_warn_bytes: Option<u64>,
    /// The depth budget this trunk starts with (see [`SPAWN_FUEL`]'s doc).
    /// Exarch's own launch sites pass [`SPAWN_FUEL`]; synod passes `0` — an
    /// office job is asked and answered, never a fleet.
    pub fuel: u32,
    /// The IT-set `fetch-url` policy, audit ledger, and rate budget —
    /// opened once at launch ([`crate::fleet::egress::Egress::open`]) and
    /// threaded into every fork verbatim, a host setting like
    /// `disk_warn_bytes` above.
    pub egress: crate::fleet::egress::Egress,
}

/// Where the trunk's engine lives — the one construction-time choice
/// [`Agent::root`]'s caller makes.  Each variant carries exactly what that
/// seat kind needs to boot.
pub enum RootSeat {
    /// In-process: the trunk boots its own shell from `scratch` and drives
    /// it through an identity transport. `cwd` is the caller's own, stated
    /// explicitly rather than read from the process: a GUI host has no
    /// per-conversation process directory to chdir into.
    Identity {
        scratch: Arc<Scratch>,
        cwd: std::path::PathBuf,
    },
    /// Out-of-process: the trunk drives an already-built `transport` whose
    /// engine lives elsewhere — a spawned `--engine` child or, as synod uses
    /// it, an adopted control-plane stream into a guest VM. `cwd`/`home` are
    /// the caller's own, since under a VM the workspace is a guest path this
    /// process cannot resolve for itself.
    Wire {
        transport: Box<ral_core::transport::WireTransport>,
        cwd: std::path::PathBuf,
        home: std::path::PathBuf,
    },
}

impl Agent {
    pub(crate) fn assemble(b: Build) -> io::Result<Self> {
        let Build {
            system,
            system_prompt,
            indexes,
            caps,
            seat,
            log,
            parent,
            fuel,
            provider,
            interactive,
            returns,
            allow_schedule,
            tool_enabled,
            agents,
            disk_warn_bytes,
            egress,
        } = b;
        // Every agent — the trunk and each fork, both modes — owns its trace,
        // born in the same dir as its `events.json`.
        let transcript = Transcript::create(&log.dir().join("transcript.jsonl"))?;
        Ok(Self {
            id: log.id(),
            system: system_prompt,
            system_base: system,
            indexes,
            log: LogCell::new(log),
            transcript,
            seat,
            caps,
            parent,
            fuel,
            provider,
            interactive,
            inbox: Inbox::new(),
            nudges: nudge::Registry::new(),
            cancel: cancel::Token::new(),
            tool_enabled,
            returns,
            allow_schedule,
            reply: None,
            agents,
            schedules: crate::fleet::schedule::ScheduleRegistry::new(),
            last_input: 0,
            pins: Arc::default(),
            ral_epoch: 0,
            disk_warn_bytes,
            disk_check_epoch: 0,
            disk_warn_latched: false,
            egress,
        })
    }

    /// Register this agent in the fleet registry — under its `parent` (`None`
    /// for the trunk).  The trunk calls it at construction so the frontend sees
    /// it from the start; a child is registered by its spawn site (which also
    /// arms the ceiling).  Idempotent enough: a re-register overwrites in place.
    fn register_self(&self) {
        self.register_self_named(TRUNK_NAME);
    }

    /// [`Self::register_self`], parameterised on the name. The trunk/headless
    /// root path always passes [`TRUNK_NAME`] through the unparameterised
    /// caller above; a test that wants a second, independently-named
    /// self-registration sharing one registry (so it does not collide with
    /// the root's own `"main"` entry under [`crate::fleet::registry::AgentRegistry::register`]'s
    /// name-uniqueness rule) reaches this directly. Silently drops a refusal,
    /// exactly as the discarded `Option` this returned before that rule
    /// existed — a production root's own registration never collides (it is
    /// the only self-registering entry its registry ever holds), and a test
    /// that deliberately wants to observe a collision calls
    /// [`crate::fleet::registry::AgentRegistry::register`] directly instead.
    pub(super) fn register_self_named(&self, name: &str) {
        let _ = self.agents.register(Registration {
            id: self.id,
            parent: self.parent,
            lease: None, // a root (trunk or headless) is never abandoned: no lease
            name: name.to_string(),
            log_dir: self.log.lock().dir().to_path_buf(),
            cancel: self.cancel.clone(),
            reach: self.parent.map(|_| self.seat.eval_reach()),
            mailbox: self.inbox.mailbox(),
            provider: self.provider.clone(),
        });
    }

    /// The trunk — the parent-less root of a fresh fleet.  Creates the fleet's
    /// shared registry, wraps the initial `provider`, and registers itself, so
    /// the frontend builds its [`Fleet`](crate::fleet::Fleet) by reading these
    /// handles back.  `cfg.interactive` makes it the *conversing* trunk
    /// (TUI); off it, a one-shot headless trunk.  `seat` states where the
    /// trunk's engine lives — the caller's one construction-time choice of
    /// seat kind.
    ///
    /// # Errors
    /// Returns `Err` if the trunk's session directory cannot be created or
    /// its event log cannot be opened.
    ///
    /// # Panics
    /// Never in practice: an internal `expect` asserts that the shell built
    /// above for an identity seat is still there when the seat itself is
    /// assembled a few lines later.
    pub fn root(cfg: RootConfig, root_seat: RootSeat, provider: Arc<Provider>) -> io::Result<Self> {
        let RootConfig {
            system,
            caps,
            run_dir,
            model,
            provider_label,
            allow_schedule,
            interactive,
            chat,
            disk_warn_bytes,
            fuel,
            egress,
        } = cfg;
        // Index resolution reads only the compiled-in builtin table, which
        // an identity seat's own shell and a wire seat's boot recipe dress
        // identically (`bootstrap::exarch_shell`). An identity seat resolves
        // off the very shell it goes on to run calls through; a wire seat's
        // real shell lives in the remote engine, so the shared dressing —
        // the boot recipe minus its engine-local scratch — stands in here
        // and is then discarded.
        let identity_shell = match &root_seat {
            RootSeat::Identity { scratch, cwd } => {
                Some(seat::boot_root_shell(scratch, cwd.clone()))
            }
            RootSeat::Wire { .. } => None,
        };
        let sessions_root = run_dir.join("sessions");
        let id = fresh_id();
        // This agent's own builtin index, resolved from its own `returns`/
        // `allow_schedule` bits — once, here: the bookend records its
        // length (the log must exist before the agent it describes does)
        // and `Build` carries the same string on to become `Agent::system`.
        let throwaway_wire_shell;
        let indexes =
            crate::prompt::BuiltinIndexes::resolve(if let Some(shell) = &identity_shell {
                shell
            } else {
                throwaway_wire_shell =
                    crate::bootstrap::exarch_shell(ral_core::io::TerminalState::default());
                &throwaway_wire_shell
            });
        let system_prompt = indexes.apply(&system, !interactive, allow_schedule);
        let log = AgentLog::root(
            &sessions_root,
            id,
            &model,
            &provider_label,
            system_prompt.len(),
        )?;
        let seat = match root_seat {
            RootSeat::Identity { scratch, cwd } => Seat::identity(
                identity_shell.expect("built above for an identity seat"),
                scratch,
                cwd,
                &log,
            ),
            RootSeat::Wire {
                transport,
                cwd,
                home,
            } => Seat::wire(*transport, cwd, home),
        };
        let agent = Self::assemble(Build {
            system,
            system_prompt,
            indexes,
            caps,
            seat,
            log,
            parent: None,
            fuel,
            provider: ProviderHandle::new(provider),
            interactive,
            returns: !interactive,
            allow_schedule,
            // Chat mode advertises no tool at all: a bare conversation,
            // nothing to call.  Otherwise the interactive trunk converses
            // and never returns, so it withholds `reply`; a headless trunk
            // is a returning agent.
            tool_enabled: !chat,
            agents: AgentRegistry::new(),
            disk_warn_bytes,
            egress,
        })?;
        agent.register_self();
        Ok(agent)
    }

    pub(crate) fn clear(&mut self) -> io::Result<()> {
        self.log.lock().clear(self.system.len())?;
        // The seat reboots its shell from its own scratch and re-runs the
        // identity ceremony onto the same run-scope cell; the outgoing
        // shell's teardown cancels its registered workers — `/clear`
        // outranks every lease, the durable class included, through the
        // same ownership edge every other teardown path takes.
        self.seat.clear(&self.log.lock());
        // A rebuilt context starts empty: drop the stale pressure reading so
        // the next step's usage sets it afresh.
        self.last_input = 0;
        // Retire the subtree, then disarm schedules, then empty the queue —
        // in that order, though no ordering among the three is actually
        // load-bearing any more: every producer that can compose a message
        // before this call and land it after carries its own compose-time
        // stamp and is caught at a consuming edge, not by racing this
        // sequence. An `AgentResult` and a deferred `spawn`'s surface batch
        // (`InboxDeferred`, shell_eval.rs) carry this bump's new generation
        // boundary and are rejected by `Agent::admits` if stale; a schedule
        // fire (`ScheduleRegistry::fire`, schedule.rs) carries `inbox`'s own
        // clear-epoch and is rejected at the inbox's pop boundary
        // (`bus.rs`) instead, since it never holds a handle to this
        // registry. The workers themselves are already cancelled above, on
        // the shell being retired; this is only about a straggler that was
        // already past cancellation's reach when it composed its message.
        // This agent itself stays registered — `/clear` rebuilds its
        // context, it does not tear it down.
        self.agents.clear_subtree(self.id);
        // Schedules are producers too: disarm them so no further fire is
        // even attempted. A rebuilt agent carries no pending wakeups.
        self.schedules.clear();
        // Drop every queued message and bump the inbox's own clear-epoch:
        // a rebuilt context carries neither stale user steering nor
        // non-human deliveries across the clear.
        self.inbox.clear();
        // A rebuilt context wears no pinned state: the frontend wipes its
        // register on `/clear`, so the session's mirror must follow.
        self.pins.lock().expect("pin register poisoned").clear();
        Ok(())
    }

    /// Fork an ordinary returning child at `caps`.  Production spawns
    /// assemble their own `Build` through the desk's spawn spine
    /// (`ExarchDesk::launch`) or, for `/branch`, call `Self::fork_with`
    /// directly with `returns: false`; this wrapper is exercised only by
    /// tests that want a plain returning fork.
    #[cfg(test)]
    pub(crate) fn fork(&self, caps: ral_core::types::Capabilities) -> io::Result<Self> {
        self.fork_with(caps, true)
    }

    /// The shared fork core: an independent child of this agent capped at
    /// `caps`; `returns` decides whether it holds `reply` — both the
    /// prompt's advertised builtin index and the desk's refusal read this
    /// same bit, so they cannot disagree.  [`Self::fork`] passes `true` (an
    /// ordinary returning sub-agent); [`Self::branch`] passes `false` (a
    /// conversing child that parks for the human, holding no `reply`).
    fn fork_with(&self, caps: ral_core::types::Capabilities, returns: bool) -> io::Result<Self> {
        // The child is an independent fork of the parent: it snapshots the
        // parent's scope (prelude, agent library, accumulated bindings),
        // dynamic context (cwd, env, grants), and installed builtin table (the
        // host's `view-text`/`grep-files`/`edit` and the rest), and starts
        // fresh in control counters and per-agent state — its own inbox, its own
        // (fresh) cancellation token, no terminal authority, no flow-back.
        // `fork_session` carries the whole scope and builtin table across as
        // one step because core, not this call site, owns the flow matrix —
        // there is no hand-copied field here that could fall out of sync
        // with it.  Its capabilities are supplied by the spawn site: the
        // parent's verbatim, or the parent's narrowed to a requested base
        // (`parent ⊓ base`).
        let shell = self.seat.shell_mut().shell.fork_session();
        let child_id = fresh_id();
        // The *child's* own builtin index, never `self.system`: `returns`
        // may differ from this agent's own (a `/branch` child withholds
        // `reply` however its creator's own bit reads), so the index is
        // applied to the template against the child's bits — once, here:
        // the bookend records its length (the log must exist before the
        // agent it describes does) and `Build` carries the same string on
        // to become the child's `Agent::system`.
        let system_prompt = self
            .indexes
            .apply(&self.system_base, returns, self.allow_schedule);
        let log = self.log.lock().fork(child_id, system_prompt.len())?;
        // One less than the parent's — the child's ceiling on how many more
        // generations of delegation may descend from it before the depth
        // budget bottoms out.
        let fuel = self.fuel.saturating_sub(1);
        // The child rides the same seat kind as its parent, sharing the
        // session scratch (the forked shell already inherited its seeding).
        // `shell_mut` above already panicked on a wire seat, so reaching
        // here means `self.seat` is an identity seat.
        let seat = match &self.seat {
            Seat::Identity { scratch, cwd, .. } => {
                Seat::identity(shell, scratch.clone(), cwd.clone(), &log)
            }
            Seat::Wire { .. } => {
                unreachable!("shell_mut already panicked above for a wire seat")
            }
        };
        Self::assemble(Build {
            // The unresolved template: the child's own children resolve
            // their indices from it in turn.
            system: self.system_base.clone(),
            system_prompt,
            indexes: self.indexes.clone(),
            caps,
            seat,
            log,
            // The spawning agent is the child's parent — the tree edge that
            // makes the child a node at this depth and carries the cascade.
            parent: Some(self.id),
            fuel,
            // The child seeds its own handle from the parent's current provider,
            // so a later `/model` on either never disturbs the other.
            provider: ProviderHandle::new(self.provider.current()),
            // Human-attachment is shared, not re-derived; engagement is a
            // registry read, per-agent from the moment a human exchanges a
            // message with the child, so it needs no inheritance here.
            interactive: self.interactive,
            returns,
            allow_schedule: self.allow_schedule,
            // Every agent spawns while its fuel lasts; `returns` decides whether
            // this child holds `reply`.  Self-scheduling authority is inherited
            // via `allow_schedule` above: a `--allow-schedule` trunk grants its
            // descendants the same right to wake themselves.  `--chat` is
            // trunk-only, so every fork keeps the tool.
            tool_enabled: true,
            // One shared fleet registry: the child registers into the same map,
            // so the tree is whole at any depth.
            agents: self.agents.clone(),
            // A host setting, not a per-agent choice: every fork shares the
            // trunk's ceiling verbatim.
            disk_warn_bytes: self.disk_warn_bytes,
            // Likewise a host setting: every fork shares the trunk's IT
            // policy, audit ledger, and rate budget verbatim.
            egress: self.egress.clone(),
        })
    }

    /// Fork a conversing child: the creator's context and capabilities
    /// verbatim, but `reply` withheld so it parks for the human (a /branch tab)
    /// instead of returning a value.  Mnemon-style context import.
    pub(crate) fn branch(&self) -> io::Result<Self> {
        let child = self.fork_with(self.caps.clone(), false)?;
        self.inherit_context(&child)?;
        Ok(child)
    }

    /// Import the creator's model-visible context into `child`, mnemon-style —
    /// the shared step behind [`Self::branch`].
    fn inherit_context(&self, child: &Self) -> io::Result<()> {
        let messages = self.log.lock().inherited_context_messages();
        child
            .log
            .lock()
            .import_context(messages)
            .map_err(io::Error::other)
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
    /// `tests/` uses this to drive [`Self::deliberate`] and [`Self::attend`] through
    /// a [`Provider::scripted`] backend.  Non-interactive, so it terminates at
    /// quiescence like any returning agent and `attend` never blocks.
    ///
    /// # Errors
    /// Returns `Err` if creating the throwaway session log under `dir` fails.
    ///
    /// # Panics
    /// Panics if the test process has no cwd (an environment fault, never a
    /// real condition in a test run).
    pub fn for_test(dir: &std::path::Path, system: &str) -> io::Result<Self> {
        let shell = crate::bootstrap::boot_shell();
        let id = fresh_id();
        // A real per-test scratch, keyed by this agent's own fresh id so
        // concurrent tests never contend on one dir.  The seat owns it —
        // `clear` reboots from it — but the baked test shell above is not
        // seeded from it, so probes read the same absence a bare boot has.
        let scratch = Arc::new(Scratch::for_test(
            crate::bootstrap::EXARCH,
            &format!("agent-{id}"),
        )?);
        // Resolved for the same `returns: true, allow_schedule: false` bits
        // `Build` below fixes for every `for_test` trunk, so the bookend
        // matches this agent's own `system` rather than the raw template.
        let indexes = crate::prompt::BuiltinIndexes::resolve(&shell);
        let system_prompt = indexes.apply(system, true, false);
        let log = AgentLog::root(
            &dir.join("sessions"),
            id,
            "test-model",
            "test",
            system_prompt.len(),
        )?;
        let provider = ProviderHandle::new(Arc::new(Provider::scripted(
            "test-model",
            ProviderKind::Openai,
            crate::provider::scripted::Script::new(),
        )));
        let cwd = std::env::current_dir().expect("test process has a cwd");
        let seat = Seat::identity(shell, scratch, cwd, &log);
        let agent = Self::assemble(Build {
            system: system.to_string(),
            system_prompt,
            indexes,
            caps: ral_core::types::Capabilities::default(),
            seat,
            log,
            parent: None,
            fuel: SPAWN_FUEL,
            provider,
            interactive: false,
            returns: true,
            allow_schedule: false,
            tool_enabled: true,
            agents: AgentRegistry::new(),
            // Unconfigured by default: a test that wants to exercise the
            // disk-warn check sets `session.disk_warn_bytes` directly.
            disk_warn_bytes: None,
            egress: crate::fleet::egress::Egress::for_test(),
        })?;
        agent.register_self();
        Ok(agent)
    }
}

impl Drop for Agent {
    /// The one place every teardown path funnels through, whatever got it
    /// here — a normal `reply`/settle, the subtree cascade, or the trunk's
    /// own [`crate::fleet::registry::AgentRegistry::deregister`] at end of `attend`. `/clear` never reaches this (it
    /// rebuilds the shell in place through [`Seat::clear`]), so it keeps its
    /// own explicit `schedules.clear`; every *other*
    /// teardown has no such call site of its own, and a subtree cascade
    /// ([`crate::fleet::registry::AgentRegistry::cancel`]/[`crate::fleet::registry::AgentRegistry::clear_subtree`]) only ever cancels an
    /// agent's *eval root* — which reaches a still-running worker
    /// cooperatively through the cancel-scope ancestor chain, but leaves
    /// the registry entries and any armed self-schedule sitting there until
    /// something drops them. This is that something: the agent's own
    /// workers die when its seat (and so its shell) drops below, and
    /// its schedules are cleared here unconditionally, so a
    /// settled-but-never-cancelled agent (the ordinary `reply` case)
    /// leaks neither — the ownership edge the session-ledger ADR calls for,
    /// closed once here rather than at every call site that can end an
    /// agent's life.
    fn drop(&mut self) {
        self.schedules.clear();
        let _ = self.log.lock().record_session_ended();
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::testkit::*;
    use crate::bus::{Emitter, Item};
    use crate::provider::scripted::Script;

    /// A forked child inherits the parent's installed builtin surface, not
    /// just the core set a bare `Shell::new` seeds.
    #[test]
    fn fork_inherits_host_builtins() {
        let dir = tmp("fork-builtins");
        let session = Agent::for_test(&dir, "system").unwrap();
        assert!(
            session
                .seat
                .shell_mut()
                .shell
                .lookup_builtin("view-text")
                .is_some(),
            "the parent boot shell must carry the exarch host builtins"
        );
        let child = session
            .fork(session.caps().clone())
            .expect("fork child session");
        for name in ["view-text", "grep-files", "edit-hash", "explore-dir"] {
            assert!(
                child.seat.shell_mut().shell.lookup_builtin(name).is_some(),
                "the forked child must inherit the host builtin `{name}`"
            );
        }
    }

    /// Bounds depth, not fan-out: forking three children off one parent
    /// leaves the parent's own fuel untouched, each child starting one below
    /// it — an agent may start any number of children without spending its
    /// own budget.  Walking a chain down instead still bottoms out at zero
    /// rather than wrapping, one generation at a time — the desk's own spawn
    /// spine refuses `agent-start` once an agent's fuel reads zero.
    #[test]
    fn fork_fans_out_without_spending_the_parents_fuel() {
        let dir = tmp("spawn-fuel");
        let parent = Agent::for_test(&dir, "system").unwrap();
        assert_eq!(parent.fuel, SPAWN_FUEL);
        for _ in 0..3 {
            let child = parent.fork(parent.caps().clone()).expect("fork child");
            assert_eq!(
                child.fuel,
                SPAWN_FUEL - 1,
                "each child starts one below the parent, regardless of how many siblings it has"
            );
        }
        assert_eq!(
            parent.fuel, SPAWN_FUEL,
            "fork never touches the parent's own fuel — fan-out is unbounded"
        );

        let mut agent = parent;
        for expected in (0..SPAWN_FUEL).rev() {
            agent = agent.fork(agent.caps().clone()).expect("fork child");
            assert_eq!(agent.fuel, expected);
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

    /// A branch imports the creator's context mnemon-style but withholds
    /// `reply`: it parks for the human rather than returning a value, and is
    /// otherwise an ordinary fork (creator's caps verbatim, one less fuel).
    #[test]
    fn branch_imports_context_and_withholds_reply() {
        let dir = tmp("branch-child");
        let parent = Agent::for_test(&dir, "system").unwrap();
        parent
            .log
            .lock()
            .append_user("what did we learn?".into())
            .unwrap();
        parent
            .log
            .lock()
            .append_assistant(
                genai::chat::ChatMessage::assistant("the invariant matters"),
                vec![],
                None,
            )
            .unwrap();

        let child = parent.branch().expect("branch child");

        let view = serde_json::to_string(&child.log.lock().history_messages()).unwrap();
        assert!(view.contains("what did we learn?"));
        assert!(view.contains("the invariant matters"));
        assert!(
            view.rfind("the invariant matters") > view.rfind("what did we learn?"),
            "the creator's context is imported in mnemon order: {view}"
        );

        assert!(
            !child.returns(),
            "a branch withholds `reply` and never returns"
        );
        assert_eq!(
            child.caps(),
            parent.caps(),
            "a branch inherits the creator's capabilities verbatim"
        );
        assert_eq!(
            child.fuel,
            parent.fuel - 1,
            "a branch is a fork: its fuel is one less than the parent's"
        );
    }

    /// The builtin index resolves per agent, from that agent's own
    /// construction-fixed `returns`, not the parent's it forked from: a
    /// template carrying [`crate::prompt::BUILTIN_INDEX_PLACEHOLDER`] is
    /// resolved once per node, and each node keeps the unresolved template
    /// (`system_base`) so its own children resolve from it in turn rather
    /// than inheriting an already-filtered list.
    #[test]
    fn builtin_index_resolves_per_agent_not_per_parent() {
        let dir = tmp("builtin-index-per-agent");
        let scratch = Scratch::for_test(crate::bootstrap::EXARCH, "builtin-index-per-agent")
            .expect("scratch dir");
        let template = format!(
            "persona\n\n# Builtins\n\n{}",
            crate::prompt::BUILTIN_INDEX_PLACEHOLDER
        );
        let root = Agent::root(
            RootConfig {
                system: template,
                caps: ral_core::types::Capabilities::default(),
                run_dir: dir,
                model: "test-model".into(),
                provider_label: "test".into(),
                allow_schedule: false,
                // The conversing trunk withholds `reply`.
                interactive: true,
                chat: false,
                disk_warn_bytes: None,
                fuel: SPAWN_FUEL,
                egress: crate::fleet::egress::Egress::for_test(),
            },
            RootSeat::Identity {
                scratch: Arc::new(scratch),
                cwd: std::env::current_dir().expect("test process has a cwd"),
            },
            scripted("test-model", Script::new()),
        )
        .expect("root trunk");
        assert!(
            !root.system.contains("reply"),
            "an interactive trunk's own index must not advertise `reply`: {}",
            root.system
        );
        assert!(
            root.system_base
                .contains(crate::prompt::BUILTIN_INDEX_PLACEHOLDER),
            "the base template stays unresolved so a child can resolve its own index"
        );

        let child = root.fork(root.caps().clone()).expect("fork child");
        assert!(
            child.system.contains("reply"),
            "an ordinary fork returns and must see `reply`, unlike its conversing parent: {}",
            child.system
        );

        let branch = root.branch().expect("branch child");
        assert!(
            !branch.system.contains("reply"),
            "a branch withholds `reply` just like the trunk it forked from: {}",
            branch.system
        );

        assert!(
            trunk(&tmp("builtin-index-headless-trunk"), false).returns(),
            "a headless trunk holds `reply`: returns is `!interactive`"
        );
    }

    /// The resolved index is exactly the live shell's own surface: since
    /// [`crate::prompt::builtin_index`] reads `shell.builtin_names()`
    /// directly rather than naming any static set, the two cannot drift
    /// apart. Recomputes the expected name set independently of that
    /// function — `shell.builtin_names()` plus the prelude and library
    /// sources, filtered exactly as `builtin_index` filters them for a
    /// `for_test` agent's fixed `returns: true, allow_schedule: false` —
    /// and compares it against the names actually resolved into the
    /// assembled agent's own `system`.
    #[test]
    fn builtin_index_equals_the_live_shells_own_surface() {
        let dir = tmp("builtin-index-equals-live-shell");
        let template = format!(
            "persona\n\n# Builtins\n\n{}",
            crate::prompt::BUILTIN_INDEX_PLACEHOLDER
        );
        let agent = Agent::for_test(&dir, &template).expect("test agent");

        let guard = agent.seat.shell_mut();
        let prelude = ral_core::builtins::help::prelude_names()
            .into_iter()
            .map(str::to_string);
        let library = crate::shell_eval::builtins::agent_library_docs()
            .into_iter()
            .map(|(name, _doc)| name);
        let mut expected: Vec<String> = guard
            .shell
            .builtin_names()
            .map(str::to_string)
            .chain(prelude)
            .chain(library)
            .filter(|n| !n.starts_with('_'))
            // `for_test` fixes allow_schedule: false; returns: true keeps `reply`.
            .filter(|n| !matches!(n.as_str(), "schedule" | "schedules" | "unschedule"))
            .collect();
        drop(guard);
        expected.sort_unstable();
        expected.dedup();

        let marker = "docs:\n\n";
        let names_blob = agent
            .system
            .split(marker)
            .nth(1)
            .expect("the resolved system must carry the builtin index preamble");
        let mut resolved: Vec<String> = names_blob.split(", ").map(str::to_string).collect();
        resolved.sort_unstable();
        resolved.dedup();

        assert_eq!(
            resolved, expected,
            "the resolved index must be exactly shell.builtin_names() ∪ prelude ∪ library, filtered"
        );
    }

    /// Read the recorded `system_prompt_bytes` off a session's
    /// `SessionStarted` bookend — the first event in its `events.json`.
    fn recorded_system_prompt_bytes(log_dir: &std::path::Path) -> usize {
        let body = std::fs::read_to_string(log_dir.join("events.json")).expect("events.json");
        let first: crate::agent::event::SessionEvent = serde_json::Deserializer::from_str(&body)
            .into_iter()
            .next()
            .expect("events.json must have at least one event")
            .expect("first event must parse");
        match first {
            crate::agent::event::SessionEvent::SessionStarted {
                system_prompt_bytes,
                ..
            } => system_prompt_bytes,
            other => panic!("first event must be SessionStarted, got {other:?}"),
        }
    }

    /// The `SessionStarted` bookend's `system_prompt_bytes` must be the
    /// constructed agent's own resolved `system.len()` — never a parent's
    /// resolved length, and never the raw (still-templated) length the
    /// unresolved `system_base` carries. `Agent::fork_with` is exercised
    /// twice here, once for each direction a `returns` flip can take: an
    /// ordinary fork gains `reply` its non-returning parent withheld, and a
    /// `/branch` child withholds `reply` its returning parent held.
    #[test]
    fn fork_and_branch_bookend_record_the_childs_own_resolved_length() {
        let dir = tmp("bookend-resolved-length");
        let scratch = Scratch::for_test(crate::bootstrap::EXARCH, "bookend-resolved-length")
            .expect("scratch dir");
        let template = format!(
            "persona\n\n# Builtins\n\n{}",
            crate::prompt::BUILTIN_INDEX_PLACEHOLDER
        );
        let root = Agent::root(
            RootConfig {
                system: template,
                caps: ral_core::types::Capabilities::default(),
                run_dir: dir,
                model: "test-model".into(),
                provider_label: "test".into(),
                allow_schedule: false,
                // interactive: withholds `reply`.
                interactive: true,
                chat: false,
                disk_warn_bytes: None,
                fuel: SPAWN_FUEL,
                egress: crate::fleet::egress::Egress::for_test(),
            },
            RootSeat::Identity {
                scratch: Arc::new(scratch),
                cwd: std::env::current_dir().expect("test process has a cwd"),
            },
            scripted("test-model", Script::new()),
        )
        .expect("root trunk");

        let child = root.fork(root.caps().clone()).expect("fork child");
        assert_ne!(
            child.system.len(),
            root.system.len(),
            "the fork's bits must actually differ from its parent's for \
             this test to be meaningful"
        );
        assert_eq!(
            recorded_system_prompt_bytes(&child.log_dir()),
            child.system.len(),
            "an ordinary fork's bookend must record its own resolved \
             system, not its non-returning parent's"
        );

        let grandchild = child.branch().expect("branch grandchild");
        assert_ne!(
            grandchild.system.len(),
            child.system.len(),
            "the branch's bits must actually differ from its parent's for \
             this test to be meaningful"
        );
        assert_eq!(
            recorded_system_prompt_bytes(&grandchild.log_dir()),
            grandchild.system.len(),
            "a /branch child's bookend must record its own resolved \
             system, not its returning parent's"
        );
    }

    /// `/clear` cancels every worker registered on the outgoing shell before
    /// replacing it — the durable class included: explicit destruction
    /// outranks every lease — and the rebuilt shell starts with an empty
    /// registry.  A cancelled worker settling *after the clear has finished*
    /// still flushes its deferred `done` batch through the deferred sink it
    /// captured before the clear — the batch reaches the inbox regardless
    /// (`InboxDeferred` never withholds it), stamped with its birth
    /// generation, and `Agent::admits` is the edge that rejects it, exactly
    /// as it rejects a stale agent result.  The workers run deaf to
    /// cancellation until [`CLEAR_RELEASE`] — without the latch, a worker
    /// settling *inside* the clear (between the worker cancel and the inbox
    /// drop) has its batch legitimately eaten by `Inbox::clear` instead,
    /// and the straggler path this test pins goes unexercised.
    #[test]
    fn clear_cancels_registered_workers_and_drops_their_late_surface() {
        let dir = tmp("clear-cancels-workers");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);

        // `run_shell` wires the real deferred sink — captured with `emit`'s
        // mailbox, which must be this session's own inbox for the late-surface
        // assertion below to mean anything.
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.inbox.mailbox());
        let _ = session.run_shell(
            "c1".into(),
            "spawn { test-clear-block-until-released }",
            30,
            &emit,
        );
        let _ = session.run_shell(
            "c2".into(),
            r#"service "clear-test" { test-clear-block-until-released }"#,
            30,
            &emit,
        );

        let entries = session.seat.shell_mut().shell.workers();
        assert_eq!(entries.len(), 2, "one ordinary worker, one service");
        let durable = entries
            .iter()
            .find(|e| e.class == ral_core::types::LeaseClass::Durable)
            .expect("the service must register under the durable class");
        assert_eq!(durable.cmd, "clear-test");
        for entry in &entries {
            assert!(
                !entry.handle.cancel.is_cancelled(),
                "freshly spawned, not yet touched by /clear"
            );
        }

        session.clear().expect("clear must succeed");

        for entry in &entries {
            assert!(
                entry.handle.cancel.is_cancelled(),
                "/clear must cancel every registered worker, the durable class included ({})",
                entry.cmd
            );
        }
        assert_eq!(
            probe_int(&session, "worker-count"),
            0,
            "the rebuilt shell's registry must start empty"
        );

        // The clear has fully returned — its inbox drop is behind us — so
        // release the workers: each observes the cancellation at its next
        // poll and flushes its deferred batch through the sink it captured
        // before the clear.  Generous budget: the suite runs oversubscribed
        // in a VM.
        CLEAR_RELEASE.store(true, std::sync::atomic::Ordering::Release);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
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
        let late = session
            .inbox
            .next_item()
            .expect("a worker settling after /clear still posts its late surface batch");
        assert!(
            matches!(late, Item::Surface { .. }),
            "expected the late post to surface as an Item::Surface, got {late:?}"
        );
        assert!(
            !session.admits(&late),
            "the late batch's birth generation must be rejected by the rebuilt session"
        );
    }

    /// The generation-and-cascade audit's real gap: a sub-agent that ends
    /// *without* ever being cancelled — the ordinary `reply`/settle path,
    /// or the trunk's own end-of-`attend` `deregister` — has no cascade edge
    /// pointed at it at all, so nothing upstream of `Agent`'s own `Drop`
    /// ever touches its workers. Pins the fix: dropping an `Agent` cancels
    /// every worker still registered on its own shell, regardless of why
    /// its life ended.
    #[test]
    fn agent_drop_cancels_its_own_unclosed_workers() {
        let dir = tmp("drop-cancels-own-workers");
        let mut agent = Agent::for_test(&dir, "system").unwrap();
        agent
            .seat
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);

        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, agent.id, agent.inbox.mailbox());
        let _ = agent.run_shell("c1".into(), "spawn { test-clear-block-forever }", 30, &emit);

        let entries = agent.seat.shell_mut().shell.workers();
        assert_eq!(entries.len(), 1, "the agent's own spawn must register");
        assert!(!entries[0].handle.cancel.is_cancelled(), "freshly spawned");

        drop(agent);

        assert!(
            entries[0].handle.cancel.is_cancelled(),
            "dropping the agent (settle, cancel, or deregister — however its \
             life ended) must cancel its own still-running workers"
        );
    }

    /// The schedules half of the same gap: a self-armed cron/`after` wakeup
    /// re-arms itself on the shared reaper for as long as its `Deadline`
    /// guard lives, which — since `ScheduleRegistry` is `Arc`-shared with
    /// the reaper's own closure — outlives a bare drop of the `Agent`
    /// unless something disarms it. `Agent`'s `Drop` now clears its own
    /// schedules unconditionally, the same law `/clear` already applies
    /// explicitly; without it, a settled agent's cron would keep firing
    /// into an inbox nobody drains, forever.
    #[test]
    fn agent_drop_clears_its_own_armed_schedules() {
        let dir = tmp("drop-clears-own-schedules");
        let agent = Agent::for_test(&dir, "system").unwrap();
        let schedules = agent.schedules.clone();
        schedules
            .schedule(
                crate::fleet::schedule::Trigger::After(std::time::Duration::from_hours(1)),
                "ping".into(),
                None,
                &agent.inbox.mailbox(),
            )
            .expect("a one-hour `after` trigger must arm");
        assert_eq!(schedules.list().len(), 1, "the schedule is armed");

        drop(agent);

        assert!(
            schedules.list().is_empty(),
            "dropping the agent must clear its own armed schedules, the same \
             law /clear already applies explicitly"
        );
    }

    /// `/clear` rebuilds the shell and re-arms with the production constant,
    /// sealing everything the fresh boot seeded as baseline — the pre-clear
    /// binding is gone: the whole `Shell`, ledger included, died with the
    /// old one.
    #[test]
    fn clear_reseals_baseline_and_forgets_ledger() {
        let dir = tmp("clear-reseals-baseline");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        session.run_shell("c0".into(), "let pre_clear_x = 1", 5, &emit);

        session.clear().expect("clear must succeed");

        assert!(
            !scope_has(&mut session, "pre_clear_x"),
            "the pre-clear binding must not survive the rebuild"
        );
    }

    /// `fork_session` snapshots the parent's whole scope into the child;
    /// `assemble` then arms the child, sealing *everything inherited* —
    /// parent scratch included — as the child's own baseline. A name the
    /// parent leased is therefore never a lease candidate in the child, no
    /// matter how many idle calls the child runs.
    #[test]
    fn fork_child_inherited_scratch_is_baseline() {
        let dir = tmp("fork-inherited-baseline");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        let (tx, _rx) = crate::bus::channel();
        let emit = Emitter::new(tx, session.id);
        session.run_shell("c0".into(), "let parent_scratch = 1", 5, &emit);

        let mut child = session
            .fork(ral_core::types::Capabilities::default())
            .expect("fork");
        assert!(
            scope_has(&mut child, "parent_scratch"),
            "fork_session snapshots the parent's whole scope"
        );

        // Re-arm with a tiny bound (assemble already armed the child once,
        // with the production constant, over this same inherited scope;
        // re-arming reseals identically, just faster to idle out for the
        // test) and idle it hard.
        child
            .seat
            .shell_mut()
            .shell
            .arm_binding_lease(ral_core::types::BindingLease {
                idle_calls: 1,
                large_binding_bytes: u64::MAX,
            });
        let (child_tx, _child_rx) = crate::bus::channel();
        let child_emit = Emitter::new(child_tx, child.id);
        for i in 0..3 {
            child.run_shell(format!("child{i}"), "let _child_spin = 0", 5, &child_emit);
        }
        assert!(
            scope_has(&mut child, "parent_scratch"),
            "inherited parent scratch is baseline in the child — never pruned, however \
             many boundary prunes the idle calls above ran"
        );
    }
}
