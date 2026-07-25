//! The control plane: one `AF_HYPERV` socket, and who is allowed to use it.
//!
//! `dev/docs/VM/SYNOD.md` §2 fixes the arrangement: a Hyper-V socket on the
//! host, an ordinary `AF_VSOCK` socket in the guest, and the `hv_sock` driver
//! joining them.  The guest side of that is already written and unchanged —
//! `ral-daemon` dials `VMADDR_CID_HOST` on the port the kernel command line
//! named, exactly as it does under Virtualization.framework, because the Linux
//! vsock API does not vary with the hypervisor underneath.  What differs is
//! only this side.
//!
//! # A vsock port is a GUID here
//!
//! Hyper-V sockets are addressed by two GUIDs — which machine, which *service*
//! — where Linux uses a context id and a port number.  The bridge is a
//! template: a Linux vsock port `p` is the service GUID
//! `pppppppp-facb-11e6-bd58-64006a7986d3`, the port in hex as the first field.
//! [`service_guid`] is that template and nothing more, which is why a guest
//! compiled with no knowledge of Windows can still be dialled.
//!
//! # Who may use the socket
//!
//! A machine's sockets carry security descriptors, and the default the compute
//! service applies grants only `SYSTEM` and the built-in Administrators group.
//! Synod is neither: it runs as an ordinary user whom IT has put in *Hyper-V
//! Administrators*, which is not the Administrators group.  So the machine's
//! document names this user explicitly ([`socket_sddl`]) — and names *only*
//! this user, alongside the two the service would have granted anyway.  A
//! wildcard descriptor (`(A;;FA;;;WD)`, "everyone") would have been shorter and
//! would have let any process on the computer connect to the guest's control
//! plane, which is the one door in an otherwise networkless machine.

use std::io;
use std::os::windows::io::{FromRawSocket, OwnedSocket};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Networking::WinSock::{
    AF_HYPERV, INVALID_SOCKET, POLLRDNORM, SOCKADDR, SOCKET, SOCKET_ERROR,
    WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSADATA, WSAPOLLFD, WSAPoll, WSASocketW,
    WSAStartup, accept, bind, closesocket, listen,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::Hypervisor::{HV_PROTOCOL_RAW, SOCKADDR_HV};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::core::GUID;

/// `SOCK_STREAM`.  Winsock's own header spells this in a family-specific
/// constant per protocol, and `windows-sys` exports none that is plainly the
/// generic one, so the value is named here rather than borrowed from a
/// neighbouring family's alias.
const SOCK_STREAM: i32 = 1;

/// The length `bind` is told to read of a [`SOCKADDR_HV`]: two GUIDs behind a
/// family and a reserved word.
///
/// Written out rather than cast from `size_of` — a `usize`→`i32` conversion in
/// a `const` cannot be checked at all — and then checked against the real
/// layout below, which is stricter than the cast would have been: a Windows SDK
/// that changed the structure would fail the build here instead of silently
/// telling `bind` the wrong length.
const ADDRESS_LEN: i32 = 36;
const _: () = assert!(
    size_of::<SOCKADDR_HV>() == 36,
    "SOCKADDR_HV is no longer 36 bytes; ADDRESS_LEN must follow it"
);

/// The template that turns a Linux vsock port into a Hyper-V service GUID.
///
/// Everything but the first field is fixed by the `hv_sock` driver's own
/// convention (`VMADDR_*` ports live under this namespace); the first field is
/// the port.
const VSOCK_TEMPLATE: GUID = GUID {
    data1: 0,
    data2: 0xfacb,
    data3: 0x11e6,
    data4: [0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3],
};

/// The service GUID a guest's vsock port `port` appears as on the host.
pub(super) fn service_guid(port: u32) -> GUID {
    GUID {
        data1: port,
        ..VSOCK_TEMPLATE
    }
}

/// A GUID in the lowercase hyphenated form HCS documents use.
pub(super) fn format_guid(guid: &GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7],
    )
}

