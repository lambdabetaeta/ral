//! Protobuf for the `vmservice.VM` service (`openvmm/openvmm_ttrpc_vmservice/src/vmservice.proto`).
//!
//! Scalars at their default are omitted, as in canonical proto3; submessages are always present.

use crate::error::PowerEvent;

/// Process exit codes for the guest's power events, one per `GuestPowerActions` event.
pub(crate) const EXIT_SHUTDOWN: u8 = 0;
pub(crate) const EXIT_RESET: u8 = 64;
pub(crate) const EXIT_CRASH: u8 = 65;
pub(crate) const EXIT_WATCHDOG: u8 = 66;

/// What `OpenVMM`'s exit code says ended the machine.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Exit {
    Guest(PowerEvent),
    /// `OpenVMM`'s own exit, which no power event explains.
    Vmm(u32),
}

pub(crate) fn exit(code: u32) -> Exit {
    match u8::try_from(code) {
        Ok(EXIT_SHUTDOWN) => Exit::Guest(PowerEvent::PowerOff),
        Ok(EXIT_RESET) => Exit::Guest(PowerEvent::Reset),
        Ok(EXIT_CRASH) => Exit::Guest(PowerEvent::TripleFault),
        Ok(EXIT_WATCHDOG) => Exit::Guest(PowerEvent::Watchdog),
        _ => Exit::Vmm(code),
    }
}

/// `GuestPowerAction.GUEST_POWER_ACTION_EXIT`.
const ACTION_EXIT: u64 = 3;

/// A SCSI disk backed by a raw image.
pub(crate) struct Disk<'a> {
    pub(crate) lun: u32,
    pub(crate) path: &'a str,
    pub(crate) read_only: bool,
}

/// A virtio-fs share of a host folder.
pub(crate) struct Share<'a> {
    pub(crate) tag: &'a str,
    pub(crate) root: &'a str,
    pub(crate) read_only: bool,
}

pub(crate) struct Machine<'a> {
    pub(crate) memory_mb: u64,
    pub(crate) processors: u32,
    pub(crate) disks: &'a [Disk<'a>],
    pub(crate) shares: &'a [Share<'a>],
    /// COM1's named pipe, which the VMM opens as a client.
    pub(crate) console_pipe: Option<&'a str>,
    pub(crate) kernel: &'a str,
    pub(crate) initrd: &'a str,
    pub(crate) cmdline: &'a str,
    pub(crate) hvsocket: &'a str,
}

/// `CreateVMRequest` bytes, power actions and exit codes included.
pub(crate) fn create_vm(machine: &Machine<'_>) -> Vec<u8> {
    let disk = |disk: &Disk<'_>| {
        message(
            1,
            &[
                uint(2, disk.lun.into()),
                bytes(3, disk.path.as_bytes()),
                flag(5, disk.read_only),
            ],
        )
    };
    let share = |share: &Share<'_>| {
        let options = if share.read_only {
            "metadata;ro"
        } else {
            "metadata"
        };
        message(
            5,
            &[
                bytes(1, share.tag.as_bytes()),
                bytes(2, share.root.as_bytes()),
                bytes(3, options.as_bytes()),
            ],
        )
    };
    let devices: Vec<_> = machine
        .disks
        .iter()
        .map(disk)
        .chain(machine.shares.iter().map(share))
        .collect();
    let console = machine.console_pipe.map_or_else(Vec::new, |pipe| {
        message(
            4,
            &[message(
                3,
                &[uint(1, 0), bytes(2, pipe.as_bytes()), flag(3, true)],
            )],
        )
    });
    let config = [
        message(1, &[uint(1, machine.memory_mb)]),
        message(2, &[uint(1, machine.processors.into())]),
        message(3, &devices),
        console,
        message(
            5,
            &[
                bytes(1, machine.kernel.as_bytes()),
                bytes(2, machine.initrd.as_bytes()),
                bytes(3, machine.cmdline.as_bytes()),
            ],
        ),
        message(9, &[bytes(1, machine.hvsocket.as_bytes())]),
        message(
            12,
            &[
                uint(1, ACTION_EXIT),
                uint(2, ACTION_EXIT),
                uint(3, ACTION_EXIT),
                uint(4, ACTION_EXIT),
                uint(5, EXIT_SHUTDOWN.into()),
                uint(6, EXIT_RESET.into()),
                uint(7, EXIT_CRASH.into()),
                uint(8, EXIT_WATCHDOG.into()),
            ],
        ),
    ];
    message(1, &config)
}

