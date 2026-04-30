//! Grant evaluation and sandboxed subprocess runner.
//!
//! [`eval_grant`] is the single entry point for executing a `grant` body.
//! It dispatches to one of three runners based on sandbox eligibility:
//!
//! - `InProcess` — no fs/net restriction or non-Thunk body: run directly.
//! - `Sandboxed` — spawn a fresh ral process under the OS sandbox and
//!   communicate over IPC.
//! - `Unavailable` — binary not sandbox-capable; propagate the reason.

use super::ipc::{AuditFrame, IPC_FD_ENV, IpcChannel, SandboxedBlockRequest, SandboxedBlockResponse, pack};
use super::reexec::{SANDBOX_SELF, SandboxSelf, verify_unswapped};
use crate::io::Sink;
use crate::serial::build_arcs;
use crate::types::{Error, EvalSignal, Shell, Value};
use std::fmt::Display;
use std::process::{Command, Stdio};

pub(super) const INTERNAL_BLOCK_MODE: &str = "--internal-sandbox-block";

// ── Runner dispatch ──────────────────────────────────────────────────────

enum Runner {
    InProcess,
    Unavailable(&'static str),
    #[cfg(unix)]
    Sandboxed(&'static SandboxSelf),
}

/// Dispatch on the top-of-stack grant, the body shape, and whether the
/// binary is sandbox-capable.  The caller always receives a `Value`
/// (or an `EvalSignal` it can handle).
pub fn eval_grant(body: &Value, shell: &mut Shell) -> Result<Value, EvalSignal> {
    match choose_runner(body, shell) {
        Runner::InProcess => crate::builtins::call_value(body, &[], shell),
        Runner::Unavailable(reason) => Err(EvalSignal::Error(Error::new(
            format!("grant sandbox unavailable: {reason}"),
            1,
        ))),
        #[cfg(unix)]
        Runner::Sandboxed(s) => eval_grant_sandboxed(s, body, shell),
    }
}

/// Choose the execution strategy for a `grant` body.
///
/// A restrictive `fs`/`net` grant on a `Thunk` body, combined with a
/// registered sandbox-self, selects `Sandboxed`.  Any failure at any
/// check falls through to `InProcess` — in-ral capability checks still
/// fire; OS-level enforcement is opportunistic hardening.
#[cfg(unix)]
fn choose_runner(body: &Value, shell: &Shell) -> Runner {
    if shell.sandbox_projection().is_none() {
        crate::dbg_trace!("grant-spawn", "skip: no fs grant or net deny");
        return Runner::InProcess;
    }
    if !matches!(body, Value::Thunk { .. }) {
        return Runner::InProcess;
    }
    if !platform_sandbox_supported() {
        return Runner::Unavailable("this Unix platform has no fs/net process sandbox backend");
    }
    let Some(s) = SANDBOX_SELF.get() else {
        crate::dbg_trace!("grant-spawn", "binary not sandbox-capable; in-process");
        return Runner::Unavailable("failed to pin the current executable for sandbox re-exec");
    };
    crate::dbg_trace!(
        "grant-spawn",
        "sandboxed (audit={}, exec={}, arg0={})",
        shell.audit.tree.is_some(),
        s.exec_path.display(),
        s.arg0.display()
    );
    Runner::Sandboxed(s)
}

#[cfg(not(unix))]
fn choose_runner(_body: &Value, _shell: &Shell) -> Runner {
    Runner::InProcess
}

#[cfg(unix)]
fn platform_sandbox_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

// ── Sandboxed execution ──────────────────────────────────────────────────

/// The sandboxed rule: pack the shell for the wire, round-trip the
/// request through a fresh sandbox child, unpack the response.
#[cfg(unix)]
fn eval_grant_sandboxed(
    s: &'static SandboxSelf,
    body: &Value,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    let Value::Thunk { body, captured, .. } = body else {
        unreachable!("choose_runner admits only Thunk bodies into the sandboxed rule")
    };
    verify_unswapped(s)?;
    let policy = shell
        .sandbox_projection()
        .ok_or_else(|| EvalSignal::Error(Error::new("grant sandbox policy missing", 1)))?;
    let request = pack(body.as_ref().clone(), captured, shell)?;
    let response = round_trip(s, &policy, &request, shell)?;
    unpack(response, shell)
}

/// Tag a low-level failure with the sandbox stage that produced it.
pub(super) fn stage_err(stage: &str, e: impl Display) -> EvalSignal {
    EvalSignal::Error(Error::new(format!("grant sandbox: {stage}: {e}"), 1))
}

/// Stdio pump sinks for the spawned child's stdout/stderr.
struct PumpSinks {
    stdout: Option<Sink>,
    stderr: Option<Sink>,
}

fn configure_subprocess_stdio(cmd: &mut Command, shell: &mut Shell) -> std::io::Result<PumpSinks> {
    match shell.io.stdin.take_reader() {
        Some(r) => cmd.stdin(Stdio::from(r)),
        None => cmd.stdin(Stdio::inherit()),
    };
    cmd.stdout(shell.io.stdout.as_stdio()?);
    cmd.stderr(stderr_stdio(&shell.io.stderr)?);
    Ok(PumpSinks {
        stdout: shell
            .io
            .stdout
            .needs_pump()
            .then(|| shell.io.stdout.try_clone())
            .transpose()?,
        stderr: stderr_needs_pump(&shell.io.stderr)
            .then(|| shell.io.stderr.try_clone())
            .transpose()?,
    })
}

fn stderr_stdio(sink: &Sink) -> std::io::Result<Stdio> {
    match sink {
        Sink::Stderr => Ok(Stdio::inherit()),
        Sink::Pipe(w) => Ok(Stdio::from(w.try_clone()?)),
        _ => Ok(Stdio::piped()),
    }
}

fn stderr_needs_pump(sink: &Sink) -> bool {
    !matches!(sink, Sink::Stderr | Sink::Pipe(_))
}

/// Round-trip a request through a freshly-spawned sandboxed child.
/// Transport specifics — socketpair, CLOEXEC, framing — live in
/// `IpcChannel`; this function owns process lifecycle: Command, stdio
/// pump threads, wait.
#[cfg(unix)]
fn round_trip(
    s: &'static SandboxSelf,
    policy: &crate::types::SandboxProjection,
    request: &SandboxedBlockRequest,
    shell: &mut Shell,
) -> Result<SandboxedBlockResponse, EvalSignal> {
    use std::os::unix::process::CommandExt;

    let policy_json = serde_json::to_string(policy).map_err(|e| stage_err("encode policy", e))?;
    let (channel, child_fd) = IpcChannel::open_pair().map_err(|e| stage_err("ipc setup", e))?;
    let mut child = Command::new(&s.exec_path);
    child
        .arg0(&s.arg0)
        .arg(super::SANDBOX_PROJECTION_FLAG)
        .arg(&policy_json)
        .arg(INTERNAL_BLOCK_MODE)
        .env(IPC_FD_ENV, child_fd.as_raw().to_string());
    let pump_sinks = configure_subprocess_stdio(&mut child, shell)
        .map_err(|e| stage_err("configure stdio", e))?;

    let mut spawned = child.spawn().map_err(|e| {
        if s.exec_path != s.arg0 {
            stage_err(
                "spawn child",
                format!(
                    "{e} (exec path {}, intended binary {})",
                    s.exec_path.display(),
                    s.arg0.display()
                ),
            )
        } else {
            stage_err("spawn child", e)
        }
    })?;
    // Drop our copy of the child end so EOF propagates when the child dies.
    drop(child_fd);

    let stdout_thread = pump_sinks
        .stdout
        .and_then(|sink| spawned.stdout.take().map(|s| sink.pump(s)));
    let stderr_thread = pump_sinks
        .stderr
        .and_then(|sink| spawned.stderr.take().map(|s| sink.pump(s)));

    let drive_result = channel.drive(request);

    let spawn_result = spawned.wait();
    if let Some(t) = stdout_thread {
        let _ = t.join();
    }
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }
    let child_status = spawn_result.map_err(|e| stage_err("wait child", e))?;
    drop(spawned);

    let (audit_frames, response) = drive_result.map_err(|e| match e {
        EvalSignal::Error(mut err) if child_status.code().is_some() => {
            err.status = child_status.code().unwrap_or(err.status);
            EvalSignal::Error(err)
        }
        other => other,
    })?;

    materialise_audit(shell, audit_frames)?;
    Ok(response)
}

/// Rehydrate the child's audit frames into the parent's audit tree.
/// When no tree is active the loop is a no-op.
#[cfg(unix)]
fn materialise_audit(shell: &mut Shell, frames: Vec<AuditFrame>) -> Result<(), EvalSignal> {
    let Some(tree) = shell.audit.tree.as_mut() else {
        return Ok(());
    };
    for frame in frames {
        let arcs = build_arcs(&frame.scope_table)?;
        tree.push(frame.node.into_runtime(&arcs)?);
    }
    Ok(())
}

/// Absorb the subprocess response into `shell` — inverse of `pack`.
///
/// Three outcome variants map to three rule conclusions: `Ok` yields a
/// value, `Exit` propagates, `Error` raises.  `shell.control.last_status`
/// is set on all three paths so the outer shell sees the correct `$?`.
fn unpack(response: SandboxedBlockResponse, shell: &mut Shell) -> Result<Value, EvalSignal> {
    match response {
        SandboxedBlockResponse::Ok {
            scope_table,
            value,
            last_status,
        } => {
            let arcs = build_arcs(&scope_table)?;
            shell.control.last_status = last_status;
            Ok(value.into_runtime(&arcs)?)
        }
        SandboxedBlockResponse::Exit { code } => Err(EvalSignal::Exit(code)),
        SandboxedBlockResponse::Error {
            message,
            status,
            hint,
        } => {
            shell.control.last_status = status;
            Err(EvalSignal::Error(Error {
                message,
                status,
                loc: None,
                hint,
                kind: crate::types::ErrorKind::Other,
            }))
        }
    }
}
