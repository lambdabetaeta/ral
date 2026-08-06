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
//! In both cases the *guest* dials and the host listens, never the reverse.
//! That asymmetry is worth stating because it is what makes the daemon so
//! small: a connection that exists is already proof the host is there, so
//! there is no accept loop, no readiness handshake, and no listening port
//! inside the guest for anything to reach.
//!
//! The dial itself — the one `socket`/`connect` pair every caller above
//! shares — lives in [`ral_core::vsock::dial_host`], not here: the engine
//! this daemon starts dials the same host from inside its own guest, over
//! the same `AF_VSOCK` port space, to hatch a child engine (`ral_core::hatch`),
//! so the primitive moved to where both processes can reach it.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

pub(crate) use ral_core::vsock::dial_host;

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
