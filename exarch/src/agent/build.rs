//! Where every [`Agent`] comes from and how it ends: `root` and `for_test`
//! build a trunk, `fork` and `branch` a child, all four through `assemble`
//! and its [`Build`] bundle. `clear` rebuilds a node's context in place
//! rather than ending it; `Drop` is the one exit every life takes.

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

/// The trunk's registry name. Never shown — the trunk lists its descendants,
/// not itself — but the entry carries its mailbox and provider for the frontend.
const TRUNK_NAME: &str = "main";

/// What `Agent::assemble` needs.  Fields are `pub(crate)` because a desk
/// handler builds a spawned child's literal from its captured `HostServices`,
/// the one place lawfully holding the adopted nursery shell; `fork_with` is
/// the ordinary in-thread path.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool sets an independent, orthogonal axis on the constructed agent (interactive, returns, allow_schedule, tool_enabled, search); not a candidate for a combined enum"
)]
pub(crate) struct Build {
    /// The still-unresolved template, carrying the builtin-index placeholder
    /// so the constructed agent's own children resolve from it in turn.
    pub(crate) system: String,
    /// `system` resolved for this bundle's own `returns`/`allow_schedule` —
    /// what reaches the model. The caller resolves it because the log's
    /// `SessionStarted` bookend records the resolved length, and the log must
    /// exist before the agent it describes does.
    pub(crate) system_prompt: String,
    /// The fleet-shared index table, carried on so this agent's own forks
    /// resolve theirs without a live shell.
    pub(crate) indexes: Arc<crate::prompt::BuiltinIndexes>,
    pub(crate) caps: ral_core::types::Capabilities,
    /// Already through its identity ceremony: `assemble` seats no engine of
    /// its own, so every construction site states which seat kind it builds.
    pub(crate) seat: Seat,
    pub(crate) log: AgentLog,
    pub(crate) parent: Option<AgentId>,
    pub(crate) fuel: u32,
    pub(crate) provider: ProviderHandle,
    pub(crate) interactive: bool,
    pub(crate) returns: bool,
    pub(crate) allow_schedule: bool,
    /// Whether provider requests advertise the `ral` tool at all — `false`
    /// only for a `--chat` trunk.
    pub(crate) tool_enabled: bool,
    /// Whether the agent may ride the provider's own hosted web search —
    /// bounded by the IT policy verdict, never a CLI flag or user config.
    pub(crate) search: bool,
    /// Fresh for the trunk, the parent's clone for a fork: one map per fleet.
    pub(crate) agents: AgentRegistry,
    /// A host setting, not a per-agent choice: every fork inherits the trunk's
    /// ceiling verbatim.
    pub(crate) disk_warn_bytes: Option<u64>,
    /// IT's network policy, audit ledger, and rate budget — likewise a host
    /// setting, inherited verbatim.
    pub(crate) egress: crate::egress::Egress,
}

/// Everything [`Agent::root`] needs beyond the seat choice and the provider.
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
    /// The depth budget this trunk starts with — `SPAWN_FUEL` from exarch's
    /// launch sites, `0` from synod: an office job is asked and answered,
    /// never a fleet.
    pub fuel: u32,
    /// IT's network policy, audit ledger, and rate budget, opened once at
    /// launch and threaded into every fork verbatim.
    pub egress: crate::egress::Egress,
}

