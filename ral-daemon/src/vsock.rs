//! The guest's one way off the machine, and the only `socket`/`connect` in
//! this daemon.
//!
//! The guest has no network device at all (`dev/docs/VM/SYNOD.md` §6): every
//! channel it has is an `AF_VSOCK` stream to `VMADDR_CID_HOST` on a port the
//! host named on the kernel command line.  Two of them exist, and they are
//! alike in everything except what happens to the connection afterwards:
//!
//! - the **control plane** ([`crate::engine::control_plane`]) hands its
//!   connection to the engine as file descriptor 3, where the frame protocol
//!   of `dev/docs/VM/SYNOD.md` §3 runs over it;
//! - the **workspace transport** ([`crate::mounts::Options::Plan9Dial`],
//!   when the host is a Hyper-V one with no virtiofs) hands its connection to
//!   the kernel, as the `trans=fd` transport of a 9p mount.
//!
//! In both cases the *guest* dials and the host listens, never the reverse.
//! That asymmetry is worth stating because it is what makes the daemon so
//! small: a connection that exists is already proof the host is there, so
//! there is no accept loop, no readiness handshake, and no listening port
//! inside the guest for anything to reach.
//!
//! The one thing here is therefore [`dial_host`], factored out of the two
//! callers so that the unsafe pair of syscalls is written once and read once.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// Open a stream to the host on `port`.
///
/// One attempt, no patience: retrying is a policy the caller owns, because
/// the two callers want different ones — the control plane waits out the
/// race with the host's listener, while a workspace mount that cannot reach
/// its 9p server has nothing to wait for.
///
/// # Errors
/// Returns the errno `socket(2)` or `connect(2)` gave, unadorned: the
/// sentence a user reads is the caller's to write, since only the caller
/// knows which channel was being opened and what its absence means.
pub(crate) fn dial_host(port: u32) -> io::Result<OwnedFd> {
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
