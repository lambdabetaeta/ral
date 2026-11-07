//! Authority construction, judgment, and its two enforcers.

pub mod grant;
pub mod guard;
pub mod policy;
pub(crate) mod remedy;
pub mod sandbox;

impl policy::Baseline {
    /// Resolve the host system reads and attach its boot-pinned images.
    ///
    /// Must run after `sandbox::boot()` or `sandbox::pin()` has pinned the images.
    ///
    /// # Errors
    /// As [`of_home`](Self::of_home): a host whose system reads are malformed
    /// or cannot be walked.
    pub(crate) fn of_host() -> Result<Self, crate::error::Error> {
        use crate::site::{Silent, site};

        let home = site!(Silent::SystemReadsHome, crate::host::home());
        Ok(Self::of_home(home.as_deref())?
            .with_images(sandbox::reexec::own_binary(), sandbox::protected_images()))
    }
}
