//! One machine core for synod.
//!
//! Synod (office work) wants one thing from a machine: give the agent a
//! folder to work in, walled off from the rest of the computer.  This
//! crate is that, and nothing more — [`detect`] finds the one backend this
//! build can actually run, [`Hypervisor::boot`] turns a [`MachineSpec`]
//! into a [`Machine`], and [`Machine::take_wires`] hands over the two wires
//! the engine's runs and the guest's network run across.
//!
//! # What this crate does, stated plainly
//!
//! The agent works in the granted folder itself — shared into a guest
//! under a hardware boundary — with a checkpoint before the job and
//! conflict-checked undo after it.  That safety net is host-side product
//! code (`synod::workspace`), and none of this crate's business: a machine
//! here only *places* the folder and starts the guest around it.
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
//! Each is compiled only on the platform it speaks for, so neither is
//! nameable as a link from a page the other one builds; they are cited here by
//! module file.
//!
//! - `vz` (`vm-manager/src/vz.rs`) — Apple's Virtualization.framework, macOS
//!   only.  It starts machines only in a signed process carrying the
//!   virtualization entitlement and handed real boot media, which is exactly
//!   what [`detect`] checks before answering with it.
//! - `hcs` (`vm-manager/src/hcs/`) — Hyper-V through the Host Compute System
//!   API, Windows only.  It starts machines only for a user the compute service
//!   serves — an administrator or a member of the *Hyper-V Administrators*
//!   group — and handed real boot media, which is what [`detect`] checks there.
//!
//! The two are the same machine assembled from different parts, and the guest
//! cannot tell which one booted it; `hcs`'s own module docs carry the table of
//! correspondences.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(windows)]
pub mod broker;
#[cfg(windows)]
pub mod hcs;
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
        // Judged by Linux's rule, deliberately, and not `Path::is_absolute` —
        // which answers with the *host's*.  The guest is Linux under every
        // backend, so `/work` is an absolute path in the only namespace this
        // field names; on Windows `Path::is_absolute` would say otherwise
        // (nothing without a drive letter or a UNC prefix is absolute there)
        // and refuse every well-formed spec on that platform alone.
        if !self.workspace.guest_path.to_string_lossy().starts_with('/') {
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
/// The kernel and initramfs are `boot.img`, versioned with the application;
/// the rootfs is the pinned, read-only Ubuntu userland, downloaded and
/// checksummed.
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

impl BootArtifact {
    /// Resolve every file to an absolute path, following symbolic links.
    ///
    /// Every backend calls this before it commits a single resource, for the
    /// same reason [`MachineSpec::resolve`] exists — but the reason is sharper
    /// here, because these paths are read by *another process*.  A hypervisor's
    /// service or framework opens the kernel itself, from its own working
    /// directory: `vmcompute` runs in `C:\Windows\System32`, so a relative path
    /// that resolved perfectly for the caller names nothing at all by the time
    /// the machine is built, and the failure arrives as a bare "the system
    /// cannot find the path specified" with no clue which path was meant.
    ///
    /// # Errors
    /// Returns [`Error::MissingBootFile`] naming the first file that is not
    /// there or cannot be read.
    #[allow(
        clippy::disallowed_methods,
        reason = "REASONED-SILENT: resolving the boot media before a machine exists, exactly as \
                  `MachineSpec::resolve` resolves the granted folder — host-side setup with no \
                  Shell to route through and no run to raise a card in."
    )]
    pub fn resolve(&self) -> Result<Self, Error> {
        let resolve_one = |path: &PathBuf| {
            std::fs::canonicalize(path).map_err(|cause| Error::MissingBootFile {
                path: path.clone(),
                cause,
            })
        };
        Ok(Self {
            kernel: resolve_one(&self.kernel)?,
            initramfs: resolve_one(&self.initramfs)?,
            rootfs: resolve_one(&self.rootfs)?,
        })
    }
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

/// The `AF_VSOCK`/`AF_HYPERV` port the host listens on for the net wire.
///
/// One number, both backends: it is carried to the guest on the kernel
/// command line as [`ral_daemon::boot::Net::port`], so the listener a
/// backend binds and the port the guest dials agree by construction, not by
/// two crates' constants happening to match.
pub const NET_PORT: u32 = 1730;

