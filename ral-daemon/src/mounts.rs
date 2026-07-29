//! The guest's filesystems, named as data before any of them is mounted.
//!
//! Which filesystems a guest needs, in which order, with which flags, is a
//! *decision*; mounting one is a syscall.  This module keeps the two apart:
//! [`plan`] is a pure function from the [`Boot`] configuration to a list of
//! [`Mount`]s, and [`Mount::apply`] is the thin edge that performs one.  The
//! plan can therefore be read, reviewed, and unit-tested on any machine;
//! only its performance needs a virtual one.
//!
//! Ordering is not decoration.  A mount point lives inside some filesystem,
//! so a plan is correct only if every filesystem is mounted before anything
//! that lies inside it — `/dev` before `/dev/pts`, `/sys` before
//! `/sys/fs/cgroup`.  That invariant is a property of the list, and [`plan`]
//! is tested for it.
//!
//! ## What is *not* here: the root filesystem
//!
//! The guest's root is composed from a read-only rootfs image (the lower
//! layer) and the per-session disk (the upper layer).  That composition is
//! deliberately absent from this daemon.  Assembling the root is, by
//! construction, the work of the stage that runs *before* a final root exists
//! — the initramfs, which mounts the two images, stacks the overlay, and
//! `switch_root`s into it.  A PID 1 that did it itself would have to
//! change root from under its own feet, carrying its `/proc`, its open
//! descriptors, and the path to the engine binary across the move: precisely
//! the sort of cleverness an init must not contain.
//!
//! So the overlay is a *precondition* here rather than a step — and one the
//! daemon checks: [`root_filesystem`] reads `/proc/mounts` and says what the
//! kernel actually handed us, so a mis-assembled boot artifact is named on
//! the host's log at second zero instead of surfacing as a baffling `EROFS`
//! in the middle of a run.
//!
//! ## The workspace, whose shape the hypervisor decides
//!
//! Everything above is the same under either hypervisor backend; the granted
//! folder is not.  It arrives either as a virtiofs export the guest mounts by
//! tag, or — where there is no virtiofs — as a 9p share the host serves on a
//! vsock port, and [`crate::boot::Export`] is how the guest learns which.
//! Both end as one mount at [`WORK`], with the same flags, so the difference
//! is confined to the mount's *options*: that is what [`Options`] is for.
//!
//! The 9p form is the one place where a mount cannot be fully written down
//! in advance, because its options must name a file descriptor that does not
//! exist until something dials the host.  [`plan`] therefore stays pure and
//! records the *intent* to dial ([`Options::Plan9Dial`]); the dial itself
//! happens in [`Mount::apply`], with the rest of the syscalls.

use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, AsRawFd, RawFd};

use rustix::fs::{Mode, mkdir};
use rustix::io::Errno;
use rustix::mount::{MountFlags, mount};

use crate::boot::{Boot, Export};
use crate::vsock;

/// Where the granted folder appears inside the guest.  The engine's working
/// directory, and the only path in the guest whose contents outlive the VM.
pub const WORK: &str = "/work";

/// The filesystem type of a virtiofs export.  The mount *source* is not a
/// device but a tag the host and guest agree on; see [`Export::Virtiofs`].
const VIRTIOFS: &str = "virtiofs";

/// The filesystem type of a 9p share.  One type name serves every 9p
/// dialect; which dialect is spoken is an option, `version=9p2000.L`.
const PLAN9: &str = "9p";

/// The largest 9p message the guest and the host's server will exchange, and
/// the size the transport socket's own buffers are set to.
///
/// 64 KiB is what Microsoft's LCOW guest agent uses for exactly this mount
/// (`microsoft/hcsshim`, `internal/guest/storage/plan9`), and the number has
/// to appear in two places at once — `msize` in the mount options and
/// `SO_RCVBUF`/`SO_SNDBUF` on the socket — which is precisely why it is a
/// constant rather than two literals.
const MSIZE: u32 = 65536;

/// A mount's filesystem-specific options: the third of `mount(2)`'s
/// arguments that is not a path.
///
/// A bare string would do for every filesystem but one, which is why this is
/// a sum.  The 9p share's options name a file descriptor, so they cannot be
/// written before the boot reaches the point of dialling the host — and yet
/// *which* share, and on which port, is known from the command line and
/// belongs in the plan.  Splitting the two lets [`plan`] stay a pure
/// function of [`Boot`] while [`Mount::apply`] keeps every syscall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Options {
    /// The filesystem wants none.
    None,
    /// Fixed options, fully known before the boot begins.
    Fixed(&'static CStr),
    /// A 9p share whose options cannot be written until the transport socket
    /// exists: the option string names the descriptor.
    Plan9Dial {
        /// The share's name, which 9p carries as `aname`.
        name: String,
        /// The host-side `AF_VSOCK` port its server answers on.
        port: u32,
    },
}

