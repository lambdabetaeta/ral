//! The guest dials control, workspace and net streams; only the engine factory listens.
//! Factory peers must have the host CID; PID 1 shares the ABI without linking the engine runtime.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

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

fn addr_len() -> libc::socklen_t {
    libc::socklen_t::try_from(size_of::<libc::sockaddr_vm>())
        .expect("a socket address is far smaller than socklen_t's range")
}

/// Retry and diagnostic policy belong to each channel's caller.
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

/// Only `VMADDR_CID_HOST` is admitted.
fn is_host(cid: u32) -> bool {
    cid == libc::VMADDR_CID_HOST
}

/// Drop every non-host connection; retry EINTR.
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

/// Match both buffer requests to the 9p msize; Linux may double and clamp them.
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
