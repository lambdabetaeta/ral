//! Sandbox child: receive a request, evaluate it, stream frames back.
//!
//! [`serve_from_env_fd`] is the child entry point — it adopts the IPC
//! fd from the environment, reads one `SandboxedBlockRequest`, streams
//! `ChildFrame::Audit` events for each capability-check / exec node the
//! body produced, and sends one `ChildFrame::Final` with the outcome.
//!
//! [`run_request`] and [`eval_request`] separate the I/O loop from the
//! evaluation so each can be tested independently.

use super::codec::{read_frame, write_frame};
use super::wire::{
    ChildFrame, IpcExecNode, SandboxedBlockRequest, SandboxedBlockResponse,
};
use crate::evaluator::eval_comp;
use crate::serial::{InternCtx, SerialValue, build_arcs};
use crate::types::{EvalSignal, ExecNode, Shell, Value};
use std::io::{self, Write};

/// Map an `EvalSignal` back to the wire response.
///
/// A `TailCall` at the IPC boundary is a bug — the body must have
/// returned to a normal value or an error before reaching here.
fn signal_to_response(signal: EvalSignal) -> SandboxedBlockResponse {
    match signal {
        EvalSignal::Exit(code) => SandboxedBlockResponse::Exit { code },
        EvalSignal::Error(e) => SandboxedBlockResponse::Error {
            message: e.message,
            status: e.status,
            hint: e.hint,
        },
        EvalSignal::TailCall { .. } => SandboxedBlockResponse::Error {
            message: "sandboxed block returned unexpected tail call".into(),
            status: 1,
            hint: None,
        },
    }
}

/// Internal result of evaluating one request.
struct EvalOutcome {
    exec_nodes: Vec<ExecNode>,
    result: Result<Value, EvalSignal>,
    last_status: i32,
}

fn eval_request(request: SandboxedBlockRequest) -> Result<EvalOutcome, EvalSignal> {
    let SandboxedBlockRequest {
        scope_table,
        body,
        captured,
        ambient,
        registry,
        modules,
        loc,
        script_args,
        audit,
    } = request;

    let arcs = build_arcs(&scope_table)?;

    let mut outer = Shell::new(Default::default());
    outer.dynamic.replace_env_vars(ambient.env_vars);
    outer.dynamic.cwd = ambient.cwd;
    outer.dynamic.capabilities_stack = ambient.capabilities_stack;
    outer.dynamic.script_args = script_args;
    outer.location = loc;
    registry.install_into(&mut outer, &arcs)?;
    modules.install_into(&mut outer, &arcs)?;
    outer.audit.tree = if audit { Some(Vec::new()) } else { None };

    let captured = captured.into_runtime(&arcs)?;
    let mut child = Shell::child_of(&captured, &mut outer);
    crate::dbg_trace!(
        "sandbox-ipc",
        "pre-eval: audit.tree={:?}",
        child.audit.tree.as_ref().map(|t| t.len())
    );
    let result = eval_comp(&body, &mut child);
    let exec_nodes = child.audit.tree.take().unwrap_or_default();
    crate::dbg_trace!(
        "sandbox-ipc",
        "post-eval: result_ok={} exec_nodes={}",
        result.is_ok(),
        exec_nodes.len()
    );
    child.return_to(&mut outer);
    Ok(EvalOutcome {
        exec_nodes,
        result,
        last_status: outer.control.last_status,
    })
}

/// Evaluate `request` and stream results over `frames`: one `Audit`
/// frame per capability-check / exec node the body produced, then one
/// `Final` frame with the outcome.  Any I/O failure on `frames` is
/// returned; evaluation errors surface as `Final(Error)` rather than
/// propagating out.
pub fn run_request<W: Write>(
    request: SandboxedBlockRequest,
    mut frames: W,
) -> io::Result<()> {
    let outcome = match eval_request(request) {
        Ok(outcome) => outcome,
        Err(err) => {
            return write_frame(&mut frames, &ChildFrame::Final(signal_to_response(err)));
        }
    };
    for node in outcome.exec_nodes {
        let mut ctx = InternCtx::new();
        let ipc_node = IpcExecNode::from_runtime(node, &mut ctx)
            .map_err(|e| io::Error::other(format!("sandbox: audit node: {e}")))?;
        write_frame(
            &mut frames,
            &ChildFrame::Audit {
                scope_table: ctx.scope_table,
                node: Box::new(ipc_node),
            },
        )?;
    }
    let response = match outcome.result {
        Ok(value) => {
            let mut ctx = InternCtx::new();
            let serial_value = SerialValue::from_runtime(&value, &mut ctx)
                .map_err(|e| io::Error::other(format!("sandbox: response value: {e}")))?;
            SandboxedBlockResponse::Ok {
                scope_table: ctx.scope_table,
                value: serial_value,
                last_status: outcome.last_status,
            }
        }
        Err(signal) => signal_to_response(signal),
    };
    write_frame(&mut frames, &ChildFrame::Final(response))
}

/// Child entry point: adopt the IPC fd from the environment, read the
/// request, stream audit + final frames back, return an exit code.
#[cfg(unix)]
pub fn serve_from_env_fd() -> std::process::ExitCode {
    use std::io::BufReader;
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;
    use std::process::ExitCode;

    let Some(fd) = std::env::var(super::transport::IPC_FD_ENV)
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
    else {
        crate::diagnostic::cmd_error("ral", &format!("{} not set or not an fd", super::transport::IPC_FD_ENV));
        return ExitCode::from(1);
    };
    // Safety: parent armed the fd with CLOEXEC off and handed off
    // ownership by dropping its ChildFd after spawn.  We are the sole
    // owner in this process.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            crate::diagnostic::cmd_error("ral", &format!("sandbox ipc clone: {e}"));
            return ExitCode::from(1);
        }
    };
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;

    let request: SandboxedBlockRequest = match read_frame(&mut reader) {
        Ok(Some(req)) => req,
        Ok(None) => {
            crate::diagnostic::cmd_error(
                "ral",
                "sandbox ipc: parent closed before sending request",
            );
            return ExitCode::from(1);
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
            return ExitCode::from(1);
        }
    };

    if let Err(e) = run_request(request, &mut writer) {
        crate::diagnostic::cmd_error("ral", &format!("sandbox ipc write: {e}"));
        return ExitCode::from(1);
    }
    ExitCode::from(0)
}
