//! Process sandbox for capability-restricted execution.
//!
//! External commands spawned inside a `grant` block run under an OS-level
//! sandbox that enforces the declared **filesystem and network**
//! capabilities.  Exec is *not* sandbox-enforced — both backends allow
//! unrestricted spawn (`(allow process-exec)` on macOS, no exec filter
//! under bwrap) and ral gates exec in-process before the spawn happens
//! (`EffectiveGrant::check_exec_args`); the OS layer is the depth-in-
//! defence for fs/net only.  On Linux the backend is bubblewrap +
//! seccomp; on macOS the Seatbelt (sandbox-exec) API; on Windows a Job
//! Object caps process count.
//!
//! The module is organised into platform backends (`linux`, `macos`,
//! `windows`), a subprocess spawner (`spawn`) that re-execs ral inside the
//! sandbox for builtin evaluation, and an IPC layer (`ipc`) that serialises
//! the evaluation request and response across the process boundary.
//!
//! Entry points:
//! - [`early_init`] — called once at program startup to consume
//!   `--sandbox-projection` and enter the OS sandbox.
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
mod reexec;
#[cfg(unix)]
mod runner;
#[cfg(not(unix))]
mod windows;

use crate::types::{Shell, SandboxProjection};
use std::process::{Command, ExitCode};

#[cfg(target_os = "linux")]
pub use linux::make_command_with_policy;
#[cfg(unix)]
pub use runner::eval_grant;
#[cfg(not(unix))]
pub use windows::eval_grant;

/// CLI flag that carries the JSON-encoded [`SandboxProjection`] into a
/// re-exec'd ral process.
const SANDBOX_PROJECTION_FLAG: &str = "--sandbox-projection";

/// Set in the environment of any ral process already running inside an
/// OS sandbox.  Children inherit the flag and consult it to avoid nested
/// initialization (which Seatbelt rejects on macOS, and which would just
/// be redundant under bwrap on Linux).  Also read by `capability_runtime`
/// to switch to lexical path resolution, since `canonicalize` can fail
/// under Seatbelt and the OS profile is the real gate anyway.
pub const SANDBOX_ACTIVE_ENV: &str = "RAL_SANDBOX_ACTIVE";

/// Undocumented debug switch.  When set (any value), front-ends call
/// [`dump_profile_if_requested`] on startup to print the OS-sandbox
/// profile they would install — Seatbelt SBPL on macOS, the bwrap argv
/// on Linux — to stderr, then continue normally.  Not surfaced in
/// `--help`: a primitive for sandbox development, same shelf as
/// `RUST_BACKTRACE`.
pub const SANDBOX_DUMP_PROFILE_ENV: &str = "RAL_DUMP_SANDBOX_PROFILE";

/// Render the OS sandbox profile for `policy` and print it to stderr
/// when [`SANDBOX_DUMP_PROFILE_ENV`] is set.  No-op otherwise.
///
/// Called by front-ends at startup (e.g. exarch's `main`) so the user
/// can inspect what *would* be installed without having to enter a
/// real sandbox.  Pass `SandboxProjection::default()` to see the
/// baseline (system reads + canned defaults); pass an actual policy
/// to see how user grants extend it.
#[cfg_attr(windows, allow(unused_variables))]
pub fn dump_profile_if_requested(policy: &crate::types::SandboxProjection) {
    if std::env::var_os(SANDBOX_DUMP_PROFILE_ENV).is_none() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let profile = macos::build_profile(policy);
        eprintln!("--- seatbelt profile ---\n{profile}\n--- end seatbelt profile ---");
    }
    #[cfg(target_os = "linux")]
    {
        let cmd = linux::make_command_with_policy("/bin/true", &[], policy);
        let mut line = String::from("bwrap");
        for arg in cmd.get_args() {
            line.push(' ');
            line.push_str(&arg.to_string_lossy());
        }
        eprintln!("--- bwrap argv ---\n{line}\n--- end bwrap argv ---");
    }
}

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
/// Handles, in order: consuming --sandbox-projection from argv; recording the
/// current executable path for subprocess re-invocation; entering the OS
/// process sandbox when --sandbox-projection was given; dispatching
/// --internal-sandbox-block (the IPC subprocess mode).
///
/// Returns (stripped_argv, Some(code)) when the process should exit
/// immediately, (stripped_argv, None) to continue normally.
#[cfg(unix)]
pub fn early_init(argv: &[String]) -> Result<(Vec<String>, Option<ExitCode>), String> {
    let (policy, stripped) = strip_policy_arg(argv)?;
    // Pin this binary's executable inode so any later restrictive
    // `grant { … }` block re-execs *us*, immune to on-disk swaps.
    reexec::register_sandbox_self();
    if let Some(code) = reexec::maybe_enter_process_sandbox(&stripped, policy.as_ref())? {
        return Ok((stripped, Some(code)));
    }
    if let Some(code) = reexec::maybe_handle_internal_mode(&stripped) {
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

/// Extract and deserialise `--sandbox-projection <json>` from `raw`, returning
/// the parsed policy (if any) and the remaining arguments.
fn strip_policy_arg(raw: &[String]) -> Result<(Option<SandboxProjection>, Vec<String>), String> {
    let mut args = Vec::new();
    let mut policy = None;
    let mut iter = raw.iter();
    while let Some(arg) = iter.next() {
        if arg != SANDBOX_PROJECTION_FLAG {
            args.push(arg.clone());
            continue;
        }
        let json = iter
            .next()
            .ok_or("ral: --sandbox-projection requires a JSON argument")?;
        if policy.is_some() {
            return Err("ral: --sandbox-projection may only be provided once".into());
        }
        policy = Some(
            serde_json::from_str(json).map_err(|e| format!("ral: invalid sandbox policy JSON: {e}"))?,
        );
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
    use crate::types::SandboxProjection;

    #[test]
    fn strip_policy_arg_extracts_json_and_preserves_other_args() {
        let (policy, args) = strip_policy_arg(&[
            "--sandbox-projection".into(),
            r#"{"fs":{"kind":"restricted","policy":{"read_prefixes":["/tmp"],"write_prefixes":[]}},"net":true}"#.into(),
            "-c".into(),
            "echo hi".into(),
        ])
        .expect("policy args");
        assert_eq!(
            policy,
            Some(SandboxProjection {
                fs: crate::types::FsProjection::Restricted(crate::types::FsPolicy {
                    read_prefixes: vec!["/tmp".into()],
                    write_prefixes: Vec::new(),
                    deny_paths: Vec::new(),
                }),
                net: true,
                exec: crate::types::ExecProjection::default(),
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
