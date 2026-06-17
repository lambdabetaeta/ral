//! Sandbox-confined eval transport — process lifecycle, stdio, IPC.
//!
//! This module owns enforcement-only mechanics: spawning a fresh ral
//! process under the OS sandbox, marshalling a [`ChildEvalRequest`]
//! through the IPC channel, and folding the [`ChildEvalResponse`] back
//! to the caller.  It is *body-shape agnostic*: it ships a `Comp` plus
//! a [`Mobile`] snapshot to the child, and the child runs the body
//! against a fresh shell installed from that mobile.
//!
//! ## Entry point
//!
//! [`run_confined`] ships a `Comp` plus a `Mobile` snapshot to a fresh
//! child, runs `eval_comp` in the child against a shell built from
//! that mobile, and returns the post-run mobile + outcome.  Audit
//! fragments received from the child are merged into `parent.local.audit`
//! here so the contract layer doesn't bubble them.  The mobile carries
//! everything the body needs — lexical env (already extended with any
//! block-scope frame by the contract layer), dynamic state, grants —
//! so this entry point is body-shape agnostic.
//!
//! ## Platform split
//!
//! Unix and Windows share the same request packing and response
//! decoding — [`pack_request`] and [`decode_response`] are
//! platform-neutral.  Only the **transport** differs:
//!
//! - On Unix the child enters Seatbelt/bwrap inside `early_init` after
//!   re-exec; the parent uses a socketpair to ferry the IPC frames.
//! - On Windows the parent configures a restricted-token primary + the
//!   Job Object at `CreateProcessAsUserW` time and ferries IPC over a
//!   named pipe; the child does no sandbox self-entry.

#[cfg(unix)]
use super::ipc::IPC_FD_ENV;
#[cfg(windows)]
use super::ipc::IPC_HANDLE_ENV;
use super::ipc::IpcChannel;
use super::reexec::{SANDBOX_SELF, SandboxSelf, verify_unswapped};
use crate::child_eval::{ChildEvalRequest, ChildEvalResponse, decode_response, pack_request};
#[cfg(unix)]
use crate::io::Sink;
use crate::ir::Comp;
use crate::types::{AuditFragment, Break, Error, Mobile, Settled, Shell, Value};
#[cfg(unix)]
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::thread::{self, JoinHandle};

/// Pump destinations carried out of the Unix `configure_subprocess_stdio`
/// so the caller can drain the child fds after `spawn()`.
#[cfg(unix)]
struct PumpSinks {
    stdout: Option<Sink>,
    stderr: Option<Sink>,
}

/// CLI flag that tells a re-exec'd ral process to serve one IPC eval
/// request.
pub(super) const INTERNAL_EVAL_MODE: &str = "--internal-sandbox-block";

// ── Public entry point (called by `runtime::transport`) ────────────────

fn unavailable_result(mobile: Mobile, reason: &'static str) -> (Mobile, Settled<Value>) {
    (
        mobile,
        Err(Break::Error(Error::new(
            format!("sandbox confinement unavailable: {reason}"),
            1,
        ))),
    )
}

// ── Confined execution ───────────────────────────────────────────────────

/// Tag a low-level failure with the sandbox stage that produced it.
pub(super) fn stage_err(stage: &str, e: impl Display) -> Break {
    Break::Error(Error::new(format!("sandbox eval: {stage}: {e}"), 1))
}

