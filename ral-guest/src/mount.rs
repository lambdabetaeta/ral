//! Mount parents before descendants; root assembly precedes this plan.

use std::ffi::CStr;

use rustix::fs::{Mode, mkdir};
use rustix::io::Errno;
use rustix::mount::{MountFlags, mount};

use guest_abi::boot::Boot;
use guest_abi::layout::{EXEC_CGROUP, EXEC_CONTROLLERS, SCRATCH, WORK};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Options {
    None,
    Fixed(&'static CStr),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Mount {
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
        }
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
        // virtiofs takes its tag as the source; there is no device.
        Mount {
            source: boot.workspace.clone(),
            target: WORK,
            fstype: "virtiofs",
            flags: scratch,
            data: Options::None,
        },
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
            workspace: "work".into(),
            port: 1729,
            epoch: 0,
            engine: guest_abi::boot::DEFAULT_ENGINE.into(),
            net: None,
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
    fn the_workspace_is_the_virtiofs_tag_mounted_at_work() {
        let workspace = plan(&boot())
            .into_iter()
            .find(|m| m.target == WORK)
            .expect("the plan mounts the workspace");
        assert_eq!(workspace.source, "work");
        assert_eq!(workspace.fstype, "virtiofs");
        assert_eq!(workspace.data, Options::None);
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
