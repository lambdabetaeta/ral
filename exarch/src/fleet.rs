//! The fleet: the run as a whole.
//!
//! Where an [`Agent`](crate::agent::Agent) is one uniform node, the `Fleet` is
//! the thin object that holds what every node *shares* — the one agent
//! registry, the one event bus, the focused-agent handle, and whether a human
//! is attached ([[decisions/260624_uniform-agent-nodes]]).  It owns no turn
//! logic: the trunk and each child drive themselves; the fleet is just where
//! the frontend reads "all live agents", "which one the human is attached to",
//! and "the bus to drain".
//!
//! It is built by the frontend ([`crate::tui::run`] / [`crate::headless::run`])
//! from handles the trunk already minted at construction — the same registry,
//! focus handle, and `interactive` flag the trunk and every fork hold — so the
//! fleet and its nodes never disagree about what is shared.

use crate::agent_registry::AgentRegistry;
use crate::bus::{AgentId, FleetBus};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The focus sentinel: no agent is attached.  Off the TUI the focus handle
/// holds this for the whole run (there is no `TAB`); in the TUI it means the
/// frontend resolves focus to the trunk.  Distinct from any real [`AgentId`].
pub const NO_FOCUS: AgentId = AgentId::MAX;

/// The shared spine of a run: the registry, the bus, the focused-agent handle,
/// and human-attachment.  Thin by design — see the module docs.
pub struct Fleet {
    /// Every live agent, the shared map cloned to each node.  "All live agents"
    /// is its contents; the fleet is alive while it is non-empty.
    pub agents: AgentRegistry,
    /// The one fan-out event bus the frontend drains.
    pub bus: FleetBus,
    /// The focused agent's id, shared with the frontend and every node.  `TAB`
    /// moves it; the focused agent parks for the human; off the TUI it stays
    /// [`NO_FOCUS`].
    pub focus: Arc<AtomicU64>,
    /// Whether a human is attached (the TUI).  With the trunk's `parent = None`
    /// this is what makes it the *conversing* trunk.
    pub interactive: bool,
}

impl Fleet {
    pub fn new(
        agents: AgentRegistry,
        bus: FleetBus,
        focus: Arc<AtomicU64>,
        interactive: bool,
    ) -> Self {
        Self {
            agents,
            bus,
            focus,
            interactive,
        }
    }

    /// The fleet is alive while any agent remains registered — the literal
    /// reading of "dies when no active agents remain".  An agent removes itself
    /// at termination (`reply`, quiescence, cancel); the conversing trunk stays
    /// until `/quit`, because it parks.
    pub fn alive(&self) -> bool {
        !self.agents.is_empty()
    }

    /// The currently focused agent's id, or `None` when focus rests on no live
    /// agent ([`NO_FOCUS`], or an id that has since settled) — the frontend
    /// then treats the trunk as focused.
    pub fn focused(&self) -> Option<AgentId> {
        let f = self.focus.load(Ordering::Relaxed);
        (f != NO_FOCUS).then_some(f)
    }
}
