//! The guest's only network endpoint is a gateway CONNECT proxy.
//! Exact public DNS names on port 443 are admitted; tunnel bytes stay opaque.

pub mod connect;
pub(crate) mod device;
mod ledger;
pub(crate) mod pipe;
mod refusal;
mod site;
pub(crate) mod stack;
pub(crate) mod vet;

pub use ledger::{AuditLog, LEDGER};
pub use stack::{Config, Session};
pub use vet::{Dialer, System};

#[cfg(feature = "test-util")]
pub use device::{TunDevice, spawn as spawn_device};

/// # Errors
/// Reports wire cloning or thread startup failure; the wire uses `guest_abi::packet` framing.
pub fn run(wire: ral_core::protocol::channel::WireStream, config: Config) -> std::io::Result<Session> {
    stack::run(wire, config)
}
