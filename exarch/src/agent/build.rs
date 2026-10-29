//! Construction, in-place rebuilding, and owner teardown.

use crate::agent::EngineLost;
#[cfg(any(test, feature = "test-util"))]
use crate::agent::SPAWN_FUEL;
use crate::agent::engine::{Engine, Incarnation};
use crate::agent::factory::Factory;
use crate::agent::fleet::{AGENT_LEASE_IDLE, Fleet, Genesis, Launch, Tenancy, Unborn};
use crate::agent::handle::Shared;
use crate::agent::index::BuiltinIndex;
use crate::agent::{Agent, AgentHandle, LogCell, ProviderHandle, Readings, ReplyCell, Role};
use crate::app::{SCRATCH_VAR, Scratch, guest_scratch_env};
use crate::bus::{Emitter, Inbox, Mailbox};
use crate::provider::{Provider, Toolset};
use crate::record::{AgentId, AgentLog, Memory, RecordedAccount, RecordedModel, Resumed};
use crate::shell_eval::builtins;
use ral_core::authority::policy::{Capabilities, GrantStack};
use ral_core::carrier::{Severed, WireTransport};
use ral_core::data::datum::Datum as _;
use ral_core::engine::EngineInstaller;
use ral_core::own::OwnDir;
use ral_core::path::{CwdPath, EnginePath, GuestPath};
use ral_core::process::{Scope, ScopeNode};
use ral_core::protocol::channel::{Liveness, WireStream};
use ral_core::protocol::{Attach, EngineSpec, Origin, Root};
use ral_core::sync::LockExt;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

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
const TRUNK_NAME: &str = "main";
const RESUME_NOTE: &str = "session resumed from disk; the shell is fresh: bindings, workers, and \
    cwd from before are gone, the scratch dir is new ($EXARCH_SCRATCH is per-pid, so scratch paths \
    in the old context are dead), pinned state and scheduled events are gone (exarch-pins `list \
    and exarch-schedules `list to confirm), and any sub-agents from before have ended.";

