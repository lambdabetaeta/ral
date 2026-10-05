//! OS-level confinement for the children a `grant` block spawns.
//!
//! Platform backends (`linux`, `macos`, `windows`), the per-command
//! launcher (`launch`) and its handoff to a confined re-exec (`warrant`),
//! binary pinning and re-exec (`reexec`), kernel-denial diagnostics (`diag`).
//!
//! Exec is checked everywhere by `capability::check_exec`, the in-process guard.
//! Both Unix backends also render the allow-list into the kernel, catching the
//! re-execs that check never sees (`sh -c`, `find -exec`): macOS a Seatbelt
//! `process-exec` clause, Linux a Landlock `Execute` ruleset the payload
//! enters inside the bwrap envelope (`linux::landlock`).  Landlock being
//! allow-list only, a deny *inside* an allow stays with the in-process guard
//! there; Seatbelt carries it into the kernel.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod carriers;
mod diag;
mod launch;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod reexec;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod warrant;
#[cfg(windows)]
mod windows;

use crate::Invocation;
use crate::types::{SandboxProjection, Shell};
#[cfg(unix)]
use std::process::Command;
use std::sync::OnceLock;

/// Host-supplied constructor a re-exec'd child calls to rebuild its
/// [`HostSurface`](crate::boot::HostSurface): a wire shell cannot carry one across
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

/// Test-only: `subprocess::bare_child_shell`'s only caller.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn run_child_shell_extension(shell: &mut Shell) {
    if let Some(surface) = CHILD_SHELL_HOOK.get() {
        surface().install_into_shell(shell);
    }
}

// `runtime::command::process::build_command` routes an external/bundled child
// through `sandboxed_command` when a projection is active and no guest jail
// already confines it; `serve_warrant` is the trampoline's side, entering the
// confinement before it becomes the program.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use carriers::carriers;
pub use launch::serve_warrant;
pub(crate) use launch::{Ownership, sandboxed_command};

// Called by the command runners on a failure that ran under an active OS
// sandbox, to attach a hint naming the denied path.
pub(crate) use diag::{augment_failure, sample_descendants};

/// Whether this platform can enforce `net: false`: Linux via `--unshare-net`,
/// macOS via deny-default Seatbelt, Windows via an `AppContainer` with no
/// network capability SID, which cannot open a socket.
const NET_ENFORCED: bool = cfg!(any(target_os = "linux", target_os = "macos", windows));

// Whether this platform's backend carries the exec allow-list into the
// kernel, per the rendering named in this module's header: Seatbelt's
// `process-exec` clause, Landlock's `Execute` ruleset.  Windows has no
// counterpart, so an exec opinion there is the in-process guard's alone.
//
// `capability::sandbox::sandbox_projection` reads this to decide whether an
// exec-only grant is worth an OS sandbox at all.  Each backend declares its
// own `RENDERS_EXEC` beside the code that renders it, so a backend gaining
// exec rendering switches the trigger on there, rather than in a second list
// here that can be forgotten.  Landlock absent from a running kernel is
// *not* an exception: the envelope is still built and
// `linux::landlock::enter` simply has nothing to enter.
#[cfg(target_os = "linux")]
pub(crate) use linux::landlock::RENDERS_EXEC as EXEC_ENFORCED;
#[cfg(target_os = "macos")]
pub(crate) use macos::RENDERS_EXEC as EXEC_ENFORCED;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) const EXEC_ENFORCED: bool = false;

/// The one refusal for "this host cannot establish the confinement the active
/// grant asks for".  Whether it is an axis no backend here enforces, an
/// envelope binary that was not there to pin at boot, or a spawn the envelope
/// itself failed, the user is owed the same answer: nothing ran, and the
/// sandbox is why.  Never the command's fault, so never phrased as such.
pub(crate) fn confinement_unavailable(reason: &str) -> crate::types::Error {
    crate::types::Error::new(format!("sandbox confinement unavailable: {reason}"), 1)
}

