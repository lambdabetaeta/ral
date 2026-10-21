//! The process sandbox a confined payload enters *inside* the bwrap envelope:
//! kernel exec confinement rendering `ExecProjection::Restricted`, and a
//! signal scope closing the same-uid `kill` hole where the host cannot build
//! a pid namespace.  The parent builds the one ruleset ([`build`]), opening
//! every admit in the host and never through a symlink; the payload inherits
//! it at [`Slot::Ruleset`] and enters it ([`Landlocked::enter`]), never before
//! bwrap: a domain handling any fs right forbids `mount(2)`, bwrap's first act.
//!
//! Landlock is allow-list only, so an exclusion inside an allowed directory
//! is rendered by subtraction ([`expand`]): the entries the table still
//! admits, as the tree stands at launch.  A program added there afterwards is
//! denied until the next launch, and a name veto is carried only into
//! hierarchies no write reaches, never beneath `Any` ([`vetoes_walk`]); a
//! directory the envelope masks is never covered from above.
//!
//! A rule attaches to a host object, and a walk from a file inside the
//! envelope meets only the inodes above it there, so every directory
//! admission is lowered against the envelope's bind roots ([`lowered`]).

use super::super::Refusal;
use super::super::warrant::Slot;
use super::{DenyMask, OpenError, open_real};
use crate::capability::{ExecRules, ExecScope, Subject};
use crate::path::{Allow, Deny, Object, RealPath, Rendered};
use crate::sandbox::{ExecProjection, RenderedFs};
use libc::{c_int, c_uint};
use rustix::fs::{FileType, Mode, OFlags};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
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

/// The table the kernel must enforce: a restricted exec projection, less
/// one whose every exclusion is a name veto beneath `Any`, which nothing
/// walks there ([`vetoes_walk`]) and so subtracts nothing.
fn kernel_table(exec: &ExecProjection) -> Option<&ExecRules> {
    let ExecProjection::Restricted(table) = exec else {
        return None;
    };
    let bare = table.supports(Subject::Any)
        && table
            .denies()
            .all(|scope| matches!(scope, ExecScope::Name(_)));
    (!bare).then_some(table)
}

