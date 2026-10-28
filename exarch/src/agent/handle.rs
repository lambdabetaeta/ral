//! Immutable identity and the one door into an agent.

#[cfg(test)]
mod tests;

use super::{ProviderHandle, Role};
use crate::bus::{AgentMessage, JobId, Lease, Mailbox, Read, Rewrite, Scoped, Status};
use crate::record::AgentId;
use ral_core::data::FOValue;
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
    pub(crate) under: Scope,
    pub(crate) mailbox: Mailbox,
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
        self.0.mailbox.steer(text).then_some(()).ok_or(Ended)
    }
    pub fn queued(&self) -> Vec<String> {
        self.0.mailbox.queued()
    }
    pub fn recall(&self) -> Vec<String> {
        self.0.mailbox.recall()
    }
    /// Stop only the matching running job and recall its queued prompts.
    pub fn stop(&self, id: JobId) -> Option<Vec<String>> {
        self.0.mailbox.stop(id)
    }
    /// End this life and every descendant; teardown remains the owner's work.
    pub fn end(&self) {
        self.0.mailbox.end(CancelCause::Cancelled);
    }
    pub(crate) fn interrupt(&self) {
        self.0.mailbox.interrupt();
    }
    pub(crate) fn post_read(&self, read: Read) {
        self.0.mailbox.post_read(read);
    }
    pub(crate) fn post_rewrite(&self, rewrite: Rewrite) {
        self.0.mailbox.post_rewrite(rewrite);
    }
    pub(crate) fn message(&self, from: &Self, text: String) -> Result<(), Ended> {
        // Owe before delivery; never hold both inboxes.
        let owed = self
            .0
            .role
            .parent()
            .filter(|parent| parent.id() == from.id())
            .and_then(|_| self.0.mailbox.report())
            .and_then(|report| report.owe());
        self.0
            .mailbox
            .vouched_by(from.0.under.clone())
            .post(Scoped::Message {
                message: AgentMessage {
                    from: from.id(),
                    from_name: from.0.name.clone(),
                    text,
                },
                owed,
            })
            .then_some(())
            .ok_or(Ended)
    }
    pub(crate) fn lapse(&self, lease: &Lease) {
        self.0.mailbox.lapse(lease);
    }
    pub(crate) fn status(&self) -> Status {
        self.0.mailbox.status()
    }
    pub(crate) fn reply(&self) -> Option<FOValue> {
        self.0.mailbox.reply()
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
        self.0.mailbox.is_live()
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
    pub(crate) fn has_reply(&self) -> bool {
        self.status().replied
    }
    pub(crate) fn reply_since(&self) -> Option<Instant> {
        self.status().reply_since
    }
    pub(crate) fn has_busy_children(&self) -> bool {
        self.status().busy
    }
    pub(crate) fn waiting_for_input(&self) -> bool {
        self.status().waiting_for_input
    }
    pub(crate) fn idle(&self) -> Duration {
        self.status()
            .last_exchange
            .unwrap_or(self.0.started)
            .elapsed()
    }
}
