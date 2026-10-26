//! Linux sandbox: the bubblewrap (`bwrap`) options that confine one external
//! child.  bwrap is never the payload: its monitor clones a process that
//! `setsid`s (`--new-session`) and then, as the pid namespace's init, forks
//! the payload or, with no namespace, becomes it; that pid is the group of
//! everything inside — the group ral addresses, read over `--info-fd` once
//! it exists ([`InfoFd`]); the monitor leads an inert group nobody signals.
//! The envelope is the child's whole world on every projection: own ipc and uts
//! namespaces, a cgroup namespace with `/sys/fs/cgroup` re-rooted on it and a
//! pid namespace with a fresh `/proc` where the host can build them
//! ([`HostEnvelope`]), the [`seccomp`] deny-set on x86-64 and aarch64.
//! bwrap has no endpoint filter — `--unshare-net` drops the network namespace
//! whole — so `SandboxProjection::net` is a bit, not a list.
//!
//! Every object the envelope shows is opened here, never through a symlink,
//! and mounted by that handle ([`Binds`]): bwrap resolves no source name, so
//! a name swapped for a symlink after the grant was rendered shows nothing
//! it did not name.  The GPU's nodes and sysfs are the exception, bound by
//! name as `/dev`'s own nodes are ([`gpu`]).
//!
//! The payload is always ral's own trampoline (`super::launch`), which enters
//! the [`landlock`] ruleset it inherits and only then becomes the target:
//! that order is forced, a Landlock domain handling any fs right forbidding
//! the `mount(2)` bwrap opens with.

mod gpu;
mod host;
pub(crate) mod landlock;
pub(crate) mod seccomp;

use gpu::GpuNodes;
use host::Builds;
pub(crate) use host::HostEnvelope;

use super::Refusal;
use super::launch::Ownership;
use super::reexec::Pinned;
use super::warrant::{Handoff, Slot, nul_terminated, parcel};
use super::{FsProjection, RenderedProjection};
use crate::Role;
use crate::authority::policy::Baseline;
use crate::path::{
    Dir, Installing, Kind, LeafMode, LexicalPath, Located, Object, RealPath, Rendered, Trace,
    is_loop, render_real, walk_lenient,
};
use crate::site::{Silent, site};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The bubblewrap binary's name: what [`pin_envelope`] walks `PATH` for,
/// and what a message calls it.  Never handed to `exec` — every launch goes
/// through the [`Pinned`] envelope.
pub(super) const BWRAP: &str = "bwrap";

static ENVELOPE: OnceLock<Pinned> = OnceLock::new();

/// Pin bwrap for the rest of the process's life, found on the `PATH` this
/// process was started with — never a shell's, none exists yet — and only in
/// its absolute entries.  Idempotent, and silent where bwrap is absent:
/// [`envelope`] says so at the first launch that needs it.
pub(super) fn pin_envelope() {
    if ENVELOPE.get().is_some() {
        return;
    }
    let host_path = crate::host::var_os("PATH").unwrap_or_default();
    let located = crate::path::locate_in(BWRAP, crate::path::path_dirs(&host_path, None));
    if let Some(pinned) = located.and_then(Pinned::open) {
        let _ = ENVELOPE.set(pinned);
    }
}

/// The pinned envelope, or why this host has none.
pub(super) fn envelope() -> Result<&'static Pinned, &'static str> {
    ENVELOPE.get().ok_or(
        "bwrap not found on PATH at startup: a grant confines every external command \
         under bubblewrap, so while one is active nothing can start without it. Is it \
         installed on this host, and on the PATH ral was started with?",
    )
}

/// bwrap's options as they are built: [`Command`]'s two appenders, and
/// nothing that could open a descriptor.
#[derive(Default)]
struct Options(Vec<OsString>);

impl Options {
    fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.0.push(arg.as_ref().to_owned());
        self
    }

    fn args(&mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> &mut Self {
        for arg in args {
            self.arg(arg);
        }
        self
    }
}

/// One launch's envelope: the pinned bwrap, the pinned trampoline it runs,
/// and what this host lets it build.
#[derive(Clone, Copy)]
pub(crate) struct Envelope<'a> {
    pub(crate) bwrap: &'a Pinned,
    pub(crate) trampoline: &'a Pinned,
    pub(crate) host: HostEnvelope,
}

