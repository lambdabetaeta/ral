//! The process sandbox a confined payload enters *inside* the bwrap envelope:
//! kernel exec confinement rendering `ExecProjection::Restricted`, and a
//! signal scope closing the same-uid `kill` hole where the host cannot build
//! a pid namespace.  The parent builds the one ruleset ([`build`]), opening
//! every admit in the host and never through a symlink; the payload inherits
//! it at [`Slot::Ruleset`] and enters it ([`Landlocked::enter`]), never before
//! bwrap: a domain handling any fs right forbids `mount(2)`, bwrap's first act.
//!
//! Declared gap: Landlock is allow-list only and cannot remove part of an
//! allowed directory, so denies and vetoes render nothing here: a deny
//! outside every allow is already absence, and a deny inside an allowed
//! directory holds only at the in-process guard on Linux, where Seatbelt
//! would carry it into the kernel.

use super::super::confinement_unavailable;
use super::super::warrant::Slot;
use crate::path::render_real;
use crate::types::{ExecProjection, ExecRule};
use libc::{c_int, c_uint};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::OnceLock;

/// `LANDLOCK_CREATE_RULESET_VERSION` in the UAPI; not exported by `libc`.
const CREATE_RULESET_VERSION: c_uint = 1;
/// `LANDLOCK_RULE_PATH_BENEATH`.
const RULE_PATH_BENEATH: c_uint = 1;

/// A Landlock ABI level, as `landlock_create_ruleset` reports it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Abi(u32);

impl Abi {
    /// `LANDLOCK_ACCESS_FS_EXECUTE`, Linux 5.13.
    #[cfg(test)]
    pub(crate) const EXEC: Self = Self(1);
    /// `LANDLOCK_ACCESS_FS_REFER`, Linux 5.19.
    pub(crate) const REFER: Self = Self(2);
    /// `LANDLOCK_SCOPE_SIGNAL`, Linux 6.12.
    pub(crate) const SIGNAL_SCOPE: Self = Self(6);
}

impl fmt::Display for Abi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What the version probe answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Landlock {
    /// Not built in (`ENOSYS`) or absent from the boot LSM list (`EOPNOTSUPP`).
    Absent,
    At(Abi),
    /// Any other errno: not a kernel without Landlock, so never treated as one.
    Unprobed(i32),
}

impl Landlock {
    /// The syscall is the only honest source: a version is not a feature list,
    /// so `uname` is never read.
    pub(crate) fn probe() -> Self {
        static PROBED: OnceLock<Landlock> = OnceLock::new();
        *PROBED.get_or_init(|| {
            // SAFETY: the version query takes a null attr and a zero size.
            let level = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<libc::c_void>(),
                    0usize,
                    CREATE_RULESET_VERSION,
                )
            };
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            classify(level, errno)
        })
    }
}

fn classify(level: libc::c_long, errno: i32) -> Landlock {
    match u32::try_from(level) {
        Ok(level) => Landlock::At(Abi(level)),
        Err(_) if errno == libc::ENOSYS || errno == libc::EOPNOTSUPP => Landlock::Absent,
        Err(_) => Landlock::Unprobed(errno),
    }
}

impl fmt::Display for Landlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => write!(
                f,
                "this kernel has no Landlock (not built in, or absent from the boot LSM list)"
            ),
            Self::At(abi) => write!(f, "this kernel's Landlock is ABI {abi}"),
            Self::Unprobed(errno) => write!(
                f,
                "the Landlock version probe failed: {}, which is not a kernel without it",
                io::Error::from_raw_os_error(*errno)
            ),
        }
    }
}

/// Landlock carries the exec allow-list into the kernel via the `Execute`
/// ruleset [`build`] makes below.
pub(crate) const RENDERS_EXEC: bool = true;

/// `struct landlock_ruleset_attr` at ABI 6; older kernels accept its zero
/// tail.
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

/// `struct landlock_path_beneath_attr`, packed in the UAPI.
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: c_int,
}

/// A set of `LANDLOCK_ACCESS_FS_*` rights.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Access(u64);

impl Access {
    const NONE: Self = Self(0);
    pub(crate) const EXECUTE: Self = Self(1 << 0);
    pub(crate) const REFER: Self = Self(1 << 13);

    fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for Access {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// A set of `LANDLOCK_SCOPE_*` scopes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Scoped(u64);

impl Scoped {
    const NONE: Self = Self(0);
    pub(crate) const SIGNAL: Self = Self(1 << 1);
}

/// A ruleset not yet entered: built by the parent, entered by the payload.
pub(crate) struct Ruleset(OwnedFd);

impl Ruleset {
    fn create(handled: Access, scoped: Scoped) -> io::Result<Self> {
        let attr = RulesetAttr {
            handled_access_fs: handled.0,
            handled_access_net: 0,
            scoped: scoped.0,
        };
        // SAFETY: `attr` is a live `landlock_ruleset_attr` of the size passed.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &raw const attr,
                size_of::<RulesetAttr>(),
                0 as c_uint,
            )
        };
        match c_int::try_from(fd) {
            // SAFETY: a fresh descriptor the kernel just returned to us alone.
            Ok(fd) if fd >= 0 => Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) })),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// `access` on `object` and, for a directory, everything beneath it.
    fn admit(&self, object: BorrowedFd<'_>, access: Access) -> io::Result<()> {
        let attr = PathBeneathAttr {
            allowed_access: access.0,
            parent_fd: object.as_raw_fd(),
        };
        // SAFETY: `attr` is a live `landlock_path_beneath_attr`; both fds are open.
        let added = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                self.0.as_raw_fd(),
                RULE_PATH_BENEATH,
                &raw const attr,
                0 as c_uint,
            )
        };
        if added == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// The payload's side: the ruleset the launch left at `slot`.
    fn take(slot: Slot) -> Result<Self, Error> {
        let at = slot.fd();
        // SAFETY: `F_GETFD` only asks whether the slot is open.
        if unsafe { libc::fcntl(at, libc::F_GETFD) } < 0 {
            return Err(Error::Missing(at));
        }
        // SAFETY: open, and lent by the launch for this call alone.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(at) }))
    }

    /// Confine this process and everything it execs; the descriptor closes.
    fn restrict_self(self) -> Result<(), Error> {
        let off = 0 as libc::c_ulong;
        // SAFETY: `prctl` and the syscall touch only this process's own state.
        let restricted = unsafe {
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1 as libc::c_ulong, off, off, off) == 0
                && libc::syscall(
                    libc::SYS_landlock_restrict_self,
                    self.0.as_raw_fd(),
                    0 as c_uint,
                ) == 0
        };
        if restricted {
            Ok(())
        } else {
            Err(Error::Restrict(io::Error::last_os_error()))
        }
    }
}

impl AsFd for Ruleset {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// What a launch promised its payload: a ruleset at [`Slot::Ruleset`], and
/// whether the payload must still grant `Refer` on its own root, which only
/// exists in there.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Landlocked {
    pub(crate) refer_root: bool,
}

impl Landlocked {
    /// Enter the promised ruleset, fail-closed: there is nothing to degrade
    /// to, and the kernel accepted the exact handled set when the parent
    /// created it.
    pub(crate) fn enter(&self) -> Result<(), String> {
        let ruleset = Ruleset::take(Slot::Ruleset)?;
        if self.refer_root {
            open_nosym("/")
                .and_then(|root| ruleset.admit(root.as_fd(), Access::REFER))
                .map_err(|source| Error::Admit {
                    name: "/".to_string(),
                    source,
                })?;
        }
        Ok(ruleset.restrict_self()?)
    }
}

/// The rights a ruleset handles and the scopes it sets.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    handled: Access,
    scoped: Scoped,
}

