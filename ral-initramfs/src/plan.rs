//! The initramfs's job, named as data before any of it is done.
//!
//! Nothing here is a decision made at boot: which disks, in which order,
//! with which options, and which binaries land where, are all fixed by the
//! host side of the contract (`vm-manager/src/vz.rs`'s disk attachment
//! order, `ral-daemon/src/boot.rs`'s `DEFAULT_ENGINE`) before this program
//! ever runs. So it is all read here as plain data, exactly as
//! `ral-daemon/src/mounts.rs::plan` reads the guest's own mount table.

use std::ffi::CStr;

use rustix::mount::MountFlags;

/// The read-only rootfs image's device. `vz.rs` attaches the two virtio-blk
/// disks in a fixed order — rootfs first — so this is a property of the
/// contract, not a guess.
pub const ROOTFS_DEVICE: &str = "/dev/vda";

/// The per-session read-write disk's device: the second virtio-blk disk,
/// created by the host as a bare zero-filled sparse file
/// (`vm-manager`'s `create_session_image`) and formatted here on every boot.
pub const SESSION_DEVICE: &str = "/dev/vdb";

/// Where the two disks are mounted before the overlay is assembled from
/// them.
pub const ROOTFS_MOUNT: &str = "/rootfs";
pub const SESSION_MOUNT: &str = "/session";

/// Where the assembled root is mounted, ready for `switch_root`.
pub const NEWROOT: &str = "/newroot";

/// The overlay's upper and work directories, both on the formatted session
/// disk so every guest write outlives nothing but the session.
pub const UPPER: &str = "/session/upper";
pub const WORKDIR: &str = "/session/work";

/// Where the kernel modules this initramfs carries live, and the file
/// naming which of them to load, and in what order.
///
/// Which modules a given kernel build needs — built from the real
/// `CONFIG_*` a kernel package ships, not assumed — is a build-time fact,
/// not a compile-time one: `build-boot.sh` writes this manifest from the
/// real dependency chain `depmod` computes, so this program never hardcodes
/// a module name that could drift out from under a kernel bump.
pub const MODULE_DIR: &str = "/modules";
pub const MODULE_MANIFEST: &str = "/modules.order";

/// The vendored formatter this initramfs carries, and its arguments.
///
/// `-F` passes past mke2fs's "are you sure" prompt, which a device with no
/// prior filesystem signature would otherwise raise on a console nobody is
/// reading interactively.
pub const MKE2FS: &str = "/sbin/mke2fs";
pub const FORMAT_SESSION_ARGS: [&str; 4] = ["-q", "-F", "-t", "ext4"];

/// One filesystem this initramfs mounts, as data.
///
/// Mirrors `ral-daemon::mounts::Mount`, minus the parts that daemon needs
/// and this program does not: there is no [`Boot`](ral_daemon::boot::Boot)
/// yet, so nothing here is session-specific.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mount {
    pub source: &'static str,
    pub target: &'static str,
    pub fstype: &'static str,
    pub flags: MountFlags,
    pub data: Option<&'static CStr>,
}

/// The kernel's device nodes, mounted before either disk can be opened.
///
/// Whether a kernel automounts devtmpfs over an initramfs varies by version
/// and config; mounting devtmpfs over an existing automount is harmless, so
/// `/dev` is claimed here rather than assumed.
pub const DEV: Mount = Mount {
    source: "devtmpfs",
    target: "/dev",
    fstype: "devtmpfs",
    flags: MountFlags::NOSUID,
    data: Some(c"mode=0755"),
};

/// The two disks, in the order they must be mounted: each stands alone, so
/// only their own emptiness constrains the order, and rootfs first matches
/// the device order they arrive in.
pub const DISKS: [Mount; 2] = [
    Mount {
        source: ROOTFS_DEVICE,
        target: ROOTFS_MOUNT,
        fstype: "ext4",
        flags: MountFlags::RDONLY,
        data: None,
    },
    Mount {
        source: SESSION_DEVICE,
        target: SESSION_MOUNT,
        fstype: "ext4",
        flags: MountFlags::empty(),
        data: None,
    },
];