impl<'a> Envelope<'a> {
    /// `bwrap` running `trampoline`, on this host as probed.
    pub(crate) fn probe(bwrap: &'a Pinned, trampoline: &'a Pinned) -> Self {
        let host = HostEnvelope::probe(bwrap, trampoline);
        Self {
            bwrap,
            trampoline,
            host,
        }
    }
}

/// The [`Command`] that runs `env`'s trampoline confined under `policy`:
/// `bwrap --args <a> -- /proc/self/fd/<t> --warrant <w>`, every descriptor
/// lent to `handoff` and named by its slot: the options in a sealed parcel,
/// ral's pin, the seccomp programs, every bind's handle and the `--info-fd`
/// peer, beside the caller's (the warrant, at `warrant`, and the Landlock
/// ruleset).  bwrap takes options alone from `--args`, so the payload rides
/// its argv, and runs the pinned inode whatever now holds its name.
/// `shown` is the bind plan the Landlock ruleset was built against.
///
/// The second element of the return is `Kept`'s `--info-fd` peer — see
/// [`InfoFd`].  Refused under a bwrap that builds no envelope or cannot mount
/// by descriptor.
pub(crate) fn bwrap_command(
    env: &Envelope<'_>,
    shown: Shown,
    mut handoff: Handoff,
    warrant: Slot,
    policy: &RenderedProjection,
    chdir: Option<&Path>,
    ownership: Ownership,
) -> Result<(Command, Option<InfoFd>), Refusal> {
    let bwrap = crate::path::shown(env.bwrap.arg0());
    let unavailable = |why: String| Err(Refusal::Unavailable(why));
    match env.host.builds {
        Builds::ByFd => {}
        Builds::ByName => {
            return unavailable(format!(
                "bwrap at {bwrap} does not take --ro-bind-fd (bubblewrap 0.8.0 or newer); ral \
                 mounts only by descriptor, so no confined command can run under it"
            ));
        }
        Builds::Nothing(why) => {
            return unavailable(format!(
                "bwrap at {bwrap} cannot build an envelope on this host: {why}"
            ));
        }
    }
    let binds = Binds::open(env, shown, &mut handoff)?;
    let (info, address) = (ownership == Ownership::Kept)
        .then(|| InfoFd::open(&mut handoff))
        .transpose()?
        .unzip();
    let mut lend_parcel = |name: &str, bytes: &[u8]| {
        let parcel =
            parcel(name, bytes).map_err(|e| format!("sandbox: cannot parcel {name}: {e}"))?;
        handoff.lend(parcel.as_fd())
    };
    let seccomp = (seccomp_programs()?.into_iter())
        .map(|program| lend_parcel("ral-seccomp", program))
        .collect::<Result<Vec<_>, _>>()?;
    let options = bwrap_argv(
        policy,
        chdir,
        address,
        env.host,
        &binds,
        &GpuNodes::discover(),
        &seccomp,
    )?;
    let args = lend_parcel(
        "ral-bwrap-args",
        &nul_terminated(options.iter().map(|arg| arg.as_bytes()))?,
    )?;
    let trampoline = handoff.lend(env.trampoline.fd())?;
    let mut c = env.bwrap.command();
    c.args(["--args", &args.to_string(), "--"])
        .arg(crate::path::proc_fd_path(trampoline.fd()))
        .arg(Role::Warrant.flag())
        .arg(warrant.to_string());
    handoff.install(&mut c);
    Ok((c, info))
}

