//! One machine core for two agents.
//!
//! Synod (office work) and exarch (coding) both want the same thing from a
//! machine: *give the agent a folder to work in, and tell me honestly what
//! stands between that agent and the rest of this computer.*  This crate is
//! that, and nothing more — [`detect`] picks a backend, [`Hypervisor::boot`]
//! turns a [`MachineSpec`] into a [`Machine`], and [`Machine::boundary`]
//! answers the honesty question.
//!
//! # What this crate does, stated plainly
//!
//! `dev/docs/VM/SYNOD.md` §4 has the agent work in the granted folder
//! itself — shared into a guest under a hardware boundary, used in place
//! under [`Host`](host::Host) — with a checkpoint before the job and
//! conflict-checked undo after it.  That safety net is host-side product
//! code (`synod::workspace`), and none of this crate's business: a machine
//! here only *places* the folder and answers for the boundary around it.
//!
//! # The boundary is a question the caller may ask
//!
//! A machine is not automatically a wall.  [`Boundary`] says which kind you
//! got — [`Boundary::Hardware`] for a real virtual machine, [`Boundary::None`]
//! when the folder is used in place and the only enforcement left is ral's
//! capability gate plus whatever the operating system sandbox provides.  A
//! caller can therefore tell its user the truth instead of implying a wall
//! that is not there.  That is the entire point of the type; do not add a
//! variant that means "sort of".
//!
//! # Backends
//!
//! - [`host`] — no virtual machine, the granted folder used where it lies.
//!   This is how synod runs on a Linux development box with no hypervisor,
//!   and it is a real backend, not a stub.
//! - [`vz`] — Apple's Virtualization.framework, macOS only.  Real code: it
//!   builds, validates, and boots a configuration — but only from a signed
//!   binary carrying the virtualization entitlement and holding a real boot
//!   image, so [`detect`] does not return it yet.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod host;
#[cfg(target_os = "macos")]
pub mod vz;

/// The folder an agent is given, and where it appears to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    /// The granted folder on this computer.
    pub host_path: PathBuf,
    /// Where that folder appears inside a guest — `/work`, by convention.
    /// Ignored by backends whose boundary is [`Boundary::None`]: they have
    /// no inside, so the folder keeps its own path.
    pub guest_path: PathBuf,
    /// Ask that the agent may read the folder but not change it.
    ///
    /// A hardware boundary enforces this at the mount.  Under
    /// [`Boundary::None`] there is no mount to enforce it with, so the
    /// request travels on to ral's capability gate; the machine reports
    /// [`Boundary::None`] precisely so the caller knows that enforcement is
    /// now its own job.
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
                  through `crate::path::canon` and no turn to raise a card in; vm-manager \
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

/// What stands between the agent and the rest of the computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Boundary {
    /// A hardware virtual machine.  The guest can reach the granted folder
    /// and the host's control socket, and nothing else — not because it is
    /// forbidden to, but because nothing else is there.
    Hardware,
    /// No machine boundary at all: the folder is worked in where it lies.
    /// What the agent may touch is decided entirely by ral's capability
    /// gate and by the operating system's own sandbox.
    None,
}

impl Boundary {
    /// Whether a virtual machine, rather than software policy, is doing the
    /// containing.
    pub fn is_hardware(self) -> bool {
        matches!(self, Self::Hardware)
    }
}

impl fmt::Display for Boundary {
    /// One sentence, fit to show the person who granted the folder.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hardware => f.write_str(
                "a separate virtual machine: the agent can reach the folder you granted and nothing else on this computer",
            ),
            Self::None => f.write_str(
                "no separate machine: the agent works in the folder itself, and the only limits are the ones ral enforces in software",
            ),
        }
    }
}

/// Something that can start a machine.
pub trait Hypervisor {
    /// The backend's name, fit to show a user: `"this computer"`,
    /// `"Virtualization.framework"`.
    fn name(&self) -> &'static str;

    /// The boundary every machine from this backend will have — askable
    /// before booting, so a caller can warn or refuse first.
    fn boundary(&self) -> Boundary;

    /// Start a machine for `spec`.
    ///
    /// # Errors
    /// Returns [`Error`] if the spec is not usable (see
    /// [`MachineSpec::resolve`]) or if the backend cannot start.
    fn boot(&self, spec: &MachineSpec) -> Result<Box<dyn Machine>, Error>;
}

/// A running machine holding one workspace.
pub trait Machine {
    /// Where the engine should work — the guest path under a real
    /// hypervisor, the host folder itself under [`Host`](host::Host).
    fn workspace_path(&self) -> &Path;

    /// What stands between this machine's agent and the rest of the
    /// computer.  Ask before promising a user anything.
    fn boundary(&self) -> Boundary;

    /// The host end of this machine's control-plane connection — the stream
    /// the frame protocol of `dev/docs/VM/SYNOD.md` §3 runs over.
    ///
    /// A hardware machine has exactly one, accepted when the guest's daemon
    /// dialed it at boot; taking it transfers ownership away from the
    /// machine, so it answers `Some` at most once.  The answer is `None`
    /// under [`Boundary::None`]: there is no wire, because there is no
    /// inside — the engine shares the host's address space, and whatever
    /// would have travelled the frame protocol is a direct call instead.
    ///
    /// Unix only: a control plane here is an `AF_VSOCK` stream, named by its
    /// file descriptor.  The Windows Hyper-V backend will choose its own
    /// socket type once it exists; this signature does not presume it.
    #[cfg(unix)]
    fn take_control(&mut self) -> Option<std::os::fd::OwnedFd>;

    /// Stop the machine and release what it holds.
    ///
    /// Dropping a machine also stops it; this exists so a caller that wants
    /// to *report* a failed shutdown can have one.
    ///
    /// # Errors
    /// Returns [`Error`] if the machine did not stop cleanly.
    fn shutdown(self: Box<Self>) -> Result<(), Error>;
}

/// The best backend this computer can actually run today.
///
/// "Best" means the strongest boundary that is *implemented and working
/// here*, not the strongest one the platform could offer: on Linux that is
/// [`host::Host`], and on macOS it is still [`host::Host`] — [`vz`] is real
/// code, but it starts machines only from a signed, entitled binary holding
/// a real boot image, and until that wiring exists the crate hands callers
/// the backend that works everywhere.  Ask the result for its
/// [`Hypervisor::boundary`] rather than assuming from the platform.
pub fn detect() -> Box<dyn Hypervisor> {
    Box::new(host::Host)
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
