//! The initramfs's job, named as data before any of it is done.
//!
//! Nothing here is a decision made at boot: which disks, in which order,
//! with which options, and which binaries land where, are all fixed by the
//! host side of the contract before this program ever runs — the *order* in
//! which the host attaches the two disks (`vm-manager/src/vz.rs` attaches
//! them as virtio-blk devices, rootfs first; the Windows backend attaches
//! the same two images as LUN 0 and LUN 1 of one SCSI controller) and
//! `ral-daemon/src/boot.rs`'s `DEFAULT_ENGINE`. So it is all read here as
//! plain data, exactly as `ral-daemon/src/mounts.rs::plan` reads the guest's
//! own mount table.
//!
//! The one thing the host's contract fixes only *up to* the hypervisor is
//! what the disks are called. Two device models present the same two disks
//! under two sets of names, so the pair is probed rather than assumed — see
//! [`CANDIDATES`] and [`resolve_disks`] — and everything downstream of the
//! probe is again plain data.

use std::ffi::CStr;

use rustix::mount::MountFlags;

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

/// The two block devices one hypervisor's device model presents the session's
/// disks as: the read-only rootfs image the host attached first, and the
/// read-write session image it attached second.
///
/// The session disk arrives as a bare zero-filled sparse file
/// (`vm-manager`'s `create_session_image`) and is formatted here on every
/// boot; the rootfs image arrives complete and is never written to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Disks {
    /// The device model these names belong to, for the sentence a failed
    /// probe has to print.
    pub model: &'static str,
    /// The read-only rootfs image's device.
    pub rootfs: &'static str,
    /// The per-session read-write disk's device.
    pub session: &'static str,
}

/// Every pair of device names a ral guest's disks may arrive under, in the
/// order they are tried.
///
/// One initramfs boots under both hypervisors of `dev/docs/VM/SYNOD.md` §2,
/// and the axis they differ on is the *device model*, not the ISA: a
/// Virtualization.framework machine attaches the two images as virtio-blk
/// devices (`/dev/vda`, `/dev/vdb`), a Hyper-V machine as LUN 0 and LUN 1 of
/// a SCSI controller (`/dev/sda`, `/dev/sdb`). Switching on the architecture
/// at compile time would encode the wrong distinction and would still be a
/// guess; asking the kernel what is actually there is neither.
///
/// The order is a *preference*, not an exclusion: it says which pair wins if
/// a machine somehow presents both, and nothing more.  virtio comes first
/// because it is the better transport, and because a Hyper-V guest has no
/// virtio disks to be confused by.
pub const CANDIDATES: [Disks; 2] = [
    Disks {
        model: "virtio-blk",
        rootfs: "/dev/vda",
        session: "/dev/vdb",
    },
    Disks {
        model: "SCSI",
        rootfs: "/dev/sda",
        session: "/dev/sdb",
    },
];

/// Find the pair of disks this machine actually has, given a way to ask
/// whether a device node is there.
///
/// The probe is only meaningful at one point in the boot: after `devtmpfs` is
/// mounted ([`DEV`]) and after the modules are loaded, since it is the block
/// driver that creates the node and `devtmpfs` that shows it. Run earlier it
/// would find nothing and refuse a machine that is perfectly well assembled.
///
/// A pair whose rootfs device exists is the machine's pair, and its session
/// device must exist too — one disk of a pair is a mis-assembled machine, not
/// a reason to keep looking, so it is refused by name rather than silently
/// skipped in favour of a device model this host does not have.
///
/// Pure in `present`, so the preference order is testable without a
/// hypervisor; `init.rs` passes the real `stat`.
///
/// # Errors
/// Returns a sentence naming every candidate device when the machine presents
/// none of them, or naming the missing half when it presents only one.
pub fn resolve_disks(present: impl Fn(&str) -> bool) -> Result<Disks, String> {
    for disks in CANDIDATES {
        if !present(disks.rootfs) {
            continue;
        }
        if !present(disks.session) {
            return Err(format!(
                "this machine has {} but no {}: the host attached the rootfs image and not the \
                 session image, so there is nothing to write the session's overlay upper onto. \
                 A ral guest is booted with exactly two {} disks.",
                disks.rootfs, disks.session, disks.model
            ));
        }
        return Ok(disks);
    }
    let looked_for = CANDIDATES
        .iter()
        .map(|disks| format!("{} and {} ({})", disks.rootfs, disks.session, disks.model))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "this machine presents none of the disks a ral guest boots from: looked for {looked_for}. \
         Either the host attached no disks, or the initramfs is missing the block driver for the \
         controller it did attach them to."
    ))
}

