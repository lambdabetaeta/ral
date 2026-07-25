//! One machine core for synod.
//!
//! Synod (office work) wants one thing from a machine: give the agent a
//! folder to work in, walled off from the rest of the computer.  This
//! crate is that, and nothing more — [`detect`] finds the one backend this
//! build can actually run, [`Hypervisor::boot`] turns a [`MachineSpec`]
//! into a [`Machine`], and [`Machine::take_control`] hands over the wire
//! the engine's runs run across.
//!
//! # What this crate does, stated plainly
//!
//! `dev/docs/VM/SYNOD.md` §4 has the agent work in the granted folder
//! itself — shared into a guest under a hardware boundary — with a
//! checkpoint before the job and conflict-checked undo after it.  That
//! safety net is host-side product code (`synod::workspace`), and none of
//! this crate's business: a machine here only *places* the folder and
//! starts the guest around it.
//!
//! # A wall, not a policy
//!
//! Every machine this crate can start is a real virtual machine: the guest
//! can reach the granted folder and the host's control socket, and nothing
//! else — not because it is forbidden to, but because nothing else is
//! there.  A synod that cannot boot one refuses to start rather than fall
//! back to some weaker mode; [`detect`] is where that refusal is decided,
//! and it says which of the wrong platform, the missing boot media, or the
//! missing entitlement stood in the way.
//!
//! # Backends
//!
//! - [`vz`] — Apple's Virtualization.framework, macOS only.  It starts
//!   machines only in a signed process carrying the virtualization
//!   entitlement and handed real boot media, which is exactly what
//!   [`detect`] checks before answering with it.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
pub mod vz;

/// The folder an agent is given, and where it appears to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// The granted folder on this computer.
    pub host_path: PathBuf,
    /// Where that folder appears inside a guest — `/work`, by convention.
    pub guest_path: PathBuf,
    /// Ask that the agent may read the folder but not change it, enforced
    /// by the guest mount itself.
    pub read_only: bool,
}

/// What to boot: one workspace and the resources to work in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineSpec {
    pub workspace: Workspace,
    /// Processors to give the machine.
    pub vcpus: u8,
    /// Memory to give the machine, in mebibytes.
    pub memory_mib: u32,
}

impl MachineSpec {
    /// The least memory any machine is allowed to ask for.  Below this a
    /// guest cannot finish booting, so it is a spec error rather than a
    /// boot failure.
    pub const LEAST_MEMORY_MIB: u32 = 256;

    /// The conventional mount point for the granted folder inside a guest.
    pub const GUEST_WORKSPACE: &'static str = "/work";

    /// A read-write grant of `folder` with modest resources: enough to run
    /// a document or build toolchain, small enough to boot on a laptop.
    pub fn for_folder(folder: impl Into<PathBuf>) -> Self {
        Self {
            workspace: Workspace {
                host_path: folder.into(),
                guest_path: PathBuf::from(Self::GUEST_WORKSPACE),
                read_only: false,
            },
            vcpus: 4,
            memory_mib: 4096,
        }
    }

    /// Check the spec and resolve the granted folder to a real absolute
    /// path, following any symbolic links.
    ///
    /// Every backend calls this before it commits a single resource, so a
    /// bad spec is refused by the same words whatever the platform.
    ///
    /// # Errors
    /// Returns [`Error`] if the granted folder is missing, is not a folder,
    /// or cannot be read; if the guest mount point is not absolute; or if
    /// the resource limits are too small to boot.
    #[allow(
        clippy::disallowed_methods,
        reason = "REASONED-SILENT: resolving the granted folder before a machine exists. \
                  This runs on the host, before any engine, so there is no Shell to route \
                  through `crate::path::canon` and no run to raise a card in; vm-manager \
                  deliberately does not depend on ral-core."
    )]
    pub fn resolve(&self) -> Result<PathBuf, Error> {
        if self.vcpus == 0 {
            return Err(Error::NoProcessors);
        }
        if self.memory_mib < Self::LEAST_MEMORY_MIB {
            return Err(Error::TooLittleMemory {
                asked_mib: self.memory_mib,
                least_mib: Self::LEAST_MEMORY_MIB,
            });
        }
        if !self.workspace.guest_path.is_absolute() {
            return Err(Error::GuestPathNotAbsolute(
                self.workspace.guest_path.clone(),
            ));
        }

        let path = &self.workspace.host_path;
        let root = std::fs::canonicalize(path).map_err(|cause| match cause.kind() {
            io::ErrorKind::NotFound => Error::NoSuchFolder(path.clone()),
            _ => Error::FolderUnreadable {
                path: path.clone(),
                cause,
            },
        })?;
        if root.is_dir() {
            Ok(root)
        } else {
            Err(Error::NotAFolder(root))
        }
    }
}

