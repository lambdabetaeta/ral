//! Process sandbox for capability-restricted execution.
//!
//! External commands spawned inside a `grant` block run under an OS-level
//! sandbox that enforces the declared **filesystem and network**
//! capabilities.  Exec is gated in-process on every platform —
//! `capability::check_exec_args` vets the arguments before the spawn — and
//! on macOS the Seatbelt profile additionally renders a `process-exec`
//! allow-list, catching the re-execs the in-process check never sees
//! (`sh -c`, `find -exec`); bwrap on Linux has no path-exec filter, so
//! there the in-process gate stands alone.
//!
//! The module is organised into platform backends (`linux`, `macos`,
//! `windows`, `windows_restricted_token`), a subprocess runner (`runner`)
//! that re-execs ral inside the sandbox for confined evaluation, and
//! an IPC layer (`ipc`) that serialises the evaluation request and
//! response across the process boundary.
//!
//! Entry points:
//! - [`early_init`] — called once at program startup to consume
//!   `--sandbox-projection`, pin the binary, and (on Unix) enter the
//!   OS process sandbox; also dispatches `--internal-sandbox-block`
//!   into the IPC child loop.
//! - [`make_command`] — builds the external [`Command`] and, when a
//!   capability grant is active, installs the resource-limit `pre_exec`
//!   hooks (no OS policy wrapping).
//! - [`apply_child_limits`] — post-spawn resource caps (Windows only;
//!   no-op on Unix where limits are set via `pre_exec`).

mod ipc;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod marker;
mod reexec;
mod runner;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_restricted_token;

use crate::types::{SandboxProjection, Shell};
use std::process::{Command, ExitCode};
use std::sync::OnceLock;

/// Host-supplied callback that the IPC child runs against its fresh
/// shell so host-owned builtin tables join `CORE_BUILTINS` before any
/// body is evaluated.
///
/// `Shell::new` installs only `CORE_BUILTINS`.  A host like `exarch`
/// publishes additional static entries (`explore-dir`, `line-hash`,
/// …) that core cannot link, and the mobile transfer cannot carry the
/// `BuiltinTable` either (the entries hold function pointers, which
/// are not valid across process address spaces).  The IPC child is a
/// re-execed copy of the host binary, so the host registers this hook
/// before [`early_init`] dispatches the child; the child invokes it on
/// its `Shell::new` *before* [`crate::subprocess::install_shell_mobile`]
/// runs.  `install_shell_mobile` preserves the receiver's builtin
/// table by design, so hook-installed entries survive the mobile
/// install and the body sees the same surface as the parent.
///
/// `OnceLock` makes the registration last for the process lifetime
/// and idempotent on second-time calls — handy for test harnesses
/// that exercise `main` more than once.
static CHILD_SHELL_HOOK: OnceLock<fn(&mut Shell)> = OnceLock::new();

/// Register the host shell-extension hook.  Must be called before
/// [`early_init`] reaches `--internal-sandbox-block`; subsequent calls
/// are silently ignored.
pub fn set_child_shell_extension(hook: fn(&mut Shell)) {
    let _ = CHILD_SHELL_HOOK.set(hook);
}

/// Run the registered host shell-extension hook on `shell`, if any.
pub(crate) fn run_child_shell_extension(shell: &mut Shell) {
    if let Some(hook) = CHILD_SHELL_HOOK.get() {
        hook(shell);
    }
}

#[cfg(target_os = "linux")]
pub use linux::make_command_with_policy;

// Confined-eval entry point.  `evaluator` routes here once
// `confined_availability()` returns `Ready`.
pub(crate) use runner::run_confined;

// Authenticate the inherited confinement marker against the capability
// token a genuine sandbox re-exec recorded.  The transport gate and the
// access-side resolver consult this instead of the marker's mere
// presence; the IPC child adopts the token via `marker::adopt`.
pub(crate) use marker::adopt as adopt_sandbox_token;
pub(crate) use marker::authenticated as marker_authenticated;
pub(crate) use marker::constant_time_eq;

/// Test seam: simulate a genuinely-confined child by adopting a
/// capability token the way the IPC child does when it receives an
/// authenticated request, letting an integration test reach the
/// authenticated-marker state.  This is a forging primitive — it mints
/// and adopts a fresh token, flipping the process into the confined
/// state for its remaining lifetime — so it is gated behind the
/// `test-util` feature and absent from default builds.  Returns the
/// minted token.
#[cfg(feature = "test-util")]
#[doc(hidden)]
pub fn adopt_token_for_test() -> String {
    let token = marker::mint();
    marker::adopt(&token);
    token
}

