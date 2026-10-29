//! Mount parents before descendants; root assembly precedes this plan.
//! Workspace transport varies only through the mount options, resolved at application.

use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, AsRawFd, RawFd};

use rustix::fs::{Mode, mkdir};
use rustix::io::Errno;
use rustix::mount::{MountFlags, mount};

use crate::vsock;
use guest_abi::boot::{Boot, Export};
use guest_abi::layout::{EXEC_CGROUP, EXEC_CONTROLLERS, SCRATCH, WORK};

const VIRTIOFS: &str = "virtiofs";

const PLAN9: &str = "9p";

/// The 9p msize and both socket buffer requests must agree.
const MSIZE: u32 = 65536;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Options {
    None,
    Fixed(&'static CStr),
    /// The options name a descriptor that exists only after dialing.
    Plan9Dial {
        port: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Mount {
    /// Export names also identify the share in /proc/mounts; neither transport has a device.
    pub(crate) source: String,
    pub(crate) target: &'static str,
    pub(crate) fstype: &'static str,
    pub(crate) flags: MountFlags,
    pub(crate) data: Options,
}

impl Mount {
    pub(crate) fn apply(&self) -> Result<(), String> {
        ensure_dir(self.target)?;
        match &self.data {
            Options::None => self.mount_with(None),
            Options::Fixed(options) => self.mount_with(Some(options)),
            Options::Plan9Dial { port } => self.mount_plan9(*port),
        }
    }

    fn mount_plan9(&self, port: u32) -> Result<(), String> {
        let name = &self.source;
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
        // The kernel's 9p trans=fd transport holds its own file reference after mount succeeds.
        drop(socket);
        Ok(())
    }

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

pub(crate) fn ensure_dir(path: &str) -> Result<(), String> {
    match mkdir(path, Mode::from_raw_mode(0o755)) {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(err) => Err(format!("could not create {path}: {err}")),
    }
}

pub(crate) fn dev() -> Mount {
    Mount {
        source: "devtmpfs".into(),
        target: "/dev",
        fstype: "devtmpfs",
        flags: MountFlags::NOSUID,
        data: Options::Fixed(c"mode=0755"),
    }
}

/// rfdno and wfdno name the same connected socket; aname names the granted share.
fn plan9_options(fd: RawFd, name: &str) -> String {
    format!("trans=fd,rfdno={fd},wfdno={fd},msize={MSIZE},version=9p2000.L,aname={name}")
}

/// /proc must precede `Boot::read`, which reads /proc/cmdline.
pub(crate) fn procfs() -> Mount {
    Mount {
        source: "proc".into(),
        target: "/proc",
        fstype: "proc",
        flags: MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        data: Options::None,
    }
}

/// Mount parents first; /tmp and /work stay executable for office and build tooling.
pub(crate) fn plan(boot: &Boot) -> Vec<Mount> {
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
        dev(),
        // gid 5 is tty in the rootfs; ptmxmode permits /dev/ptmx without a helper.
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
            target: SCRATCH,
            fstype: "tmpfs",
            flags: scratch,
            data: Options::Fixed(c"mode=1777"),
        },
        // nsdelegate lets the engine hand a command a subtree without handing it the root.
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

/// Delegate the same controllers the engine requires; failure refuses uncapped execution.
pub(crate) fn exec_cgroup() -> Result<(), String> {
    std::fs::write("/sys/fs/cgroup/cgroup.subtree_control", EXEC_CONTROLLERS).map_err(|err| {
        format!(
            "could not delegate {EXEC_CONTROLLERS} from the cgroup2 root to {EXEC_CGROUP}: {err}"
        )
    })?;
    ensure_dir(EXEC_CGROUP)
}

pub(crate) fn scratch_temp() -> Result<(), String> {
    let path = format!("{SCRATCH}/tmp");
    let mode = Mode::from_raw_mode(0o1777);
    mkdir(path.as_str(), mode).map_err(|err| {
        format!("could not create the guest scratch's temp directory {path}: {err}")
    })?;
    rustix::fs::chmod(path.as_str(), mode).map_err(|err| {
        format!("could not make {path} writable for guest commands with sticky mode 1777: {err}")
    })
}

fn workspace(export: &Export, flags: MountFlags) -> Mount {
    match export {
        Export::Virtiofs { tag } => Mount {
            source: tag.clone(),
            target: WORK,
            fstype: VIRTIOFS,
            flags,
            data: Options::None,
        },
        Export::Plan9 { name, port } => Mount {
            source: name.clone(),
            target: WORK,
            fstype: PLAN9,
            flags,
            data: Options::Plan9Dial { port: *port },
        },
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Root {
    pub(crate) fstype: String,
    pub(crate) writable: bool,
}

impl Root {
    pub(crate) fn is_session_overlay(&self) -> bool {
        self.fstype == "overlay" && self.writable
    }
}

pub(crate) fn root_filesystem(mounts: &str) -> Option<Root> {
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
            engine: guest_abi::boot::DEFAULT_ENGINE.into(),
            net: None,
        }
    }

    fn boot_over_plan9() -> Boot {
        Boot {
            workspace: Export::Plan9 {
                name: "work".into(),
                port: 50001,
            },
            ..boot()
        }
    }

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

    #[test]
    fn the_plan_does_not_repeat_procfs() {
        assert!(plan(&boot()).iter().all(|m| m.target != procfs().target));
    }

    #[test]
    fn no_target_is_mounted_twice() {
        let plan = plan(&boot());
        let mut targets: Vec<_> = plan.iter().map(|m| m.target).collect();
        targets.sort_unstable();
        let mounted = targets.len();
        targets.dedup();
        assert_eq!(targets.len(), mounted, "a target appears twice in the plan");
    }

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

    #[test]
    fn only_the_kernels_interfaces_are_noexec() {
        let executable: Vec<_> = plan(&boot())
            .into_iter()
            .filter(|m| !m.flags.contains(MountFlags::NOEXEC))
            .map(|m| m.target)
            .collect();
        assert_eq!(executable, vec!["/dev", "/dev/shm", "/run", "/tmp", WORK]);
    }

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
        assert_eq!(work.data, Options::Plan9Dial { port: 50001 });
    }

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

    #[test]
    fn a_session_overlay_root_is_recognised() {
        let table = "overlay / overlay rw,relatime,lowerdir=/lower,upperdir=/upper 0 0\n\
                     proc /proc proc rw,nosuid,nodev,noexec 0 0\n";
        let root = root_filesystem(table).expect("the table names a root");
        assert!(root.is_session_overlay());
        assert_eq!(root.fstype, "overlay");
    }

    #[test]
    fn a_bare_read_only_root_is_not_a_session_overlay() {
        let root = root_filesystem("/dev/vda / ext4 ro,relatime 0 0\n").expect("a root");
        assert!(!root.is_session_overlay());
        assert!(!root.writable);
        assert_eq!(root.fstype, "ext4");
    }

    #[test]
    fn a_writable_non_overlay_root_is_not_a_session_overlay() {
        let root = root_filesystem("/dev/vda / ext4 rw,relatime 0 0\n").expect("a root");
        assert!(root.writable);
        assert!(!root.is_session_overlay());
    }

    #[test]
    fn a_table_without_a_root_yields_nothing() {
        assert_eq!(root_filesystem("proc /proc proc rw 0 0\n"), None);
        assert_eq!(root_filesystem(""), None);
    }
}
