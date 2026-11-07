//! What the envelope shows, planned before anything is opened: the
//! [`Shown`] view, its [`Bind`]s opened and lent as [`Binds`], and the
//! [`Deny`] masks over them.  [`super::bwrap_argv`] renders the plan; the
//! [`landlock`](super::landlock) ruleset is built against it.

use super::super::reexec::Pinned;
use super::super::warrant::{Handoff, Slot};
use super::super::{FsProjection, RenderedProjection, TOKEN};
use super::host::HostEnvelope;
use super::{Envelope, Options};
use crate::authority::policy::FsOp;
use crate::path::{
    Dir, Installing, Kind, LeafMode, LexicalPath, Located, Object, RealPath, Rendered, Trace,
    is_loop, render_real, walk_lenient,
};
use crate::site::{Silent, site};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::os::fd::BorrowedFd;
use std::path::Path;

/// Where a bind goes in bwrap's order.  `Shown`: the projection's view,
/// before the envelope's own `/proc` and `/dev`.  `Spine`: each pinned
/// directory, writable onto its own name, which as a mountpoint no rename or
/// removal moves; it shows every name beneath it at itself.  `Over`: over
/// everything, the cgroup tree and the pinned binaries.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Layer {
    Shown,
    Spine,
    Over,
}

/// What the envelope shows at one name, by a handle on the object.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Bind {
    pub(super) layer: Layer,
    /// `Write` lets the envelope write through to the host.
    pub(super) op: FsOp,
    pub(super) dest: Rendered,
    /// The object's real name, and its position, held when the binds open.
    pub(super) object: Rendered,
    pub(super) position: RealPath,
}

impl Bind {
    pub(super) fn read_only(
        layer: Layer,
        dest: Rendered,
        (position, object): (RealPath, Rendered),
    ) -> Self {
        Self {
            layer,
            op: FsOp::Read,
            dest,
            object,
            position,
        }
    }

    /// The pinned directory `name` onto itself; `None` where nothing stands
    /// there.  A name that now reaches another object refuses the launch:
    /// bound, it would show that object at `name`.
    fn pin(name: &Rendered) -> Result<Option<Self>, String> {
        let Some((position, real)) = object(name.as_str())? else {
            return Ok(None);
        };
        if real != *name {
            return Err(format!(
                "sandbox: the directory '{}' is pinned for this launch, but it now reaches '{}' \
                 through a symbolic link, which would be shown at its name; retry once nothing \
                 is relinking it",
                crate::path::shown(name.as_str()),
                crate::path::shown(real.as_str())
            ));
        }
        Ok(Some(Self {
            layer: Layer::Spine,
            op: FsOp::Write,
            dest: real.clone(),
            object: real,
            position,
        }))
    }

    /// Whether `inner`'s name is the host's own symlink beneath `self`: the
    /// mount shows `inner`'s object elsewhere.
    fn spells(&self, inner: &Self) -> bool {
        self.shows(&inner.object).as_ref() != Some(&inner.dest)
    }

    /// Whether `self` already shows `inner`.  A read-only bind laid inside a
    /// writable one would deny writes the grant admits, so only a writable
    /// bind covers a writable object; the host's own symlink within any bind
    /// is covered too, as bwrap will not mount onto one.
    fn covers(&self, inner: &Self) -> bool {
        if self.dest == inner.dest {
            return self.op == FsOp::Write && inner.op == FsOp::Read;
        }
        self.shadows(&inner.dest)
            && (inner.op == FsOp::Read || self.spells(inner) || self.op == FsOp::Write)
    }

    /// Whether this bind's mount hides whatever lay at `name`.
    fn shadows(&self, name: &Rendered) -> bool {
        self.dest.holds(name.as_str())
    }

    /// Where this bind shows `name`, if its object holds it.
    fn shows(&self, name: &Rendered) -> Option<Rendered> {
        name.rebased(&self.object, &self.dest)
    }
}

/// The binds, each with the slot its handle was lent at, and the denies
/// [`Shown`] planned over them.
pub(crate) struct Binds {
    pub(super) lent: Vec<(Slot, Bind)>,
    pub(super) denies: Vec<Deny>,
}

