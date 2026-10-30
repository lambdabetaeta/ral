//! Fresh projections of fleet identity and mailbox status.

use crate::agent::{AgentHandle, fleet::Fleet};
use crate::enquiry::{AgentInfo, RosterState, Spawner, Summary};
use crate::record::ChildStatus;
use std::time::Duration;

impl AgentInfo {
    fn of(agent: &AgentHandle) -> Option<Self> {
        let status = agent.status().ok()?;
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
        Some(Self {
            name: agent.0.name.clone(),
            spawner: agent
                .parent()
                .map_or(Spawner::Human, |up| Spawner::Agent(up.0.name.clone())),
            log_dir: agent.0.log_dir.path().to_path_buf(),
            elapsed: agent.elapsed(),
            state,
            idle: status.rest.map_or(Duration::ZERO, |at| at.elapsed()),
        })
    }
}
pub(crate) fn listing(reader: &AgentHandle, fleet: &Fleet) -> Vec<AgentInfo> {
    let founder = reader.founder().id();
    fleet
        .registry
        .live()
        .iter()
        .filter(|node| node.founder().id() == founder)
        .filter_map(AgentInfo::of)
        .collect()
}
pub(crate) fn summary(reader: &AgentHandle, fleet: &Fleet) -> Summary {
    let founder = reader.founder().id();
    let counted: Vec<_> = fleet
        .registry
        .live()
        .into_iter()
        .filter(|node| node.founder().id() == founder)
        .filter_map(|node| {
            let status = node.status().ok()?;
            Some((node, status))
        })
        .collect();
    let reader_counted = counted.iter().any(|(node, _)| node.id() == reader.id());
    Summary {
        live: counted.len() - usize::from(reader_counted),
        replied: counted
            .iter()
            .filter(|(node, status)| {
                node.parent()
                    .is_some_and(|parent| parent.id() == reader.id())
                    && !status.working()
                    && matches!(status.report, Some(ChildStatus::Replied(_)))
            })
            .count(),
    }
}
