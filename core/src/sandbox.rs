//! OS-level confinement for the children a `grant` block spawns.
//!
//! Platform backends (`linux`, `macos`, `windows`), the per-command
//! launcher (`launch`), binary pinning and re-exec (`reexec`), kernel-denial
//! diagnostics (`diag`).
//!
//! Exec is gated in-process everywhere by `capability::check_exec_args`.
//! macOS also renders a Seatbelt `process-exec` allow-list, catching the
//! re-execs that check never sees (`sh -c`, `find -exec`); bwrap has no
//! path-exec filter, so on Linux the in-process gate stands alone.

mod diag;
mod launch;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod reexec;
#[cfg(windows)]
mod windows;

use crate::types::{SandboxProjection, Shell};
use std::process::Command;
use std::sync::OnceLock;

/// Host-supplied constructor a re-exec'd child calls to rebuild its
/// [`HostSurface`](crate::boot::HostSurface): a mobile cannot carry one across
/// processes, its entries being function pointers, and `Shell::new` installs
/// core's manifest alone.  `subprocess::bare_child_shell` runs the hook before
/// any [`crate::serial::WireDecoder`] is built, so the child's own manifest is
/// what re-links a captured native's name.
static CHILD_SHELL_HOOK: OnceLock<fn() -> crate::boot::HostSurface> = OnceLock::new();

/// Register the host's builtin surface for re-exec'd children.  Must be
/// called before [`early_init`]; subsequent calls are silently ignored.
pub fn set_child_shell_extension(surface: fn() -> crate::boot::HostSurface) {
    let _ = CHILD_SHELL_HOOK.set(surface);
}

pub(crate) fn run_child_shell_extension(shell: &mut Shell) {
    if let Some(surface) = CHILD_SHELL_HOOK.get() {
        surface().install_into_shell(shell);
    }
}

// `runtime::command::process::build_command` routes an external/bundled child
// through `sandboxed_command` when a projection is active and no guest jail
// already confines it; `serve_sandbox_exec` is the macOS post-Seatbelt tail.
pub use launch::serve_sandbox_exec;
pub(crate) use launch::{LaunchTarget, Ownership, sandboxed_command};

// Called by the command runners on a failure that ran under an active OS
// sandbox, to attach a hint naming the denied path.
pub(crate) use diag::{augment_failure, sample_descendants};

/// Whether this platform can enforce `net: false`: Linux via `--unshare-net`,
/// macOS via deny-default Seatbelt, Windows via an `AppContainer` with no
/// network capability SID, which cannot open a socket.
fn net_enforced() -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        true
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        false
    }
}

/// The one refusal for "this host cannot establish the confinement the active
/// grant asks for".  Whether we learn it before the spawn — an axis no backend
/// here enforces — or from the spawn itself, when the envelope binary turns out
/// to be missing, the user is owed the same answer: nothing ran, and the
/// sandbox is why.  Never the command's fault, so never phrased as such.
pub(crate) fn confinement_unavailable(reason: &str) -> crate::types::Error {
    crate::types::Error::new(format!("sandbox confinement unavailable: {reason}"), 1)
}

/// `Err(reason)` when `projection` asks for a restriction this platform cannot
/// enforce.  Offline is the only such axis: `net: false` must refuse rather
/// than run somewhere the bit is silently ignored.
pub(crate) fn projection_enforceable(projection: &SandboxProjection) -> Result<(), &'static str> {
    if !projection.net && !net_enforced() {
        return Err(
            "offline mode (net: false) is unsupported on this platform: \
                    no kernel network enforcement exists",
        );
    }
    Ok(())
}

/// Carries the JSON-encoded [`SandboxProjection`] into a re-exec'd ral process.
const SANDBOX_PROJECTION_FLAG: &str = "--sandbox-projection";

/// Tail of the macOS per-command host re-exec: once `early_init` has entered
/// Seatbelt, [`serve_sandbox_exec`] `execve`s the program inside it.  Distinct
/// from `--ral-bundled-tool`, which runs a bundled tool in-process rather than
/// execing a host binary.
#[cfg(target_os = "macos")]
const SANDBOX_EXEC_FLAG: &str = "--ral-sandbox-exec";

/// Debug switch: set to any value to make [`dump_profile_if_requested`] print
/// the OS-sandbox profile that would be installed.
pub const SANDBOX_DUMP_PROFILE_ENV: &str = "RAL_DUMP_SANDBOX_PROFILE";