/// The bwrap options that confine a payload under `policy`: `binds` by
/// slot, layer by layer, `deny` masked last at every name a bind
/// shows them; a deny hiding `/proc`, which bwrap execs the trampoline
/// through, is refused, as is one holding `chdir`, which no mask lets bwrap
/// enter.  Descriptors appear by slot number alone, one
/// `--add-seccomp-fd` per slot of `seccomp`; [`bwrap_command`] lends them.
///
/// Every name is taken from `policy`, rendered, so a rule lands on each
/// spelling the kernel might present rather than on the one the grant author
/// happened to write — a deny naming a symlink masks the target the resolved
/// twin names.  `policy.exec` reaches the options not at all: bwrap filters
/// mounts and syscalls, not exec by path, so [`landlock`] builds its ruleset
/// in the host, and the payload enters it *inside* this envelope.
///
/// `chdir` is the in-sandbox cwd — bwrap starts the child in its
/// mount-namespace root — so a per-command launch passes the target's
/// logical cwd; a launch with none to carry passes `None`.
///
/// `address`, the `--info-fd` slot of a launch the session keeps, carries
/// both ties between the session and the envelope: death
/// (`--die-with-parent`) and address (`--info-fd`).  A surrendered
/// (detached) launch has none, and carries neither: the survivor must not be
/// killed by our death, and there is no session left here to address once
/// we stop watching it.  Confinement itself — the mounts, the seccomp
/// filter — is otherwise identical.
///
/// The render is pure in `host`, `gpu` and in descriptors, so a test can assert
/// options for a host it is not running on.
pub(crate) fn bwrap_argv(
    policy: &RenderedProjection,
    chdir: Option<&Path>,
    address: Option<Slot>,
    host: HostEnvelope,
    binds: &Binds,
    gpu: &GpuNodes,
    seccomp: &[Slot],
) -> Result<Vec<OsString>, String> {
    let mut c = Options::default();
    c.arg("--new-session");
    c.args(["--unshare-ipc", "--unshare-uts"]);
    if host.private_pids {
        c.arg("--unshare-pid");
    }
    if host.private_cgroup {
        c.arg("--unshare-cgroup");
    }
    if let Some(info) = address {
        c.arg("--die-with-parent");
        c.args(["--info-fd", &info.to_string()]);
    }
    if !policy.net {
        c.arg("--unshare-net");
    }
    if let Some(dir) = chdir {
        c.arg("--chdir").arg(dir);
    }
    match &policy.fs {
        FsProjection::Restricted(_) => {
            // Before the prefix binds: a write prefix under `/tmp` lands inside it.
            c.args(["--tmpfs", "/tmp"]);
            // `--chdir` needs a directory to enter: a cwd outside the grant gets
            // an empty unwritable one, as macOS leaves it unreadable.  Before the
            // binds, which may hide it; `/` and `/tmp` are the envelope's own.
            if let Some(dir) =
                chdir.filter(|dir| !matches!(dir.as_os_str().as_bytes(), b"/" | b"/tmp"))
            {
                c.args(["--perms", "0555", "--tmpfs"]).arg(dir);
            }
            binds.render(&mut c, Layer::Shown);
            // After it, or a prefix of `/` or `/proc` re-binds the host's over these.
            c.args(["--proc", "/proc"]);
            render_dev(&mut c, host);
            if policy.gpu {
                gpu.admit(&mut c);
            }
        }
        FsProjection::Unrestricted => {
            // `--bind` would skip device nodes.  `/proc` goes over the root:
            // a fresh table under `--unshare-pid`, the host's bound without,
            // so `/proc/self` is the payload's own either way.
            c.args(["--dev-bind", "/", "/"]);
            c.args(["--proc", "/proc"]);
            if !policy.gpu {
                gpu.withhold(&mut c);
            }
        }
    }
    binds.render(&mut c, Layer::Over);
    // Masks go on after every bind: last mount wins.  `pinned_dirs` goes
    // unused because a mask is anchored to the inode, so a renamed ancestor
    // carries it — the pin macOS renders explicitly, Linux gets for free.
    if let Some(rules) = policy.fs.rules() {
        // By stored name, where a mask lands, not folded as an exclusion is;
        // a cwd that resolves nowhere is left to bwrap's `--chdir`.
        let cwd =
            chdir.and_then(|dir| Some((dir, RealPath::of(&LexicalPath::absolute(dir)?).ok()?)));
        let mut denied: Vec<_> = rules.deny.iter().collect();
        denied.sort();
        for path in denied {
            if let Some((dir, at)) = &cwd
                && path.holds_position(at)
            {
                return Err(format!(
                    "sandbox: the working directory {} is excluded by {}, and no confined \
                     command can start inside an exclusion; run it from another directory \
                     (`cd` or `within [dir: …]`)",
                    crate::path::shown(dir),
                    path.as_str()
                ));
            }
            let mask = DenyMask::over(path);
            for name in binds.names(path) {
                if name.holds("/proc") {
                    return Err(format!(
                        "sandbox: the grant denies {}, which hides /proc, and the envelope \
                         needs /proc to start its program; did you mean a path inside it?",
                        name.as_str()
                    ));
                }
                mask.render(&mut c, name.as_str());
            }
        }
    }
    for program in seccomp {
        c.args(["--add-seccomp-fd", &program.to_string()]);
    }
    Ok(c.0)
}

