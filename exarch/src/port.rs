//! A frontend's whole reach into a host's agents.

pub use crate::agent::Ended;
pub use crate::bus::{BusReceiver, Command, Read, Rewrite, Signal, Sink};
#[cfg(any(test, feature = "test-util"))]
pub use crate::bus::{BusSender, channel};
pub(crate) use crate::bus::{MERGE_TEXT_CAP, WORKER_PANIC_PREFIX};

use crate::agent::AgentHandle;
use crate::agent::fleet::Registry;
use crate::bus::UsageMeter;
use crate::provider::{Selected, Usage};
use crate::record::{AgentId, JobId};
use std::sync::Arc;
use std::time::Duration;

/// Agents by id, through what their mailboxes publish; retaining one never
/// retains the launch (P4).
#[derive(Clone)]
pub struct Port {
    registry: Arc<Registry>,
    meter: UsageMeter,
}

/// One live agent, as its mailbox publishes it.
#[derive(Clone, Debug)]
pub struct AgentRow {
    pub id: AgentId,
    pub waiting_for_input: bool,
    /// Since its last exchange, or its birth.
    pub idle: Duration,
    pub selected: Selected,
}

impl Port {
    pub(crate) fn new(registry: Arc<Registry>, meter: UsageMeter) -> Self {
        Self { registry, meter }
    }

    /// The live agents, in id order.
    pub fn roster(&self) -> Vec<AgentRow> {
        self.registry.live().iter().filter_map(row_of).collect()
    }

    /// `agent`'s row while it lives.
    pub fn row(&self, agent: AgentId) -> Option<AgentRow> {
        self.registry.get(agent).as_ref().and_then(row_of)
    }

    /// Whether `agent` still lives, without reading its status.
    pub fn is_live(&self, agent: AgentId) -> bool {
        self.registry.get(agent).is_some()
    }

    /// # Errors
    /// Returns `Ended` once the agent has ended.
    pub fn submit(&self, agent: AgentId, text: String) -> Result<(), Ended> {
        self.live(agent)?.steer(text)
    }

    /// # Errors
    /// Returns `Ended` once the agent has ended.
    pub fn queued(&self, agent: AgentId) -> Result<Vec<String>, Ended> {
        Ok(self.live(agent)?.queued())
    }

    /// # Errors
    /// Returns `Ended` once the agent has ended.
    pub fn recall(&self, agent: AgentId) -> Result<Vec<String>, Ended> {
        Ok(self.live(agent)?.recall())
    }

    /// Stop `job` if it is open, taking back its queued human prompts.
    ///
    /// # Errors
    /// Returns `Ended` once the agent has ended.
    pub fn stop(&self, agent: AgentId, job: JobId) -> Result<Option<Vec<String>>, Ended> {
        Ok(self.live(agent)?.stop(job))
    }

    /// # Errors
    /// Returns `Ended` once the agent has ended.
    pub fn command(&self, agent: AgentId, command: Command) -> Result<(), Ended> {
        self.live(agent)?.command(command)
    }

    /// End `agent` and its descendants; their owners still join them.
    ///
    /// # Errors
    /// Returns `Ended` once the agent has ended.
    pub fn end(&self, agent: AgentId) -> Result<(), Ended> {
        self.live(agent)?.end();
        Ok(())
    }

    /// This run's spend, metered at the seam.
    pub fn usage(&self) -> Usage {
        self.meter.total()
    }

    fn live(&self, agent: AgentId) -> Result<AgentHandle, Ended> {
        self.registry.get(agent).ok_or(Ended(agent))
    }
}

fn row_of(handle: &AgentHandle) -> Option<AgentRow> {
    let status = handle.status().ok()?;
    Some(AgentRow {
        id: handle.id(),
        waiting_for_input: status.waiting_for_input,
        idle: status
            .last_exchange
            .map_or_else(|| handle.elapsed(), |at| at.elapsed()),
        selected: status.selected,
    })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