/// Whether the sandbox-confined evaluation transport is usable in this
/// process, independent of any particular body.  The neutral
/// [`crate::evaluator`] queries this to decide between local and
/// confined dispatch when a sandbox projection is active.
#[derive(Debug)]
pub enum ConfinedAvailability {
    /// Backend is ready: platform supports a sandbox profile and our
    /// own executable is pinned for re-exec.
    Ready,
    /// Backend cannot be used; the `&'static str` carries a reason
    /// suitable for surfacing in a `fail-closed` error.
    Unavailable(&'static str),
}

/// Probe the confined-eval backend without inspecting any body.
pub fn confined_availability() -> ConfinedAvailability {
    #[cfg(unix)]
    {
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            return ConfinedAvailability::Unavailable(
                "this Unix platform has no fs/net process sandbox backend",
            );
        }
        if reexec::SANDBOX_SELF.get().is_none() {
            return ConfinedAvailability::Unavailable(
                "failed to pin the current executable for sandbox re-exec",
            );
        }
        ConfinedAvailability::Ready
    }
    #[cfg(windows)]
    {
        if reexec::SANDBOX_SELF.get().is_none() {
            return ConfinedAvailability::Unavailable(
                "failed to pin the current executable for sandbox re-exec",
            );
        }
        if !windows_restricted_token::backend_ready() {
            return ConfinedAvailability::Unavailable(
                "restricted-token backend is not available on this Windows host",
            );
        }
        ConfinedAvailability::Ready
    }
    #[cfg(not(any(unix, windows)))]
    {
        ConfinedAvailability::Unavailable("sandbox confinement unavailable on this platform")
    }
}

/// CLI flag that carries the JSON-encoded [`SandboxProjection`] into a
/// re-exec'd ral process.
const SANDBOX_PROJECTION_FLAG: &str = "--sandbox-projection";

/// Marks a ral process as running inside an OS sandbox.  Its mere
/// presence is *not* trusted as proof of confinement — an arbitrary
/// parent can export it (review findings S1, S2).  A genuinely-confined
/// child stamps the per-re-exec capability token from its IPC request
/// into this var via [`marker::adopt`]; [`marker::authenticated`] trusts
/// the marker only when its value equals that recorded token.  The
/// presence reads that remain (`self_command`, the nested-respawn guard)
/// are not enforcement gates — they fail safe under a forged value.
pub const SANDBOX_ACTIVE_ENV: &str = "RAL_SANDBOX_ACTIVE";

/// Undocumented debug switch.  When set (any value), front-ends call
/// [`dump_profile_if_requested`] on startup to print the OS-sandbox
/// profile they would install.
pub const SANDBOX_DUMP_PROFILE_ENV: &str = "RAL_DUMP_SANDBOX_PROFILE";

/// Render the OS sandbox profile for `policy` and print it to stderr
/// when [`SANDBOX_DUMP_PROFILE_ENV`] is set.  No-op otherwise.
// A sandbox marker presence probe, not a basedir.
#[allow(clippy::disallowed_methods)]
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
    #[cfg(windows)]
    {
        let dump = windows_restricted_token::dump_profile_for_windows(policy);
        eprintln!("--- restricted-token profile ---\n{dump}--- end restricted-token profile ---");
    }
}

/// Assign OS-level resource limits to an already-spawned child process.
///
/// On Unix the limits are applied before exec via a pre_exec hook in
/// make_command; this function is a no-op there.  On Windows, where
/// pre_exec does not exist, a Job Object is attached post-spawn to cap
/// the process tree at 512 processes (preventing fork bombs).
pub fn apply_child_limits(_child: &std::process::Child) {
    #[cfg(windows)]
    windows::apply_job_limits(_child);
}

/// Same as [`apply_child_limits`] but for a child that is already a
/// member of a pipeline Job Object.
pub fn apply_child_limits_in_pipeline(
    _child: &std::process::Child,
    _leader: Option<crate::process::Pgid>,
) {
    #[cfg(windows)]
    {
        match _leader {
            Some(crate::process::Pgid(p)) if crate::process::is_known_group(p) => {
                if !crate::process::apply_group_active_process_limit(p, 512) {
                    eprintln!("ral: warning: failed to apply active-process limit to pipeline job");
                }
            }
            _ => windows::apply_job_limits(_child),
        }
    }
}

/// Register the current executable for helper subprocesses that run before
/// `early_init` reaches the normal sandbox setup path.  Used by the Unix
/// pipeline-helper re-exec path; the Windows pipeline helper goes through
/// `early_init` directly.
#[cfg(unix)]
pub(crate) fn register_self_for_helpers() {
    reexec::register_sandbox_self();
}

