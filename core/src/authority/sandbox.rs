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

#[cfg(unix)]
mod carriers;
#[cfg(any(windows, all(test, target_os = "linux")))]
mod carve;
mod diag;
mod launch;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(all(target_os = "linux", any(test, feature = "test-util")))]
pub(crate) use linux::landlock::{BASELINE_ABI, probe_abi};
#[cfg(all(test, seccomp))]
pub(crate) use linux::seccomp::in_child;
#[cfg(seccomp)]
pub(crate) use linux::seccomp::{Filter, Programs};
#[cfg(target_os = "macos")]
mod macos;
mod projection;
pub(crate) mod reexec;
#[cfg(unix)]
mod warrant;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(any(unix, test))]
pub(crate) use projection::ExecProjection;
#[cfg(target_os = "macos")]
pub(crate) use projection::RenderedLists;
pub use projection::{FsLists, FsProjection, SandboxProjection};
#[cfg(target_os = "linux")]
pub(crate) use projection::{RenderedFs, RenderedProjection};
pub use reexec::pinned_arg0;

// Command preparation selects one confinement mechanism. The trampoline
// enters the host sandbox through `serve_warrant` before running its payload.
#[cfg(unix)]
pub(crate) use carriers::carriers;
pub(crate) use launch::{Confinement, Image, Ownership, ProgramLaunch, interpreter_of};
#[cfg(target_os = "macos")]
pub use macos::ALREADY_PROFILED;
#[cfg(unix)]
pub(crate) use warrant::serve_warrant;

// Read by `shell::exec::command` on a failure that ran under an active OS
// sandbox, for a hint naming the denied path or the refused interpreter.
pub(crate) use diag::denial_hint;
#[cfg(unix)]
pub(crate) use diag::interpreter_hint;
// Read by `start` as an enveloped program begins, for the pids its deny
// lines may name.
#[cfg(target_os = "linux")]
pub(crate) use linux::payload::pids as payload_pids;

/// Whether this platform can withhold the GPU: macOS by absent `IOKit` and
/// Metal rules, Linux by absent device nodes.  An `AppContainer` reaches
/// Direct3D with no capability to withhold, so on Windows a withheld GPU
/// projects as left on and every summary says so.
pub(crate) const WITHHOLDS_GPU: bool = cfg!(unix);

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
#[cfg(windows)]
pub(crate) const EXEC_ENFORCED: bool = false;

#[cfg(any(target_os = "linux", windows))]
pub(crate) use crate::process::launch::unavailable;

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
    /// `RealPath::open_nofollow` and `open_dir_nofollow`, which only the sandbox
    /// side may call: it holds the token, so these tests lend it.
    #[cfg(any(target_os = "linux", windows))]
    mod nofollow {
        use crate::authority::sandbox::TOKEN;
        use crate::path::{Dir, Installing, Located, ObjectId, RealPath};
        use crate::test_helper::{place, real};
        #[cfg(target_os = "linux")]
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
        /// its descriptor; that it reads nothing is `path::handle::held::tests::markers`'.
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
            #[cfg(target_os = "linux")]
            let made = File::open(tmp.path().join("d")).unwrap();
            #[cfg(windows)]
            let made = {
                use std::os::windows::fs::OpenOptionsExt;
                use windows_sys::Win32::Storage::FileSystem::{
                    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
                };

                // `File::open` cannot open a Windows directory. Ask only for
                // attributes, leaving the installing hold's no-delete sharing intact.
                std::fs::OpenOptions::new()
                    .access_mode(FILE_READ_ATTRIBUTES)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                    .open(tmp.path().join("d"))
                    .unwrap()
            };
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
