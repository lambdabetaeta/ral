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
//! `windows`, `windows_restricted_token`) and a per-command launcher
//! (`launch`) that confines a single external/bundled child under the
//! platform backend.
//!
//! Entry points:
//! - [`early_init`] — called once at program startup to consume
//!   `--sandbox-projection`, pin the binary, and (on Unix) enter the
//!   OS process sandbox for the per-command `--sandbox-projection` child.
//! - [`make_command`] — builds the external [`Command`] and, when a
//!   capability grant is active, installs the resource-limit `pre_exec`
//!   hooks (no OS policy wrapping).
//! - [`apply_child_limits`] — post-spawn resource caps (Windows only;
//!   no-op on Unix where limits are set via `pre_exec`).

mod diag;
mod launch;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod reexec;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_restricted_token;

use crate::types::{SandboxProjection, Shell};
use std::process::Command;
use std::sync::OnceLock;

/// Host-supplied callback that a re-exec'd child runs against its fresh
/// shell so host-owned builtin tables join `CORE_BUILTINS` before any
/// body is evaluated.
///
/// `Shell::new` installs only `CORE_BUILTINS`.  A host like `exarch`
/// publishes additional static entries (`explore-dir`, `line-hash`,
/// …) that core cannot link, and the mobile transfer cannot carry the
/// `BuiltinTable` either (the entries hold function pointers, which
/// are not valid across process address spaces).  A pipeline-stage
/// helper is a re-execed copy of the host binary, so the host registers
/// this hook before [`early_init`]; the child invokes it on its
/// `Shell::new` *before* [`crate::subprocess::install_shell_mobile`]
/// runs.  `install_shell_mobile` preserves the receiver's builtin
/// table by design, so hook-installed entries survive the mobile
/// install and the body sees the same surface as the parent.
///
/// `OnceLock` makes the registration last for the process lifetime
/// and idempotent on second-time calls — handy for test harnesses
/// that exercise `main` more than once.
static CHILD_SHELL_HOOK: OnceLock<fn(&mut Shell)> = OnceLock::new();

/// Register the host shell-extension hook.  Must be called before
/// [`early_init`]; subsequent calls are silently ignored.
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

// Per-command OS sandbox launcher: `build_command` routes an external /
// bundled child through here when the process is not already confined and
// a projection is active.  `serve_sandbox_exec` is the macOS post-Seatbelt
// `execve` hook for the host re-exec tail.
pub use launch::serve_sandbox_exec;
pub(crate) use launch::{LaunchTarget, sandboxed_command};

// Kernel-denial diagnostics: the command runners call these on a failure
// that ran under an active OS sandbox to attach an actionable hint
// naming the denied path and how to grant it.
pub(crate) use diag::{augment_failure, sample_descendants};

/// Whether this platform's OS backend can actually enforce a network
/// restriction (`net: false`).  Linux (`--unshare-net`) and macOS
/// (deny-default Seatbelt) can; the Windows restricted-token backend has
/// no kernel network enforcement.
fn net_enforced() -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        true
    }
    #[cfg(windows)]
    {
        false
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        false
    }
}

/// Returns `Err(reason)` when `projection` requests a restriction this
/// platform's OS backend cannot actually enforce. The only such axis is
/// offline/`net`: a `net: false` projection on a backend without kernel
/// network enforcement must error rather than run where net is ignored.
pub(crate) fn projection_enforceable(projection: &SandboxProjection) -> Result<(), &'static str> {
    if !projection.net && !net_enforced() {
        return Err(
            "offline mode (net: false) is unsupported on this platform: \
                    no kernel network enforcement exists",
        );
    }
    Ok(())
}

/// CLI flag that carries the JSON-encoded [`SandboxProjection`] into a
/// re-exec'd ral process.
const SANDBOX_PROJECTION_FLAG: &str = "--sandbox-projection";

/// macOS-only sentinel for the per-command host re-exec tail
/// (`ral --sandbox-projection <json> --ral-sandbox-exec <program> <args…>`).
/// After `early_init` enters Seatbelt, [`serve_sandbox_exec`] `execve`s the
/// program inside that Seatbelt.  Distinct from `--ral-bundled-tool`, which
/// runs a bundled tool in-process confined instead of execing a host binary.
#[cfg(target_os = "macos")]
const SANDBOX_EXEC_FLAG: &str = "--ral-sandbox-exec";

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
        let cmd = linux::make_command_with_policy("/bin/true", &[], policy, None);
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
/// On Unix the limits are applied before exec via a `pre_exec` hook in
/// `make_command`; this function is a no-op there.  On Windows, where
/// `pre_exec` does not exist, a Job Object is attached post-spawn to cap
/// the process tree at 512 processes (preventing fork bombs).
pub fn apply_child_limits(_child: &crate::process::ChildHandle) {
    #[cfg(windows)]
    windows::apply_job_limits(_child);
}

