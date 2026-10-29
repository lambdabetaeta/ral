//! One owner, one mailbox, one cancellation tree.

mod arrival;
mod attend;
mod build;
mod command;
mod deliberate;
pub(crate) mod desk;
pub mod digest;
pub(crate) mod engine;
mod engine_lost;
mod factory;
pub mod fleet;
pub(crate) mod gauge;
mod handle;
pub(crate) mod index;
pub(crate) mod loan;
pub mod nudge;
pub(crate) mod nursery;
pub mod outcome;
pub mod resources;
pub mod roster;
mod shell;
pub(crate) mod spawn;
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
pub(crate) mod testkit;
pub(crate) mod tools;

pub use crate::bus::Admission;
#[cfg(test)]
pub(crate) use build::TestTrunk;
pub(crate) use build::fresh_id;
pub use build::{RootConfig, RootEngine, Trunk};
pub use engine_lost::{EngineLost, EnginePhase};
pub use factory::Factory;
pub use handle::{AgentHandle, Ended};
pub(crate) use shell::Evaluated;

use crate::agent::engine::Engine;
use crate::agent::fleet::{Fleet, Name};
use crate::agent::loan::Loanable;
use crate::agent::nursery::Nursery;
use crate::bus::AgentMailbox;
use crate::provider::Provider;
use crate::record::{AgentId, AgentLog, Unavailable};
use crate::schedule::ScheduleRegistry;
use crate::shell_eval::PinDigest;
use ral_core::data::FOValue;
use ral_core::own::OwnDir;
use ral_core::sync::LockExt;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// An agent, owned by the one thread that attends it.
pub struct Agent {
    handle: AgentHandle,
    // Drop order: end life, join helpers, drop engine, release name.
    pub(crate) mailbox: AgentMailbox,
    pub(crate) body: Loanable<Body>,
    engine: Option<Engine>,
    generation: u64,
    pub(crate) fleet: Arc<Fleet>,
    _name: Name,
}

pub(crate) struct Body {
    pub(crate) log: AgentLog,
    pub(crate) state: State,
}

pub(crate) type Pins = BTreeMap<String, PinDigest>;
pub(crate) struct State {
    pub(crate) nursery: Nursery,
    pub(crate) reply: Option<FOValue>,
    /// A5: a reply is owed to the parent, or to a headless root's human.
    pub(crate) owed: bool,
    pub(crate) failures: Vec<String>,
    pub(crate) pins: Pins,
    pub(crate) schedules: ScheduleRegistry,
    pub(crate) readings: Readings,
}

pub(crate) enum Role {
    Converses,
    Returns,
    Child { parent: AgentHandle },
}
impl Role {
    pub(crate) fn returns(&self) -> bool {
        !matches!(self, Self::Converses)
    }
    pub(crate) fn parent(&self) -> Option<&AgentHandle> {
        match self {
            Self::Child { parent } => Some(parent),
            Self::Converses | Self::Returns => None,
        }
    }
}
pub(crate) struct Readings {
    measure: Option<gauge::Measure>,
    gauges: gauge::Gauges,
    nudges: nudge::Nudges,
}
impl Readings {
    fn new(steers: bool) -> Self {
        Self {
            measure: None,
            gauges: gauge::Gauges::default(),
            nudges: nudge::Nudges::new(steers),
        }
    }
    fn reborn(&self) -> Self {
        Self::new(self.nudges.steers())
    }
}
pub const SPAWN_FUEL: u32 = 3;

#[derive(Clone)]
pub struct ProviderHandle(Arc<Mutex<Arc<Provider>>>);
impl ProviderHandle {
    pub fn new(provider: Arc<Provider>) -> Self {
        Self(Arc::new(Mutex::new(provider)))
    }
    pub fn current(&self) -> Arc<Provider> {
        self.0.lock_ignore_poison().clone()
    }
    pub fn swap(&self, provider: Arc<Provider>) {
        *self.0.lock_ignore_poison() = provider;
    }
}
impl Agent {
    pub fn handle(&self) -> AgentHandle {
        self.handle.clone()
    }
    pub fn id(&self) -> AgentId {
        self.handle.id()
    }
    pub fn log_dir(&self) -> &OwnDir {
        &self.handle.0.log_dir
    }
    pub fn run_dir(&self) -> &OwnDir {
        self.handle.run_dir()
    }
    pub fn severance(&self) -> Option<ral_core::carrier::Severed> {
        self.engine.as_ref().and_then(Engine::severed)
    }
    pub(crate) fn engine(&self) -> &Engine {
        self.engine
            .as_ref()
            .expect("an attending agent owns its engine")
    }
    pub(crate) fn resumes_at_reset(&self) -> bool {
        self.handle.0.role.parent().is_none() && self.fleet.launch.resume_on_reset
    }
    pub fn is_ready(&self) -> bool {
        self.check_available().is_ok() && self.body.log.context().is_ready()
    }
    /// # Errors
    /// Recording failed: this session has ended; resuming the run reconstructs what was recorded.
    pub fn check_available(&self) -> Result<(), Unavailable> {
        self.body.log.record_emitter().check_available()
    }
    pub fn rendered_messages(&self) -> Vec<genai::chat::ChatMessage> {
        self.body.log.context().rendered()
    }
}