/// The guest's fixed address on the net wire's virtual link, and the subnet
/// and gateway it is told alongside it.
///
/// The one fact both backends' `kernel_command_line` build a
/// [`ral_daemon::boot::Net`] from, so it lives here once rather than as
/// three private constants kept in agreement by hand. It is not a route to
/// anywhere real: the far end is not a router but a user-mode TCP/IP stack
/// in a host process (`guest-net`), answering as if it were the gateway so
/// the guest's kernel hands it every packet leaving the subnet. The guest
/// never learns any of this beyond the address, prefix, and gateway it is
/// handed on the kernel command line under `ral.net`
/// ([`ral_daemon::boot::command_line`]) — it boots believing it is on an
/// ordinary `/24`.
pub const GUEST_LINK: GuestLink = GuestLink {
    address: std::net::Ipv4Addr::new(10, 0, 2, 15),
    prefix: 24,
    gateway: std::net::Ipv4Addr::new(10, 0, 2, 2),
};

/// [`ral_daemon::boot::Net`] minus the port.
///
/// The shape both backends build that struct from, since the port
/// ([`NET_PORT`]) is a constant of this crate while the address is
/// [`GUEST_LINK`], a value of this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestLink {
    pub address: std::net::Ipv4Addr,
    pub prefix: u8,
    pub gateway: std::net::Ipv4Addr,
}

/// The host ends of a machine's two wires.
///
/// [`Wires::control`] is the control-plane stream the engine's frame
/// protocol runs over; [`Wires::net`] is the guest's peer for the net
/// wire's user-mode TCP/IP stack.
///
/// One struct rather than two fields on [`Machine`] because a machine hands
/// both over at once, from the same accepted-at-boot pair, and a caller that
/// took only one would leave the other's ownership nowhere — every consumer
/// wants both wires or neither.
///
/// The handle differs by platform because the socket does, and neither
/// platform's owner type is the other's: on Unix a wire is an `AF_VSOCK`
/// stream named by its file descriptor, on Windows an `AF_HYPERV` socket.
/// Both are adopted into the same frame channel by
/// [`ral_core::wire::WireStream`](../../core/src/wire.rs), whose docs explain
/// why std's stream types can carry either.
#[cfg(unix)]
#[derive(Debug)]
pub struct Wires {
    /// The control plane — see the struct docs.
    pub control: std::os::fd::OwnedFd,
    /// The net wire — see the struct docs.
    pub net: std::os::fd::OwnedFd,
}

/// See the Unix twin above for the whole contract, which this shares but for
/// the handles' type.
#[cfg(windows)]
#[derive(Debug)]
pub struct Wires {
    /// The control plane — see the struct docs.
    pub control: std::os::windows::io::OwnedSocket,
    /// The net wire — see the struct docs.
    pub net: std::os::windows::io::OwnedSocket,
}

/// One connection into a guest — the owned handle
/// [`Machine::connect_guest`] hands back.
///
/// The same one-word-per-platform shape [`Wires`] carries per field: a
/// single owned socket handle, `OwnedFd` on Unix and `OwnedSocket` on
/// Windows, because that is what each platform's dial produces and
/// [`ral_core::wire::WireStream`](../../core/src/wire.rs) adopts either
/// kind the same way.
#[cfg(unix)]
pub type AgentDial = std::os::fd::OwnedFd;

/// See the Unix twin above for the whole contract, which this shares but
/// for the handle's type.
#[cfg(windows)]
pub type AgentDial = std::os::windows::io::OwnedSocket;

/// A running machine holding one workspace, walled off in a virtual
/// machine.
///
/// `Send` states a fact every backend already meets — the `!Send`
/// `VZVirtualMachine` stays on its own thread, spoken to through a channel —
/// and exists so a machine may be moved to whichever thread holds a session.
pub trait Machine: Send {
    /// Where the engine should work — the guest path a real hypervisor
    /// mounted the granted folder at.
    fn workspace_path(&self) -> &Path;