/// What a kernel at `landlock` can hold of `exec`; `None` where there is
/// nothing to enter.
///
/// # Errors
/// A failed probe, whatever the projection, which tells nothing; and an
/// exec-restricting grant on a kernel without Landlock, which would otherwise
/// run with no kernel exec layer.
fn plan(exec: &ExecProjection, landlock: Landlock) -> Result<Option<Plan>, crate::types::Error> {
    let restricted = matches!(exec, ExecProjection::Restricted(_));
    let abi = match landlock {
        Landlock::At(abi) => abi,
        Landlock::Absent if !restricted => return Ok(None),
        Landlock::Absent => {
            return Err(confinement_unavailable(
                "landlock: this kernel cannot enforce the grant's limits on which programs \
                 may run, because Landlock is unavailable; ral refuses rather than run them \
                 unchecked.  Is the kernel older than 5.13, or does it have Landlock disabled?",
            ));
        }
        Landlock::Unprobed(_) => {
            return Err(confinement_unavailable(&format!(
                "landlock: {landlock}; refusing to launch confined"
            )));
        }
    };
    let handled = match (restricted, abi >= Abi::REFER) {
        (false, _) => Access::NONE,
        (true, false) => Access::EXECUTE,
        (true, true) => Access::EXECUTE | Access::REFER,
    };
    let scoped = if abi >= Abi::SIGNAL_SCOPE {
        Scoped::SIGNAL
    } else {
        Scoped::NONE
    };
    Ok((handled != Access::NONE || scoped != Scoped::NONE).then_some(Plan { handled, scoped }))
}

/// The ruleset for `exec` on a kernel at `landlock`, and what the payload is
/// promised; `None` where there is nothing to enter.
///
/// # Errors
/// As [`plan`]; a ruleset or rule the kernel refuses; an admit that cannot be
/// opened, or that a symlink has replaced since the grant was rendered.
pub(crate) fn build(
    exec: &ExecProjection,
    landlock: Landlock,
) -> Result<Option<(Ruleset, Landlocked)>, crate::types::Error> {
    let Some(Plan { handled, scoped }) = plan(exec, landlock)? else {
        return Ok(None);
    };
    let failed = |why: String| crate::types::Error::new(why, 1);
    let ruleset = Ruleset::create(handled, scoped).map_err(|e| failed(Error::Create(e).into()))?;
    if let ExecProjection::Restricted(rules) = exec {
        admit_exec(&ruleset, rules).map_err(failed)?;
    }
    let refer_root = handled.contains(Access::REFER);
    Ok(Some((ruleset, Landlocked { refer_root })))
}

/// ral's own pinned inode, the loader base, and every allow rule: a dir as
/// the hierarchy beneath it, a file as itself.
fn admit_exec(ruleset: &Ruleset, rules: &[ExecRule]) -> Result<(), String> {
    let execute = |name: &str, fd: BorrowedFd<'_>| {
        ruleset
            .admit(fd, Access::EXECUTE)
            .map_err(|source| Error::Admit {
                name: name.to_string(),
                source,
            })
    };
    // A ral run inside starts its own bundled tools and pipeline anchors by
    // re-executing itself, so no policy names it: as macOS admits its own.
    let own = super::super::reexec::own()?;
    execute(&own.arg0().to_string_lossy(), own.fd())?;
    let mut named: Vec<(String, bool)> = platform_base().into_iter().map(|n| (n, true)).collect();
    for rule in rules {
        let (path, file) = match rule {
            ExecRule::Dir { path, allow: true } => (path, false),
            ExecRule::File { path, allow: true } => (path, true),
            _ => continue,
        };
        named.extend(render_real(path)?.iter().map(|n| (n.as_str().to_owned(), file)));
    }
    for (name, file) in &named {
        let Some(fd) = open_admit(name)? else {
            continue;
        };
        // A file admit names that file alone, never what a directory since
        // put there holds.
        if *file && is_directory(name, &fd)? {
            continue;
        }
        execute(name, fd.as_fd())?;
    }
    Ok(())
}

/// `name`, opened here in the host; `None` where it names nothing.
fn open_admit(name: &str) -> Result<Option<OwnedFd>, Error> {
    match open_nosym(name) {
        Ok(fd) => Ok(Some(fd)),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Err(Error::Race {
            name: name.to_string(),
        }),
        Err(source) => Err(Error::Admit {
            name: name.to_string(),
            source,
        }),
    }
}

/// Never through a symlink: a rule attaches to the inode the open reaches.
fn open_nosym(path: &str) -> io::Result<OwnedFd> {
    use rustix::fs::{CWD, Mode, OFlags, ResolveFlags, openat2};
    Ok(openat2(
        CWD,
        path,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS,
    )?)
}

fn is_directory(name: &str, fd: &OwnedFd) -> Result<bool, Error> {
    use rustix::fs::{FileType, fstat};
    let stat = fstat(fd).map_err(|errno| Error::Admit {
        name: name.to_string(),
        source: errno.into(),
    })?;
    Ok(FileType::from_raw_mode(stat.st_mode) == FileType::Directory)
}

