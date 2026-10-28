//! One owner, one inbox, one cancellation tree.

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
pub mod nudge;
pub mod outcome;
pub mod resources;
pub mod roster;
mod shell;
pub(crate) mod spawn;
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
pub(crate) mod testkit;
pub(crate) mod tools;

#[cfg(test)]
pub(crate) use build::TestTrunk;
pub(crate) use build::fresh_id;
pub use build::{RootConfig, RootEngine, Trunk};
pub use engine_lost::{EngineLost, EnginePhase};
pub use factory::Factory;
pub use handle::{AgentHandle, Ended};
pub(crate) use shell::{Evaluated, LogCell, ReplyCell, SharedCell};

use crate::agent::engine::Incarnation;
use crate::agent::fleet::{Fleet, TenantId};
use crate::bus::{Inbox, Queue};
use crate::provider::Provider;
use crate::record::{AgentId, Unavailable};
use crate::schedule::ScheduleRegistry;
use crate::shell_eval::PinDigests;
use ral_core::own::OwnDir;
use ral_core::sync::LockExt;
use std::sync::{Arc, Mutex};

/// An agent, owned by the one thread that attends it.
pub struct Agent {
    handle: AgentHandle,
    inbox: Inbox,
    log: LogCell,
    reply: ReplyCell,
    pins: PinDigests,
    schedules: ScheduleRegistry,
    pub(crate) fleet: Arc<Fleet>,
    readings: Readings,
    // Last: the tenancy ends the engine, then releases, after SessionEnded and the inbox.
    pub(crate) engine: Incarnation,
}

pub(crate) enum Role {
    Converses,
    Headless,
    Child { parent: AgentHandle },
}
impl Role {
    pub(crate) fn returns(&self) -> bool {
        !matches!(self, Self::Converses)
    }
    pub(crate) fn parent(&self) -> Option<&AgentHandle> {
        match self {
            Self::Child { parent } => Some(parent),
            Self::Converses | Self::Headless => None,
        }
    }
}
struct Readings {
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
    pub(crate) fn tenant(&self) -> TenantId {
        self.engine.tenant()
    }
    pub(crate) fn await_job(&self) -> Option<crate::bus::Job> {
        self.inbox.await_job()
    }
    pub fn log_dir(&self) -> &OwnDir {
        &self.handle.0.log_dir
    }
    pub fn run_dir(&self) -> &OwnDir {
        self.handle.run_dir()
    }
    pub fn severance(&self) -> Option<ral_core::carrier::Severed> {
        self.engine.severed()
    }
    pub(crate) fn abandon(&self, queue: Queue) -> Vec<String> {
        let recalled = self.inbox.abandon(queue);
        self.schedules.clear();
        recalled
    }
    pub(crate) fn resumes_at_reset(&self) -> bool {
        self.handle.0.role.parent().is_none() && self.fleet.launch.resume_on_reset
    }
    /// # Panics
    /// Panics on reentrant log access.
    pub fn is_ready(&self) -> bool {
        self.check_available().is_ok() && self.log.borrow().context().is_ready()
    }
    /// # Errors
    /// Recording failed: this session has ended; resuming the run reconstructs what was recorded.
    ///
    /// # Panics
    /// Panics on reentrant log access.
    pub fn check_available(&self) -> Result<(), Unavailable> {
        self.log.borrow().record_emitter().check_available()
    }
    /// # Panics
    /// Panics on reentrant log access.
    pub fn rendered_messages(&self) -> Vec<genai::chat::ChatMessage> {
        self.log.borrow().context().rendered()
    }
}