/// What a kernel at `landlock` can hold of `exec`; `None` where there is
/// nothing to enter.
///
/// # Errors
/// A failed probe, whatever the projection, which tells nothing; and an
/// exec-restricting grant on a kernel without Landlock, which would otherwise
/// run with no kernel exec layer.
fn plan(exec: &ExecProjection, landlock: Landlock) -> Result<Option<Plan>, Refusal> {
    let restricted = kernel_table(exec).is_some();
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
/// promised; `None` where there is nothing to enter.  `roots` are the
/// objects the envelope shows, which a directory admission is lowered onto;
/// `fs` decides where a name veto is carried into one.
///
/// # Errors
/// As [`plan`]; a ruleset or rule the kernel refuses; an admit that cannot be
/// opened, or that a symlink has replaced since the grant was rendered.
pub(crate) fn build(
    exec: &ExecProjection,
    fs: &RenderedFs,
    roots: &[RealPath],
    landlock: Landlock,
) -> Result<Option<(Ruleset, Landlocked)>, Refusal> {
    let Some(Plan { handled, scoped }) = plan(exec, landlock)? else {
        return Ok(None);
    };
    let ruleset = Ruleset::create(handled, scoped)
        .map_err(|e| Refusal::Launch(Error::Create(e).to_string()))?;
    if let Some(table) = kernel_table(exec) {
        admit_exec(&ruleset, table, fs, roots)?;
    }
    let refer_root = handled.contains(Access::REFER);
    Ok(Some((ruleset, Landlocked { refer_root })))
}

/// Every live file as itself, and every live directory lowered onto the
/// envelope, less what the table excludes beneath it.
fn admit_exec(
    ruleset: &Ruleset,
    table: &ExecRules,
    fs: &RenderedFs,
    roots: &[RealPath],
) -> Result<(), String> {
    for file in table.allowed_files() {
        let name = file.as_path();
        let Some(fd) = open_real(name, OFlags::empty())? else {
            continue;
        };
        // A file admit names that file alone, never what a directory since
        // put there holds.
        if shape(&fd).map_err(|e| Error::admit(crate::path::shown(name), e))? == FileType::Directory
        {
            continue;
        }
        (ruleset.admit(fd.as_fd(), Access::EXECUTE))
            .map_err(|source| Error::admit(crate::path::shown(name), source))?;
    }
    let walks = vetoes_walk(table, fs);
    let masks: Vec<_> = (fs.rules().into_iter().flat_map(|rules| &rules.deny))
        .filter(|path| matches!(DenyMask::over(path), DenyMask::EmptyDir))
        .cloned()
        .collect();
    expand(table, roots, &walks, &masks, &mut |admit| {
        ruleset.admit(admit.fd, Access::EXECUTE)
    })?;
    Ok(())
}

/// Where a name veto is carried into an allowed directory: wherever no write
/// reaches it, so the launch's snapshot stays exact and building then
/// running in one command still works.  A veto beneath a write, and beneath
/// `Any`, is advisory.
fn vetoes_walk<'a>(table: &'a ExecRules, fs: &'a RenderedFs) -> impl Fn(&RealPath) -> bool + 'a {
    let any = table.supports(Subject::Any);
    move |dir: &RealPath| !any && !written(fs, dir)
}

/// Whether some write reaches `dir`: it lies within a write prefix, or a
/// write prefix lies within it.  Every write does under an unrestricted fs.
fn written(fs: &RenderedFs, dir: &RealPath) -> bool {
    let Some(rules) = fs.rules() else {
        return true;
    };
    // A name that does not render is walked: over-listing is cost.
    #[allow(
        clippy::disallowed_methods,
        reason = "native: a name that does not render is walked"
    )]
    let Some(name) = dir.as_path().to_str() else {
        return false;
    };
    (rules.write.iter().flat_map(Object::names))
        .any(|w| w.holds(name) || dir.contains_path::<Allow>(w.as_str().as_ref()))
}

/// The objects a live directory admission puts a rule on.  A rule on `dir`
/// itself where some bind root lies at or above it, since a walk inside the
/// envelope from a file in `dir` then meets `dir`'s inode; and one on every
/// bind root inside `dir`, whatever its name, since a walk from there meets
/// the root and the envelope's own directories, never `dir`: an alias, or a
/// sub-bind under a ceiling that reads only it.  `Any` is `/`, so every root.
fn lowered(table: &ExecRules, roots: &[RealPath]) -> BTreeSet<RealPath> {
    if table.supports(Subject::Any) {
        return roots.iter().cloned().collect();
    }
    (table.allowed_dirs())
        .flat_map(|dir| {
            let at = roots.iter().any(|root| dir.within::<Allow>(root));
            let inside = roots.iter().filter(|root| root.within::<Allow>(dir));
            at.then(|| dir.clone()).into_iter().chain(inside.cloned())
        })
        .collect()
}

/// One object [`expand`] admits: a directory with its hierarchy, a file as
/// itself.
#[derive(Clone, Copy)]
struct Admit<'a> {
    path: &'a RealPath,
    fd: BorrowedFd<'a>,
}

