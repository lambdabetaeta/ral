//! The process sandbox a confined payload enters *inside* the bwrap envelope:
//! kernel exec confinement rendering `ExecProjection::Restricted`, and a
//! signal scope closing the same-uid `kill` hole where the host cannot build
//! a pid namespace.  The parent builds the one ruleset ([`build`]), opening
//! every admit in the host and never through a symlink; the payload inherits
//! it at [`Slot::Ruleset`] and enters it ([`Landlocked::enter`]), never before
//! bwrap: a domain handling any fs right forbids `mount(2)`, bwrap's first act.
//!
//! Landlock is allow-list only, so a block inside an allowed directory is
//! rendered by subtraction ([`expand`]): the entries the table still admits,
//! as the tree stands at launch.  A program added there afterwards is denied
//! until the next launch, and a veto is carried only into hierarchies no
//! trusted write reaches ([`vetoes_walk`]).

use super::super::Refusal;
use super::super::warrant::Slot;
use super::{OpenError, open_real};
use crate::capability::{ExecRules, ExecScope, Subject};
use crate::path::{Deny, RealPath, Rendered, render_real};
use crate::sandbox::{ExecProjection, FsProjection, WriteReach};
use libc::{c_int, c_uint};
use rustix::fs::{FileType, Mode, OFlags};
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

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
        static PROBED: LazyLock<Landlock> = LazyLock::new(|| {
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
        });
        *PROBED
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
        let at = Slot::Ruleset;
        let ruleset = Ruleset(at.take().ok_or_else(|| Error::Missing(at.fd()))?);
        if self.refer_root {
            let root = open_real("/".as_ref(), OFlags::empty())?
                .ok_or("landlock: the envelope has no root to admit")?;
            (ruleset.admit(root.as_fd(), Access::REFER)).map_err(|e| Error::admit("/", e))?;
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
fn plan(exec: &ExecProjection, landlock: Landlock) -> Result<Option<Plan>, Refusal> {
    let restricted = matches!(exec, ExecProjection::Restricted(_));
    let abi = match landlock {
        Landlock::At(abi) => abi,
        Landlock::Absent if !restricted => return Ok(None),
        Landlock::Absent => {
            return Err(Refusal::Unavailable(
                "landlock: this kernel cannot enforce the grant's limits on which programs \
                 may run, because Landlock is unavailable; ral refuses rather than run them \
                 unchecked.  Is the kernel older than 5.13, or does it have Landlock disabled?"
                    .into(),
            ));
        }
        Landlock::Unprobed(_) => {
            return Err(Refusal::Unavailable(format!(
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
/// promised; `None` where there is nothing to enter.  `fs` decides where a
/// veto is carried into an allowed directory.
///
/// # Errors
/// As [`plan`]; a ruleset or rule the kernel refuses; an admit that cannot be
/// opened, or that a symlink has replaced since the grant was rendered.
pub(crate) fn build(
    exec: &ExecProjection,
    fs: &FsProjection<Rendered>,
    landlock: Landlock,
) -> Result<Option<(Ruleset, Landlocked)>, Refusal> {
    let Some(Plan { handled, scoped }) = plan(exec, landlock)? else {
        return Ok(None);
    };
    let ruleset = Ruleset::create(handled, scoped)
        .map_err(|e| Refusal::Launch(Error::Create(e).to_string()))?;
    admit_exec(&ruleset, exec, fs)?;
    let refer_root = handled.contains(Access::REFER);
    Ok(Some((ruleset, Landlocked { refer_root })))
}

/// ral's own pinned inode, the loader base, every allowed file as itself, and
/// every allowed directory less what the table blocks beneath it.
fn admit_exec(
    ruleset: &Ruleset,
    exec: &ExecProjection,
    fs: &FsProjection<Rendered>,
) -> Result<(), String> {
    let ExecProjection::Restricted(table) = exec else {
        return Ok(());
    };
    let execute = |name: &Path, fd: BorrowedFd<'_>| {
        (ruleset.admit(fd, Access::EXECUTE)).map_err(|source| Error::admit(name.display(), source))
    };
    // A ral run inside starts its own bundled tools and pipeline anchors by
    // re-executing itself, so no policy names it: as macOS admits its own.
    let own = super::super::reexec::own()?;
    execute(own.arg0(), own.fd())?;
    let base = platform_base();
    let files = table.allowed_files().map(RealPath::as_path);
    for name in base.iter().map(PathBuf::as_path).chain(files) {
        let Some(fd) = open_real(name, OFlags::empty())? else {
            continue;
        };
        // A file admit names that file alone, never what a directory since
        // put there holds.
        if shape(&fd).map_err(|e| Error::admit(name.display(), e))? == FileType::Directory {
            continue;
        }
        execute(name, fd.as_fd())?;
    }
    let walks = vetoes_walk(table, fs)?;
    expand(table, &walks, &mut |admit| {
        ruleset.admit(admit.fd, Access::EXECUTE)
    })?;
    Ok(())
}

/// How the write region stands to one name of an allowed directory: the one
/// classification the envelope and this layer share, the envelope freezing a
/// covered directory and a veto walking every one but a trusted one.  Under
/// an unrestricted `fs` every write reaches it: covered when a veto asks for
/// the freeze, trusted otherwise, as macOS reads it.
pub(crate) fn write_reach<'a>(
    table: &ExecRules,
    fs: &'a FsProjection<Rendered>,
) -> Result<impl Fn(&Rendered) -> WriteReach + 'a, String> {
    let unrestricted = if table.denies().next().is_some() {
        WriteReach::Covered
    } else {
        WriteReach::Trusted
    };
    let reach = match fs {
        FsProjection::Unrestricted => None,
        FsProjection::Restricted(writes) => {
            let admitted = (table.allowed_dirs())
                .map(render_real)
                .collect::<Result<Vec<_>, _>>()?
                .concat();
            Some((writes, admitted))
        }
    };
    Ok(move |name: &Rendered| match &reach {
        None => unrestricted,
        Some((writes, admitted)) => writes.write_reach(name, admitted),
    })
}

/// Where a veto is carried into an allowed directory: wherever no trusted
/// write reaches ([`write_reach`]), since a covered directory is frozen and
/// an apart one unwritable, so the launch's snapshot stays exact.  Under a
/// trusted write a bare-name veto is advisory.
fn vetoes_walk<'a>(
    table: &ExecRules,
    fs: &'a FsProjection<Rendered>,
) -> Result<impl Fn(&RealPath) -> bool + 'a, String> {
    let reach = write_reach(table, fs)?;
    // A name that does not render is walked: over-listing is cost.
    Ok(move |dir: &RealPath| {
        render_real(dir).map_or(true, |names| {
            (names.iter()).any(|name| reach(name) != WriteReach::Trusted)
        })
    })
}

/// One object [`expand`] admits: a directory with its hierarchy, a file as
/// itself.
#[derive(Clone, Copy)]
struct Admit<'a> {
    path: &'a RealPath,
    fd: BorrowedFd<'a>,
}

/// What `table` admits of this host's tree, into `sink`: a live directory
/// with no block beneath it whole; one with a block beneath it entry by
/// entry, each judged by the table, each subdirectory in turn.  A denied
/// directory is never entered: what the table allows beneath it is a live
/// scope of its own, admitted from the top.
fn expand(
    table: &ExecRules,
    vetoes_walk: &dyn Fn(&RealPath) -> bool,
    sink: &mut dyn FnMut(Admit<'_>) -> io::Result<()>,
) -> Result<(), Error> {
    let mut walk = Walk {
        table,
        walks: vetoes_walk,
        sink,
        spine: Vec::new(),
    };
    for root in table.allowed_dirs() {
        if let Some(fd) = open_real(root.as_path(), OFlags::DIRECTORY)? {
            walk.dir(root, fd.as_fd())?;
        }
    }
    Ok(())
}

/// Whether a rule could speak strictly beneath `d` other than through `d`'s
/// own verdict: a deny dir or file strictly within `d` under `Deny`'s
/// identity (over-listing is cost, never a hole), or any veto where vetoes
/// walk `d`.
fn listed(table: &ExecRules, d: &RealPath, vetoes_walk: bool) -> bool {
    table.denies().any(|scope| match scope {
        ExecScope::Dir(p) | ExecScope::File(p) => p.within::<Deny>(d) && !d.within::<Deny>(p),
        ExecScope::Name(_) => vetoes_walk,
        ExecScope::Carrier(_) | ExecScope::Tool(_) => false,
    })
}

/// One expansion in progress.  Every handle is opened relative to its
/// parent's, never through a symlink, handed to the sink, and closed.
struct Walk<'w> {
    table: &'w ExecRules,
    walks: &'w dyn Fn(&RealPath) -> bool,
    sink: &'w mut dyn FnMut(Admit<'_>) -> io::Result<()>,
    /// The `(dev, ino)` of each directory being listed, so a bind mount
    /// looping back onto one ends the descent there.
    spine: Vec<(u64, u64)>,
}

