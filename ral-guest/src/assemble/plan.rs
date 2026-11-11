//! Rootfs precedes session disk under both device models; probe names rather than the ISA.
//! The rootfs is read-only and the session disk supplies the overlay upper.

use rustix::mount::MountFlags;

use crate::mount::{Mount, Options};

pub(crate) const ROOTFS_MOUNT: &str = "/rootfs";
pub(crate) const SESSION_MOUNT: &str = "/session";

pub(crate) const NEWROOT: &str = "/newroot";

/// Upper and work must both live on the session disk.
pub(crate) const UPPER: &str = "/session/upper";
pub(crate) const WORKDIR: &str = "/session/work";

/// build-boot.sh derives the module manifest from the pinned kernel; no names are hardcoded.
pub(crate) const MODULE_DIR: &str = "/modules";
pub(crate) const MODULE_MANIFEST: &str = "/modules.order";

pub(crate) const MKE2FS: &str = "/sbin/mke2fs";
/// -F bypasses the formatter prompt on the unattended console.
pub(crate) const FORMAT_SESSION_ARGS: [&str; 4] = ["-q", "-F", "-t", "ext4"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Disks {
    pub(crate) model: &'static str,
    pub(crate) rootfs: &'static str,
    pub(crate) session: &'static str,
}

/// Probe device models, not architecture; virtio wins when both complete pairs exist.
pub(crate) const CANDIDATES: [Disks; 2] = [
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

/// Refuse a rootfs without its session disk; never skip that half-pair for another model.
pub(crate) fn resolve_disks(present: impl Fn(&str) -> bool) -> Result<Disks, String> {
    match CANDIDATES.into_iter().find(|disks| present(disks.rootfs)) {
        Some(disks) if present(disks.session) => Ok(disks),
        Some(disks) => Err(format!(
            "this machine has {} but no {}: the host attached the rootfs image and not the \
             session image, so there is nothing to write the session's overlay upper onto. \
             A ral guest is booted with exactly two {} disks.",
            disks.rootfs, disks.session, disks.model
        )),
        None => {
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
    }
}

pub(crate) fn disk_mounts(disks: Disks) -> [Mount; 2] {
    [
        Mount {
            source: disks.rootfs.into(),
            target: ROOTFS_MOUNT,
            fstype: "ext4",
            flags: MountFlags::RDONLY,
            data: Options::None,
        },
        Mount {
            source: disks.session.into(),
            target: SESSION_MOUNT,
            fstype: "ext4",
            flags: MountFlags::empty(),
            data: Options::None,
        },
    ]
}

/// Mount only after both disks; options name mount points, independent of device model.
pub(super) fn overlay() -> Mount {
    Mount {
        source: "overlay".into(),
        target: NEWROOT,
        fstype: "overlay",
        flags: MountFlags::empty(),
        data: Options::Fixed(c"lowerdir=/rootfs,upperdir=/session/upper,workdir=/session/work"),
    }
}

pub(crate) const ENGINE_EMBEDDED: &str = "/engine";

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_probe_prefers_virtio_when_every_device_is_present() {
        let disks = resolve_disks(|_| true).expect("a machine with every device has a pair");
        assert_eq!(disks.rootfs, "/dev/vda");
        assert_eq!(disks.session, "/dev/vdb");
    }

    #[test]
    fn a_machine_with_only_scsi_disks_boots_from_them() {
        let disks = resolve_disks(|device| device.starts_with("/dev/sd"))
            .expect("a SCSI machine has a pair");
        assert_eq!(disks.rootfs, "/dev/sda");
        assert_eq!(disks.session, "/dev/sdb");
        assert_eq!(disks.model, "SCSI");
    }

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
}
