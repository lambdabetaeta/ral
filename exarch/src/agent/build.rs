//! Construction, in-place rebuilding, and owner teardown.

use crate::agent::EngineLost;
#[cfg(test)]
use crate::agent::SPAWN_FUEL;
use crate::agent::engine::Engine;
use crate::agent::factory::Factory;
#[cfg(test)]
use crate::agent::fleet::HELPER_IDLE;
use crate::agent::fleet::{Fleet, Launch, Name, Registry};
use crate::agent::handle::Shared;
use crate::agent::index::BuiltinIndex;
use crate::agent::loan::Loanable;
use crate::agent::nursery::Nursery;
use crate::agent::{Admission, Agent, AgentHandle, Body, Readings};
use crate::app::{SCRATCH_VAR, Scratch, guest_scratch_env};
use crate::bus::{AgentMailbox, Emitter};
use crate::provider::{Provider, Toolset};
use crate::record::Opening;
use crate::record::{AgentId, AgentLog, Inherited, Mail, Model, Resumed, Schedules, Selected};
use crate::shell_eval::builtins;
use ral_core::authority::policy::{Capabilities, GrantStack};
use ral_core::carrier::{Severed, WireTransport};
use ral_core::data::datum::Datum as _;
use ral_core::engine::EngineInstaller;
use ral_core::own::OwnDir;
use ral_core::path::{CwdPath, EnginePath, GuestPath};
use ral_core::process::ScopeNode;
use ral_core::protocol::channel::{Liveness, WireStream};
use ral_core::protocol::{Attach, EngineSpec, Origin, Root};
use std::{
    collections::BTreeMap,
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
fn with_teardown_failures(error: io::Error, failures: &[String]) -> io::Error {
    if failures.is_empty() {
        error
    } else {
        io::Error::other(format!("{error}\n{}", failures.join("\n")))
    }
}
pub(crate) const FIRST_NAME: &str = "main";
const RESUME_NOTE: &str = "session resumed from disk; the shell is fresh: bindings, workers, and \
    cwd from before are gone, the scratch dir is new ($EXARCH_SCRATCH is per-pid, so scratch paths \
    in the old context are dead), pinned state and scheduled events are gone (exarch-pins `list \
    and exarch-schedules `list to confirm), and any sub-agents from before have ended.";

/// Whom a new agent answers. Every other difference is in [`Parent::terms`].
pub(crate) enum Parent<'a> {
    Human,
    /// The parent and the life the child's is cut from.
    Agent(&'a AgentHandle, ScopeNode),
}

/// What a parent lends its child.
pub(crate) struct Terms {
    pub(crate) parent: Option<AgentHandle>,
    life: ScopeNode,
    pub(crate) fuel: u32,
    search: bool,
    admission: Admission,
    pub(crate) returns: bool,
}
impl Parent<'_> {
    fn terms(self, launch: &Launch, registry: &Registry) -> Terms {
        match self {
            Self::Human => Terms {
                parent: None,
                life: registry.root.child(),
                fuel: launch.fuel,
                search: launch.search,
                admission: launch.admission,
                returns: launch.returns,
            },
            Self::Agent(parent, life) => Terms {
                parent: Some(parent.clone()),
                life,
                fuel: parent.0.fuel.saturating_sub(1),
                search: parent.0.search,
                admission: Admission::Parent { idle: launch.idle },
                returns: true,
            },
        }
    }
}
#[allow(clippy::struct_excessive_bools, reason = "independent launch policies")]
pub struct LaunchConfig {
    pub system: String,
    pub run_dir: OwnDir,
    pub returns: bool,
    pub admission: Admission,
    pub helper_idle: Duration,
    pub tools: Toolset,
    pub max_tokens: Option<u32>,
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
        cfg: LaunchConfig,
        root_engine: RootEngine,
        installers: &'static [EngineInstaller],
    ) -> io::Result<(Arc<Self>, Engine)> {
        let LaunchConfig {
            system,
            run_dir,
            returns,
            admission,
            helper_idle,
            tools,
            max_tokens,
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
                "a wire root with spawn fuel needs an engine factory to make helper engines with: pass one via LaunchConfig::factory, or build this root with fuel: 0",
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
        let engine = match root_engine {
            RootEngine::Identity {
                scratch,
                cwd,
                grants,
                terminal,
            } => Engine::root(installers, cwd, grants, terminal, scratch).map_err(lost)?,
            RootEngine::Wire { transport, attach } => {
                Engine::wire(*transport, attach).map_err(lost)?
            }
        };
        let index = BuiltinIndex::resolve(engine.read(|t| t.builtin_names()).map_err(lost)?);
        let scratch = engine
            .read(|t| t.env_var(SCRATCH_VAR))
            .map_err(lost)?
            .ok_or_else(|| io::Error::other(format!("the engine names no ${SCRATCH_VAR}")))?;
        let fleet = Self::new(Launch {
            returns,
            admission,
            idle: helper_idle,
            allow_schedule,
            resume_on_reset,
            tools,
            max_tokens,
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
            engine: engine.rebirth(),
            #[cfg(test)]
            _scratch: None,
        });
        Ok((fleet, engine))
    }
}
impl Agent {
    #[allow(
        clippy::too_many_arguments,
        reason = "construction takes the owner resources and reconciled folds separately"
    )]
    pub(crate) fn assemble(
        name: Name,
        engine: Engine,
        log: AgentLog,
        folds: (Mail, Schedules, Model),
        terms: Terms,
        provider: Arc<Provider>,
        fleet: Arc<Fleet>,
    ) -> Self {
        let Terms {
            parent,
            life,
            fuel,
            search,
            admission,
            ..
        } = terms;
        log.record_emitter().bind(life.clone());
        engine.transport().bind(life.clone());
        let mailbox = AgentMailbox::new(
            log.id(),
            life,
            log.record_emitter(),
            admission,
            parent.as_ref().map(|parent| parent.0.address.clone()),
            folds,
        );
        engine.install(&mailbox.address(), 0);
        let handle = AgentHandle(Arc::new(Shared {
            id: log.id(),
            name: name.name().to_string(),
            log_dir: log.dir().clone(),
            run_dir: fleet.launch.run_dir().clone(),
            started: Instant::now(),
            parent,
            fuel,
            search,
            address: mailbox.address(),
        }));
        fleet.registry.bind(&name, &handle);
        let readings = Readings::new(!fleet.launch.tools.is_empty());
        Self {
            handle,
            mailbox,
            body: Loanable::new(Body {
                log,
                client: provider,
                nursery: Nursery::default(),
                reply: None,
                failures: Vec::new(),
                pins: BTreeMap::default(),
                schedules: crate::schedule::ScheduleRegistry::new(),
                readings,
            }),
            engine: Some(engine),
            generation: 0,
            fleet,
            _name: name,
        }
    }
    /// A new agent under `parent`, in a fresh log.
    pub(crate) fn born(
        fleet: &Arc<Fleet>,
        parent: Parent<'_>,
        name: Name,
        engine: Engine,
        provider: Arc<Provider>,
        inherited: Option<Inherited>,
    ) -> io::Result<Self> {
        let id = fresh_id();
        let terms = parent.terms(&fleet.launch, &fleet.registry);
        let parent = terms.parent.as_ref().map(AgentHandle::id);
        let returns = terms.returns;
        let log_name = name.name().to_string();
        Self::hatch(
            fleet,
            terms,
            name,
            engine,
            provider,
            |sessions, selected, system| {
                AgentLog::create(
                    sessions,
                    id,
                    Opening {
                        parent,
                        name: &log_name,
                        returns,
                        selected,
                        system,
                        inherited,
                    },
                )
                .map(|log| (log, ()))
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("could not open the session log of agent '{log_name}': {error}"),
                    )
                })
            },
        )
        .map(|(agent, ())| agent)
    }
    /// Reopen the human's children not closed with `/close`, with fresh engines.
    ///
    /// # Errors
    /// Refuses chat, wire engines, missing sessions, or failed construction.
    pub(crate) fn resume(
        cfg: LaunchConfig,
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
        let mut live = AgentLog::human_children(&sessions_root)?.into_iter();
        let Some(first) = live.next() else {
            return Err(io::Error::other(format!(
                "cannot resume {}: no session is open there; were they all closed with /close?",
                cfg.run_dir
            )));
        };
        let (fleet, first_engine) = Fleet::found(cfg, engine, &crate::INSTALLERS)?;
        let mut founding = Some(first_engine);
        let mut roots: Vec<(Self, Resumed)> = Vec::new();
        for (id, name) in std::iter::once(first).chain(live) {
            let reopened = (|| {
                let name = fleet
                    .registry
                    .name(&name)
                    .map_err(|why| io::Error::other(why.to_string()))?;
                let engine = founding.take().map_or_else(|| fleet.launch.engine(), Ok)?;
                Self::reopen(&fleet, name, engine, id, provider.clone())
            })();
            match reopened {
                Ok(root) => roots.push(root),
                Err(error) => {
                    let failures: Vec<_> = roots
                        .into_iter()
                        .flat_map(|(agent, _)| agent.discard())
                        .collect();
                    return Err(with_teardown_failures(error, &failures));
                }
            }
        }
        Ok(roots)
    }

    fn reopen(
        fleet: &Arc<Fleet>,
        name: Name,
        engine: Engine,
        id: AgentId,
        provider: Arc<Provider>,
    ) -> io::Result<(Self, Resumed)> {
        let terms = Parent::Human.terms(&fleet.launch, &fleet.registry);
        let returns = terms.returns;
        let (mut agent, resumed) = Self::hatch(
            fleet,
            terms,
            name,
            engine,
            provider,
            |sessions, selected, system| {
                let (log, mail, schedules, model) = AgentLog::reopen(
                    sessions,
                    id,
                    selected,
                    returns,
                    system,
                    crate::app::now_unix_ms(),
                )?;
                let resumed = log.resumed_summary();
                Ok(((log, mail, schedules, model), resumed))
            },
        )?;
        if let Err(error) = agent
            .body
            .log
            .import_note(genai::chat::ChatMessage::user(RESUME_NOTE))
        {
            return Err(with_teardown_failures(
                io::Error::other(error),
                &agent.discard(),
            ));
        }
        Ok((agent, resumed))
    }
    fn hatch<T>(
        fleet: &Arc<Fleet>,
        terms: Terms,
        name: Name,
        engine: Engine,
        provider: Arc<Provider>,
        open: impl FnOnce(
            &OwnDir,
            &Selected,
            &str,
        ) -> io::Result<((AgentLog, Mail, Schedules, Model), T)>,
    ) -> io::Result<(Self, T)> {
        let launch = &fleet.launch;
        let system = launch
            .prompt_for(&engine, &terms, name.name())
            .map_err(|lost| io::Error::other(EngineLost::starting(&lost, launch.run_dir())))?;
        let ((log, mail, schedules, model), opened) =
            open(&launch.sessions_root, provider.selected(), &system)?;
        let agent = Self::assemble(
            name,
            engine,
            log,
            (mail, schedules, model),
            terms,
            provider,
            fleet.clone(),
        );
        Ok((agent, opened))
    }
    pub(crate) fn clear(&mut self) -> Result<(), String> {
        self.body
            .log
            .prepare_clear()
            .map_err(|why| why.to_string())?;
        let fresh = self.engine().prepare_rebirth()?;
        let failures = self.body.nursery.dismiss_all();
        self.body.failures.extend(failures);
        self.body.nursery = Nursery::default();
        if let Err(failure) = self.body.log.clear() {
            self.body.retire_engine(&self.handle.0.name, &fresh);
            return Err(failure.to_string());
        }
        fresh.inherit(self.engine());
        let outgoing = self
            .engine
            .replace(fresh)
            .expect("clear replaces a live engine");
        self.body.retire_engine(&self.handle.0.name, &outgoing);
        self.generation += 1;
        self.engine()
            .install(&self.mailbox.address(), self.generation);
        self.body.pins.clear();
        #[allow(
            clippy::redundant_closure_for_method_calls,
            reason = "the method item is not general enough for try_transact's higher-ranked lifetime"
        )]
        let cleared = self.mailbox.try_transact(|txn| txn.clear());
        self.body.schedules.clear();
        self.body.readings = self.body.readings.reborn();
        cleared.map_err(|why| why.to_string())
    }
    pub(crate) fn rewind(&mut self, anchor: u64, emit: &Emitter) -> Result<(), String> {
        self.couple(emit);
        self.body
            .log
            .rewind(anchor)
            .map_err(|error| error.to_string())?;
        self.body.readings.nudges = self.body.readings.nudges.reborn();
        Ok(())
    }
    #[cfg(test)]
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
        let name = self
            .fleet
            .registry
            .name(name)
            .map_err(|error| error.to_string())?;
        let engine = self
            .fleet
            .launch
            .engine()
            .map_err(|error| error.to_string())?;
        Self::born(
            &self.fleet,
            Parent::Agent(&self.handle, self.mailbox.life().child()),
            name,
            engine,
            self.body.client.clone(),
            None,
        )
        .map_err(|error| error.to_string())
    }
    /// # Errors
    /// Refuses an engine or session log that cannot be opened.
    #[cfg(test)]
    pub fn born_for_test(
        cfg: LaunchConfig,
        engine: RootEngine,
        provider: Arc<Provider>,
    ) -> io::Result<Self> {
        let (fleet, engine) = Fleet::found(cfg, engine, &crate::INSTALLERS)?;
        let name = fleet
            .registry
            .name(FIRST_NAME)
            .map_err(|why| io::Error::other(why.to_string()))?;
        Self::born(&fleet, Parent::Human, name, engine, provider, None)
    }
    /// # Errors
    /// Refuses chat, wire engines, missing sessions, or failed construction.
    #[cfg(test)]
    pub fn resume_for_test(
        cfg: LaunchConfig,
        engine: RootEngine,
        provider: &Arc<Provider>,
    ) -> io::Result<Vec<(Self, Resumed)>> {
        Self::resume(cfg, engine, provider)
    }
    /// # Errors
    /// Refuses a scratch directory or session log that cannot be created.
    #[cfg(test)]
    pub fn for_test(system: &str) -> io::Result<Self> {
        Self::for_test_with(TestLaunch::new(system))
    }
    /// # Errors
    /// Refuses a scratch directory or session log that cannot be created.
    /// # Panics
    /// If the test process has no cwd.
    #[cfg(test)]
    pub(crate) fn for_test_with(cfg: TestLaunch) -> io::Result<Self> {
        let TestLaunch {
            system,
            allow_schedule,
            resume_on_reset,
            search,
            disk_warn_bytes,
            helper_idle,
            returns,
            admission,
            tools,
            fuel,
            installers,
            grants,
        } = cfg;
        let scratch = Arc::new(Scratch::for_test(crate::app::EXARCH, "agent")?);
        let run_dir = scratch.test_sibling("run")?;
        let config = LaunchConfig {
            system,
            run_dir,
            returns,
            admission,
            helper_idle,
            tools,
            max_tokens: None,
            allow_schedule,
            resume_on_reset,
            disk_warn_bytes,
            fuel,
            search,
            factory: None,
            bureau: Arc::new(crate::provider::Bureau::scripted([])),
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
        let (fleet, engine) = Fleet::found(config, engine, installers)?;
        let name = fleet
            .registry
            .name(FIRST_NAME)
            .map_err(|why| io::Error::other(why.to_string()))?;
        Self::born(&fleet, Parent::Human, name, engine, provider, None)
    }
}
#[cfg(test)]
#[allow(clippy::struct_excessive_bools, reason = "independent launch policies")]
pub(crate) struct TestLaunch {
    pub(crate) system: String,
    pub(crate) grants: GrantStack,
    pub(crate) allow_schedule: bool,
    pub(crate) resume_on_reset: bool,
    pub(crate) search: bool,
    pub(crate) disk_warn_bytes: Option<u64>,
    pub(crate) helper_idle: Duration,
    pub(crate) returns: bool,
    pub(crate) admission: Admission,
    pub(crate) tools: Toolset,
    pub(crate) fuel: u32,
    pub(crate) installers: &'static [EngineInstaller],
}
#[cfg(test)]
impl TestLaunch {
    pub(crate) fn new(system: &str) -> Self {
        Self {
            system: system.to_string(),
            grants: GrantStack::default(),
            allow_schedule: false,
            resume_on_reset: false,
            search: true,
            disk_warn_bytes: None,
            helper_idle: HELPER_IDLE,
            returns: true,
            admission: Admission::Free,
            tools: Toolset::offered(false),
            fuel: SPAWN_FUEL,
            installers: &crate::INSTALLERS,
        }
    }
}
impl Drop for Agent {
    fn drop(&mut self) {
        self.body.schedules.clear();
        // An unwind leaves the fields to end the life and join helpers (A3).
        if !std::thread::panicking() {
            let _unclaimed = self.teardown();
            self.recorder()
                .emit_or_end(crate::record::Forensic::SessionEnded);
        }
    }
}
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
