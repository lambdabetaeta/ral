//! Subprocess spawning for sandboxed `grant` blocks.
//!
//! `make_command` in the parent module wraps individual external
//! commands.  This module re-execs a fresh ral process inside the OS
//! sandbox so ral builtins inside a `grant` block also run under
//! kernel enforcement.
//!
//! ## Sandbox-self pinning
//!
//! At [`crate::sandbox::early_init`] we capture an immutable handle
//! on our own executable.  Restrictive grant blocks then re-exec
//! that same binary, so a swap on disk between boot and spawn
//! (`cargo install` mid-session, package upgrade) cannot redirect
//! the child to a different binary with an incompatible IPC wire
//! format.
//!
//! - **Linux**: open the file as `OwnedFd` (no `O_CLOEXEC`) and
//!   re-exec via `/proc/self/fd/<N>`.  The kernel resolves through
//!   our fd table to the original inode; protection is total.
//! - **macOS**: the kernel refuses `execve("/dev/fd/<N>", …)`
//!   (devfs entries lack the X bit), so we stat the file at boot
//!   instead and verify `(dev, ino, mtime)` immediately before each
//!   spawn.  If anything differs we refuse with a "restart exarch"
//!   message.  TOCTOU window is microseconds and bounded by an
//!   explicit check.
//!
//! `argv[0]` is the resolved on-disk path so the child sees a
//! recognisable name regardless of the exec mechanism.
//!
//! Used to be the `RAL_SANDBOX_SELF_BIN` env var, which inherited
//! from shells or earlier sessions and poisoned the dispatch.
//! In-process state is the right shape: only the binary that ran
//! `early_init` is sandbox-capable, and test binaries / embedders
//! that build a `Shell` without `early_init` fall through to
//! in-process evaluation cleanly.

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
use std::sync::OnceLock;

const INTERNAL_BLOCK_MODE: &str = "--internal-sandbox-block";

#[cfg(unix)]
pub(super) struct SandboxSelf {
    /// Platform-specific anchor that ties `exec_path` to the binary
    /// inode opened at boot.  Variants are picked at registration
    /// time — see [`Pin`].
    pin: Pin,
    /// Exec target passed to `Command::new`.
    pub exec_path: PathBuf,
    /// `argv[0]` for the spawned child.
    pub arg0: PathBuf,
}

/// How we keep `exec_path` bound to the binary we registered.
///
/// `Fd` is the strong guarantee — the held fd makes the kernel
/// resolve `/proc/self/fd/<N>` to the original inode regardless of
/// what's at the path now.  `Stat` is the macOS / other-Unix
/// fallback: a snapshot of `(dev, ino, mtime)` we re-check before
/// each spawn (since `execve("/dev/fd/<N>", …)` is a kernel-rejected
/// no-go on macOS).
#[cfg(unix)]
enum Pin {
    /// Held purely for its `Drop` — the open fd keeps
    /// `/proc/self/fd/<N>` resolving to the boot inode.
    #[cfg(target_os = "linux")]
    Fd(#[allow(dead_code)] std::os::fd::OwnedFd),
    #[cfg(all(unix, not(target_os = "linux")))]
    Stat { dev: u64, ino: u64, mtime: i64 },
}

#[cfg(unix)]
pub(super) static SANDBOX_SELF: OnceLock<SandboxSelf> = OnceLock::new();

/// Pin our own executable for the rest of the process's life.
/// Idempotent; failures are silent (sandbox-capable becomes false,
/// restrictive grants fall through to in-process evaluation).
#[cfg(unix)]
pub(super) fn register_sandbox_self() {
    if SANDBOX_SELF.get().is_some() {
        return;
    }
    let Ok(arg0) = std::env::current_exe() else { return };
    let Some((pin, exec_path)) = build_pin(&arg0) else { return };
    let _ = SANDBOX_SELF.set(SandboxSelf {
        pin,
        exec_path,
        arg0,
    });
}

#[cfg(not(unix))]
pub(super) fn register_sandbox_self() {}

/// Open `arg0` and produce the `(pin, exec_path)` pair the platform
/// uses for binary-swap protection.  Returns `None` on any failure;
/// the caller treats that as "binary not sandbox-capable."
#[cfg(target_os = "linux")]
fn build_pin(arg0: &std::path::Path) -> Option<(Pin, PathBuf)> {
    use std::os::fd::{AsRawFd, OwnedFd};

    let exe: OwnedFd = std::fs::File::open(arg0).ok()?.into();

    // Clear FD_CLOEXEC so the fd survives execve into the sandbox
    // child, where `/proc/self/fd/<N>` needs to resolve.
    let raw = exe.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
    Some((Pin::Fd(exe), PathBuf::from(format!("/proc/self/fd/{raw}"))))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn build_pin(arg0: &std::path::Path) -> Option<(Pin, PathBuf)> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::metadata(arg0).ok()?;
    let pin = Pin::Stat {
        dev: meta.dev(),
        ino: meta.ino(),
        mtime: meta.mtime(),
    };
    Some((pin, arg0.to_path_buf()))
}

/// Refuse to spawn if our executable on disk has been swapped since
/// registration (`cargo install`, package upgrade).  `Pin::Fd` is a
/// no-op — the fd-derived exec path makes it impossible.
#[cfg(unix)]
fn verify_unswapped(s: &SandboxSelf) -> Result<(), EvalSignal> {
    match &s.pin {
        #[cfg(target_os = "linux")]
        Pin::Fd(_) => Ok(()),
        #[cfg(all(unix, not(target_os = "linux")))]
        Pin::Stat { dev, ino, mtime } => {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(&s.arg0).map_err(|e| {
                stage_err("verify self", format!("cannot stat {}: {e}", s.arg0.display()))
            })?;
            if meta.dev() == *dev && meta.ino() == *ino && meta.mtime() == *mtime {
                Ok(())
            } else {
                Err(EvalSignal::Error(Error::new(
                    format!(
                        "exarch binary at {} changed since startup; \
                         restart exarch to pick up the new build",
                        s.arg0.display()
                    ),
                    1,
                )))
            }
        }
    }
}

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
    Err("ral: fs/net sandboxing is unavailable on this Unix platform".into())
}