/// `Err(reason)` when `projection` asks for a restriction this platform cannot
/// enforce.  Offline is the only such axis: `net: false` must refuse rather
/// than run somewhere the bit is silently ignored.
pub(crate) fn projection_enforceable(projection: &SandboxProjection) -> Result<(), &'static str> {
    if !projection.net && !NET_ENFORCED {
        return Err(
            "offline mode (net: false) is unsupported on this platform: \
                    no kernel network enforcement exists",
        );
    }
    Ok(())
}

/// The boot-pinned binary `path` names — `"bwrap"` for the Linux envelope,
/// `"ral"` for our own executable — or `None`.  Judged by inode, so a hard
/// link or a rename since boot still answers; a name with nothing on disk
/// under it names no pin.  `capability::fs_verdict` asks this of every write
/// ral itself dispatches: the enforcer's inode is not the body's to rewrite,
/// whatever the grant admits ([`launch`] binds it read-only for the confined
/// child, and this is the same fact for the in-process half).
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:pin-identity] stats a write's resolved target to compare its inode against the boot pins; a predicate stat for the fs guard's own verdict, not the model's data I/O"
)]
pub(crate) fn pinned_binary(path: &std::path::Path) -> Option<&'static str> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(target_os = "linux")]
    if linux::envelope().is_ok_and(|envelope| envelope.is_inode(&meta)) {
        return Some(linux::BWRAP);
    }
    reexec::SANDBOX_SELF
        .get()
        .is_some_and(|own| own.is_inode(&meta))
        .then_some("ral")
}

/// The confined re-exec's whole argv, its warrant arriving on a descriptor.
pub(crate) const WARRANT_FLAG: &str = "--warrant";

/// Debug switch: set to any value to make [`dump_profile_if_requested`] print
/// the OS-sandbox profile that would be installed.
pub(crate) const SANDBOX_DUMP_PROFILE_ENV: &str = "RAL_DUMP_SANDBOX_PROFILE";

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
        let envelope = match linux::envelope() {
            Ok(envelope) => envelope,
            Err(why) => {
                eprintln!("--- bwrap argv error ---\n{why}");
                return;
            }
        };
        let host = linux::HostEnvelope::probe(envelope);
        let payload = linux::Payload {
            program: "/bin/true",
            args: &[],
            image: None,
            handoff: &[],
        };
        match linux::bwrap_argv(
            envelope,
            payload,
            policy,
            None,
            launch::Ownership::Kept,
            host,
        ) {
            Ok(options) => {
                let options: Vec<_> = options.iter().map(|arg| arg.to_string_lossy()).collect();
                eprint!(
                    "--- bwrap argv ---\n{} --args {} -- {}\n--- bwrap --args ---\n{}\n\
                     --- host envelope ---\n{host}",
                    envelope.arg0().display(),
                    warrant::ARGS_FD,
                    payload.program,
                    options.join(" "),
                );
                if envelope.writable_by_us() {
                    eprintln!(
                        "envelope writable by this uid: {} — any process running as you can \
                         rewrite it between launches; ral refuses its own writes and cannot \
                         see another's.  Lifted by a root-owned bwrap (the distro package).",
                        envelope.arg0().display()
                    );
                }
                eprintln!("--- end bwrap argv ---");
            }
            Err(e) => eprintln!("--- bwrap argv error ---\n{e}"),
        }
        linux::landlock::dump(policy, host.landlock);
        linux::seccomp::dump();
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
pub(crate) fn apply_child_limits(child: &crate::process::ChildHandle) {
    #[cfg(windows)]
    windows::apply_job_limits(child);
}