/// One filesystem the guest needs, as data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// The `mount(2)` source: a device, or — for a virtiofs export or a 9p
    /// share — the name the host published, since neither has a device at
    /// all.  Pseudo-filesystems conventionally repeat their own type here,
    /// and whatever stands here is what shows up in `/proc/mounts`.
    pub source: String,
    /// The absolute path the filesystem appears at.
    pub target: &'static str,
    /// The `mount(2)` filesystem type.
    pub fstype: &'static str,
    /// The flags the mount is made with.
    pub flags: MountFlags,
    /// The filesystem's own options, in `mount(8)`'s comma-separated
    /// spelling — or, for a 9p share, the intent to go and get a transport
    /// whose descriptor number those options must carry.
    pub data: Options,
}

impl Mount {
    /// Perform this mount, creating its mount point if the rootfs does not
    /// already carry one.
    ///
    /// [`Options::Plan9Dial`] is resolved here, and only here: the host is
    /// dialled, the socket's buffers are sized to [`MSIZE`], and the option
    /// string is written around the descriptor that came back.
    ///
    /// # Errors
    /// Returns a sentence naming the mount that failed and the reason the
    /// kernel gave — or, for a 9p share whose server could not be reached,
    /// the port that went unanswered.
    pub fn apply(&self) -> Result<(), String> {
        // A rootfs image carries most of these directories, but a freshly
        // mounted `devtmpfs` carries no `pts` or `shm`, so the mount point
        // is made here rather than assumed.  An existing one is the normal
        // case, not a failure.
        if let Err(err) = mkdir(self.target, Mode::from_raw_mode(0o755))
            && err != Errno::EXIST
        {
            return Err(format!(
                "could not create the mount point {}: {err}",
                self.target
            ));
        }
        match &self.data {
            Options::None => self.mount_with(None),
            Options::Fixed(options) => self.mount_with(Some(options)),
            Options::Plan9Dial { name, port } => self.mount_plan9(name, *port),
        }
    }

    /// Mount a 9p share over a fresh connection to the host's server.
    ///
    /// The transport is the socket: 9p's `trans=fd` reads and writes an
    /// already-open file, so the guest dials the port the host named and
    /// hands the kernel the descriptor number rather than a device path.
    /// This is the arrangement Microsoft's LCOW guest agent uses for the same
    /// job (`microsoft/hcsshim`, `internal/guest/storage/plan9`).
    fn mount_plan9(&self, name: &str, port: u32) -> Result<(), String> {
        let socket = vsock::dial_host(port).map_err(|err| {
            format!(
                "could not reach the host's 9p server for the granted folder on vsock port \
                 {port}: {err}. The host serves the share `{name}` there — is `ral.plan9` the \
                 port the session manager published? (`ral.port` is the control plane, a \
                 different one.)"
            )
        })?;
        vsock::set_buffer_sizes(socket.as_fd(), MSIZE).map_err(|err| {
            format!(
                "could not size the 9p transport socket's buffers to {MSIZE} bytes, the mount's \
                 own msize: {err}"
            )
        })?;
        let options = plan9_options(socket.as_raw_fd(), name);
        let options = CString::new(options).map_err(|err| {
            format!("the 9p mount options for `{name}` are not a C string: {err}")
        })?;
        self.mount_with(Some(&options))?;
        // Not a leak: a successful `mount(2)` leaves the kernel's 9p
        // `trans=fd` transport holding its own reference to the open file
        // (`fget`), so the transport outlives this descriptor and the guest's
        // copy has done its work the moment the mount succeeded.
        drop(socket);
        Ok(())
    }

    /// The `mount(2)` itself, once the options are a string the kernel can
    /// read.
    fn mount_with(&self, data: Option<&CStr>) -> Result<(), String> {
        mount(
            self.source.as_str(),
            self.target,
            self.fstype,
            self.flags,
            data,
        )
        .map_err(|err| {
            format!(
                "could not mount {} ({}) at {}: {err}",
                self.source, self.fstype, self.target
            )
        })
    }
}