const VARINT: u64 = 0;
const I64: u64 = 1;
const LEN: u64 = 2;
const I32: u64 = 5;

/// A varint field (`uint32`, `uint64`, `bool`, enum), omitted when zero.
pub(super) fn uint(field: u32, value: u64) -> Vec<u8> {
    if value == 0 {
        return Vec::new();
    }
    let mut out = key(field, VARINT);
    put_varint(&mut out, value);
    out
}

fn flag(field: u32, value: bool) -> Vec<u8> {
    uint(field, value.into())
}

/// A `string` or `bytes` field, omitted when empty.
pub(super) fn bytes(field: u32, value: &[u8]) -> Vec<u8> {
    if value.is_empty() {
        Vec::new()
    } else {
        delimited(field, value)
    }
}

/// A submessage field whose body is the concatenation of `parts`.
fn message(field: u32, parts: &[Vec<u8>]) -> Vec<u8> {
    delimited(field, &parts.concat())
}

fn delimited(field: u32, value: &[u8]) -> Vec<u8> {
    let mut out = key(field, LEN);
    put_varint(&mut out, value.len() as u64);
    out.extend_from_slice(value);
    out
}

fn key(field: u32, wire: u64) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, u64::from(field) << 3 | wire);
    out
}

fn put_varint(out: &mut Vec<u8>, value: u64) {
    let low = value.to_le_bytes()[0] & 0x7f;
    match value >> 7 {
        0 => out.push(low),
        high => {
            out.push(low | 0x80);
            put_varint(out, high);
        }
    }
}

/// A decoded field value; fixed-width values are skipped, never read.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Value<'a> {
    Varint(u64),
    Fixed,
    Bytes(&'a [u8]),
}

/// The fields of `message` in order, ending after the first malformed one.
pub(super) fn fields(
    message: &[u8],
) -> impl Iterator<Item = Result<(u64, Value<'_>), &'static str>> {
    let mut rest = message;
    std::iter::from_fn(move || {
        (!rest.is_empty()).then(|| {
            let field = next_field(&mut rest);
            if field.is_err() {
                rest = &[];
            }
            field
        })
    })
}

fn next_field<'a>(rest: &mut &'a [u8]) -> Result<(u64, Value<'a>), &'static str> {
    let key = varint(rest)?;
    let value = match key & 7 {
        VARINT => Value::Varint(varint(rest)?),
        I64 => {
            take(rest, 8)?;
            Value::Fixed
        }
        LEN => {
            let length = usize::try_from(varint(rest)?).map_err(|_| TRUNCATED)?;
            Value::Bytes(take(rest, length)?)
        }
        I32 => {
            take(rest, 4)?;
            Value::Fixed
        }
        _ => return Err("a protobuf field uses a group or an unknown wire type"),
    };
    Ok((key >> 3, value))
}

const TRUNCATED: &str = "a protobuf field runs past the end of its message";

fn take<'a>(rest: &mut &'a [u8], length: usize) -> Result<&'a [u8], &'static str> {
    let (taken, tail) = rest.split_at_checked(length).ok_or(TRUNCATED)?;
    *rest = tail;
    Ok(taken)
}

