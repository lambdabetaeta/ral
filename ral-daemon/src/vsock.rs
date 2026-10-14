//! The guest's one way off the machine.
//!
//! Neither hypervisor gives this guest a network adapter: every channel it
//! has is an `AF_VSOCK` stream to `VMADDR_CID_HOST` on a port the host named
//! on the kernel command line.  Three of them exist, and they are alike in
//! everything except what happens to the connection afterwards:
//!
//! - the **control plane** ([`crate::engine::control_plane`]) hands its
//!   connection to the engine as file descriptor 3, where the host's typed
//!   frame protocol runs over it;
//! - the **workspace transport** ([`crate::mounts::Options::Plan9Dial`],
//!   when the host is a Hyper-V one with no virtiofs) hands its connection to
//!   the kernel, as the `trans=fd` transport of a 9p mount;
//! - the **net wire** ([`crate::pump`]) keeps its connection in the packet
//!   pump, which carries the `tun`'s IP to and from the user-mode TCP/IP
//!   stack that is the guest's entire network.
//!
//! In all three the *guest* dials and the host listens.  That asymmetry is
//! what makes the daemon so small: a connection that exists is already proof
//! the host is there, so there is no readiness handshake on any of them.  The
//! daemon binds exactly one guest port, the engine factory
//! ([`crate::boot::FACTORY_PORT`]), and that is the one channel the host
//! opens; the peer-CID check in [`accept_from_host`] is what makes a standing
//! listener safe.
//!
//! The `socket`/`connect` pair below is written out here rather than borrowed
//! from the engine's own copy, for the reason [`crate::reap`] gives about
//! `waitpid`: PID 1 is a static binary in a boot artifact, and a
//! `sockaddr_vm` is a struct layout out of the kernel's headers, not a
//! doctrine anyone owns.  The vocabulary is shared; the dependency is not.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

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

/// The `sockaddr_vm` for `cid` and `port`.
///
/// # Panics
/// Never in practice: `AF_VSOCK` is a small positive constant, always a valid
/// `sa_family_t`.
fn addr(cid: u32, port: u32) -> libc::sockaddr_vm {
    libc::sockaddr_vm {
        svm_family: libc::sa_family_t::try_from(libc::AF_VSOCK)
            .expect("AF_VSOCK is a small positive address family"),
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: cid,
        svm_zero: [0; 4],
    }
}

/// The size of a `sockaddr_vm`.
///
/// # Panics
/// Never: a socket address is far smaller than `socklen_t`'s range.
fn addr_len() -> libc::socklen_t {
    libc::socklen_t::try_from(size_of::<libc::sockaddr_vm>())
        .expect("a socket address is far smaller than socklen_t's range")
}

/// Open a stream to the host on `port`.
///
/// One attempt, no patience: retrying is a policy the caller owns, because
/// callers want different ones — the control plane waits out the race with the
/// host's listener, while a workspace mount that cannot reach its 9p server
/// has nothing to wait for.
///
/// # Errors
/// Returns the errno `socket(2)` or `connect(2)` gave, unadorned: the sentence
/// a user reads is the caller's to write, since only the caller knows which
/// channel was being opened and what its absence means.
pub(crate) fn dial_host(port: u32) -> io::Result<OwnedFd> {
    let socket = vsock_socket()?;
    let addr = addr(libc::VMADDR_CID_HOST, port);
    let len = addr_len();
    // SAFETY: `addr` is a fully initialised `sockaddr_vm` and `len` is its own
    // size, which is what `connect(2)` reads.
    let rc = unsafe { libc::connect(socket.as_raw_fd(), std::ptr::from_ref(&addr).cast(), len) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}

/// Bind and listen on `port` of this guest, any peer, `SOCK_CLOEXEC`.
///
/// # Errors
/// Returns the errno `socket(2)`, `bind(2)` or `listen(2)` gave.
pub(crate) fn listen(port: u32) -> io::Result<OwnedFd> {
    let socket = vsock_socket()?;
    let addr = addr(libc::VMADDR_CID_ANY, port);
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
    // SAFETY: a plain `listen(2)` on a descriptor this call owns.
    if unsafe { libc::listen(socket.as_raw_fd(), libc::SOMAXCONN) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}

/// The one admission rule, as data: only `VMADDR_CID_HOST` is the host.
fn is_host(cid: u32) -> bool {
    cid == libc::VMADDR_CID_HOST
}

/// Accept one connection; `None` if the peer was not the host, whose
/// connection is then dropped. Retries `EINTR`.
///
/// # Errors
/// Returns the errno `accept4(2)` gave.
pub(crate) fn accept_from_host(listener: &OwnedFd) -> io::Result<Option<OwnedFd>> {
    loop {
        let mut peer: libc::sockaddr_vm = addr(0, 0);
        let mut len = addr_len();
        // SAFETY: `peer` and `len` are live out parameters of the size
        // `accept4(2)` is told.
        let raw = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::from_mut(&mut peer).cast(),
                &raw mut len,
                libc::SOCK_CLOEXEC,
            )
        };
        if raw < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        // SAFETY: `raw` is a fresh descriptor this call owns.
        let conn = unsafe { OwnedFd::from_raw_fd(raw) };
        return Ok(is_host(peer.svm_cid).then_some(conn));
    }
}

/// Ask the kernel for `bytes` of send and receive buffer on `socket`.
///
/// Only the 9p transport needs this, and it needs it for a concrete reason:
/// the mount's `msize` is the largest 9p message the two ends will exchange,
/// and a socket whose buffers are smaller than that turns every message into
/// several round trips through a full buffer.  Microsoft's LCOW guest agent
/// sets both to the same 65536 it mounts with (`microsoft/hcsshim`,
/// `internal/guest/storage/plan9`), and the guest side of a share the host
/// serves has no reason to differ.
///
/// Both options are advisory: Linux doubles the value for bookkeeping and
/// clamps it to `net.core.{r,w}mem_max`, so the buffer that results is a
/// request granted approximately, not a promise.
///
/// # Errors
/// Returns the errno `setsockopt(2)` gave, or `InvalidInput` when `bytes`
/// exceeds what the kernel's `int`-typed option can carry.
pub(crate) fn set_buffer_sizes(socket: BorrowedFd<'_>, bytes: u32) -> io::Result<()> {
    let size = libc::c_int::try_from(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a socket buffer of {bytes} bytes does not fit setsockopt's int"),
        )
    })?;
    let len = libc::socklen_t::try_from(size_of::<libc::c_int>())
        .expect("an int is far smaller than socklen_t's range");
    for option in [libc::SO_RCVBUF, libc::SO_SNDBUF] {
        // SAFETY: `size` is a live `c_int` and `len` is its own size, which
        // is what `setsockopt(2)` reads for both of these options.
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                std::ptr::from_ref(&size).cast(),
                len,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_host_is_the_host() {
        assert!(is_host(libc::VMADDR_CID_HOST));
        for cid in [libc::VMADDR_CID_LOCAL, libc::VMADDR_CID_ANY, 3] {
            assert!(!is_host(cid), "cid {cid} is not the host");
        }
    }
}