/// A fresh identifier for one machine, from the operating system's own
/// generator.
///
/// It has to be unguessable, not merely unique: the identifier *is* the
/// `VmId` half of the control plane's address, so a process that can predict
/// it can try to reach the socket before synod's own listener has claimed it.
/// A counter or a timestamp would be fine for uniqueness and wrong for that.
pub(super) fn fresh_machine_id() -> GUID {
    let mut bytes = [0u8; 16];
    // SAFETY: `ProcessPrng` fills exactly the buffer described by the pointer
    // and length it is given, and is documented as always succeeding.
    unsafe {
        windows_sys::Win32::Security::Cryptography::ProcessPrng(bytes.as_mut_ptr(), bytes.len())
    };
    // Version 4, variant 1 — an ordinary random UUID, so tools that read the
    // identifier back (`hcsdiag list`) see a well-formed one.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    GUID {
        data1: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        data2: u16::from_be_bytes([bytes[4], bytes[5]]),
        data3: u16::from_be_bytes([bytes[6], bytes[7]]),
        data4: [
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ],
    }
}

/// The security descriptor a machine's sockets are given: `SYSTEM`, the
/// built-in Administrators, and the user synod is running as — nobody else.
///
/// `D:P` is a protected DACL, so no inherited entry widens it afterwards.
///
/// # Errors
/// Returns a sentence if this process cannot read its own user, which would
/// leave synod unable to say who may reach the guest — a refusal, never a
/// fallback to a permissive descriptor.
pub(super) fn socket_sddl() -> Result<String, String> {
    let sid = current_user_sid()?;
    Ok(format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{sid})"))
}