/// Ship `body` plus `mobile` to a fresh ral process under the OS
/// sandbox; return the post-run mobile and outcome.  The body's audit
/// fragment is merged into `parent.local.audit` here so the contract
/// layer doesn't bubble it.  Body-shape agnostic: the mobile carries
/// everything the child needs to install onto its own shell.
pub(crate) fn run_confined(
    body: &Arc<Comp>,
    mobile: Mobile,
    parent: &mut Shell,
) -> Settled<(Mobile, Settled<Value>)> {
    let Some(s) = SANDBOX_SELF.get() else {
        return Ok(unavailable_result(
            mobile,
            "failed to pin the current executable for sandbox re-exec",
        ));
    };
    verify_unswapped(s)?;
    let policy = parent
        .sandbox_projection()
        .ok_or_else(|| Break::Error(Error::new("sandbox eval: policy missing", 1)))?;
    let audit_policy = parent.local.audit.active_policy();
    crate::dbg_trace!(
        "eval-confined",
        "audit={} exec={} arg0={}",
        audit_policy.is_some(),
        s.exec_path.display(),
        s.arg0.display()
    );
    // Mint a fresh capability token for this re-exec and ship it inside
    // the request.  The confined child adopts it (records it, stamps it
    // into `RAL_SANDBOX_ACTIVE`) so its own nested grants dispatch
    // locally; an arbitrary parent that exports the marker cannot forge
    // this token because it never reaches the private IPC request
    // channel.  See `sandbox::marker`.
    let token = super::marker::mint();
    // Sandbox: the mobile already carries the lexical env (`captured:
    // None`), the parent installs the post-run mobile (`wants_mobile`),
    // and the success value is always reported (`wants_value`).
    let request = pack_request(
        Arc::clone(body),
        &mobile,
        None,
        audit_policy,
        true,
        true,
        Some(token),
    )?;
    // Mobile was reified into `request`; drop the original so the only
    // surviving `Mobile` value is the post-run snapshot we hand
    // back to the caller below.
    drop(mobile);

    let response = round_trip(s, &policy, &request, parent)?;
    let (mobile, outcome, audit, surface_events) = fold_response(response)?;
    // The builtin table is session state, not mobile — `install_mobile`
    // swaps only `shell.mobile`, so the parent's dispatch survives a confined
    // turn untouched and there is nothing to restore here.
    // Replay the events the child surfaced through the parent's own sink
    // now that the confined eval has returned (batched delivery).
    if let Some(sink) = parent.turn.surface.as_ref() {
        for event in surface_events {
            sink(event);
        }
    }
    if !audit.is_empty() {
        parent.local.audit.merge(audit);
    }
    Ok((mobile, outcome))
}

/// The only constructor of the confined re-exec spawn names the complete
/// inheritable set: the command's std fds plus the one IPC fd.  On Unix
/// the discipline is CLOEXEC on every other fd (guaranteed by
/// [`IpcEndpoint`]), so the inherited set is exactly this allow-list.
#[cfg(unix)]
struct ConfinedSpawn<'a> {
    command: &'a mut std::process::Command,
    inherit_ipc: std::os::unix::io::RawFd,
}

#[cfg(unix)]
impl ConfinedSpawn<'_> {
    /// Place the lent IPC fd in the child's environment and spawn.
    fn spawn(self) -> std::io::Result<std::process::Child> {
        self.command
            .env(IPC_FD_ENV, self.inherit_ipc.to_string())
            .spawn()
    }
}

#[cfg(unix)]
fn round_trip(
    s: &'static SandboxSelf,
    policy: &crate::types::SandboxProjection,
    request: &ChildEvalRequest,
    parent: &mut Shell,
) -> Settled<ChildEvalResponse> {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    super::dump_profile_if_requested(policy);
    let policy_json = serde_json::to_string(policy).map_err(|e| stage_err("encode policy", e))?;
    let (channel, endpoint) = IpcChannel::open_pair().map_err(|e| stage_err("ipc setup", e))?;
    let mut child = Command::new(&s.exec_path);
    child
        .arg0(&s.arg0)
        .arg(super::SANDBOX_PROJECTION_FLAG)
        .arg(&policy_json)
        .arg(INTERNAL_EVAL_MODE)
        // Strip any inherited confinement marker so the child enters the
        // OS sandbox fresh.  A forged marker in the parent's environment
        // would otherwise trip `assert_not_already_confined` in the
        // child's `early_init` and abort confinement; the child receives
        // its authentic marker only by adopting the token from the IPC
        // request.
        .env_remove(super::SANDBOX_ACTIVE_ENV);
    let pump_sinks = configure_subprocess_stdio(&mut child, parent)
        .map_err(|e| stage_err("configure stdio", e))?;

    let spawn_err = |e: std::io::Error| {
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
    };
    // The lend scope brackets the spawn: the IPC fd is inheritable only
    // here, restored to CLOEXEC the instant the closure returns.
    let mut spawned = endpoint
        .lend_for_spawn(|fd| {
            ConfinedSpawn {
                command: &mut child,
                inherit_ipc: fd,
            }
            .spawn()
        })
        .map_err(|e| stage_err("ipc lend", e))?
        .map_err(spawn_err)?;
    drop(endpoint);
    let cancel_watch =
        SandboxCancelWatch::start(spawned.id(), parent.turn.cancel.as_scope().clone());

    let stdout_thread = pump_sinks
        .stdout
        .and_then(|sink| spawned.stdout.take().map(|s| sink.pump(s)));
    let stderr_thread = pump_sinks
        .stderr
        .and_then(|sink| spawned.stderr.take().map(|s| sink.pump(s)));

    let drive_result = channel.drive(request);

    // Sandbox IPC subprocess: a one-shot helper that won't be
    // SIGSTOP'd by user code (no shell-level job control reaches it),
    // so the WUNTRACED ceremony of ChildHandle is unnecessary.
    #[allow(clippy::disallowed_methods)]
    let spawn_result = spawned.wait();
    if let Some(t) = stdout_thread {
        let _ = t.join();
    }
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }
    let child_status = spawn_result.map_err(|e| stage_err("wait child", e))?;
    drop(spawned);
    drop(cancel_watch);

    drive_result.map_err(|e| match e {
        Break::Error(mut err) if child_status.code().is_some() => {
            err.status = crate::types::Status::Code(child_status.code().unwrap_or(err.exit_code()));
            Break::Error(err)
        }
        other => other,
    })
}