/// The kernel, initramfs, and rootfs image a hardware machine boots from.
///
/// Not part of a [`MachineSpec`] — a spec says *what folder, how much
/// machine*, not *which image* — and not this crate's to find: the media
/// ships with the application, so the application hands it to [`detect`].
/// The kernel and initramfs are `boot.img` of `dev/docs/VM/SYNOD.md` §7,
/// versioned with the application; the rootfs is the pinned, read-only
/// Ubuntu userland, downloaded and checksummed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootArtifact {
    /// The Linux kernel to boot directly.
    pub kernel: PathBuf,
    /// The initial ramdisk that assembles the overlay root and hands off to
    /// the daemon.
    pub initramfs: PathBuf,
    /// The read-only rootfs image: the guest's `/dev/vda`.
    pub rootfs: PathBuf,
}

/// Something that can start a machine.
pub trait Hypervisor {
    /// The backend's name, fit to show a user: `"Virtualization.framework"`.
    fn name(&self) -> &'static str;

    /// Start a machine for `spec`.
    ///
    /// # Errors
    /// Returns [`Error`] if the spec is not usable (see
    /// [`MachineSpec::resolve`]) or if the backend cannot start.
    fn boot(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>, Error>;
}

/// A running machine holding one workspace, walled off in a virtual
/// machine.
pub trait Machine {
    /// Where the engine should work — the guest path a real hypervisor
    /// mounted the granted folder at.
    fn workspace_path(&self) -> &Path;

    /// The host end of this machine's control-plane connection — the stream
    /// the frame protocol of `dev/docs/VM/SYNOD.md` §3 runs over.
    ///
    /// A machine has exactly one, accepted when the guest's daemon dialed it
    /// at boot; taking it transfers ownership away from the machine, so a
    /// caller may ask for it no more than once.
    ///
    /// Unix only: a control plane here is an `AF_VSOCK` stream, named by its
    /// file descriptor.  The Windows Hyper-V backend will choose its own
    /// socket type once it exists; this signature does not presume it.
    ///
    /// # Panics
    /// Implementations panic if asked twice — a caller's error, not a
    /// machine's: the one caller in this crate's graph
    /// (`synod::session::Conversation::begin`) takes it once, immediately
    /// after boot.
    #[cfg(unix)]
    fn take_control(&mut self) -> std::os::fd::OwnedFd;

    /// Stop the machine and release what it holds.
    ///
    /// Dropping a machine also stops it; this exists so a caller that wants
    /// to *report* a failed shutdown can have one.
    ///
    /// # Errors
    /// Returns [`Error`] if the machine did not stop cleanly.
    fn shutdown(self: Box<Self>) -> Result<(), Error>;
}

/// Shown by [`detect`] on every platform this crate has no backend for at
/// all — today, anything but an Apple Silicon Mac — whatever boot media it
/// was given.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
const NOT_APPLE_SILICON: &str =
    "this computer cannot run synod's virtual machine — it only starts on an Apple Silicon Mac";

/// Shown by [`detect`] when the platform could run a machine, but this
/// build shipped with no boot media to start one from.
const NO_BOOT_MEDIA: &str = "synod could not find the virtual-machine image it ships with — \
                              ask your IT department to reinstall it";

/// Shown by [`detect`] when boot media is present but this process is not
/// signed with the entitlement Virtualization.framework demands.
const UNENTITLED: &str = "this copy of synod is not signed with the entitlement it needs to \
                           start a virtual machine — ask your IT department for a properly \
                           signed build";

/// Start the one backend this build can actually run, given the boot media
/// the application ships.
///
/// There is no fallback: a synod that cannot boot a real machine refuses to
/// start.
///
/// # Errors
/// Returns a sentence for the person running synod — not a programmer —
/// naming whichever of these stood in the way: this computer is not an
/// Apple Silicon Mac ([`NOT_APPLE_SILICON`]); it is one, but `boot` is
/// `None` ([`NO_BOOT_MEDIA`]); or it is one with media in hand, but this
/// process is not signed with the virtualization entitlement
/// ([`UNENTITLED`]).
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn detect(boot: Option<BootArtifact>) -> Result<Box<dyn Hypervisor>, String> {
    let _ = boot;
    Err(NOT_APPLE_SILICON.to_string())
}

/// Start the one backend this build can actually run, given the boot media
/// the application ships.
///
/// On Apple Silicon macOS the platform check the other-platform overload
/// makes always passes, so the only refusals left are [`NO_BOOT_MEDIA`] and
/// [`UNENTITLED`].
///
/// # Errors
/// Returns [`NO_BOOT_MEDIA`] if `boot` is `None`, or [`UNENTITLED`] if this
/// process is not signed with the virtualization entitlement.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn detect(boot: Option<BootArtifact>) -> Result<Box<dyn Hypervisor>, String> {
    let Some(artifact) = boot else {
        return Err(NO_BOOT_MEDIA.to_string());
    };
    if !vz::entitled() {
        return Err(UNENTITLED.to_string());
    }
    Ok(Box::new(vz::Vz::new(artifact, std::env::temp_dir())))
}