fn varint(rest: &mut &[u8]) -> Result<u64, &'static str> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let (&byte, tail) = rest.split_first().ok_or(TRUNCATED)?;
        *rest = tail;
        // The tenth byte holds bit 63 alone; more would wrap, as a length of 2^64 to 0.
        if shift == 63 && byte > 1 {
            break;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("a protobuf varint overflows 64 bits")
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn machine<'a>(shares: &'a [Share<'a>], console_pipe: Option<&'a str>) -> Machine<'a> {
        const DISKS: &[Disk<'_>] = &[
            Disk {
                lun: 0,
                path: "r.img",
                read_only: true,
            },
            Disk {
                lun: 1,
                path: "s.img",
                read_only: false,
            },
        ];
        Machine {
            memory_mb: 6144,
            processors: 4,
            disks: DISKS,
            shares,
            console_pipe,
            kernel: "k",
            initrd: "i",
            cmdline: "c",
            hvsocket: "v.sock",
        }
    }

    /// `VMConfig` fields 1 and 2, and field 3's two disks.
    const HEAD: &[&[u8]] = &[
        &[0x0a, 0x03],       // 1: MemoryConfig, 3 bytes
        &[0x08, 0x80, 0x30], //   1: memory_mb = 6144
        &[0x12, 0x02],       // 2: ProcessorConfig, 2 bytes
        &[0x08, 0x04],       //   1: processor_count = 4
    ];
    const DISKS: &[&[u8]] = &[
        &[0x0a, 0x09], //   1: SCSIDisk, 9 bytes; 2: lun = 0 omitted
        &[0x1a, 0x05], //     3: host_path, 5 bytes
        b"r.img",
        &[0x28, 0x01], //     5: read_only = true
        &[0x0a, 0x09], //   1: SCSIDisk, 9 bytes
        &[0x10, 0x01], //     2: lun = 1
        &[0x1a, 0x05], //     3: host_path, 5 bytes; 5: read_only = false omitted
        b"s.img",
    ];
    /// `VMConfig` fields 5, 9 and 12.
    const TAIL: &[&[u8]] = &[
        &[0x2a, 0x09],       // 5: DirectBoot, 9 bytes
        &[0x0a, 0x01, b'k'], //   1: kernel_path
        &[0x12, 0x01, b'i'], //   2: initrd_path
        &[0x1a, 0x01, b'c'], //   3: kernel_cmdline
        &[0x4a, 0x08],       // 9: HVSocketConfig, 8 bytes
        &[0x0a, 0x06],       //   1: path, 6 bytes
        b"v.sock",
        &[0x62, 0x0e], // 12: GuestPowerActions, 14 bytes
        &[0x08, 0x03], //   1: shutdown = GUEST_POWER_ACTION_EXIT
        &[0x10, 0x03], //   2: reset = GUEST_POWER_ACTION_EXIT
        &[0x18, 0x03], //   3: crash = GUEST_POWER_ACTION_EXIT
        &[0x20, 0x03], //   4: watchdog = GUEST_POWER_ACTION_EXIT; 5: shutdown_exit_code = 0 omitted
        &[0x30, 0x40], //   6: reset_exit_code = 64
        &[0x38, 0x41], //   7: crash_exit_code = 65
        &[0x40, 0x42], //   8: watchdog_exit_code = 66
    ];

    #[test]
    fn create_vm_with_console_and_writable_share() {
        let shares = [Share {
            tag: "w",
            root: r"C:\w",
            read_only: false,
        }];
        let expected: &[&[&[u8]]] = &[
            &[&[0x0a, 0x6d]], // CreateVMRequest 1: VMConfig, 109 bytes
            HEAD,
            &[&[0x1a, 0x2b]], // 3: DevicesConfig, 43 bytes
            DISKS,
            &[
                &[0x2a, 0x13],       //   5: VirtioFs, 19 bytes
                &[0x0a, 0x01, b'w'], //     1: tag
                &[0x12, 0x04],       //     2: root_path, 4 bytes
                br"C:\w",
                &[0x1a, 0x08], //     3: mount_options, 8 bytes
                b"metadata",
                &[0x22, 0x10], // 4: SerialConfig, 16 bytes
                &[0x1a, 0x0e], //   3: Config, 14 bytes; 1: port = 0 (COM1) omitted
                &[0x12, 0x0a], //     2: socket_path, 10 bytes
                b"//./pipe/c",
                &[0x18, 0x01], //     3: connect = true
            ],
            TAIL,
        ];
        assert_eq!(
            create_vm(&machine(&shares, Some("//./pipe/c"))),
            expected.concat().concat()
        );
    }

    #[test]
    fn create_vm_without_console_and_read_only_share() {
        let shares = [Share {
            tag: "w",
            root: r"C:\w",
            read_only: true,
        }];
        let expected: &[&[&[u8]]] = &[
            &[&[0x0a, 0x5e]], // CreateVMRequest 1: VMConfig, 94 bytes
            HEAD,
            &[&[0x1a, 0x2e]], // 3: DevicesConfig, 46 bytes
            DISKS,
            &[
                &[0x2a, 0x16],       //   5: VirtioFs, 22 bytes
                &[0x0a, 0x01, b'w'], //     1: tag
                &[0x12, 0x04],       //     2: root_path, 4 bytes
                br"C:\w",
                &[0x1a, 0x0b], //     3: mount_options, 11 bytes; 4: SerialConfig absent
                b"metadata;ro",
            ],
            TAIL,
        ];
        assert_eq!(
            create_vm(&machine(&shares, None)),
            expected.concat().concat()
        );
    }

    #[test]
    fn varints_round_trip_at_every_width() {
        for value in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            let mut encoded = Vec::new();
            put_varint(&mut encoded, value);
            let mut rest = &encoded[..];
            assert_eq!(varint(&mut rest), Ok(value));
            assert_eq!(rest, []);
        }
        assert!(varint(&mut &[0xff; 11][..]).is_err());
    }

    #[test]
    fn a_varint_past_64_bits_is_refused_rather_than_wrapped() {
        let two_to_the_63 = [[0x80; 9].as_slice(), &[0x01]].concat();
        assert_eq!(varint(&mut &two_to_the_63[..]), Ok(1 << 63));
        // 2^64, which would wrap to 0.
        let two_to_the_64 = [[0x80; 9].as_slice(), &[0x02]].concat();
        assert_eq!(
            varint(&mut &two_to_the_64[..]),
            Err("a protobuf varint overflows 64 bits")
        );
        // 3: bytes of length 2^64, which would otherwise read as an empty payload.
        let field = [[0x1a].as_slice(), &two_to_the_64].concat();
        assert_eq!(
            fields(&field).collect::<Vec<_>>(),
            [Err("a protobuf varint overflows 64 bits")]
        );
    }

    #[test]
    fn exit_codes_name_their_power_events_and_nothing_else() {
        assert_eq!(exit(0), Exit::Guest(PowerEvent::PowerOff));
        assert_eq!(exit(64), Exit::Guest(PowerEvent::Reset));
        assert_eq!(exit(65), Exit::Guest(PowerEvent::TripleFault));
        assert_eq!(exit(66), Exit::Guest(PowerEvent::Watchdog));
        // 320 is 64 modulo 256: no power event.
        for code in [1, 63, 67, 256, 320, 0xC000_0005] {
            assert_eq!(exit(code), Exit::Vmm(code));
        }
    }

    #[test]
    fn fields_skip_every_wire_type_and_stop_at_the_first_malformed() {
        let message: &[u8] = &[
            0x08, 0x96, 0x01, // 1: varint 150
            0x11, 0, 0, 0, 0, 0, 0, 0, 0, // 2: fixed64
            0x1a, 0x02, b'h', b'i', // 3: bytes "hi"
            0x25, 0, 0, 0, 0,    // 4: fixed32
            0x2b, // 5: start group, refused
            0x08, 0x01, // never reached
        ];
        assert_eq!(
            fields(message).collect::<Vec<_>>(),
            [
                Ok((1, Value::Varint(150))),
                Ok((2, Value::Fixed)),
                Ok((3, Value::Bytes(b"hi"))),
                Ok((4, Value::Fixed)),
                Err("a protobuf field uses a group or an unknown wire type"),
            ]
        );
        assert_eq!(
            fields(&[0x1a, 0x05, b'h']).collect::<Vec<_>>(),
            [Err(TRUNCATED)]
        );
    }
}
