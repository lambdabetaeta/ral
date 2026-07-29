//! The machine, as a document.
//!
//! HCS takes no builder objects: a compute system is described by one JSON
//! document handed to `HcsCreateComputeSystem`, so this module *is* the
//! spec→hypervisor mapping, the way `vm-manager/src/vz.rs`'s `build_configuration` is on
//! macOS.  Being data rather than a sequence of setter calls, it can be built
//! and read back in a unit test on any Windows, with no VM and no privilege —
//! which is what the tests at the bottom do.
//!
//! # What the document says, and why each part is there
//!
//! - **`Chipset.LinuxKernelDirect`** — direct kernel boot, the same shape
//!   `VZLinuxBootLoader` gives on macOS: a kernel, an initial ramdisk, and a
//!   command line.  No UEFI, no bootloader, no boot disk to keep in step.
//! - **`ComputeTopology`** — the processors and memory the grant asked for.
//!   `AllowOvercommit` lets the host back the guest's memory lazily, so a
//!   4 GiB session does not take 4 GiB of the user's RAM to sit idle.
//! - **`Devices.Scsi`** — the two disks, in the order the guest's initramfs
//!   expects: the read-only rootfs at LUN 0, the read-write session disk at
//!   LUN 1.  Hyper-V has no virtio, so these arrive as SCSI devices
//!   (`/dev/sda`, `/dev/sdb`) rather than `/dev/vda`, `/dev/vdb`, and both
//!   must be VHDs — [`super::vhd`] is why.
//! - **`Devices.Plan9`** — the granted folder.  Hyper-V has no virtiofs, so the
//!   folder is exported as a 9p2000.L share that the *host* serves on a vsock
//!   port; the guest dials that port and mounts it at `/work`.  This is the
//!   mechanism WSL uses for `/mnt/c` and Microsoft's own Linux containers use
//!   for every host path, so it is neither exotic nor synod's invention.
//! - **`Devices.HvSocket`** — the control plane and the net wire, the guest's
//!   only two ways out of the machine.  There is **no network adapter at
//!   all**: not a disabled one, not a filtered one — absent, so there is
//!   nothing to misconfigure.  The net wire is a second `HvSocketService`
//!   entry in the same table, dialled by the guest exactly as the control
//!   plane is, and carries only IPv4 frames to the host's own user-mode
//!   TCP/IP stack — no NIC, no ARP, no broadcast domain.
//! - **`Devices.ComPorts`** — the guest console on a named pipe, which
//!   [`super::console`] reads onto synod's own output.  This is where a kernel
//!   panic or a daemon's refusal becomes visible; without it a failed boot is
//!   just a timeout.
//! - **`ShouldTerminateOnLastHandleClosed`** — if synod dies, the guest dies.
//!   The one line that makes a crashed host safe rather than merely tidy.

use std::collections::BTreeMap;
use std::path::Path;

use ral_daemon::boot;
use serde::Serialize;

/// The schema version this document is written against.
///
/// 2.1 is what Microsoft's own Linux-container host asks for, including for
/// `LinuxKernelDirect` boots, so it is the version with the most traffic over
/// it rather than the newest one available.
const SCHEMA: Version = Version { major: 2, minor: 1 };

/// The vsock port the host's 9p server listens on for the workspace share.
///
/// 564 is the registered `plan9` port and the number Microsoft's container host
/// uses for the same job; the guest is told it on the kernel command line
/// (`ral.plan9`) rather than compiling it in, so this constant is one side of a
/// contract and not a shared secret.
pub(super) const PLAN9_PORT: u32 = 564;

/// The share name the folder is exported under — the `aname` the guest mounts
/// by, and the value of `ral.workspace` on the kernel command line.
///
/// It plays exactly the part `WORKSPACE_TAG` plays for virtiofs on macOS: a
/// word the host and guest agree on, never a path.
pub(super) const WORKSPACE_SHARE: &str = "workspace";

/// 9p share flags, as the compute service defines them.
///
/// `LINUX_METADATA` is what makes the share carry uid/gid/mode/symlink
/// information instead of presenting everything as the sharing user's own
/// files — without it a Linux guest cannot keep the permissions of what it
/// writes.  `READ_ONLY` is how a read-only grant becomes the *mount's* law
/// rather than a promise the guest is trusted to keep.
mod plan9_flags {
    pub(super) const READ_ONLY: i32 = 0x0000_0001;
    pub(super) const LINUX_METADATA: i32 = 0x0000_0004;
}

