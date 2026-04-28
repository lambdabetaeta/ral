//! IPC layer for the sandboxed `grant { }` subprocess.
//!
//! The parent ral process and the sandbox child communicate over a
//! socketpair fd inherited across `bwrap`.  Messages are length-prefixed
//! JSON frames: one `SandboxedBlockRequest` from parent to child, then a
//! stream of `ChildFrame::Audit` events (one per capability-check /
//! command the body produced) followed by a single `ChildFrame::Final`
//! carrying the outcome.
//!
//! The streamed audit channel lets the parent observe capability checks
//! independently of whether the body succeeded; a denied write emits its
//! audit node and then errors without losing the record.  A mid-body
//! SIGKILL still delivers every frame that made it into the socket
//! buffer before the kill.
//!
//! Wire fields mirror the Shell sub-struct layout (ambient, registry,
//! modules, loc) so parent/child conversion stays symmetric with
//! `inherit_from`.  `Value`-bearing fields go through `crate::serial`
//! which interns shared `Arc` scopes so closure capture costs
//! O(unique) not O(referenced).

use crate::evaluator::eval_comp;
use crate::ir::Comp;
use crate::serial::{InternCtx, SerialEnvSnapshot, SerialValue, build_arcs};
use crate::types::{
    AliasEntry, Shell, Env, Error, EvalSignal, ExecNode, ExecNodeKind, Value,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, Read, Write};

// ── Wire types ────────────────────────────────────────────────────────────
//
// Types that don't carry `Value` get Serialize/Deserialize directly on
// the runtime definition; wire mirrors here are only for types that
// need `SerialValue` + the shared `scope_table` to dedup Arc<scope>
// sharing.

/// Plugin-registered aliases — wire form of `types::Registry`.  Loaded
/// plugins themselves are not carried across: plugin bodies capture
/// runtime thunks and the subprocess reconstructs aliases from the wire.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IpcRegistry {
    aliases: Vec<(String, SerialValue, Option<String>)>,
}

/// Module-loader state — wire form of `types::Modules`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct IpcModules {
    cache: Vec<(String, SerialValue)>,
    stack: Vec<String>,
    depth: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct IpcExecNode {
    kind: String,
    cmd: String,
    args: Vec<String>,
    status: i32,
    script: String,
    line: usize,
    col: usize,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    value: SerialValue,
    children: Vec<IpcExecNode>,
    start: i64,
    end: i64,
    principal: String,
}

/// Wire mirror of the ambient cluster of `Dynamic`: env_vars, cwd,
/// capabilities_stack.  Excludes `handler_stack` (Value thunks not
/// transmissible) and `script_args` (separate wire field).  Constructed
/// by `pack` from `shell.dynamic`; reconstructed into `outer.dynamic`
/// in `eval_request`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct IpcAmbient {
    pub env_vars: std::collections::HashMap<String, String>,
    pub cwd: Option<std::path::PathBuf>,
    pub capabilities_stack: Vec<crate::types::Capabilities>,
}