/// Print the OS-sandbox profile for `policy` to stderr when
/// [`SANDBOX_DUMP_PROFILE_ENV`] is set.
// A presence probe on a debug switch, not a basedir lookup.
#[allow(clippy::disallowed_methods)]
pub fn dump_profile_if_requested(policy: &crate::types::SandboxProjection) {
    if std::env::var_os(SANDBOX_DUMP_PROFILE_ENV).is_none() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        match macos::build_profile(policy) {
            Ok(profile) => {
                eprintln!("--- seatbelt profile ---\n{profile}\n--- end seatbelt profile ---");
            }
            Err(e) => eprintln!("--- seatbelt profile error ---\n{e}"),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match linux::make_command_with_policy(
            "/bin/true",
            &[],
            policy,
            None,
            launch::Ownership::Kept,
        ) {
            Ok(cmd) => {
                let mut line = String::from("bwrap");
                for arg in cmd.get_args() {
                    line.push(' ');
                    line.push_str(&arg.to_string_lossy());
                }
                eprintln!("--- bwrap argv ---\n{line}\n--- end bwrap argv ---");
            }
            Err(e) => eprintln!("--- bwrap argv error ---\n{e}"),
        }
    }
    #[cfg(windows)]
    {
        let dump = windows::dump_profile_for_windows(policy);
        eprintln!("--- appcontainer profile ---\n{dump}--- end appcontainer profile ---");
    }
}

/// Live-process ceiling on a grant-confined child's Job Object: fork-bomb cap.
#[cfg(windows)]
pub(crate) const ACTIVE_PROCESS_CAP: u32 = 512;

/// Assign OS-level resource limits to an already-spawned child.
///
/// Windows only: `pre_exec` does not exist there, so a Job Object caps the tree
/// at `ACTIVE_PROCESS_CAP` after the spawn.  On Unix `apply_resource_limits`
/// set them before exec, and this is a no-op.
#[cfg_attr(
    not(windows),
    allow(
        unused_variables,
        reason = "the child is read only by the Windows Job Object arm; elsewhere the limits are already in place from pre-exec, and this is a no-op"
    )
)]
pub fn apply_child_limits(child: &crate::process::ChildHandle) {
    #[cfg(windows)]
    windows::apply_job_limits(child);
}

/// Whether a `bwrap` envelope on this host can actually open a device node it
/// binds in — proven false, non-deterministically, on some GitHub-hosted CI
/// runners (`/bin/sh: cannot open /dev/null: Permission denied`) even with
/// every AppArmor/userns knob tried, on hosts where `bwrap` itself launches
/// fine. A mount-layer property of the host, not a policy this crate can ask
/// for, so it is probed rather than inferred from `bwrap`'s mere presence.
///
/// Test-only: the caller is one of the sandbox-dependent tests deciding
/// whether to skip, per [`confinement_unavailable`]'s "nothing ran, and the
/// sandbox is why" — never a real spawn path, which already fails closed on
/// the same underlying error.
#[cfg(all(target_os = "linux", any(test, feature = "test-util")))]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:bwrap-devnull-probe] A host capability probe for the test suite, never a spawn a grant or the model can observe."
)]
pub fn bwrap_devnull_writable() -> bool {
    use std::sync::OnceLock;
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(|| {
        std::process::Command::new(linux::BWRAP)
            .args([
                "--unshare-net",
                "--dev",
                "/dev",
                "--",
                "/bin/sh",
                "-c",
                "echo x > /dev/null",
            ])
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// [`apply_child_limits`] for a child already in a pipeline Job Object.
#[cfg_attr(
    not(windows),
    allow(
        unused_variables,
        reason = "child and leader are read only by the Windows Job Object arm; elsewhere this is a no-op"
    )
)]
pub fn apply_child_limits_in_pipeline(
    child: &crate::process::ChildHandle,
    leader: Option<crate::process::Pgid>,
) {
    #[cfg(windows)]
    {
        match leader {
            Some(group) if crate::process::is_known_group(group.as_raw()) => {
                if !crate::process::apply_group_active_process_limit(
                    group.as_raw(),
                    ACTIVE_PROCESS_CAP,
                ) {
                    eprintln!("ral: warning: failed to apply active-process limit to pipeline job");
                }
            }
            _ => windows::apply_job_limits(child),
        }
    }
}

/// Pin this executable for the Unix pipeline-stage helper, which serves its
/// mode and exits before [`early_init`] would have pinned it.
#[cfg(unix)]
pub(crate) fn register_self_for_helpers() {
    reexec::register_sandbox_self();
}

/// Re-exec the current ral binary, preferring the pinned self-path so helpers
/// stay bound to the boot-time build even if the on-disk path is swapped.
#[cfg(unix)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:self-reexec] Builds the ral-re-exec Command for sandbox helper subprocesses (pipeline helper, bundled-tool multicall). Infrastructure spawn, not a model exec image — the model's exec surfaces at command::run, not here."
)]
pub(crate) fn self_command() -> std::io::Result<Command> {
    if let Some(s) = reexec::SANDBOX_SELF.get() {
        return Ok(s.reexec_command());
    }
    let exe = std::env::current_exe()?;
    Ok(Command::new(exe))
}