/// Everything the host has decided before a document can be written.
///
/// Assembled by [`Hypervisor::boot`](crate::Hypervisor::boot) from the [`MachineSpec`](crate::MachineSpec),
/// the [`BootArtifact`](crate::BootArtifact), and the per-session files it made;
/// kept as one struct so [`document`] is a pure function of it and the tests
/// can write one by hand.
pub(super) struct Plan<'a> {
    pub(super) kernel: &'a Path,
    pub(super) initramfs: &'a Path,
    /// The read-only rootfs, wrapped as a VHD.
    pub(super) rootfs: &'a Path,
    /// The per-session read-write disk, freshly made and formatted by the guest.
    pub(super) session: &'a Path,
    /// The granted folder on this computer, resolved absolute.
    pub(super) folder: &'a Path,
    pub(super) read_only: bool,
    pub(super) vcpus: u32,
    pub(super) memory_mib: u32,
    /// The host's `AF_HYPERV` control-plane port, as the guest's vsock port.
    pub(super) control_port: u32,
    /// The host's `AF_HYPERV` net-wire port, as the guest's vsock port.
    pub(super) net_port: u32,
    /// The host's wall clock at boot, for the guest to adopt.
    pub(super) epoch: u64,
    /// The named pipe the guest's console is wired to.
    pub(super) console_pipe: &'a str,
    /// Who may bind and connect this machine's sockets, in SDDL.
    pub(super) socket_sddl: &'a str,
    /// The service GUID the control port maps to, spelled as HCS wants it.
    pub(super) control_service: &'a str,
    /// The service GUID the net port maps to, spelled as HCS wants it.
    pub(super) net_service: &'a str,
}

/// The kernel command line: the console, and the session settings
/// [`ral_daemon::boot::Boot`] reads back out of `/proc/cmdline`.
///
/// Built and rendered by `ral-daemon`'s own [`boot::command_line`] rather than
/// formatted here, so the two ends of the boot contract cannot drift apart at
/// compile time — the stale-`boot.img` case stays possible, and stays a loud
/// one-line refusal, but a *spelling* mismatch between writer and reader is no
/// longer possible at all.
///
/// The console is an emulated COM port (`ttyS0`), where the macOS backend's is
/// a virtio console (`hvc0`) — the one difference [`boot::command_line`]
/// leaves to its caller, because it is the hypervisor's fact, not the boot
/// contract's.  Everything else — the workspace as a named 9p share, the two
/// control-plane and net-wire ports, the epoch — is the shared shape.
fn kernel_command_line(plan: &Plan<'_>) -> String {
    boot::command_line(
        &boot::Boot {
            workspace: boot::Export::Plan9 {
                name: WORKSPACE_SHARE.to_string(),
                port: PLAN9_PORT,
            },
            port: plan.control_port,
            epoch: i64::try_from(plan.epoch).unwrap_or(i64::MAX),
            engine: boot::DEFAULT_ENGINE.to_string(),
            net: Some(boot::Net {
                port: plan.net_port,
                address: crate::GUEST_LINK.address,
                prefix: crate::GUEST_LINK.prefix,
                gateway: crate::GUEST_LINK.gateway,
            }),
        },
        "ttyS0",
    )
}

/// The whole compute system, ready to be serialised and handed to HCS.
pub(super) fn document(plan: &Plan<'_>) -> ComputeSystem {
    let mut scsi = BTreeMap::new();
    scsi.insert(
        "0".to_string(),
        Scsi {
            attachments: BTreeMap::from([
                (
                    // LUN 0: the guest's `/dev/sda`, the read-only rootfs.
                    "0".to_string(),
                    Attachment {
                        kind: "VirtualDisk",
                        path: display(plan.rootfs),
                        read_only: true,
                    },
                ),
                (
                    // LUN 1: the guest's `/dev/sdb`, formatted on every boot.
                    "1".to_string(),
                    Attachment {
                        kind: "VirtualDisk",
                        path: display(plan.session),
                        read_only: false,
                    },
                ),
            ]),
        },
    );

    let mut flags = plan9_flags::LINUX_METADATA;
    if plan.read_only {
        flags |= plan9_flags::READ_ONLY;
    }

    ComputeSystem {
        schema_version: SCHEMA,
        owner: "synod".to_string(),
        should_terminate_on_last_handle_closed: true,
        virtual_machine: VirtualMachine {
            stop_on_reset: true,
            chipset: Chipset {
                linux_kernel_direct: LinuxKernelDirect {
                    kernel_file_path: display(plan.kernel),
                    init_rd_path: display(plan.initramfs),
                    kernel_cmd_line: kernel_command_line(plan),
                },
            },
            compute_topology: ComputeTopology {
                memory: Memory {
                    size_in_mb: u64::from(plan.memory_mib),
                    allow_overcommit: true,
                },
                processor: Processor { count: plan.vcpus },
            },
            devices: Devices {
                scsi,
                plan9: Plan9 {
                    shares: vec![Plan9Share {
                        name: WORKSPACE_SHARE.to_string(),
                        access_name: WORKSPACE_SHARE.to_string(),
                        path: display(plan.folder),
                        port: PLAN9_PORT,
                        flags,
                    }],
                },
                hv_socket: HvSocket {
                    hv_socket_config: HvSocketConfig {
                        default_bind_security_descriptor: plan.socket_sddl.to_string(),
                        default_connect_security_descriptor: plan.socket_sddl.to_string(),
                        service_table: BTreeMap::from([
                            (
                                plan.control_service.to_string(),
                                HvSocketService {
                                    bind_security_descriptor: plan.socket_sddl.to_string(),
                                    connect_security_descriptor: plan.socket_sddl.to_string(),
                                },
                            ),
                            (
                                plan.net_service.to_string(),
                                HvSocketService {
                                    bind_security_descriptor: plan.socket_sddl.to_string(),
                                    connect_security_descriptor: plan.socket_sddl.to_string(),
                                },
                            ),
                        ]),
                    },
                },
                com_ports: BTreeMap::from([(
                    "0".to_string(),
                    ComPort {
                        named_pipe: plan.console_pipe.to_string(),
                    },
                )]),
            },
        },
    }
}

