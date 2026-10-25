//! OS-level confinement for the children a `grant` block spawns.
//!
//! Platform backends (`linux`, `macos`, `windows`), the per-command
//! launcher (`launch`) and its handoff to a confined re-exec (`warrant`),
//! binary pinning and re-exec (`reexec`), kernel-denial diagnostics (`diag`),
//! and the carving of exclusions out of allowances that the Windows backend,
//! which cannot subtract, grants instead (`carve`).
//!
//! Exec is checked everywhere by `authority::guard::check_exec`, the in-process guard.
//! Both Unix backends also render the allow-list into the kernel, catching the
//! re-execs that check never sees (`sh -c`, `find -exec`): macOS a Seatbelt
//! `process-exec` clause, Linux a Landlock `Execute` ruleset the payload
//! enters inside the bwrap envelope (`linux::landlock`).  Landlock being
//! allow-list only, a deny *inside* an allow is rendered by subtraction, over
//! the tree as it stands at launch; Seatbelt carries it into the kernel as is.

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("ral's sandbox has backends for Linux, macOS and Windows only");

use crate::path::ObjectId;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod carriers;
#[cfg(any(windows, all(test, target_os = "linux")))]
mod carve;
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
#[cfg(target_os = "macos")]
pub(crate) use projection::RenderedRules;
pub use projection::{FsProjection, FsRules, SandboxProjection};
#[cfg(target_os = "linux")]
pub(crate) use projection::{RenderedFs, RenderedProjection};
pub use reexec::pinned_arg0;

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
            "offline mode (`` net: `offline` ``) is unsupported on this platform: \
             no kernel network enforcement exists"
                .into(),
        ));
    }
    Ok(())
}

