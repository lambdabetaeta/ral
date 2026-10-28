//! Fresh projections of fleet identity and inbox status.

use crate::agent::{AgentHandle, fleet::Fleet};
use crate::enquiry::{AgentInfo, RosterState, Spawner, Summary};
use std::time::Duration;

impl AgentInfo {
    fn of(agent: &AgentHandle) -> Self {
        let status = agent.status();
        let state = match (status.rest, status.replied) {
            (None, _) if status.busy => RosterState::WaitingOnAgents,
            (None, _) => RosterState::Busy,
            (Some(_), true) => RosterState::Replied,
            (Some(_), false) => RosterState::Waiting,
        };
        Self {
            name: agent.0.name.clone(),
            spawner: agent
                .0
                .role
                .parent()
                .map_or(Spawner::Root, |up| Spawner::Agent(up.0.name.clone())),
            log_dir: agent.0.log_dir.path().to_path_buf(),
            elapsed: agent.elapsed(),
            state,
            idle: status.rest.map_or(Duration::ZERO, |at| at.elapsed()),
        }
    }
}
pub(crate) fn listing(reader: &AgentHandle, fleet: &Fleet) -> Vec<AgentInfo> {
    let root = reader.root().id();
    fleet
        .live()
        .iter()
        .filter(|node| node.root().id() == root)
        .map(AgentInfo::of)
        .collect()
}
pub(crate) fn summary(reader: &AgentHandle, fleet: &Fleet) -> Summary {
    let live = fleet.live();
    let root = reader.root().id();
    Summary {
        live: live
            .iter()
            .filter(|node| node.root().id() == root)
            .count()
            .saturating_sub(1),
        replied: live
            .iter()
            .filter(|node| {
                node.0
                    .role
                    .parent()
                    .is_some_and(|parent| parent.id() == reader.id())
                    && node.has_reply()
            })
            .count(),
    }
}