#[cfg(unix)]
fn configure_subprocess_stdio(
    cmd: &mut std::process::Command,
    shell: &mut Shell,
) -> std::io::Result<PumpSinks> {
    use std::process::Stdio;
    match shell.turn.io.stdin.take_reader() {
        Some(r) => cmd.stdin(Stdio::from(r)),
        None => cmd.stdin(Stdio::inherit()),
    };
    let stdout = if !shell.turn.io.job_control.may_foreground()
        && matches!(shell.turn.io.stdout, Sink::Terminal)
    {
        crate::io::ChildStdioPlan::inherit()
    } else {
        shell.turn.io.stdout.child_stdout(false)?
    };
    cmd.stdout(stdout.stdio);
    let stderr = shell.turn.io.stderr.child_stderr()?;
    cmd.stderr(stderr.stdio);
    Ok(PumpSinks {
        stdout: stdout.pump,
        stderr: stderr.pump,
    })
}

/// Out-of-band cancellation for the sandbox IPC child.
///
/// The parent is otherwise blocked in a synchronous frame read while the
/// confined helper evaluates the body.  Foreground deadlines and Esc only flip
/// the parent's cancel scope; they do not cross the IPC channel.  This watcher
/// observes that scope and tears down the helper subtree from outside the OS
/// sandbox, where signalling descendants is still allowed.
#[cfg(unix)]
struct SandboxCancelWatch {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

#[cfg(unix)]
impl SandboxCancelWatch {
    fn start(root_pid: u32, scope: crate::process::CancelScope) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                if let Some(cause) = scope.cause() {
                    terminate_confined_subtree(root_pid, cause);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

#[cfg(unix)]
impl Drop for SandboxCancelWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(unix)]
fn terminate_confined_subtree(root_pid: u32, cause: crate::process::CancelCause) {
    let first_signal = match cause {
        crate::process::CancelCause::Interrupt => libc::SIGINT,
        crate::process::CancelCause::Explicit | crate::process::CancelCause::Deadline => {
            libc::SIGTERM
        }
        crate::process::CancelCause::RootAbort => libc::SIGKILL,
    };
    let mut pids = sample_descendants(root_pid);
    pids.insert(root_pid);
    signal_confined_set(&pids, first_signal);
    if first_signal == libc::SIGKILL {
        return;
    }

    std::thread::sleep(std::time::Duration::from_millis(200));
    pids.extend(sample_descendants(root_pid));
    pids.insert(root_pid);
    signal_confined_set(&pids, libc::SIGKILL);
}

#[cfg(unix)]
fn signal_confined_set(pids: &HashSet<u32>, signal: libc::c_int) {
    for &pid in pids {
        if pid <= i32::MAX as u32 {
            unsafe {
                libc::kill(pid as libc::pid_t, signal);
            }
        }
    }
}

#[cfg(unix)]
fn sample_descendants(root: u32) -> HashSet<u32> {
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid="])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return HashSet::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid_s) = parts.next() else {
            continue;
        };
        let Some(ppid_s) = parts.next() else {
            continue;
        };
        let Ok(pid) = pid_s.parse::<u32>() else {
            continue;
        };
        let Ok(ppid) = ppid_s.parse::<u32>() else {
            continue;
        };
        children_of.entry(ppid).or_default().push(pid);
    }
    let mut seen = HashSet::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        if let Some(children) = children_of.get(&parent) {
            for &child in children {
                if seen.insert(child) {
                    frontier.push(child);
                }
            }
        }
    }
    seen
}

/// The only constructor of the confined re-exec spawn names the complete
/// inheritable set: the three std handles plus the one IPC handle.  Its
/// Windows backend installs that set as a `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`,
/// so `bInheritHandles = TRUE` passes the child exactly these handles and
/// no other.
#[cfg(windows)]
struct ConfinedSpawn<'a> {
    exec_path: &'a std::path::Path,
    arg0: &'a std::path::Path,
    argv_tail: &'a [String],
    spec: &'a super::windows_restricted_token::RestrictedTokenSpec,
    stdio: [windows_sys::Win32::Foundation::HANDLE; 3],
    inherit_ipc: windows_sys::Win32::Foundation::HANDLE,
    env_block: &'a [u16],
}