/// Whether a bind lets the envelope write through to the host.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Access {
    ReadOnly,
    Writable,
}

/// Where a bind goes in bwrap's order.  `Shown`: the projection's view,
/// before the envelope's own `/proc` and `/dev`.  `Over`: over everything,
/// the cgroup tree and the pinned binaries.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Layer {
    Shown,
    Over,
}

/// What the envelope shows at one name, by a handle on the object.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Bind {
    layer: Layer,
    access: Access,
    dest: Rendered,
    /// The object's real name, and its position, held when the binds open.
    object: Rendered,
    position: RealPath,
}

impl Bind {
    fn read_only(layer: Layer, dest: Rendered, (position, object): (RealPath, Rendered)) -> Self {
        Self {
            layer,
            access: Access::ReadOnly,
            dest,
            object,
            position,
        }
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
            return self.access == Access::Writable && inner.access == Access::ReadOnly;
        }
        self.dest.holds(inner.dest.as_str())
            && (inner.access == Access::ReadOnly
                || self.spells(inner)
                || self.access == Access::Writable)
    }

    /// Where this bind shows `name`, if its object holds it.
    fn shows(&self, name: &Rendered) -> Option<Rendered> {
        name.rebased(&self.object, &self.dest)
    }
}

/// The binds, each with the slot its handle was lent at.
pub(crate) struct Binds(Vec<(Slot, Bind)>);

