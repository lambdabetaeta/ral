// Subprocess spawning for sandboxed grant blocks.
//
// make_command in mod.rs handles OS-level sandbox wrapping for individual
// external commands.  This module handles the heavier operation: forking a
// fresh ral process inside the OS sandbox so that ral builtins inside a
// grant block also run under kernel enforcement.

use super::SANDBOX_ACTIVE_ENV;
use super::ipc::{
    AuditFrame, IPC_FD_ENV, IpcChannel, SandboxedBlockResponse, pack, serve_from_env_fd,
};
use crate::io::Sink;
use crate::serial::build_arcs;
use crate::types::{Shell, Error, EvalSignal, Value};
use std::fmt::Display;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

pub(super) const SANDBOX_SELF_ENV: &str = "RAL_SANDBOX_SELF_BIN";
const INTERNAL_BLOCK_MODE: &str = "--internal-sandbox-block";

pub(super) fn maybe_enter_process_sandbox(
    args_without_policy: &[String],
    policy: Option<&crate::types::SandboxPolicy>,
) -> Result<Option<ExitCode>, String> {
    let Some(policy) = policy else {
        return Ok(None);
    };
    enter_for_platform(args_without_policy, policy)
}

#[cfg(target_os = "macos")]
fn enter_for_platform(
    _args: &[String],
    policy: &crate::types::SandboxPolicy,
) -> Result<Option<ExitCode>, String> {
    super::macos::enter_current_process(policy, SANDBOX_ACTIVE_ENV)?;
    Ok(None)
}

#[cfg(target_os = "linux")]
fn enter_for_platform(
    args: &[String],
    policy: &crate::types::SandboxPolicy,
) -> Result<Option<ExitCode>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("ral: current_exe: {e}"))?;
    super::linux::respawn_under_bwrap(&exe, args, policy, SANDBOX_ACTIVE_ENV).map(Some)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn enter_for_platform(
    _args: &[String],
    _policy: &crate::types::SandboxPolicy,
) -> Result<Option<ExitCode>, String> {
    Ok(None)
}

pub(super) fn maybe_handle_internal_mode(args: &[String]) -> Option<ExitCode> {
    if args.first().map(|arg| arg.as_str()) != Some(INTERNAL_BLOCK_MODE) {
        return None;
    }
    Some(run_internal_block_mode(args))
}

fn configure_subprocess_stdio(cmd: &mut Command, shell: &mut Shell) -> std::io::Result<Option<Sink>> {
    match shell.io.stdin.take_pipe() {
        Some(r) => cmd.stdin(Stdio::from(r)),
        None => cmd.stdin(Stdio::inherit()),
    };
    cmd.stdout(shell.io.stdout.as_stdio()?);
    cmd.stderr(Stdio::inherit());
    // Return a cloned sink only when a pump thread is needed.
    if shell.io.stdout.needs_pump() {
        Ok(Some(shell.io.stdout.try_clone()?))
    } else {
        Ok(None)
    }
}

/// Dispatch on the top-of-stack grant, the body shape, and whether we
/// can re-spawn ourselves.  Caller always gets a `Value` back (or an
/// `EvalSignal` it can handle); no tri-state "did we sandbox?" leaks
/// out.
pub fn eval_grant(body: &Value, shell: &mut Shell) -> Result<Value, EvalSignal> {
    match choose_runner(body, shell) {
        Runner::InProcess => crate::builtins::call_value(body, &[], shell),
        Runner::Sandboxed(exe) => eval_grant_sandboxed(exe, body, shell),
    }
}

enum Runner {
    InProcess,
    Sandboxed(PathBuf),
}

