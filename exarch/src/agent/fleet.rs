//! Fleet names, launch services, and the tenancy census; cancellation is a scope tree.

use crate::agent::engine::Engine;
use crate::agent::index::{BuiltinIndex, Grants};
use crate::agent::{AgentHandle, EngineLost, Factory, Role, Trunk};
use crate::app::Scratch;
use crate::bus::{Lease, Pumping, Refused};
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
    collections::{BTreeMap, HashMap},
    fmt, io, mem,
    ops::Deref,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
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
/// Detached from Fleet so a waiter retains no launch or factory.
#[derive(Default)]
pub(crate) struct Census {
    registry: Mutex<Registry>,
    changed: Condvar,
}
#[derive(Default)]
struct Registry {
    names: HashMap<Arc<str>, TenantId>,
    /// `None` until bound.
    tenants: BTreeMap<TenantId, Option<AgentHandle>>,
    failures: Vec<String>,
    next: u64,
}
impl Registry {
    fn handles(&self) -> impl Iterator<Item = &AgentHandle> {
        self.tenants.values().flatten()
    }
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct TenantId(u64);
impl Census {
    fn claim(self: &Arc<Self>, name: &str) -> Result<Claim, Unborn> {
        let mut registry = self.registry.lock_ignore_poison();
        if registry.names.contains_key(name) {
            return Err(Unborn::NameTaken(name.to_string()));
        }
        let id = TenantId(registry.next);
        registry.next += 1;
        let name: Arc<str> = name.into();
        registry.names.insert(name.clone(), id);
        registry.tenants.insert(id, None);
        drop(registry);
        Ok(Claim {
            census: self.clone(),
            id,
            name,
        })
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.registry.lock_ignore_poison().tenants.is_empty()
    }
    pub(crate) fn wait_empty_after(&self, waiting: impl FnOnce()) {
        let registry = self.registry.lock_ignore_poison();
        waiting();
        drop(
            self.changed
                .wait_while_ignore_poison(registry, |r| !r.tenants.is_empty()),
        );
    }
}
/// A name and a census entry; dropping it releases both silently.
#[must_use = "a claim only holds its name until it is dropped"]
pub(crate) struct Claim {
    census: Arc<Census>,
    id: TenantId,
    name: Arc<str>,
}
impl Claim {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
    pub(crate) fn id(&self) -> TenantId {
        self.id
    }
    pub(crate) fn bind(&self, handle: &AgentHandle) {
        self.census
            .registry
            .lock_ignore_poison()
            .tenants
            .insert(self.id, Some(handle.clone()));
    }
    pub(crate) fn occupy(self, engine: Engine) -> Tenancy {
        Tenancy {
            engine,
            claim: self,
        }
    }
    // Failures accumulate: a later clean teardown never absolves an earlier one.
    fn certify(&self, engine: &Engine) {
        if let Err(cause) = engine.end() {
            self.census.registry.lock_ignore_poison().failures.push(format!(
                "agent '{}' ended without certified engine teardown: {cause}; settlement is unsafe",
                self.name
            ));
        }
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        let mut registry = self.census.registry.lock_ignore_poison();
        registry.names.remove(&*self.name);
        let tenant = registry.tenants.remove(&self.id);
        drop(registry);
        drop(tenant);
        self.census.changed.notify_all();
    }
}
/// An engine and the claim it answers to; Drop ends the engine, then releases.
pub(crate) struct Tenancy {
    // Field order: the engine drops before the claim releases.
    engine: Engine,
    claim: Claim,
}
impl Tenancy {
    pub(crate) fn claim(&self) -> &Claim {
        &self.claim
    }
    /// Boot a fresh engine; on success certify and replace the old one.
    pub(crate) fn reboot(&mut self) -> Result<(), String> {
        let fresh = self.engine.reborn()?;
        self.claim.certify(&mem::replace(&mut self.engine, fresh));
        Ok(())
    }
}
impl Deref for Tenancy {
    type Target = Engine;
    fn deref(&self) -> &Engine {
        &self.engine
    }
}
impl Drop for Tenancy {
    fn drop(&mut self) {
        self.claim.certify(&self.engine);
    }
}
pub struct Fleet {
    census: Arc<Census>,
    pub(crate) root: DurableRoot,
    lease: Duration,
    pub(crate) launch: Launch,
}
impl Fleet {
    pub(crate) fn new(launch: Launch, lease: Duration) -> Arc<Self> {
        Arc::new(Self {
            census: Arc::default(),
            root: DurableRoot::default(),
            lease,
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
    pub(crate) fn claim(&self, name: &str) -> Result<Claim, Unborn> {
        check_name(name).map_err(Unborn::NameMalformed)?;
        self.census.claim(name)
    }
    pub fn resolve(&self, name: &str) -> Option<AgentHandle> {
        let registry = self.census.registry.lock_ignore_poison();
        let id = registry.names.get(name)?;
        registry
            .tenants
            .get(id)?
            .clone()
            .filter(AgentHandle::is_live)
    }
    pub(crate) fn live(&self) -> Vec<AgentHandle> {
        let mut live: Vec<_> = self
            .census
            .registry
            .lock_ignore_poison()
            .handles()
            .filter(|h| h.is_live())
            .cloned()
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
        // Cancellation hides live handles immediately; notify all owned inboxes.
        let handles: Vec<_> = self
            .census
            .registry
            .lock_ignore_poison()
            .handles()
            .cloned()
            .collect();
        for handle in handles {
            handle.0.mailbox.notify();
        }
        self.census.changed.notify_all();
    }
    pub fn is_empty(&self) -> bool {
        self.census.is_empty()
    }
    pub fn wait_empty(&self) {
        self.census.wait_empty_after(|| {});
    }
    pub(crate) fn census(&self) -> Arc<Census> {
        self.census.clone()
    }
    /// Wait until no tenant but `waiter` is unbound or ended, then report any
    /// uncertified teardown.
    pub(crate) fn wait_settled(&self, waiter: TenantId, _: &Pumping) -> Result<(), String> {
        let mut registry = self.census.registry.lock_ignore_poison();
        // Ancestor cancellation does not notify.
        while !registry
            .tenants
            .iter()
            .all(|(&id, h)| id == waiter || h.as_ref().is_some_and(AgentHandle::is_live))
        {
            (registry, _) = self
                .census
                .changed
                .wait_timeout_ignore_poison(registry, Duration::from_millis(100));
        }
        if registry.failures.is_empty() {
            Ok(())
        } else {
            Err(registry.failures.join("; "))
        }
    }
    pub fn nearest_reap(&self) -> Option<Duration> {
        self.live()
            .iter()
            .filter_map(AgentHandle::reply_since)
            .map(|since| self.lease.saturating_sub(since.elapsed()))
            .min()
    }
    pub(crate) fn lease_reply(&self, handle: &AgentHandle, lease: Lease) {
        if handle.0.role.parent().is_some() {
            let handle = Arc::downgrade(&handle.0);
            process::arm_callback(self.lease, move || {
                if let Some(shared) = handle.upgrade() {
                    AgentHandle(shared).lapse(&lease);
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
impl From<Refused> for Unborn {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::Abandoned => Self::Abandoned,
            Refused::Dead => Self::SessionDead,
        }
    }
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
