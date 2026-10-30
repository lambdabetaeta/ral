//! Immutable identity and the one door into an agent.

#[cfg(test)]
mod tests;

use crate::bus::{AgentAddress, AgentMessage, Command, Input, Refused, Status};
use crate::record::{AgentId, ChildStatus, JobId, Unavailable};
use ral_core::own::OwnDir;
use ral_core::process::CancelCause;
use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

/// Identity and the one door into an agent; retaining it never retains the owner.
#[derive(Clone)]
pub struct AgentHandle(pub(crate) Arc<Shared>);
pub(crate) struct Shared {
    pub(crate) id: AgentId,
    pub(crate) name: String,
    pub(crate) log_dir: OwnDir,
    pub(crate) run_dir: OwnDir,
    pub(crate) started: Instant,
    pub(crate) parent: Option<AgentHandle>,
    pub(crate) fuel: u32,
    pub(crate) search: bool,
    pub(crate) address: AgentAddress,
}
/// Steering was refused because the agent's life has ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ended(pub AgentId);
impl fmt::Display for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "agent {} has ended", self.0)
    }
}
impl std::error::Error for Ended {}
impl AgentHandle {
    pub(crate) fn id(&self) -> AgentId {
        self.0.id
    }
    /// # Errors
    /// Refuses steering once the agent has ended.
    pub(crate) fn steer(&self, text: String) -> Result<(), Ended> {
        self.0.address.steer(text).map_err(|_| Ended(self.id()))
    }
    pub(crate) fn queued(&self) -> Vec<String> {
        self.0.address.queued()
    }
    pub(crate) fn recall(&self) -> Vec<String> {
        self.0.address.recall()
    }
    /// Stop `job` if it is open, taking back its queued human prompts.
    pub(crate) fn stop(&self, job: JobId) -> Option<Vec<String>> {
        self.0.address.stop(job)
    }
    /// End this life and every descendant; teardown remains the owner's work.
    pub(crate) fn end(&self) {
        self.0.address.end(CancelCause::Cancelled);
    }
    /// Admit one job on a gated root.
    pub(crate) fn release(&self) -> bool {
        self.0.address.release()
    }
    /// Stop the open job, leaving queued prompts in place.
    pub(crate) fn stop_current(&self) -> bool {
        self.0.address.stop_current()
    }
    pub(crate) fn command(&self, command: Command) -> Result<(), Ended> {
        self.0
            .address
            .command(command)
            .map_err(|_| Ended(self.id()))
    }
    pub(crate) fn message(&self, from: &Self, text: String) -> Result<(), Refused> {
        let message = AgentMessage {
            from: from.id(),
            from_name: from.0.name.clone(),
            text,
        };
        self.0.address.post(Input::Message(message))
    }
    pub(crate) fn report(&self, status: ChildStatus) -> Result<(), Unavailable> {
        let notice = |status| Input::Child {
            id: self.id(),
            name: Arc::from(self.0.name.as_str()),
            status,
            elapsed: self.elapsed(),
        };
        self.0.address.report(status, notice)
    }
    pub(crate) fn status(&self) -> Result<Status, Unavailable> {
        self.0.address.status()
    }
    pub(crate) fn parent(&self) -> Option<&Self> {
        self.0.parent.as_ref()
    }
    #[cfg(test)]
    pub(crate) fn log_dir(&self) -> &OwnDir {
        &self.0.log_dir
    }
    pub(crate) fn descendant(&self, target: &Self) -> Option<Self> {
        let mut above = target.parent();
        while let Some(node) = above {
            if node.id() == self.id() {
                return Some(target.clone());
            }
            above = node.parent();
        }
        None
    }
    pub(crate) fn founder(&self) -> Self {
        let mut here = self.clone();
        while let Some(parent) = here.parent().cloned() {
            here = parent;
        }
        here
    }
    pub(crate) fn is_live(&self) -> bool {
        self.0.address.is_live()
    }
    pub(crate) fn run_dir(&self) -> &OwnDir {
        &self.0.run_dir
    }
    pub(crate) fn elapsed(&self) -> Duration {
        self.0.started.elapsed()
    }
}
