//! HCS configuration is JSON, built without creating a machine.
//! Rootfs is read-only at LUN 0; the guest formats the writable session disk at LUN 1.
//! Read-only grants are enforced by share flags; no network adapter is configured.

use std::collections::BTreeMap;
use std::path::Path;

use guest_abi::{boot::Export, layout::WORKSPACE_NAME, ports};
use serde::Serialize;

use crate::error::Error;
use crate::hcs::win::document_text;

/// Schema 2.1 matches Microsoft's Linux-container host, including direct kernel boots.
const SCHEMA: Version = Version { major: 2, minor: 1 };

/// `LINUX_METADATA` preserves Linux ownership and modes; `READ_ONLY` enforces the grant at the mount.
mod plan9_flags {
    pub(super) const READ_ONLY: i32 = 0x0000_0001;
    pub(super) const LINUX_METADATA: i32 = 0x0000_0004;
}

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
    /// The host's wall clock at boot, for the guest to adopt.
    pub(super) epoch: u64,
    pub(super) console_pipe: &'a str,
    pub(super) socket_sddl: &'a str,
    pub(super) control_service: &'a str,
    pub(super) net_service: &'a str,
}

/// # Errors
/// Returns [`Error::NotUnicode`] rather than respelling a path in the JSON document.
pub(super) fn document(plan: &Plan<'_>) -> Result<ComputeSystem, Error> {
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
                        path: document_text(plan.rootfs)?.to_owned(),
                        read_only: true,
                    },
                ),
                (
                    // LUN 1: the guest's `/dev/sdb`, formatted on every boot.
                    "1".to_string(),
                    Attachment {
                        kind: "VirtualDisk",
                        path: document_text(plan.session)?.to_owned(),
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

    Ok(ComputeSystem {
        schema_version: SCHEMA,
        owner: "synod".to_string(),
        should_terminate_on_last_handle_closed: true,
        virtual_machine: VirtualMachine {
            stop_on_reset: true,
            chipset: Chipset {
                linux_kernel_direct: LinuxKernelDirect {
                    kernel_file_path: document_text(plan.kernel)?.to_owned(),
                    init_rd_path: document_text(plan.initramfs)?.to_owned(),
                    kernel_cmd_line: crate::spec::boot_line(
                        Export::Plan9 {
                            name: WORKSPACE_NAME.into(),
                            port: ports::PLAN9,
                        },
                        "ttyS0",
                        plan.epoch,
                    ),
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
                        name: WORKSPACE_NAME.to_string(),
                        access_name: WORKSPACE_NAME.to_string(),
                        path: document_text(plan.folder)?.to_owned(),
                        port: ports::PLAN9,
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
    })
}

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
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::spec::{MachineSpec, SIZE};
    use serde_json::Value;

    fn spec() -> MachineSpec {
        MachineSpec {
            folder: Path::new(r"C:\Users\secretary\Documents\Invoices").into(),
            read_only: false,
        }
    }

    fn plan(spec: &MachineSpec) -> Plan<'_> {
        Plan {
            kernel: Path::new(r"C:\synod\boot\kernel"),
            initramfs: Path::new(r"C:\synod\boot\initramfs.img"),
            rootfs: Path::new(r"C:\synod\cache\rootfs.vhd"),
            session: Path::new(r"C:\synod\cache\session-1.vhd"),
            folder: &spec.folder,
            read_only: spec.read_only,
            vcpus: u32::from(SIZE.vcpus),
            memory_mib: SIZE.memory_mib,
            epoch: 1_771_200_000,
            console_pipe: r"\\.\pipe\synod-console-1",
            socket_sddl: "D:P(A;;FA;;;SY)",
            control_service: "000006c1-facb-11e6-bd58-64006a7986d3",
            net_service: "000006c2-facb-11e6-bd58-64006a7986d3",
        }
    }

    fn json() -> Value {
        serde_json::to_value(document(&plan(&spec())).expect("every path is Unicode"))
            .expect("the document serialises")
    }

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

    #[test]
    fn a_read_only_grant_is_carried_by_the_share_flags() {
        let writable = json();
        let flags = writable["VirtualMachine"]["Devices"]["Plan9"]["Shares"][0]["Flags"]
            .as_i64()
            .unwrap();
        assert_eq!(flags, i64::from(plan9_flags::LINUX_METADATA));

        let mut spec = spec();
        spec.read_only = true;
        let read_only_plan = plan(&spec);
        let doc = serde_json::to_value(document(&read_only_plan).unwrap()).unwrap();
        let flags = doc["VirtualMachine"]["Devices"]["Plan9"]["Shares"][0]["Flags"]
            .as_i64()
            .unwrap();
        assert_eq!(
            flags,
            i64::from(plan9_flags::LINUX_METADATA | plan9_flags::READ_ONLY)
        );
    }

    #[test]
    fn the_command_line_addresses_the_guest() {
        let doc = json();
        let line = doc["VirtualMachine"]["Chipset"]["LinuxKernelDirect"]["KernelCmdLine"]
            .as_str()
            .unwrap();
        assert!(line.contains("console=ttyS0"), "{line}");
        assert!(line.contains("ral.workspace=workspace"), "{line}");
        assert!(line.contains("ral.plan9=564"), "{line}");
        assert!(line.contains("ral.port=1729"), "{line}");
        assert!(line.contains("ral.epoch=1771200000"), "{line}");
        assert!(
            line.contains("ral.net=1730,10.0.2.15/24,10.0.2.2"),
            "{line}"
        );
    }

    #[test]
    fn the_machine_dies_with_its_last_handle() {
        assert_eq!(
            json()["ShouldTerminateOnLastHandleClosed"],
            Value::Bool(true)
        );
    }

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

    #[test]
    fn a_path_that_is_not_unicode_is_refused_rather_than_respelled() {
        use std::os::windows::ffi::OsStringExt;

        let mut units: Vec<u16> = r"C:\Users\secretary\Invoices".encode_utf16().collect();
        units.push(0xD800);
        let folder = std::path::PathBuf::from(std::ffi::OsString::from_wide(&units));
        let spec = MachineSpec {
            folder: folder.clone(),
            read_only: false,
        };
        let refused = plan(&spec);
        match document(&refused) {
            Err(Error::NotUnicode(path)) => assert_eq!(path, folder),
            Err(other) => panic!("refused for the wrong reason: {other:?}"),
            Ok(_) => panic!("a name with an unpaired surrogate must not be written"),
        }
    }
}
