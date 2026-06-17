//! Sandbox child: receive a [`ChildEvalRequest`], evaluate it under the
//! shared runner, write one [`ChildEvalResponse`] back.
//!
//! [`serve_from_env_fd`] is the Unix child entry point — it adopts the
//! IPC fd from the environment and hands the socketpair halves to
//! [`serve`].  [`serve_from_env_handle`] mirrors it on Windows over the
//! inherited named-pipe handle.  Both delegate the transport-neutral
//! body — read one request, adopt the confinement token, run
//! [`run_child_eval`] with [`ChildKind::Sandbox`], write one response —
//! to the shared [`serve`].
//!
//! The OS sandbox is already entered in `early_init` (Unix self-entry /
//! bwrap respawn; Windows applied by the parent at
//! `CreateProcessAsUserW`) before this loop runs, so the child does no
//! sandbox setup — only eval.  The mobile carries the body's lexical
//! environment already (the contract layer shaped it before transport),
//! so the child does no scope manipulation either.

use crate::child_eval::{ChildEvalRequest, ChildKind, run_child_eval};
use crate::subprocess_codec::{Tokened, read_frame, write_frame};
use std::io::{BufReader, Read, Write};

/// Transport-neutral child body: read one request from `reader`, adopt
/// its confinement token, evaluate under [`ChildKind::Sandbox`], and
/// write the single response to `writer`.  Returns the `u8` process
/// exit code.
fn serve<R: Read, W: Write>(reader: R, mut writer: W) -> u8 {
    let mut reader = BufReader::new(reader);
    let request: ChildEvalRequest = match read_frame(&mut reader) {
        Ok(Some(req)) => req,
        Ok(None) => {
            crate::diagnostic::cmd_error(
                "ral",
                "sandbox ipc: parent closed before sending request",
            );
            return 1;
        }
        Err(e) => {
            // First-frame deserialise failure is almost always a
            // wire-format mismatch (stale binary vs. fresh build).
            crate::diagnostic::cmd_error(
                "ral",
                "sandbox subprocess: failed to decode request from parent — \
                 the parent and sandbox child binaries appear to have \
                 incompatible wire formats",
            );
            eprintln!("  cause: {e}");
            eprintln!(
                "  hint: rebuild ral/exarch from a clean checkout so parent \
                 and child are the same binary"
            );
            return 1;
        }
    };

    // Authenticate the confinement marker from the request the parent
    // delivered over this private channel.  Adopting the token before
    // eval means nested grants in this confined child dispatch locally
    // (the OS sandbox already holds) instead of re-entering confinement.
    if let Some(token) = &request.sandbox_token {
        super::super::adopt_sandbox_token(token);
    }
    // The same per-re-exec token stamps the response frame so the parent
    // can prove the reply came from this confined child and not from an
    // impostor on the channel.  A sandbox request always carries it.
    let token = request
        .sandbox_token
        .clone()
        .expect("sandbox re-exec request always carries a token");
    let (response, _) = run_child_eval(request, None, ChildKind::Sandbox);
    let framed = Tokened {
        token,
        inner: response,
    };
    if let Err(e) = write_frame(&mut writer, &framed) {
        crate::diagnostic::cmd_error("ral", &format!("sandbox ipc write: {e}"));
        return 1;
    }
    0
}

/// Child entry point: adopt the IPC fd from the environment, then serve
/// one request over the socketpair.
#[cfg(unix)]
pub fn serve_from_env_fd() -> u8 {
    let Some(fd) = std::env::var(super::transport::IPC_FD_ENV)
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
    else {
        crate::diagnostic::cmd_error(
            "ral",
            &format!("{} not set or not an fd", super::transport::IPC_FD_ENV),
        );
        return 1;
    };
    // Safety: the parent lent the fd inheritable across the spawn and is
    // the sole prior owner; this child now owns it.  `adopt` re-arms
    // CLOEXEC so the externals the grant body spawns cannot inherit a
    // live writable handle to the parent's private IPC endpoint.
    let endpoint = match unsafe { super::IpcEndpoint::adopt(fd) } {
        Ok(e) => e,
        Err(e) => {
            crate::diagnostic::cmd_error("ral", &format!("sandbox ipc adopt: {e}"));
            return 1;
        }
    };
    let (reader_stream, writer_stream) = match endpoint.streams() {
        Ok(pair) => pair,
        Err(e) => {
            crate::diagnostic::cmd_error("ral", &format!("sandbox ipc clone: {e}"));
            return 1;
        }
    };
    serve(reader_stream, writer_stream)
}

/// Windows child entry point: adopt the inherited pipe handle from the
/// environment, then serve one request over the named pipe.  Mirrors
/// `serve_from_env_fd` but uses a handle inherited via
/// [`super::IPC_HANDLE_ENV`].
#[cfg(windows)]
pub fn serve_from_env_handle() -> u8 {
    use std::os::windows::io::RawHandle;

    // Parent encoded the inherited HANDLE as a pointer-sized integer in
    // base-10.  Parse it back into a HANDLE.
    let Some(raw) = std::env::var(super::IPC_HANDLE_ENV)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    else {
        crate::diagnostic::cmd_error(
            "ral",
            &format!("{} not set or not a handle", super::IPC_HANDLE_ENV),
        );
        return 1;
    };

    // Safety: the parent lent the handle inheritable across the spawn and
    // is the sole prior owner; this child now owns it.  `adopt` clears
    // `HANDLE_FLAG_INHERIT` so the externals the grant body spawns cannot
    // inherit the parent's IPC channel.
    let endpoint = match unsafe { super::IpcEndpoint::adopt(raw as RawHandle) } {
        Ok(e) => e,
        Err(e) => {
            crate::diagnostic::cmd_error("ral", &format!("sandbox ipc adopt: {e}"));
            return 1;
        }
    };

    // The borrowed adapter is `Copy`, so the same handle backs both the
    // read half and the write half; `serve` reads to completion before
    // writing.  `endpoint` keeps the handle alive across the whole serve.
    let io = endpoint.pipe_io();
    serve(io, io)
}