/// The overlay root itself — `dev/docs/VM/SYNOD.md` §7's composition, RO
/// rootfs lower plus RW session upper — mounted only once both disks above
/// are up, which is why it is not part of [`DISKS`].
pub const OVERLAY: Mount = Mount {
    source: "overlay",
    target: NEWROOT,
    fstype: "overlay",
    flags: MountFlags::empty(),
    data: Some(c"lowerdir=/rootfs,upperdir=/session/upper,workdir=/session/work"),
};

/// One binary this initramfs carries and installs onto the writable
/// overlay upper before `switch_root`.
///
/// This is the mechanism behind `boot.img`'s claim to carry "kernel +
/// ral-daemon + ral engine as one artifact": `rootfs.img` holds neither, so
/// both are installed fresh into the session overlay on every boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Install {
    /// Where this binary is embedded in the initramfs cpio.
    pub embedded: &'static str,
    /// The absolute path inside the mounted overlay it is installed at.
    pub installed: &'static str,
}

/// The path `switch_root` execs after handoff.
///
/// The engine is installed at the one path `ral-daemon`'s own
/// [`DEFAULT_ENGINE`](ral_daemon::boot::DEFAULT_ENGINE) names, so a boot
/// artifact that installs it anywhere else silently breaks a contract this
/// table exists to keep in one place.
pub const RAL_DAEMON: &str = "/sbin/ral-daemon";

pub const INSTALLS: [Install; 2] = [
    Install {
        embedded: "/ral-daemon",
        installed: RAL_DAEMON,
    },
    Install {
        embedded: "/engine",
        installed: ral_daemon::boot::DEFAULT_ENGINE,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The rootfs disk is read-only; the session disk is not — it is the
    /// overlay's writable upper.
    #[test]
    fn only_the_rootfs_disk_is_mounted_read_only() {
        let ro: Vec<_> = DISKS
            .into_iter()
            .filter(|m| m.flags.contains(MountFlags::RDONLY))
            .map(|m| m.target)
            .collect();
        assert_eq!(ro, vec![ROOTFS_MOUNT]);
    }

    /// The overlay names both disks' mount points, and nothing else: a
    /// third source here would be a filesystem [`DISKS`] never mounted.
    #[test]
    fn the_overlay_is_built_from_exactly_the_two_mounted_disks() {
        let data = OVERLAY.data.expect("the overlay carries options");
        let options = data.to_str().expect("overlay options are ASCII");
        assert_eq!(
            options,
            format!("lowerdir={ROOTFS_MOUNT},upperdir={UPPER},workdir={WORKDIR}")
        );
    }

    /// The module manifest and the directory it names are both absolute:
    /// `/init` reads them before any relative path could mean anything.
    #[test]
    fn the_module_manifest_and_its_directory_are_absolute() {
        assert!(MODULE_DIR.starts_with('/'));
        assert!(MODULE_MANIFEST.starts_with('/'));
    }

    /// The session disk is formatted before it is mounted, so the format
    /// step must never claim to be conditional: `-F` forces past the one
    /// prompt an unconditional format would otherwise raise.
    #[test]
    fn the_format_step_never_asks_a_question() {
        assert!(FORMAT_SESSION_ARGS.contains(&"-F"));
    }

    /// Two binaries are installed, and each lands at a distinct, absolute
    /// path — installing over the same path twice would silently drop one.
    #[test]
    fn each_installed_binary_has_its_own_absolute_path() {
        let paths: Vec<_> = INSTALLS.into_iter().map(|i| i.installed).collect();
        assert!(paths.iter().all(|p| p.starts_with('/')));
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), paths.len(), "an install path is reused");
    }

    /// `ral-daemon` is installed at the exact path `switch_root` will exec,
    /// and the engine at `ral-daemon`'s own default — one source of truth
    /// for each.
    #[test]
    fn ral_daemon_and_the_engine_land_at_their_contracted_paths() {
        assert_eq!(INSTALLS[0].installed, RAL_DAEMON);
        assert_eq!(INSTALLS[1].installed, ral_daemon::boot::DEFAULT_ENGINE);
    }
}