/// The dynamic linkers, and nothing else.  `execve` of a dynamic binary needs
/// `Execute` on the binary and on its `PT_INTERP` file — and of a `#!` script,
/// on its interpreter, which is a carrier rather than base; the shared
/// libraries the loader then maps need no right from this layer.  So the base
/// is a set of regular files — never a directory, which under `/usr/bin`
/// would be a layer that denies nothing.
fn platform_base() -> Vec<String> {
    const PATTERNS: &[&str] = &[
        "/lib/ld*.so*",
        "/lib64/ld*.so*",
        "/lib32/ld*.so*",
        "/usr/lib/ld*.so*",
        "/usr/lib64/ld*.so*",
        "/usr/lib32/ld*.so*",
        // Debian multiarch, e.g. /usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1.
        "/lib/*/ld*.so*",
        "/usr/lib/*/ld*.so*",
    ];
    let mut found: Vec<String> = PATTERNS
        .iter()
        .filter_map(|p| glob::glob(p).ok())
        .flatten()
        .flatten()
        // Merged-/usr makes /lib/x and /usr/lib/x one inode; canonicalise so
        // the two spellings collapse to one rule.
        .filter_map(|p| crate::path::canon::canonicalise_strict(&p).ok())
        .filter(|p| p.is_file())
        .filter_map(|p| p.into_os_string().into_string().ok())
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Which stage refused, and what the user can do about it.
#[derive(Debug)]
enum Error {
    Create(io::Error),
    Admit { name: String, source: io::Error },
    Race { name: String },
    Restrict(io::Error),
    Missing(c_int),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create(e) => write!(f, "landlock: this kernel refused the ruleset: {e}"),
            Self::Admit { name, source } => write!(f, "landlock: cannot admit {name}: {source}"),
            Self::Race { name } => write!(
                f,
                "landlock: {name} was a real path when the grant was rendered and now \
                 resolves through a symlink: a race, not a policy error"
            ),
            Self::Restrict(e) => write!(
                f,
                "landlock: cannot enter the ruleset: {e}; refusing to run unconfined"
            ),
            Self::Missing(at) => write!(
                f,
                "landlock: the launch promised a Landlock ruleset at fd {at} and none \
                 arrived; refusing to run unconfined"
            ),
        }
    }
}