/// The mount data a 9p `trans=fd` mount takes, for the share `name` reached
/// over the connected socket sitting on descriptor `fd`.
///
/// Both descriptor numbers are deliberately the same socket: `rfdno` is what
/// the transport reads and `wfdno` what it writes, and a stream socket is
/// both ends at once.  `version=9p2000.L` is the Linux dialect — the only one
/// with the semantics a working userland needs — and `aname` is the share the
/// server should attach us to, which is the host's own word for the granted
/// folder ([`crate::boot::Export::Plan9`]).
///
/// Pure, so the spelling the kernel will be handed is testable without a
/// hypervisor; it is the one part of the 9p mount that is a decision rather
/// than a syscall.
fn plan9_options(fd: RawFd, name: &str) -> String {
    format!("trans=fd,rfdno={fd},wfdno={fd},msize={MSIZE},version=9p2000.L,aname={name}")
}

/// The one mount that must precede every decision.
///
/// The boot configuration is read from `/proc/cmdline`, so `/proc` cannot be
/// part of a plan that is itself computed from that configuration.  It is
/// the daemon's first act, and it is the only mount that needs no argument.
pub fn procfs() -> Mount {
    Mount {
        source: "proc".into(),
        target: "/proc",
        fstype: "proc",
        flags: MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        data: Options::None,
    }
}

/// Everything else the guest needs, in dependency order.
///
/// The pseudo-filesystems come first — they are what makes the guest a
/// working Linux userland at all — and the workspace last, because it is the
/// only one whose identity depends on the host's grant, and the only one
/// whose *shape* depends on which hypervisor the host is running.
///
/// The flags follow one rule: deny what the filesystem has no business
/// carrying.  `nosuid` everywhere, since nothing in the guest legitimately
/// gains privilege from a setuid bit; `nodev` everywhere except the two
/// filesystems whose entire purpose is device nodes, `/dev` and the
/// pseudo-terminals under it; `noexec` on the kernel's own interfaces, which
/// hold no programs.  `/tmp` and `/work` stay executable on purpose: office
/// and build tooling writes a script and runs it, and a guest that forbade
/// that would be lying about being a Linux userland.  Confining what a
/// *spawned* process may do is the engine's jail, not a mount flag.
pub fn plan(boot: &Boot) -> Vec<Mount> {
    let kernel = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
    let scratch = MountFlags::NOSUID | MountFlags::NODEV;
    vec![
        Mount {
            source: "sysfs".into(),
            target: "/sys",
            fstype: "sysfs",
            flags: kernel,
            data: Options::None,
        },
        // The kernel populates this itself, which is why the guest needs no
        // device manager at all.
        Mount {
            source: "devtmpfs".into(),
            target: "/dev",
            fstype: "devtmpfs",
            flags: MountFlags::NOSUID,
            data: Options::Fixed(c"mode=0755"),
        },
        // gid 5 is `tty` in the Debian/Ubuntu userland the rootfs is built
        // from; `ptmxmode` is what makes `/dev/ptmx` usable without a helper.
        Mount {
            source: "devpts".into(),
            target: "/dev/pts",
            fstype: "devpts",
            flags: MountFlags::NOSUID | MountFlags::NOEXEC,
            data: Options::Fixed(c"mode=0620,gid=5,ptmxmode=0666"),
        },
        Mount {
            source: "tmpfs".into(),
            target: "/dev/shm",
            fstype: "tmpfs",
            flags: scratch,
            data: Options::Fixed(c"mode=1777"),
        },
        Mount {
            source: "tmpfs".into(),
            target: "/run",
            fstype: "tmpfs",
            flags: scratch,
            data: Options::Fixed(c"mode=0755"),
        },
        Mount {
            source: "tmpfs".into(),
            target: "/tmp",
            fstype: "tmpfs",
            flags: scratch,
            data: Options::Fixed(c"mode=1777"),
        },
        // The unified hierarchy only; `nsdelegate` is what lets the engine
        // hand a subtree to a spawned command without handing it the root.
        Mount {
            source: "cgroup2".into(),
            target: "/sys/fs/cgroup",
            fstype: "cgroup2",
            flags: kernel,
            data: Options::Fixed(c"nsdelegate"),
        },
        workspace(&boot.workspace, scratch),
    ]
}