/// The two disks, in the order they must be mounted: each stands alone, so
/// only their own emptiness constrains the order, and rootfs first matches
/// the order the host attached them in.
///
/// A function rather than a constant because the device names are what
/// [`resolve_disks`] found; everything else about the two mounts is fixed.
pub fn disk_mounts(disks: Disks) -> [Mount; 2] {
    [
        Mount {
            source: disks.rootfs,
            target: ROOTFS_MOUNT,
            fstype: "ext4",
            flags: MountFlags::RDONLY,
            data: None,
        },
        Mount {
            source: disks.session,
            target: SESSION_MOUNT,
            fstype: "ext4",
            flags: MountFlags::empty(),
            data: None,
        },
    ]
}

/// The overlay root itself: `dev/docs/VM/SYNOD.md` §7's composition, RO
/// rootfs lower plus RW session upper.
///
/// Mounted only once both disks above are up, which is why it is not part of
/// [`disk_mounts`]. It names the two *mount points*, never the devices, so
/// which device model the machine turned out to have cannot reach this far.
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
    /// overlay's writable upper.  True of whichever pair the probe returns:
    /// the device model changes the names, never the flags.
    #[test]
    fn only_the_rootfs_disk_is_mounted_read_only() {
        for disks in CANDIDATES {
            let ro: Vec<_> = disk_mounts(disks)
                .into_iter()
                .filter(|m| m.flags.contains(MountFlags::RDONLY))
                .map(|m| m.target)
                .collect();
            assert_eq!(ro, vec![ROOTFS_MOUNT], "{} disks", disks.model);
        }
    }

    /// The two disks are mounted where the overlay expects to find them, and
    /// from the devices the probe chose — a pair whose mounts named some
    /// other device would assemble a root out of the wrong session.
    #[test]
    fn the_disk_mounts_carry_the_resolved_devices_to_the_overlays_mount_points() {
        for disks in CANDIDATES {
            let mounts = disk_mounts(disks);
            assert_eq!(mounts[0].source, disks.rootfs);
            assert_eq!(mounts[0].target, ROOTFS_MOUNT);
            assert_eq!(mounts[1].source, disks.session);
            assert_eq!(mounts[1].target, SESSION_MOUNT);
        }
    }

    /// Every candidate is a pair of distinct absolute device paths: the two
    /// disks are two disks, under every device model.
    #[test]
    fn every_candidate_names_two_distinct_devices() {
        for disks in CANDIDATES {
            assert!(disks.rootfs.starts_with("/dev/"), "{}", disks.rootfs);
            assert!(disks.session.starts_with("/dev/"), "{}", disks.session);
            assert_ne!(disks.rootfs, disks.session, "{} disks", disks.model);
        }
    }

    /// The order in [`CANDIDATES`] is a preference: a machine that somehow
    /// presented both device models would be booted from virtio, the better
    /// transport, rather than from whichever the kernel happened to name
    /// first.
    #[test]
    fn the_probe_prefers_virtio_when_every_device_is_present() {
        let disks = resolve_disks(|_| true).expect("a machine with every device has a pair");
        assert_eq!(disks.rootfs, "/dev/vda");
        assert_eq!(disks.session, "/dev/vdb");
    }

    /// A Hyper-V guest has no virtio disks at all, and the probe falls
    /// through to the SCSI controller's LUNs rather than refusing.
    #[test]
    fn a_machine_with_only_scsi_disks_boots_from_them() {
        let disks = resolve_disks(|device| device.starts_with("/dev/sd"))
            .expect("a SCSI machine has a pair");
        assert_eq!(disks.rootfs, "/dev/sda");
        assert_eq!(disks.session, "/dev/sdb");
        assert_eq!(disks.model, "SCSI");
    }

    /// With no disk of any candidate present there is nothing to boot from,
    /// and the refusal names every device that was looked for — the reader is
    /// a maintainer staring at a console, and the list is the whole diagnosis.
    #[test]
    fn a_machine_with_no_candidate_disks_is_refused_by_name() {
        let err = resolve_disks(|_| false).expect_err("no disks, no boot");
        for disks in CANDIDATES {
            assert!(err.contains(disks.rootfs), "{err}");
            assert!(err.contains(disks.session), "{err}");
        }
    }

    /// Half a pair is a mis-assembled machine, not a hint to try the next
    /// device model: a rootfs with no session disk has nowhere to put the
    /// overlay's upper, and saying so beats a second, more confusing failure
    /// further down.
    #[test]
    fn a_machine_with_only_the_rootfs_disk_is_refused_rather_than_skipped() {
        let err = resolve_disks(|device| device == "/dev/vda")
            .expect_err("one disk of a pair is not a machine");
        assert!(err.contains("/dev/vda"), "{err}");
        assert!(err.contains("/dev/vdb"), "{err}");
        assert!(
            !err.contains("/dev/sda"),
            "the refusal is about the pair that answered, not the one that did not: {err}"
        );
    }

    /// The overlay names both disks' mount points, and nothing else: a
    /// third source here would be a filesystem [`disk_mounts`] never
    /// mounted.
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
