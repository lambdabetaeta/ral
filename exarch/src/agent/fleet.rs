//! Fleet names, launch services, and the human's nursery.

use crate::agent::build::Terms;
use crate::agent::engine::{Engine, Rebirth};
use crate::agent::index::{BuiltinIndex, Grants};
use crate::agent::nursery::{Joined, Nursery, Teardown};
use crate::agent::{Admission, AgentHandle, EngineLost, Factory};
use crate::enquiry::check_name;
use crate::provider::{Bureau, Toolset, roles::Roles};
use crate::record::AgentId;
use ral_core::carrier::Severed;
use ral_core::own::OwnDir;
#[cfg(test)]
use ral_core::own::TestDir;
use ral_core::process::{CancelCause, DurableRoot};
use ral_core::sync::LockExt;
use std::{
    collections::{BTreeMap, HashMap},
    fmt, io, mem,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

/// How long a helper rests before ending, unless `--helper-idle` says otherwise.
pub const HELPER_IDLE: Duration = Duration::from_hours(5);
#[allow(clippy::struct_excessive_bools, reason = "independent launch policies")]
pub(crate) struct Launch {
    pub returns: bool,
    pub admission: Admission,
    pub idle: Duration,
    pub allow_schedule: bool,
    pub resume_on_reset: bool,
    pub tools: Toolset,
    pub max_tokens: Option<u32>,
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
    pub engine: Option<Rebirth>,
    #[cfg(test)]
    pub(crate) _scratch: Option<TestDir>,
}
impl Launch {
    pub(crate) fn run_dir(&self) -> &OwnDir {
        &self.run_dir
    }
    pub(crate) fn engine(&self) -> io::Result<Engine> {
        let recipe = self.engine.as_ref().ok_or_else(|| {
            io::Error::other(
                "this host boots no further engines, so it cannot open another session",
            )
        })?;
        recipe
            .boot()
            .map_err(|lost| io::Error::other(EngineLost::starting(&lost, self.run_dir())))
    }
    pub(crate) fn prompt_for(
        &self,
        engine: &Engine,
        terms: &Terms,
        name: &str,
    ) -> Result<String, Severed> {
        let own = crate::prompt::with_grant(&self.system, &engine.read(|t| t.grants())?);
        Ok(if self.tools.is_empty() {
            own
        } else {
            self.index.apply(
                &own,
                &Grants {
                    spawned: terms.parent.is_some(),
                    returns: terms.returns,
                    allow_schedule: self.allow_schedule,
                    spawns: terms.fuel > 0,
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
            returns: true,
            admission: Admission::Free,
            idle: HELPER_IDLE,
            allow_schedule: false,
            resume_on_reset: false,
            tools: Toolset::offered(false),
            max_tokens: None,
            disk_warn_bytes: None,
            factory: None,
            bureau: Arc::new(Bureau::scripted([])),
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
            engine: None,
            _scratch: Some(scratch),
        }
    }
}
/// The fleet's root scope and its named agents: all a port may reach (P4).
pub(crate) struct Registry {
    pub(crate) root: DurableRoot,
    directory: Mutex<Directory>,
}

/// Reserved names, each bound to its agent's id once born; agents by id.
#[derive(Default)]
struct Directory {
    names: HashMap<Arc<str>, Option<AgentId>>,
    agents: BTreeMap<AgentId, AgentHandle>,
}

pub(crate) struct Fleet {
    pub(crate) registry: Arc<Registry>,
    pub(crate) launch: Launch,
    pub(crate) human: Mutex<Nursery>,
}

/// A reserved directory name, removed when its owner drops.
#[must_use = "a name is reserved only until it is dropped"]
pub(crate) struct Name {
    registry: Weak<Registry>,
    name: Arc<str>,
}
impl Name {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}
impl Drop for Name {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            let mut directory = registry.directory.lock_ignore_poison();
            if let Some(Some(id)) = directory.names.remove(&self.name) {
                directory.agents.remove(&id);
            }
        }
    }
}
impl Fleet {
    pub(crate) fn new(launch: Launch) -> Arc<Self> {
        Arc::new(Self {
            registry: Arc::new(Registry {
                root: DurableRoot::default(),
                directory: Mutex::default(),
            }),
            launch,
            human: Mutex::default(),
        })
    }
    #[cfg(test)]
    pub(crate) fn for_test() -> Arc<Self> {
        Self::new(Launch::for_test())
    }
    pub(crate) fn adopt(&self, handle: AgentHandle, thread: Joined<Teardown>) {
        self.human.lock_ignore_poison().adopt(handle, thread);
    }
    pub(crate) fn join_children(&self) -> Vec<String> {
        let mut failures = Vec::new();
        loop {
            let mut children = mem::take(&mut *self.human.lock_ignore_poison());
            if children.children().next().is_none() {
                return failures;
            }
            failures.extend(children.dismiss_all());
        }
    }
}
impl Registry {
    pub(crate) fn unnamed(self: &Arc<Self>) -> Name {
        let mut directory = self.directory.lock_ignore_poison();
        let name: Arc<str> = (1..=directory.names.len() + 1)
            .map(|n| format!("agent-{n}"))
            .find(|name| !directory.names.contains_key(name.as_str()))
            .expect("a finite directory leaves an unused name")
            .into();
        directory.names.insert(name.clone(), None);
        drop(directory);
        Name {
            registry: Arc::downgrade(self),
            name,
        }
    }
    pub(crate) fn name(self: &Arc<Self>, name: &str) -> Result<Name, Unborn> {
        check_name(name).map_err(Unborn::NameMalformed)?;
        let mut directory = self.directory.lock_ignore_poison();
        if directory.names.contains_key(name) {
            return Err(Unborn::NameTaken(name.to_string()));
        }
        let name: Arc<str> = name.into();
        directory.names.insert(name.clone(), None);
        drop(directory);
        Ok(Name {
            registry: Arc::downgrade(self),
            name,
        })
    }
    pub(crate) fn bind(&self, name: &Name, handle: &AgentHandle) {
        let mut directory = self.directory.lock_ignore_poison();
        let reserved = directory
            .names
            .get_mut(&name.name)
            .expect("a name is reserved before binding");
        assert!(reserved.is_none(), "a reserved name is bound once");
        *reserved = Some(handle.id());
        directory.agents.insert(handle.id(), handle.clone());
        drop(directory);
    }
    pub(crate) fn resolve(&self, name: &str) -> Option<AgentHandle> {
        let directory = self.directory.lock_ignore_poison();
        let id = (*directory.names.get(name)?)?;
        directory.agents.get(&id).filter(|h| h.is_live()).cloned()
    }
    pub(crate) fn get(&self, id: AgentId) -> Option<AgentHandle> {
        self.directory
            .lock_ignore_poison()
            .agents
            .get(&id)
            .filter(|h| h.is_live())
            .cloned()
    }
    /// The live agents, in id order.
    pub(crate) fn live(&self) -> Vec<AgentHandle> {
        let mut handles = self.handles();
        handles.retain(AgentHandle::is_live);
        handles
    }
    fn handles(&self) -> Vec<AgentHandle> {
        self.directory
            .lock_ignore_poison()
            .agents
            .values()
            .cloned()
            .collect()
    }
    pub(crate) fn human_children(&self) -> Vec<AgentHandle> {
        self.live()
            .into_iter()
            .filter(|handle| handle.parent().is_none())
            .collect()
    }
    pub(crate) fn end(&self, cause: CancelCause) {
        self.root.cancel(cause);
        for handle in self.handles() {
            handle.0.address.end(cause);
        }
    }
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.directory.lock_ignore_poison().names.is_empty()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unborn {
    NameTaken(String),
    NameMalformed(String),
}
impl fmt::Display for Unborn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NameTaken(name) => write!(
                f,
                "an agent already bears the name '{name}': pick another, or wait for it to end. Names stay reserved through teardown; agents lists yours."
            ),
            Self::NameMalformed(why) => f.write_str(why),
        }
    }
}
#[cfg(test)]
mod tests;
