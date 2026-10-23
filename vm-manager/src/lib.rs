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

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::site::{Silent, site};

#[cfg(windows)]
pub mod broker;
mod error;
#[cfg(windows)]
pub mod hcs;
pub mod media;
mod os_path;
mod site;
#[cfg(target_os = "macos")]
pub mod vz;

pub use error::{Act, Error, HostFile, MachineFiles, Spelling, Wire, Worded};

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
        let guest_path = self.workspace.guest_path.as_os_str();
        if !guest_path.as_encoded_bytes().starts_with(b"/") {
            return Err(Error::GuestPathNotAbsolute(
                self.workspace.guest_path.clone(),
            ));
        }

        let path = &self.workspace.host_path;
        let root =
            site!(Silent::GrantedFolderResolve, std::fs::canonicalize(path)).map_err(|cause| {
                match cause.kind() {
                    io::ErrorKind::NotFound => Error::NoSuchFolder(path.clone()),
                    _ => Error::FolderUnreadable {
                        path: path.clone(),
                        cause: cause.to_string(),
                    },
                }
            })?;
        let is_folder = site!(Silent::GrantedFolderStat, &root => is_dir());
        if is_folder {
            Ok(root)
        } else {
            Err(Error::NotAFolder(root))
        }
    }
}

/// The zstd window the shipped rootfs archive is written with, and so the
/// largest a decoder of it must be willing to allocate.
///
/// `vm-image/build.sh` compresses with `--long=27`, and a decoder that will
/// not allocate 128 MiB cannot read the result at all.  `ruzstd` defaults to
/// a 100 MiB ceiling — a sound guard against a hostile stream naming a window
/// far larger than it needs, and one that this archive, written by our own
/// pipeline, sits just above.  So the ceiling is raised to exactly what the
/// pipeline writes and no further: still a bound, but one that admits the
/// only archive it is ever pointed at.
pub const ROOTFS_WINDOW: u64 = 1 << 27;

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
    pub fn resolve(&self) -> Result<Self, Error> {
        let resolve_one = |path: &PathBuf| {
            site!(Silent::BootMediaResolve, std::fs::canonicalize(path)).map_err(|cause| {
                Error::MissingBootFile {
                    path: path.clone(),
                    cause: cause.to_string(),
                }
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
/// [`ral_core::protocol::channel::WireStream`](../../core/src/protocol/channel.rs), whose docs explain
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
/// [`ral_core::protocol::channel::WireStream`](../../core/src/protocol/channel.rs) adopts either
/// kind the same way.
#[cfg(unix)]
pub type AgentDial = std::os::fd::OwnedFd;

/// See the Unix twin above for the whole contract, which this shares but
/// for the handle's type.
#[cfg(windows)]
pub type AgentDial = std::os::windows::io::OwnedSocket;

/// What a guest said on its console, so far as the host can still reach it.
///
/// This is the only diagnostic channel a host has into a guest that will not
/// talk to it.  `ral-daemon` gives the engine the daemon's own standard
/// descriptors, which the kernel has connected to `/dev/console`, so anything
/// the engine writes outside the protocol — a panic, a refusal, a failed
/// mount — lands on the guest's console beside the daemon's own lines and
/// nowhere else.  When the protocol then closes with no explanation, this is
/// the explanation.
///
/// Both halves may be empty, and an empty one is a finding rather than a
/// failure: no `log` means this backend keeps nothing on disk for a host to
/// open, and an empty `tail` means the guest genuinely said nothing.  Callers
/// must say which of those they saw rather than writing an empty file and
/// letting a reader assume the guest was silent.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GuestConsole {
    /// The whole console on disk, if this backend kept it somewhere the host
    /// can name.  Naming it is worth doing even when it cannot be read from
    /// here — under the broker it belongs to a `LocalSystem` service — since
    /// a path an administrator can open beats no path at all.
    #[serde(with = "crate::os_path::option")]
    pub log: Option<PathBuf>,
    /// The guest's last lines, oldest first, as a boot failure already quotes
    /// them.  A handful, not a dump: the point is to be quotable inside a
    /// failure a person is already reading.
    pub tail: Vec<String>,
}

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

    /// What the guest has said on its console — see [`GuestConsole`] for why
    /// this is the one thing worth asking a machine whose engine has gone.
    ///
    /// Defaulted to nothing, because "nothing" is the honest answer for a
    /// backend that hands its guest's console straight to this process's
    /// standard output and keeps no copy: there is no handle to read it back
    /// from, and pretending otherwise would have a caller write an empty file
    /// that reads as a silent guest.  A backend that *does* keep a copy
    /// overrides this.
    fn console(&self) -> GuestConsole {
        GuestConsole::default()
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
pub type BootMedia = Box<dyn FnOnce() -> Result<BootArtifact, Error>>;

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
/// Always, with [`Error::NoBackend`].
#[cfg(not(any(windows, all(target_os = "macos", target_arch = "aarch64"))))]
pub fn detect(boot: Option<BootMedia>) -> Result<Box<dyn Hypervisor>, Error> {
    drop(boot);
    Err(Error::NoBackend)
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
/// Returns [`Error::NoBootMedia`] if there is no service and `boot` is `None`,
/// [`hcs::available`]'s own refusal if this Windows will not host a guest for
/// this user directly either, or whatever `boot` itself reports when the media
/// cannot be readied.
#[cfg(windows)]
pub fn detect(boot: Option<BootMedia>) -> Result<Box<dyn Hypervisor>, Error> {
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
        return Err(Error::NoBootMedia);
    };
    hcs::available()?;
    Ok(Box::new(hcs::Hyperv::new(
        media()?,
        hcs::Hyperv::default_cache(),
    )))
}

/// Start the one backend this build can actually run, given the boot media
/// the application ships.
///
/// On Apple Silicon macOS the platform check the other-platform overload
/// makes always passes, so the only refusals left are
/// [`Error::NoBootMedia`] and [`Error::Unentitled`].
///
/// # Errors
/// Returns [`Error::NoBootMedia`] if `boot` is `None`, [`Error::Unentitled`]
/// if this process is not signed with the virtualization entitlement, or
/// whatever `boot` itself reports when the media cannot be readied.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn detect(boot: Option<BootMedia>) -> Result<Box<dyn Hypervisor>, Error> {
    let Some(media) = boot else {
        return Err(Error::NoBootMedia);
    };
    if !vz::entitled() {
        return Err(Error::Unentitled);
    }
    Ok(Box::new(vz::Vz::new(
        media()?,
        site!(Silent::SessionDiskFolder, std::env::temp_dir()),
    )))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
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
        assert!(matches!(detect(None), Err(Error::NoBackend)));
        assert!(matches!(detect(Some(media())), Err(Error::NoBackend)));
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
            Err(refusal) => assert!(matches!(refusal, Error::NoBootMedia), "{refusal:?}"),
        }
    }

    /// With media, the answer follows what this computer will actually do:
    /// a machine whose user the compute service serves gets a backend, and one
    /// whose user it refuses gets the compute service's own refusal. The law under
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

    /// On the eligible platform, no media is its own refusal.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn without_media_detect_refuses_by_name() {
        let refusal = detect(None).err().unwrap();
        assert!(matches!(refusal, Error::NoBootMedia), "{refusal:?}");
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
            let refusal = detect(Some(media())).err().unwrap();
            assert!(matches!(refusal, Error::Unentitled), "{refusal:?}");
        }
    }
}
