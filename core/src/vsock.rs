//! The one place a Linux guest process opens a raw `AF_VSOCK` connection to
//! the host: `socket(2)` + `connect(2)`, nothing else.
//!
//! Two guests share it. `ral-daemon` dials with it three times — the control
//! plane, the workspace's 9p transport, the net wire — always guest-dials,
//! host-listens, so a connection that exists is already proof the host is
//! there. This crate's own [`crate::hatch`] dials with it once more, from
//! inside a booted engine that is itself the guest, to reach the host's
//! agent port for a spawned child engine. Neither caller ever asks the
//! resulting socket for its address; both hold it only as std's owner of a
//! connected stream, the same pretence [`crate::wire`] documents for
//! [`crate::wire::WireStream`].

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

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
///
/// # Panics
/// Never in practice: `AF_VSOCK` is a small positive constant, always a
/// valid `sa_family_t`.
pub fn dial_host(port: u32) -> io::Result<OwnedFd> {
    // SAFETY: a plain `socket(2)`; the returned descriptor is adopted by an
    // `OwnedFd` immediately, so it is closed on every path out of here.
    let raw = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor this call owns.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };

    let addr = libc::sockaddr_vm {
        svm_family: libc::sa_family_t::try_from(libc::AF_VSOCK)
            .expect("AF_VSOCK is a small positive address family"),
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: libc::VMADDR_CID_HOST,
        svm_zero: [0; 4],
    };
    let len = libc::socklen_t::try_from(size_of::<libc::sockaddr_vm>())
        .expect("a socket address is far smaller than socklen_t's range");
    // SAFETY: `addr` is a fully initialised `sockaddr_vm` and `len` is its
    // own size, which is what `connect(2)` reads.
    let rc = unsafe { libc::connect(socket.as_raw_fd(), std::ptr::from_ref(&addr).cast(), len) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}
