//! Immutable identity and the one door into an agent.

#[cfg(test)]
mod tests;

use super::{ProviderHandle, Role};
use crate::bus::{AgentAddress, AgentMessage, Command, Input, Refused, Status};
use crate::record::{AgentId, ChildStatus, JobId};
use ral_core::own::OwnDir;
use ral_core::process::{CancelCause, Scope};
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
    pub(crate) role: Role,
    pub(crate) fuel: u32,
    pub(crate) search: bool,
    pub(crate) provider: ProviderHandle,
    pub(crate) address: AgentAddress,
}
/// Steering was refused because the agent's life has ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ended;
impl fmt::Display for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the agent has ended")
    }
}
impl std::error::Error for Ended {}
impl AgentHandle {
    pub fn id(&self) -> AgentId {
        self.0.id
    }
    /// # Errors
    /// Refuses steering once the agent has ended.
    pub fn steer(&self, text: String) -> Result<(), Ended> {
        self.0.address.steer(text).map_err(|_| Ended)
    }
    pub fn queued(&self) -> Vec<String> {
        self.0.address.queued()
    }
    pub fn recall(&self) -> Vec<String> {
        self.0.address.recall()
    }
    /// Stop `job` if it is open, taking back its queued human prompts.
    pub fn stop(&self, job: JobId) -> Option<Vec<String>> {
        self.0.address.stop(job)
    }
    /// End this life and every descendant; teardown remains the owner's work.
    pub fn end(&self) {
        self.0.address.end(CancelCause::Cancelled);
    }
    /// Admit one job on a gated root.
    pub fn release(&self) -> bool {
        self.0.address.release()
    }
    /// Stop the open job, leaving queued prompts in place.
    pub(crate) fn stop_current(&self) -> bool {
        self.0.address.stop_current()
    }
    pub(crate) fn post_command(&self, command: Command) {
        let _ = self.0.address.command(command);
    }
    pub(crate) fn message(&self, from: &Self, text: String) -> Result<(), Refused> {
        let message = AgentMessage {
            from: from.id(),
            from_name: from.0.name.clone(),
            text,
            from_parent: self.parent().is_some_and(|parent| parent.id() == from.id()),
        };
        self.0.address.post(Input::Message(message))
    }
    pub(crate) fn report(&self, status: ChildStatus) {
        let parent = self.parent().map(|parent| {
            (
                &parent.0.address,
                Input::Child {
                    id: self.id(),
                    name: Arc::from(self.0.name.as_str()),
                    status: status.clone(),
                    elapsed: self.elapsed(),
                },
            )
        });
        self.0.address.report(status, parent);
    }
    pub(crate) fn status(&self) -> Status {
        self.0.address.status()
    }
    pub(crate) fn parent(&self) -> Option<&Self> {
        self.0.role.parent()
    }
    pub(crate) fn is_root(&self) -> bool {
        self.parent().is_none()
    }
    pub(crate) fn log_dir(&self) -> &OwnDir {
        &self.0.log_dir
    }
    pub(crate) fn descendant(&self, target: &Self) -> Option<Self> {
        let mut above = target.0.role.parent();
        while let Some(node) = above {
            if node.id() == self.id() {
                return Some(target.clone());
            }
            above = node.0.role.parent();
        }
        None
    }
    pub(crate) fn root(&self) -> Self {
        let mut here = self.clone();
        while let Some(parent) = here.0.role.parent().cloned() {
            here = parent;
        }
        here
    }
    pub fn is_live(&self) -> bool {
        self.0.address.is_live()
    }
    pub(crate) fn life(&self) -> Scope {
        self.0.address.life()
    }
    pub(crate) fn current_provider(&self) -> Arc<crate::provider::Provider> {
        self.0.provider.current()
    }
    pub(crate) fn run_dir(&self) -> &OwnDir {
        &self.0.run_dir
    }
    pub(crate) fn elapsed(&self) -> Duration {
        self.0.started.elapsed()
    }
}
