//! OS-level confinement for the children a `grant` block spawns.
//!
//! Platform backends (`linux`, `macos`, `windows`), the per-command
//! launcher (`launch`) and its handoff to a confined re-exec (`warrant`),
//! binary pinning and re-exec (`reexec`), kernel-denial diagnostics (`diag`).
//!
//! Exec is checked everywhere by `guard::check_exec`, the in-process guard.
//! Both Unix backends also render the allow-list into the kernel, catching the
//! re-execs that check never sees (`sh -c`, `find -exec`): macOS a Seatbelt
//! `process-exec` clause, Linux a Landlock `Execute` ruleset the payload
//! enters inside the bwrap envelope (`linux::landlock`).  Landlock being
//! allow-list only, a deny *inside* an allow is rendered by subtraction, over
//! the tree as it stands at launch; Seatbelt carries it into the kernel as is.

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("ral's sandbox has backends for Linux, macOS and Windows only");

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod carriers;
mod diag;
mod launch;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod projection;
pub(crate) mod reexec;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod warrant;
#[cfg(windows)]
mod windows;

#[cfg(any(unix, test))]
pub(crate) use projection::ExecProjection;
#[cfg(unix)]
pub(crate) use projection::WriteReach;
pub use projection::{FsProjection, FsRules, SandboxProjection};

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

// Read by `runtime::command::with_denials` on a failure that ran under an
// active OS sandbox, for a hint naming the denied path.
pub(crate) use diag::denial_hint;

/// Whether this platform can enforce `net: false`: Linux via `--unshare-net`,
/// macOS via deny-default Seatbelt, Windows via an `AppContainer` with no
/// network capability SID, which cannot open a socket.
const NET_ENFORCED: bool = cfg!(any(target_os = "linux", target_os = "macos", windows));

// Whether this platform's backend carries the exec allow-list into the
// kernel, per the rendering named in this module's header: Seatbelt's
// `process-exec` clause, Landlock's `Execute` ruleset.  Windows has no
// counterpart, so an exec opinion there is the in-process guard's alone.
//
// `SandboxProjection::of` reads this to decide whether an
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

/// Why no confined child exists: nothing ran, and the sandbox is why.  Never
/// the command's fault, so never phrased as such.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// This host cannot establish the confinement the active grant asks for:
    /// an axis no backend here enforces, an envelope binary that was not there
    /// to pin at boot, or a spawn the envelope itself failed.
    Unavailable(String),
    /// The launch could not be built.
    Launch(String),
    /// A cancel arrived mid-launch: not a sandbox failure.
    #[cfg(windows)]
    Cancelled(crate::process::CancelCause),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why) => write!(f, "sandbox confinement unavailable: {why}"),
            Self::Launch(why) => f.write_str(why),
            #[cfg(windows)]
            Self::Cancelled(cause) => f.write_str(cause.message()),
        }
    }
}

impl From<String> for Refusal {
    fn from(why: String) -> Self {
        Self::Launch(why)
    }
}

/// `Err` when `projection` asks for a restriction this platform cannot
/// enforce.  Offline is the only such axis: `net: false` must refuse rather
/// than run somewhere the bit is silently ignored.
pub(crate) fn projection_enforceable(projection: &SandboxProjection) -> Result<(), Refusal> {
    if !projection.net && !NET_ENFORCED {
        return Err(Refusal::Unavailable(
            "offline mode (net: false) is unsupported on this platform: \
             no kernel network enforcement exists"
                .into(),
        ));
    }
    Ok(())
}

/// The boot-pinned binary `path` names — `"bwrap"` for the Linux envelope,
/// `"ral"` for our own executable: or `None`.  Judged by inode, so a hard
/// link or a rename since boot still answers; a name with nothing on disk
/// under it names no pin.  `guard::enforce::fs_verdict` asks this of every write
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
    use std::sync::OnceLock;
    static LAUNCHES: OnceLock<bool> = OnceLock::new();
    *LAUNCHES.get_or_init(|| {
        let projection = SandboxProjection {
            fs: FsProjection::Restricted(FsRules::default()),
            net: true,
            exec: ExecProjection::default(),
        };
        pin();
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

/// Pin this binary and, on Linux, the bwrap envelope, so no session can
/// choose its own launcher.  Idempotent.
pub(crate) fn pin() {
    reexec::pin_self();
    #[cfg(target_os = "linux")]
    linux::pin_envelope();
}

/// All sandbox startup work for a primary session, before any shell exists:
/// [`pin`], then reclaim what a crashed prior session left registered (its
/// `AppContainer` profiles, and any per-session grant ACEs a pre-capability
/// ledger still records).  A confined re-exec child only [`pin`]s: it could not
/// reach the ledger from inside its `AppContainer` anyway.
pub(crate) fn boot() {
    pin();
    #[cfg(windows)]
    windows::session::boot_recover();
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

#[cfg(test)]
mod tests {
    use super::{
        ExecProjection, FsProjection, NET_ENFORCED, SandboxProjection, projection_enforceable,
    };

    #[test]
    fn projection_enforceable_allows_net_true_on_any_platform() {
        // `net: true` asks for no restriction, so there is nothing to enforce.
        let p_net_true = SandboxProjection {
            fs: FsProjection::default(),
            net: true,
            exec: ExecProjection::default(),
        };
        assert!(projection_enforceable(&p_net_true).is_ok());
    }

    #[test]
    fn projection_enforceable_net_false_tracks_net_enforced() {
        // Stated relationally, so the assertion holds on any host.
        let p_net_false = SandboxProjection {
            fs: FsProjection::default(),
            net: false,
            exec: ExecProjection::default(),
        };
        assert_eq!(projection_enforceable(&p_net_false).is_ok(), NET_ENFORCED);
    }

    #[cfg(windows)]
    #[test]
    fn projection_enforceable_allows_net_false_on_windows() {
        // An AppContainer with no network capability SID cannot open a socket.
        let p_net_false = SandboxProjection {
            fs: FsProjection::default(),
            net: false,
            exec: ExecProjection::default(),
        };
        assert!(
            projection_enforceable(&p_net_false).is_ok(),
            "net: false must be enforceable on Windows via the AppContainer backend"
        );
    }
}
