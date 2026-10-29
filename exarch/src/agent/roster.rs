//! Fresh projections of fleet identity and mailbox status.

use crate::agent::{AgentHandle, fleet::Fleet};
use crate::enquiry::{AgentInfo, RosterState, Spawner, Summary};
use crate::record::ChildStatus;
use std::time::Duration;

impl AgentInfo {
    fn of(agent: &AgentHandle) -> Self {
        let status = agent.status();
        let state = if status.working() {
            if status.busy {
                RosterState::WaitingOnAgents
            } else {
                RosterState::Busy
            }
        } else if matches!(status.report, Some(ChildStatus::Replied(_))) {
            RosterState::Replied
        } else {
            RosterState::Waiting
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
        .registry
        .live()
        .iter()
        .filter(|node| node.root().id() == root)
        .map(AgentInfo::of)
        .collect()
}
pub(crate) fn summary(reader: &AgentHandle, fleet: &Fleet) -> Summary {
    let live = fleet.registry.live();
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
                let status = node.status();
                node.parent()
                    .is_some_and(|parent| parent.id() == reader.id())
                    && !status.working()
                    && matches!(status.report, Some(ChildStatus::Replied(_)))
            })
            .count(),
    }
}
