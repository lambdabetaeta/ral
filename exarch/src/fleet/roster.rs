//! What the `exarch-agents` family answers.
//!
//! [`listing`] is one row per live agent in the reader's own tree; [`summary`]
//! is the two integers every other transition answers instead.  Both derive
//! fresh at read time — nothing here stores state of its own.

use crate::agent::Agent;
use std::path::PathBuf;
use std::sync::Arc;
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
    /// Parked holding a reply its spawner has yet to fetch.
    Replied,
    /// Parked with no reply — a child a human is talking to.
    Waiting,
}

/// Who started a listed agent — the edge that keeps a flat fleet listing a
/// tree.  A root was started by a human, and there is no name to give.
pub enum Spawner {
    Root,
    Agent(String),
}

/// One live agent, for the `exarch-agents` listing.
pub struct AgentInfo {
    pub name: String,
    pub spawner: Spawner,
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
            spawner: agent
                .parent()
                .map_or(Spawner::Root, |up| Spawner::Agent(up.name().to_string())),
            log_dir: agent.log_dir().to_path_buf(),
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

/// What every tag but `` `list `` and `` `read `` answers: the world after the
/// transition, at O(1) to read rather than O(fleet) to carry.
pub struct Summary {
    /// Other live agents in `reader`'s tree — company, not bookkeeping.  Zero
    /// is the honest "you are alone here".
    pub live: usize,
    /// How many of `reader`'s own children park holding a value it has not
    /// fetched.  Direct children alone: a reply is deposited for the spawner,
    /// so a deeper descendant's value is owed to its own parent, not up the
    /// whole chain.  The only number that asks for an action (`` `read ``), and
    /// the one an eviction cannot take away.
    pub replied: usize,
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
