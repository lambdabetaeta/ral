//! Windows `AF_UNIX` stream sockets, only at paths that fit `sun_path`.

use std::io;
use std::os::windows::io::{AsRawSocket, FromRawSocket, OwnedSocket, RawSocket};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows_sys::Win32::Networking::WinSock::{
    AF_UNIX, INVALID_SOCKET, POLLRDNORM, SOCK_STREAM, SOCKADDR, SOCKADDR_UN, SOCKET, SOCKET_ERROR,
    WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSADATA, WSAPOLLFD, WSAPoll, WSASocketW,
    WSAStartup, accept, bind, connect, listen,
};

use super::win;
use crate::error::Error;

/// `sun_path` holds 108 bytes, the last a NUL.
const PATH_BUDGET: usize = 107;

/// The literal avoids a narrowing cast; the assertion catches SDK drift.
const ADDRESS_LEN: i32 = 110;
const _: () = assert!(size_of::<SOCKADDR_UN>() == 110);

/// A socket path proved to fit `sun_path` as UTF-8.
pub(super) struct Address {
    path: PathBuf,
    raw: SOCKADDR_UN,
}

impl Address {
    /// # Errors
    /// Returns [`Error::NoPlainSpelling`], [`Error::NotUnicode`] or [`Error::SocketPathTooLong`].
    pub(super) fn new(path: &Path) -> Result<Self, Error> {
        let path = win::plain(path)?;
        let text = win::document_text(&path)?;
        if text.len() > PATH_BUDGET {
            return Err(Error::SocketPathTooLong { path });
        }
        let mut raw = SOCKADDR_UN {
            sun_family: AF_UNIX,
            ..SOCKADDR_UN::default()
        };
        for (slot, byte) in raw.sun_path.iter_mut().zip(text.bytes()) {
            *slot = byte.cast_signed();
        }
        Ok(Self { path, raw })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    fn sockaddr(&self) -> *const SOCKADDR {
        (&raw const self.raw).cast()
    }
}

pub(super) struct Listener(OwnedSocket);

impl Listener {
    pub(super) fn bind(at: &Address) -> io::Result<Self> {
        let socket = stream()?;
        // SAFETY: a live socket and a live, NUL-terminated SOCKADDR_UN of ADDRESS_LEN bytes.
        if unsafe { bind(raw(&socket), at.sockaddr(), ADDRESS_LEN) } == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        // One dial is admitted per wire.
        // SAFETY: a live bound socket.
        if unsafe { listen(raw(&socket), 1) } == SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(socket))
    }

    /// # Errors
    /// Returns `TimedOut` when `patience` passes with no connection, or the socket error.
    pub(super) fn accept_within(&self, patience: Duration) -> io::Result<OwnedSocket> {
        let deadline = Instant::now() + patience;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "nothing connected before the deadline",
                ));
            }
            let mut poll = WSAPOLLFD {
                fd: raw(&self.0),
                events: POLLRDNORM,
                revents: 0,
            };
            let millis = i32::try_from(left.as_millis()).unwrap_or(i32::MAX);
            // SAFETY: one initialised descriptor, of which only `revents` is written.
            match unsafe { WSAPoll(&raw mut poll, 1, millis) } {
                SOCKET_ERROR => return Err(io::Error::last_os_error()),
                0 => continue,
                _ => {}
            }
            // SAFETY: a readable listener; null address outputs are permitted.
            let accepted =
                unsafe { accept(raw(&self.0), std::ptr::null_mut(), std::ptr::null_mut()) };
            if accepted == INVALID_SOCKET {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a fresh connected socket, owned from here on.
            return Ok(unsafe { adopt(accepted) });
        }
    }
}

pub(super) fn connect_to(at: &Address) -> io::Result<OwnedSocket> {
    let socket = stream()?;
    // SAFETY: a live socket and a live, NUL-terminated SOCKADDR_UN of ADDRESS_LEN bytes.
    if unsafe { connect(raw(&socket), at.sockaddr(), ADDRESS_LEN) } == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}

fn stream() -> io::Result<OwnedSocket> {
    winsock()?;
    // SAFETY: plain AF_UNIX stream arguments; null info selects the provider.
    let socket = unsafe {
        WSASocketW(
            i32::from(AF_UNIX),
            SOCK_STREAM,
            0,
            std::ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if socket == INVALID_SOCKET {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh socket, owned from here on.
    Ok(unsafe { adopt(socket) })
}

/// # Safety
/// `socket` is open and owned by nothing else.
unsafe fn adopt(socket: SOCKET) -> OwnedSocket {
    let socket = RawSocket::try_from(socket).expect("a SOCKET is pointer-sized");
    // SAFETY: the caller's.
    unsafe { OwnedSocket::from_raw_socket(socket) }
}

fn raw(socket: &OwnedSocket) -> SOCKET {
    SOCKET::try_from(socket.as_raw_socket()).expect("a SOCKET is pointer-sized")
}

/// Raw `WSASocketW` needs `WSAStartup`; its reference is kept for the process lifetime.
fn winsock() -> io::Result<()> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let code = *STARTED.get_or_init(|| {
        let mut data = WSADATA::default();
        // SAFETY: a writable WSADATA; 2.2 is the version every supported Windows offers.
        unsafe { WSAStartup(0x0202, &raw mut data) }
    });
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn a_path_past_the_budget_is_refused_and_one_at_it_is_not() {
        let at = |bytes: usize| PathBuf::from(format!(r"C:\{}", "a".repeat(bytes - 3)));
        assert!(Address::new(&at(PATH_BUDGET)).is_ok());
        assert!(matches!(
            Address::new(&at(PATH_BUDGET + 1)),
            Err(Error::SocketPathTooLong { path }) if path == at(PATH_BUDGET + 1)
        ));
    }

    #[test]
    fn a_listener_accepts_one_connection_and_times_out_on_none() {
        let dir = tempfile::tempdir().unwrap();
        let at = Address::new(&dir.path().join("v.sock_1729")).unwrap();
        let listener = Listener::bind(&at).unwrap();
        assert_eq!(
            listener
                .accept_within(Duration::from_millis(50))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let _client = connect_to(&at).unwrap();
        listener.accept_within(Duration::from_secs(5)).unwrap();
    }
}