pub(super) fn maybe_handle_internal_mode(args: &[String]) -> Option<ExitCode> {
    if args.first().map(|arg| arg.as_str()) != Some(INTERNAL_BLOCK_MODE) {
        return None;
    }
    Some(run_internal_block_mode(args))
}

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

/// Dispatch on the top-of-stack grant, the body shape, and whether
/// the binary is sandbox-capable.  Caller always gets a `Value` back
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

enum Runner {
    InProcess,
    Unavailable(&'static str),
    #[cfg(unix)]
    Sandboxed(&'static SandboxSelf),
}

/// Sandbox eligibility: a restrictive `fs`/`net` grant on a Thunk
/// body, plus a registered sandbox-self.  Anything that fails any
/// check falls through to `InProcess` — in-ral capability checks
/// still fire; OS-level enforcement is opportunistic hardening.
#[cfg(unix)]
fn choose_runner(body: &Value, shell: &Shell) -> Runner {
    if shell.sandbox_policy().is_none() {
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

/// The sandboxed rule, three premises wide: pack the shell for the
/// wire, round-trip the request, unpack the response back.
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
        .sandbox_policy()
        .ok_or_else(|| EvalSignal::Error(Error::new("grant sandbox policy missing", 1)))?;
    let request = pack(body.as_ref().clone(), captured, shell)?;
    let response = round_trip(s, &policy, &request, shell)?;
    unpack(response, shell)
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
    s: &'static SandboxSelf,
    policy: &crate::types::SandboxPolicy,
    request: &crate::sandbox::ipc::SandboxedBlockRequest,
    shell: &mut Shell,
) -> Result<SandboxedBlockResponse, EvalSignal> {
    use std::os::unix::process::CommandExt;

    let policy_json = serde_json::to_string(policy).map_err(|e| stage_err("encode policy", e))?;
    let (channel, child_fd) = IpcChannel::open_pair().map_err(|e| stage_err("ipc setup", e))?;
    let mut child = Command::new(&s.exec_path);
    child
        .arg0(&s.arg0)
        .arg(super::SANDBOX_POLICY_FLAG)
        .arg(&policy_json)
        .arg(INTERNAL_BLOCK_MODE)
        .env(IPC_FD_ENV, child_fd.as_raw().to_string());
    let pump_sinks = configure_subprocess_stdio(&mut child, shell)
        .map_err(|e| stage_err("configure stdio", e))?;

    let mut spawned = child.spawn().map_err(|e| {
        // If we used a non-trivial exec path (the fd-derived form),
        // include both paths so users see which one the kernel
        // actually rejected.  Plain path-based exec failures still
        // get the unadorned message.
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