/// What `table` admits of this host's tree, into `sink`: each root of
/// [`lowered`] the table permits, whole where nothing is excluded beneath it,
/// else entry by entry, each judged by the table, each subdirectory in turn.
/// An excluded directory is never entered: what the table allows beneath it
/// is a live scope of its own, admitted from the top.  A directory in `masks`
/// is excluded likewise: the envelope shows a scratch tmpfs there that the
/// payload may `chmod` open and write, so no rule may reach it from above.
fn expand(
    table: &ExecRules,
    roots: &[RealPath],
    walks: &dyn Fn(&RealPath) -> bool,
    masks: &[Rendered],
    sink: &mut dyn FnMut(Admit<'_>) -> io::Result<()>,
) -> Result<(), Error> {
    let mut walk = Walk {
        table,
        walks,
        masks,
        sink,
        spine: Vec::new(),
    };
    for root in lowered(table, roots) {
        if let Some(fd) = open_real(root.as_path(), OFlags::empty())? {
            walk.object(&root, fd.as_fd())?;
        }
    }
    Ok(())
}

/// Whether a rule could speak strictly beneath `d` other than through `d`'s
/// own evidence: an exclusion of a directory or file strictly within `d`
/// under `Deny`'s identity (over-listing is cost, never a hole), a masked
/// directory likewise, or any name veto where vetoes `walks` `d`.
fn listed(table: &ExecRules, masks: &[Rendered], d: &RealPath, walks: bool) -> bool {
    let masked = masks
        .iter()
        .any(|m| d.contains_path::<Deny>(m.as_str().as_ref()) && !held(m, d));
    masked
        || table.denies().any(|scope| match scope {
            ExecScope::Dir(p) | ExecScope::File { path: p, .. } => {
                p.within::<Deny>(d) && !d.within::<Deny>(p)
            }
            ExecScope::Name(_) => walks,
            ExecScope::Any | ExecScope::Tool(_) => false,
        })
}

/// Whether `mask` holds `path`; a name that is not Unicode lies in no
/// rendered one.
#[allow(
    clippy::disallowed_methods,
    reason = "native: a name that is not Unicode lies in no rendered mask"
)]
fn held(mask: &Rendered, path: &RealPath) -> bool {
    path.as_path().to_str().is_some_and(|name| mask.holds(name))
}

/// One expansion in progress.  Every handle is opened relative to its
/// parent's, never through a symlink, handed to the sink, and closed.
struct Walk<'w> {
    table: &'w ExecRules,
    walks: &'w dyn Fn(&RealPath) -> bool,
    masks: &'w [Rendered],
    sink: &'w mut dyn FnMut(Admit<'_>) -> io::Result<()>,
    /// The `(dev, ino)` of each directory being listed, so a bind mount
    /// looping back onto one ends the descent there.
    spine: Vec<(u64, u64)>,
}

