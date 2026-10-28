//! Where every [`Avatar`] comes from and how it ends: `root`, `resume`, `hatch`
//! and `for_test` build a root, `child_of` a returning child, all through
//! `assemble` and its [`Build`] bundle. `clear` rebuilds a node's context in
//! place rather than ending it; `Drop` is the one exit every life takes.

use crate::agent::EngineLost;
#[cfg(any(test, feature = "test-util"))]
use crate::agent::SPAWN_FUEL;
use crate::agent::factory::Factory;
use crate::agent::fleet::{AGENT_LEASE_IDLE, Claim, Fleet, Genesis, Launch, Unborn};
use crate::agent::index::BuiltinIndex;
use crate::agent::seat::Seat;
use crate::agent::{Agent, Avatar, Birth, LogCell, ProviderHandle, Readings, ReplyCell, Role};
use crate::app::{SCRATCH_VAR, Scratch, guest_scratch_env};
use crate::bus::{Emitter, Inbox};
use crate::provider::Provider;
use crate::provider::Toolset;
use crate::record::AgentId;
use crate::record::{AgentLog, Memory, RecordedAccount, RecordedModel, Resumed};
use crate::shell_eval::builtins;
use ral_core::authority::policy::{Capabilities, GrantStack};
use ral_core::carrier::{Severed, WireTransport};
use ral_core::data::datum::Datum as _;
use ral_core::engine::EngineInstaller;
use ral_core::own::OwnDir;
use ral_core::path::{CwdPath, EnginePath, GuestPath};
use ral_core::protocol::channel::{Liveness, WireStream};
use ral_core::protocol::{Attach, EngineSpec, Origin, Root};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub(crate) fn fresh_id() -> AgentId {
    AgentId::new(NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn seed_id_counter(sessions_root: &OwnDir) -> io::Result<()> {
    if let Some(last) = AgentLog::session_ids(sessions_root)?.last() {
        NEXT_ID.fetch_max(last.get().saturating_add(1), Ordering::Relaxed);
    }
    Ok(())
}

/// The first root's fleet name, and so the label its tab carries: the frontend
/// reads a tab's name off the agent, which is what lets `/focus` resolve it by
/// the same name every other agent answers to.
const TRUNK_NAME: &str = "main";

/// The note a resumed root's model reads first: what the record kept, and
/// what died with the process.
const RESUME_NOTE: &str = "session resumed from disk; the shell is fresh: bindings, workers, and \
    cwd from before are gone, the scratch dir is new ($EXARCH_SCRATCH is per-pid, so scratch paths \
    in the old context are dead), pinned state and scheduled events are gone (exarch-pins `list \
    and exarch-schedules `list to confirm), and any sub-agents from before have ended.";

/// What `Avatar::assemble` needs: the node's own bits, and the fleet it joins.
pub(crate) struct Build {
    /// The tab-bar identity, held before construction; the agent's name is
    /// the claim's.
    pub(crate) claim: Claim,
    /// Already through its identity ceremony: `assemble` seats no engine of
    /// its own, so every construction site states which seat kind it builds.
    pub(crate) seat: Seat,
    pub(crate) log: AgentLog,
    pub(crate) role: Role,
    pub(crate) fuel: u32,
    pub(crate) provider: ProviderHandle,
    pub(crate) search: bool,
    /// One fleet per run, shared by every root and child.
    pub(crate) fleet: Arc<Fleet>,
}

/// How a root is driven — which fixes whether it holds `reply` and whom it
/// waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trunk {
    /// A one-shot job: returns through `reply` and ends at quiescence.
    Headless,
    /// Converses — `reply` withheld, parked between messages — with a human
    /// attached who types into it: the TUI.
    Attended,
    /// Converses through an embedding host that steers it, one settled exchange
    /// at a time: synod.
    Embedded,
}

impl Trunk {
    pub(crate) fn role(self) -> Role {
        match self {
            Self::Headless => Role::Headless,
            Self::Attended | Self::Embedded => Role::Converses,
        }
    }

    pub(crate) fn attended(self) -> bool {
        matches!(self, Self::Attended)
    }
}

/// Everything a run's roots need beyond the seat choice and the provider.
pub struct RootConfig {
    pub system: String,
    /// The run's directory — the one its `sessions/` hangs under, and the
    /// one [`Avatar::resume`] reads back.
    pub run_dir: OwnDir,
    pub trunk: Trunk,
    /// What requests advertise; empty for `--chat`, whose prompt then takes
    /// no builtin surface at all.
    pub tools: Toolset,
    pub allow_schedule: bool,
    /// Whether a turn refused until a reset past the in-place wait is resumed
    /// there by a harness wakeup. Only exarch's terminal roots set it: a
    /// headless run fails fast, synod's exchange ends on the refusal, and a
    /// child fails up to its parent.
    pub resume_on_reset: bool,
    pub disk_warn_bytes: Option<u64>,
    /// The depth budget every root starts with — `SPAWN_FUEL`, the one
    /// figure both products' roots carry: every agent may delegate, and an
    /// exchange or a chain of forks ends only once the whole tree does.
    pub fuel: u32,
    /// Hosted search permission, inherited by every root and narrowed by forks.
    pub search: bool,
    /// Where a wire root gets its helper engines; `None` for every identity
    /// root. A wire root built with `fuel > 0` and no factory is refused
    /// here — never a runtime surprise reached only once a model calls
    /// `agent`.
    pub factory: Option<Arc<dyn Factory>>,
    /// Where this run's providers come from, shared by every fork.  A host
    /// that mints no providers of its own passes
    /// [`Bureau::Scripted`](crate::provider::Bureau::Scripted).
    pub bureau: Arc<crate::provider::Bureau>,
    /// The roles the human bound at launch; `Roles::default()` binds none.
    pub roles: crate::provider::roles::Roles,
}

/// Where the first root's engine lives — the root constructors' one
/// construction-time choice.
pub enum RootSeat {
    /// In-process, over an identity transport. `cwd` is stated rather than
    /// read from the process, since a GUI host has no per-conversation
    /// process directory to chdir into; the scratch is the run's, and
    /// outlives every `/clear`.
    Identity {
        scratch: Arc<Scratch>,
        cwd: CwdPath,
        /// The launch's composed layers, resolved on this host; every root's
        /// engine installs them at boot.
        grants: GrantStack,
        terminal: ral_core::io::terminal::TerminalState,
    },
    /// Out-of-process: an already-built `transport` onto an engine elsewhere,
    /// a spawned `--engine` child or synod's adopted control-plane stream into
    /// a guest VM. The `attach` is the caller's, since under a VM its paths
    /// are the guest's, which this process cannot resolve. Its authority rides
    /// `Root::config` unresolved.
    Wire {
        transport: Box<ral_core::carrier::WireTransport>,
        attach: ral_core::protocol::Attach,
    },
}

impl RootSeat {
    /// A wire root over a guest engine's control stream, its scratch named by `SCRATCH_VAR`.
    ///
    /// # Errors
    /// Returns a sentence if the control stream cannot be adopted.
    pub fn guest(
        stream: WireStream,
        cwd: GuestPath,
        scratch: GuestPath,
        ceiling: Capabilities,
    ) -> Result<Self, String> {
        let transport = WireTransport::adopt(stream, Liveness::default())
            .map_err(|e| format!("could not take control of the machine: {e}"))?;
        let root = Root::new(EnginePath::Guest(cwd))
            .with_env(guest_scratch_env(&scratch))
            .with_config(ceiling.encode());
        let attach = Attach::root(
            EngineSpec::new(builtins::INSTALLER_TAG, EnginePath::Guest(scratch)),
            root,
        );
        Ok(Self::Wire {
            transport: Box::new(transport),
            attach,
        })
    }
}

impl Fleet {
    /// Found a run's fleet: seat its first root's engine, read the builtin
    /// index and `$EXARCH_SCRATCH` off it, and settle the [`Launch`] every
    /// later root and child is made from.  The seat comes back for the caller
    /// to [`Avatar::hatch`] the first root on.
    ///
    /// # Errors
    /// A wire seat with resolved grants or spawn fuel but no engine factory,
    /// or a start failure of the engine.
    pub(crate) fn found(
        cfg: RootConfig,
        root_seat: RootSeat,
        installers: &'static [EngineInstaller],
        lease: Duration,
    ) -> io::Result<(Arc<Self>, Seat)> {
        let RootConfig {
            system,
            run_dir,
            trunk,
            tools,
            allow_schedule,
            resume_on_reset,
            disk_warn_bytes,
            fuel,
            search,
            factory,
            bureau,
            roles,
        } = cfg;
        // Stated, not discovered by a model calling `agent`: a fuelled wire
        // root with no engine factory to make helpers with cannot ever answer
        // `` exarch-agents `start ``'s wire arm, so refuse the construction itself.
        if matches!(&root_seat, RootSeat::Wire { .. }) && fuel > 0 && factory.is_none() {
            return Err(io::Error::other(
                "a wire root with spawn fuel needs an engine factory to make helper engines with: \
                 pass one via RootConfig::factory, or build this root with fuel: 0",
            ));
        }
        if let RootSeat::Wire { attach, .. } = &root_seat
            && let Origin::Root(root) = &attach.origin
            && !root.grants.is_root()
        {
            return Err(io::Error::other(
                "a wire root's attach carries resolved grants, which name objects on this host, \
                 but its engine may run on another: pass the ceiling unresolved in Root::config, \
                 for the engine's recipe to resolve where it runs",
            ));
        }
        let sessions_root = run_dir.make_dir("sessions")?;
        // A root's attach: nothing has run here yet, so this is a start
        // failure and says so, and it names the run directory it has just
        // made — which is where the engine's own output was captured, if
        // anything was. Carried whole, not flattened: synod downcasts this to
        // tell a dead engine from a log directory it could not make, and only
        // the former has a guest console worth saving.
        let lost = |s: Severed| io::Error::other(EngineLost::starting(&s, &run_dir));
        let (seat, genesis) = match root_seat {
            RootSeat::Identity {
                scratch,
                cwd,
                grants,
                terminal,
            } => {
                let genesis = Genesis {
                    installers,
                    cwd,
                    grants,
                    terminal,
                    scratch,
                };
                let seat = genesis.seat().map_err(lost)?;
                (seat, Some(genesis))
            }
            RootSeat::Wire { transport, attach } => {
                (Seat::wire(*transport, attach).map_err(lost)?, None)
            }
        };
        let index = BuiltinIndex::resolve(seat.read(|t| t.builtin_names()).map_err(lost)?);
        let scratch = seat
            .read(|t| t.env_var(SCRATCH_VAR))
            .map_err(lost)?
            .ok_or_else(|| io::Error::other(format!("the engine names no ${SCRATCH_VAR}")))?;
        let fleet = Self::new(
            Launch {
                trunk,
                allow_schedule,
                resume_on_reset,
                tools,
                disk_warn_bytes,
                factory,
                bureau,
                roles,
                system: system
                    .replace(crate::prompt::SCRATCH_PLACEHOLDER, &scratch)
                    .into(),
                index,
                fuel,
                search,
                run_dir,
                sessions_root,
                genesis,
                #[cfg(test)]
                _scratch: None,
            },
            lease,
        );
        Ok((fleet, seat))
    }
}

impl Avatar {
    /// Build an agent and its avatar, already named and enrolled in its tree.
    ///
    /// # Errors
    /// Whatever [`Fleet::enrol`] refuses.  The refusal is decided before the
    /// avatar exists, so there is nothing to unwind.
    pub(crate) fn assemble(b: Build) -> Result<Self, Unborn> {
        let Build {
            claim,
            seat,
            log,
            role,
            fuel,
            provider,
            search,
            fleet,
        } = b;
        let inbox = Inbox::new();
        let agent = Agent::new(Birth {
            id: log.id(),
            name: claim.name().to_string(),
            log_dir: log.dir().clone(),
            run_dir: fleet.launch.run_dir().clone(),
            started: Instant::now(),
            role,
            fuel,
            provider,
            search,
            reach: seat.reach(),
            mailbox: inbox.mailbox(),
        });
        let membership = fleet.enrol(&agent, claim)?;
        let readings = Readings::new(!fleet.launch.tools.is_empty());
        Ok(Self {
            work: inbox.mailbox().stamp(),
            agent,
            log: LogCell::new(log),
            seat,
            inbox,
            reply: ReplyCell::default(),
            fleet,
            readings,
            _membership: membership,
        })
    }

    /// The first root of a fresh fleet, over a fresh session log.
    ///
    /// # Errors
    /// If the root's session directory or its record cannot be opened, or
    /// its engine is lost before it starts.
    pub fn root(cfg: RootConfig, seat: RootSeat, provider: Arc<Provider>) -> io::Result<Self> {
        Self::first(
            fresh_id(),
            cfg,
            seat,
            provider,
            &crate::INSTALLERS,
            AGENT_LEASE_IDLE,
        )
    }

    /// Every root of `cfg.run_dir` that was not `/close`d, over its record
    /// replayed: each model's context survives, each shell is fresh and is
    /// told so.  The first is seated by the fleet's founding, the rest by its
    /// genesis.
    ///
    /// # Errors
    /// A chat run, which keeps no resumable history; a wire seat, whose
    /// engine process is gone; a run with no open session; or whatever
    /// [`Self::root`] refuses.
    pub fn resume(
        cfg: RootConfig,
        seat: RootSeat,
        provider: &Arc<Provider>,
    ) -> io::Result<Vec<(Self, Resumed)>> {
        if cfg.tools.is_empty() {
            return Err(io::Error::other(
                "cannot resume a chat session: chat keeps no resumable harness history",
            ));
        }
        if matches!(seat, RootSeat::Wire { .. }) {
            return Err(io::Error::other(
                "--resume is unavailable for a wire seat: the engine process is gone; resume an identity session instead",
            ));
        }
        let sessions_root = cfg.run_dir.dir("sessions")?;
        seed_id_counter(&sessions_root)?;
        let mut live = AgentLog::live_roots(&sessions_root)?.into_iter();
        let Some(first) = live.next() else {
            return Err(io::Error::other(format!(
                "cannot resume {}: no session is open there; were they all closed with /close?",
                cfg.run_dir
            )));
        };
        let (fleet, first_seat) = Fleet::found(cfg, seat, &crate::INSTALLERS, AGENT_LEASE_IDLE)?;
        let mut founding = Some(first_seat);
        std::iter::once(first)
            .chain(live)
            .map(|(id, name)| {
                let claim = fleet
                    .claim(&name)
                    .map_err(|why| io::Error::other(why.to_string()))?;
                let seat = founding.take().map_or_else(|| fleet.launch.seat(), Ok)?;
                Self::reopen(&fleet, claim, seat, id, provider.clone())
            })
            .collect()
    }

    /// Session `id`'s root, under the name `claim` holds, over its record
    /// replayed.
    fn reopen(
        fleet: &Arc<Fleet>,
        claim: Claim,
        seat: Seat,
        id: AgentId,
        provider: Arc<Provider>,
    ) -> io::Result<(Self, Resumed)> {
        let live = provider.clone();
        let (avatar, resumed) = Self::hatch(
            fleet,
            claim,
            seat,
            provider,
            |sessions, _, account, system| {
                let mut log = AgentLog::resume(sessions, id)?;
                let resumed = log.resumed_summary();
                if let Some((model, window)) = log.learned_window()
                    && model == live.model()
                {
                    live.narrow_window(*window);
                }
                log.record_resumed(
                    &RecordedModel::of(&live),
                    account,
                    system,
                    crate::app::now_unix_ms(),
                )?;
                Ok((log, resumed))
            },
        )?;
        avatar
            .log
            .borrow_mut()
            .import_note(genai::chat::ChatMessage::user(RESUME_NOTE))
            .map_err(io::Error::other)?;
        Ok((avatar, resumed))
    }

    /// A fresh fleet's first root, session `id` under the name `main`.
    pub(crate) fn first(
        id: AgentId,
        cfg: RootConfig,
        seat: RootSeat,
        provider: Arc<Provider>,
        installers: &'static [EngineInstaller],
        lease: Duration,
    ) -> io::Result<Self> {
        let (fleet, seat) = Fleet::found(cfg, seat, installers, lease)?;
        let claim = fleet
            .claim(TRUNK_NAME)
            .map_err(|why| io::Error::other(why.to_string()))?;
        Self::hatch(
            &fleet,
            claim,
            seat,
            provider,
            |sessions, model, account, system| {
                AgentLog::root(sessions, id, TRUNK_NAME, model, account, system, None)
                    .map(|log| (log, ()))
            },
        )
        .map(|(avatar, ())| avatar)
    }

    /// A root of `fleet` on `seat` under the name `claim` holds: its prompt
    /// resolved against its own grants, its log opened through `open` — which may hand back a
    /// fact of the opening beside it — and its launch recipe read off the
    /// fleet.
    ///
    /// # Errors
    /// A severed engine, a log that will not open, or whatever `open`
    /// refuses.  The caller claims the name before seating and hatching, so
    /// a refused name boots no engine and leaves no session behind.
    pub(crate) fn hatch<T>(
        fleet: &Arc<Fleet>,
        claim: Claim,
        seat: Seat,
        provider: Arc<Provider>,
        open: impl FnOnce(&OwnDir, &RecordedModel, &RecordedAccount, &str) -> io::Result<(AgentLog, T)>,
    ) -> io::Result<(Self, T)> {
        let launch = &fleet.launch;
        let role = launch.trunk.role();
        let system_prompt = launch
            .prompt_for(&seat, &role, launch.fuel, claim.name())
            .map_err(|lost| io::Error::other(EngineLost::starting(&lost, launch.run_dir())))?;
        let account = RecordedAccount::of(provider.account(), &launch.bureau.available());
        let (log, opened) = open(
            &launch.sessions_root,
            &RecordedModel::of(&provider),
            &account,
            &system_prompt,
        )?;
        let avatar = Self::assemble(Build {
            claim,
            seat,
            log,
            role,
            fuel: launch.fuel,
            provider: ProviderHandle::new(provider),
            search: launch.search,
            fleet: fleet.clone(),
        })
        .map_err(|why| io::Error::other(why.to_string()))?;
        Ok((avatar, opened))
    }

    /// A returning child of `parent` under the name `claim` holds, seated on
    /// `seat`: its fuel one less, its prompt resolved against its own grants,
    /// its log forked off `parent_log`, and — for a `mnemon` — carrying the
    /// parent's context.
    ///
    /// # Errors
    /// A severed engine, a log that will not fork, or whatever
    /// [`Fleet::enrol`] refuses; in words that read after "refused:".
    pub(crate) fn child_of(
        (parent, consumer): (&Arc<Agent>, crate::bus::Stamp),
        parent_log: &AgentLog,
        fleet: &Arc<Fleet>,
        claim: Claim,
        seat: Seat,
        provider: Arc<Provider>,
        memory: Memory,
    ) -> Result<Self, String> {
        let launch = &fleet.launch;
        let fuel = parent.fuel.saturating_sub(1);
        let role = Role::Child {
            parent: parent.clone(),
            consumer,
        };
        // Against the *child's* grants, never the parent's, so the opening
        // records the child's real prompt.  Its grant layer was resolved in
        // its own engine, so only that engine can name it.
        let system_prompt = launch
            .prompt_for(&seat, &role, fuel, claim.name())
            .map_err(|lost| EngineLost::starting(&lost, parent.run_dir()).to_string())?;
        let account = RecordedAccount::of(provider.account(), &launch.bureau.available());
        let log = parent_log
            .fork(
                fresh_id(),
                claim.name(),
                &RecordedModel::of(&provider),
                &account,
                &system_prompt,
                memory,
            )
            .map_err(|e| format!("could not fork child session log: {e}"))?;
        Self::assemble(Build {
            claim,
            seat,
            log,
            role,
            fuel,
            provider: ProviderHandle::new(provider),
            search: parent.search,
            fleet: fleet.clone(),
        })
        .map_err(|why| why.to_string())
    }

    pub(crate) fn clear(&mut self) -> io::Result<()> {
        // The queue goes first, before the seat reboot below spends real time
        // booting a shell: what is waiting *now* is old context, but a prompt
        // typed during the reboot was typed against an already-blanked screen
        // and belongs to the new one.  Sweeping it afterwards eats it in
        // silence — the queue holds no record of what it dropped.
        self.agent.forget();
        self.work = self.agent.mailbox.stamp();
        // Rebooting the seat drops the outgoing shell, whose teardown cancels
        // its registered workers — `/clear` ends them whatever their owner. First of
        // the rest, so a seat that cannot reboot refuses before anything goes.
        self.seat.clear().map_err(io::Error::other)?;
        self.log.borrow_mut().clear()?;
        // A rebuilt context has been measured and told nothing.
        self.readings = self.readings.reborn();
        Ok(())
    }

    /// `/rewind`: turn `anchor` and every turn after it cease to be, for the
    /// model and the screen alike.  Descendants and the shell are untouched.
    pub(crate) fn rewind(&mut self, anchor: u64, emit: &Emitter) -> Result<(), String> {
        // Coupled first, so the edit's record — the one notification there is
        // — publishes live.
        self.couple(emit);
        self.log.borrow_mut().rewind(anchor)?;
        // A nudge decided for the rewound prompt must neither commit nor
        // leave its edges consumed.
        self.inbox.drop_nudges();
        self.readings.nudges = self.readings.nudges.reborn();
        Ok(())
    }

    /// A plain returning fork, under a minted name, for a test.
    #[cfg(test)]
    pub(crate) fn fork(&self) -> Result<Self, String> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        self.fork_named(&format!("fork-{}", SEQ.fetch_add(1, Ordering::Relaxed)))
    }

    /// A returning child under a chosen name, for a test that goes on to
    /// name it in an assertion.  Seated fresh on the genesis, like every
    /// root, so it carries none of this shell's state.
    #[cfg(test)]
    pub(crate) fn fork_named(&self, name: &str) -> Result<Self, String> {
        let seat = self.fleet.launch.seat().map_err(|e| e.to_string())?;
        let claim = self.fleet.claim(name).map_err(|e| e.to_string())?;
        Self::child_of(
            (&self.agent, self.agent.mailbox.stamp()),
            &self.log.borrow(),
            &self.fleet,
            claim,
            seat,
            self.agent.current_provider(),
            Memory::Amnemon,
        )
    }

    /// Seed a freshly forked child's inbox with its launch prompt — the spawn
    /// site calls this once and then drops its handle, so it is the only
    /// downward edge.
    pub(crate) fn seed(&self, prompt: String) {
        self.inbox.push_user(prompt);
    }

    /// A trunk against a throwaway sessions root under `dir`: unrestricted
    /// capabilities, a baked shell, and an empty [`Provider::scripted`] the
    /// harness fills.  Headless, so it terminates at quiescence like any
    /// returning agent and `attend` never blocks.
    ///
    /// # Errors
    /// If the throwaway scratch or the session log inside it cannot be created.
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(system: &str) -> io::Result<Self> {
        Self::for_test_with(TestTrunk::new(system))
    }

    /// [`Self::for_test`] with a knob turned — the bits a launch fixes and no
    /// test can set afterwards, its `Arc` being shared the moment it exists.
    ///
    /// # Errors
    /// If the throwaway scratch or the session log inside it cannot be created.
    ///
    /// # Panics
    /// If the test process has no cwd.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn for_test_with(cfg: TestTrunk) -> io::Result<Self> {
        let TestTrunk {
            system,
            allow_schedule,
            resume_on_reset,
            search,
            disk_warn_bytes,
            lease,
            installers,
            grants,
        } = cfg;
        let id = fresh_id();
        // Keyed by this agent's own fresh id, so concurrent tests never
        // contend on one dir; the run beside it goes when the scratch does.
        let scratch = Arc::new(Scratch::for_test(
            crate::app::EXARCH,
            &format!("agent-{id}"),
        )?);
        let run_dir = scratch.test_sibling("run")?;
        let config = RootConfig {
            system,
            run_dir,
            trunk: Trunk::Headless,
            tools: Toolset::offered(false),
            allow_schedule,
            resume_on_reset,
            disk_warn_bytes,
            fuel: SPAWN_FUEL,
            search,
            factory: None,
            bureau: Arc::new(crate::provider::Bureau::Scripted),
            roles: crate::provider::roles::Roles::default(),
        };
        let seat = RootSeat::Identity {
            scratch,
            cwd: ral_core::host::cwd().expect("test process has a cwd"),
            grants,
            terminal: ral_core::io::terminal::TerminalState::default(),
        };
        let provider = Arc::new(Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new(),
        ));
        Self::first(id, config, seat, provider, installers, lease)
    }
}