#[cfg(windows)]
impl ConfinedSpawn<'_> {
    /// Spawn the suspended child with inheritance bounded to the named
    /// handles.  Must run inside [`IpcEndpoint::lend_for_spawn`] so the
    /// IPC handle is inheritable at `CreateProcessAsUserW`.
    fn spawn(self) -> Settled<super::windows_restricted_token::Spawned> {
        let [in_h, out_h, err_h] = self.stdio;
        super::windows_restricted_token::spawn_confined(
            self.exec_path,
            self.arg0,
            self.argv_tail,
            self.spec,
            [in_h, out_h, err_h, self.inherit_ipc],
            Some(self.env_block),
        )
    }
}

/// Windows round-trip: build the IPC named pipe, spawn the child under
/// a restricted token + a Job Object, drive the IPC, and reap.
///
/// For v1 the child inherits the parent's stdin/stdout/stderr handles
/// directly; capture/audit pump support is a follow-up (the audit
/// stream itself still rides the IPC channel).
#[cfg(windows)]
fn round_trip(
    s: &'static SandboxSelf,
    policy: &crate::types::SandboxProjection,
    request: &ChildEvalRequest,
    _parent: &mut Shell,
) -> Settled<ChildEvalResponse> {
    use super::windows::JobHandle;
    use super::windows_restricted_token::{RestrictedTokenSpec, resume};
    // SANDBOX_SELF handle access types are used implicitly through
    // the spawn helper; no direct imports needed here.
    use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    super::dump_profile_if_requested(policy);
    let t0 = std::time::Instant::now();
    crate::dbg_trace!("sandbox-rt", "begin elapsed={:?}", t0.elapsed());
    let policy_json = serde_json::to_string(policy).map_err(|e| stage_err("encode policy", e))?;
    crate::dbg_trace!("sandbox-rt", "policy-encoded elapsed={:?}", t0.elapsed());
    let (channel, endpoint) = IpcChannel::open_pair().map_err(|e| stage_err("ipc setup", e))?;
    crate::dbg_trace!("sandbox-rt", "ipc-paired elapsed={:?}", t0.elapsed());

    // Restricted-token primary for the child.  RAII; closes the token
    // handle and removes the scratch dir on drop.
    let spec = RestrictedTokenSpec::new_for(policy)?;
    crate::dbg_trace!("sandbox-rt", "token-built elapsed={:?}", t0.elapsed());

    // The std handles are made inheritable so they can ride the
    // HANDLE_LIST allow-list; the list still restricts inheritance to
    // exactly these plus the IPC handle.
    let (in_h, out_h, err_h) = unsafe {
        (
            GetStdHandle(STD_INPUT_HANDLE) as HANDLE,
            GetStdHandle(STD_OUTPUT_HANDLE) as HANDLE,
            GetStdHandle(STD_ERROR_HANDLE) as HANDLE,
        )
    };
    for h in [in_h, out_h, err_h] {
        let ok = unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        if ok == 0 {
            return Err(stage_err(
                "std handle inherit",
                std::io::Error::last_os_error(),
            ));
        }
    }

    let argv = vec![
        super::SANDBOX_PROJECTION_FLAG.to_string(),
        policy_json,
        INTERNAL_EVAL_MODE.to_string(),
    ];

    // The lend scope brackets the spawn: the IPC handle is inheritable
    // only here, cleared the instant the closure returns.  The handle
    // value is stable across the lend, so the env block (which names it)
    // and the spawn (which inherits it) are both built inside.
    let spawned = endpoint
        .lend_for_spawn(|ipc_raw| {
            let ipc_h = ipc_raw as HANDLE;
            // Build the env block: inherit the current environment with
            // the IPC handle added, and strip any inherited confinement
            // marker.  The child receives its authentic marker only by
            // adopting the token from the IPC request; a forged marker
            // inherited here must not survive into the confined child.
            let env_block = build_child_env_block(
                &[(IPC_HANDLE_ENV, &(ipc_h as usize).to_string())],
                &[super::SANDBOX_ACTIVE_ENV],
            );
            ConfinedSpawn {
                exec_path: &s.exec_path,
                arg0: &s.arg0,
                argv_tail: &argv,
                spec: &spec,
                stdio: [in_h, out_h, err_h],
                inherit_ipc: ipc_h,
                env_block: &env_block,
            }
            .spawn()
        })
        .map_err(|e| stage_err("ipc lend", e))??;
    crate::dbg_trace!("sandbox-rt", "child-spawned elapsed={:?}", t0.elapsed());

    // Build the Job Object and assign before resuming.  Drop order
    // matters: job goes last so KILL_ON_JOB_CLOSE fires after the
    // restricted-token spec (and its scratch dir) is released.
    let job = JobHandle::build_confined().map_err(|e| stage_err("job setup", e))?;
    job.assign(spawned.process)
        .map_err(|e| stage_err("job assign", e))?;
    crate::dbg_trace!("sandbox-rt", "job-assigned elapsed={:?}", t0.elapsed());
    resume(&spawned)?;
    crate::dbg_trace!("sandbox-rt", "child-resumed elapsed={:?}", t0.elapsed());

    // Drop our copy of the child pipe endpoint so EOF propagates when the
    // child exits.
    drop(endpoint);
    crate::dbg_trace!(
        "sandbox-rt",
        "child-pipe-dropped elapsed={:?}",
        t0.elapsed()
    );

    let drive_result = channel.drive(request);
    crate::dbg_trace!(
        "sandbox-rt",
        "drive-done elapsed={:?} ok={}",
        t0.elapsed(),
        drive_result.is_ok(),
    );
    let exit_code = spawned.wait()?;
    crate::dbg_trace!(
        "sandbox-rt",
        "child-wait-done elapsed={:?} exit={}",
        t0.elapsed(),
        exit_code,
    );

    drop(spawned);
    drop(spec);
    drop(job);
    crate::dbg_trace!("sandbox-rt", "raii-dropped elapsed={:?}", t0.elapsed());

    drive_result.map_err(|e| match e {
        Break::Error(mut err) => {
            err.status = crate::types::Status::Code(exit_code);
            Break::Error(err)
        }
        other => other,
    })
}