impl Walk<'_> {
    fn dir(&mut self, d: &RealPath, fd: BorrowedFd<'_>) -> Result<(), Error> {
        if !listed(self.table, d, (self.walks)(d)) {
            return self.admit(Admit { path: d, fd });
        }
        let stat = rustix::fs::fstat(fd).map_err(|errno| Error::admit(d, errno))?;
        let at = (stat.st_dev, stat.st_ino);
        if self.spine.contains(&at) {
            return Ok(());
        }
        self.spine.push(at);
        self.list(d, fd)?;
        self.spine.pop();
        Ok(())
    }

    /// `d`'s entries, each judged by the table.
    fn list(&mut self, d: &RealPath, fd: BorrowedFd<'_>) -> Result<(), Error> {
        use rustix::fs::{RawDir, openat};
        let read = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let Some(listing) = reached(openat(fd, c".", read, Mode::empty()), d)? else {
            return Ok(());
        };
        let mut buf = Vec::with_capacity(8192);
        let mut entries = RawDir::new(listing, buf.spare_capacity_mut());
        while let Some(entry) = entries.next() {
            let entry = entry.map_err(|errno| Error::admit(d, errno))?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            // Symlinks are nothing: a rule attaches to the inode one
            // reaches, and the guard judges the real path.
            let shaped = matches!(
                entry.file_type(),
                FileType::Directory | FileType::RegularFile | FileType::Unknown
            );
            if !shaped || name == "." || name == ".." {
                continue;
            }
            let e = d.entry(name);
            let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let Some(object) = reached(openat(fd, entry.file_name(), flags, Mode::empty()), &e)?
            else {
                continue;
            };
            // The inode opened decides, not the type listed before it.
            match shape(&object).map_err(|errno| Error::admit(&e, errno))? {
                FileType::Directory if !self.table.verdict(Subject::Under(&e)).is_denied() => {
                    self.dir(&e, object.as_fd())?;
                }
                FileType::RegularFile if !self.table.verdict(Subject::File(&e)).is_denied() => {
                    self.admit(Admit {
                        path: &e,
                        fd: object.as_fd(),
                    })?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn admit(&mut self, admit: Admit<'_>) -> Result<(), Error> {
        (self.sink)(admit).map_err(|source| Error::admit(admit.path, source))
    }
}

/// `None` where the walk cannot reach an entry: gone since it was listed, a
/// symlink now, or unreadable, which admits it nowhere and fails closed.
fn reached<T>(opened: rustix::io::Result<T>, at: &RealPath) -> Result<Option<T>, Error> {
    use rustix::io::Errno;
    match opened {
        Ok(object) => Ok(Some(object)),
        Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP | Errno::ACCESS) => Ok(None),
        Err(errno) => Err(Error::admit(at, errno)),
    }
}

fn shape(fd: &OwnedFd) -> rustix::io::Result<FileType> {
    Ok(FileType::from_raw_mode(rustix::fs::fstat(fd)?.st_mode))
}

/// The dynamic linkers, and nothing else.  `execve` of a dynamic binary needs
/// `Execute` on the binary and on its `PT_INTERP` file — and of a `#!` script,
/// on its interpreter, which is a carrier rather than base; the shared
/// libraries the loader then maps need no right from this layer.  So the base
/// is a set of regular files — never a directory, which under `/usr/bin`
/// would be a layer that denies nothing.
fn platform_base() -> Vec<PathBuf> {
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
    let mut found: Vec<PathBuf> = PATTERNS
        .iter()
        .filter_map(|p| glob::glob(p).ok())
        .flatten()
        .flatten()
        // Merged-/usr makes /lib/x and /usr/lib/x one inode; canonicalise so
        // the two spellings collapse to one rule.
        .filter_map(|p| crate::path::canon::canonicalise_strict(&p).ok())
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Which stage refused, and what the user can do about it.
#[derive(Debug)]
pub(super) enum Error {
    Create(io::Error),
    Admit { name: String, source: io::Error },
    Open(OpenError),
    Restrict(io::Error),
    Missing(c_int),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create(e) => write!(f, "landlock: this kernel refused the ruleset: {e}"),
            Self::Admit { name, source } => write!(f, "landlock: cannot admit {name}: {source}"),
            Self::Open(e) => e.fmt(f),
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

impl Error {
    fn admit(name: impl fmt::Display, source: impl Into<io::Error>) -> Self {
        Self::Admit {
            name: name.to_string(),
            source: source.into(),
        }
    }
}

impl From<OpenError> for Error {
    fn from(e: OpenError) -> Self {
        Self::Open(e)
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
    use crate::capability::Verdict;
    use std::collections::BTreeSet;

    type Rule = (ExecScope, Verdict);

    fn restricted() -> ExecProjection {
        ExecProjection::Restricted(ExecRules::default())
    }

    fn table_of(rules: &[Rule]) -> ExecRules {
        rules.iter().cloned().collect()
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
            .to_string();
        assert!(
            why.starts_with("sandbox confinement unavailable: "),
            "{why}"
        );
        assert!(why.contains("Landlock") && why.contains("5.13"), "{why}");
        assert_eq!(
            plan(&ExecProjection::Unrestricted, Landlock::Absent).expect("nothing to enforce"),
            None
        );
        for exec in [restricted(), ExecProjection::Unrestricted] {
            let why = plan(&exec, Landlock::Unprobed(libc::EPERM))
                .expect_err("a failed probe is not a kernel without Landlock")
                .to_string();
            assert!(
                why.starts_with("sandbox confinement unavailable: "),
                "{why}"
            );
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
        matches!(probed, Landlock::At(_))
            .then_some(probed)
            .or_else(|| {
                eprintln!("skipping: {probed}");
                None
            })
    }

    fn rule(path: &Path, dir: bool, allow: bool) -> Rule {
        let path = RealPath::assumed(path);
        let scope = if dir {
            ExecScope::Dir(path)
        } else {
            ExecScope::File(path)
        };
        (scope, Verdict::from(allow))
    }

    fn allow(path: &Path, dir: bool) -> Rule {
        rule(path, dir, true)
    }

    fn deny(path: &Path, dir: bool) -> Rule {
        rule(path, dir, false)
    }

    fn veto(name: &str) -> Rule {
        (
            ExecScope::Name(crate::path::command_name_key(name)),
            Verdict::Deny,
        )
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
        let built = build(
            &ExecProjection::Restricted(table_of(&rules)),
            &FsProjection::Unrestricted,
            landlock,
        )
        .expect("builds");
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
        let Err(why) = build(
            &ExecProjection::Restricted(table_of(&[allow(&allowed, true)])),
            &FsProjection::Unrestricted,
            landlock,
        ) else {
            panic!("a symlink would admit its target");
        };
        assert!(why.to_string().contains("symbolic link"), "{why}");
    }

    #[test]
    fn the_platform_base_is_linker_files_and_never_a_command_directory() {
        let base = platform_base();
        assert!(
            !base.is_empty(),
            "a Linux host runs dynamic binaries, so it has a loader"
        );
        for path in &base {
            assert!(path.is_file(), "{} is not a regular file", path.display());
            assert!(
                path.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("ld")),
                "{} is not a linker",
                path.display()
            );
            for dir in [
                "/bin/",
                "/usr/bin/",
                "/sbin/",
                "/usr/sbin/",
                "/usr/local/bin/",
            ] {
                assert!(
                    !path.starts_with(dir),
                    "{} would make the base admit a whole command directory",
                    path.display()
                );
            }
        }
    }

    // ── Expansion ────────────────────────────────────────────────────────

    /// Every regular file of [`tree`].
    const FILES: [&str; 8] = ["a", "x", "n", "s/f", "s/t/g", "s/t/n", "sub/h", "sub/n"];

    /// [`FILES`] under a real `d`.
    fn tree() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("temp dir");
        let d = RealPath::of(tmp.path()).expect("real").as_path().join("d");
        for file in FILES {
            let path = d.join(file);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
            std::fs::write(&path, "").expect("file");
        }
        (tmp, d)
    }

    /// What [`expand`] hands its sink, each handle checked against its name,
    /// and whether it stands for a hierarchy.
    fn expanded(rules: &[Rule], walks: bool) -> Result<BTreeSet<(PathBuf, bool)>, Error> {
        use std::os::unix::fs::MetadataExt;
        let mut seen = BTreeSet::new();
        expand(&table_of(rules), &|_| walks, &mut |admit| {
            let path = admit.path.as_path();
            let opened = rustix::fs::fstat(admit.fd)?;
            assert_eq!(
                opened.st_ino,
                std::fs::symlink_metadata(path)?.ino(),
                "{}",
                admit.path
            );
            let whole = FileType::from_raw_mode(opened.st_mode) == FileType::Directory;
            seen.insert((path.to_path_buf(), whole));
            Ok(())
        })?;
        Ok(seen)
    }

    fn admits(d: &Path, admitted: &[(&str, bool)]) -> BTreeSet<(PathBuf, bool)> {
        (admitted.iter())
            .map(|&(rel, whole)| (d.join(rel), whole))
            .collect()
    }

    /// Each admit is the table's own verdict on a file the walk reached: a
    /// denied file absent, a denied directory never entered, an allow inside
    /// it a root of its own, a vetoed name absent wherever it lies.
    #[test]
    fn an_expansion_admits_exactly_what_the_table_allows() {
        let (_tmp, d) = tree();
        let rules = [
            allow(&d, true),
            deny(&d.join("x"), false),
            deny(&d.join("s"), true),
            allow(&d.join("s/t"), true),
            veto("n"),
        ];
        let table = table_of(&rules);
        let model: BTreeSet<_> = (FILES.iter())
            .map(|f| d.join(f))
            .filter(|f| {
                !table
                    .verdict(Subject::File(&RealPath::assumed(f)))
                    .is_denied()
            })
            .map(|f| (f, false))
            .collect();
        assert_eq!(
            model,
            admits(&d, &[("a", false), ("s/t/g", false), ("sub/h", false)])
        );
        assert_eq!(expanded(&rules, true).expect("expands"), model);
    }

    /// Only the spine to a block is listed; every directory off it is one
    /// rule, however much lies beneath.
    #[test]
    fn a_directory_with_no_block_beneath_it_is_one_whole_admit() {
        let (_tmp, d) = tree();
        assert_eq!(
            expanded(&[allow(&d, true)], true).expect("expands"),
            admits(&d, &[("", true)])
        );
        let spine = admits(
            &d,
            &[
                ("a", false),
                ("x", false),
                ("n", false),
                ("sub", true),
                ("s/t", true),
            ],
        );
        let rules = [allow(&d, true), deny(&d.join("s/f"), false)];
        assert_eq!(expanded(&rules, true).expect("expands"), spine);
    }

    /// A deny holds every spelling of its name, in the kernel as in the guard.
    #[test]
    fn a_deny_by_another_spelling_blocks_the_entry() {
        let (_tmp, d) = tree();
        let rules = [allow(&d, true), deny(&d.join("X"), false)];
        let seen = expanded(&rules, true).expect("expands");
        assert!(!seen.contains(&(d.join("x"), false)), "{seen:?}");
        assert!(seen.contains(&(d.join("a"), false)), "{seen:?}");
    }

    /// Where vetoes do not walk, a vetoed name costs no listing; where they
    /// do, it is gone from every directory.
    #[test]
    fn a_veto_is_carried_only_where_vetoes_walk() {
        let (_tmp, d) = tree();
        let rules = [allow(&d, true), veto("n")];
        assert_eq!(
            expanded(&rules, false).expect("expands"),
            admits(&d, &[("", true)])
        );
        let seen = expanded(&rules, true).expect("expands");
        for vetoed in ["n", "sub/n", "s/t/n"] {
            assert!(
                !seen.contains(&(d.join(vetoed), false)),
                "{vetoed}: {seen:?}"
            );
        }
        assert!(seen.contains(&(d.join("sub/h"), false)), "{seen:?}");
    }

    /// A file admit names a file; a directory since put at its name is no
    /// root, so nothing beneath it is admitted.
    #[test]
    fn a_file_admit_now_a_directory_admits_nothing_beneath() {
        let (_tmp, d) = tree();
        let seen = expanded(&[allow(&d.join("sub"), false)], true).expect("expands");
        assert!(seen.is_empty(), "{seen:?}");
    }

    #[test]
    fn an_absent_root_is_dropped() {
        let (_tmp, d) = tree();
        let seen = expanded(&[allow(&d.join("absent"), true)], true).expect("expands");
        assert!(seen.is_empty(), "{seen:?}");
    }

    /// A grant renders real paths, so a root that is a symlink now was
    /// planted since, which refuses rather than admit its target.
    #[test]
    fn a_root_now_a_symlink_refuses_the_expansion() {
        let (_tmp, d) = tree();
        let link = d.join("link");
        std::os::unix::fs::symlink(d.join("sub"), &link).expect("symlink");
        let refused = expanded(&[allow(&link, true)], true);
        assert!(
            matches!(refused, Err(Error::Open(OpenError::Linked(_)))),
            "{refused:?}"
        );
    }

    /// A veto walks the hierarchies the envelope keeps unwritable, frozen or
    /// apart, and never a trusted one.
    #[test]
    fn a_veto_walks_every_hierarchy_but_a_trusted_one() {
        let bin = Path::new("/ral-test/w/bin");
        let table = |rules: &[Rule]| ExecProjection::Restricted(table_of(rules));
        let walks = |exec: &ExecProjection, write: &[&str]| {
            let ExecProjection::Restricted(table) = exec else {
                unreachable!("an exec grant");
            };
            let write: Vec<String> = write.iter().map(ToString::to_string).collect();
            let fs = FsProjection::Restricted(crate::sandbox::FsRules {
                write_prefixes: crate::path::render_paths(&write).expect("renders"),
                ..crate::sandbox::FsRules::default()
            });
            vetoes_walk(table, &fs).expect("renders")(&RealPath::assumed(bin))
        };
        let exec = table(&[allow(bin, true), veto("n")]);
        assert!(walks(&exec, &["/ral-test/w"]), "covered");
        assert!(walks(&exec, &[]), "apart");
        assert!(!walks(&exec, &["/ral-test/w/bin"]), "trusted");
        let unrestricted = |exec: &ExecProjection| {
            let ExecProjection::Restricted(table) = exec else {
                unreachable!("an exec grant");
            };
            vetoes_walk(table, &FsProjection::Unrestricted).expect("renders")(&RealPath::assumed(
                bin,
            ))
        };
        assert!(unrestricted(&exec), "a veto under an unrestricted fs");
        let no_veto = table(&[allow(bin, true)]);
        assert!(!unrestricted(&no_veto), "nothing frozen, nothing to walk");
    }
}