/// Where the trunk's engine lives — [`Agent::root`]'s one construction-time
/// choice.
pub enum RootSeat {
    /// In-process, over an identity transport. `cwd` is stated rather than
    /// read from the process, since a GUI host has no per-conversation
    /// process directory to chdir into; `detach` says whether the host judged
    /// the verb meaningful here at all.
    Identity {
        scratch: Arc<Scratch>,
        cwd: std::path::PathBuf,
        detach: bool,
    },
    /// Out-of-process: an already-built `transport` onto an engine elsewhere,
    /// a spawned `--engine` child or synod's adopted control-plane stream into
    /// a guest VM. `cwd`/`home` come from the caller because under a VM the
    /// workspace is a guest path this process cannot resolve.
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
            search,
            agents,
            disk_warn_bytes,
            egress,
        } = b;
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
            nudges: tool_enabled.then(nudge::Registry::new),
            cancel: cancel::Token::new(),
            tool_enabled,
            search,
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
            context_warn_latched: false,
            egress,
        })
    }

    /// Register this agent under its `parent` (`None` for the trunk).  Only a
    /// root does this for itself, at construction; a child is registered by
    /// its spawn site, which also arms the ceiling.
    fn register_self(&self) {
        self.register_self_named(TRUNK_NAME);
    }

    /// `register_self` under a chosen name — a second self-registration
    /// sharing one registry needs one, since names are unique among live
    /// entries. The refusal is dropped: a root never collides, being the only
    /// self-registering entry its registry holds.
    pub(super) fn register_self_named(&self, name: &str) {
        let _ = self.agents.register(Registration {
            id: self.id,
            parent: self.parent,
            lease: None, // a root (trunk or headless) is never abandoned: no lease
            name: name.to_string(),
            log_dir: self.log.lock().dir().to_path_buf(),
            cancel: self.cancel.clone(),
            reach: self.seat.eval_reach(),
            mailbox: self.inbox.mailbox(),
            provider: self.provider.clone(),
        });
    }

    /// The trunk — the parent-less root of a fresh fleet.  Creates the fleet's
    /// shared registry and registers itself in it, so the frontend can build
    /// its [`Fleet`](crate::fleet::Fleet) by reading these handles back.
    /// `cfg.interactive` makes it the *conversing* trunk; off it, a one-shot
    /// headless one.
    ///
    /// # Errors
    /// If the trunk's session directory or its event log cannot be opened.
    ///
    /// # Panics
    /// Never: the `expect` below only restates for the compiler that an
    /// identity seat's shell was built a few lines above.
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
        // Read before `egress` moves into `Build` below.
        let search = egress.policy.search;
        // An identity seat resolves its builtin index off the very shell it
        // goes on to run calls through; a wire seat's real shell lives in the
        // remote engine, so `exarch_shell` dresses a throwaway with the same
        // compiled-in surface and index resolution reads that instead.
        let identity_shell = match &root_seat {
            RootSeat::Identity {
                scratch,
                cwd,
                detach,
            } => Some(seat::boot_root_shell(scratch, cwd.clone(), *detach)),
            RootSeat::Wire { .. } => None,
        };
        let sessions_root = run_dir.join("sessions");
        let id = fresh_id();
        // A binding of its own, so the stand-in outlives the borrow below.
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
            RootSeat::Identity {
                scratch,
                cwd,
                detach,
            } => Seat::identity(
                identity_shell.expect("built above for an identity seat"),
                scratch,
                cwd,
                detach,
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
            // Chat mode advertises no tool at all: a bare conversation.
            tool_enabled: !chat,
            search,
            agents: AgentRegistry::new(),
            disk_warn_bytes,
            egress,
        })?;
        agent.register_self();
        Ok(agent)
    }

    pub(crate) fn clear(&mut self) -> io::Result<()> {
        self.log.lock().clear(self.system.len())?;
        // Rebooting the seat drops the outgoing shell, whose teardown cancels
        // its registered workers — `/clear` outranks every lease.
        self.seat.clear(&self.log.lock());
        // The rebuilt context is empty: the next step's usage sets this afresh.
        self.last_input = 0;
        // A fresh context carries no pressure of its own to warn about.
        self.context_warn_latched = false;
        // Retire the subtree — this agent itself stays registered — then
        // disarm the schedules and drop the queue.  A straggler that composed
        // its message before this call carries its own stamp and is rejected
        // at a consuming edge, so no ordering among the three is load-bearing.
        self.agents.clear_subtree(self.id);
        self.schedules.clear();
        self.inbox.clear();
        // The frontend wipes its pin register on `/clear`, so the session's
        // mirror must follow.
        self.pins.lock().expect("pin register poisoned").clear();
        Ok(())
    }

    /// A plain returning fork.  Tests only: a production spawn assembles its
    /// own `Build` through the desk's spawn spine (`ExarchDesk::launch`), and
    /// `/branch` calls `fork_with` directly.
    #[cfg(test)]
    pub(crate) fn fork(&self, caps: ral_core::types::Capabilities) -> io::Result<Self> {
        self.fork_with(caps, true)
    }

    /// An independent child of this agent capped at `caps`; `returns` decides
    /// whether it holds `reply` — both the prompt's advertised builtin index
    /// and the desk's refusal read that one bit, so they cannot disagree.
    fn fork_with(&self, caps: ral_core::types::Capabilities, returns: bool) -> io::Result<Self> {
        // Scope, dynamic context, and installed builtin table cross in this
        // one step, because core owns the flow matrix — no hand-copied field
        // here can drift from it.  Per-agent state starts fresh.
        let shell = self.seat.shell_mut().shell.fork_session();
        let child_id = fresh_id();
        // Against the *child's* bits, never this agent's: a `/branch` child
        // withholds `reply` however its creator's own bit reads.
        let system_prompt = self
            .indexes
            .apply(&self.system_base, returns, self.allow_schedule);
        let log = self.log.lock().fork(child_id, system_prompt.len())?;
        let fuel = self.fuel.saturating_sub(1);
        let seat = match &self.seat {
            // No detach: `fork_session` carries no such policy across, so
            // granting it here would grant nothing now and conjure the verb
            // at the child's first `/clear`, which reboots from the seat.
            Seat::Identity { scratch, cwd, .. } => {
                Seat::identity(shell, scratch.clone(), cwd.clone(), false, &log)
            }
            Seat::Wire { .. } => {
                unreachable!("shell_mut already panicked above for a wire seat")
            }
        };
        Self::assemble(Build {
            system: self.system_base.clone(),
            system_prompt,
            indexes: self.indexes.clone(),
            caps,
            seat,
            log,
            parent: Some(self.id),
            fuel,
            // Seeded from the parent's *current* provider, so a later `/model`
            // on either never disturbs the other.
            provider: ProviderHandle::new(self.provider.current()),
            // Human-attachment is inherited; engagement is not, being a
            // per-agent registry read from the child's first exchange.
            interactive: self.interactive,
            returns,
            allow_schedule: self.allow_schedule,
            // `--chat` is trunk-only, so every fork keeps the tool.
            tool_enabled: true,
            // Never a fresh grant: a child's reach is bounded by its parent's.
            search: self.search,
            agents: self.agents.clone(),
            disk_warn_bytes: self.disk_warn_bytes,
            egress: self.egress.clone(),
        })
    }

    /// Fork a conversing child: the creator's context and capabilities
    /// verbatim, but `reply` withheld, so it parks for the human instead of
    /// returning a value.
    pub(crate) fn branch(&self) -> io::Result<Self> {
        let child = self.fork_with(self.caps.clone(), false)?;
        self.inherit_context(&child)?;
        Ok(child)
    }

    /// Import the creator's model-visible context into `child`, mnemon-style.
    fn inherit_context(&self, child: &Self) -> io::Result<()> {
        let messages = self.log.lock().inherited_context_messages();
        child
            .log
            .lock()
            .import_context(messages)
            .map_err(io::Error::other)
    }

    /// Seed a freshly forked child's inbox with its launch prompt — the spawn
    /// site calls this once and then drops its handle, so it is the only
    /// downward edge.
    pub(crate) fn seed(&self, prompt: String) {
        self.inbox.push_user(prompt);
    }

    /// A trunk against a throwaway sessions root under `dir`: unrestricted
    /// capabilities, a baked shell, and an empty [`Provider::scripted`] the
    /// harness fills.  Non-interactive, so it terminates at quiescence like
    /// any returning agent and `attend` never blocks.
    ///
    /// # Errors
    /// If the throwaway session log under `dir` cannot be created.
    ///
    /// # Panics
    /// If the test process has no cwd.
    pub fn for_test(dir: &std::path::Path, system: &str) -> io::Result<Self> {
        let shell = crate::bootstrap::boot_shell();
        let id = fresh_id();
        // Keyed by this agent's own fresh id, so concurrent tests never
        // contend on one dir.  The baked shell above is not seeded from it, so
        // probes read the absence a bare boot has.
        let scratch = Arc::new(Scratch::for_test(
            crate::bootstrap::EXARCH,
            &format!("agent-{id}"),
        )?);
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
        let seat = Seat::identity(shell, scratch, cwd, false, &log);
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
            // Matches `Egress::for_test`'s own permissive policy below.
            search: true,
            agents: AgentRegistry::new(),
            // A test exercising the disk-warn check sets this directly.
            disk_warn_bytes: None,
            egress: crate::egress::Egress::for_test(),
        })?;
        agent.register_self();
        Ok(agent)
    }
}

