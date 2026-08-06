//! The desk's dial-side capability: [`crate::agent::RootConfig::hatchery`]
//! plumbs one in, `None` for every identity trunk. A wire trunk with spawn
//! fuel and no hatchery is refused at [`crate::agent::Agent::root`] with a
//! sentence — never a runtime surprise reached only once a model calls
//! `agent`.
//!
//! The desk cannot depend on `vm_manager` — this trait is the seam instead:
//! synod implements it over `vm_manager::Machine::accept_agent`, a test fake
//! implements it over a plain socketpair, and the desk's `agent-hatched`
//! handler never learns which.

use std::time::Duration;

/// Block until the guest dial correlated by `token` arrives.
///
/// Hand back the raw stream for the desk to adopt — preamble bytes still on
/// it, so the desk, not the hatchery, is what checks a stream's token against
/// the magic before ever trusting it as this hatch's own.
pub trait Hatchery: Send + Sync {
    /// # Errors
    /// Returns a sentence naming the wait if no dial bearing `token` arrives
    /// within `patience`.
    fn await_dial(
        &self,
        token: u64,
        patience: Duration,
    ) -> Result<ral_core::wire::WireStream, String>;

    /// The vsock port hatched children must dial — carried in `agent-start`'s
    /// `` `hatch `` answer, so the engine holds no ambient network config of
    /// its own.
    fn port(&self) -> u32;
}

/// The bound `agent-hatched` awaits a dial for: `ral-daemon`'s own dial
/// patience, the precedent this milestone reuses rather than inventing a
/// second number.
pub const DIAL_PATIENCE: Duration = Duration::from_secs(5);