impl Binds {
    /// Plan, open and lend: the one place a launch opens what it mounts, the
    /// two pinned binaries aside, which are open already.  Each object is
    /// opened once, and each bind lends its own copy, bwrap closing a
    /// descriptor after the one mount it serves.
    pub(super) fn open(
        env: &Envelope<'_>,
        shown: Shown,
        handoff: &mut Handoff,
    ) -> Result<Self, String> {
        let Shown {
            binds: mut planned,
            tolerated,
            denies,
            ..
        } = shown;
        if let Some(tree) = cgroup_tree(env.host)
            && let (Some(object), Some((_, dest))) = (object(&tree)?, object(CGROUP)?)
        {
            planned.push(Bind::read_only(Layer::Over, dest, object));
        }
        let mut opened = BTreeMap::new();
        for bind in &planned {
            if !opened.contains_key(&bind.position) {
                let held = open_object(&bind.position, tolerated.contains(&bind.position))?;
                opened.insert(bind.position.clone(), held);
            }
        }
        let mut lent = Vec::new();
        for bind in planned {
            // Absent: a name that shows nothing binds nothing.
            if let Some(Some(held)) = opened.get(&bind.position) {
                let source = leaf_fd(held).map_err(|e| {
                    format!(
                        "sandbox: cannot copy the handle on {}: {e}",
                        bind.object.as_str()
                    )
                })?;
                lent.push((handoff.lend(source)?, bind));
            }
        }
        for (bind, pin) in pinned([env.bwrap, env.trampoline])? {
            lent.push((handoff.lend(pin)?, bind));
        }
        Ok(Self { lent, denies })
    }

    pub(super) fn layer(&self, layer: Layer) -> impl Iterator<Item = &(Slot, Bind)> {
        self.lent
            .iter()
            .filter(move |(_, bind)| bind.layer == layer)
    }

    /// Names still visible after the overlaid binds replace their destinations.
    pub(super) fn names(&self, name: &Rendered) -> Vec<Rendered> {
        let mut names: Vec<_> = (self.layer(Layer::Shown))
            .filter_map(|(_, bind)| bind.shows(name))
            .chain([name.clone()])
            .filter(|name| !self.layer(Layer::Over).any(|(_, bind)| bind.shadows(name)))
            .chain(
                self.layer(Layer::Over)
                    .filter_map(|(_, bind)| bind.shows(name)),
            )
            .collect();
        names.sort();
        names.dedup();
        names
    }

    pub(super) fn render(&self, c: &mut Options, layer: Layer) {
        for (slot, bind) in self.layer(layer) {
            let op = match bind.op {
                FsOp::Read => "--ro-bind-fd",
                FsOp::Write => "--bind-fd",
            };
            c.args([op, &slot.fd().to_string(), bind.dest.as_str()]);
        }
    }
}

/// What the envelope shows, planned before anything is opened, so that the
/// Landlock ruleset is built against it.
pub(crate) struct Shown {
    pub(super) binds: Vec<Bind>,
    /// The baseline's objects, which a host may keep from this user.
    pub(super) tolerated: BTreeSet<RealPath>,
    /// The deduped objects the view is rooted at: every shown object under a
    /// restricted fs, `/` under an unrestricted one.
    pub(super) roots: Vec<RealPath>,
    /// Sorted by name; the one answer bwrap and Landlock both read.
    pub(super) denies: Vec<Deny>,
}

impl Shown {
    /// What a restricted projection shows: every read scope of the fs
    /// projection, places among them, and every write prefix, each frozen
    /// object at its real name and at the name it was granted by, less what
    /// another bind already shows; read-only before writable, each sorted by
    /// name, so a parent precedes its children and a writable prefix inside
    /// a read-only one wins.  Then each pinned directory onto itself, which
    /// shows nothing new and so roots nothing.  Each deny is read once, for
    /// its mask.
    pub(crate) fn of(policy: &RenderedProjection) -> Result<Self, String> {
        let FsProjection::Restricted(lists) = &policy.fs else {
            return Ok(Self {
                binds: Vec::new(),
                tolerated: BTreeSet::new(),
                roots: vec![RealPath::universal()],
                denies: Vec::new(),
            });
        };
        let mut names: Vec<_> = binds(&lists.read, FsOp::Read)
            .chain(binds(&lists.write, FsOp::Write))
            .collect();
        names.sort();
        names.dedup();
        let binds: Vec<_> = (names.iter())
            .filter(|inner| !names.iter().any(|outer| outer.covers(inner)))
            .cloned()
            .collect();
        let roots = (binds.iter().map(|bind| &bind.position))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .cloned()
            .collect();
        // Sorted, so a parent is pinned before its children, whose pins it
        // would bury.  No shown bind lies beneath one to bury: each pinned
        // directory lies within a writable bind, which covers every bind in it.
        let spine = (lists.pinned_dirs.iter())
            .filter_map(|name| Bind::pin(name).transpose())
            .collect::<Result<Vec<_>, _>>()?;
        let mut denied: Vec<_> = lists.deny.iter().collect();
        denied.sort();
        Ok(Self {
            binds: binds.into_iter().chain(spine).collect(),
            tolerated: lists.tolerated.clone(),
            roots,
            denies: denied.into_iter().map(Deny::read).collect(),
        })
    }

