//! Only real machines are constructed; unavailable backends or media yield refusals.
//! The granted folder is shared into the guest; all product policy stays above this crate.
//! Boot returns the machine and both accepted wires together.

#[cfg(any(windows, all(target_os = "macos", target_arch = "aarch64")))]
use crate::site::{Silent, site};

mod dial;
mod error;
#[cfg(any(test, target_os = "macos", windows))]
mod lifecycle;
mod machine;
pub mod media;
#[cfg(any(windows, test))]
mod openvmm;
mod site;
mod spec;
#[cfg(target_os = "macos")]
pub mod vz;

pub use dial::{DialBackend, DialError};
pub use error::{
    Act, Error, GuestConsole, HostFile, MachineFiles, PowerEvent, Spelling, Wire, Worded,
};
pub use machine::{Booted, Hypervisor, Machine, Socket, Wires};
pub use spec::{BootArtifact, MachineSpec, Resources, SIZE};

/// Lazy so a refused backend never inflates media.
pub type BootMedia = Box<dyn FnOnce() -> Result<BootArtifact, Error>>;

/// # Errors
/// Returns the platform, entitlement or boot-file refusal.
pub fn direct(artifact: BootArtifact) -> Result<Box<dyn Hypervisor>, Error> {
    detect(Box::new(|| Ok(artifact)))
}

/// # Errors
/// Returns the platform, entitlement or media refusal.
pub fn detect(media: BootMedia) -> Result<Box<dyn Hypervisor>, Error> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        if !vz::entitled() {
            return Err(Error::Unentitled);
        }
        Ok(Box::new(vz::Vz::new(
            media()?.resolve()?,
            site!(Silent::SessionDiskFolder, std::env::temp_dir()),
        )))
    }
    #[cfg(windows)]
    {
        let vmm = openvmm::available()?;
        Ok(Box::new(openvmm::OpenVmm::new(
            media()?.resolve()?,
            vmm,
            site!(Silent::SessionDiskFolder, std::env::temp_dir()),
        )))
    }
    #[cfg(not(any(windows, all(target_os = "macos", target_arch = "aarch64"))))]
    {
        drop(media);
        Err(Error::NoBackend)
    }
}

#[cfg(test)]
#[cfg(not(any(windows, all(target_os = "macos", target_arch = "aarch64"))))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn without_a_backend_detect_does_not_ready_media() {
        assert!(matches!(
            detect(Box::new(|| panic!(
                "a platform without a backend needs no media"
            ))),
            Err(Error::NoBackend)
        ));
    }
}