/// Whether this host builds a `Restricted` envelope at all.
///
/// A rootless container refuses the read-only remount of its own locked
/// `/etc/hosts` and `/etc/resolv.conf`, and the launch dies in setup.
/// Test-only: a spawning test on such a host proves nothing either way, so
/// callers skip rather than assert.
// `Surrendered` carries no `--info-fd` to read and no parent-death tie:
// neither decides whether the envelope builds, and `/bin/true` outlives nobody.
#[cfg(all(target_os = "linux", any(test, feature = "test-util")))]
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:restricted-envelope-probe] host capability probe for tests, not a grant spawn"
)]
pub fn restricted_envelope_launches() -> bool {
    use crate::types::{FsProjection, FsRules};
    static LAUNCHES: OnceLock<bool> = OnceLock::new();
    *LAUNCHES.get_or_init(|| {
        let projection = SandboxProjection {
            fs: FsProjection::Restricted(FsRules::default()),
            net: true,
            exec: crate::types::ExecProjection::default(),
        };
        linux::register_envelope();
        let Ok(envelope) = linux::envelope() else {
            return false;
        };
        linux::make_command_with_policy(
            envelope,
            linux::Payload {
                program: "/bin/true",
                args: &[],
                image: None,
                handoff: &[],
            },
            &projection,
            None,
            launch::Ownership::Surrendered,
            linux::HostEnvelope::probe(envelope),
        )
        .is_ok_and(|(mut cmd, _no_info_fd)| {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
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
pub(crate) fn apply_child_limits_in_pipeline(
    child: &crate::process::ChildHandle,
    leader: crate::process::Pgid,
) {
    #[cfg(windows)]
    {
        if crate::process::is_known_group(leader.as_raw()) {
            if !crate::process::apply_group_active_process_limit(
                leader.as_raw(),
                ACTIVE_PROCESS_CAP,
            ) {
                eprintln!("ral: warning: failed to apply active-process limit to pipeline job");
            }
        } else {
            windows::apply_job_limits(child);
        }
    }
}

/// Pin this executable for a role served before [`early_init`] — the pipeline
/// anchor and the engine — whose sandboxed launches need ral pinned.
#[cfg(unix)]
pub fn register_self_for_helpers() {
    reexec::register_sandbox_self();
}

/// Re-exec the current ral binary, preferring the pinned self-path so helpers
/// stay bound to the boot-time build even if the on-disk path is swapped.
#[cfg(unix)]
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:self-reexec] Builds the ral-re-exec Command for sandbox helper subprocesses (pipeline anchor, bundled-tool multicall). Infrastructure spawn, not a model exec image — the model's exec surfaces at command::run, not here."
)]
pub(crate) fn self_command() -> std::io::Result<Command> {
    if let Some(s) = reexec::SANDBOX_SELF.get() {
        return Ok(s.command());
    }
    let exe = std::env::current_exe()?;
    Ok(Command::new(exe))
}

/// All sandbox startup work, before any shell exists: pin this binary and,
/// on Linux, the bwrap envelope, so no session can choose its own launcher.
#[cfg_attr(
    not(windows),
    allow(
        unused_variables,
        reason = "only Windows asks whether this is a re-exec child"
    )
)]
pub fn early_init(role: &Invocation<'_>) {
    reexec::register_sandbox_self();
    #[cfg(target_os = "linux")]
    linux::register_envelope();
    // Reclaim what a crashed prior session left registered — its AppContainer
    // profiles, and any per-session grant ACEs a pre-capability ledger still
    // records.  Only a primary
    // session sweeps: a confined re-exec child could not reach the ledger from
    // inside its AppContainer anyway.
    #[cfg(windows)]
    if !matches!(
        role,
        Invocation::BundledTool(_) | Invocation::PipelineAnchor
    ) {
        windows::session::boot_recover();
    }
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

/// The whole pre-`main` sandbox stage as one `Option<u8>`, for `ral`, exarch
/// and the test ctors, over the role [`classify`](crate::classify) named.
///
/// A confined re-exec child is served first, and by [`serve_warrant`] alone:
/// [`early_init`] opens pins its confinement would hand the target.  Any other
/// role runs [`early_init`], and the `--ral-bundled-tool` multicall is then
/// served.
pub fn serve_sandbox_early_init(role: &Invocation<'_>) -> Option<u8> {
    if let Invocation::Warrant(extra) = role {
        return Some(serve_warrant(extra));
    }
    early_init(role);
    match role {
        Invocation::BundledTool(args) => {
            Some(crate::runtime::pipeline::helper::serve_bundled_tool(args))
        }
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use super::{NET_ENFORCED, projection_enforceable};
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
        assert_eq!(projection_enforceable(&p_net_false).is_ok(), NET_ENFORCED);
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
}