impl Walk<'_> {
    fn dir(&mut self, d: &RealPath, fd: BorrowedFd<'_>) -> Result<(), Error> {
        if !listed(self.table, self.masks, d, (self.walks)(d)) {
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
            self.object(&e, object.as_fd())?;
        }
        Ok(())
    }

    /// `e` as the inode opened decides, not the type listed before it: a
    /// directory the table permits, entered; a permitted file, admitted.
    fn object(&mut self, e: &RealPath, fd: BorrowedFd<'_>) -> Result<(), Error> {
        let kind = shape(fd).map_err(|errno| Error::admit(e, errno))?;
        let subject = match kind {
            FileType::Directory => Subject::Under(e),
            FileType::RegularFile => Subject::File(e),
            _ => return Ok(()),
        };
        let masked = kind == FileType::Directory && self.masks.iter().any(|m| held(m, e));
        if masked || !self.table.evidence(subject).permits() {
            return Ok(());
        }
        match kind {
            FileType::Directory => self.dir(e, fd),
            _ => self.admit(Admit { path: e, fd }),
        }
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

fn shape(fd: impl AsFd) -> rustix::io::Result<FileType> {
    Ok(FileType::from_raw_mode(rustix::fs::fstat(fd)?.st_mode))
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
    use crate::capability::Evidence;
    use crate::path::{render_objects, render_paths};
    use crate::sandbox::{FsProjection, FsRules};
    use std::path::{Path, PathBuf};

    type Rule = (ExecScope, Evidence);

    /// The roots of an unrestricted fs.
    fn whole() -> Vec<RealPath> {
        vec![RealPath::assumed("/")]
    }

    fn real(path: &str) -> RealPath {
        RealPath::assumed(path)
    }

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
            ExecScope::file(path)
        };
        (
            scope,
            if allow {
                Evidence::ALLOW
            } else {
                Evidence::DENY
            },
        )
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
            Evidence::DENY,
        )
    }

    fn any() -> Rule {
        (ExecScope::Any, Evidence::ALLOW)
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
            &whole(),
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
            &whole(),
            landlock,
        ) else {
            panic!("a symlink would admit its target");
        };
        assert!(why.to_string().contains("symbolic link"), "{why}");
    }

    // ── Expansion ────────────────────────────────────────────────────────

    /// Every regular file of [`tree`].
    const FILES: [&str; 8] = ["a", "x", "n", "s/f", "s/t/g", "s/t/n", "sub/h", "sub/n"];

    /// [`FILES`] under a real `d`.
    fn tree() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("temp dir");
        let d = RealPath::of_path(tmp.path())
            .expect("real")
            .as_path()
            .join("d");
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
        expanded_on(rules, &whole(), walks)
    }

    fn expanded_on(
        rules: &[Rule],
        roots: &[RealPath],
        walks: bool,
    ) -> Result<BTreeSet<(PathBuf, bool)>, Error> {
        expanded_masking(rules, roots, walks, &[])
    }

    fn expanded_masking(
        rules: &[Rule],
        roots: &[RealPath],
        walks: bool,
        masks: &[Rendered],
    ) -> Result<BTreeSet<(PathBuf, bool)>, Error> {
        use std::os::unix::fs::MetadataExt;
        let mut seen = BTreeSet::new();
        expand(&table_of(rules), roots, &|_| walks, masks, &mut |admit| {
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
    /// denied file absent, a denied directory never entered and an allow
    /// inside it no root (an exclusion is never reopened, §7.5), a vetoed
    /// name absent wherever it lies.
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
                table
                    .evidence(Subject::File(&real(&f.to_string_lossy())))
                    .permits()
            })
            .map(|f| (f, false))
            .collect();
        assert_eq!(model, admits(&d, &[("a", false), ("sub/h", false)]));
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

    /// The write prefixes of an fs projection, each a rendered object.
    fn writing(prefixes: &[&str]) -> RenderedFs {
        let write = render_objects(prefixes.iter().map(|p| (real(p), None))).expect("renders");
        FsProjection::Restricted(FsRules {
            write,
            ..FsRules::default()
        })
    }

    /// A name veto walks a directory only where no write reaches it, whether
    /// the write holds the directory or lies inside it, and never beneath
    /// `Any` or under an unrestricted fs, where every write does.
    #[test]
    fn a_name_veto_walks_only_where_no_write_reaches() {
        let bin = real("/ral-test/w/bin");
        let table = table_of(&[allow(bin.as_path(), true), veto("n")]);
        let walks = |fs: &RenderedFs| vetoes_walk(&table, fs)(&bin);
        assert!(walks(&writing(&[])), "apart");
        assert!(walks(&writing(&["/ral-test/other"])), "elsewhere");
        assert!(!walks(&writing(&["/ral-test/w"])), "within a write");
        assert!(!walks(&writing(&["/ral-test/w/bin"])), "the write itself");
        assert!(!walks(&writing(&["/ral-test/w/bin/out"])), "a write inside");
        assert!(!walks(&FsProjection::Unrestricted), "every write reaches");
        let beneath_any = table_of(&[any(), allow(bin.as_path(), true), veto("n")]);
        assert!(
            !vetoes_walk(&beneath_any, &writing(&[]))(&bin),
            "beneath Any"
        );
    }

    /// Every directory admission is lowered onto the bind roots: a rule on
    /// the directory where a root lies at or above it, one on each root
    /// inside it, `Any` being every root.
    #[test]
    fn a_directory_admission_is_lowered_onto_the_bind_roots() {
        let lowered = |rules: &[Rule], roots: &[&str]| {
            let roots: Vec<_> = roots.iter().map(|r| real(r)).collect();
            lowered(&table_of(rules), &roots)
                .iter()
                .map(|r| r.as_path().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let tools = [allow(Path::new("/tools"), true)];
        assert_eq!(lowered(&tools, &["/"]), ["/tools"], "a root above");
        assert_eq!(lowered(&tools, &["/tools"]), ["/tools"], "a root at it");
        assert_eq!(
            lowered(&tools, &["/tools/sub"]),
            ["/tools/sub"],
            "a sub-bind"
        );
        assert_eq!(
            lowered(&tools, &["/", "/tools/sub", "/other"]),
            ["/tools", "/tools/sub"],
            "above and inside, never beside"
        );
        assert!(lowered(&tools, &["/other"]).is_empty(), "not shown at all");
        assert_eq!(
            lowered(&[any()], &["/usr", "/work"]),
            ["/usr", "/work"],
            "Any is every root"
        );
        assert_eq!(lowered(&[any()], &["/"]), ["/"], "or `/` itself");
    }

    /// An `every` table whose exclusions are all name vetoes subtracts
    /// nothing, so the kernel handles no exec for it and keeps its signal
    /// scope; one that excludes a path still does.
    #[test]
    fn an_every_table_with_only_name_vetoes_handles_no_exec() {
        let of = |rules: &[Rule]| ExecProjection::Restricted(table_of(rules));
        let bare = of(&[any(), veto("rm")]);
        assert!(kernel_table(&bare).is_none());
        assert_eq!(
            plan(&bare, Landlock::At(Abi::SIGNAL_SCOPE)).expect("plans"),
            Some(Plan {
                handled: Access::NONE,
                scoped: Scoped::SIGNAL
            })
        );
        assert_eq!(plan(&bare, Landlock::At(Abi(5))).expect("plans"), None);
        assert_eq!(
            plan(&bare, Landlock::Absent).expect("nothing to enforce"),
            None
        );
        let cut = of(&[any(), deny(Path::new("/x"), false)]);
        assert!(kernel_table(&cut).is_some());
        assert!(kernel_table(&of(&[allow(Path::new("/x"), true)])).is_some());
    }

    /// A bind root inside an admitted directory is a root of its own, and the
    /// exclusions lying inside it are subtracted along its spine; one the
    /// table excludes whole is no root.
    #[test]
    fn a_sub_bind_is_expanded_from_its_own_root() {
        let (_tmp, d) = tree();
        let sub = RealPath::assumed(d.join("s"));
        let rules = [allow(&d, true), deny(&d.join("s/f"), false)];
        let seen = expanded_on(&rules, std::slice::from_ref(&sub), true).expect("expands");
        assert_eq!(
            seen,
            admits(&d, &[("s/t", true)]),
            "the root `s`, less the file `s/f`, never `d` itself"
        );
        let rules = [allow(&d, true), deny(&d.join("s"), true)];
        let seen = expanded_on(&rules, &[sub], true).expect("expands");
        assert!(seen.is_empty(), "an excluded root admits nothing: {seen:?}");
    }

    /// An fs-denied directory is a scratch tmpfs the payload may `chmod` open
    /// and write, so no exec rule covers it from above: the admitted directory
    /// is lowered to rules around it, and a mask at the root admits nothing.
    #[test]
    fn a_masked_directory_under_an_admitted_one_is_covered_by_no_rule() {
        let (_tmp, d) = tree();
        let mask = |rel: &str| {
            render_paths(&[d.join(rel).to_string_lossy().into_owned()]).expect("renders")
        };
        let rules = [allow(&d, true)];
        let seen = expanded_masking(&rules, &whole(), false, &mask("s")).expect("expands");
        assert_eq!(
            seen,
            admits(
                &d,
                &[("a", false), ("x", false), ("n", false), ("sub", true)]
            ),
            "everything but `s`, which the walk never enters"
        );
        let seen = expanded_masking(&rules, &[RealPath::assumed(d.join("s"))], false, &mask("s"))
            .expect("expands");
        assert!(seen.is_empty(), "a masked root admits nothing: {seen:?}");
    }
}
