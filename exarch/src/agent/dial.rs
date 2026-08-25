//! The desk's dial-side capability: [`crate::agent::RootConfig::dial`] plumbs
//! one in, `None` for every identity trunk. A wire trunk with spawn fuel and
//! no dialler is refused at [`crate::agent::Agent::root`] with a sentence —
//! never a runtime surprise reached only once a model calls `agent`.
//!
//! The desk cannot depend on `vm_manager` — this trait is the seam instead:
//! synod implements it over `vm_manager::Machine::connect_guest`, a test fake
//! implements it over a plain socketpair, and the desk never learns which.

/// Open a connection to the listener a guest bound for the duration of one
/// spawn, and named in its enquiry.
pub trait Dial: Send + Sync {
    /// # Errors
    /// Returns a sentence naming the dial if nothing on `port` answers.
    fn dial(&self, port: u32) -> Result<ral_core::wire::WireStream, String>;
}
