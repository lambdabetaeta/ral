//! The one `AF_VSOCK` endpoint exarch opens itself: an ephemeral guest port a
//! hatched agent's parent engine listens on for the duration of a single
//! spawn, so the host can dial in and reach the child before its roster names
//! it.
//!
//! Every other channel off this machine is dialled outward by `ral-daemon`,
//! which keeps the boot law — guest dials, host listens, so a connection that
//! exists is already proof the host is there. This is the one exchange that
//! runs the other way, which is why its port has to be asked for and named in
//! the enquiry: nothing published it in advance.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Deep enough that a stray dial ahead of the host's does not turn the
/// listener away; the exchange itself only ever wants one connection.
const BACKLOG: libc::c_int = 4;

/// Bind and listen on an ephemeral port of this guest, and answer with the
/// port the kernel chose — the caller has to tell the host where to dial.
///
/// # Errors
/// Returns a sentence naming the syscall that refused. By far the likeliest
/// cause is not being inside a VM at all, so the sentence asks.
pub(crate) fn bind() -> Result<(OwnedFd, u32), String> {
    listen_any().map_err(|e| {
        format!(
            "could not bind a guest port for the host to dial: {e} — is this engine running \
             inside a VM with a vsock device?"
        )
    })
}

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

/// Any port, any peer: what a listener binds before the kernel has chosen for
/// it, and what `getsockname(2)` overwrites with the choice.
///
/// # Panics
/// Never in practice: `AF_VSOCK` is a small positive constant, always a valid
/// `sa_family_t`.
fn unnamed_addr() -> libc::sockaddr_vm {
    libc::sockaddr_vm {
        svm_family: libc::sa_family_t::try_from(libc::AF_VSOCK)
            .expect("AF_VSOCK is a small positive address family"),
        svm_reserved1: 0,
        svm_port: libc::VMADDR_PORT_ANY,
        svm_cid: libc::VMADDR_CID_ANY,
        svm_zero: [0; 4],
    }
}

/// # Panics
/// Never: a socket address is far smaller than `socklen_t`'s range.
fn addr_len() -> libc::socklen_t {
    libc::socklen_t::try_from(size_of::<libc::sockaddr_vm>())
        .expect("a socket address is far smaller than socklen_t's range")
}

/// # Errors
/// Returns the errno `socket(2)`, `bind(2)`, `listen(2)` or `getsockname(2)`
/// gave, unadorned; [`bind`] writes the sentence over it.
fn listen_any() -> io::Result<(OwnedFd, u32)> {
    let socket = vsock_socket()?;
    let addr = unnamed_addr();
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

    let mut bound = unnamed_addr();
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
