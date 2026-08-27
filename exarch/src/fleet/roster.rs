//! The `agents` listing: one row per live descendant, derived fresh from the
//! tree at read time — nothing here stores state of its own.

use crate::agent::Agent;
use std::path::PathBuf;
use std::time::Duration;

/// What a listed agent is doing, as a roster row states it.  Derived at
/// [`listing`] time from an agent's rest and deposit and from its own children;
/// nothing stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RosterState {
    /// Working: not parked for a message.
    Busy,
    /// Working only in the sense of holding for a busy child of its own.
    WaitingOnAgents,
    /// Parked holding a reply the parent has yet to fetch.
    Replied,
    /// Parked with no reply — a child a human is talking to.
    Waiting,
}

impl RosterState {
    /// The variant label the roster row carries.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::WaitingOnAgents => "waiting-on-agents",
            Self::Replied => "replied",
            Self::Waiting => "waiting",
        }
    }
}

/// One live agent, for the `agents` listing.
pub struct AgentInfo {
    pub name: String,
    pub log_dir: PathBuf,
    pub elapsed: Duration,
    pub state: RosterState,
    /// How long it has been parked; zero while busy.
    pub idle: Duration,
}

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
            name: agent.name().to_string(),
            log_dir: agent.log_dir().to_path_buf(),
            elapsed: agent.elapsed(),
            state,
            idle: rest.map_or(Duration::ZERO, |at| at.elapsed()),
        }
    }
}

/// The `agents` listing: `ancestor`'s live proper descendants at any depth,
/// ordered by id.  An agent lists what it spawned, never itself.
pub(crate) fn listing(ancestor: &Agent) -> Vec<AgentInfo> {
    let mut live = ancestor.walk();
    live.sort_unstable_by_key(|node| node.id);
    live.iter().map(|node| AgentInfo::of(node)).collect()
}