    /// The positions masked by a scratch tmpfs, which the payload may `chmod`
    /// open: each deny that stood as a directory.
    pub(super) fn scratch(&self) -> Vec<RealPath> {
        (self.denies.iter())
            .filter(|deny| deny.mask == DenyMask::EmptyDir)
            .filter_map(|deny| deny.position.clone())
            .collect()
    }
}

/// A deny as planned: the mask its name takes, and where the name led.
pub(super) struct Deny {
    pub(super) name: Rendered,
    pub(super) mask: DenyMask,
    /// `None` where the name could not be walked.
    pub(super) position: Option<RealPath>,
}

impl Deny {
    /// `name`, from one read of what stands there now.
    fn read(name: &Rendered) -> Self {
        let stood = standing(name);
        Self {
            name: name.clone(),
            mask: DenyMask::of(stood.as_ref().and_then(Trace::kind)),
            position: stood.map(|stood| stood.real().clone()),
        }
    }
}

/// `objects`, each by its real name and by every name it is shown at.
fn binds(objects: &[Object], op: FsOp) -> impl Iterator<Item = Bind> {
    objects.iter().flat_map(move |object| {
        (object.names()).map(move |dest| Bind {
            layer: Layer::Shown,
            op,
            dest: dest.clone(),
            object: object.real.clone(),
            position: object.position.clone(),
        })
    })
}

/// `name` as the one object it reaches, its position and its real name;
/// `None` where it names nothing.
pub(super) fn object(
    name: &(impl AsRef<Path> + ?Sized),
) -> Result<Option<(RealPath, Rendered)>, String> {
    let name = name.as_ref();
    let Some(spelling) = LexicalPath::absolute(name) else {
        return Ok(None);
    };
    match RealPath::of(&spelling) {
        Ok(real) => {
            let rendered = render_real(&real)?.into_iter().next();
            Ok(rendered.map(|name| (real, name)))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "sandbox: cannot resolve {}: {e}",
            crate::path::shown(name)
        )),
    }
}

/// The object at `position`, held in the host to be mounted; `None` where
/// nothing is there, or it is `optional` and kept from this user.
pub(super) fn open_object(
    position: &RealPath,
    optional: bool,
) -> Result<Option<Located<Installing>>, String> {
    match open_nofollow(position) {
        Err(OpenError::Failed { source, .. })
            if optional && source.kind() == io::ErrorKind::PermissionDenied =>
        {
            Ok(None)
        }
        opened => Ok(opened?),
    }
}

/// `real`, held in the host never through a symlink, so a mount or a
/// Landlock rule attaches to the inode the grant rendered; `None` where it
/// names nothing.
pub(super) fn open_nofollow(real: &RealPath) -> Result<Option<Located<Installing>>, OpenError> {
    real.open_nofollow(&TOKEN)
        .map_err(|source| OpenError::of(real, source))
}

/// The descriptor `held` lends, which every position held on Linux has.
pub(super) fn leaf_fd(held: &Located<Installing>) -> io::Result<std::os::fd::BorrowedFd<'_>> {
    held.leaf_fd()
        .ok_or_else(|| io::Error::other("the object is not held"))
}

/// [`open_nofollow`] where `real` must be a directory: `None` also where it
/// is not one.
pub(super) fn open_dir_nofollow(real: &RealPath) -> Result<Option<Dir<Installing>>, OpenError> {
    real.open_dir_nofollow(&TOKEN)
        .map_err(|source| OpenError::of(real, source))
}

/// Why [`open_nofollow`] refused.
#[derive(Debug)]
pub(super) enum OpenError {
    /// A component is a symlink now, which the grant did not render.
    Linked(String),
    Failed {
        name: String,
        source: io::Error,
    },
}

impl OpenError {
    fn of(real: &RealPath, source: io::Error) -> Self {
        let name = crate::path::shown(real.as_path());
        if is_loop(&source) {
            Self::Linked(name)
        } else {
            Self::Failed { name, source }
        }
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Linked(name) => write!(
                f,
                "sandbox: '{name}' was granted by its real path, but it now goes through a \
                 symbolic link; {}",
                crate::authority::remedy::grant_the_new_target()
            ),
            Self::Failed { name, source } => write!(f, "sandbox: cannot open {name}: {source}"),
        }
    }
}

impl From<OpenError> for String {
    fn from(e: OpenError) -> Self {
        e.to_string()
    }
}

