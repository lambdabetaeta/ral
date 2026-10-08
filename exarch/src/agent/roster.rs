//! What the `exarch-agents` family answers.
//!
//! [`listing`] is one row per live agent in the reader's own tree; [`summary`]
//! is the two integers every other transition answers instead.  Both derive
//! fresh at read time — nothing here stores state of its own.

use crate::agent::Agent;
use crate::enquiry::{AgentInfo, RosterState, Spawner, Summary};
use std::sync::Arc;
use std::time::Duration;

impl AgentInfo {
    fn of(agent: &Agent) -> Self {
        let rest = agent.rest();
        let state = match (rest, agent.has_reply()) {
            (None, _) if agent.has_busy_children() => RosterState::WaitingOnAgents,
            (None, _) => RosterState::Busy,
            (Some(_), true) => RosterState::Replied,
            (Some(_), false) => RosterState::Waiting,
        };
        Self {
            name: agent.name.clone(),
            spawner: agent
                .parent
                .as_ref()
                .map_or(Spawner::Root, |up| Spawner::Agent(up.name.clone())),
            log_dir: agent.log_dir.clone(),
            elapsed: agent.elapsed(),
            state,
            idle: rest.map_or(Duration::ZERO, |at| at.elapsed()),
        }
    }
}

/// The `exarch-agents` listing: every live agent in `reader`'s own tree, ordered by
/// id and `reader` among them.  An agent sees its whole tree, not only what it
/// spawned, because what it may *message* is wider than what it spawned and a
/// name it cannot see is a name it cannot be told to write to.  The climb stops
/// at a root, so one `/branch` tab never lists another's.
///
/// `spawner` carries the scope `` `cancel `` and `` `read `` still enforce: the
/// listing states the rule it does not impose.
pub(crate) fn listing(reader: &Arc<Agent>) -> Vec<AgentInfo> {
    let root = reader.root();
    let mut live = root.walk();
    live.push(root);
    live.sort_unstable_by_key(|node| node.id);
    live.iter().map(|node| AgentInfo::of(node)).collect()
}

/// [`Summary`] for one reader.  The two counts answer different questions — who
/// is out there, and what is owed to me — so they are scoped differently on
/// purpose: `live` over the tree, `replied` over this agent's own children.
pub(crate) fn summary(reader: &Arc<Agent>) -> Summary {
    Summary {
        live: listing(reader).len().saturating_sub(1),
        replied: reader.children().iter().filter(|a| a.has_reply()).count(),
    }
}
