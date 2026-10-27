//! Linux vsock ports map to Hyper-V service GUIDs through the `hv_sock` template.
//! Only SYSTEM, Administrators and the owning user may reach these sockets.

use std::io;
use std::os::windows::io::{FromRawSocket, OwnedSocket};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::hcs::win::{self, HYPERVISOR};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
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

/// Named here because the bindings offer only family-specific aliases for `SOCK_STREAM`.
const SOCK_STREAM: i32 = 1;

/// The literal avoids an unchecked const narrowing; the layout assertion catches SDK drift.
const ADDRESS_LEN: i32 = 36;
const _: () = assert!(
    size_of::<SOCKADDR_HV>() == 36,
    "SOCKADDR_HV is no longer 36 bytes; ADDRESS_LEN must follow it"
);

/// `hv_sock` fixes every field except data1, which carries the Linux port.
const VSOCK_TEMPLATE: GUID = GUID {
    data1: 0,
    data2: 0xfacb,
    data3: 0x11e6,
    data4: [0xbd, 0x58, 0x64, 0x00, 0x6a, 0x79, 0x86, 0xd3],
};

pub(super) fn service_guid(port: u32) -> GUID {
    GUID {
        data1: port,
        ..VSOCK_TEMPLATE
    }
}

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

/// The machine ID addresses its sockets and must be unguessable, not merely unique.
pub(super) fn fresh_machine_id() -> GUID {
    let mut bytes = win::prng16();
    // Version 4, variant 1 keeps platform diagnostics able to read the UUID.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    GUID::from_u128(u128::from_be_bytes(bytes))
}

/// # Errors
/// Refuses an unreadable user identity; never falls back to a permissive descriptor.
pub(super) fn socket_sddl() -> Result<String, Error> {
    let sid = current_user_sid()?;
    Ok(format!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;{sid})"))
}

fn current_user_sid() -> Result<String, Error> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: a pseudo-handle for this process, a read-only access mask, and a
    // writable slot for the token handle.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
    if opened == 0 {
        return Err(Error::backend(
            HYPERVISOR,
            format!(
                "synod could not read its own user identity: {}",
                io::Error::last_os_error()
            ),
        ));
    }
    let token = TokenHandle(token);

    // TOKEN_USER includes a trailing SID; probe for the full allocation size.
    let mut needed = 0u32;
    // SAFETY: a deliberate zero-length probe; `GetTokenInformation` fails with
    // the required length in `needed`, writing nothing.
    unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &raw mut needed) };
    if needed == 0 {
        return Err(Error::backend(
            HYPERVISOR,
            "synod could not size its own user identity",
        ));
    }
    // u64 storage provides TOKEN_USER alignment; a byte vector does not.
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
        return Err(Error::backend(
            HYPERVISOR,
            format!(
                "synod could not read its own user identity: {}",
                io::Error::last_os_error()
            ),
        ));
    }

    // SAFETY: on success the word-aligned buffer holds a `TOKEN_USER` whose `User.Sid`
    // points into that same buffer, valid while `buffer` lives.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text: windows_sys::core::PWSTR = std::ptr::null_mut();
    // SAFETY: `sid` is a valid SID pointer for the length of this call, and
    // `text` is a writable slot the call fills with a `LocalAlloc`'d string.
    let converted = unsafe { ConvertSidToStringSidW(sid, &raw mut text) };
    let refused = || {
        Error::backend(
            HYPERVISOR,
            format!(
                "synod could not render its own user identity: {}",
                io::Error::last_os_error()
            ),
        )
    };
    if converted == 0 {
        return Err(refused());
    }
    // SAFETY: on success the call transfers a LocalAlloc allocation, or null.
    unsafe { win::take_wide(text) }.ok_or_else(refused)
}

struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        // SAFETY: a handle from `OpenProcessToken`, closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Listeners bind before boot so the guest cannot exhaust its dial patience before a listener exists.
pub(super) struct Listener {
    socket: SOCKET,
}

impl Listener {
    /// # Errors
    /// Returns the socket error, usually a descriptor mismatch if bind is denied.
    pub(super) fn bind(vm: &GUID, port: u32) -> io::Result<Self> {
        winsock_started()?;
        // SAFETY: AF_HYPERV and HV_PROTOCOL_RAW select Hyper-V; null info selects the provider; inheritance is disabled.
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
        // SAFETY: bind reads ADDRESS_LEN bytes of the live SOCKADDR_HV required by AF_HYPERV.
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
        // One dial is admitted per wire.
        // SAFETY: listen receives a live bound socket.
        if unsafe { listen(listener.socket, 1) } == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        Ok(listener)
    }

    /// # Errors
    /// Returns `TimedOut` when patience expires, or the accept error.
    pub(super) fn accept_within(&self, patience: Duration) -> io::Result<OwnedSocket> {
        let deadline = Instant::now() + patience;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the guest did not connect before the deadline",
                ));
            }
            let mut fds = WSAPOLLFD {
                fd: self.socket,
                events: POLLRDNORM,
                revents: 0,
            };
            // SAFETY: one initialized descriptor is polled; only revents is written; timeout stays within patience.
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
            // SAFETY: the readable listener makes accept prompt; null address outputs are permitted.
            let accepted =
                unsafe { accept(self.socket, std::ptr::null_mut(), std::ptr::null_mut()) };
            if accepted == INVALID_SOCKET {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: this fresh connected socket is transferred once and closed only by OwnedSocket.
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

/// Raw `WSASocketW` needs explicit `WSAStartup`; its reference is kept for the process lifetime.
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

    /// Directory and local users both have ordinary SIDs; no machine-local prefix is required.
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

    #[test]
    fn winsock_starts_once() {
        winsock_started().expect("Winsock 2.2 is available");
        winsock_started().expect("the second ask is the same answer");
    }
}