/// The files a launch execs, the envelope and the trampoline, each by its own
/// descriptor at its current name, to go read-only over whatever the
/// projection bound: no confined child rewrites a pinned inode in place, nor
/// moves it, a mountpoint being unrenamable.  An unlinked pin has no name to
/// lock, and runs by descriptor all the same.
fn pinned<'a>(pins: [&'a Pinned; 2]) -> Result<Vec<(Bind, BorrowedFd<'a>)>, String> {
    let mut locked: Vec<(Bind, BorrowedFd<'a>)> = Vec::new();
    for pin in pins {
        let what = crate::path::shown(pin.arg0());
        let names = pin
            .names()
            .map_err(|e| format!("sandbox: cannot find where the pinned {what} lives now: {e}"))?;
        let Some(name) = names else { continue };
        let Some((position, dest)) = object(&name)? else {
            continue;
        };
        if locked.iter().all(|(bind, _)| bind.dest != dest) {
            locked.push((
                Bind::read_only(Layer::Over, dest.clone(), (position, dest)),
                pin.fd(),
            ));
        }
    }
    Ok(locked)
}

/// The envelope's cgroup tree, a read-only bind over every other mount.
pub(super) const CGROUP: &str = "/sys/fs/cgroup";

/// What [`CGROUP`] shows: the tree the payload's own `/proc/self/cgroup`
/// names.  Under a cgroup namespace that file reads `0::/`, and a runtime
/// joining it onto the host's tree reads the root's limits — none — so the
/// bind re-roots the tree on ral's own cgroup, the namespace's root: what a
/// fresh cgroup2 mount inside it would show, built from the op bwrap has.
/// Without the namespace the host's tree is the true one.  Over the
/// projection's binds, so a grant reading `/sys` wholesale still gets the
/// tree its `/proc` describes.
fn cgroup_tree(host: HostEnvelope) -> Option<String> {
    if host.private_cgroup {
        own_cgroup().map(|own| format!("{CGROUP}{own}"))
    } else {
        Some(CGROUP.to_string())
    }
}

/// ral's cgroup, `/`-rooted and without its trailing slash, from the one
/// `0::` line cgroup2 writes to `/proc/self/cgroup`; `None` on a v1 or
/// hybrid host, whose lines are many and name no single tree.
pub(super) fn own_cgroup() -> Option<String> {
    let table = site!(
        Silent::OwnCgroup,
        std::fs::read_to_string("/proc/self/cgroup")
    )
    .ok()?;
    let mut lines = table.lines();
    let own = lines.next()?.strip_prefix("0::")?;
    lines
        .next()
        .is_none()
        .then(|| own.trim_end_matches('/').to_string())
}

/// The mount that masks one denied path, bwrap having no negative path rule.
/// The target's shape forces which one, and getting it wrong costs the launch
/// rather than the deny — `--tmpfs` over a regular file dies in `mkdir` before
/// the body execs — so [`Deny::read`] reads it off the denied path, and the
/// mask goes on at each name that shows it.
///
/// Every mask goes over a name that already exists: a mount bwrap must first
/// `mkdir` is not available to us, for the reason [`Self::LeftAbsent`] gives.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DenyMask {
    /// An empty directory with no permission bits, which refuse the owner as
    /// much as anyone.  The bits are the whole of it: the tmpfs is the
    /// sandboxed uid's own, so a child that deliberately `chmod`s them back
    /// has scratch memory at that name — never the denied directory, and
    /// never the host.  An immutable mode needs a read-only mount, and bwrap
    /// gives that only for a bind, from a source this process would hold.
    EmptyDir,
    /// A device node bound without `MS_DEV`: unopenable, `EACCES` either way.
    UnopenableNode,
    /// Nothing — no mount lands on a symlink; the resolved twin holds it.
    OnItsTarget,
    /// Nothing — every mask bwrap could lay on a name that does not exist it
    /// must `mkdir` a mountpoint for first: `EROFS` and a dead envelope under
    /// a read-only bind, and under a writable one a mkdir on the host, the
    /// deny creating the very name it forbids (`deny: ['cwd:/.env']` leaving
    /// an `.env` directory in the user's tree).  So the name is left alone: a
    /// read-only bind refuses the only access an absent name has, and under a
    /// writable one the in-process guard stays its single enforcer.
    LeftAbsent,
}

impl DenyMask {
    /// The mask an object of `kind` needs; a name that stands nothing, none.
    pub(super) fn of(kind: Option<Kind>) -> Self {
        match kind {
            Some(Kind::Symlink) => Self::OnItsTarget,
            Some(Kind::File | Kind::Other) => Self::UnopenableNode,
            Some(Kind::Dir) => Self::EmptyDir,
            None => Self::LeftAbsent,
        }
    }

    pub(super) fn render(self, c: &mut Options, at: &str) {
        match self {
            Self::EmptyDir => {
                c.args(["--perms", "0000", "--tmpfs", at]);
            }
            Self::UnopenableNode => {
                c.args(["--ro-bind", "/dev/null", at]);
            }
            Self::OnItsTarget | Self::LeftAbsent => {}
        }
    }
}

/// What stands at the rendered `name` now, never followed; `None` where it
/// cannot be walked.
fn standing(name: &Rendered) -> Option<Trace> {
    let name = LexicalPath::absolute_str(name.as_str())?;
    walk_lenient(&name, LeafMode::NoFollow).ok()
}
