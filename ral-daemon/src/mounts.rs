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
//! §7 of the design record composes the guest's root from a read-only rootfs
//! image (the lower layer) and the per-session disk (the upper layer).  That
//! composition is deliberately absent from this daemon.  Assembling the root
//! is, by construction, the work of the stage that runs *before* a final root
//! exists — the initramfs, which mounts the two images, stacks the overlay,
//! and `switch_root`s into it.  A PID 1 that did it itself would have to
//! change root from under its own feet, carrying its `/proc`, its open
//! descriptors, and the path to the engine binary across the move: precisely
//! the sort of cleverness an init must not contain.
//!
//! So the overlay is a *precondition* here rather than a step — and one the
//! daemon checks: [`root_filesystem`] reads `/proc/mounts` and says what the
//! kernel actually handed us, so a mis-assembled boot artifact is named on
//! the host's log at second zero instead of surfacing as a baffling `EROFS`
//! in the middle of a turn.

use std::ffi::CStr;

use rustix::fs::{Mode, mkdir};
use rustix::io::Errno;
use rustix::mount::{MountFlags, mount};

use crate::boot::Boot;

/// Where the granted folder appears inside the guest.  The engine's working
/// directory, and the only path in the guest whose contents outlive the VM.
pub const WORK: &str = "/work";

/// The filesystem type of a virtiofs export.  The mount *source* is not a
/// device but a tag the host and guest agree on; see [`Boot::workspace`].
const VIRTIOFS: &str = "virtiofs";

/// One filesystem the guest needs, as data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// The `mount(2)` source: a device, or — for a virtiofs export — the
    /// tag the host published.  Pseudo-filesystems conventionally repeat
    /// their own type here, and that is what shows up in `/proc/mounts`.
    pub source: String,
    /// The absolute path the filesystem appears at.
    pub target: &'static str,
    /// The `mount(2)` filesystem type.
    pub fstype: &'static str,
    /// The flags the mount is made with.
    pub flags: MountFlags,
    /// The filesystem's own options, in `mount(8)`'s comma-separated
    /// spelling, or `None` when it wants none.
    pub data: Option<&'static CStr>,
}

impl Mount {
    /// Perform this mount, creating its mount point if the rootfs does not
    /// already carry one.
    ///
    /// # Errors
    /// Returns a sentence naming the mount that failed and the reason the
    /// kernel gave.
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
        mount(
            self.source.as_str(),
            self.target,
            self.fstype,
            self.flags,
            self.data,
        )
        .map_err(|err| {
            format!(
                "could not mount {} ({}) at {}: {err}",
                self.source, self.fstype, self.target
            )
        })
    }
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
        data: None,
    }
}

/// Everything else the guest needs, in dependency order.
///
/// The pseudo-filesystems come first — they are what makes the guest a
/// working Linux userland at all — and the workspace last, because it is the
/// only one whose identity depends on the host's grant.
///
/// The flags follow one rule: deny what the filesystem has no business
/// carrying.  `nosuid` everywhere, since nothing in the guest legitimately
/// gains privilege from a setuid bit; `nodev` everywhere except the two
/// filesystems whose entire purpose is device nodes, `/dev` and the
/// pseudo-terminals under it; `noexec` on the kernel's own interfaces, which
/// hold no programs.  `/tmp` and `/work` stay executable on purpose: office
/// and build tooling writes a script and runs it, and a guest that forbade
/// that would be lying about being a Linux userland.  Confining what a
/// *spawned* process may do is the engine's jail (§5), not a mount flag.
pub fn plan(boot: &Boot) -> Vec<Mount> {
    let kernel = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
    let scratch = MountFlags::NOSUID | MountFlags::NODEV;
    vec![
        Mount {
            source: "sysfs".into(),
            target: "/sys",
            fstype: "sysfs",
            flags: kernel,
            data: None,
        },
        // The kernel populates this itself, which is why the guest needs no
        // device manager at all.
        Mount {
            source: "devtmpfs".into(),
            target: "/dev",
            fstype: "devtmpfs",
            flags: MountFlags::NOSUID,
            data: Some(c"mode=0755"),
        },
        // gid 5 is `tty` in the Debian/Ubuntu userland the rootfs is built
        // from; `ptmxmode` is what makes `/dev/ptmx` usable without a helper.
        Mount {
            source: "devpts".into(),
            target: "/dev/pts",
            fstype: "devpts",
            flags: MountFlags::NOSUID | MountFlags::NOEXEC,
            data: Some(c"mode=0620,gid=5,ptmxmode=0666"),
        },
        Mount {
            source: "tmpfs".into(),
            target: "/dev/shm",
            fstype: "tmpfs",
            flags: scratch,
            data: Some(c"mode=1777"),
        },
        Mount {
            source: "tmpfs".into(),
            target: "/run",
            fstype: "tmpfs",
            flags: scratch,
            data: Some(c"mode=0755"),
        },
        Mount {
            source: "tmpfs".into(),
            target: "/tmp",
            fstype: "tmpfs",
            flags: scratch,
            data: Some(c"mode=1777"),
        },
        // The unified hierarchy only; `nsdelegate` is what lets the engine
        // hand a subtree to a spawned command without handing it the root.
        Mount {
            source: "cgroup2".into(),
            target: "/sys/fs/cgroup",
            fstype: "cgroup2",
            flags: kernel,
            data: Some(c"nsdelegate"),
        },
        Mount {
            source: boot.workspace.clone(),
            target: WORK,
            fstype: VIRTIOFS,
            flags: scratch,
            data: None,
        },
    ]
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
    /// supposed to assemble (§7): a read-only rootfs image under a
    /// per-session upper layer.
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
            workspace: "work".into(),
            port: 1729,
            epoch: 0,
            engine: crate::boot::DEFAULT_ENGINE.into(),
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
        config.workspace = "granted-folder".into();
        let work = plan(&config)
            .into_iter()
            .find(|m| m.target == WORK)
            .expect("the plan mounts the workspace");
        assert_eq!(work.source, "granted-folder");
        assert_eq!(work.fstype, VIRTIOFS);
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

    /// A writable root that is not an overlay is still not the arrangement
    /// §7 asks for: rootfs writes would land on the image, not the session.
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