/// The granted folder's mount, in whichever of the two shapes the host chose.
///
/// The two differ in exactly three fields and agree on the rest, which is the
/// claim worth making here: the same target, the same flags, and so the same
/// guest — `nosuid,nodev` but *executable*, because office and build tooling
/// writes a script and runs it, and a workspace that forbade that would be
/// lying about being a Linux userland.
fn workspace(export: &Export, flags: MountFlags) -> Mount {
    match export {
        Export::Virtiofs { tag } => Mount {
            source: tag.clone(),
            target: WORK,
            fstype: VIRTIOFS,
            flags,
            data: Options::None,
        },
        // The source is the share's name for the same reason the virtiofs
        // one is a tag: there is no device to name, and a word in
        // `/proc/mounts` that matches what the host published is the only
        // way a reader of that table can tell where `/work` came from.
        Export::Plan9 { name, port } => Mount {
            source: name.clone(),
            target: WORK,
            fstype: PLAN9,
            flags,
            data: Options::Plan9Dial {
                name: name.clone(),
                port: *port,
            },
        },
    }
}

/// What the kernel handed the daemon as `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    /// The filesystem type, as `/proc/mounts` spells it.
    pub fstype: String,
    /// Whether the root was mounted read-write.
    pub writable: bool,
}

impl Root {
    /// True when the root is the writable overlay the boot stage was
    /// supposed to assemble: a read-only rootfs image under a per-session
    /// upper layer.
    pub fn is_session_overlay(&self) -> bool {
        self.fstype == "overlay" && self.writable
    }
}

