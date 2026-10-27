use std::path::{Path, PathBuf};

use guest_abi::{boot::Export, layout::WORKSPACE_NAME};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSFileHandle, NSString, NSURL};
use objc2_virtualization::{
    VZDiskImageStorageDeviceAttachment, VZFileHandleSerialPortAttachment, VZLinuxBootLoader,
    VZSharedDirectory, VZSingleDirectoryShare, VZStorageDeviceConfiguration,
    VZVirtioBlockDeviceConfiguration, VZVirtioConsoleDeviceSerialPortConfiguration,
    VZVirtioFileSystemDeviceConfiguration, VZVirtioSocketDeviceConfiguration,
    VZVirtualMachineConfiguration,
};

use crate::error::{Act, Error, HostFile};
use crate::lifecycle::SessionDisk;

pub(super) const SESSION_IMAGE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

pub(super) struct Plan {
    pub(super) kernel: PathBuf,
    pub(super) initramfs: PathBuf,
    pub(super) rootfs: PathBuf,
    pub(super) session: SessionDisk,
    pub(super) folder: PathBuf,
    pub(super) read_only: bool,
    pub(super) vcpus: u8,
    pub(super) memory_mib: u32,
    pub(super) epoch: u64,
}

/// # Errors
/// Returns a path, disk attachment or framework refusal.
pub(super) fn build_configuration(
    plan: &Plan,
) -> Result<Retained<VZVirtualMachineConfiguration>, Error> {
    let kernel = file_url(&plan.kernel)?;
    let initramfs = file_url(&plan.initramfs)?;
    let folder = file_url(&plan.folder)?;
    // SAFETY: constructors and setters use SDK argument types; Retained owns every referenced object.
    unsafe {
        let config = VZVirtualMachineConfiguration::new();

        let boot = VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &kernel);
        boot.setInitialRamdiskURL(Some(&initramfs));
        boot.setCommandLine(&NSString::from_str(&command_line(plan)));
        config.setBootLoader(Some(&boot));

        // The disk stack: read-only rootfs first, read-write session second.
        let rootfs = block_device(&plan.rootfs, true)?;
        let session = block_device(plan.session.path(), false)?;
        let storage: [&VZStorageDeviceConfiguration; 2] = [&**rootfs, &**session];
        config.setStorageDevices(&NSArray::from_slice(&storage));

        let tag = NSString::from_str(WORKSPACE_NAME);
        VZVirtioFileSystemDeviceConfiguration::validateTag_error(&tag).map_err(|error| {
            Error::backend(
                super::HYPERVISOR,
                format!("the workspace tag was refused: {}", ns_error(&error)),
            )
        })?;
        let sharing = VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &tag,
        );
        let shared = VZSharedDirectory::initWithURL_readOnly(
            VZSharedDirectory::alloc(),
            &folder,
            plan.read_only,
        );
        let share =
            VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &shared);
        sharing.setShare(Some(&share));
        config.setDirectorySharingDevices(&NSArray::from_slice(&[&**sharing]));

        let socket = VZVirtioSocketDeviceConfiguration::new();
        config.setSocketDevices(&NSArray::from_slice(&[&**socket]));
        // No network device: the socket is the only route out.

        let console = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        let attachment =
            VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                None,
                Some(&NSFileHandle::fileHandleWithStandardOutput()),
            );
        console.setAttachment(Some(&attachment));
        config.setSerialPorts(&NSArray::from_slice(&[&**console]));

        config.setCPUCount(clamp_cpus(plan.vcpus));
        config.setMemorySize(clamp_memory(plan.memory_mib));

        Ok(config)
    }
}

/// # Errors
/// Returns [`Error::Backend`] with the framework's validation refusal.
pub(super) fn validate(config: &VZVirtualMachineConfiguration) -> Result<(), Error> {
    // SAFETY: `validateWithError:` inspects the configuration and reports an
    // `NSError` or nothing; it starts no machine.
    unsafe { config.validateWithError() }
        .map_err(|error| Error::backend(super::HYPERVISOR, ns_error(&error)))
}

fn command_line(plan: &Plan) -> String {
    crate::spec::boot_line(
        Export::Virtiofs {
            tag: WORKSPACE_NAME.into(),
        },
        "hvc0",
        plan.epoch,
    )
}

/// # Errors
/// Returns [`Error::NotAFileUrl`] or [`Error::HostFile`] if the image cannot attach.
fn block_device(
    image: &Path,
    read_only: bool,
) -> Result<Retained<VZVirtioBlockDeviceConfiguration>, Error> {
    let url = file_url(image)?;
    // SAFETY: the URL is valid and the returned attachment or NSError is retained.
    unsafe {
        let attachment = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &url,
            read_only,
        )
        .map_err(|error| Error::HostFile {
            file: HostFile::DiskImage,
            act: Act::Open,
            path: image.to_path_buf(),
            cause: ns_error(&error),
        })?;
        Ok(VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &attachment,
        ))
    }
}