/// A Windows path as the document must carry it.
///
/// Lossy on purpose and harmless in practice: every path in a document is one
/// synod itself made under its own cache directory, or a folder the user picked
/// through the platform's own picker, so a path this cannot render is a path
/// Windows would not have given us.
fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ── The document's own shape ──────────────────────────────────────────────
//
// PascalCase throughout, because that is HCS's spelling; the two fields whose
// idiomatic Rust names do not PascalCase into the right word (`SizeInMB`, and
// `Type` which is a keyword here) say so explicitly.

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct ComputeSystem {
    schema_version: Version,
    owner: String,
    should_terminate_on_last_handle_closed: bool,
    virtual_machine: VirtualMachine,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "PascalCase")]
struct Version {
    major: u32,
    minor: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct VirtualMachine {
    stop_on_reset: bool,
    chipset: Chipset,
    compute_topology: ComputeTopology,
    devices: Devices,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Chipset {
    linux_kernel_direct: LinuxKernelDirect,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LinuxKernelDirect {
    kernel_file_path: String,
    init_rd_path: String,
    kernel_cmd_line: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ComputeTopology {
    memory: Memory,
    processor: Processor,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Memory {
    #[serde(rename = "SizeInMB")]
    size_in_mb: u64,
    allow_overcommit: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Processor {
    count: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Devices {
    scsi: BTreeMap<String, Scsi>,
    plan9: Plan9,
    hv_socket: HvSocket,
    com_ports: BTreeMap<String, ComPort>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Scsi {
    attachments: BTreeMap<String, Attachment>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Attachment {
    #[serde(rename = "Type")]
    kind: &'static str,
    path: String,
    read_only: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Plan9 {
    shares: Vec<Plan9Share>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Plan9Share {
    name: String,
    access_name: String,
    path: String,
    port: u32,
    flags: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct HvSocket {
    hv_socket_config: HvSocketConfig,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct HvSocketConfig {
    default_bind_security_descriptor: String,
    default_connect_security_descriptor: String,
    service_table: BTreeMap<String, HvSocketService>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct HvSocketService {
    bind_security_descriptor: String,
    connect_security_descriptor: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ComPort {
    named_pipe: String,
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: test scaffolding names the literal paths a document would carry; \
              no shell, no run, no card."
)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn plan() -> Plan<'static> {
        Plan {
            kernel: Path::new(r"C:\synod\boot\kernel"),
            initramfs: Path::new(r"C:\synod\boot\initramfs.img"),
            rootfs: Path::new(r"C:\synod\cache\rootfs.vhd"),
            session: Path::new(r"C:\synod\cache\session-1.vhd"),
            folder: Path::new(r"C:\Users\secretary\Documents\Invoices"),
            read_only: false,
            vcpus: 4,
            memory_mib: 4096,
            control_port: 1729,
            net_port: 1730,
            epoch: 1_771_200_000,
            console_pipe: r"\\.\pipe\synod-console-1",
            socket_sddl: "D:P(A;;FA;;;SY)",
            control_service: "000006c1-facb-11e6-bd58-64006a7986d3",
            net_service: "000006c2-facb-11e6-bd58-64006a7986d3",
        }
    }

    fn json() -> Value {
        serde_json::to_value(document(&plan())).expect("the document serialises")
    }

    /// The document carries exactly the machine the design describes: two
    /// disks in the guest's own order, one share, two socket services — the
    /// control plane and the net wire — one console, and, the load-bearing
    /// absence, no network adapter at all: the net wire is a second
    /// `HvSocket` service, not a NIC.  This is the Windows twin of the macOS
    /// backend's `the_configuration_is_the_machine_the_design_describes`.
    #[test]
    fn the_document_is_the_machine_the_design_describes() {
        let doc = json();
        let devices = &doc["VirtualMachine"]["Devices"];
        assert_eq!(
            devices["Scsi"]["0"]["Attachments"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(devices["Plan9"]["Shares"].as_array().unwrap().len(), 1);
        assert_eq!(devices["ComPorts"].as_object().unwrap().len(), 1);
        assert_eq!(
            devices["HvSocket"]["HvSocketConfig"]["ServiceTable"]
                .as_object()
                .unwrap()
                .len(),
            2,
            "the control plane and the net wire, and nothing else"
        );
        assert!(
            devices.get("NetworkAdapters").is_none(),
            "the guest has no way out but the two sockets"
        );
    }

    /// The rootfs is read-only and first; the session disk is writable and
    /// second.  The guest's initramfs resolves its two disks by *device
    /// order*, so this ordering is a contract with it, not a preference.
    #[test]
    fn the_disks_are_attached_in_the_order_the_guest_expects() {
        let doc = json();
        let luns = &doc["VirtualMachine"]["Devices"]["Scsi"]["0"]["Attachments"];
        assert_eq!(luns["0"]["ReadOnly"], Value::Bool(true));
        assert!(luns["0"]["Path"].as_str().unwrap().ends_with("rootfs.vhd"));
        assert_eq!(luns["1"]["ReadOnly"], Value::Bool(false));
        assert!(luns["1"]["Path"].as_str().unwrap().contains("session-"));
        assert_eq!(luns["0"]["Type"], Value::String("VirtualDisk".into()));
    }

    /// A read-only grant is the mount's own law: the flag rides in the share's
    /// `Flags`, so the guest cannot write the folder even if it tries.  Linux
    /// metadata is always on — without it the guest cannot preserve the
    /// permissions of what it writes.
    #[test]
    fn a_read_only_grant_is_carried_by_the_share_flags() {
        let writable = json();
        let flags = writable["VirtualMachine"]["Devices"]["Plan9"]["Shares"][0]["Flags"]
            .as_i64()
            .unwrap();
        assert_eq!(flags, i64::from(plan9_flags::LINUX_METADATA));

        let mut read_only_plan = plan();
        read_only_plan.read_only = true;
        let doc = serde_json::to_value(document(&read_only_plan)).unwrap();
        let flags = doc["VirtualMachine"]["Devices"]["Plan9"]["Shares"][0]["Flags"]
            .as_i64()
            .unwrap();
        assert_eq!(
            flags,
            i64::from(plan9_flags::LINUX_METADATA | plan9_flags::READ_ONLY)
        );
    }

    /// The command line addresses the daemon in the spelling its parser
    /// expects, and keeps the three vsock ports distinct — the control plane,
    /// the workspace transport and the net wire are three different sockets,
    /// and `service_guid` folds each into its own GUID, so a collision here
    /// would silently alias two of the machine's services onto one.
    #[test]
    fn the_command_line_addresses_the_daemon() {
        let line = kernel_command_line(&plan());
        assert!(line.contains("console=ttyS0"), "{line}");
        assert!(line.contains("ral.workspace=workspace"), "{line}");
        assert!(line.contains("ral.plan9=564"), "{line}");
        assert!(line.contains("ral.port=1729"), "{line}");
        assert!(line.contains("ral.epoch=1771200000"), "{line}");
        assert!(
            line.contains("ral.net=1730,10.0.2.15/24,10.0.2.2"),
            "{line}"
        );
        assert_ne!(
            PLAN9_PORT,
            super::super::CONTROL_PORT,
            "plan9/control collide"
        );
        assert_ne!(PLAN9_PORT, crate::NET_PORT, "plan9/net collide");
        assert_ne!(
            super::super::CONTROL_PORT,
            crate::NET_PORT,
            "control/net collide"
        );
    }

    /// Losing the handle stops the machine: a crashed synod leaves no guest
    /// running.
    #[test]
    fn the_machine_dies_with_its_last_handle() {
        assert_eq!(
            json()["ShouldTerminateOnLastHandleClosed"],
            Value::Bool(true)
        );
    }

    /// Every key HCS reads is `PascalCase`, and the two that do not fall out of
    /// that rule mechanically are spelled the way the service wants.
    #[test]
    fn the_documents_spelling_is_the_services() {
        let doc = json();
        assert!(doc["VirtualMachine"]["Chipset"]["LinuxKernelDirect"]["InitRdPath"].is_string());
        assert!(
            doc["VirtualMachine"]["ComputeTopology"]["Memory"]["SizeInMB"].is_u64(),
            "SizeInMB is not SizeInMb"
        );
        assert_eq!(doc["SchemaVersion"]["Major"], Value::from(2));
        assert_eq!(doc["SchemaVersion"]["Minor"], Value::from(1));
    }
}