impl Drop for Agent {
    /// The one exit every life takes — a settle, the subtree cascade, or the
    /// trunk's own `deregister` at the end of `attend`.  A cascade cancels
    /// only an agent's *eval root*, leaving its armed schedules for whoever
    /// drops it, so they are cleared here unconditionally and its workers die
    /// with the seat below: a settled-but-never-cancelled agent leaks neither.
    /// `/clear` never reaches this — it rebuilds in place, clearing its own.
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

    /// Fuel bounds depth, not fan-out: siblings cost the parent nothing, and
    /// a chain walked all the way down bottoms out at zero rather than
    /// wrapping.
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

    /// A fork carries its parent's search reach verbatim, in both directions
    /// — the ceiling the desk's own clamp narrows a spawn against.
    #[test]
    fn fork_inherits_its_parents_search_reach() {
        let dir = tmp("fork-search");
        let mut parent = Agent::for_test(&dir, "system").unwrap();
        assert!(parent.fork(parent.caps().clone()).unwrap().search);
        parent.search = false;
        assert!(
            !parent.fork(parent.caps().clone()).unwrap().search,
            "a searchless parent can hand out no search of its own"
        );
    }

    /// The provider is per-agent: a later swap on either side never disturbs
    /// the other — what `/model` on the focused agent relies on.
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
        parent.provider.swap(scripted("p-b", Script::new()));
        assert_eq!(parent.provider.current().model(), "p-b");
        assert_eq!(
            child.provider.current().model(),
            "p-a",
            "a swap on the parent never disturbs an already-forked child"
        );
    }

    /// A branch imports the creator's context and withholds `reply`, but is
    /// otherwise an ordinary fork: caps verbatim, one less fuel.
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

    /// The index resolves from each node's own construction-fixed `returns`,
    /// not its parent's, and every node keeps the unresolved `system_base` so
    /// its children resolve afresh rather than inheriting a filtered list.
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
                egress: crate::egress::Egress::for_test(),
            },
            RootSeat::Identity {
                scratch: Arc::new(scratch),
                cwd: std::env::current_dir().expect("test process has a cwd"),
                detach: false,
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

    /// The resolved index is exactly the live shell's own surface.  Recomputes
    /// the expected names independently of `builtin_index` — shell, prelude,
    /// and library, filtered for a `for_test` agent's fixed bits — so the two
    /// cannot drift apart unnoticed.
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

    /// Read `system_prompt_bytes` off a session's `SessionStarted` bookend,
    /// the first event in its `events.json`.
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

    /// The bookend records the child's own resolved length — never a
    /// parent's, never the raw template's — checked in both directions a
    /// `returns` flip can take.
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
                egress: crate::egress::Egress::for_test(),
            },
            RootSeat::Identity {
                scratch: Arc::new(scratch),
                cwd: std::env::current_dir().expect("test process has a cwd"),
                detach: false,
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

    /// `/clear` cancels every registered worker, the durable class included,
    /// and the rebuilt shell starts empty.  A worker settling *after* the
    /// clear still flushes its batch to the inbox, stamped with its birth
    /// generation, and `Agent::admits` is the edge that rejects it.  The
    /// workers stay deaf until `CLEAR_RELEASE` so they settle past the inbox
    /// drop; settling inside the clear would have `Inbox::clear` eat the
    /// batch instead, leaving this straggler path unexercised.
    #[test]
    fn clear_cancels_registered_workers_and_drops_their_late_surface() {
        let dir = tmp("clear-cancels-workers");
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session
            .seat
            .shell_mut()
            .shell
            .install_builtins(WORKER_REGISTRY_TEST_BUILTINS);

        // The deferred sink `run_shell` wires captures `emit`'s mailbox, which
        // must be this session's own inbox for the late-surface assertion
        // below to mean anything.
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

        // The clear has fully returned — its inbox drop is behind us — so the
        // workers may now observe cancellation and flush.  Generous budget:
        // the suite runs oversubscribed in a VM.
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

    /// An agent that ends without ever being cancelled — the ordinary settle,
    /// or the trunk's end-of-`attend` `deregister` — has no cascade edge
    /// pointed at it, so `Drop` is the only thing that reaches its workers.
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

    /// A self-armed wakeup re-arms on the shared reaper for as long as its
    /// guard lives, and the registry is `Arc`-shared with the reaper's
    /// closure, so it outlives a bare drop of the `Agent`: without `Drop`'s
    /// clear, a settled agent's cron fires into an inbox nobody drains.
    #[test]
    fn agent_drop_clears_its_own_armed_schedules() {
        let dir = tmp("drop-clears-own-schedules");
        let agent = Agent::for_test(&dir, "system").unwrap();
        let schedules = agent.schedules.clone();
        schedules
            .schedule(
                crate::fleet::schedule::Trigger::After(std::time::Duration::from_hours(1)),
                "ping".into(),
                "ping".into(),
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

    /// `/clear` re-arms over a freshly booted shell, so the fresh boot's scope
    /// is the new baseline and the pre-clear binding is gone with the whole
    /// old `Shell`, ledger included.
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

    /// `assemble` arms the child over the whole scope `fork_session`
    /// snapshotted, sealing parent scratch included as its baseline: a name
    /// the parent leased is never a lease candidate in the child, however
    /// many idle calls it runs.
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

        // Re-arm with a tiny bound: `assemble` already armed this same scope
        // with the production constant, and re-arming reseals identically,
        // just fast enough to idle out inside a test.
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
