//! The process sandbox a confined payload enters *inside* the bwrap envelope:
//! kernel exec confinement rendering `ExecProjection::Restricted`, and scopes
//! closing the same-uid `kill` hole where the host cannot build a pid
//! namespace and, under `net`, the host's abstract Unix sockets.  The parent
//! builds the one ruleset ([`build`]), opening every admit in the host and
//! never through a symlink; the payload inherits it at the slot its warrant
//! names and enters it ([`Landlocked::enter`]), never before bwrap: a domain
//! handling any fs right forbids `mount(2)`, bwrap's first act.
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
//!
//! `LANDLOCK_ACCESS_FS_RESOLVE_UNIX` confines pathname socket connections on
//! Linux 7.1 (ABI 9). Only stated fs writes admit outside sockets; an
//! unrestricted fs admits none. Every ABI 9 envelope handles fs rights and
//! therefore cannot mount; ral builds the ruleset before bwrap mounts and
//! enters it afterwards.

use super::super::Refusal;
use super::super::warrant::{Handoff, Slot};
use super::{DenyMask, OpenError, leaf_fd, open_dir_nofollow, open_nofollow, standing};
use crate::authority::policy::{ExecDeny, ExecRules, Subject};
use crate::authority::sandbox::{ExecProjection, RenderedFs};
use crate::path::{Dir, Installing, Kind, LeafMode, Located, Object, ObjectId, RealPath, Trace};
use crate::site::{Silent, site};
use libc::{c_int, c_uint};
use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::LazyLock;

/// `LANDLOCK_CREATE_RULESET_VERSION` in the UAPI; not exported by `libc`.
const CREATE_RULESET_VERSION: c_uint = 1;
/// `LANDLOCK_RULE_PATH_BENEATH`.
const RULE_PATH_BENEATH: c_uint = 1;

/// A Landlock ABI level, as `landlock_create_ruleset` reports it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Abi(u32);

impl Abi {
    /// `LANDLOCK_ACCESS_FS_EXECUTE`, Linux 5.13: Landlock's first ABI.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) const EXEC: Self = Self(1);
    /// `LANDLOCK_ACCESS_FS_REFER`, Linux 5.19.
    pub(crate) const REFER: Self = Self(2);
    /// `LANDLOCK_SCOPE_*`, Linux 6.12.
    pub(crate) const SCOPES: Self = Self(6);
    /// `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`, Linux 7.1.
    pub(crate) const RESOLVE_UNIX: Self = Self(9);

    #[cfg(test)]
    pub(crate) const fn level(self) -> u32 {
        self.0
    }
}