/// All sandbox startup work, returning argv stripped of
/// `--sandbox-projection`.
///
/// Pins this binary and, on Unix, enters the OS process sandbox when a
/// projection was supplied; Windows instead confines the child from the
/// parent at spawn time.  A returned code means we are the bwrap respawn
/// parent and should exit with it.
///
/// # Errors
/// A malformed `--sandbox-projection`, or a failure to enter the sandbox.
pub fn early_init(argv: &[String]) -> Result<(Vec<String>, Option<u8>), String> {
    let (policy, stripped) = strip_policy_arg(argv)?;
    // So a per-command `--sandbox-projection` child re-execs this binary and
    // not whatever the on-disk path holds by then.
    reexec::register_sandbox_self();
    // Reclaim what a crashed prior session left registered — its AppContainer
    // profiles, and any per-session grant ACEs a pre-capability ledger still
    // records.  Only a primary
    // session sweeps: a confined re-exec child could not reach the ledger from
    // inside its AppContainer anyway.
    #[cfg(windows)]
    {
        use crate::runtime::pipeline::helper::{BUNDLED_TOOL_FLAG, HELPER_FLAG};
        let is_reexec_child = argv
            .iter()
            .any(|a| a == BUNDLED_TOOL_FLAG || a == HELPER_FLAG);
        if !is_reexec_child {
            windows::session::boot_recover();
        }
    }
    if let Some(code) = reexec::maybe_enter_process_sandbox(&stripped, policy.as_ref())? {
        return Ok((stripped, Some(code)));
    }
    Ok((stripped, None))
}

/// Delete this session's `AppContainer` profiles.  Grant ACEs stay: they are
/// capability-keyed and persistent, inert for any token no ral spawn carries.
///
/// Windows only, and idempotent, so the portable shutdown seams that call it
/// (`ral`'s `Drop for Session`, `exarch`'s `main`) cost nothing elsewhere.  A
/// session that never reaches the seam leaves its DACL ledger for the next
/// start's boot sweep in [`early_init`] to reclaim.
pub fn teardown_session() {
    #[cfg(windows)]
    windows::session::teardown();
}

/// The whole pre-`main` sandbox stage as one `Option<u8>`, for exarch and
/// the test ctors.
///
/// First [`early_init`], then the per-command re-exec tails on the argv it
/// leaves — the `--ral-bundled-tool` multicall via
/// [`crate::try_run_bundled_tool`], and on macOS the host `execve` via
/// [`serve_sandbox_exec`].  That order is the point: a `--sandbox-projection`
/// child is confined before its tail runs.  The stripped argv is discarded, so
/// the `ral` binary, which parses a CLI out of it, calls [`early_init`] itself.
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

/// Split `--sandbox-projection <json>` out of `raw`: the parsed policy, then
/// the arguments that remain.
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

/// Install `pre_exec` hooks: no core dumps anywhere, plus a 512-process
/// `RLIMIT_NPROC` cap off macOS as fork-bomb mitigation.  Darwin counts
/// `RLIMIT_NPROC` against the whole real UID rather than the sandboxed subtree,
/// so lowering it there starves a busy desktop session of spawn slots
/// (`EAGAIN`); Seatbelt still gates `process-fork`, so macOS waits for a
/// subtree-scoped mechanism.
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
                libc::setrlimit(libc::RLIMIT_NPROC, &raw const nproc);
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
    reason = "[io-door:surface:make-command] Builds the external exec image (ExecImage::Host) the model launches. `finish_command` builds the exec observation for this image, wrapping the whole dispatch, with the resolved argv and exit status when the spawn/wait completes."
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
        // `net: true` asks for no restriction, so there is nothing to enforce.
        let p_net_true = SandboxProjection {
            fs: crate::types::FsProjection::default(),
            net: true,
            exec: crate::types::ExecProjection::default(),
        };
        assert!(projection_enforceable(&p_net_true).is_ok());
    }

    #[test]
    fn projection_enforceable_net_false_tracks_net_enforced() {
        // Stated relationally, so the assertion holds on any host.
        let p_net_false = SandboxProjection {
            fs: crate::types::FsProjection::default(),
            net: false,
            exec: crate::types::ExecProjection::default(),
        };
        assert_eq!(projection_enforceable(&p_net_false).is_ok(), net_enforced());
    }

    #[cfg(windows)]
    #[test]
    fn projection_enforceable_allows_net_false_on_windows() {
        // An AppContainer with no network capability SID cannot open a socket.
        let p_net_false = SandboxProjection {
            fs: crate::types::FsProjection::default(),
            net: false,
            exec: crate::types::ExecProjection::default(),
        };
        assert!(
            projection_enforceable(&p_net_false).is_ok(),
            "net: false must be enforceable on Windows via the AppContainer backend"
        );
    }

    #[test]
    fn strip_policy_arg_extracts_json_and_preserves_other_args() {
        let (policy, args) = strip_policy_arg(&[
            "--sandbox-projection".into(),
            r#"{"fs":{"kind":"restricted","rules":{"read_prefixes":["/tmp"]}},"net":true}"#.into(),
            "-c".into(),
            "echo hi".into(),
        ])
        .expect("policy args");
        assert_eq!(
            policy,
            Some(SandboxProjection {
                fs: crate::types::FsProjection::Restricted(crate::types::FsRules {
                    read_prefixes: vec!["/tmp".to_string()],
                    ..crate::types::FsRules::default()
                }),
                net: true,
                exec: crate::types::ExecProjection::default(),
            })
        );
        assert_eq!(args, vec!["-c", "echo hi"]);
    }
}