/// What a unit test may vary about a [`Avatar::for_test`] trunk.
#[cfg(any(test, feature = "test-util"))]
pub(crate) struct TestTrunk {
    pub(crate) system: String,
    pub(crate) grants: GrantStack,
    pub(crate) allow_schedule: bool,
    pub(crate) resume_on_reset: bool,
    pub(crate) search: bool,
    pub(crate) disk_warn_bytes: Option<u64>,
    /// The idle bound of the fleet this trunk is born into.
    pub(crate) lease: std::time::Duration,
    /// The recipes its engine boots from.
    pub(crate) installers: &'static [ral_core::engine::EngineInstaller],
}

#[cfg(any(test, feature = "test-util"))]
impl TestTrunk {
    pub(crate) fn new(system: &str) -> Self {
        Self {
            system: system.to_string(),
            grants: GrantStack::default(),
            allow_schedule: false,
            resume_on_reset: false,
            search: true,
            disk_warn_bytes: None,
            lease: AGENT_LEASE_IDLE,
            installers: &crate::INSTALLERS,
        }
    }
}

impl Drop for Avatar {
    /// Close admission and schedules before resource teardown. Membership
    /// leaves last; retained identities remain available to tree walks.
    /// `/clear` rebuilds in place and never reaches this.
    fn drop(&mut self) {
        self.agent.mailbox.admission(|| {
            self.agent
                .token
                .cancel(ral_core::process::CancelCause::Cancelled);
        });
        self.agent.schedules.clear();
        // A panic may have poisoned the log under its guard, and a second
        // panic here would abort the process.
        if std::thread::panicking() {
            return;
        }
        self.recorder()
            .emit_or_report(crate::record::Forensic::SessionEnded);
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