/// Boot-pinned objects the baseline protects against writes under every grant.
pub(super) fn protected_images() -> Vec<(ObjectId, &'static str)> {
    let images: [Option<(ObjectId, &'static str)>; _] = [
        #[cfg(target_os = "linux")]
        linux::envelope()
            .ok()
            .map(|e| (e.object_id(), linux::BWRAP)),
        #[cfg(unix)]
        reexec::OWN.get().map(|o| (o.object_id(), "ral")),
    ];
    images.into_iter().flatten().collect()
}

/// The sandbox side's proof that it is the sandbox side: what
/// `RealPath::open_nofollow` and `open_dir_nofollow` ask for before they hold
/// a position as `Installing`.  The field is private to this module and its
/// children, and the one `static` below is its one construction site: a
/// renderer lends it, and nothing outside `sandbox` can name it.  No macOS
/// renderer holds a position.
#[cfg(any(target_os = "linux", windows))]
pub(crate) struct InstallToken(());

#[cfg(any(target_os = "linux", windows))]
static TOKEN: InstallToken = InstallToken(());

/// Whether this host builds a `Restricted` envelope at all.
///
/// A rootless container refuses the read-only remount of its own locked
/// `/etc/hosts` and `/etc/resolv.conf`, and the launch dies in setup.
/// Test-only: a spawning test on such a host proves nothing either way, so
/// callers skip rather than assert.
#[cfg(all(target_os = "linux", any(test, feature = "test-util")))]
pub fn restricted_envelope_launches() -> bool {
    use crate::site::{Silent, site};
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
        let Ok(program) = crate::authority::policy::Program::file("/bin/true".into()) else {
            return false;
        };
        launch::enveloped(
            &linux::Envelope::probe(bwrap, own),
            &projection,
            &crate::authority::policy::Baseline::default(),
            &launch::admitted(program, &[]),
            None,
            // `Surrendered` carries no `--info-fd` to read and no parent-death tie:
            // neither decides whether the envelope builds, and `/bin/true` outlives nobody.
            launch::Ownership::Surrendered,
        )
        .is_ok_and(|(mut cmd, _no_info_fd)| {
            site!(
                Silent::RestrictedEnvelopeProbe,
                cmd.stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null()) => status()
            )
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
#[allow(clippy::disallowed_methods, reason = "test code")]
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

    /// `RealPath::open_nofollow` and `open_dir_nofollow`, which only the sandbox
    /// side may call: it holds the token, so these tests lend it.
    #[cfg(any(target_os = "linux", windows))]
    mod nofollow {
        use crate::authority::sandbox::TOKEN;
        use crate::path::{Dir, Installing, Located, ObjectId, RealPath};
        use crate::test_helper::{place, real};
        use std::fs::File;
        use std::io;

        #[cfg(target_os = "linux")]
        const ELOOP: &str = "too many levels of symbolic links";

        /// `name` in `parent`, present or not.
        fn at(parent: &RealPath, name: &str) -> RealPath {
            place(parent.as_path().join(name))
        }

        fn hold(position: &RealPath) -> io::Result<Option<Located<Installing>>> {
            position.open_nofollow(&TOKEN)
        }

        fn hold_dir(position: &RealPath) -> io::Result<Option<Dir<Installing>>> {
            position.open_dir_nofollow(&TOKEN)
        }

        /// A directory with a file `f` and a directory `d` in it.
        fn fixture() -> (tempfile::TempDir, RealPath) {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join("f"), b"held").unwrap();
            std::fs::create_dir(tmp.path().join("d")).unwrap();
            let root = real(tmp.path());
            (tmp, root)
        }

        #[test]
        fn an_absent_position_is_none() {
            let (_tmp, root) = fixture();
            for position in [at(&root, "absent"), at(&at(&root, "absent"), "f")] {
                assert!(hold(&position).unwrap().is_none());
                assert!(hold_dir(&position).unwrap().is_none());
            }
        }

        #[test]
        fn a_file_asked_as_a_directory_is_none() {
            let (_tmp, root) = fixture();
            assert!(hold_dir(&at(&root, "f")).unwrap().is_none());
            assert!(hold_dir(&at(&at(&root, "f"), "g")).unwrap().is_none());
        }

        /// The file is held as an `Installing` object of kind `File` that lends
        /// its descriptor; that it reads nothing is `path::walk::tests::markers`'.
        #[cfg(target_os = "linux")]
        #[test]
        fn a_file_is_held_as_an_installing_object_lending_its_leaf_fd() {
            use crate::path::Kind;
            let (_tmp, root) = fixture();
            let located = hold(&at(&root, "f")).unwrap().expect("a file");
            assert_eq!(located.trace().kind(), Some(Kind::File));
            let fd = located.leaf_fd().expect("a held leaf lends its descriptor");
            let lent = File::from(rustix::io::fcntl_dupfd_cloexec(fd, 0).unwrap());
            assert_eq!(
                ObjectId::of(&lent).unwrap(),
                located.trace().last_id(),
                "the descriptor is of the object the walk found"
            );
        }

        /// A directory removed and made again at its name is the new object.
        #[test]
        fn a_recreated_directory_is_its_new_object() {
            let (tmp, root) = fixture();
            let position = at(&root, "d");
            let first = hold_dir(&position).unwrap().expect("a directory");
            let first_id = first.trace().last_id();
            drop(first);
            std::fs::rename(tmp.path().join("d"), tmp.path().join("old")).unwrap();
            std::fs::create_dir(tmp.path().join("d")).unwrap();
            let second = hold_dir(&position).unwrap().expect("the new directory");
            assert_ne!(first_id, second.trace().last_id());
            let made = File::open(tmp.path().join("d")).unwrap();
            assert_eq!(second.trace().last_id(), ObjectId::of(&made).unwrap());
        }

        /// A directory held for installing is held without delete sharing: it
        /// cannot be renamed while the hold lives.
        #[cfg(windows)]
        #[test]
        fn an_installing_directory_is_held_without_delete_sharing() {
            let (tmp, root) = fixture();
            let held = hold_dir(&at(&root, "d")).unwrap().expect("a directory");
            assert!(std::fs::rename(tmp.path().join("d"), tmp.path().join("e")).is_err());
            drop(held);
            std::fs::rename(tmp.path().join("d"), tmp.path().join("e")).unwrap();
        }

        /// A position whose last component became a link, or with a link on
        /// the way, is refused as `ELOOP` is, not followed and not absent.
        #[cfg(target_os = "linux")]
        #[test]
        fn a_link_in_a_position_refuses_with_eloops_wording() {
            let (tmp, root) = fixture();
            let positions = [
                at(&root, "link"),
                at(&at(&root, "via"), "g"),
                at(&root, "via"),
            ];
            std::os::unix::fs::symlink(tmp.path().join("f"), tmp.path().join("link")).unwrap();
            std::os::unix::fs::symlink(tmp.path().join("d"), tmp.path().join("via")).unwrap();
            std::fs::write(tmp.path().join("d").join("g"), b"x").unwrap();
            for position in positions {
                let held = hold(&position).err().expect("a link refuses");
                assert_eq!(held.to_string(), ELOOP);
                let held = hold_dir(&position).err().expect("a link refuses");
                assert_eq!(held.to_string(), ELOOP);
            }
        }
    }
}