/// Build a `Command` that re-execs the current ral binary.
///
/// Prefers the pinned sandbox self-path when available so helper subprocesses
/// stay bound to the boot-time binary even if the on-disk path changes.
#[cfg(unix)]
// SANDBOX_ACTIVE_ENV is a confinement marker presence probe, not a basedir.
#[allow(clippy::disallowed_methods)]
pub(crate) fn self_command() -> std::io::Result<Command> {
    use std::os::unix::process::CommandExt;

    if let Some(s) = reexec::SANDBOX_SELF.get() {
        let exec = if std::env::var_os(SANDBOX_ACTIVE_ENV).is_some() {
            &s.arg0
        } else {
            &s.exec_path
        };
        let mut cmd = Command::new(exec);
        cmd.arg0(&s.arg0);
        return Ok(cmd);
    }
    let exe = std::env::current_exe()?;
    Ok(Command::new(exe))
}

/// Perform all sandbox startup work and return the stripped argument list.
///
/// Handles, in order: consuming --sandbox-projection from argv; recording the
/// current executable path for subprocess re-invocation; entering the OS
/// process sandbox when --sandbox-projection was given (Unix only — on
/// Windows the sandbox is applied by the parent at child spawn time);
/// dispatching --internal-sandbox-block (the IPC subprocess mode).
pub fn early_init(argv: &[String]) -> Result<(Vec<String>, Option<ExitCode>), String> {
    let (policy, stripped) = strip_policy_arg(argv)?;
    // Pin this binary's executable so any later restrictive
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

/// Run [`early_init`] from a test binary's pre-`main` `#[ctor]` and, when
/// this process is a re-exec child (a `grant { … }` block's
/// `--internal-sandbox-block` IPC server, or the Linux bwrap respawn),
/// terminate it before libtest sees the flags it would reject.  A normal
/// top-level test invocation returns, letting libtest run; `early_init`
/// also pins `SANDBOX_SELF`, so the confined-transport availability probe
/// reports `Ready` for tests that exercise the confined path.
///
/// The binary's own `main` keeps the faithful child [`ExitCode`]
/// `early_init` returns.  A `#[ctor]` cannot — `ExitCode` is opaque to
/// `std::process::exit` — so it maps success to `0` and an `early_init`
/// error to `1`; the genuine result of an internal-block child travels
/// back over the IPC channel, and any error detail is on stderr, so the
/// child's own exit code is not what the parent reads.  This is the one
/// place every ral-family test ctor expresses that mapping.
pub fn early_init_or_exit_for_test_ctor() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match early_init(&argv) {
        Ok((_, Some(_))) => std::process::exit(0),
        Err(e) => {
            eprintln!("ral test harness: sandbox early_init: {e}");
            std::process::exit(1);
        }
        Ok((_, None)) => {}
    }
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
            serde_json::from_str(json)
                .map_err(|e| format!("ral: invalid sandbox policy JSON: {e}"))?,
        );
    }
    Ok((policy, args))
}

/// Install `pre_exec` hooks for cheap resource hardening.
///
/// Core dumps are disabled on every Unix sandboxed child.  On non-macOS
/// Unix we also cap `RLIMIT_NPROC` at 512 as fork-bomb mitigation.
/// Darwin counts `RLIMIT_NPROC` against the whole real UID rather than the
/// sandboxed process tree, so lowering it here can leave only a handful of
/// spawn slots when the desktop session is already busy (native builds then
/// fail with `EAGAIN` / "Resource temporarily unavailable").  Seatbelt
/// still gates `process-fork` and `process-exec`; macOS therefore skips the
/// per-UID process cap until we have a subtree-scoped mechanism there.
#[cfg(unix)]
pub(crate) fn apply_resource_limits(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            let zero = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &zero);
            #[cfg(not(target_os = "macos"))]
            {
                let nproc = libc::rlimit {
                    rlim_cur: 512,
                    rlim_max: 512,
                };
                libc::setrlimit(libc::RLIMIT_NPROC, &nproc);
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn apply_resource_limits(_cmd: &mut Command) {}

/// Build a [`Command`] for an external program.
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
        let value = Value::map(vec![
            ("a".into(), Value::Int(1)),
            ("b".into(), Value::String("x".into())),
        ]);
        let mut ctx = InternCtx::new();
        let ipc = SerialValue::from_runtime(&value, &mut ctx).expect("to serial");
        let arcs = build_arcs(&ctx.scope_table).expect("build arcs");
        assert_eq!(ipc.into_runtime(&arcs).expect("from serial"), value);
    }
}
