//! IPC layer for the sandbox-confined eval transport.
//!
//! The parent ral process and the sandbox child communicate over a
//! kernel transport — a socketpair fd on Unix, a named pipe on
//! Windows.  Messages are length-prefixed JSON frames: one
//! [`ChildEvalRequest`](crate::child_eval::ChildEvalRequest) from
//! parent to child, then one
//! [`ChildEvalResponse`](crate::child_eval::ChildEvalResponse) back.
//! The transport is body-shape agnostic — it ships a `Comp` plus a
//! [`crate::subprocess::WireMobile`] snapshot; the child runs the
//! body against a fresh shell installed from that mobile.
//!
//! Module layout:
//! - `child` — the OS-specific child entry points
//!   (`serve_from_env_fd` on Unix, `serve_from_env_handle` on Windows)
//!   that adopt the inherited transport endpoint and run one shared
//!   eval via [`crate::child_eval::run_child_eval`].
//! - `endpoint` — `IpcEndpoint`, the child-side transport half confined
//!   at construction and lent inheritable only across a spawn.
//! - `transport` — Unix `IpcChannel` over a socketpair.
//! - `transport_windows` — Windows `IpcChannel` over a named pipe.

mod child;
mod endpoint;
#[cfg(unix)]
mod transport;
#[cfg(windows)]
mod transport_windows;

use crate::child_eval::{ChildEvalRequest, ChildEvalResponse};
use crate::subprocess_codec::{ExpectToken, read_frame_seeded, write_frame};
use crate::types::{Break, Error, Settled};
use std::io::{Read, Write};

/// Parent side of one request/response exchange, shared by both
/// transports: write the request frame, then read back the single
/// response, rejecting any frame whose confinement token does not match
/// the one minted for this re-exec.  A subprocess that closes before
/// replying is reported as a failure rather than a missing frame.
pub(super) fn drive_exchange(
    writer: &mut impl Write,
    reader: &mut impl Read,
    request: &ChildEvalRequest,
) -> Settled<ChildEvalResponse> {
    let io_err = |e: std::io::Error| Break::Error(Error::new(format!("sandbox eval: ipc: {e}"), 1));
    write_frame(writer, request).map_err(io_err)?;
    let token = request
        .sandbox_token
        .as_deref()
        .expect("sandbox re-exec request always carries a token");
    let seed = ExpectToken::<ChildEvalResponse>::new(token);
    match read_frame_seeded(reader, seed).map_err(io_err)? {
        Some(response) => Ok(response),
        None => Err(Break::Error(Error::new(
            "sandbox eval: subprocess closed before sending a response",
            1,
        ))),
    }
}

// ── Re-exports for `runner.rs` ────────────────────────────────────────────

#[cfg(unix)]
pub(super) use child::serve_from_env_fd;
#[cfg(windows)]
pub(super) use child::serve_from_env_handle;
pub(super) use endpoint::IpcEndpoint;
#[cfg(unix)]
pub(super) use transport::{IPC_FD_ENV, IpcChannel};
#[cfg(windows)]
pub(super) use transport_windows::{IPC_HANDLE_ENV, IpcChannel};
