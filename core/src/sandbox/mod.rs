//! Process sandbox for capability-restricted execution.
//!
//! External commands spawned inside a `grant` block run under an OS-level
//! sandbox that enforces the declared filesystem, network, and exec
//! capabilities.  On Linux this is bubblewrap + seccomp; on macOS the
//! Seatbelt (sandbox-exec) API; on Windows a Job Object caps process count.
//!
//! The module is organised into platform backends (`linux`, `macos`,
//! `windows`), a subprocess spawner (`spawn`) that re-execs ral inside the
//! sandbox for builtin evaluation, and an IPC layer (`ipc`) that serialises
//! the evaluation request and response across the process boundary.
//!
//! Entry points:
//! - [`early_init`] — called once at program startup to consume
//!   `--sandbox-policy` and enter the OS sandbox.
//! - [`make_command`] — wraps an external command with the active sandbox
//!   policy (or applies resource limits inside a bare grant).
//! - [`apply_child_limits`] — post-spawn resource caps (Windows only;
//!   no-op on Unix where limits are set via `pre_exec`).

#[cfg(unix)]
mod ipc;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod spawn;
#[cfg(not(unix))]
mod windows;

use crate::types::{Shell, SandboxPolicy};
use std::process::{Command, ExitCode};

#[cfg(target_os = "linux")]
pub use linux::make_command_with_policy;
#[cfg(unix)]
pub use spawn::eval_grant;
#[cfg(not(unix))]
pub use windows::eval_grant;

/// CLI flag that carries the JSON-encoded [`SandboxPolicy`] into a
/// re-exec'd ral process.
const SANDBOX_POLICY_FLAG: &str = "--sandbox-policy";

/// Set in the environment of any ral process already running inside an
/// OS sandbox.  Children inherit the flag and consult it to avoid nested
/// initialization (which Seatbelt rejects on macOS, and which would just
/// be redundant under bwrap on Linux).
pub(super) const SANDBOX_ACTIVE_ENV: &str = "RAL_SANDBOX_ACTIVE";

/// Assign OS-level resource limits to an already-spawned child process.
///
/// On Unix the limits are applied before exec via a pre_exec hook in
/// make_command; this function is a no-op there.  On Windows, where
/// pre_exec does not exist, a Job Object is attached post-spawn to cap
/// the process tree at 512 processes (preventing fork bombs).
///
/// Call this immediately after spawn when the child is inside a grant block.
pub fn apply_child_limits(_child: &std::process::Child) {
    #[cfg(windows)]
    windows::apply_job_limits(_child);
}

/// Perform all sandbox startup work and return the stripped argument list.
///
/// Handles, in order: consuming --sandbox-policy from argv; recording the
/// current executable path for subprocess re-invocation; entering the OS
/// process sandbox when --sandbox-policy was given; dispatching
/// --internal-sandbox-block (the IPC subprocess mode).
///
/// Returns (stripped_argv, Some(code)) when the process should exit
/// immediately, (stripped_argv, None) to continue normally.
#[cfg(unix)]
pub fn early_init(argv: &[String]) -> Result<(Vec<String>, Option<ExitCode>), String> {
    let (policy, stripped) = strip_policy_arg(argv)?;
    if std::env::var_os(spawn::SANDBOX_SELF_ENV).is_none() {
        if let Ok(exe) = std::env::current_exe() {
            unsafe {
                std::env::set_var(spawn::SANDBOX_SELF_ENV, exe);
            }
        }
    }
    if let Some(code) = spawn::maybe_enter_process_sandbox(&stripped, policy.as_ref())? {
        return Ok((stripped, Some(code)));
    }
    if let Some(code) = spawn::maybe_handle_internal_mode(&stripped) {
        return Ok((stripped, Some(code)));
    }
    Ok((stripped, None))
}

/// Non-Unix fallback: strip the policy flag but take no sandbox action.
#[cfg(not(unix))]
pub fn early_init(argv: &[String]) -> Result<(Vec<String>, Option<ExitCode>), String> {
    let (_policy, stripped) = strip_policy_arg(argv)?;
    Ok((stripped, None))
}