    /// This machine's two wires — see [`Wires`] for what each carries.
    ///
    /// A machine has exactly one of each, accepted when the guest's daemon
    /// dialed them at boot; taking them transfers ownership away from the
    /// machine, so a caller may ask for them no more than once.
    ///
    /// # Panics
    /// Implementations panic if asked twice — a caller's error, not a
    /// machine's: the one caller in this crate's graph
    /// (`synod::session::control_seat`) takes them once, immediately after
    /// boot.
    fn take_wires(&mut self) -> Wires;

    /// Dial a port the *guest* listens on, and hand back the host end.
    ///
    /// The host opens every wire it does not accept at boot, and that is why
    /// no token correlates one: the side that opened the connection already
    /// knows what it opened it for.
    ///
    /// Defaulted, so a backend that cannot dial inwards refuses in a sentence
    /// rather than failing to compile.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::Unsupported`] unless the backend overrides
    /// this, and otherwise whatever the dial itself reports.
    fn connect_guest(&self, port: u32) -> io::Result<AgentDial> {
        let _ = port;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this backend cannot dial into its guest — it can only accept the connections a \
             guest opens, so a port bound inside the guest is unreachable from here",
        ))
    }

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
/// all — anything but an Apple Silicon Mac or a Windows with the Virtual
/// Machine Platform — whatever boot media it was given.
#[cfg(not(any(windows, all(target_os = "macos", target_arch = "aarch64"))))]
const NO_BACKEND: &str = "this computer cannot run synod's virtual machine — it starts on an \
                          Apple Silicon Mac, or on Windows with the Virtual Machine Platform";

/// Shown by [`detect`] when the platform could run a machine, but this
/// build shipped with no boot media to start one from.
#[cfg(any(windows, all(target_os = "macos", target_arch = "aarch64")))]
const NO_BOOT_MEDIA: &str = "synod could not find the virtual-machine image it ships with — \
                              ask your IT department to reinstall it";

/// Shown by [`detect`] when boot media is present but this process is not
/// signed with the entitlement Virtualization.framework demands.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const UNENTITLED: &str = "this copy of synod is not signed with the entitlement it needs to \
                           start a virtual machine — ask your IT department for a properly \
                           signed build";

/// How a front-end readies its boot media, run only if the backend turns out
/// to need it.
///
/// A closure rather than a [`BootArtifact`] because readying media is not free
/// and is not always necessary.  The rootfs ships compressed — a Windows
/// Installer cabinet cannot hold a two-gigabyte file at all — so producing an
/// attachable image means inflating two and a half gigabytes.  An installed
/// synod never needs to: it boots through the machine service, which has its own
/// copy and inflates it once into its own cache, where every session on the
/// computer shares it.  Whether that is the case is [`detect`]'s to decide, so
/// what it takes is the *means* of getting media rather than media itself, and
/// the work happens on exactly the paths that use it.
pub type BootMedia = Box<dyn FnOnce() -> Result<BootArtifact, String>>;

/// Start the one backend this build can actually run, given the boot media
/// the application ships.
///
/// There is no fallback: a synod that cannot boot a real machine refuses to
/// start.
///
/// This build has no backend at all, so the refusal is the platform itself
/// and nothing downstream of it is ever weighed — the boot media goes
/// unread, and is not so much as readied. The refusals the two real overloads
/// can additionally give are their own, and named in their docs.
///
/// # Errors
/// Always, with [`NO_BACKEND`].
#[cfg(not(any(windows, all(target_os = "macos", target_arch = "aarch64"))))]
pub fn detect(boot: Option<BootMedia>) -> Result<Box<dyn Hypervisor>, String> {
    drop(boot);
    Err(NO_BACKEND.to_string())
}