impl Binds {
    /// Plan, open and lend: the one place a launch opens what it mounts, the two
    /// pins aside, which are open already.  Each object is opened once, and
    /// each bind lends its own copy, bwrap closing a descriptor after the
    /// one mount it serves.
    fn open(env: &Envelope<'_>, shown: Shown, handoff: &mut Handoff) -> Result<Self, String> {
        let Shown {
            binds: mut planned,
            tolerated,
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
        let mut binds = Vec::new();
        for bind in planned {
            // Absent: a name that shows nothing binds nothing.
            if let Some(Some(held)) = opened.get(&bind.position) {
                let source = leaf_fd(held).map_err(|e| {
                    format!(
                        "sandbox: cannot copy the handle on {}: {e}",
                        bind.object.as_str()
                    )
                })?;
                binds.push((handoff.lend(source)?, bind));
            }
        }
        for (bind, pin) in pinned([env.bwrap, env.trampoline])? {
            binds.push((handoff.lend(pin)?, bind));
        }
        Ok(Self(binds))
    }

    fn layer(&self, layer: Layer) -> impl Iterator<Item = &(Slot, Bind)> {
        self.0.iter().filter(move |(_, bind)| bind.layer == layer)
    }

    /// Every name the envelope shows `name` at: itself, and wherever a
    /// `Shown` bind of an object holding it puts it.  A mount laid at one
    /// does not show at another.
    fn names(&self, name: &Rendered) -> Vec<Rendered> {
        let mut names: Vec<_> = (self.layer(Layer::Shown))
            .filter_map(|(_, bind)| bind.shows(name))
            .chain([name.clone()])
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn render(&self, c: &mut Options, layer: Layer) {
        for (slot, bind) in self.layer(layer) {
            let op = match bind.access {
                Access::ReadOnly => "--ro-bind-fd",
                Access::Writable => "--bind-fd",
            };
            c.args([op, &slot.fd().to_string(), bind.dest.as_str()]);
        }
    }
}

/// What the envelope shows, planned before anything is opened, so that the
/// Landlock ruleset is built against it.
pub(crate) struct Shown {
    binds: Vec<Bind>,
    /// The baseline's objects, which a host may keep from this user.
    tolerated: BTreeSet<RealPath>,
    /// The deduped objects the view is rooted at: every shown object under a
    /// restricted fs, `/` under an unrestricted one.
    pub(crate) roots: Vec<RealPath>,
}

impl Shown {
    /// What a restricted projection shows: every read scope of the fs
    /// projection, places among them, and every write prefix, each frozen
    /// object at its real name and at the name it was granted by, less what
    /// another bind already shows; read-only before writable, each sorted by
    /// name, so a parent precedes its children and a writable prefix inside
    /// a read-only one wins.
    pub(crate) fn of(policy: &RenderedProjection, baseline: &Baseline) -> Result<Self, String> {
        let FsProjection::Restricted(rules) = &policy.fs else {
            let root = open_dir_nofollow(&RealPath::universal())?.ok_or("sandbox: no root")?;
            return Ok(Self {
                binds: Vec::new(),
                tolerated: BTreeSet::new(),
                roots: vec![root.real().clone()],
            });
        };
        let tolerated = (rules.read.iter())
            .map(|object| &object.position)
            .filter(|position| baseline.allows().any(|f| f.real() == Some(*position)))
            .cloned()
            .collect();
        let mut names: Vec<_> = binds(&rules.read, Access::ReadOnly)
            .chain(binds(&rules.write, Access::Writable))
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
        Ok(Self {
            binds,
            tolerated,
            roots,
        })
    }
}

/// `objects`, each by its real name and by every name it is shown at.
fn binds(objects: &[Object], access: Access) -> impl Iterator<Item = Bind> {
    objects.iter().flat_map(move |object| {
        (object.names()).map(move |dest| Bind {
            layer: Layer::Shown,
            access,
            dest: dest.clone(),
            object: object.real.clone(),
            position: object.position.clone(),
        })
    })
}

/// `name` as the one object it reaches, its position and its real name;
/// `None` where it names nothing.
fn object(name: &(impl AsRef<Path> + ?Sized)) -> Result<Option<(RealPath, Rendered)>, String> {
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
fn open_object(position: &RealPath, optional: bool) -> Result<Option<Located<Installing>>, String> {
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
fn open_nofollow(real: &RealPath) -> Result<Option<Located<Installing>>, OpenError> {
    real.open_nofollow(&super::TOKEN)
        .map_err(|source| OpenError::of(real, source))
}

/// The descriptor `held` lends, which every position held on Linux has.
fn leaf_fd(held: &Located<Installing>) -> io::Result<std::os::fd::BorrowedFd<'_>> {
    held.leaf_fd()
        .ok_or_else(|| io::Error::other("the object is not held"))
}

/// [`open_nofollow`] where `real` must be a directory: `None` also where it
/// is not one.
fn open_dir_nofollow(real: &RealPath) -> Result<Option<Dir<Installing>>, OpenError> {
    real.open_dir_nofollow(&super::TOKEN)
        .map_err(|source| OpenError::of(real, source))
}

/// Why [`open_nofollow`] refused.
#[derive(Debug)]
enum OpenError {
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
                "sandbox: {name} now goes through a symbolic link that ral will not follow; \
                 reload the grant or restart ral if you made that change"
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

/// The envelope's seccomp programs, each stacked by its own
/// `--add-seccomp-fd` (bwrap refuses that beside `--seccomp`): the kernel
/// applies every installed filter and keeps the most severe result, so which
/// program lands on which slot carries no meaning.  None off the two arches
/// the deny-set is compiled for.
pub(crate) fn seccomp_programs() -> Result<Vec<&'static [u8]>, String> {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        seccomp::Filter::ENVELOPE
            .programs()
            .map(|programs| programs.iter().collect())
            .map_err(|e| format!("sandbox: {e}"))
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        Ok(Vec::new())
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

/// The envelope's `/dev`: bwrap's `--dev`, or its shape by hand where the
/// host refuses a fresh devpts.
///
/// By hand, `/dev/pts` is the host's, so a pty opened inside is not the
/// envelope's own, and `/dev/ptmx` reaches it only where that devpts's
/// `ptmxmode` admits: the kernel finds a ptmx node's devpts at the `pts` beside
/// it in the same mount, which a bind of the host's node lacks.  An absent node
/// is dropped: binding one costs the launch.
fn render_dev(c: &mut Options, host: HostEnvelope) {
    if host.virtual_dev {
        c.args(["--dev", "/dev"]);
        return;
    }
    c.args(["--tmpfs", "/dev"]);
    for node in [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
        "/dev/pts",
    ] {
        if crate::path::exists(node) {
            c.args(["--dev-bind", node, node]);
        }
    }
    c.args(["--symlink", "pts/ptmx", "/dev/ptmx"]);
    c.args(["--symlink", "/proc/self/fd", "/dev/fd"]);
    for (fd, name) in ["/dev/stdin", "/dev/stdout", "/dev/stderr"]
        .into_iter()
        .enumerate()
    {
        c.args(["--symlink", &format!("/proc/self/fd/{fd}"), name]);
    }
    // After `/dev`'s tmpfs, or it is buried.
    c.args(["--tmpfs", "/dev/shm"]);
}

/// The envelope's cgroup tree, a read-only bind over every other mount.
const CGROUP: &str = "/sys/fs/cgroup";

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
fn own_cgroup() -> Option<String> {
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

/// Both ends of a `Kept` launch's `--info-fd` socketpair — a socket, which
/// unlike a pipe no same-uid process can reopen through `/proc/<pid>/fd` to
/// forge a `child-pid`.  The writer crosses twice, bwrap's copy and the
/// payload's, both kept here, never lent: a lent copy lives as long as the
/// [`Command`], and a read reaches EOF only once every copy is closed.
pub(super) struct InfoFd {
    reader: UnixStream,
    writers: [OwnedFd; 2],
}

impl InfoFd {
    /// A socketpair whose writer crosses twice: bwrap's copy, at the slot
    /// returned for its `--info-fd`, and the payload's, which bwrap leaves it
    /// until its sweep.
    fn open(handoff: &mut Handoff) -> Result<(Self, Slot), String> {
        let (reader, writer) = crate::process::cloexec_socketpair()
            .map_err(|e| format!("sandbox: cannot open bwrap's --info-fd: {e}"))?;
        let (bwraps, at) = handoff.keep(writer.as_fd())?;
        let (payloads, _) = handoff.keep(writer.as_fd())?;
        Ok((
            Self {
                reader,
                writers: [bwraps, payloads],
            },
            at,
        ))
    }

    /// The payload's process group, bwrap's `child-pid`, read to EOF.  bwrap
    /// writes it before that process's `setsid`, naming no group yet; EOF
    /// waits for the payload's sweep of its copy of the writer, by which the
    /// group exists, or for every holder's death.  `None` if bwrap died first.
    pub(super) fn payload_pgid(self) -> Option<crate::process::Pgid> {
        #[derive(serde::Deserialize)]
        struct Info {
            #[serde(rename = "child-pid")]
            child_pid: i32,
        }
        let Self { reader, writers } = self;
        drop(writers);
        let info: Info = serde_json::from_reader(reader).ok()?;
        crate::process::Pgid::from_raw(info.child_pid)
    }
}

/// The mount that masks one denied path, bwrap having no negative path rule.
/// The target's shape forces which one, and getting it wrong costs the launch
/// rather than the deny — `--tmpfs` over a regular file dies in `mkdir` before
/// the body execs — so [`Self::over`] reads it off the denied path, and the
/// mask goes on at each name that shows it.
///
/// Every mask goes over a name that already exists: a mount bwrap must first
/// `mkdir` is not available to us, for the reason [`Self::LeftAbsent`] gives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DenyMask {
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
    /// The mask `path` needs, from what stands there.
    fn over(path: &Rendered) -> Self {
        Self::of(standing(path).and_then(|stood| stood.kind()))
    }

    /// The mask an object of `kind` needs; a name that stands nothing, none.
    fn of(kind: Option<Kind>) -> Self {
        match kind {
            Some(Kind::Symlink) => Self::OnItsTarget,
            Some(Kind::File | Kind::Other) => Self::UnopenableNode,
            Some(Kind::Dir) => Self::EmptyDir,
            None => Self::LeftAbsent,
        }
    }

    fn render(self, c: &mut Options, at: &str) {
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
    let name = LexicalPath::absolute(&PathBuf::from(name.as_str()))?;
    walk_lenient(&name, LeafMode::NoFollow).ok()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