/// Why a machine could not be started.
///
/// The [`Display`](fmt::Display) text is written for the person who granted
/// the folder — a university secretary, not a programmer.
#[derive(Debug)]
pub enum Error {
    /// The granted folder is not there.
    NoSuchFolder(PathBuf),
    /// The granted path exists but is a file, not a folder.
    NotAFolder(PathBuf),
    /// The granted folder is there but could not be read.
    FolderUnreadable { path: PathBuf, cause: io::Error },
    /// The workspace's mount point inside the guest is not an absolute path.
    GuestPathNotAbsolute(PathBuf),
    /// The spec asked for a machine with no processors.
    NoProcessors,
    /// The spec asked for less memory than a guest needs to boot.
    TooLittleMemory { asked_mib: u32, least_mib: u32 },
    /// The backend itself could not start.
    Unavailable {
        hypervisor: &'static str,
        why: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchFolder(path) => write!(
                f,
                "there is no folder at {}. Has it been moved or renamed?",
                path.display()
            ),
            Self::NotAFolder(path) => write!(
                f,
                "{} is a file, not a folder. Please choose the folder that holds the documents to work on.",
                path.display()
            ),
            Self::FolderUnreadable { path, cause } => write!(
                f,
                "the folder {} could not be opened: {cause}. Do you have permission to open it?",
                path.display()
            ),
            Self::GuestPathNotAbsolute(path) => write!(
                f,
                "the folder has to appear inside the machine at a full path such as {}, but {} is not one",
                MachineSpec::GUEST_WORKSPACE,
                path.display()
            ),
            Self::NoProcessors => {
                f.write_str("a machine needs at least one processor, and none were asked for")
            }
            Self::TooLittleMemory {
                asked_mib,
                least_mib,
            } => write!(
                f,
                "a machine needs at least {least_mib} MB of memory to start, and only {asked_mib} MB were asked for"
            ),
            Self::Unavailable { hypervisor, why } => {
                write!(f, "{hypervisor} could not start a machine: {why}")
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::FolderUnreadable { cause, .. } => Some(cause),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact() -> BootArtifact {
        BootArtifact {
            kernel: PathBuf::from("/boot/kernel"),
            initramfs: PathBuf::from("/boot/initramfs"),
            rootfs: PathBuf::from("/boot/rootfs.img"),
        }
    }

    /// Off Apple Silicon, `detect` refuses outright — media or not, since
    /// this crate has no backend for the platform at all.
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    #[test]
    fn off_apple_silicon_detect_refuses_regardless_of_media() {
        assert!(detect(None).is_err());
        assert!(detect(Some(artifact())).is_err());
    }

    /// On the eligible platform, no media is its own refusal.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn without_media_detect_refuses_by_name() {
        let message = detect(None).err().unwrap();
        assert!(message.contains("could not find"), "{message}");
    }

    /// With media, the answer follows the process's own entitlement: a bare
    /// `cargo test` binary is unentitled, while the same binary signed
    /// through `dev/scripts/sign-virtualization.sh` is not — so the law
    /// under test is agreement, not either fixed answer.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn with_media_detect_follows_the_entitlement() {
        if vz::entitled() {
            assert!(detect(Some(artifact())).is_ok());
        } else {
            let message = detect(Some(artifact())).err().unwrap();
            assert!(message.contains("entitlement"), "{message}");
        }
    }
}