/// This process's user, as an SDDL SID string.
fn current_user_sid() -> Result<String, String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: a pseudo-handle for this process, a read-only access mask, and a
    // writable slot for the token handle.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
    if opened == 0 {
        return Err(format!(
            "synod could not read its own user identity: {}",
            io::Error::last_os_error()
        ));
    }
    let token = TokenHandle(token);

    // `TOKEN_USER` is a header followed by the SID itself, so the buffer is
    // sized by asking first: the two-call form is what the API documents, and
    // it costs nothing.
    let mut needed = 0u32;
    // SAFETY: a deliberate zero-length probe; `GetTokenInformation` fails with
    // the required length in `needed`, writing nothing.
    unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &raw mut needed) };
    if needed == 0 {
        return Err("synod could not size its own user identity".to_string());
    }
    // A `Vec<u64>` rather than a `Vec<u8>`, because what comes back is a
    // `TOKEN_USER` — a struct with a pointer in it — and a byte vector's
    // allocation carries no alignment guarantee strong enough to hold one.
    // Rounding the byte count up to whole words is the whole cost of being
    // right about that.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>())];
    // SAFETY: `buffer` is writable and at least `needed` bytes, the length the
    // probe above asked for, and is aligned for the struct written into it.
    let read = unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        )
    };
    if read == 0 {
        return Err(format!(
            "synod could not read its own user identity: {}",
            io::Error::last_os_error()
        ));
    }

    // SAFETY: on success the word-aligned buffer holds a `TOKEN_USER` whose `User.Sid`
    // points into that same buffer, valid while `buffer` lives.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `sid` is a valid SID pointer for the length of this call, and
    // `text` is a writable slot the call fills with a `LocalAlloc`'d string.
    let converted = unsafe { ConvertSidToStringSidW(sid, &raw mut text) };
    if converted == 0 || text.is_null() {
        return Err(format!(
            "synod could not render its own user identity: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: `text` is a NUL-terminated wide string owned by this function.
    let rendered = unsafe {
        let mut len = 0usize;
        while *text.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    };
    // SAFETY: released exactly once, after the copy above.
    unsafe { LocalFree(text.cast()) };
    Ok(rendered)
}

/// An access token closed on every path out of [`current_user_sid`].
struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        // SAFETY: a handle from `OpenProcessToken`, closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The host's end of one machine's control plane, before the guest has dialled
/// it.
///
/// Bound *before* the machine starts, deliberately: the guest's daemon dials
/// with a few seconds' patience, and a listener that appeared afterwards would
/// be racing a boot.  Whoever holds this holds the only door into the guest.
pub(super) struct Listener {
    socket: SOCKET,
}

impl Listener {
    /// Listen for `vm`'s guest to dial `port`.
    ///
    /// # Errors
    /// Returns the socket error.  A refusal here is nearly always the
    /// machine's security descriptor: binding a service of a specific machine
    /// is a permission the document ([`socket_sddl`]) grants, so a `bind` this
    /// process is not allowed is a mismatch between the two.
    pub(super) fn bind(vm: &GUID, port: u32) -> io::Result<Self> {
        winsock_started()?;
        // SAFETY: a plain socket creation.  `AF_HYPERV` with `HV_PROTOCOL_RAW`
        // is the Hyper-V socket family; the null protocol-info pointer asks for
        // the default provider, and the no-inherit flag keeps the socket out of
        // any child a run spawns.
        let socket = unsafe {
            WSASocketW(
                i32::from(AF_HYPERV),
                SOCK_STREAM,
                HV_PROTOCOL_RAW.cast_signed(),
                std::ptr::null(),
                0,
                WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
            )
        };
        if socket == INVALID_SOCKET {
            return Err(io::Error::last_os_error());
        }
        let listener = Self { socket };

        let addr = SOCKADDR_HV {
            Family: AF_HYPERV,
            Reserved: 0,
            VmId: *vm,
            ServiceId: service_guid(port),
        };
        // SAFETY: `bind` reads `namelen` bytes of the address it is given, and
        // `SOCKADDR_HV` is the address type `AF_HYPERV` sockets take; the cast
        // is the same one every family's `bind` call performs.
        let bound = unsafe {
            bind(
                listener.socket,
                (&raw const addr).cast::<SOCKADDR>(),
                ADDRESS_LEN,
            )
        };
        if bound == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        // A machine has exactly one control plane, so a backlog of one is not a
        // limit — a second dial is an anomaly, and refusing it is right.
        // SAFETY: a plain `listen` on a bound socket.
        if unsafe { listen(listener.socket, 1) } == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        Ok(listener)
    }

    /// Wait for the guest to dial, up to `patience`, and hand back the stream.
    ///
    /// A connection is proof the guest booted far enough to run its init — the
    /// same readiness signal the macOS backend takes from its virtio socket
    /// listener — and the connection *is* the session's wire, not a handshake
    /// to be spent and dropped.
    ///
    /// # Errors
    /// Returns [`io::ErrorKind::TimedOut`] if nothing dialled within
    /// `patience`, or the socket error if the accept itself failed.
    pub(super) fn accept_within(&self, patience: Duration) -> io::Result<OwnedSocket> {
        let deadline = Instant::now() + patience;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the guest did not dial the control plane",
                ));
            }
            let mut fds = WSAPOLLFD {
                fd: self.socket,
                events: POLLRDNORM,
                revents: 0,
            };
            // SAFETY: one fully initialised descriptor and its count; `WSAPoll`
            // writes only `revents`.  The wait is clamped to what is left of
            // the caller's patience.
            let ready = unsafe {
                WSAPoll(
                    &raw mut fds,
                    1,
                    i32::try_from(left.as_millis().min(i32::MAX as u128)).expect("clamped"),
                )
            };
            if ready == SOCKET_ERROR {
                return Err(io::Error::last_os_error());
            }
            if ready == 0 {
                continue;
            }
            // SAFETY: the listener is readable, so `accept` returns promptly;
            // the address is of no interest — a machine's one guest is the only
            // thing that can be dialling — so both out-parameters are null,
            // which `accept` documents as permitted.
            let accepted =
                unsafe { accept(self.socket, std::ptr::null_mut(), std::ptr::null_mut()) };
            if accepted == INVALID_SOCKET {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `accepted` is a fresh connected socket this function owns
            // and does not close on any other path, so ownership transfers
            // cleanly to the `OwnedSocket`.
            return Ok(unsafe { OwnedSocket::from_raw_socket(accepted as u64) });
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // SAFETY: `socket` is this listener's own, closed exactly once.  The
        // accepted connection is a separate socket and is unaffected.
        unsafe { closesocket(self.socket) };
    }
}

