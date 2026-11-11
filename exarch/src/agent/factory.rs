//! The desk's engine-making seam. The desk cannot depend on `vm_manager`, so
//! synod implements [`Factory`] over `Machine::connect_guest` and a test fake
//! over a socketpair.

/// Where a wire host gets engines: one connection to a fresh, unattached
/// engine per call. synod's is the guest daemon; the desk never learns which.
pub trait Factory: Send + Sync {
    /// # Errors
    /// Returns the original typed factory cause when no engine could be made.
    fn engine(
        &self,
    ) -> Result<ral_core::protocol::channel::WireStream, ral_core::fact::EngineFactoryFailure>;
}
