//! Where every [`Avatar`] comes from and how it ends: `root`, `resume` and
//! `for_test` build a trunk, `fork` and `branch` a child, all through
//! `assemble` and its [`Build`] bundle. `clear` rebuilds a node's context in
//! place rather than ending it; `Drop` is the one exit every life takes.

use crate::agent::EngineLost;
use crate::agent::desk::Fork;
use crate::agent::factory::Factory;
use crate::agent::fleet::{AGENT_LEASE_IDLE, Fleet, Launch, Unborn};
use crate::agent::index::{BuiltinIndex, Grants};
use crate::agent::seat::Seat;
use crate::agent::{
    Agent, Avatar, Birth, LogCell, ProviderHandle, Readings, ReplyCell, Role, SPAWN_FUEL,
};
use crate::app::Scratch;
use crate::bus::{Emitter, Inbox};
use crate::provider::Provider;
use crate::provider::Toolset;
use crate::record::AgentId;
use crate::record::{AgentLog, RecordedAccount, RecordedModel, Resumed};
use ral_core::carrier::Severed;
use ral_core::engine::EngineInstaller;
use ral_core::protocol::{Ending, Report};
use ral_core::sync::LockExt;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) fn fresh_id() -> AgentId {
    AgentId::new(NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn seed_id_counter(sessions_root: &Path) -> io::Result<()> {
    let max = match std::fs::read_dir(sessions_root) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                entry.file_type().ok().filter(std::fs::FileType::is_dir)?;
                entry.file_name().to_str()?.parse::<u64>().ok()
            })
            .max()
            .unwrap_or(0),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    NEXT_ID.fetch_max(max.saturating_add(1), Ordering::Relaxed);
    Ok(())
}

/// The trunk's fleet name, and so the label its tab carries: the frontend reads
/// the root tab's name off the agent, which is what lets `/focus` resolve the
/// trunk by the same name every other agent answers to.
const TRUNK_NAME: &str = "main";

/// The note a resumed trunk's model reads first: what the record kept, and
/// what died with the process.
const RESUME_NOTE: &str = "session resumed from disk; the shell is fresh: bindings, workers, and \
    cwd from before are gone, the scratch dir is new ($EXARCH_SCRATCH is per-pid, so scratch paths \
    in the old context are dead), pinned state and scheduled events are gone (exarch-pins `list \
    and exarch-schedules `list to confirm), and any sub-agents from before have ended.";

/// What `Avatar::assemble` needs: the node's own bits, and the fleet it joins.
/// Fields are `pub(crate)` because a desk handler builds every child's literal
/// from its captured `HostServices`, where the adopted fork is seated.
pub(crate) struct Build {
    /// The tab-bar identity — known to every caller before construction, `/branch`'s
    /// and `` exarch-agents `start ``'s own choice reached down to here.
    pub(crate) name: String,
    /// The launch's template resolved for this node's own [`Grants`] — what
    /// reaches the model.  The caller resolves it because the log's
    /// `SessionStarted` bookend records the resolved length, and the log must
    /// exist before the agent it describes does.
    pub(crate) system_prompt: String,
    pub(crate) caps: ral_core::capability::GrantStack,
    /// Already through its identity ceremony: `assemble` seats no engine of
    /// its own, so every construction site states which seat kind it builds.
    pub(crate) seat: Seat,
    pub(crate) log: AgentLog,
    pub(crate) role: Role,
    pub(crate) fuel: u32,
    pub(crate) provider: ProviderHandle,
    pub(crate) search: bool,
    /// Fresh for the trunk, the parent's clone for a fork: one fleet per run.
    pub(crate) fleet: Arc<Fleet>,
}

/// Long enough for a fork, never a turn's worth of wall.
const BRANCH_TIMEOUT_SECS: u64 = 30;

/// A run's refusal in its own words; `None` for one that settled.
fn refusal(report: Report) -> Option<String> {
    match report {
        Report::Ran {
            ending: Ending::Settled { .. },
            ..
        } => None,
        Report::Ran {
            ending:
                Ending::Raised { rendered, .. }
                | Ending::Stopped { rendered, .. }
                | Ending::Unreturnable { rendered, .. },
            ..
        }
        | Report::Static { rendered, .. } => Some(rendered),
        Report::Ran {
            ending: Ending::Exited(status),
            ..
        } => Some(format!("the fork's run exited with status {status}")),
    }
}