impl From<Error> for String {
    fn from(e: Error) -> Self {
        e.to_string()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[test] test fs scaffolding")]
mod tests {
    use super::*;

    fn restricted() -> ExecProjection {
        ExecProjection::Restricted(Vec::new())
    }

    #[test]
    fn only_the_two_absence_errnos_read_as_a_kernel_without_landlock() {
        assert_eq!(classify(5, 0), Landlock::At(Abi(5)));
        assert_eq!(classify(-1, libc::ENOSYS), Landlock::Absent);
        assert_eq!(classify(-1, libc::EOPNOTSUPP), Landlock::Absent);
        assert_eq!(
            classify(-1, libc::EPERM),
            Landlock::Unprobed(libc::EPERM),
            "a seccomp EPERM is not absence"
        );
    }

    /// Landlock is the only kernel exec layer on Linux, so a grant that limits
    /// which programs run is refused where there is none, not run unchecked;
    /// a failed probe tells nothing, so it refuses every launch.
    #[test]
    fn an_exec_restricting_grant_is_refused_without_landlock() {
        let why = plan(&restricted(), Landlock::Absent)
            .expect_err("a restricted exec grant needs Landlock")
            .message;
        assert!(why.starts_with("sandbox confinement unavailable: "), "{why}");
        assert!(why.contains("Landlock") && why.contains("5.13"), "{why}");
        assert_eq!(
            plan(&ExecProjection::Unrestricted, Landlock::Absent).expect("nothing to enforce"),
            None
        );
        for exec in [restricted(), ExecProjection::Unrestricted] {
            let why = plan(&exec, Landlock::Unprobed(libc::EPERM))
                .expect_err("a failed probe is not a kernel without Landlock")
                .message;
            assert!(why.starts_with("sandbox confinement unavailable: "), "{why}");
            assert!(why.contains("probe failed"), "{why}");
        }
    }

    #[test]
    fn a_restricted_projection_handles_refer_from_abi_two_and_scopes_from_six() {
        let exec = Access::EXECUTE;
        let refer = Access::EXECUTE | Access::REFER;
        for (abi, handled, scoped) in [
            (Abi::EXEC, exec, Scoped::NONE),
            (Abi::REFER, refer, Scoped::NONE),
            (Abi(5), refer, Scoped::NONE),
            (Abi::SIGNAL_SCOPE, refer, Scoped::SIGNAL),
            (Abi(9), refer, Scoped::SIGNAL),
        ] {
            assert_eq!(
                plan(&restricted(), Landlock::At(abi)).expect("a kernel with Landlock"),
                Some(Plan { handled, scoped }),
                "abi {abi}"
            );
        }
    }

    #[test]
    fn an_unrestricted_projection_asks_for_a_ruleset_only_where_the_scope_exists() {
        for abi in [Abi::EXEC, Abi::REFER, Abi(5)] {
            assert_eq!(
                plan(&ExecProjection::Unrestricted, Landlock::At(abi)).expect("plans"),
                None,
                "abi {abi} has no signal scope, so there is nothing to enter"
            );
        }
        for abi in [Abi::SIGNAL_SCOPE, Abi(9)] {
            assert_eq!(
                plan(&ExecProjection::Unrestricted, Landlock::At(abi)).expect("plans"),
                Some(Plan {
                    handled: Access::NONE,
                    scoped: Scoped::SIGNAL
                }),
                "abi {abi}"
            );
        }
    }

    /// The live kernel, where the build itself is under test: without one
    /// there is no ruleset to create.
    fn landlock() -> Option<Landlock> {
        let probed = Landlock::probe();
        matches!(probed, Landlock::At(_)).then_some(probed).or_else(|| {
            eprintln!("skipping: {probed}");
            None
        })
    }

    fn allow(path: &std::path::Path, dir: bool) -> ExecRule {
        let path = crate::path::RealPath::assumed(path);
        if dir {
            ExecRule::Dir { path, allow: true }
        } else {
            ExecRule::File { path, allow: true }
        }
    }

    /// An admit naming nothing, and a file admit since replaced by a
    /// directory, admit nothing; neither is a reason to refuse the launch.
    #[test]
    fn an_absent_admit_and_a_file_admit_now_a_directory_are_dropped() {
        let Some(landlock) = landlock() else {
            return;
        };
        let tmp = tempfile::tempdir().expect("temp dir");
        let rules = vec![
            allow(&tmp.path().join("absent"), true),
            allow(&tmp.path().join("absent/beneath"), false),
            allow(tmp.path(), false),
        ];
        let built = build(&ExecProjection::Restricted(rules), landlock).expect("builds");
        assert!(built.is_some(), "a restricted projection is entered");
    }

    /// A grant renders real paths, so a symlink at one now was planted since.
    #[test]
    fn an_admit_now_reached_through_a_symlink_refuses_the_launch() {
        let Some(landlock) = landlock() else {
            return;
        };
        let tmp = tempfile::tempdir().expect("temp dir");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).expect("outside");
        let allowed = tmp.path().join("allowed");
        std::os::unix::fs::symlink(&outside, &allowed).expect("symlink");
        let Err(why) = build(&ExecProjection::Restricted(vec![allow(&allowed, true)]), landlock)
        else {
            panic!("a symlink would admit its target");
        };
        assert!(why.message.contains("a race"), "{}", why.message);
    }

    #[test]
    fn the_platform_base_is_linker_files_and_never_a_command_directory() {
        let base = platform_base();
        assert!(
            !base.is_empty(),
            "a Linux host runs dynamic binaries, so it has a loader"
        );
        for entry in &base {
            let path = std::path::Path::new(entry);
            assert!(path.is_file(), "{entry} is not a regular file");
            assert!(
                path.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("ld")),
                "{entry} is not a linker"
            );
            for dir in [
                "/bin/",
                "/usr/bin/",
                "/sbin/",
                "/usr/sbin/",
                "/usr/local/bin/",
            ] {
                assert!(
                    !entry.starts_with(dir),
                    "{entry} would make the base admit a whole command directory"
                );
            }
        }
    }
}
