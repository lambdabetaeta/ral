//! OS-level confinement for the children a `grant` block spawns.
//!
//! Platform backends (`linux`, `macos`, `windows`), the per-command
//! launcher (`launch`) and its handoff to a confined re-exec (`warrant`),
//! binary pinning and re-exec (`reexec`), kernel-denial diagnostics (`diag`).
//! Every ral-family process starts in [`serve_pre_main`], which lives here
//! because what it decides is when each role is pinned.
//!
//! Exec is checked everywhere by `capability::check_exec`, the in-process guard.
//! Both Unix backends also render the allow-list into the kernel, catching the
//! re-execs that check never sees (`sh -c`, `find -exec`): macOS a Seatbelt
//! `process-exec` clause, Linux a Landlock `Execute` ruleset the payload
//! enters inside the bwrap envelope (`linux::landlock`).  Landlock being
//! allow-list only, a deny *inside* an allow is rendered by subtraction, over
//! the tree as it stands at launch; Seatbelt carries it into the kernel as is.

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod carriers;
mod diag;
#[cfg(target_os = "macos")]
mod fork_brake;
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
use crate::types::SandboxProjection;
#[cfg(unix)]
use std::process::Command;

// `runtime::command::process::build_launch` routes an external/bundled child
// through `sandboxed_command` when a projection is active and no guest jail
// already confines it; `serve_warrant` is the trampoline's side, entering the
// confinement before it becomes the program.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use carriers::carriers;
pub use launch::serve_warrant;
pub(crate) use launch::{Ownership, sandboxed_command};
#[cfg(target_os = "macos")]
pub use macos::ALREADY_PROFILED;

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
// here that can be forgotten.  Landlock absent from a running kernel leaves
// the trigger on: `linux::landlock::build` refuses a restricting exec
// grant there, and only an unrestricted exec projection launches with nothing
// to enter.
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
    reexec::OWN
        .get()
        .is_some_and(|own| own.is_inode(&meta))
        .then_some("ral")
}

/// The confined re-exec's whole argv, its warrant arriving on a descriptor.
pub(crate) const WARRANT_FLAG: &str = "--warrant";

/// How many live processes a grant-confined child may add: the fork-bomb cap.
/// A Job Object limit on Windows; on macOS, headroom over the user's count at launch.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) const ACTIVE_PROCESS_CAP: u32 = 512;

/// Assign OS-level resource limits to an already-spawned child.
///
/// Windows only: a Job Object caps the tree at `ACTIVE_PROCESS_CAP` after the
/// spawn.  On Unix `limit_resources` ran before exec, and this is a no-op.
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
    use std::sync::OnceLock;
    static LAUNCHES: OnceLock<bool> = OnceLock::new();
    *LAUNCHES.get_or_init(|| {
        let projection = SandboxProjection {
            fs: FsProjection::Restricted(FsRules::default()),
            net: true,
            exec: crate::types::ExecProjection::default(),
        };
        reexec::pin_self();
        linux::pin_envelope();
        let (Ok(bwrap), Ok(own)) = (linux::envelope(), reexec::own()) else {
            return false;
        };
        let Ok(program) = crate::capability::Program::file("/bin/true".into()) else {
            return false;
        };
        launch::enveloped(
            &linux::Envelope::probe(bwrap, own),
            &projection,
            &launch::admitted(program, &[]),
            None,
            launch::Ownership::Surrendered,
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

/// Re-exec the current ral binary, preferring the pinned self-path so helpers
/// stay bound to the boot-time build even if the on-disk path is swapped.
#[cfg(unix)]
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:self-reexec] Builds the ral-re-exec Command for sandbox helper subprocesses (pipeline anchor, bundled-tool multicall). Infrastructure spawn, not a model exec image — the model's exec surfaces at command::run, not here."
)]
pub(crate) fn self_command() -> std::io::Result<Command> {
    if let Some(s) = reexec::OWN.get() {
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
pub(crate) fn boot(role: &Invocation<'_>) {
    reexec::pin_self();
    #[cfg(target_os = "linux")]
    linux::pin_envelope();
    // Reclaim what a crashed prior session left registered — its AppContainer
    // profiles, and any per-session grant ACEs a pre-capability ledger still
    // records.  Only a primary session sweeps: a confined re-exec child could
    // not reach the ledger from inside its AppContainer anyway.
    #[cfg(windows)]
    if !matches!(role, Invocation::BundledTool(_)) {
        windows::session::boot_recover();
    }
}

/// Delete this session's `AppContainer` profiles.  Grant ACEs stay: they are
/// capability-keyed and persistent, inert for any token no ral spawn carries.
///
/// Windows only, and idempotent, so the portable shutdown seams that call it
/// (`ral`'s `Drop for Session`, `exarch`'s `main`) cost nothing elsewhere.  A
/// session that never reaches the seam leaves its DACL ledger for the next
/// start's [`boot`] to sweep.
pub fn teardown_session() {
    #[cfg(windows)]
    windows::session::teardown();
}

/// Serve the role [`classify`](crate::classify) named: the one pre-`main`
/// dispatch, for `ral`, exarch and the test ctors alike.
///
/// `Some(code)` is a served role's exit; `None` leaves the process, pinned, to
/// its caller — the shell, or a test binary's own fixture.
///
/// The order is the sandbox's.  A confined re-exec is served by
/// [`serve_warrant`] alone, pinning and opening nothing before it is confined;
/// the anchor and the pgid probe spawn nothing, so pin nothing.  Every other
/// role boots first, the engine included: its grant-confined launches need
/// the pinned envelope as much as the shell's.
#[cfg_attr(
    not(unix),
    allow(unused_variables, reason = "only a Unix engine reads its installers")
)]
pub fn serve_pre_main(
    role: &Invocation<'_>,
    installers: &'static [crate::engine::EngineInstaller],
) -> Option<u8> {
    use crate::runtime::pipeline::helper::{serve_anchor, serve_bundled_tool};
    match *role {
        Invocation::Warrant(extra) => Some(serve_warrant(extra)),
        Invocation::PipelineAnchor => Some(serve_anchor()),
        #[cfg(unix)]
        Invocation::PgidCheck { tag } => Some(crate::test_helper::serve_pgid_check(tag)),
        #[cfg(unix)]
        Invocation::Engine => {
            boot(role);
            crate::engine::run_engine(installers)
        }
        Invocation::BundledTool(args) => {
            boot(role);
            Some(serve_bundled_tool(args))
        }
        #[cfg(unix)]
        Invocation::DetachBirth { .. } => {
            boot(role);
            None
        }
        Invocation::Shell => {
            boot(role);
            None
        }
    }
}

/// Limit `cmd`'s child resources in one `pre_exec`: no core dumps and, on
/// macOS, a fork brake.  Darwin counts processes per real UID but compares the
/// count with the *forking* process's own `RLIMIT_NPROC`, so lowering it in the
/// child refuses its descendants' forks and no other process of the user's.
/// The budget is measured here, in the parent: the hook itself only calls
/// `setrlimit`.
#[cfg(unix)]
pub(crate) fn limit_resources(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    #[cfg(target_os = "macos")]
    let nproc = fork_brake::limit()
        .inspect_err(|e| {
            crate::diagnostic::shell_warning(&format!(
                "could not count your processes ({e}), so this command has no process budget"
            ));
        })
        .ok();
    unsafe {
        cmd.pre_exec(move || {
            let zero = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &raw const zero);
            #[cfg(target_os = "macos")]
            if let Some(limit) = nproc {
                let cap = libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                };
                libc::setrlimit(libc::RLIMIT_NPROC, &raw const cap);
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