/// How a trunk is driven — which fixes whether it holds `reply` and whom it
/// waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trunk {
    /// A one-shot job: returns through `reply` and ends at quiescence.
    Headless,
    /// Converses — `reply` withheld, parked between messages — with a human
    /// attached who types into it: the TUI.
    Attended,
    /// Converses, driven one exchange at a time by an embedder with no human
    /// at the inbox: synod.
    Embedded,
}

impl Trunk {
    fn role(self) -> Role {
        match self {
            Self::Headless => Role::Headless,
            Self::Attended | Self::Embedded => Role::Converses,
        }
    }

    fn attended(self) -> bool {
        matches!(self, Self::Attended)
    }
}

/// Everything a trunk needs beyond the seat choice and the provider.
pub struct RootConfig {
    pub system: String,
    pub caps: ral_core::capability::GrantStack,
    /// The run's directory — the one its `sessions/` hangs under, and the
    /// one [`Avatar::resume`] reads back.
    pub run_dir: std::path::PathBuf,
    pub account: RecordedAccount,
    pub trunk: Trunk,
    /// What requests advertise; empty for `--chat`, whose prompt then takes
    /// no builtin surface at all.
    pub tools: Toolset,
    pub allow_schedule: bool,
    /// Whether a turn refused until a reset past the in-place wait is resumed
    /// there by a harness wakeup. Only exarch's terminal trunk sets it: a
    /// headless run fails fast, synod's exchange ends on the refusal, and a
    /// child fails up to its parent.
    pub resume_on_reset: bool,
    pub disk_warn_bytes: Option<u64>,
    /// The depth budget this trunk starts with — `SPAWN_FUEL`, the one
    /// figure both products' trunks carry: every agent may delegate, and an
    /// exchange or a chain of forks ends only once the whole tree does.
    pub fuel: u32,
    /// The network policy, whose `search` verdict is the trunk's reach and
    /// the ceiling every fork narrows under.
    pub egress: crate::egress::Egress,
    /// Where a wire trunk gets its helper engines; `None` for every identity
    /// trunk. A wire trunk built with `fuel > 0` and no factory is refused
    /// here — never a runtime surprise reached only once a model calls
    /// `agent`.
    pub factory: Option<Arc<dyn Factory>>,
    /// Where this trunk's providers come from, shared by every fork.  A host
    /// that mints no providers of its own passes
    /// [`Bureau::Scripted`](crate::provider::Bureau::Scripted).
    pub bureau: Arc<crate::provider::Bureau>,
}

/// Where the trunk's engine lives — the trunk constructors' one
/// construction-time choice.
pub enum RootSeat {
    /// In-process, over an identity transport. `cwd` is stated rather than
    /// read from the process, since a GUI host has no per-conversation
    /// process directory to chdir into; the scratch is the session's, and
    /// outlives every `/clear`.
    Identity {
        scratch: Arc<Scratch>,
        cwd: std::path::PathBuf,
        terminal: ral_core::terminal::TerminalState,
    },
    /// Out-of-process: an already-built `transport` onto an engine elsewhere,
    /// a spawned `--engine` child or synod's adopted control-plane stream into
    /// a guest VM. The `attach` is the caller's, since under a VM its paths
    /// are the guest's, which this process cannot resolve.
    Wire {
        transport: Box<ral_core::carrier::WireTransport>,
        attach: ral_core::protocol::Attach,
    },
}