/// Read the root filesystem's identity out of the contents of
/// `/proc/mounts`.
///
/// Pure, so the check that names a mis-assembled boot artifact can be tested
/// without one.  Returns `None` when the table carries no entry for `/` at
/// all — a table this daemon cannot interpret, which is worth saying rather
/// than guessing about.
pub fn root_filesystem(mounts: &str) -> Option<Root> {
    mounts.lines().find_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        let (_source, target, fstype, options) = (
            fields.next()?,
            fields.next()?,
            fields.next()?,
            fields.next()?,
        );
        (target == "/").then(|| Root {
            fstype: fstype.to_string(),
            // The kernel always writes `rw` or `ro` first in the option list.
            writable: options.split(',').next() == Some("rw"),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot() -> Boot {
        Boot {
            workspace: Export::Virtiofs { tag: "work".into() },
            port: 1729,
            epoch: 0,
            engine: crate::boot::DEFAULT_ENGINE.into(),
            net: None,
        }
    }

    /// The same boot, with the granted folder arriving the way a Hyper-V
    /// host must serve it.
    fn boot_over_plan9() -> Boot {
        Boot {
            workspace: Export::Plan9 {
                name: "work".into(),
                port: 50001,
            },
            ..boot()
        }
    }

    /// The plan's central invariant: a mount point lives inside some
    /// filesystem, so nothing may be mounted inside a filesystem that is
    /// not up yet.
    #[test]
    fn a_filesystem_is_mounted_before_anything_inside_it() {
        let targets: Vec<_> = std::iter::once(procfs().target)
            .chain(plan(&boot()).into_iter().map(|m| m.target))
            .collect();
        for (position, inner) in targets.iter().enumerate() {
            for outer in &targets[position + 1..] {
                assert!(
                    !inner
                        .strip_prefix(outer)
                        .is_some_and(|rest| rest.starts_with('/')),
                    "{inner} is mounted before {outer}, the filesystem it lies inside"
                );
            }
        }
    }

    /// `/proc` is the daemon's first act and belongs to no plan: a plan is
    /// computed *from* the configuration `/proc/cmdline` carries.
    #[test]
    fn the_plan_does_not_repeat_procfs() {
        assert!(plan(&boot()).iter().all(|m| m.target != procfs().target));
    }

    /// No filesystem is mounted twice.
    #[test]
    fn no_target_is_mounted_twice() {
        let plan = plan(&boot());
        let mut targets: Vec<_> = plan.iter().map(|m| m.target).collect();
        targets.sort_unstable();
        let mounted = targets.len();
        targets.dedup();
        assert_eq!(targets.len(), mounted, "a target appears twice in the plan");
    }

    /// Nothing in the guest gains privilege from a setuid bit, and only the
    /// filesystems that exist to hold device nodes may hold them.
    #[test]
    fn only_the_device_filesystems_may_carry_device_nodes() {
        assert!(
            plan(&boot())
                .iter()
                .all(|m| m.flags.contains(MountFlags::NOSUID)),
            "a filesystem is mounted without nosuid"
        );
        let with_devices: Vec<_> = plan(&boot())
            .into_iter()
            .filter(|m| !m.flags.contains(MountFlags::NODEV))
            .map(|m| m.target)
            .collect();
        assert_eq!(with_devices, vec!["/dev", "/dev/pts"]);
    }

    /// The kernel's own interfaces hold no programs; the places real work
    /// happens do.
    #[test]
    fn only_the_kernels_interfaces_are_noexec() {
        let executable: Vec<_> = plan(&boot())
            .into_iter()
            .filter(|m| !m.flags.contains(MountFlags::NOEXEC))
            .map(|m| m.target)
            .collect();
        assert_eq!(executable, vec!["/dev", "/dev/shm", "/run", "/tmp", WORK]);
    }

    /// The workspace is the host's grant: its source is the tag the command
    /// line carried, and nothing else in the plan is session-specific.
    #[test]
    fn the_workspace_is_mounted_from_its_virtiofs_tag() {
        let mut config = boot();
        config.workspace = Export::Virtiofs {
            tag: "granted-folder".into(),
        };
        let work = plan(&config)
            .into_iter()
            .find(|m| m.target == WORK)
            .expect("the plan mounts the workspace");
        assert_eq!(work.source, "granted-folder");
        assert_eq!(work.fstype, VIRTIOFS);
        assert_eq!(work.data, Options::None);
    }

    /// Where the host serves the granted folder over 9p, the plan carries
    /// both halves of what the dial will need — the share's name and the
    /// port — and carries them as an intent, because a pure function cannot
    /// open a socket.
    #[test]
    fn a_nine_p_workspace_is_planned_as_a_dial_naming_the_share_and_the_port() {
        let work = plan(&boot_over_plan9())
            .into_iter()
            .find(|m| m.target == WORK)
            .expect("the plan mounts the workspace");
        assert_eq!(work.fstype, PLAN9);
        assert_eq!(
            work.source, "work",
            "`/proc/mounts` should name the share the host published"
        );
        assert_eq!(
            work.data,
            Options::Plan9Dial {
                name: "work".into(),
                port: 50001,
            }
        );
    }

    /// The two arrangements differ in the mount's identity and in nothing
    /// else: the guest gets the same `/work`, with the same flags, either
    /// way.
    #[test]
    fn the_workspace_is_mounted_the_same_way_under_either_hypervisor() {
        let of = |config: &Boot| {
            plan(config)
                .into_iter()
                .find(|m| m.target == WORK)
                .expect("the plan mounts the workspace")
        };
        let (virtiofs, plan9) = (of(&boot()), of(&boot_over_plan9()));
        assert_eq!(virtiofs.target, plan9.target);
        assert_eq!(virtiofs.flags, plan9.flags);
        assert_eq!(
            plan(&boot()).len(),
            plan(&boot_over_plan9()).len(),
            "the shape of the export changes one mount, not the plan"
        );
    }

    /// The exact string the kernel is handed. Both descriptor numbers name
    /// the one connected socket, `msize` matches the buffers the socket was
    /// given, and `aname` is the host's own word for the granted folder.
    #[test]
    fn the_nine_p_options_name_the_one_socket_twice() {
        assert_eq!(
            plan9_options(7, "granted-folder"),
            "trans=fd,rfdno=7,wfdno=7,msize=65536,version=9p2000.L,aname=granted-folder"
        );
        assert_eq!(
            MSIZE, 65536,
            "the option string above spells msize out; the two must agree"
        );
    }

    /// The root the boot stage was supposed to assemble.
    #[test]
    fn a_session_overlay_root_is_recognised() {
        let table = "overlay / overlay rw,relatime,lowerdir=/lower,upperdir=/upper 0 0\n\
                     proc /proc proc rw,nosuid,nodev,noexec 0 0\n";
        let root = root_filesystem(table).expect("the table names a root");
        assert!(root.is_session_overlay());
        assert_eq!(root.fstype, "overlay");
    }

    /// A bare read-only rootfs is not one, and the difference is exactly
    /// what the boot-time report exists to name.
    #[test]
    fn a_bare_read_only_root_is_not_a_session_overlay() {
        let root = root_filesystem("/dev/vda / ext4 ro,relatime 0 0\n").expect("a root");
        assert!(!root.is_session_overlay());
        assert!(!root.writable);
        assert_eq!(root.fstype, "ext4");
    }

    /// A writable root that is not an overlay is still the wrong
    /// arrangement: rootfs writes would land on the image, not the session.
    #[test]
    fn a_writable_non_overlay_root_is_not_a_session_overlay() {
        let root = root_filesystem("/dev/vda / ext4 rw,relatime 0 0\n").expect("a root");
        assert!(root.writable);
        assert!(!root.is_session_overlay());
    }

    /// A table with no entry for `/` is not interpreted into a guess.
    #[test]
    fn a_table_without_a_root_yields_nothing() {
        assert_eq!(root_filesystem("proc /proc proc rw 0 0\n"), None);
        assert_eq!(root_filesystem(""), None);
    }
}
