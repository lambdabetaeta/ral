//! Fleet names, launch services, and teardown membership; cancellation is a scope tree.

use crate::agent::engine::Engine;
use crate::agent::index::{BuiltinIndex, Grants};
use crate::agent::{AgentHandle, EngineLost, Factory, Role, Trunk};
use crate::app::Scratch;
use crate::enquiry::check_name;
use crate::provider::{Bureau, Toolset, roles::Roles};
use ral_core::authority::policy::GrantStack;
use ral_core::carrier::Severed;
use ral_core::engine::EngineInstaller;
use ral_core::io::terminal::TerminalState;
use ral_core::own::OwnDir;
#[cfg(test)]
use ral_core::own::TestDir;
use ral_core::path::CwdPath;
use ral_core::process::{self, CancelCause, DurableRoot};
use ral_core::sync::{CondvarExt, LockExt};
use std::{
    collections::HashMap,
    fmt, io,
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};

pub const AGENT_LEASE_IDLE: Duration = Duration::from_hours(1);
pub(crate) struct Launch {
    pub trunk: Trunk,
    pub allow_schedule: bool,
    pub resume_on_reset: bool,
    pub tools: Toolset,
    pub disk_warn_bytes: Option<u64>,
    pub factory: Option<Arc<dyn Factory>>,
    pub bureau: Arc<Bureau>,
    pub roles: Roles,
    pub system: Arc<str>,
    pub index: Arc<BuiltinIndex>,
    pub fuel: u32,
    pub search: bool,
    pub run_dir: OwnDir,
    pub sessions_root: OwnDir,
    pub genesis: Option<Genesis>,
    #[cfg(test)]
    pub(crate) _scratch: Option<TestDir>,
}
pub(crate) struct Genesis {
    pub(crate) installers: &'static [EngineInstaller],
    pub(crate) cwd: CwdPath,
    pub(crate) grants: GrantStack,
    pub(crate) terminal: TerminalState,
    pub(crate) scratch: Arc<Scratch>,
}
impl Genesis {
    pub(crate) fn engine(&self) -> Result<Engine, Severed> {
        Engine::root(
            self.installers,
            self.cwd.clone(),
            self.grants.clone(),
            self.terminal,
            self.scratch.clone(),
        )
    }
}
impl Launch {
    pub(crate) fn run_dir(&self) -> &OwnDir {
        &self.run_dir
    }
    pub(crate) fn engine(&self) -> io::Result<Engine> {
        let genesis = self.genesis.as_ref().ok_or_else(|| {
            io::Error::other(
                "this host boots no further engines, so it cannot open another session",
            )
        })?;
        genesis
            .engine()
            .map_err(|lost| io::Error::other(EngineLost::starting(&lost, self.run_dir())))
    }
    pub(crate) fn prompt_for(
        &self,
        engine: &Engine,
        role: &Role,
        fuel: u32,
        name: &str,
    ) -> Result<String, Severed> {
        let own = crate::prompt::with_grant(&self.system, &engine.read(|t| t.grants())?);
        Ok(if self.tools.is_empty() {
            own
        } else {
            self.index.apply(
                &own,
                &Grants {
                    role,
                    allow_schedule: self.allow_schedule,
                    spawns: fuel > 0,
                    roles: &self.roles,
                },
                name,
            )
        })
    }
}
#[cfg(test)]
impl Launch {
    pub(crate) fn for_test() -> Self {
        let scratch = OwnDir::for_test("launch").expect("a test run directory");
        Self {
            trunk: Trunk::Headless,
            allow_schedule: false,
            resume_on_reset: false,
            tools: Toolset::offered(false),
            disk_warn_bytes: None,
            factory: None,
            bureau: Arc::new(Bureau::Scripted),
            roles: Roles::default(),
            system: Arc::from(""),
            index: BuiltinIndex::resolve(
                ral_core::test_helper::core_shell()
                    .builtin_names()
                    .map(str::to_string)
                    .collect(),
            ),
            fuel: 0,
            search: false,
            run_dir: OwnDir::clone(&scratch),
            sessions_root: scratch.make_dir("sessions").expect("a test sessions root"),
            genesis: None,
            _scratch: Some(scratch),
        }
    }
}
enum Slot {
    Claimed,
    Live(AgentHandle),
}
#[must_use = "a claim only holds its name until it is dropped"]
pub(crate) struct Claim(Name);
impl Claim {
    pub(crate) fn name(&self) -> &str {
        &self.0.text
    }
    pub(crate) fn bind(self, handle: &AgentHandle) -> Name {
        self.0
            .fleet
            .names
            .lock_ignore_poison()
            .insert(self.0.text.clone(), Slot::Live(handle.clone()));
        self.0
    }
}
pub(crate) struct Name {
    fleet: Arc<Fleet>,
    text: String,
    _membership: Membership,
}
impl Drop for Name {
    fn drop(&mut self) {
        self.fleet.names.lock_ignore_poison().remove(&self.text);
    }
}
pub(crate) struct Membership(Arc<Departures>);
#[derive(Default)]
pub(crate) struct Departures {
    count: Mutex<usize>,
    changed: Condvar,
}
impl Departures {
    pub(crate) fn wait_after(&self, waiting: impl FnOnce()) {
        let count = self.count.lock_ignore_poison();
        waiting();
        drop(self.changed.wait_while_ignore_poison(count, |n| *n != 0));
    }
}
impl Drop for Membership {
    fn drop(&mut self) {
        let mut count = self.0.count.lock_ignore_poison();
        *count -= 1;
        drop(count);
        self.0.changed.notify_all();
    }
}
pub struct Fleet {
    names: Mutex<HashMap<String, Slot>>,
    pub(crate) root: DurableRoot,
    departures: Arc<Departures>,
    lease: Duration,
    teardown_failure: OnceLock<String>,
    pub(crate) launch: Launch,
}
impl Fleet {
    pub(crate) fn new(launch: Launch, lease: Duration) -> Arc<Self> {
        Arc::new(Self {
            names: Mutex::new(HashMap::new()),
            root: DurableRoot::default(),
            departures: Arc::default(),
            lease,
            teardown_failure: OnceLock::new(),
            launch,
        })
    }
    #[cfg(test)]
    pub(crate) fn for_test() -> Arc<Self> {
        Self::new(Launch::for_test(), AGENT_LEASE_IDLE)
    }
    #[cfg(test)]
    pub(crate) fn leased_for_test(lease: Duration) -> Arc<Self> {
        Self::new(Launch::for_test(), lease)
    }
    pub(crate) fn claim(self: &Arc<Self>, name: &str) -> Result<Claim, Unborn> {
        check_name(name).map_err(Unborn::NameMalformed)?;
        // I8: count construction before reserving or publishing its name.
        let membership = self.member();
        let mut names = self.names.lock_ignore_poison();
        if names.contains_key(name) {
            return Err(Unborn::NameTaken(name.to_string()));
        }
        names.insert(name.to_string(), Slot::Claimed);
        drop(names);
        Ok(Claim(Name {
            fleet: self.clone(),
            text: name.to_string(),
            _membership: membership,
        }))
    }
    fn member(&self) -> Membership {
        *self.departures.count.lock_ignore_poison() += 1;
        Membership(self.departures.clone())
    }
    pub fn resolve(&self, name: &str) -> Option<AgentHandle> {
        match self.names.lock_ignore_poison().get(name) {
            Some(Slot::Live(handle)) if handle.is_live() => Some(handle.clone()),
            _ => None,
        }
    }
    pub(crate) fn live(&self) -> Vec<AgentHandle> {
        let mut live: Vec<_> = self
            .names
            .lock_ignore_poison()
            .values()
            .filter_map(|slot| match slot {
                Slot::Live(handle) if handle.is_live() => Some(handle.clone()),
                _ => None,
            })
            .collect();
        live.sort_by_key(AgentHandle::id);
        live
    }
    pub(crate) fn roots(&self) -> Vec<AgentHandle> {
        self.live()
            .into_iter()
            .filter(|h| h.0.role.parent().is_none())
            .collect()
    }
    pub fn end(&self, cause: CancelCause) {
        self.root.cancel(cause);
        // Cancellation hides live slots immediately; notify all owned inboxes.
        let handles: Vec<_> = self
            .names
            .lock_ignore_poison()
            .values()
            .filter_map(|slot| match slot {
                Slot::Live(handle) => Some(handle.clone()),
                Slot::Claimed => None,
            })
            .collect();
        for handle in handles {
            handle.0.mailbox.notify();
        }
        self.departures.changed.notify_all();
    }
    pub fn is_empty(&self) -> bool {
        *self.departures.count.lock_ignore_poison() == 0
    }
    pub fn wait_empty(&self) {
        self.wait_empty_after(|| {});
    }
    pub(super) fn wait_empty_after(&self, waiting: impl FnOnce()) {
        self.departures.wait_after(waiting);
    }
    pub(crate) fn departures(&self) -> Arc<Departures> {
        self.departures.clone()
    }
    pub(crate) fn teardown_failed(&self, why: String) {
        let _ = self.teardown_failure.set(why);
    }
    pub(crate) fn wait_settled(&self, waiter: &AgentHandle) -> Result<(), String> {
        let mut count = self.departures.count.lock_ignore_poison();
        let retained = || {
            1 + self.names.lock_ignore_poison().values().filter(|slot| {
                matches!(slot, Slot::Live(handle) if handle.id() != waiter.id() && handle.is_live())
            }).count()
        };
        // Ancestor cancellation need not notify departures; teardown always does.
        while *count != retained() {
            (count, _) = self
                .departures
                .changed
                .wait_timeout_ignore_poison(count, Duration::from_millis(100));
        }
        drop(count);
        self.teardown_failure.get().cloned().map_or(Ok(()), Err)
    }
    pub fn nearest_reap(&self) -> Option<Duration> {
        self.live()
            .iter()
            .filter_map(AgentHandle::reply_since)
            .map(|since| self.lease.saturating_sub(since.elapsed()))
            .min()
    }
    pub(crate) fn lease_reply(&self, handle: &AgentHandle, since: Instant) {
        if handle.0.role.parent().is_some() {
            let handle = Arc::downgrade(&handle.0);
            process::arm_callback(self.lease, move || {
                if let Some(shared) = handle.upgrade() {
                    AgentHandle(shared).lapse(since);
                }
            })
            .keep();
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unborn {
    SessionDead,
    Abandoned,
    NameTaken(String),
    NameMalformed(String),
}
impl fmt::Display for Unborn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionDead => f.write_str("this session is no longer live"),
            Self::Abandoned => f.write_str("the work this call belonged to was stopped or cleared, so nothing was started; issue it again on your next turn"),
            Self::NameTaken(name) => write!(f, "a live agent already bears the name '{name}': pick another, or wait for it to settle. Names identify live agents; agents lists yours."),
            Self::NameMalformed(why) => f.write_str(why),
        }
    }
}
#[cfg(test)]
mod tests;
