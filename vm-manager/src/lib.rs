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
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[cfg(any(windows, all(target_os = "macos", target_arch = "aarch64")))]
    fn media() -> (tempfile::TempDir, BootMedia) {
        let dir = tempfile::tempdir().unwrap();
        let make = |leaf: &str| {
            let path = dir.path().join(leaf);
            std::fs::write(&path, b"media").unwrap();
            path
        };
        let artifact = BootArtifact {
            kernel: make("kernel"),
            initramfs: make("initramfs"),
            rootfs: make("rootfs.img"),
        };
        (dir, Box::new(move || Ok(artifact)))
    }

    #[cfg(not(any(windows, all(target_os = "macos", target_arch = "aarch64"))))]
    #[test]
    fn without_a_backend_detect_does_not_ready_media() {
        assert!(matches!(
            detect(Box::new(|| panic!(
                "a platform without a backend needs no media"
            ))),
            Err(Error::NoBackend)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn with_media_detect_follows_what_this_computer_permits() {
        use std::path::Path;

        let (_dir, media) = media();
        match (hcs::available(), detect(media)) {
            (Ok(()), answer) => assert!(answer.is_ok(), "a permitted computer must get a backend"),
            (Err(_), Ok(_)) => assert!(
                broker::client::Brokered::available(),
                "a computer the compute service refuses can only get a backend from a machine \
                 service, and no service is listening"
            ),
            (Err(why), Err(refusal)) => {
                let spell = |_: &Path| String::new();
                assert_eq!(
                    refusal.worded(spell).to_string(),
                    why.worded(spell).to_string(),
                    "the refusal must be the compute service's own"
                );
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn with_media_detect_follows_the_entitlement() {
        let (_dir, media) = media();
        if vz::entitled() {
            assert!(detect(media).is_ok());
        } else {
            let refusal = detect(media).err().unwrap();
            assert!(matches!(refusal, Error::Unentitled), "{refusal:?}");
        }
    }
}
