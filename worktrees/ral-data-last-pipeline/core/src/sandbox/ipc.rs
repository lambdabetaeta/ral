//! IPC layer for the sandboxed `grant { }` subprocess.
//!
//! The parent ral process and the sandbox child communicate over a
//! socketpair fd inherited across `bwrap`.  Messages are length-prefixed
//! JSON frames: one `SandboxedBlockRequest` from parent to child, then a
//! stream of `ChildFrame::Audit` events followed by a single
//! `ChildFrame::Final` carrying the outcome.
//!
//! Module layout:
//! - `wire`      — all `Serialize + Deserialize` message types and their
//!                 runtime ↔ wire conversions.
//! - `codec`     — `write_frame` / `read_frame` (length-prefixed JSON).
//! - `transport` — `IpcChannel`, `ChildFd`, `AuditFrame`, socket setup.
//! - `child`     — child entry point `serve_from_env_fd`, `run_request`.

mod wire;
mod codec;
mod transport;
mod child;

// ── Re-exports for `runner.rs` ────────────────────────────────────────────

pub(super) use wire::{SandboxedBlockRequest, SandboxedBlockResponse, pack};
pub(super) use transport::{IPC_FD_ENV, AuditFrame, IpcChannel};
#[cfg(unix)]
pub(super) use child::serve_from_env_fd;