fn clamp_cpus(vcpus: u8) -> usize {
    // SAFETY: reads two class-level constants; no instance, no side effect.
    let (least, most) = unsafe {
        (
            VZVirtualMachineConfiguration::minimumAllowedCPUCount(),
            VZVirtualMachineConfiguration::maximumAllowedCPUCount(),
        )
    };
    usize::from(vcpus).clamp(least, most)
}

fn clamp_memory(memory_mib: u32) -> u64 {
    // SAFETY: reads two class-level constants; no instance, no side effect.
    let (least, most) = unsafe {
        (
            VZVirtualMachineConfiguration::minimumAllowedMemorySize(),
            VZVirtualMachineConfiguration::maximumAllowedMemorySize(),
        )
    };
    (u64::from(memory_mib) * 1024 * 1024).clamp(least, most)
}

/// # Errors
/// Returns [`Error::NotAFileUrl`] if `path` is empty or holds a NUL character.
fn file_url(path: &Path) -> Result<Retained<NSURL>, Error> {
    NSURL::from_file_path(path).ok_or_else(|| Error::NotAFileUrl(path.to_path_buf()))
}

pub(super) fn ns_error(error: &objc2_foundation::NSError) -> String {
    error.localizedDescription().to_string()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::spec::{MachineSpec, SIZE};

    struct Fixture {
        plan: Plan,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let make = |name: &str, bytes: u64| {
            let path = dir.path().join(name);
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(bytes).unwrap();
            path
        };
        let folder = dir.path().join("work");
        std::fs::create_dir(&folder).unwrap();
        let spec = MachineSpec {
            folder,
            read_only: false,
        };
        let plan = Plan {
            kernel: make("kernel", 4096),
            initramfs: make("initramfs", 4096),
            rootfs: make("rootfs.img", 64 * 1024 * 1024),
            session: SessionDisk::create(dir.path(), crate::lifecycle::Leaf::Raw, |disk| {
                disk.set_len(SESSION_IMAGE_BYTES)
            })
            .unwrap(),
            folder: spec.folder,
            read_only: spec.read_only,
            vcpus: SIZE.vcpus,
            memory_mib: SIZE.memory_mib,
            epoch: 1_771_200_000,
        };
        Fixture { _dir: dir, plan }
    }

    #[test]
    fn a_well_formed_plan_builds_and_is_refused_only_for_the_entitlement() {
        let fixture = fixture();
        let config = build_configuration(&fixture.plan).expect("a well-formed plan must build");
        match validate(&config) {
            Ok(()) => {}
            Err(Error::Backend { why, .. }) => assert!(
                why.contains("entitlement"),
                "a well-formed configuration must fail only for the entitlement, not: {why}"
            ),
            Err(other) => panic!("validation failed for an unexpected reason: {other:?}"),
        }
    }

    #[test]
    fn the_configuration_is_the_machine_the_design_describes() {
        let fixture = fixture();
        let config = build_configuration(&fixture.plan).unwrap();
        // SAFETY: reading back the properties just set, off any queue: the
        // configuration is not a running machine and has no queue affinity.
        unsafe {
            assert_eq!(config.storageDevices().count(), 2, "rootfs and session");
            assert_eq!(config.directorySharingDevices().count(), 1, "the share");
            assert_eq!(
                config.socketDevices().count(),
                1,
                "one socket device carries both the control plane and the net wire"
            );
            assert_eq!(config.serialPorts().count(), 1, "the console");
            assert_eq!(
                config.networkDevices().count(),
                0,
                "the guest has no way out but the socket"
            );
        }
    }

    #[test]
    fn resources_are_clamped_into_the_permitted_range() {
        let cpus = clamp_cpus(2);
        // SAFETY: class-level range reads.
        let (least_cpu, most_cpu) = unsafe {
            (
                VZVirtualMachineConfiguration::minimumAllowedCPUCount(),
                VZVirtualMachineConfiguration::maximumAllowedCPUCount(),
            )
        };
        assert!((least_cpu..=most_cpu).contains(&cpus));

        let memory = clamp_memory(2048);
        assert_eq!(memory % (1024 * 1024), 0, "memory is a whole-megabyte size");
    }

    #[test]
    fn a_missing_disk_image_is_refused_by_the_attachment() {
        let error = block_device(Path::new("/nowhere/at/all/rootfs.img"), true)
            .expect_err("a missing image cannot attach");
        assert!(
            matches!(
                error,
                Error::HostFile {
                    file: HostFile::DiskImage,
                    act: Act::Open,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn the_command_line_addresses_the_guest() {
        let fixture = fixture();
        let line = command_line(&fixture.plan);
        assert!(line.contains("console=hvc0"), "{line}");
        assert!(line.contains("ral.workspace=workspace"), "{line}");
        assert!(line.contains("ral.port=1729"), "{line}");
        assert!(line.contains("ral.epoch=1771200000"), "{line}");
        assert!(
            line.contains("ral.net=1730,10.0.2.15/24,10.0.2.2"),
            "{line}"
        );
    }
}