/// Start the one backend this build can actually run, given the boot media
/// the application ships.
///
/// On Windows the platform check the no-backend overload makes always passes,
/// and what remains are two questions about *this* computer rather than about
/// the operating system: is there an image to boot, and will the compute
/// service serve this user (see [`hcs::available`], which names the Virtual
/// Machine Platform feature or the Hyper-V Administrators group as the case
/// requires).
///
/// The order is deliberate and matches the macOS overload's: the missing image
/// is asked about first, because it is the answer a fresh install is most
/// likely to need and the cheaper question to ask.  Asking is all that happens
/// there, though — the media is *readied* last, after the compute service has
/// agreed to serve this user, so that a refusal costs no work.
///
/// # Errors
/// Returns [`NO_BOOT_MEDIA`] if there is no service and `boot` is `None`,
/// [`hcs::available`]'s own sentence — with the service named beside it — if
/// this Windows will not host a guest for this user directly either, or
/// whatever `boot` itself reports when the media cannot be readied.
#[cfg(windows)]
pub fn detect(boot: Option<BootMedia>) -> Result<Box<dyn Hypervisor>, String> {
    // The broker first, always: an installed synod runs unprivileged, and the
    // service is what makes that possible (`broker`'s own docs carry the
    // argument).  It needs no boot media from this process — it has its own,
    // installed beside it — so the question of media does not even arise on the
    // path a user is on.
    if broker::client::Brokered::available() {
        return Ok(Box::new(broker::client::Brokered));
    }

    // No service: this is a checkout, not an installation.  Fall back to
    // creating the machine here, which works for whoever the compute service
    // already serves — a maintainer in an elevated shell, or in the group.
    let Some(media) = boot else {
        return Err(NO_BOOT_MEDIA.to_string());
    };
    hcs::available().map_err(|why| {
        format!(
            "{why}. (Synod's own installer avoids this entirely: it registers a service that \
             holds that one privilege so the application does not have to. This copy of synod is \
             running without it.)"
        )
    })?;
    Ok(Box::new(hcs::Hyperv::new(
        media()?,
        hcs::Hyperv::default_cache(),
    )))
}