/// Build a wide-string environment block from the parent's env plus
/// per-spawn overrides, with `strip` keys removed entirely.  Format:
/// `KEY=VALUE\0KEY=VALUE\0...\0`.
#[cfg(windows)]
fn build_child_env_block(extra: &[(&str, &str)], strip: &[&str]) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    let mut entries: Vec<String> = Vec::new();
    // Skip parent entries that we override or strip.
    for (k, v) in std::env::vars() {
        if extra.iter().any(|(ek, _)| k.eq_ignore_ascii_case(ek))
            || strip.iter().any(|sk| k.eq_ignore_ascii_case(sk))
        {
            continue;
        }
        entries.push(format!("{k}={v}"));
    }
    for (k, v) in extra {
        entries.push(format!("{k}={v}"));
    }
    let mut block: Vec<u16> = Vec::new();
    for e in entries {
        block.extend(OsStr::new(&e).encode_wide());
        block.push(0);
    }
    // A valid environment block ends in two NULs; an empty block is
    // `\0\0`.  Without an entry the loop emits no NUL, so an empty env
    // needs both terminators pushed here.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// Decode the wire response into the post-run mobile, the body's
/// outcome, the rehydrated audit fragment, and the surfaced events.
///
/// The outer `Err` is a *decode fault* — the wire payload couldn't be
/// turned back into runtime values — so there is no usable triple to
/// hand back at all.  The inner `Settled<Value>` is the body's outcome
/// that crossed the wire intact; a `Break::Error` here is a value the
/// child produced.  When the child shipped no mobile (a non-transferable
/// post-run mobile downgraded its success to an error), substitute a
/// local placeholder carrying the body's `last_status` so the contract
/// layer has a well-defined mobile to install / discard.
fn fold_response(
    response: ChildEvalResponse,
) -> Settled<(Mobile, Settled<Value>, AuditFragment, Vec<Value>)> {
    let decoded = decode_response(response)?;
    let mobile = decoded
        .mobile
        .unwrap_or_else(|| placeholder_mobile(decoded.last_status));
    let outcome = match decoded.signal {
        Some(signal) => Err(signal),
        None => Ok(decoded.value.unwrap_or(Value::Unit)),
    };
    let audit = AuditFragment::from_nodes(decoded.audit_nodes);
    Ok((mobile, outcome, audit, decoded.surface_events))
}

/// A bare mobile carrying only `last_status`, installed when the child
/// could not ship its post-run mobile.
fn placeholder_mobile(status: i32) -> Mobile {
    let mut mobile = Shell::default().mobile();
    mobile.control.last_status = status;
    mobile
}