/// Side conditions for the sandboxed rule, collected at one site so the
/// precondition is visible rather than inferred from a ladder of early
/// returns.
///
/// `fs:` / `net:` declarations are the only kinds of restriction that
/// benefit from OS-level enforcement; everything else is gated by the
/// in-ral checks (`check_exec_args`, etc.) which run inline.  If
/// `RAL_SANDBOX_SELF_BIN` isn't set we can't spawn a confined child and
/// fall through to InProcess: in-ral checks still fire and emit audit
/// events; OS-level enforcement is opportunistic hardening, not the only
/// line of defence.
fn choose_runner(body: &Value, shell: &Shell) -> Runner {
    let needs_sandbox = shell.sandbox_policy().is_some();
    if !needs_sandbox {
        crate::dbg_trace!("grant-spawn", "skip: no fs grant or net deny");
        return Runner::InProcess;
    }
    if !matches!(body, Value::Thunk { .. }) {
        return Runner::InProcess;
    }
    let Some(exe) = std::env::var_os(SANDBOX_SELF_ENV).map(PathBuf::from) else {
        crate::dbg_trace!(
            "grant-spawn",
            "RAL_SANDBOX_SELF_BIN not set; running in-process without OS sandbox"
        );
        return Runner::InProcess;
    };
    crate::dbg_trace!(
        "grant-spawn",
        "sandboxed (audit={}, exe={})",
        shell.audit.tree.is_some(),
        exe.display()
    );
    Runner::Sandboxed(exe)
}

/// The sandboxed rule, three premises wide: pack the shell for the wire,
/// round-trip the request, unpack the response back into the shell.
#[cfg(unix)]
fn eval_grant_sandboxed(exe: PathBuf, body: &Value, shell: &mut Shell) -> Result<Value, EvalSignal> {
    let Value::Thunk { body, captured, .. } = body else {
        unreachable!("choose_runner admits only Thunk bodies into the sandboxed rule")
    };
    let policy = shell
        .sandbox_policy()
        .ok_or_else(|| EvalSignal::Error(Error::new("grant sandbox policy missing", 1)))?;
    let request = pack(body.as_ref().clone(), captured, shell)?;
    let response = round_trip(exe, &policy, &request, shell)?;
    unpack(response, shell)
}

#[cfg(not(unix))]
fn eval_grant_sandboxed(_exe: PathBuf, _body: &Value, _env: &mut Shell) -> Result<Value, EvalSignal> {
    Err(EvalSignal::Error(Error::new(
        "grant: sandbox subprocess is Unix-only",
        1,
    )))
}

/// Tag a low-level failure with the sandbox stage that produced it so
/// users see *which* step broke ("encode policy", "spawn", "wait"),
/// not just `grant: <bare-io-error>`.
fn stage_err(stage: &str, e: impl Display) -> EvalSignal {
    EvalSignal::Error(Error::new(format!("grant sandbox: {stage}: {e}"), 1))
}

/// Round-trip a request through a freshly-spawned sandboxed child.
/// Transport specifics — socketpair, CLOEXEC, framing — live in
/// `IpcChannel`; this function is pure process lifecycle: Command,
/// stdio pump thread, wait.
#[cfg(unix)]
fn round_trip(
    exe: PathBuf,
    policy: &crate::types::SandboxPolicy,
    request: &crate::sandbox::ipc::SandboxedBlockRequest,
    shell: &mut Shell,
) -> Result<SandboxedBlockResponse, EvalSignal> {
    let policy_json = serde_json::to_string(policy).map_err(|e| stage_err("encode policy", e))?;
    let (channel, child_fd) = IpcChannel::open_pair().map_err(|e| stage_err("ipc setup", e))?;
    let mut child = Command::new(exe);
    child
        .arg(super::SANDBOX_POLICY_FLAG)
        .arg(&policy_json)
        .arg(INTERNAL_BLOCK_MODE)
        .env(IPC_FD_ENV, child_fd.as_raw().to_string());
    let pump_sink = configure_subprocess_stdio(&mut child, shell)
        .map_err(|e| stage_err("configure stdio", e))?;

    let mut spawned = child.spawn().map_err(|e| stage_err("spawn child", e))?;
    // Drop our copy of the child end so EOF propagates when the child dies.
    drop(child_fd);

    let drain_thread = pump_sink.and_then(|sink| spawned.stdout.take().map(|s| sink.pump(s)));

    let drive_result = channel.drive(request);

    let spawn_result = spawned.wait();
    if let Some(t) = drain_thread {
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
/// When no tree is active the whole loop is a no-op, so the unaudited
/// fast path pays only for the (already-deserialised) `Vec` itself.
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
/// Three outcome variants map to three rule conclusions: `Ok` yields a
/// value, `Exit` propagates, `Error` raises.  `shell.control.last_status` is set
/// on all three paths so the outer shell sees the correct `$?`.
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

fn run_internal_block_mode(_args: &[String]) -> ExitCode {
    serve_from_env_fd()
}