/// The ABI ral itself needs: [`plan`] refuses an exec-restricting grant on a
/// kernel without Landlock, and every ABI from `Execute` on can enforce it;
/// `Refer` and the scopes only add to it.
#[cfg(any(test, feature = "test-util"))]
pub(crate) const BASELINE_ABI: u32 = Abi::EXEC.0;

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
                site!(
                    Silent::LandlockVersion,
                    libc::syscall(
                        libc::SYS_landlock_create_ruleset,
                        std::ptr::null::<libc::c_void>(),
                        0usize,
                        CREATE_RULESET_VERSION,
                    )
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

#[cfg(any(test, feature = "test-util"))]
pub(crate) fn probe_abi() -> Result<u32, i32> {
    match Landlock::probe() {
        Landlock::Absent => Ok(0),
        Landlock::At(Abi(level)) => Ok(level),
        Landlock::Unprobed(errno) => Err(errno),
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
    /// `LANDLOCK_ACCESS_FS_RESOLVE_UNIX`, Linux 7.1.
    pub(crate) const RESOLVE_UNIX: Self = Self(1 << 16);

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
    /// `ABSTRACT_UNIX_SOCKET | SIGNAL`: without `--unshare-net` the abstract
    /// namespace is the host's, and a socket there reaches a daemon acting
    /// with the user's authority.
    pub(crate) const ENVELOPE: Self = Self(1 | 1 << 1);
}

/// A ruleset not yet entered: built by the parent, entered by the payload.
struct Ruleset(OwnedFd);

impl Ruleset {
    fn create(handled: Access, scoped: Scoped) -> io::Result<Self> {
        let attr = RulesetAttr {
            handled_access_fs: handled.0,
            handled_access_net: 0,
            scoped: scoped.0,
        };
        // SAFETY: `attr` is a live `landlock_ruleset_attr` of the size passed.
        let fd = unsafe {
            site!(
                Silent::LandlockCreate,
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    &raw const attr,
                    size_of::<RulesetAttr>(),
                    0 as c_uint,
                )
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
            site!(
                Silent::LandlockAdmit,
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    self.0.as_raw_fd(),
                    RULE_PATH_BENEATH,
                    &raw const attr,
                    0 as c_uint,
                )
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
                && site!(
                    Silent::LandlockRestrict,
                    libc::syscall(
                        libc::SYS_landlock_restrict_self,
                        self.0.as_raw_fd(),
                        0 as c_uint,
                    )
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

/// What a launch promised its payload: a ruleset at `at`, and whether the
/// payload must still grant `Refer` on its own root, which only exists in
/// there.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Landlocked {
    pub(crate) refer_root: bool,
    pub(crate) at: Slot,
}

impl Landlocked {
    /// Enter the promised ruleset, fail-closed: there is nothing to degrade
    /// to, and the kernel accepted the exact handled set when the parent
    /// created it.
    pub(crate) fn enter(&self) -> Result<(), String> {
        let at = self.at;
        let ruleset = Ruleset(at.take().ok_or_else(|| Error::Missing(at.fd()))?);
        if self.refer_root {
            let root = open_dir_nofollow(&RealPath::universal())?
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
            .all(|scope| matches!(scope, ExecDeny::Name(_)));
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
    let when = |condition, access| if condition { access } else { Access::NONE };
    let fs =
        when(restricted, Access::EXECUTE) | when(abi >= Abi::RESOLVE_UNIX, Access::RESOLVE_UNIX);
    let handled = fs | when(fs != Access::NONE && abi >= Abi::REFER, Access::REFER);
    let scoped = if abi >= Abi::SCOPES {
        Scoped::ENVELOPE
    } else {
        Scoped::NONE
    };
    Ok((handled != Access::NONE || scoped != Scoped::NONE).then_some(Plan { handled, scoped }))
}

/// The ruleset for `exec` on a kernel at `landlock`, lent to `handoff`, and
/// what the payload is promised; `None` where there is nothing to enter.
/// `roots` are the objects the envelope shows, which a directory admission is
/// lowered onto; `fs` decides where a name veto is carried into one.
///
/// # Errors
/// As [`plan`]; a ruleset or rule the kernel refuses; an admit that cannot be
/// opened, or that a symlink has replaced since the grant was rendered, or a
/// lend the descriptor limit refuses.
pub(crate) fn build(
    exec: &ExecProjection,
    fs: &RenderedFs,
    roots: &[RealPath],
    landlock: Landlock,
    handoff: &mut Handoff,
) -> Result<Option<Landlocked>, Refusal> {
    let Some(Plan { handled, scoped }) = plan(exec, landlock)? else {
        return Ok(None);
    };
    let ruleset = Ruleset::create(handled, scoped)
        .map_err(|e| Refusal::Launch(Error::Create(e).to_string()))?;
    if let Some(table) = kernel_table(exec) {
        admit_exec(&ruleset, table, fs, roots)?;
    }
    if handled.contains(Access::RESOLVE_UNIX) {
        admit_sockets(fs, roots, &mut |admit| {
            ruleset.admit(admit.fd, Access::RESOLVE_UNIX)
        })
        .map_err(String::from)?;
    }
    Ok(Some(Landlocked {
        refer_root: handled.contains(Access::REFER),
        at: handoff.lend(ruleset.as_fd())?,
    }))
}

/// Stated writes alone admit outside sockets; denies are masked by bwrap.
fn admit_sockets(
    fs: &RenderedFs,
    roots: &[RealPath],
    sink: &mut dyn FnMut(Admit<'_>) -> io::Result<()>,
) -> Result<(), Error> {
    let Some(rules) = fs.rules() else {
        return Ok(());
    };
    let mut dirs = Vec::new();
    for object in &rules.write {
        let path = &object.position;
        let Some(held) = open_nofollow(path)? else {
            continue;
        };
        if held.kind() == Some(Kind::Dir) {
            dirs.push(path.clone());
        } else {
            let fd = leaf_fd(&held).map_err(|e| Error::admit(path, e))?;
            sink(Admit { path, fd }).map_err(|e| Error::admit(path, e))?;
        }
    }
    for path in lowered(dirs, roots) {
        let Some(held) = open_nofollow(&path)? else {
            continue;
        };
        let fd = leaf_fd(&held).map_err(|e| Error::admit(&path, e))?;
        sink(Admit { path: &path, fd }).map_err(|e| Error::admit(&path, e))?;
    }
    Ok(())
}

/// Every live file as itself, and every live directory lowered onto the
/// envelope, less what the table excludes beneath it.
fn admit_exec(
    ruleset: &Ruleset,
    table: &ExecRules,
    fs: &RenderedFs,
    roots: &[RealPath],
) -> Result<(), String> {
    let sink = &mut |admit: Admit<'_>| ruleset.admit(admit.fd, Access::EXECUTE);
    admit_files(table, sink)?;
    let walks = vetoes_walk(table, fs);
    expand(table, roots, &walks, &masked(fs), sink)?;
    Ok(())
}

/// The positions the envelope masks with a scratch tmpfs: each deny that
/// stands as a directory now.
fn masked(fs: &RenderedFs) -> Vec<RealPath> {
    (fs.rules().into_iter().flat_map(|rules| &rules.deny))
        .filter_map(standing)
        .filter(|stood| DenyMask::of(stood.kind()) == DenyMask::EmptyDir)
        .map(|stood| stood.real().clone())
        .collect()
}

/// Each file `table` allows, as itself, into `sink`.
fn admit_files(
    table: &ExecRules,
    sink: &mut dyn FnMut(Admit<'_>) -> io::Result<()>,
) -> Result<(), Error> {
    for file in table.allowed_files() {
        let Some(held) = open_nofollow(file)? else {
            continue;
        };
        // A file admit names that file alone, never what a directory since
        // put there holds.
        if held.kind() == Some(Kind::Dir)
            || !table
                .evidence(Subject::Program(crate::path::Subject::Access(held.trace())))
                .permits()
        {
            continue;
        }
        let fd = leaf_fd(&held).map_err(|e| Error::admit(file, e))?;
        sink(Admit { path: file, fd }).map_err(|source| Error::admit(file, source))?;
    }
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
    (rules.write.iter().flat_map(Object::names))
        .any(|w| w.holds_position(dir) || w.under_position(dir))
}

/// The objects a live directory admission puts a rule on.  A rule on `dir`
/// itself where some bind root lies at or above it, since a walk inside the
/// envelope from a file in `dir` then meets `dir`'s inode; and one on every
/// bind root inside `dir`, whatever its name, since a walk from there meets
/// the root and the envelope's own directories, never `dir`: an alias, or a
/// sub-bind under a ceiling that reads only it.  `Any` is `/`, so every root.
fn lowered(dirs: impl IntoIterator<Item = RealPath>, roots: &[RealPath]) -> BTreeSet<RealPath> {
    dirs.into_iter()
        .flat_map(|dir| {
            let at = roots.iter().any(|root| root.contains(&dir));
            let inside: Vec<_> = (roots.iter())
                .filter(|root| dir.contains(root))
                .cloned()
                .collect();
            at.then_some(dir).into_iter().chain(inside)
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
    masks: &[RealPath],
    sink: &mut dyn FnMut(Admit<'_>) -> io::Result<()>,
) -> Result<(), Error> {
    let mut holding = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    for deny in table.denies() {
        let (ExecDeny::Dir(path) | ExecDeny::File(path)) = deny else {
            continue;
        };
        for trace in std::iter::once(path.real.named()).chain(path.real.target()) {
            let mut ids = trace.spine_ids();
            if let Some(id) = trace.leaf_id() {
                excluded.insert(id);
                ids.next_back();
            }
            holding.extend(ids);
        }
    }
    let mut walk = Walk {
        table,
        walks,
        masks,
        sink,
        holding,
        excluded,
        spine: Vec::new(),
    };
    let dirs = table
        .allowed_dirs()
        .chain(table.supports(Subject::Any).then(RealPath::universal));
    for root in lowered(dirs, roots) {
        match open_nofollow(&root)?.map(Located::into_dir) {
            Some(Ok(dir)) => walk.dir(&dir)?,
            Some(Err(file)) => walk.file(&file)?,
            None => {}
        }
    }
    Ok(())
}

/// Whether a rule could speak strictly beneath `d` other than through `d`'s
/// own evidence: an exclusion of a directory or file strictly within `d`
/// under the folded reading (over-listing is cost, never a hole), a masked
/// directory likewise, or any name veto where vetoes `walks` `d`.
fn listed(table: &ExecRules, masks: &[RealPath], d: &RealPath, walks: bool) -> bool {
    let masked = masks.iter().any(|m| d.contains_folded(m) && !m.contains(d));
    masked
        || table.denies().any(|scope| match scope {
            ExecDeny::Dir(p) | ExecDeny::File(p) => {
                let holds = std::iter::once(p.real.named()).chain(p.real.target());
                holds
                    .map(Trace::real)
                    .any(|p| d.contains_folded(p) && !p.contains_folded(d))
            }
            ExecDeny::Name(_) => walks,
        })
}

/// One expansion in progress.  Every handle is opened relative to its
/// parent's, never through a symlink, handed to the sink, and closed.
struct Walk<'w> {
    table: &'w ExecRules,
    walks: &'w dyn Fn(&RealPath) -> bool,
    masks: &'w [RealPath],
    sink: &'w mut dyn FnMut(Admit<'_>) -> io::Result<()>,
    holding: BTreeSet<ObjectId>,
    excluded: BTreeSet<ObjectId>,
    /// The identity of each directory being listed, so a bind mount looping
    /// back onto one ends the descent there.
    spine: Vec<ObjectId>,
}

impl Walk<'_> {
    /// Judge the position and inode; list every exclusion-holding directory.
    fn dir(&mut self, d: &Dir<Installing>) -> Result<(), Error> {
        let at = crate::path::Subject::Access(d.trace());
        let id = d.trace().last_id();
        let masked = self.masks.iter().any(|m| m.contains(d.real()));
        if masked || self.excluded.contains(&id) || !self.table.evidence(Subject::Dir(at)).permits()
        {
            return Ok(());
        }
        if !self.holding.contains(&id)
            && !listed(self.table, self.masks, d.real(), (self.walks)(d.real()))
        {
            return self.admit(Admit {
                path: d.real(),
                fd: d.as_fd(),
            });
        }
        if self.spine.contains(&id) {
            return Ok(());
        }
        self.spine.push(id);
        self.list(d)?;
        self.spine.pop();
        Ok(())
    }

    /// Admit a regular file only where its position and inode permit it.
    fn file(&mut self, f: &Located<Installing>) -> Result<(), Error> {
        let at = crate::path::Subject::Access(f.trace());
        if f.kind() != Some(Kind::File)
            || self.excluded.contains(&f.trace().last_id())
            || !self.table.evidence(Subject::Program(at)).permits()
        {
            return Ok(());
        }
        let fd = leaf_fd(f).map_err(|e| Error::admit(f.real(), e))?;
        self.admit(Admit { path: f.real(), fd })
    }

    /// `d`'s entries, each judged by the table.  Symlinks are nothing: a rule
    /// attaches to the inode one reaches, and the guard judges the real path.
    fn list(&mut self, d: &Dir<Installing>) -> Result<(), Error> {
        let Some(entries) = reached(d.read_dir(), d.real())? else {
            return Ok(());
        };
        for entry in entries {
            let Ok(seen) = entry.seen() else { continue };
            match seen.stat.kind {
                Kind::Dir => {
                    let child = d.open_dir(entry.name());
                    if let Some(child) = reached(child, &entry.real())? {
                        self.dir(&child)?;
                    }
                }
                Kind::File => {
                    let child = d.locate(entry.name(), LeafMode::NoFollow);
                    if let Some(child) = reached(child, &entry.real())? {
                        self.file(&child)?;
                    }
                }
                Kind::Symlink | Kind::Other => {}
            }
        }
        Ok(())
    }

    fn admit(&mut self, admit: Admit<'_>) -> Result<(), Error> {
        (self.sink)(admit).map_err(|source| Error::admit(admit.path, source))
    }
}

/// `None` where the walk cannot reach an entry: gone since it was listed,
/// not what it was listed as, or unreadable, which admits it nowhere and
/// fails closed.
fn reached<T>(opened: io::Result<T>, at: &RealPath) -> Result<Option<T>, Error> {
    use io::ErrorKind::{NotADirectory, NotFound, PermissionDenied};
    match opened {
        Ok(object) => Ok(Some(object)),
        Err(e) if matches!(e.kind(), NotFound | NotADirectory | PermissionDenied) => Ok(None),
        Err(e) => Err(Error::admit(at, e)),
    }
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
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
