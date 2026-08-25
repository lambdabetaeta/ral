//! The one place a Linux guest process opens a raw `AF_VSOCK` endpoint to the
//! host: `socket(2)` plus `connect(2)`, or `socket(2)` plus `bind`/`listen`.
//!
//! Two guests share it. `ral-daemon` dials with it three times — the control
//! plane, the workspace's 9p transport, the net wire — and those three keep
//! the boot law: guest dials, host listens, so a connection that exists is
//! already proof the host is there. One exchange runs the other way. When
//! this crate's own [`crate::hatch`] spawns a child engine, the parent
//! [`listen_any`]s for the duration of that one spawn and the host dials in,
//! so the child exists before the host's roster names it. That listener is
//! the one socket here whose address is asked for: nothing published its
//! ephemeral port in advance. A dialled socket is never asked — the daemon
//! holds each only as std's owner of a connected stream, the same pretence
//! [`crate::wire`] documents for [`crate::wire::WireStream`].

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Deep enough that a stray dial ahead of the host's does not turn the
/// listener away; the exchange itself only ever wants one connection.
const BACKLOG: libc::c_int = 4;

/// A fresh `AF_VSOCK` stream socket, owned.
fn vsock_socket() -> io::Result<OwnedFd> {
    // SAFETY: a plain `socket(2)`; the returned descriptor is adopted by an
    // `OwnedFd` immediately, so it is closed on every path out of here.
    let raw = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor this call owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// # Panics
/// Never in practice: `AF_VSOCK` is a small positive constant, always a
/// valid `sa_family_t`.
fn vsock_addr(cid: u32, port: u32) -> libc::sockaddr_vm {
    libc::sockaddr_vm {
        svm_family: libc::sa_family_t::try_from(libc::AF_VSOCK)
            .expect("AF_VSOCK is a small positive address family"),
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: cid,
        svm_zero: [0; 4],
    }
}

/// # Panics
/// Never: a socket address is far smaller than `socklen_t`'s range.
fn addr_len() -> libc::socklen_t {
    libc::socklen_t::try_from(size_of::<libc::sockaddr_vm>())
        .expect("a socket address is far smaller than socklen_t's range")
}

/// Open a stream to the host on `port`.
///
/// One attempt, no patience: retrying is a policy the caller owns, because
/// callers want different ones — a control plane waits out the race with the
/// host's listener, while a workspace mount that cannot reach its 9p server
/// has nothing to wait for.
///
/// # Errors
/// Returns the errno `socket(2)` or `connect(2)` gave, unadorned: the
/// sentence a user reads is the caller's to write, since only the caller
/// knows which channel was being opened and what its absence means.
pub fn dial_host(port: u32) -> io::Result<OwnedFd> {
    let socket = vsock_socket()?;
    let addr = vsock_addr(libc::VMADDR_CID_HOST, port);
    // SAFETY: `addr` is a fully initialised `sockaddr_vm` and the length is
    // its own size, which is what `connect(2)` reads.
    let rc = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            std::ptr::from_ref(&addr).cast(),
            addr_len(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}

/// Listen on an ephemeral port of this guest, and answer with the port the
/// kernel chose — the caller has to tell the host where to dial.
///
/// # Errors
/// Returns the errno `socket(2)`, `bind(2)`, `listen(2)` or `getsockname(2)`
/// gave, unadorned, for the same reason [`dial_host`] does.
pub fn listen_any() -> io::Result<(OwnedFd, u32)> {
    let socket = vsock_socket()?;
    let addr = vsock_addr(libc::VMADDR_CID_ANY, libc::VMADDR_PORT_ANY);
    // SAFETY: `addr` is a fully initialised `sockaddr_vm` and the length is
    // its own size, which is what `bind(2)` reads.
    let rc = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            std::ptr::from_ref(&addr).cast(),
            addr_len(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a plain `listen(2)` on a bound socket this call owns.
    if unsafe { libc::listen(socket.as_raw_fd(), BACKLOG) } < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut bound = vsock_addr(libc::VMADDR_CID_ANY, libc::VMADDR_PORT_ANY);
    let mut len = addr_len();
    // SAFETY: `bound` is a `sockaddr_vm` and `len` its size; `getsockname(2)`
    // writes at most that many bytes and updates `len` in place.
    let rc = unsafe {
        libc::getsockname(
            socket.as_raw_fd(),
            std::ptr::from_mut(&mut bound).cast(),
            &raw mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((socket, bound.svm_port))
}
