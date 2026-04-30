//! IPC channel over a Unix socketpair.
//!
//! [`IpcChannel`] owns the parent side of the pair.  [`ChildFd`] is an
//! RAII holder for the child-side fd: pass it to the subprocess's
//! environment, then drop it once the child is spawned so the parent's
//! EOF signal propagates cleanly when the child exits.
//!
//! [`IpcChannel::drive`] sends one request, drains streaming `Audit`
//! frames, and returns the final response alongside all audit frames
//! for the parent to materialise into its audit tree.

use super::codec::{read_frame, write_frame};
use super::wire::{ChildFrame, SandboxedBlockRequest, SandboxedBlockResponse};
use crate::serial::SerialValue;
use crate::types::{Error, EvalSignal};
use std::io;

/// Shell var telling the child process which fd is the IPC endpoint.
pub const IPC_FD_ENV: &str = "RAL_SANDBOX_IPC_FD";

/// Opaque RAII holder for the child-side socket fd.
///
/// The caller writes its number into the child's environment, spawns,
/// then drops this so the parent's copy closes and EOF propagates
/// cleanly when the child exits.
#[cfg(unix)]
pub struct ChildFd(pub std::os::unix::net::UnixStream);

#[cfg(unix)]
impl ChildFd {
    /// The raw fd number to place in the child's environment.
    pub fn as_raw(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        self.0.as_raw_fd()
    }
}

/// One audit frame as it crossed the wire.
///
/// Paired with the scope table it interns against so a single
/// `build_arcs` call can rehydrate the node for the parent's audit
/// tree.
#[cfg(unix)]
pub struct AuditFrame {
    pub scope_table: Vec<Vec<(String, SerialValue)>>,
    pub node: super::wire::IpcExecNode,
}

/// Parent side of the IPC socketpair.
#[cfg(unix)]
pub struct IpcChannel {
    pub reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    pub writer: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
impl IpcChannel {
    /// Make a socketpair, clear `CLOEXEC` on the child end so it
    /// survives fork/exec and bwrap's inner exec, and return the parent
    /// half plus an RAII holder for the child end.
    pub fn open_pair() -> io::Result<(Self, ChildFd)> {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::net::UnixStream;
        let (parent, child) = UnixStream::pair()?;
        let fd = child.as_raw_fd();
        // Safety: fcntl with F_GETFD/F_SETFD on a valid fd is a narrow
        // straight-line op; we only observe its return to skip the set
        // on error.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags != -1 {
                libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
        }
        let reader = std::io::BufReader::new(parent.try_clone()?);
        Ok((IpcChannel { reader, writer: parent }, ChildFd(child)))
    }

    /// Send the request, drain every audit frame the child emits, and
    /// return the audit frames alongside the body's final response.
    ///
    /// The child writes audit frames only after evaluation finishes
    /// (it drains its own `audit.tree` post-eval; see `child.rs`), so
    /// buffering on the parent side does not lose any concurrency that
    /// could have existed.
    pub fn drive(
        mut self,
        request: &SandboxedBlockRequest,
    ) -> Result<(Vec<AuditFrame>, SandboxedBlockResponse), EvalSignal> {
        let io_err =
            |e: io::Error| EvalSignal::Error(Error::new(format!("grant sandbox: ipc: {e}"), 1));
        write_frame(&mut self.writer, request).map_err(io_err)?;
        let mut audit = Vec::new();
        loop {
            match read_frame::<_, ChildFrame>(&mut self.reader).map_err(io_err)? {
                Some(ChildFrame::Audit { scope_table, node }) => {
                    audit.push(AuditFrame { scope_table, node: *node });
                }
                Some(ChildFrame::Final(resp)) => return Ok((audit, resp)),
                None => {
                    return Err(EvalSignal::Error(Error::new(
                        "grant sandbox: subprocess closed before Final frame",
                        1,
                    )));
                }
            }
        }
    }
}
