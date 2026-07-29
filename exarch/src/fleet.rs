//! The fleet: what every agent node of a run shares — one agent registry, one
//! event bus, one transport engine.
//!
//! It drives no exchange of its own; the trunk and each child attend to
//! themselves, and the fleet is only where the frontend reads "all live
//! agents" and the bus to drain.
//!
//! The frontend ([`crate::tui::run`] / [`crate::headless::run`]) builds it over
//! the trunk's own registry and inbox and the engine every provider is built
//! on — never fresh ones — so no node can disagree about what is shared.

pub(crate) mod desk;
pub mod registry;
pub mod schedule;

use crate::bus::FleetBus;
use crate::provider;
use registry::AgentRegistry;
use std::sync::Arc;

/// The one registry, bus, and engine every node of the run shares.
pub struct Fleet {
    pub agents: AgentRegistry,
    /// Fan-in: every node emits into it, and only the frontend drains it.
    pub(crate) bus: FleetBus,
    /// A `/model` switch mints its replacement provider on this same engine.
    pub engine: Arc<provider::Engine>,
}