/// Start the one backend this build can actually run, given the boot media
/// the application ships.
///
/// On Apple Silicon macOS the platform check the other-platform overload
/// makes always passes, so the only refusals left are [`NO_BOOT_MEDIA`] and
/// [`UNENTITLED`].
///
/// # Errors
/// Returns [`NO_BOOT_MEDIA`] if `boot` is `None`, [`UNENTITLED`] if this
/// process is not signed with the virtualization entitlement, or whatever
/// `boot` itself reports when the media cannot be readied.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn detect(boot: Option<BootMedia>) -> Result<Box<dyn Hypervisor>, String> {
    let Some(media) = boot else {
        return Err(NO_BOOT_MEDIA.to_string());
    };
    if !vz::entitled() {
        return Err(UNENTITLED.to_string());
    }
    Ok(Box::new(vz::Vz::new(media()?, std::env::temp_dir())))
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
    /// A file of the boot media this build ships is missing or unreadable.
    MissingBootFile { path: PathBuf, cause: io::Error },
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
            Self::MissingBootFile { path, cause } => write!(
                f,
                "synod's virtual-machine image is missing a file it needs to start: {} ({cause}). \
                 Ask your IT department to reinstall synod.",
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
            Self::FolderUnreadable { cause, .. } | Self::MissingBootFile { cause, .. } => {
                Some(cause)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conventional spec for a real folder resolves on every platform.
    ///
    /// It is worth a test of its own because the two paths in a spec live in
    /// different namespaces — the granted folder is the host's, the mount point
    /// is the guest's — and judging the guest's by the host's rule refused every
    /// spec on Windows, where `/work` is not an absolute path. A boot cannot
    /// begin until this passes, so the failure looked like a hypervisor problem
    /// rather than an arithmetic one.
    #[test]
    fn a_conventional_spec_resolves_on_this_platform() {
        let dir = tempfile::tempdir().unwrap();
        let spec = MachineSpec::for_folder(dir.path());
        assert_eq!(spec.workspace.guest_path, PathBuf::from("/work"));
        let root = spec.resolve().expect("a real folder and the /work mount");
        assert!(root.is_dir());
    }

    /// A mount point that is not a path in the guest's own namespace is
    /// refused — including one that a *host* would call absolute.
    #[test]
    fn a_guest_path_is_judged_by_the_guests_rule() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = MachineSpec::for_folder(dir.path());
        spec.workspace.guest_path = PathBuf::from(r"C:\work");
        assert!(matches!(
            spec.resolve(),
            Err(Error::GuestPathNotAbsolute(_))
        ));
    }

    fn artifact() -> BootArtifact {
        BootArtifact {
            kernel: PathBuf::from("/boot/kernel"),
            initramfs: PathBuf::from("/boot/initramfs"),
            rootfs: PathBuf::from("/boot/rootfs.img"),
        }
    }

    /// The same media, as the closure [`detect`] takes.  It is never called on
    /// the paths that refuse, which is the point of it being a closure — these
    /// tests would notice, because a `detect` that readied media before deciding
    /// would have to open `/boot/kernel`, and that path exists on no computer
    /// this test suite runs on.
    fn media() -> BootMedia {
        Box::new(|| Ok(artifact()))
    }

    /// On a platform with no backend at all, `detect` refuses outright —
    /// media or not, since there is nothing downstream of the platform to
    /// weigh.
    #[cfg(not(any(windows, all(target_os = "macos", target_arch = "aarch64"))))]
    #[test]
    fn without_a_backend_detect_refuses_regardless_of_media() {
        assert!(detect(None).is_err());
        assert!(detect(Some(media())).is_err());
    }

    /// On Windows, no media is its own refusal — asked before the compute
    /// service is troubled at all.
    ///
    /// Unless there is a machine service, which has media of its own and needs
    /// none from this process. That is not a caveat to the law but the law's
    /// other half, and it is the half that matters most now that media is readied
    /// lazily: an installed synod hands `detect` a closure it must never call.
    /// Which half applies is read from the answer rather than asked in advance,
    /// for the reason its sibling test sets out — a broker can appear part-way
    /// through this binary's own run.
    #[cfg(windows)]
    #[test]
    fn without_media_detect_refuses_by_name() {
        match detect(None) {
            Ok(_) => assert!(
                broker::client::Brokered::available(),
                "nothing but a machine service can boot with no media from this process"
            ),
            Err(message) => assert!(message.contains("could not find"), "{message}"),
        }
    }

    /// With media, the answer follows what this computer will actually do:
    /// a machine whose user the compute service serves gets a backend, and one
    /// whose user it refuses gets the sentence naming the remedy. The law under
    /// test is agreement between the two, not either fixed answer — a developer
    /// box outside the Hyper-V Administrators group and a deployed one inside
    /// it must both be described correctly.
    ///
    /// A third answer is lawful and has to be allowed for: a machine service.
    /// One may be installed on this computer, and one is *started by another test
    /// in this binary* — `broker::tests::the_broker_answers_a_client_over_a_real_pipe`
    /// serves on the real pipe from a thread that outlives it, deliberately,
    /// because the pipe's name and its security descriptor are part of what that
    /// test exists to check. Tests run in parallel, so the service can appear
    /// between this test's question and its answer; asking beforehand would be a
    /// race. So the broker is checked *after* `detect` instead, on the one branch
    /// where it changes the verdict — a computer the compute service refuses can
    /// only have got a backend through a broker, and if it did, one must be
    /// reachable. Checking afterwards is sound because nothing in this binary
    /// ever stops serving: within one test process a broker only appears, never
    /// goes away, so an answer that was true when `detect` ran is still true when
    /// the assertion asks.
    #[cfg(windows)]
    #[test]
    fn with_media_detect_follows_what_this_computer_permits() {
        match (hcs::available(), detect(Some(media()))) {
            (Ok(()), answer) => assert!(answer.is_ok(), "a permitted computer must get a backend"),
            (Err(_), Ok(_)) => assert!(
                broker::client::Brokered::available(),
                "a computer the compute service refuses can only get a backend from a machine \
                 service, and no service is listening"
            ),
            (Err(why), Err(message)) => {
                assert!(
                    message.starts_with(&why),
                    "the refusal must carry the machine layer's own words: {message}"
                );
                assert!(
                    message.contains("installer"),
                    "and name the way out of it: {message}"
                );
            }
        }
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
            assert!(detect(Some(media())).is_ok());
        } else {
            let message = detect(Some(media())).err().unwrap();
            assert!(message.contains("entitlement"), "{message}");
        }
    }
}
