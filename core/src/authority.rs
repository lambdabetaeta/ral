//! Authority construction, judgment, and its two enforcers.

pub mod grant;
pub mod guard;
pub mod policy;
pub mod sandbox;

impl policy::Baseline {
    /// Resolve the host system reads and attach its boot-pinned images.
    ///
    /// Must run after `sandbox::boot()` or `sandbox::pin()` has pinned the images.
    ///
    /// # Errors
    /// A host path whose system reads cannot be resolved.
    pub(crate) fn of_host() -> Result<Self, String> {
        use crate::site::{Silent, site};

        let home = site!(Silent::SystemReadsHome, crate::host::home());
        Ok(Self::of_home(home.as_deref())?
            .with_images(sandbox::reexec::own_binary(), sandbox::protected_images()))
    }
}