/// Same as [`apply_child_limits`] but for a child that is already a
/// member of a pipeline Job Object.
pub fn apply_child_limits_in_pipeline(
    _child: &crate::process::ChildHandle,
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
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:self-reexec] Builds the ral-re-exec Command for sandbox helper subprocesses (pipeline helper, bundled-tool multicall). Infrastructure spawn, not a model exec image — the model's exec surfaces at command::run, not here."
)]
pub(crate) fn self_command() -> std::io::Result<Command> {
    use std::os::unix::process::CommandExt;

    if let Some(s) = reexec::SANDBOX_SELF.get() {
        let mut cmd = Command::new(&s.exec_path);
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
/// Windows the sandbox is applied by the parent at child spawn time).
pub fn early_init(argv: &[String]) -> Result<(Vec<String>, Option<u8>), String> {
    let (policy, stripped) = strip_policy_arg(argv)?;
    // Pin this binary's executable so a per-command `--sandbox-projection`
    // child re-execs *us*, immune to on-disk swaps.
    reexec::register_sandbox_self();
    if let Some(code) = reexec::maybe_enter_process_sandbox(&stripped, policy.as_ref())? {
        return Ok((stripped, Some(code)));
    }
    Ok((stripped, None))
}

/// The OS-sandbox stage of the pre-`main` dispatch, as an `Option<u8>`
/// building block:
///
/// run [`early_init`] over the process argv and, when this
/// process is the Linux bwrap respawn child, surface its exit code so the
/// caller can terminate. A normal top-level invocation yields `None`.
/// `early_init` also pins `SANDBOX_SELF` here. The stripped argv is
/// otherwise discarded — callers that need to parse a CLI from it (the
/// `ral` binary) call `early_init` directly.
///
/// After sandbox setup this also serves the per-command re-exec tails on
/// the post-`early_init` argv (Seatbelt / bwrap already applied): the
/// `--ral-bundled-tool` multicall (via [`crate::try_run_bundled_tool`])
/// runs a bundled tool in-process confined, and on macOS
/// [`serve_sandbox_exec`] `execve`s the host program for a per-command
/// host launch.  Both are reachable from the test ctors and the exarch
/// frontend that route their pre-`main` dispatch through here — exactly as
/// the `ral` binary serves them on its own `early_init` result. The order
/// matters: a `--sandbox-projection` child enters the OS sandbox in
/// `early_init` first, then runs the tool / host binary confined.
pub fn serve_sandbox_early_init() -> Option<u8> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match early_init(&argv) {
        Ok((_, Some(code))) => Some(code),
        Ok((stripped, None)) => {
            serve_sandbox_exec(&stripped).or_else(|| crate::try_run_bundled_tool(&stripped))
        }
        Err(e) => {
            eprintln!("ral: sandbox init: {e}");
            Some(1)
        }
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
            libc::setrlimit(libc::RLIMIT_CORE, &raw const zero);
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
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:make-command] Builds the external exec image (ExecImage::Host) the model launches. The exec card is fused onto this image at command::run, which emits the exec event with the resolved argv and exit status when the spawn/wait completes."
)]
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
    use super::{net_enforced, projection_enforceable, strip_policy_arg};
    use crate::types::SandboxProjection;

    #[test]
    fn projection_enforceable_allows_net_true_on_any_platform() {
        // `net: true` requests no network restriction, so there is nothing
        // for the OS backend to enforce — it is `Ok` everywhere.
        let p_net_true = SandboxProjection {
            fs: crate::types::FsProjection::default(),
            net: true,
            exec: crate::types::ExecProjection::default(),
        };
        assert!(projection_enforceable(&p_net_true).is_ok());
    }

    #[test]
    fn projection_enforceable_net_false_tracks_net_enforced() {
        // The guard's invariant, stated relationally so it holds on any
        // host: a `net: false` projection is enforceable exactly when this
        // platform's OS backend can enforce a network restriction.
        let p_net_false = SandboxProjection {
            fs: crate::types::FsProjection::default(),
            net: false,
            exec: crate::types::ExecProjection::default(),
        };
        assert_eq!(projection_enforceable(&p_net_false).is_ok(), net_enforced());
    }

    #[cfg(windows)]
    #[test]
    fn projection_enforceable_rejects_net_false_on_windows() {
        // Windows has no kernel network enforcement, so a `net: false`
        // projection must fail closed rather than run with net ignored.
        let p_net_false = SandboxProjection {
            fs: crate::types::FsProjection::default(),
            net: false,
            exec: crate::types::ExecProjection::default(),
        };
        let err = projection_enforceable(&p_net_false)
            .expect_err("net: false must be rejected where net is unenforceable");
        assert!(
            err.contains("offline mode"),
            "fail-closed message must explain the offline restriction; got: {err:?}"
        );
    }

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
