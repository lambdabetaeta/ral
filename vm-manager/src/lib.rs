//! Only real machines are constructed; unavailable backends or media yield refusals.
//! The granted folder is shared into the guest; all product policy stays above this crate.
//! Boot returns the machine and both accepted wires together.

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::site::{Silent, site};

#[cfg(windows)]
pub mod broker;
mod error;
#[cfg(windows)]
pub mod hcs;
#[cfg(any(test, target_os = "macos", windows))]
mod lifecycle;
mod machine;
pub mod media;
mod os_path;
mod site;
mod spec;
#[cfg(target_os = "macos")]
pub mod vz;

pub use error::{Act, Error, GuestConsole, HostFile, MachineFiles, Spelling, Wire, Worded};
pub use machine::{Booted, Hypervisor, Machine, Socket, Wires};
pub use spec::{BootArtifact, MachineSpec, Resources, SIZE};

/// Lazy so an installed Windows client never inflates media owned by the broker's shared cache.
pub type BootMedia = Box<dyn FnOnce() -> Result<BootArtifact, Error>>;

fn available() -> Result<(), Error> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        if vz::entitled() {
            Ok(())
        } else {
            Err(Error::Unentitled)
        }
    }
    #[cfg(windows)]
    {
        hcs::available()
    }
    #[cfg(not(any(windows, all(target_os = "macos", target_arch = "aarch64"))))]
    {
        Err(Error::NoBackend)
    }
}

#[cfg(any(windows, all(target_os = "macos", target_arch = "aarch64")))]
fn construct(artifact: BootArtifact) -> Box<dyn Hypervisor> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Box::new(vz::Vz::new(
            artifact,
            site!(Silent::SessionDiskFolder, std::env::temp_dir()),
        ))
    }
    #[cfg(windows)]
    {
        Box::new(hcs::Hyperv::new(artifact, hcs::Hyperv::default_cache()))
    }
}

/// # Errors
/// Returns the platform, entitlement, permission or boot-file refusal.
pub fn direct(artifact: BootArtifact) -> Result<Box<dyn Hypervisor>, Error> {
    local(|| Ok(artifact))
}

/// # Errors
/// Returns the platform, entitlement, permission or media refusal.
pub fn detect(media: BootMedia) -> Result<Box<dyn Hypervisor>, Error> {
    #[cfg(windows)]
    if broker::client::Brokered::available() {
        return Ok(Box::new(broker::client::Brokered));
    }
    local(media)
}

fn local(
    media: impl FnOnce() -> Result<BootArtifact, Error>,
) -> Result<Box<dyn Hypervisor>, Error> {
    available()?;
    #[cfg(any(windows, all(target_os = "macos", target_arch = "aarch64")))]
    {
        Ok(construct(media()?.resolve()?))
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
