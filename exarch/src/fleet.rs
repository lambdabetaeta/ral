//! The fleet: the run as a whole.
//!
//! Where an [`Agent`](crate::agent::Agent) is one uniform node, the `Fleet` is
//! the thin object that holds what every node *shares* — the one agent
//! registry, the one event bus, and the one transport engine
//! ([[decisions/260624_uniform-agent-nodes]]).  It owns no turn logic: the
//! trunk and each child drive themselves; the fleet is just where the frontend
//! reads "all live agents" and "the bus to drain".
//!
//! It is built by the frontend ([`crate::tui::run`] / [`crate::headless::run`])
//! from handles the trunk already minted at construction — the same registry
//! and engine the trunk and every fork hold — so the fleet and its nodes never
//! disagree about what is shared.

pub(crate) mod desk;
pub mod egress;
pub mod registry;
pub mod schedule;

use crate::bus::FleetBus;
use crate::provider;
use registry::AgentRegistry;
use std::sync::Arc;

/// The shared spine of a run: the registry, the bus, and the transport engine.
/// Thin by design — see the module docs.
pub struct Fleet {
    /// Every live agent, the shared map cloned to each node.  "All live agents"
    /// is its contents; the fleet is alive while it is non-empty.
    pub agents: AgentRegistry,
    /// The one fan-out event bus the frontend drains.
    pub bus: FleetBus,
    /// The shared transport borrowed by every provider in the fleet.
    pub engine: Arc<provider::Engine>,
}

impl Fleet {
    pub fn new(agents: AgentRegistry, bus: FleetBus, engine: Arc<provider::Engine>) -> Self {
        Self {
            agents,
            bus,
            engine,
        }
    }
}