/// Extract and deserialise `--sandbox-policy <json>` from `raw`, returning
/// the parsed policy (if any) and the remaining arguments.
fn strip_policy_arg(raw: &[String]) -> Result<(Option<SandboxPolicy>, Vec<String>), String> {
    let mut args = Vec::new();
    let mut policy = None;
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == SANDBOX_POLICY_FLAG {
            i += 1;
            if i >= raw.len() {
                return Err("ral: --sandbox-policy requires a JSON argument".into());
            }
            if policy.is_some() {
                return Err("ral: --sandbox-policy may only be provided once".into());
            }
            policy = Some(
                serde_json::from_str(&raw[i])
                    .map_err(|e| format!("ral: invalid sandbox policy JSON: {e}"))?,
            );
            i += 1;
            continue;
        }
        args.push(raw[i].clone());
        i += 1;
    }
    Ok((policy, args))
}

/// Install `pre_exec` hooks that zero `RLIMIT_CORE` (no core dumps) and
/// cap `RLIMIT_NPROC` at 512 (fork-bomb mitigation).  Unix only; a no-op
/// elsewhere so [`make_command`] can call it unconditionally.
#[cfg(unix)]
fn apply_resource_limits(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            let zero = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &zero);
            let nproc = libc::rlimit {
                rlim_cur: 512,
                rlim_max: 512,
            };
            libc::setrlimit(libc::RLIMIT_NPROC, &nproc);
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn apply_resource_limits(_cmd: &mut Command) {}

/// Build a [`Command`] for an external program.  OS-level sandboxing
/// happens *only* inside the sandboxed-child process spawned by
/// `eval_grant_sandboxed` — there the child enters Shell-mode Seatbelt /
/// bwrap once at startup and every command it spawns inherits the
/// confinement.  In the parent (and inside plugin handlers, which use
/// `with_capabilities` rather than `eval_grant`), externals run with the
/// user's full authority; the in-ral capability checks
/// (`check_fs_*`, `check_exec_*`) gate ral builtins and command-name
/// dispatch instead.
///
/// Resource limits (RLIMIT_CORE=0, RLIMIT_NPROC=512) still apply whenever
/// any capability layer is active, as cheap parent-side hardening that
/// doesn't depend on the OS sandbox API.
pub fn make_command(name: &str, args: &[String], shell: &Shell) -> Command {
    let mut c = Command::new(name);
    c.args(args);
    if shell.has_active_capabilities() {
        apply_resource_limits(&mut c);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::strip_policy_arg;
    use crate::types::SandboxPolicy;

    #[test]
    fn strip_policy_arg_extracts_json_and_preserves_other_args() {
        let (policy, args) = strip_policy_arg(&[
            "--sandbox-policy".into(),
            r#"{"fs":{"read_prefixes":["/tmp"],"write_prefixes":[]},"net":true}"#.into(),
            "-c".into(),
            "echo hi".into(),
        ])
        .expect("policy args");
        assert_eq!(
            policy,
            Some(SandboxPolicy {
                fs: crate::types::FsPolicy {
                    read_prefixes: vec!["/tmp".into()],
                    write_prefixes: Vec::new(),
                },
                net: true,
            })
        );
        assert_eq!(args, vec!["-c", "echo hi"]);
    }
}

#[cfg(all(test, unix))]
mod ipc_tests {
    use crate::serial::{InternCtx, SerialValue, build_arcs};
    use crate::types::Value;

    #[test]
    fn ipc_value_roundtrips_simple_values() {
        let value = Value::Map(vec![
            ("a".into(), Value::Int(1)),
            ("b".into(), Value::String("x".into())),
        ]);
        let mut ctx = InternCtx::new();
        let ipc = SerialValue::from_runtime(&value, &mut ctx).expect("to serial");
        let arcs = build_arcs(&ctx.scope_table).expect("build arcs");
        assert_eq!(ipc.into_runtime(&arcs).expect("from serial"), value);
    }
}