/// Winsock, initialised once per process.
///
/// std does this for its own sockets, but a socket created through
/// `WSASocketW` before std has ever opened one would find Winsock
/// uninitialised, so this backend cannot rely on that having happened.
/// Calling `WSAStartup` more than once is explicitly allowed — it is
/// reference-counted — and the reference is deliberately never released: the
/// library is wanted for the whole life of the process.
fn winsock_started() -> io::Result<()> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let code = *STARTED.get_or_init(|| {
        let mut data = WSADATA::default();
        // SAFETY: `data` is a writable `WSADATA`; 2.2 is the version every
        // supported Windows offers.
        unsafe { WSAStartup(0x0202, &raw mut data) }
    });
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The template really is the `hv_sock` convention: a port lands in the
    /// first field, in hex, and the rest of the GUID is fixed.  1729 is the
    /// control port the daemon is told about, so its rendering is the one that
    /// must be right.
    #[test]
    fn a_vsock_port_becomes_its_service_guid() {
        assert_eq!(
            format_guid(&service_guid(1729)),
            "000006c1-facb-11e6-bd58-64006a7986d3"
        );
        assert_eq!(
            format_guid(&service_guid(564)),
            "00000234-facb-11e6-bd58-64006a7986d3"
        );
    }

    /// Two machines never share an identifier, and an identifier is a
    /// well-formed version-4 UUID — the form the platform's own diagnostics
    /// expect to read back.
    #[test]
    fn machine_identifiers_are_fresh_and_well_formed() {
        let a = format_guid(&fresh_machine_id());
        let b = format_guid(&fresh_machine_id());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4', "version 4: {a}");
        assert!(
            matches!(a.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "variant 1: {a}"
        );
    }

    /// The descriptor names three principals and no wildcard: `SYSTEM`, the
    /// built-in Administrators, and this user.  `WD` — everyone — would open
    /// the guest's one door to every process on the computer.
    ///
    /// The user's entry is a *resolved* SID rather than a two-letter alias, and
    /// deliberately not asserted to be a machine-local one: a university
    /// account is as likely to be a directory identity (`S-1-12-1-…`, Entra ID)
    /// as a local one (`S-1-5-21-…`), and both are ordinary SIDs to SDDL.
    #[test]
    fn the_socket_descriptor_names_this_user_and_no_wildcard() {
        let sddl = socket_sddl().expect("this process can read its own user");
        assert!(sddl.starts_with("D:P"), "a protected DACL: {sddl}");
        assert!(sddl.contains("(A;;FA;;;SY)"), "{sddl}");
        assert!(sddl.contains("(A;;FA;;;BA)"), "{sddl}");
        let user = sddl
            .rsplit("(A;;FA;;;")
            .next()
            .and_then(|tail| tail.strip_suffix(')'))
            .expect("the last entry is the user's");
        assert!(user.starts_with("S-1-"), "a resolved SID: {user}");
        assert!(
            user.split('-').count() >= 4,
            "an account SID, not an authority alone: {user}"
        );
        assert!(!sddl.contains(";WD)"), "no wildcard: {sddl}");
    }

    /// Winsock initialises, and initialises once — the second call is the
    /// cached answer, not a second `WSAStartup`.
    #[test]
    fn winsock_starts_once() {
        winsock_started().expect("Winsock 2.2 is available");
        winsock_started().expect("the second ask is the same answer");
    }
}