impl Avatar {
    /// Build an agent and its avatar in one step: the agent is in its parent's
    /// subtree, holds its name, and carries a lease before this returns, so no
    /// caller can ever hold an unenrolled child.
    ///
    /// # Errors
    /// Whatever [`Fleet::enrol`] refuses.  The refusal is decided before the
    /// avatar exists, so there is nothing to unwind.
    pub(crate) fn assemble(b: Build) -> Result<Self, Unborn> {
        let Build {
            name,
            system_prompt,
            caps,
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
            name,
            log_dir: log.dir().to_path_buf(),
            started: Instant::now(),
            system: system_prompt.into(),
            caps,
            role,
            fuel,
            provider,
            search,
            reach: seat.reach(),
            mailbox: inbox.mailbox(),
        });
        fleet.enrol(&agent)?;
        let readings = Readings::new(!fleet.launch.tools.is_empty());
        Ok(Self {
            agent,
            log: LogCell::new(log),
            seat,
            inbox,
            reply: ReplyCell::default(),
            fleet,
            readings,
        })
    }

    /// The trunk — the root of a fresh fleet, over a fresh session log.
    ///
    /// # Errors
    /// If the trunk's session directory or its record cannot be opened, or
    /// its engine is lost before it starts.
    pub fn root(cfg: RootConfig, seat: RootSeat, provider: Arc<Provider>) -> io::Result<Self> {
        let id = fresh_id();
        Self::trunk(
            cfg,
            seat,
            provider,
            id,
            &crate::INSTALLERS,
            AGENT_LEASE_IDLE,
            |sessions, model, account, bytes| {
                AgentLog::root(sessions, id, model, account, bytes).map(|log| (log, ()))
            },
        )
        .map(|(avatar, ())| avatar)
    }

    /// The trunk, over session 0's record in `cfg.run_dir` replayed: the
    /// model's context survives, the shell is fresh and is told so.
    ///
    /// # Errors
    /// A chat trunk, which keeps no resumable history; a wire seat, whose
    /// engine process is gone; or whatever [`Self::root`] refuses.
    pub fn resume(
        cfg: RootConfig,
        seat: RootSeat,
        provider: Arc<Provider>,
    ) -> io::Result<(Self, Resumed)> {
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
        seed_id_counter(&cfg.run_dir.join("sessions"))?;
        let live = provider.clone();
        let (avatar, resumed) = Self::trunk(
            cfg,
            seat,
            provider,
            AgentId::new(0),
            &crate::INSTALLERS,
            AGENT_LEASE_IDLE,
            |sessions, _, account, bytes| {
                let mut log = AgentLog::resume(sessions, AgentId::new(0))?;
                let resumed = log.resumed_summary();
                if let Some((model, window)) = log.learned_window()
                    && model == live.model()
                {
                    live.narrow_window(*window);
                }
                log.record_resumed(
                    &RecordedModel::of(&live),
                    account,
                    bytes,
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

    /// Seat the trunk's engine, resolve its prompt against it, open its log
    /// through `open` — which may hand back a fact of the opening beside it —
    /// and found the fleet around it.
    fn trunk<T>(
        cfg: RootConfig,
        root_seat: RootSeat,
        provider: Arc<Provider>,
        id: AgentId,
        installers: &'static [EngineInstaller],
        lease: Duration,
        open: impl FnOnce(&Path, &RecordedModel, &RecordedAccount, usize) -> io::Result<(AgentLog, T)>,
    ) -> io::Result<(Self, T)> {
        let RootConfig {
            system,
            caps,
            run_dir,
            account,
            trunk,
            tools,
            allow_schedule,
            resume_on_reset,
            disk_warn_bytes,
            fuel,
            egress,
            factory,
            bureau,
        } = cfg;
        // Stated, not discovered by a model calling `agent`: a fuelled wire
        // trunk with no engine factory to make helpers with cannot ever answer
        // `` exarch-agents `start ``'s wire arm, so refuse the construction itself.
        if matches!(&root_seat, RootSeat::Wire { .. }) && fuel > 0 && factory.is_none() {
            return Err(io::Error::other(
                "a wire trunk with spawn fuel needs an engine factory to make helper engines with: \
                 pass one via RootConfig::factory, or build this trunk with fuel: 0",
            ));
        }
        let sessions_root = run_dir.join("sessions");
        // A trunk's attach: nothing has run here yet, so this is a start
        // failure and says so, and it names the run directory it has just
        // made — which is where the engine's own output was captured, if
        // anything was. Carried whole, not flattened: synod downcasts this to
        // tell a dead engine from a log directory it could not make, and only
        // the former has a guest console worth saving.
        let lost = |s: Severed| io::Error::other(EngineLost::starting(&s, Some(&run_dir)));
        let seat = match root_seat {
            RootSeat::Identity {
                scratch,
                cwd,
                terminal,
            } => Seat::root(
                installers,
                cwd,
                terminal,
                scratch,
                &AgentLog::dir_of(&sessions_root, id),
            )
            .map_err(lost)?,
            RootSeat::Wire { transport, attach } => Seat::wire(*transport, attach).map_err(lost)?,
        };
        let index = BuiltinIndex::resolve(seat.read(|t| t.builtin_names()).map_err(lost)?);
        let scratch_var = crate::app::EXARCH.scratch_var();
        let scratch = seat
            .read(|t| t.env_var(&scratch_var))
            .map_err(lost)?
            .ok_or_else(|| io::Error::other(format!("the engine names no ${scratch_var}")))?;
        let system: Arc<str> = system
            .replace(crate::prompt::SCRATCH_PLACEHOLDER, &scratch)
            .into();
        let own =
            crate::prompt::with_grant(&system, &seat.read(|t| t.grants()).map_err(lost)?, &caps);
        let role = trunk.role();
        // Chat advertises no tool, so its prompt takes no builtin surface at
        // all — no index, no taught section, the bare stand-in verbatim.
        let system_prompt = if tools.is_empty() {
            own
        } else {
            index.apply(
                &own,
                &Grants {
                    role: &role,
                    allow_schedule,
                    spawns: fuel > 0,
                },
                TRUNK_NAME,
            )
        };
        let (log, opened) = open(
            &sessions_root,
            &RecordedModel::of(&provider),
            &account,
            system_prompt.len(),
        )?;
        // The policy's one verdict the tree reads; the ledger is the host's.
        let search = egress.policy.search;
        let fleet = Fleet::new(
            Launch {
                attended: trunk.attended(),
                allow_schedule,
                resume_on_reset,
                tools,
                disk_warn_bytes,
                factory,
                bureau,
                system,
                index,
            },
            lease,
        );
        // A brand-new fleet, so this can never collide or find its rootless
        // self dead.
        let avatar = Self::assemble(Build {
            name: TRUNK_NAME.to_string(),
            system_prompt,
            caps,
            seat,
            log,
            role,
            fuel,
            provider: ProviderHandle::new(provider),
            search,
            fleet,
        })
        .expect("a fresh fleet's trunk is born unrefused");
        Ok((avatar, opened))
    }

    pub(crate) fn clear(&mut self) -> io::Result<()> {
        let at_unix_ms = crate::app::now_unix_ms();
        // The queue goes first, before the seat reboot below spends real time
        // booting a shell: what is waiting *now* is old context, but a prompt
        // typed during the reboot was typed against an already-blanked screen
        // and belongs to the new one.  Sweeping it afterwards eats it in
        // silence — the queue holds no record of what it dropped.
        self.inbox.clear();
        // Rebooting the seat drops the outgoing shell, whose teardown cancels
        // its registered workers — `/clear` outranks every lease. First of
        // the rest, so a seat that cannot reboot refuses before anything goes.
        self.seat.clear().map_err(io::Error::other)?;
        // Abandon the subtree — this agent itself stays live — before the
        // segment rotates below, so no live descendant can still resolve an
        // ancestry link into a replaced file.
        self.agent.forget();
        let record = self
            .log
            .borrow_mut()
            .clear(self.agent.system.len(), at_unix_ms)?;
        // A rebuilt context has been measured and told nothing.
        self.readings = self.readings.reborn();
        record.rotation_error.map_or(Ok(()), Err)
    }

    /// `/rewind`: turn `anchor` and every turn after it cease to be, for the
    /// model and the screen alike.  Descendants and the shell are untouched.
    pub(crate) fn rewind(&mut self, anchor: u64, emit: &Emitter) -> Result<(), String> {
        // Coupled first, so the edit's record — the one notification there is
        // — publishes live.
        self.couple(emit);
        self.log.borrow_mut().rewind(anchor)?;
        self.recorder()
            .emit(crate::record::Display::Rewound { anchor })
            .map(drop)
            .map_err(|e| e.to_string())?;
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

    /// A returning fork under a chosen name, for a test that goes on to name
    /// the child in an assertion.
    #[cfg(test)]
    pub(crate) fn fork_named(&self, name: &str) -> Result<Self, String> {
        let (emit, _rx) = crate::bus::dummy_emitter();
        self.fork_with(
            name.to_string(),
            Fork::Returning(crate::enquiry::Memory::Amnemon),
            &emit,
        )
    }

    /// Fork a conversing child under `name`: the creator's context and
    /// authority verbatim, but `reply` withheld, so it parks for the human
    /// instead of returning a value.
    ///
    /// # Errors
    /// Whatever [`Self::fork_with`] refuses.
    pub(crate) fn branch(&self, name: String, emit: &Emitter) -> Result<Self, String> {
        self.fork_with(name, Fork::Conversing, emit)
    }

    /// A child of this agent, forked by its own engine the way a spawn is —
    /// `_exarch-branch`, answered by the desk — so no shell ever crosses here.
    fn fork_with(&self, name: String, fork: Fork, emit: &Emitter) -> Result<Self, String> {
        let lost = |s: Severed| EngineLost::running(&s, self.agent.run_dir()).to_string();
        let order = Arc::new(crate::agent::desk::BranchOrder {
            name,
            fork,
            child: Mutex::default(),
        });
        let mut services = self.host_services(emit);
        services.branch = Some(order.clone());
        let host = Arc::new(crate::agent::desk::RunHost {
            desk: crate::agent::desk::ExarchDesk { services },
            apply: crate::shell_eval::surface::SurfaceApplier::new(self.recorder()),
        });
        let report = crate::shell_eval::run_shell(
            self.seat.transport(),
            &self.agent.caps,
            "/branch",
            "_exarch-branch",
            BRANCH_TIMEOUT_SECS,
            host,
        )
        .map_err(lost)?;
        if let Some(why) = refusal(report) {
            return Err(why);
        }
        order
            .child
            .lock_ignore_poison()
            .take()
            .ok_or_else(|| "the engine forked, but the desk was never asked to take it up".into())
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
    pub(crate) fn for_test_with(cfg: TestTrunk) -> io::Result<Self> {
        let TestTrunk {
            system,
            allow_schedule,
            resume_on_reset,
            egress,
            disk_warn_bytes,
            lease,
            installers,
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
            caps: ral_core::capability::GrantStack::root(),
            run_dir,
            account: RecordedAccount::for_test("test"),
            trunk: Trunk::Headless,
            tools: Toolset::offered(false),
            allow_schedule,
            resume_on_reset,
            disk_warn_bytes,
            fuel: SPAWN_FUEL,
            egress,
            factory: None,
            bureau: Arc::new(crate::provider::Bureau::Scripted),
        };
        let seat = RootSeat::Identity {
            scratch,
            cwd: std::env::current_dir().expect("test process has a cwd"),
            terminal: ral_core::terminal::TerminalState::default(),
        };
        let provider = Arc::new(Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new(),
        ));
        Self::trunk(
            config,
            seat,
            provider,
            id,
            installers,
            lease,
            |sessions, model, account, bytes| {
                AgentLog::root(sessions, id, model, account, bytes).map(|log| (log, ()))
            },
        )
        .map(|(avatar, ())| avatar)
    }
}

/// What a unit test may vary about a [`Avatar::for_test`] trunk.
pub(crate) struct TestTrunk {
    pub(crate) system: String,
    pub(crate) allow_schedule: bool,
    pub(crate) resume_on_reset: bool,
    /// The network policy the trunk's `search` reach is derived from.
    pub(crate) egress: crate::egress::Egress,
    pub(crate) disk_warn_bytes: Option<u64>,
    /// The idle bound of the fleet this trunk is born into.
    pub(crate) lease: std::time::Duration,
    /// The recipes its engine boots from.
    pub(crate) installers: &'static [ral_core::engine::EngineInstaller],
}

impl TestTrunk {
    pub(crate) fn new(system: &str) -> Self {
        Self {
            system: system.to_string(),
            allow_schedule: false,
            resume_on_reset: false,
            egress: crate::egress::Egress::for_test(),
            disk_warn_bytes: None,
            lease: AGENT_LEASE_IDLE,
            installers: &crate::INSTALLERS,
        }
    }
}

impl Drop for Avatar {
    /// The one exit every life takes, and the whole of deregistration: the
    /// agent's last strong reference goes with this, so its parent's subtree
    /// and both fleet indices prune it at the next walk.  A cascade cancels
    /// only an agent's *eval root*, leaving its armed schedules for whoever
    /// drops it, so they are cleared here unconditionally and its workers die
    /// with the seat below: a settled-but-never-cancelled agent leaks neither.
    /// `/clear` never reaches this — it rebuilds in place, clearing its own.
    fn drop(&mut self) {
        self.agent.schedules.clear();
        // A panic may have poisoned the log under its guard, and a second
        // panic here would abort the process.
        if std::thread::panicking() {
            return;
        }
        let mut log = self.log.borrow_mut();
        if let Err(error) = log.record_session_ended() {
            log.record_emitter().report_fault(&error);
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests;
