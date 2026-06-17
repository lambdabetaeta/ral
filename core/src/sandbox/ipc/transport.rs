//! IPC channel over a Unix socketpair.
//!
//! [`IpcChannel`] owns the parent side of the pair; the child side is an
//! [`IpcEndpoint`], confined at construction and lent inheritable only
//! across the spawn.
//!
//! [`IpcChannel::drive`] sends one request and reads back the single
//! [`ChildEvalResponse`] for the parent to fold into its shell, checking
//! the response's confinement token as it decodes.

use super::IpcEndpoint;
use crate::child_eval::{ChildEvalRequest, ChildEvalResponse};
use crate::types::Settled;
use std::io;

/// Shell var telling the child process which fd is the IPC endpoint.
pub const IPC_FD_ENV: &str = "RAL_SANDBOX_IPC_FD";

/// Parent side of the IPC socketpair.
pub struct IpcChannel {
    pub reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    pub writer: std::os::unix::net::UnixStream,
}

impl IpcChannel {
    /// Make a socketpair and return the parent half plus the child-side
    /// [`IpcEndpoint`], which is born CLOEXEC-set; inheritance is opened
    /// only transiently inside [`IpcEndpoint::lend_for_spawn`].
    pub fn open_pair() -> io::Result<(Self, IpcEndpoint)> {
        use std::os::unix::io::IntoRawFd;
        use std::os::unix::net::UnixStream;
        let (parent, child) = UnixStream::pair()?;
        // Safety: `child` is the freshly-made socketpair half this process
        // solely owns; `into_raw_fd` transfers that ownership to the
        // endpoint, whose constructor re-arms CLOEXEC.
        let endpoint = unsafe { IpcEndpoint::adopt(child.into_raw_fd())? };
        let reader = std::io::BufReader::new(parent.try_clone()?);
        Ok((
            IpcChannel {
                reader,
                writer: parent,
            },
            endpoint,
        ))
    }

    /// Send the eval request and read back the child's single response,
    /// rejecting any frame whose confinement token does not match the
    /// one minted for this re-exec.
    pub fn drive(mut self, request: &ChildEvalRequest) -> Settled<ChildEvalResponse> {
        super::drive_exchange(&mut self.writer, &mut self.reader, request)
    }
}