pub(crate) struct Build {
    pub(crate) tenancy: Tenancy,
    pub(crate) log: AgentLog,
    pub(crate) role: Role,
    pub(crate) fuel: u32,
    pub(crate) provider: ProviderHandle,
    pub(crate) search: bool,
    pub(crate) fleet: Arc<Fleet>,
    pub(crate) parent: Option<Mailbox>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trunk {
    Headless,
    Attended,
    /// A host opens jobs on it: synod.
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
pub struct RootConfig {
    pub system: String,
    pub run_dir: OwnDir,
    pub trunk: Trunk,
    pub tools: Toolset,
    pub allow_schedule: bool,
    pub resume_on_reset: bool,
    pub disk_warn_bytes: Option<u64>,
    pub fuel: u32,
    pub search: bool,
    pub factory: Option<Arc<dyn Factory>>,
    pub bureau: Arc<crate::provider::Bureau>,
    pub roles: crate::provider::roles::Roles,
}
/// Where a root's ral engine runs.
pub enum RootEngine {
    Identity {
        scratch: Arc<Scratch>,
        cwd: CwdPath,
        grants: GrantStack,
        terminal: ral_core::io::terminal::TerminalState,
    },
    Wire {
        transport: Box<WireTransport>,
        attach: Attach,
    },
}
impl RootEngine {
    /// A guest engine's control stream, its scratch named by `SCRATCH_VAR`.
    ///
    /// # Errors
    /// Refuses a control stream that cannot be adopted.
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
    pub(crate) fn found(
        cfg: RootConfig,
        root_engine: RootEngine,
        installers: &'static [EngineInstaller],
        lease: Duration,
    ) -> io::Result<(Arc<Self>, Engine)> {
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
        if matches!(&root_engine, RootEngine::Wire { .. }) && fuel > 0 && factory.is_none() {
            return Err(io::Error::other(
                "a wire root with spawn fuel needs an engine factory to make helper engines with: pass one via RootConfig::factory, or build this root with fuel: 0",
            ));
        }
        if let RootEngine::Wire { attach, .. } = &root_engine
            && let Origin::Root(root) = &attach.origin
            && !root.grants.is_root()
        {
            return Err(io::Error::other(
                "a wire root's attach carries resolved grants, which name objects on this host, but its engine may run on another: pass the ceiling unresolved in Root::config, for the engine's recipe to resolve where it runs",
            ));
        }
        let sessions_root = run_dir.make_dir("sessions")?;
        let lost = |s: Severed| io::Error::other(EngineLost::starting(&s, &run_dir));
        let (engine, genesis) = match root_engine {
            RootEngine::Identity {
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
                (genesis.engine().map_err(lost)?, Some(genesis))
            }
            RootEngine::Wire { transport, attach } => {
                (Engine::wire(*transport, attach).map_err(lost)?, None)
            }
        };
        let index = BuiltinIndex::resolve(engine.read(|t| t.builtin_names()).map_err(lost)?);
        let scratch = engine
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
        Ok((fleet, engine))
    }
    /// I1: a life hangs under its parent's work or the fleet root.
    pub(crate) fn lodge(
        &self,
        parent: Option<Mailbox>,
        name: &str,
        born: Instant,
    ) -> Result<(Scope, ScopeNode, Inbox), Unborn> {
        let (under, life, owing) = match parent {
            Some(mailbox) => {
                let (life, owing) = mailbox.adopt(name.into(), born)?;
                (mailbox.scope(), life, Some(owing))
            }
            None => ((*self.root).clone(), self.root.child(), None),
        };
        Ok((under, life.clone(), Inbox::new(life, owing)))
    }
}
impl Agent {
    pub(crate) fn assemble(b: Build) -> Result<Self, Unborn> {
        let Build {
            tenancy,
            log,
            role,
            fuel,
            provider,
            search,
            fleet,
            parent,
        } = b;
        let id = log.id();
        let started = Instant::now();
        let (under, life, inbox) = fleet.lodge(parent, tenancy.claim().name(), started)?;
        // A failed writer or a lost engine ends this life.
        log.record_emitter().bind(life.clone());
        tenancy.transport().bind(life);
        let handle = AgentHandle(Arc::new(Shared {
            id,
            name: tenancy.claim().name().to_string(),
            log_dir: log.dir().clone(),
            run_dir: fleet.launch.run_dir().clone(),
            started,
            role,
            fuel,
            provider,
            search,
            under,
            mailbox: inbox.life_mailbox(),
        }));
        tenancy.claim().bind(&handle);
        let readings = Readings::new(!fleet.launch.tools.is_empty());
        let engine = Incarnation::new(tenancy, inbox.life());
        Ok(Self {
            handle,
            inbox,
            log: LogCell::new(log),
            reply: ReplyCell::default(),
            pins: Arc::default(),
            schedules: crate::schedule::ScheduleRegistry::new(),
            fleet,
            readings,
            engine,
        })
    }
    /// # Errors
    /// Refuses an engine or session log that cannot be opened.
    pub fn root(cfg: RootConfig, engine: RootEngine, provider: Arc<Provider>) -> io::Result<Self> {
        Self::first(
            fresh_id(),
            cfg,
            engine,
            provider,
            &crate::INSTALLERS,
            AGENT_LEASE_IDLE,
        )
    }
    /// Reopen every root not closed with `/close`, with fresh engines.
    ///
    /// # Errors
    /// Refuses chat, wire engines, missing sessions, or failed construction.
    pub fn resume(
        cfg: RootConfig,
        engine: RootEngine,
        provider: &Arc<Provider>,
    ) -> io::Result<Vec<(Self, Resumed)>> {
        if cfg.tools.is_empty() {
            return Err(io::Error::other(
                "cannot resume a chat session: chat keeps no resumable harness history",
            ));
        }
        if matches!(engine, RootEngine::Wire { .. }) {
            return Err(io::Error::other(
                "--resume is unavailable for a wire engine: the engine process is gone; resume an identity session instead",
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
        let (fleet, first_engine) =
            Fleet::found(cfg, engine, &crate::INSTALLERS, AGENT_LEASE_IDLE)?;
        let mut founding = Some(first_engine);
        std::iter::once(first)
            .chain(live)
            .map(|(id, name)| {
                let claim = fleet
                    .claim(&name)
                    .map_err(|why| io::Error::other(why.to_string()))?;
                let engine = founding.take().map_or_else(|| fleet.launch.engine(), Ok)?;
                Self::reopen(&fleet, claim.occupy(engine), id, provider.clone())
            })
            .collect()
    }
    fn reopen(
        fleet: &Arc<Fleet>,
        tenancy: Tenancy,
        id: AgentId,
        provider: Arc<Provider>,
    ) -> io::Result<(Self, Resumed)> {
        let live = provider.clone();
        let (agent, resumed) =
            Self::hatch(fleet, tenancy, provider, |sessions, _, account, system| {
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
            })?;
        agent
            .log
            .borrow_mut()
            .import_note(genai::chat::ChatMessage::user(RESUME_NOTE))
            .map_err(io::Error::other)?;
        Ok((agent, resumed))
    }
    pub(crate) fn first(
        id: AgentId,
        cfg: RootConfig,
        engine: RootEngine,
        provider: Arc<Provider>,
        installers: &'static [EngineInstaller],
        lease: Duration,
    ) -> io::Result<Self> {
        let (fleet, engine) = Fleet::found(cfg, engine, installers, lease)?;
        let tenancy = fleet
            .claim(TRUNK_NAME)
            .map_err(|why| io::Error::other(why.to_string()))?
            .occupy(engine);
        Self::hatch(
            &fleet,
            tenancy,
            provider,
            |sessions, model, account, system| {
                AgentLog::root(sessions, id, TRUNK_NAME, model, account, system, None)
                    .map(|log| (log, ()))
            },
        )
        .map(|(agent, ())| agent)
    }
    pub(crate) fn hatch<T>(
        fleet: &Arc<Fleet>,
        tenancy: Tenancy,
        provider: Arc<Provider>,
        open: impl FnOnce(&OwnDir, &RecordedModel, &RecordedAccount, &str) -> io::Result<(AgentLog, T)>,
    ) -> io::Result<(Self, T)> {
        let launch = &fleet.launch;
        let role = launch.trunk.role();
        let system = launch
            .prompt_for(&tenancy, &role, launch.fuel, tenancy.claim().name())
            .map_err(|lost| io::Error::other(EngineLost::starting(&lost, launch.run_dir())))?;
        let account = RecordedAccount::of(provider.account(), &launch.bureau.available());
        let (log, opened) = open(
            &launch.sessions_root,
            &RecordedModel::of(&provider),
            &account,
            &system,
        )?;
        let agent = Self::assemble(Build {
            tenancy,
            log,
            role,
            fuel: launch.fuel,
            provider: ProviderHandle::new(provider),
            search: launch.search,
            fleet: fleet.clone(),
            parent: None,
        })
        .map_err(|why| io::Error::other(why.to_string()))?;
        Ok((agent, opened))
    }
    pub(crate) fn child_of(
        (parent, mailbox): (&AgentHandle, Mailbox),
        parent_log: &AgentLog,
        fleet: &Arc<Fleet>,
        tenancy: Tenancy,
        provider: Arc<Provider>,
        memory: Memory,
    ) -> Result<Self, String> {
        let launch = &fleet.launch;
        let fuel = parent.0.fuel.saturating_sub(1);
        let role = Role::Child {
            parent: parent.clone(),
        };
        let system = launch
            .prompt_for(&tenancy, &role, fuel, tenancy.claim().name())
            .map_err(|lost| EngineLost::starting(&lost, parent.run_dir()).to_string())?;
        let account = RecordedAccount::of(provider.account(), &launch.bureau.available());
        let log = parent_log
            .fork(
                fresh_id(),
                tenancy.claim().name(),
                &RecordedModel::of(&provider),
                &account,
                &system,
                memory,
            )
            .map_err(|e| format!("could not fork child session log: {e}"))?;
        Self::assemble(Build {
            tenancy,
            log,
            role,
            fuel,
            provider: ProviderHandle::new(provider),
            search: parent.0.search,
            fleet: fleet.clone(),
            parent: Some(mailbox),
        })
        .map_err(|why| why.to_string())
    }
    pub(crate) fn clear(&mut self) -> Result<(), String> {
        self.log
            .borrow()
            .prepare_clear()
            .map_err(|why| why.to_string())?;
        let boundary = self.inbox.clear_boundary();
        let fresh = self.engine.prepare_rebirth()?;
        // Bound, so the log's borrow ends before the engine is touched.
        let committed = self.log.borrow_mut().clear();
        if let Err(failure) = committed {
            self.engine.discard(fresh);
            return Err(failure.to_string());
        }

        self.inbox.apply_clear(boundary);
        self.schedules.clear();
        self.pins.lock_ignore_poison().clear();
        self.readings = self.readings.reborn();
        self.engine.install_rebirth(fresh);
        Ok(())
    }
    pub(crate) fn rewind(&mut self, anchor: u64, emit: &Emitter) -> Result<(), String> {
        self.couple(emit);
        self.log
            .borrow_mut()
            .rewind(anchor)
            .map_err(|e| e.to_string())?;
        self.inbox.close_exchange();
        self.readings.nudges = self.readings.nudges.reborn();
        Ok(())
    }
    pub(crate) fn seed(&self, prompt: String) {
        let _ = self.handle.steer(prompt);
    }
    #[cfg(test)]
    pub(crate) fn fork(&self) -> Result<Self, String> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        self.fork_named(&format!("fork-{}", SEQ.fetch_add(1, Ordering::Relaxed)))
    }
    #[cfg(test)]
    pub(crate) fn fork_named(&self, name: &str) -> Result<Self, String> {
        let claim = self.fleet.claim(name).map_err(|e| e.to_string())?;
        let tenancy = claim.occupy(self.fleet.launch.engine().map_err(|e| e.to_string())?);
        Self::child_of(
            (&self.handle, self.inbox.mailbox()),
            &self.log.borrow(),
            &self.fleet,
            tenancy,
            self.handle.current_provider(),
            Memory::Amnemon,
        )
    }
    /// # Errors
    /// Refuses a scratch directory or session log that cannot be created.
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(system: &str) -> io::Result<Self> {
        Self::for_test_with(TestTrunk::new(system))
    }
    /// # Errors
    /// Refuses a scratch directory or session log that cannot be created.
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
        let engine = RootEngine::Identity {
            scratch,
            cwd: ral_core::host::cwd().expect("test process has a cwd"),
            grants,
            terminal: ral_core::io::terminal::TerminalState::default(),
        };
        let provider = Arc::new(Provider::scripted(
            "test-model",
            crate::provider::scripted::Script::new(),
        ));
        Self::first(id, config, engine, provider, installers, lease)
    }
}
#[cfg(any(test, feature = "test-util"))]
pub(crate) struct TestTrunk {
    pub(crate) system: String,
    pub(crate) grants: GrantStack,
    pub(crate) allow_schedule: bool,
    pub(crate) resume_on_reset: bool,
    pub(crate) search: bool,
    pub(crate) disk_warn_bytes: Option<u64>,
    pub(crate) lease: Duration,
    pub(crate) installers: &'static [EngineInstaller],
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
impl Drop for Agent {
    fn drop(&mut self) {
        // I8: end the life before any owner field tears down.
        self.handle.end();
        self.schedules.clear();
        if !std::thread::panicking() {
            self.recorder()
                .emit_or_end(crate::record::Forensic::SessionEnded);
        }
    }
}
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