impl IpcAmbient {
    fn from_dynamic(d: &crate::types::Dynamic) -> Self {
        Self {
            env_vars: d.env_vars.clone(),
            cwd: d.cwd.clone(),
            capabilities_stack: d.capabilities_stack.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SandboxedBlockRequest {
    /// Interned scope table shared across every `SerialValue` /
    /// `SerialEnvSnapshot` in this request.  See `crate::serial::InternCtx`.
    scope_table: Vec<Vec<(String, SerialValue)>>,
    body: Comp,
    captured: SerialEnvSnapshot,
    /// Ambient cluster of the runtime `Dynamic` — env_vars, cwd, and
    /// capabilities_stack.  Excludes `handler_stack` (within-handlers
    /// capture `Value` thunks whose scopes the wire can't easily carry,
    /// and the subprocess has no effect handlers to dispatch to anyway)
    /// and `script_args` (packed as a separate wire field below).
    ///
    /// Lives in this module as the wire-format mirror; `Dynamic` itself
    /// is not `Serialize`.
    ambient: IpcAmbient,
    registry: IpcRegistry,
    modules: IpcModules,
    loc: crate::types::Location,
    script_args: Vec<String>,
    pipe_value: Option<SerialValue>,
    audit: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum SandboxedBlockResponse {
    Ok {
        scope_table: Vec<Vec<(String, SerialValue)>>,
        value: SerialValue,
        last_status: i32,
    },
    Exit {
        code: i32,
    },
    Error {
        message: String,
        status: i32,
        hint: Option<String>,
    },
}

/// One message from the child to the parent.  Audit frames stream
/// eagerly; `Final` terminates the session and carries the body's
/// outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum ChildFrame {
    Audit {
        scope_table: Vec<Vec<(String, SerialValue)>>,
        node: Box<IpcExecNode>,
    },
    Final(SandboxedBlockResponse),
}

// ── IpcExecNode conversions ───────────────────────────────────────────────

impl IpcExecNode {
    fn from_runtime(node: ExecNode, ctx: &mut InternCtx) -> Result<Self, EvalSignal> {
        let mut children = Vec::with_capacity(node.children.len());
        for child in node.children {
            children.push(IpcExecNode::from_runtime(child, ctx)?);
        }
        Ok(Self {
            kind: node.kind.to_string(),
            cmd: node.cmd,
            args: node.args,
            status: node.status,
            script: node.script,
            line: node.line,
            col: node.col,
            stdout: node.stdout,
            stderr: node.stderr,
            value: SerialValue::from_runtime(&node.value, ctx)?,
            children,
            start: node.start,
            end: node.end,
            principal: node.principal,
        })
    }

    pub(super) fn into_runtime(
        self,
        arcs: &[Option<std::sync::Arc<HashMap<String, Value>>>],
    ) -> Result<ExecNode, EvalSignal> {
        let mut children = Vec::with_capacity(self.children.len());
        for child in self.children {
            children.push(child.into_runtime(arcs)?);
        }
        Ok(ExecNode {
            kind: if self.kind == "capability-check" {
                ExecNodeKind::CapabilityCheck
            } else {
                ExecNodeKind::Command
            },
            cmd: self.cmd,
            args: self.args,
            status: self.status,
            script: self.script,
            line: self.line,
            col: self.col,
            stdout: self.stdout,
            stderr: self.stderr,
            value: self.value.into_runtime(arcs)?,
            children,
            start: self.start,
            end: self.end,
            principal: self.principal,
        })
    }
}

// ── Registry / Modules conversions ───────────────────────────────────────

impl IpcRegistry {
    fn from_runtime(shell: &Shell, ctx: &mut InternCtx) -> Result<Self, EvalSignal> {
        let mut aliases = Vec::with_capacity(shell.registry.aliases.len());
        for (name, entry) in shell.registry.aliases.iter() {
            aliases.push((
                name.clone(),
                SerialValue::from_runtime(&entry.value, ctx)?,
                entry.source.clone(),
            ));
        }
        Ok(Self { aliases })
    }

    fn install_into(
        self,
        shell: &mut Shell,
        arcs: &[Option<std::sync::Arc<HashMap<String, Value>>>],
    ) -> Result<(), EvalSignal> {
        for (name, value, source) in self.aliases {
            let value = value.into_runtime(arcs)?;
            let entry = match source {
                Some(src) => AliasEntry::with_source(value, src),
                None => AliasEntry::new(value),
            };
            shell.registry.aliases.insert(name, entry);
        }
        Ok(())
    }
}

impl IpcModules {
    fn from_runtime(shell: &Shell, ctx: &mut InternCtx) -> Result<Self, EvalSignal> {
        let mut cache = Vec::with_capacity(shell.modules.cache.len());
        for (name, value) in shell.modules.cache.iter() {
            cache.push((name.clone(), SerialValue::from_runtime(value, ctx)?));
        }
        Ok(Self {
            cache,
            stack: shell.modules.stack.clone(),
            depth: shell.modules.depth,
        })
    }

    fn install_into(
        self,
        shell: &mut Shell,
        arcs: &[Option<std::sync::Arc<HashMap<String, Value>>>],
    ) -> Result<(), EvalSignal> {
        for (name, value) in self.cache {
            shell.modules.cache.insert(name, value.into_runtime(arcs)?);
        }
        shell.modules.stack = self.stack;
        shell.modules.depth = self.depth;
        Ok(())
    }
}

// ── Frame transport ──────────────────────────────────────────────────────
//
// Every message is a 4-byte little-endian length followed by that many
// bytes of serde_json.  Reading the length returns Ok(None) on a clean
// EOF at message boundary; any other partial read is an error.

fn write_frame<W: Write, T: Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    let len =
        u32::try_from(bytes.len()).map_err(|_| io::Error::other("sandbox: frame exceeds 4 GiB"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len_buf[got..])? {
            0 if got == 0 => return Ok(None),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "sandbox: partial frame length",
                ));
            }
            n => got += n,
        }
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    match serde_json::from_slice(&body) {
        Ok(value) => Ok(Some(value)),
        Err(e) => {
            // Best-effort dump for diagnosing wire-format mismatches:
            // write the raw bytes to a unique tmpfile and include the
            // path in the error so the user can ship the slice that
            // failed.  Silent on dump failure — we still want the
            // serde error to surface.
            let path = std::env::temp_dir().join(format!(
                "ral-ipc-fail-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            ));
            let _ = std::fs::write(&path, &body);
            Err(io::Error::other(format!(
                "{e} (raw frame written to {})",
                path.display()
            )))
        }
    }
}

// ── Channel ──────────────────────────────────────────────────────────────
//
// `IpcChannel` owns everything about the parent/child socket: pair
// creation, CLOEXEC, the read/write loop, and the decoding of audit
// frames into runtime `ExecNode`s.  Callers only see a pair of high
// level methods and an opaque `ChildFd` holder to pass to `Command`.

/// Shell var telling the child process which fd is the IPC endpoint.
pub(super) const IPC_FD_ENV: &str = "RAL_SANDBOX_IPC_FD";

/// Opaque RAII holder for the child-side socket fd between pair
/// creation and subprocess spawn.  The caller writes its number into the
/// child's environment, spawns, then drops this so the parent's copy
/// closes and EOF propagates cleanly when the child exits.
#[cfg(unix)]
pub(super) struct ChildFd(std::os::unix::net::UnixStream);

#[cfg(unix)]
impl ChildFd {
    /// The raw fd number to place in the child's environment.
    pub fn as_raw(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        self.0.as_raw_fd()
    }
}

#[cfg(unix)]
pub(super) struct IpcChannel {
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
impl IpcChannel {
    /// Make a socketpair, clear CLOEXEC on the child end so it survives
    /// fork/exec and bwrap's inner exec, and return the parent half
    /// plus an RAII holder for the child end.
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
        Ok((
            IpcChannel {
                reader,
                writer: parent,
            },
            ChildFd(child),
        ))
    }

    /// Parent loop: send the request, drain every audit frame the child
    /// emits, return the audit frames alongside the body's final response.
    ///
    /// The child writes audit frames only after evaluation finishes (it
    /// drains its own `audit.tree` post-eval, see `eval_request`), so
    /// buffering on the parent side does not lose any concurrency that
    /// could have existed.  The caller decides whether to walk the
    /// frames at all — when no audit tree is active the whole loop
    /// over the returned `Vec` is skipped, so `build_arcs` still pays
    /// nothing on the common path.
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

/// One audit frame as it crossed the wire — paired with the scope table
/// it interns against, so a single `build_arcs` call can rehydrate the
/// node for the parent's audit tree.
#[cfg(unix)]
pub(super) struct AuditFrame {
    pub scope_table: Vec<Vec<(String, SerialValue)>>,
    pub node: IpcExecNode,
}

/// Child entry point: adopt the IPC fd from the environment, read the
/// request, stream audit + final frames, return an exit code.
#[cfg(unix)]
pub(super) fn serve_from_env_fd() -> std::process::ExitCode {
    use std::io::BufReader;
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;
    use std::process::ExitCode;

    let Some(fd) = std::env::var(IPC_FD_ENV)
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
    else {
        crate::diagnostic::cmd_error("ral", &format!("{IPC_FD_ENV} not set or not an fd"));
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
            // wire-format mismatch: the child binary was built from
            // different code than the parent (stale `~/.cargo/bin`
            // shim vs. fresh `cargo build`, or vice versa).  Lead with
            // the actionable diagnosis; carry the underlying serde
            // text on a follow-up line for debugging.
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

// ── Request building and evaluation ──────────────────────────────────────

/// Reify `shell` and the grant body into a wire-ready request.  Inverse
/// of `unpack` in `spawn.rs`.
pub(super) fn pack(
    body: Comp,
    captured: &Env,
    shell: &Shell,
) -> Result<SandboxedBlockRequest, EvalSignal> {
    let mut ctx = InternCtx::new();
    let captured = SerialEnvSnapshot::from_runtime(captured, &mut ctx)?;
    let registry = IpcRegistry::from_runtime(shell, &mut ctx)?;
    let modules = IpcModules::from_runtime(shell, &mut ctx)?;
    let pipe_value = shell
        .io
        .value_in
        .as_ref()
        .map(|v| SerialValue::from_runtime(v, &mut ctx))
        .transpose()?;
    Ok(SandboxedBlockRequest {
        scope_table: ctx.scope_table,
        body,
        captured,
        ambient: IpcAmbient::from_dynamic(&shell.dynamic),
        registry,
        modules,
        loc: shell.location.clone(),
        script_args: shell.dynamic.script_args.clone(),
        pipe_value,
        audit: shell.audit.tree.is_some(),
    })
}

/// Evaluate `request` and stream results over `frames`: one Audit
/// frame per capability-check / exec node the body produced, then one
/// Final frame with the outcome.  Any I/O failure on `frames` is
/// reported back; evaluation errors instead surface as
/// `SandboxedBlockResponse::Error` inside the Final frame.
pub(super) fn run_request<W: Write>(
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

/// Map an `EvalSignal` back to the wire response.  A `TailCall` is a bug
/// — the body must have returned to a normal value or an error before the
/// IPC boundary — so we surface it as an `Error` rather than panic.
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

/// Internal outcome of evaluation — audit nodes plus the body's Result
/// in runtime (not serialised) form.
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
        pipe_value,
        audit,
    } = request;

    let arcs = build_arcs(&scope_table)?;

    let mut outer = Shell::new(Default::default());
    outer.dynamic.env_vars = ambient.env_vars;
    outer.dynamic.cwd = ambient.cwd;
    outer.dynamic.capabilities_stack = ambient.capabilities_stack;
    outer.dynamic.script_args = script_args;
    outer.location = loc;
    registry.install_into(&mut outer, &arcs)?;
    modules.install_into(&mut outer, &arcs)?;
    outer.io.value_in = pipe_value.map(|v| v.into_runtime(&arcs)).transpose()?;
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
