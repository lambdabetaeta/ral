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
//! it did not name.
//!
//! The payload is always ral's own trampoline (`super::launch`), which enters
//! the [`landlock`] ruleset it inherits and only then becomes the target:
//! that order is forced, a Landlock domain handling any fs right forbidding
//! the `mount(2)` bwrap opens with.

mod host;
pub(crate) mod landlock;
pub(crate) mod seccomp;

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
    let options = bwrap_argv(policy, chdir, address, env.host, &binds, &seccomp)?;
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
/// The render is pure in `host` and in descriptors, so a test can assert
/// options for a host it is not running on.
pub(crate) fn bwrap_argv(
    policy: &RenderedProjection,
    chdir: Option<&Path>,
    address: Option<Slot>,
    host: HostEnvelope,
    binds: &Binds,
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
        }
        FsProjection::Unrestricted => {
            // `--bind` would skip device nodes.  `/proc` goes over the root:
            // a fresh table under `--unshare-pid`, the host's bound without,
            // so `/proc/self` is the payload's own either way.
            c.args(["--dev-bind", "/", "/"]);
            c.args(["--proc", "/proc"]);
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
                mask.render(&mut c, &name);
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

    fn render(self, c: &mut Options, at: &Rendered) {
        let at = at.as_str();
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
mod tests {
    use super::{
        Access, Binds, Builds, CGROUP, Envelope, HostEnvelope, Layer, Pinned, Shown, bwrap_argv,
        bwrap_command,
    };
    use crate::Role;
    use crate::authority::policy::{Admitted, Baseline, Capabilities, FsAllow, FsPolicy, Program};
    use crate::authority::policy::{Evidence, ExecAllow, ExecDeny, GrantPath, Join, Rule};
    use crate::authority::sandbox::launch::{Ownership, admitted, enveloped};
    use crate::authority::sandbox::warrant::{Handoff, Slot};
    use crate::authority::sandbox::{ExecProjection, FsProjection, FsRules, SandboxProjection};
    use crate::path::{Excluded, FrozenPath, RealPath, Trace};
    use std::process::Stdio;

    /// The host's baseline, which a restricted read region carries.
    fn host_system_reads() -> Baseline {
        crate::test_helper::core_runtime().baseline().clone()
    }

    fn dir_rule(path: &RealPath, allow: bool) -> Rule {
        let written = FrozenPath::of_real(path);
        if allow {
            Rule::Allow(ExecAllow::Dir(
                GrantPath::<Trace>::walk(written).expect("a directory walks"),
            ))
        } else {
            Rule::Deny(Box::new(ExecDeny::Dir(
                GrantPath::<Excluded>::walk(written).expect("a directory walks"),
            )))
        }
    }

    fn file_rule(path: &RealPath, allow: bool) -> Rule {
        if allow {
            Rule::Allow(ExecAllow::support(path))
        } else {
            Rule::Deny(Box::new(ExecDeny::File(
                GrantPath::<Excluded>::walk(FrozenPath::of_real(path)).expect("a file walks"),
            )))
        }
    }

    fn veto_rule(name: &str) -> Rule {
        Rule::Deny(Box::new(ExecDeny::Name(crate::path::command_name_key(
            name,
        ))))
    }

    /// A bare Linux host.
    const WHOLE: HostEnvelope = HostEnvelope {
        private_pids: true,
        virtual_dev: true,
        private_cgroup: true,
        landlock: super::landlock::Landlock::At(super::landlock::Abi::SIGNAL_SCOPE),
        builds: Builds::ByFd,
    };

    fn workdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ral-bwrap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create work dir");
        dir
    }

    fn deny_within(dir: &std::path::Path, denied: &[&std::path::Path]) -> SandboxProjection {
        restricted(&[], &[dir], denied)
    }

    fn unrestricted() -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Unrestricted,
            net: true,
            exec: ExecProjection::default(),
        }
    }

    /// This test binary stands in for bwrap and the trampoline where only the
    /// options are read: the render tests need no bwrap on the host, and no
    /// payload shares its name.
    fn stand_in() -> Pinned {
        Pinned::open(std::env::current_exe().expect("own path")).expect("own binary pins")
    }

    /// Where the stand-in's pin is locked: its current name.
    fn pin_name() -> std::path::PathBuf {
        (stand_in().names())
            .expect("read the pin")
            .expect("the stand-in has a name")
    }

    /// `pin` as both bwrap and the trampoline, on `host`.
    fn on(pin: &Pinned, host: HostEnvelope) -> Envelope<'_> {
        Envelope {
            bwrap: pin,
            trampoline: pin,
            host,
        }
    }

    fn prefix(path: &str) -> FsAllow {
        FsAllow::Prefix(
            GrantPath::<Trace>::walk(FrozenPath::from_surface(path)).expect("a prefix walks"),
        )
    }

    fn excluding(path: &str) -> GrantPath<Excluded> {
        GrantPath::<Excluded>::walk(FrozenPath::from_surface(path)).expect("an exclusion walks")
    }

    /// What the envelope would show of `policy`.
    fn plan(policy: &SandboxProjection) -> Shown {
        let rendered = policy.rendered().expect("ASCII paths render");
        Shown::of(&rendered, &host_system_reads()).expect("the plan builds")
    }

    /// `policy`'s binds, opened over this host's tree, and the handoff
    /// holding their handles.
    fn lent(policy: &SandboxProjection, host: HostEnvelope) -> (Binds, Handoff) {
        let mut handoff = Handoff::default();
        let binds = Binds::open(&on(&stand_in(), host), plan(policy), &mut handoff)
            .expect("the binds open");
        (binds, handoff)
    }

    /// `policy`'s binds, for a render, which reads their slots alone.
    fn binds(policy: &SandboxProjection, host: HostEnvelope) -> Binds {
        lent(policy, host).0
    }

    /// What the descriptor lent at `slot` holds, while its handoff lives.
    fn stat_of(slot: Slot) -> rustix::fs::Stat {
        rustix::fs::stat(crate::path::proc_fd_path(slot.fd())).expect("stat the lent handle")
    }

    /// What the handle lent for the bind at `dest` holds, while its
    /// handoff lives.
    fn held_at(binds: &Binds, dest: &std::path::Path) -> rustix::fs::Stat {
        let dest = dest.to_string_lossy();
        let (slot, _) = (binds.0.iter())
            .find(|(_, bind)| bind.dest.as_str() == dest)
            .expect("a bind there");
        stat_of(*slot)
    }

    /// The `--info-fd` slot a kept render names: a number, nothing lent.
    fn info_slot() -> Slot {
        Slot::parse("98").expect("a slot")
    }

    /// A handoff lending `/dev/null` as the warrant: for a launch never
    /// spawned, or refused before its trampoline would read one.
    fn null_warrant() -> (Handoff, Slot) {
        use std::os::fd::AsFd;
        let null = std::fs::File::open("/dev/null").expect("open /dev/null");
        let mut handoff = Handoff::default();
        let at = handoff.lend(null.as_fd()).expect("lends");
        (handoff, at)
    }

    /// `policy` rendered over `binds`, kept or surrendered.
    fn render(
        policy: &SandboxProjection,
        binds: &Binds,
        chdir: Option<&str>,
        ownership: Ownership,
        host: HostEnvelope,
    ) -> Vec<String> {
        let rendered = policy.rendered().expect("ASCII paths render");
        let address = (ownership == Ownership::Kept).then(info_slot);
        let chdir = chdir.map(std::path::Path::new);
        bwrap_argv(&rendered, chdir, address, host, binds, &[])
            .expect("ASCII paths render")
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn options(
        policy: &SandboxProjection,
        chdir: Option<&str>,
        ownership: Ownership,
        host: HostEnvelope,
    ) -> Vec<String> {
        render(policy, &binds(policy, host), chdir, ownership, host)
    }

    fn options_on(
        host: HostEnvelope,
        policy: &SandboxProjection,
        ownership: Ownership,
    ) -> Vec<String> {
        options(policy, None, ownership, host)
    }

    fn options_for(policy: &SandboxProjection) -> Vec<String> {
        options_on(WHOLE, policy, Ownership::Kept)
    }

    fn position_of(args: &[String], mask: &[&str]) -> Option<usize> {
        args.windows(mask.len()).position(|w| w == mask)
    }

    /// Where `op` mounts a lent handle at `dest`, whatever its slot.
    fn bound(args: &[String], op: &str, dest: &str) -> Option<usize> {
        (args.windows(3)).position(|w| w[0] == op && w[1].parse::<i32>().is_ok() && w[2] == dest)
    }

    #[test]
    fn an_existing_file_is_masked_by_an_unopenable_node_after_its_bind() {
        let dir = workdir("deny-file");
        let denied = dir.join(".exarch.toml");
        std::fs::write(&denied, "capabilities").unwrap();

        let policy = deny_within(&dir, &[&denied]);
        let args = options_for(&policy);
        let bind = bound(&args, "--bind-fd", &dir.to_string_lossy());
        let mask = position_of(
            &args,
            &["--ro-bind", "/dev/null", &denied.to_string_lossy()],
        );

        assert!(bind.is_some(), "rw bind missing: {args:?}");
        assert!(
            mask.is_some(),
            "node mask missing for an existing file: {args:?}"
        );
        assert!(bind.unwrap() < mask.unwrap(), "the mask must win");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without `--perms` the mask is writable and a child's write inside it
    /// succeeds into throwaway memory — a lie rather than a deny.
    #[test]
    fn a_directory_is_masked_by_a_tmpfs_with_no_permission_bits() {
        let dir = workdir("deny-dir");
        let denied = dir.join(".git");
        std::fs::create_dir_all(&denied).unwrap();

        let args = options_for(&deny_within(&dir, &[&denied]));
        assert!(
            position_of(
                &args,
                &["--perms", "0000", "--tmpfs", &denied.to_string_lossy()]
            )
            .is_some(),
            "a denied directory needs an unwritable tmpfs mask: {args:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bwrap cannot `--chdir` into a name nothing mounted, so a cwd outside
    /// the grant gets an empty unwritable directory — before the binds, which
    /// hide it where they cover the name — and the root, the scratch `/tmp`
    /// and an unrestricted envelope, which already have one, get none.
    #[test]
    fn a_cwd_outside_the_grant_is_stood_up_empty_and_unwritable() {
        let dir = workdir("cwd-stand-in");
        let dir_s = dir.to_string_lossy().into_owned();
        let granted = deny_within(&dir, &[]);
        for (label, policy, cwd, stood_up) in [
            ("ungranted", &granted, "/ral-ungranted/cwd", true),
            ("root", &granted, "/", false),
            ("scratch", &granted, "/tmp", false),
            ("unrestricted", &unrestricted(), "/ral-ungranted/cwd", false),
        ] {
            let args = options(policy, Some(cwd), Ownership::Kept, WHOLE);
            let mask = position_of(&args, &["--perms", "0555", "--tmpfs", cwd]);
            assert_eq!(mask.is_some(), stood_up, "{label}: {args:?}");
            if let Some(mask) = mask {
                let bind = bound(&args, "--bind-fd", &dir_s).expect("the grant's bind");
                assert!(
                    mask < bind,
                    "{label}: a bind must be able to cover it: {args:?}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bwrap can neither `--chdir` into a mask nor pass through one, so a cwd
    /// at or beneath a deny, named directly or through a link, is refused
    /// before bwrap runs, naming both; a deny inside the cwd is ordinary.
    #[test]
    fn a_cwd_within_a_deny_is_refused_naming_both() {
        let dir = workdir("cwd-denied");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let denied = dir.join("secret");
        let inner = denied.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&denied, &link).unwrap();
        let argv = |deny: &std::path::Path, cwd: &std::path::Path| {
            let policy = deny_within(&dir, &[deny]);
            let rendered = policy.rendered().expect("ASCII paths render");
            let binds = binds(&policy, WHOLE);
            bwrap_argv(&rendered, Some(cwd), None, WHOLE, &binds, &[])
        };
        for (deny, cwd) in [(&denied, &denied), (&denied, &inner), (&link, &inner)] {
            let Err(why) = argv(deny, cwd) else {
                panic!(
                    "{} within {} must be refused",
                    cwd.display(),
                    deny.display()
                );
            };
            assert!(
                why.contains(&*denied.to_string_lossy()) && why.contains(&*cwd.to_string_lossy()),
                "{why}"
            );
        }
        assert!(
            argv(&denied, &dir).is_ok(),
            "a deny inside the cwd must not refuse"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One rule under either bind, for two reasons that meet in it: a mask is
    /// a mount, a mount over an absent name needs a mountpoint bwrap must
    /// `mkdir`, and that `mkdir` fails with `EROFS` under a read-only bind,
    /// killing the envelope — the failure mode a denied-but-absent path like
    /// `xdg:config/gcloud` triggers on a host with no gcloud — while under a
    /// writable identity bind it succeeds on the host, and the deny creates
    /// the name it forbids.
    #[test]
    fn an_absent_deny_is_never_mounted_over() {
        let dir = workdir("deny-absent");
        let config = dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        // Deliberately not created: the deny target must be absent.
        let under_write = dir.join("secret-not-yet-created");
        let under_read = config.join("gcloud");

        let read_only = restricted(&[&config], &[], &[&under_read]);
        for (bind, denied, policy) in [
            ("writable", &under_write, deny_within(&dir, &[&under_write])),
            ("read-only", &under_read, read_only),
        ] {
            let args = options_for(&policy);
            assert!(
                !args.contains(&denied.to_string_lossy().into_owned()),
                "no mount may land on an absent deny beneath a {bind} bind: {args:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Naming a symlink in the options costs the whole launch, so the mask goes
    /// on the resolved twin — which the backend derives at render time, the
    /// projection naming only the link.
    #[test]
    fn a_symlinked_deny_is_masked_at_its_target_and_never_at_the_link() {
        let dir = workdir("deny-link");
        let target = dir.join("id_rsa");
        let link = dir.join("link-to-id_rsa");
        std::fs::write(&target, "PRIVATE KEY").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let args = options_for(&deny_within(&dir, &[&link]));
        assert!(
            position_of(
                &args,
                &["--ro-bind", "/dev/null", &target.to_string_lossy()]
            )
            .is_some(),
            "the resolved target must carry the mask: {args:?}"
        );
        assert!(
            !args.iter().any(|a| *a == link.to_string_lossy()),
            "the link's own name must not be mounted over: {args:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `real/tool`, runnable, under a fresh work dir with `linkdir -> real`
    /// beside it: the dir, and the tool's real path.
    fn linked_tool(tag: &str) -> (std::path::PathBuf, String) {
        use std::os::unix::fs::PermissionsExt;
        let dir = workdir(tag);
        let tool = dir.join("real/tool");
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("linkdir")).unwrap();
        let tool = std::fs::canonicalize(&tool).unwrap();
        (dir, tool.to_string_lossy().into_owned())
    }

    /// The launched file is no bind of its own: law 7 has put it in the read
    /// region, which shows it, and a grant that does not read it shows
    /// nothing at its name.  The payload starts in the logical cwd.
    #[test]
    fn the_launched_file_is_shown_by_the_reads_alone_and_the_cwd_is_entered() {
        let (dir, real) = linked_tool("target-bind");
        let elsewhere = workdir("target-bind-elsewhere");
        let args = options(
            &deny_within(&elsewhere, &[]),
            Some("/"),
            Ownership::Kept,
            WHOLE,
        );
        assert!(
            bound(&args, "--ro-bind-fd", &real).is_none(),
            "an unread image is not shown: {args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|a| a == "--symlink" || a.contains("linkdir")),
            "nothing inside looks the spelled name up: {args:?}"
        );
        assert!(
            position_of(&args, &["--chdir", "/"]).is_some(),
            "the payload must start in the logical cwd: {args:?}"
        );
        let args = options(&restricted(&[&dir], &[], &[]), None, Ownership::Kept, WHOLE);
        assert!(
            bound(&args, "--ro-bind-fd", &dir.to_string_lossy()).is_some()
                && bound(&args, "--ro-bind-fd", &real).is_none(),
            "a read of the directory shows it, and the image within it only there: {args:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// Through the fold, as a launch projects a grant: the baseline included.
    fn restricted(
        read: &[&std::path::Path],
        write: &[&std::path::Path],
        deny: &[&std::path::Path],
    ) -> SandboxProjection {
        let frozen = |paths: &[&std::path::Path]| {
            paths
                .iter()
                .map(|p| FrozenPath::from_surface(crate::path::utf8(p).unwrap()))
                .collect()
        };
        let fs = FsPolicy {
            read_prefixes: frozen(read),
            write_prefixes: frozen(write),
            deny_paths: frozen(deny),
        };
        let caps = Capabilities {
            fs: Some(fs),
            ..Capabilities::default()
        };
        let stack = crate::test_helper::stack([crate::test_helper::layer(caps)]);
        SandboxProjection::of(&stack, &host_system_reads(), None).expect("an fs grant projects")
    }

    /// The `Shown` binds beneath `dir`, the baseline left out.
    fn shown_within(binds: &Binds, dir: &std::path::Path) -> Vec<(String, Access)> {
        (binds.layer(Layer::Shown))
            .map(|(_, bind)| (bind.dest.as_str().to_string(), bind.access))
            .filter(|(dest, _)| std::path::Path::new(dest).starts_with(dir))
            .collect()
    }

    /// One bind per name the projection shows and another bind does not:
    /// an absent prefix shows nothing; a read-only one within another bind,
    /// or a run place within the grant's writable one, shows nothing; a
    /// writable one within a read-only one wins; a file mounts as a file; a
    /// run place outside every prefix is shown, as on macOS.
    #[test]
    fn a_bind_is_opened_for_each_name_no_other_bind_shows() {
        use std::os::unix::fs::MetadataExt;
        let dir = workdir("binds-open");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (ro, inner, w, file, tools) = (
            dir.join("ro"),
            dir.join("ro/inner"),
            dir.join("ro/w"),
            dir.join("file"),
            dir.join("tools"),
        );
        for at in [&inner, &w.join("bin"), &tools] {
            std::fs::create_dir_all(at).unwrap();
        }
        std::fs::write(&file, "x").unwrap();
        let absent = dir.join("absent");
        let mut policy = restricted(&[&ro, &inner, &file, &absent], &[&w], &[]);
        if let FsProjection::Restricted(rules) = &mut policy.fs {
            rules.read.extend(
                [&tools, &w.join("bin")].map(|d| FsAllow::RunPlace(crate::test_helper::real(d))),
            );
        }
        let (binds, _handoff) = lent(&policy, WHOLE);
        let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
        assert_eq!(
            shown_within(&binds, &dir),
            [
                (name(&file), Access::ReadOnly),
                (name(&ro), Access::ReadOnly),
                (name(&tools), Access::ReadOnly),
                (name(&w), Access::Writable),
            ]
        );
        let opened = held_at(&binds, &file);
        assert_eq!(opened.st_ino, std::fs::metadata(&file).unwrap().ino());
        assert!(rustix::fs::FileType::from_raw_mode(opened.st_mode).is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A name through a symlink is shown at the object it reaches, and at
    /// itself unless another bind shows the host's own link there.
    #[test]
    fn a_linked_prefix_shows_its_object_at_both_names() {
        let dir = workdir("binds-spelling");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (real, link) = (dir.join("real"), dir.join("link"));
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink("real", &link).unwrap();
        let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let both = binds(&restricted(&[&link], &[], &[]), WHOLE);
        assert_eq!(
            shown_within(&both, &dir),
            [
                (name(&link), Access::ReadOnly),
                (name(&real), Access::ReadOnly)
            ]
        );
        let covered = binds(&restricted(&[&dir, &link], &[], &[]), WHOLE);
        assert_eq!(
            shown_within(&covered, &dir),
            [(name(&dir), Access::ReadOnly)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A writable bind beneath a symlinked ancestor is a mount of its own,
    /// not the host's symlink: the read-only bind at the link does not cover
    /// it.
    #[test]
    fn a_write_beneath_a_linked_ancestor_stays_writable() {
        let dir = workdir("binds-linked-ancestor");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (real, link) = (dir.join("real"), dir.join("link"));
        std::fs::create_dir_all(real.join("target")).unwrap();
        std::os::unix::fs::symlink("real", &link).unwrap();
        let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let binds = binds(&restricted(&[&link], &[&link.join("target")], &[]), WHOLE);
        assert_eq!(
            shown_within(&binds, &dir),
            [
                (name(&link), Access::ReadOnly),
                (name(&real), Access::ReadOnly),
                (name(&link.join("target")), Access::Writable),
                (name(&real.join("target")), Access::Writable),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writing does not withdraw run authority: a write that covers a run
    /// place leaves the one writable bind as the whole of the view there.
    #[test]
    fn a_write_covering_a_run_place_leaves_one_writable_bind() {
        let dir = workdir("binds-write-over-place");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (w, bin) = (dir.join("w"), dir.join("w/bin"));
        std::fs::create_dir_all(&bin).unwrap();
        let mut policy = restricted(&[], &[&w], &[]);
        if let FsProjection::Restricted(rules) = &mut policy.fs {
            rules
                .read
                .push(FsAllow::RunPlace(crate::test_helper::real(&bin)));
        }
        policy.exec = ExecProjection::Restricted(
            std::iter::once(dir_rule(&crate::test_helper::real(&bin), true)).collect(),
        );
        let binds = binds(&policy, WHOLE);
        let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
        assert_eq!(shown_within(&binds, &w), [(name(&w), Access::Writable)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A write prefix through a symlink is one handle bound at two names, and
    /// a mount laid inside one bind does not show at the other: a mask goes
    /// on at both.
    #[test]
    fn a_write_through_a_link_is_masked_at_both_its_names() {
        let dir = workdir("binds-linked-write");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (real, link) = (dir.join("real"), dir.join("link"));
        let secret = real.join("secret");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(&secret, "x").unwrap();
        std::os::unix::fs::symlink("real", &link).unwrap();
        let policy = restricted(&[], &[&link], &[&secret]);
        let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let args = options_for(&policy);
        for masked in [&secret, &link.join("secret")] {
            assert!(
                position_of(&args, &["--ro-bind", "/dev/null", &name(masked)]).is_some(),
                "{}: {args:?}",
                masked.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bwrap starts the trampoline through the envelope's own `/proc`, so a
    /// deny hiding it is refused before anything is built; one inside it is
    /// a mask like any other.
    #[test]
    fn a_deny_that_hides_proc_is_refused() {
        for (denied, refused) in [("/proc", true), ("/proc/sys", false)] {
            let policy = SandboxProjection {
                fs: FsProjection::Restricted(FsRules {
                    deny: vec![excluding(denied)],
                    ..FsRules::default()
                }),
                ..unrestricted()
            };
            let rendered = policy.rendered().expect("renders");
            let binds = binds(&policy, WHOLE);
            let argv = bwrap_argv(&rendered, None, None, WHOLE, &binds, &[]);
            assert_eq!(argv.is_err(), refused, "{denied}: {argv:?}");
        }
    }

    /// A baseline path this user may not reach is left out, as an absent
    /// one is; any other prefix the grant names is refused instead.
    #[test]
    fn an_unreachable_object_is_absent_only_where_no_grant_names_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = workdir("binds-eacces");
        let locked = dir.join("locked");
        std::fs::create_dir_all(locked.join("inner")).unwrap();
        let inner = crate::test_helper::real(locked.join("inner"));
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let opened = super::open_object(&inner, false);
        if opened.is_ok() {
            eprintln!("skipping: this user reaches through a mode-0 directory");
        } else {
            assert!(opened.is_err_and(|why| why.contains("Permission denied")));
            assert!(matches!(super::open_object(&inner, true), Ok(None)));
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A grant renders real paths; a symlink on one when the launch opens it
    /// was planted since, and refuses the launch rather than show its target.
    #[test]
    fn a_prefix_whose_component_became_a_symlink_since_render_is_refused() {
        let dir = workdir("binds-race");
        let (parent, elsewhere) = (dir.join("parent"), dir.join("elsewhere"));
        std::fs::create_dir_all(parent.join("data")).unwrap();
        std::fs::create_dir_all(elsewhere.join("data")).unwrap();
        let rendered = restricted(&[&parent.join("data")], &[], &[])
            .rendered()
            .expect("ASCII paths render");
        std::fs::rename(&parent, dir.join("moved")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &parent).unwrap();
        let shown = Shown::of(&rendered, &host_system_reads()).expect("the plan builds");
        let Err(why) = Binds::open(&on(&stand_in(), WHOLE), shown, &mut Handoff::default()) else {
            panic!("a symlinked component would show its target");
        };
        assert!(why.contains("symbolic link"), "{why}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory removed and made again after the grant rendered it is what
    /// the launch mounts: the object at the position, not the one it named.
    #[test]
    fn a_directory_recreated_after_the_render_is_what_the_bind_holds() {
        use std::os::unix::fs::MetadataExt;
        let dir = workdir("binds-recreated");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let data = dir.join("data");
        std::fs::create_dir(&data).unwrap();
        let policy = restricted(&[&data], &[], &[]);
        let rendered = policy.rendered().expect("ASCII paths render");
        let before = std::fs::metadata(&data).unwrap().ino();
        std::fs::rename(&data, dir.join("old")).unwrap();
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("fresh"), "x").unwrap();
        let shown = Shown::of(&rendered, &host_system_reads()).expect("the plan builds");
        let mut handoff = Handoff::default();
        let binds =
            Binds::open(&on(&stand_in(), WHOLE), shown, &mut handoff).expect("the binds open");
        let held = held_at(&binds, &data);
        assert_ne!(held.st_ino, before, "the directory the grant named is gone");
        assert_eq!(held.st_ino, std::fs::metadata(&data).unwrap().ino());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A position that names a link when the launch opens it refuses the
    /// launch, in the sentence it always did.
    #[test]
    fn a_position_turned_link_refuses_the_launch_in_todays_sentence() {
        let dir = workdir("binds-turned-link");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (data, elsewhere) = (dir.join("data"), dir.join("elsewhere"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let policy = restricted(&[&data], &[], &[]);
        let rendered = policy.rendered().expect("ASCII paths render");
        std::fs::rename(&data, dir.join("old")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &data).unwrap();
        let shown = Shown::of(&rendered, &host_system_reads()).expect("the plan builds");
        let Err(why) = Binds::open(&on(&stand_in(), WHOLE), shown, &mut Handoff::default()) else {
            panic!("a link at the position would show its target");
        };
        assert_eq!(
            why,
            format!(
                "sandbox: {} now goes through a symbolic link that ral will not follow; \
                 reload the grant or restart ral if you made that change",
                data.display()
            )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file allowance and a run place that is a file are bound as files.
    #[test]
    fn a_file_allowance_and_a_run_place_are_bound_as_files() {
        use std::os::unix::fs::MetadataExt;
        let dir = workdir("binds-files");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (allowed, tool) = (dir.join("allowed"), dir.join("tool"));
        for file in [&allowed, &tool] {
            std::fs::write(file, "x").unwrap();
        }
        let mut policy = restricted(&[&allowed], &[], &[]);
        if let FsProjection::Restricted(rules) = &mut policy.fs {
            let place = crate::test_helper::real(&tool);
            rules.read.push(FsAllow::RunPlace(place));
        }
        let (binds, _handoff) = lent(&policy, WHOLE);
        for file in [&allowed, &tool] {
            let held = held_at(&binds, file);
            assert!(rustix::fs::FileType::from_raw_mode(held.st_mode).is_file());
            assert_eq!(held.st_ino, std::fs::metadata(file).unwrap().ino());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every handle by its slot, in layer order: the projection's view, the
    /// envelope's own `/proc` and `/dev`, what goes over everything (the
    /// cgroup tree, then the pins at their current names), then the masks;
    /// under an unrestricted fs the root, then `/proc`.  No source is a name
    /// bwrap resolves but `/dev/null`.
    #[test]
    fn every_bind_is_a_handle_by_slot_in_layer_order() {
        let dir = workdir("argv-order");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (ro, w) = (dir.join("ro"), dir.join("ro/w"));
        let secret = w.join("secret");
        std::fs::create_dir_all(&w).unwrap();
        std::fs::write(&secret, "x").unwrap();
        let policy = restricted(&[&ro], &[&w], &[&secret]);
        let binds = binds(&policy, WHOLE);
        let args = render(&policy, &binds, None, Ownership::Kept, WHOLE);
        let at = |args: &[String], piece: &[&str]| {
            position_of(args, piece).unwrap_or_else(|| panic!("no {piece:?}: {args:?}"))
        };
        let by_handle = |args: &[String], op: &str, dest: &std::path::Path| {
            bound(args, op, &dest.to_string_lossy())
                .unwrap_or_else(|| panic!("no {op} at {}: {args:?}", dest.display()))
        };
        let order = [
            by_handle(&args, "--ro-bind-fd", &ro),
            by_handle(&args, "--bind-fd", &w),
            at(&args, &["--proc", "/proc"]),
            at(&args, &["--dev", "/dev"]),
            by_handle(&args, "--ro-bind-fd", std::path::Path::new(CGROUP)),
            by_handle(&args, "--ro-bind-fd", &pin_name()),
            at(
                &args,
                &["--ro-bind", "/dev/null", &secret.to_string_lossy()],
            ),
        ];
        assert!(order.is_sorted(), "{order:?}: {args:?}");
        let over_root = options_for(&unrestricted());
        let order = [
            at(&over_root, &["--dev-bind", "/", "/"]),
            at(&over_root, &["--proc", "/proc"]),
        ];
        assert!(order.is_sorted(), "{order:?}: {over_root:?}");
        let mounted: Vec<_> = (args.windows(3))
            .filter(|w| w[0] == "--ro-bind-fd" || w[0] == "--bind-fd")
            .map(|w| (w[1].clone(), w[2].as_str()))
            .collect();
        let expected: Vec<_> = (binds.layer(Layer::Shown))
            .chain(binds.layer(Layer::Over))
            .map(|(slot, bind)| (slot.to_string(), bind.dest.as_str()))
            .collect();
        assert_eq!(mounted, expected, "one slot per bind, in layer order");
        for w in args.windows(2).filter(|w| w[0] == "--ro-bind") {
            assert_eq!(w[1], "/dev/null", "a source bwrap would resolve by name");
        }
        assert!(
            !args.iter().any(|a| a == "--bind" || a == "--symlink"),
            "{args:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without `--ro-bind-fd` nothing could be shown but by name, so nothing
    /// is launched at all; a bwrap that builds no envelope is refused in its
    /// own words, never blamed on its version.
    #[test]
    fn a_bwrap_that_cannot_mount_by_descriptor_launches_nothing() {
        let rendered = unrestricted().rendered().expect("renders");
        let pin = stand_in();
        let path = crate::path::shown(pin.arg0());
        let refusal = |builds| {
            let (handoff, warrant) = null_warrant();
            let host = HostEnvelope { builds, ..WHOLE };
            let launch = bwrap_command(
                &on(&pin, host),
                plan(&unrestricted()),
                handoff,
                warrant,
                &rendered,
                None,
                Ownership::Kept,
            );
            launch
                .err()
                .expect("a launch it cannot mount must be refused")
                .to_string()
        };
        let why = refusal(Builds::ByName);
        assert!(why.contains(&path) && why.contains("0.8.0"), "{why}");
        let why = refusal(Builds::Nothing("No permissions to create a new namespace"));
        assert!(why.contains(&path) && why.contains("cannot build"), "{why}");
        assert!(
            why.contains("No permissions") && !why.contains("0.8.0"),
            "{why}"
        );
    }

    /// `/bin/sh -c script`, through [`run_admitted`].
    fn run_confined(
        env: &Envelope<'_>,
        policy: &SandboxProjection,
        script: &str,
    ) -> Option<std::process::Output> {
        run_admitted(env, policy, &sh_c(script))
    }

    fn sh_c(script: &str) -> Admitted {
        let sh = Program::file("/bin/sh".into()).expect("/bin/sh exists");
        admitted(sh, &["-c".to_string(), script.to_string()])
    }

    /// Through the very argv a launch builds: the payload is the trampoline,
    /// which enters the Landlock layer before `execve`ing the admitted
    /// program.  A broken trampoline is exactly what these tests exist to
    /// catch, so even the positive control goes this way.
    fn run_admitted(
        env: &Envelope<'_>,
        policy: &SandboxProjection,
        admitted: &Admitted,
    ) -> Option<std::process::Output> {
        run_after(env, policy, admitted, || ())
    }

    /// [`run_admitted`], built now and spawned after `between`: the window in
    /// which a same-uid writer races the launch.
    fn run_after(
        env: &Envelope<'_>,
        policy: &SandboxProjection,
        admitted: &Admitted,
        between: impl FnOnce(),
    ) -> Option<std::process::Output> {
        let (mut cmd, info_fd) = enveloped(
            env,
            policy,
            &host_system_reads(),
            admitted,
            None,
            Ownership::Kept,
        )
        .expect("the launch builds");
        between();
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().ok();
        // Kept open across the fork inside `output()`, whose hook names its copies' numbers.
        drop(info_fd);
        out
    }

    /// This host's pinned bwrap running ral's own pin, where it can build an
    /// envelope at all.  Where it cannot — bwrap absent, or user namespaces
    /// unavailable — a spawning test proves nothing either way and says so on
    /// the way out.
    fn envelope_launches(policy: &SandboxProjection) -> Option<Envelope<'static>> {
        super::pin_envelope();
        let Ok(bwrap) = super::envelope() else {
            eprintln!("skipping: this host has no bwrap to pin");
            return None;
        };
        let own = crate::authority::sandbox::reexec::own().expect("ral pins itself");
        let env = Envelope::probe(bwrap, own);
        let control = run_confined(&env, policy, "echo READY");
        if control
            .as_ref()
            .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("READY"))
        {
            return Some(env);
        }
        let why = control.map_or_else(
            || "the envelope did not spawn".to_string(),
            |o| String::from_utf8_lossy(&o.stderr).trim().to_string(),
        );
        eprintln!("skipping: this host cannot build a bwrap envelope: {why}");
        None
    }

    /// `sh -c script` confined under `unrestricted()` through the path a
    /// real command takes, `Launch::spawn` then `RunningChild`, and the
    /// group the spawn named.  Callers skip first where no envelope builds.
    fn spawn_confined(
        script: &str,
        scope: &crate::process::Scope,
    ) -> (
        crate::shell::exec::command::RunningChild,
        crate::process::Pgid,
    ) {
        use crate::process::{Group, PgidPolicy};
        use crate::shell::exec::command::{Pumps, RunningChild};
        let shell = crate::test_helper::core_shell();
        let mut launch = crate::authority::sandbox::sandboxed_command(
            &unrestricted(),
            &Baseline::default(),
            &sh_c(script),
            Ownership::Kept,
            &shell.cwd(),
            scope,
        )
        .expect("build confined launch");
        let (child, pgid, jail) = launch
            .spawn(PgidPolicy::NewLeader)
            .expect("spawn confined sh");
        let pgid = pgid.expect("a kept launch names its payload's group");
        let running = RunningChild::assemble_with_owner(
            child,
            "sh".to_string(),
            Pumps::default(),
            Some(Group::Owns(pgid)),
            scope.clone(),
            jail,
        );
        (running, pgid)
    }

    /// The whole regression: `xdg:config` readable, `xdg:config/gcloud`
    /// denied, and no gcloud ever installed — the mask over that absent name
    /// had bwrap `mkdir` a mountpoint on a read-only bind, and its `EROFS`
    /// killed every external command under the grant.  The options were
    /// well-formed throughout, so only a spawn catches it.
    #[test]
    fn an_absent_deny_under_a_read_only_bind_still_lets_the_body_run() {
        let dir = workdir("deny-absent-spawn");
        let config = dir.join("config");
        let work = dir.join("work");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let denied = config.join("gcloud");

        let policy = |deny: &[&std::path::Path]| restricted(&[&config], &[&work], deny);
        let Some(env) = envelope_launches(&policy(&[])) else {
            return;
        };

        let script = format!(
            "echo READY\n\
             mkdir -p '{denied}' 2>/dev/null || echo DENY-CREATE-REFUSED\n",
            denied = denied.display(),
        );
        let out = run_confined(&env, &policy(&[&denied]), &script).expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);

        assert!(
            stdout.contains("READY"),
            "an absent deny stopped the envelope from launching: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("DENY-CREATE-REFUSED"),
            "the read-only bind must still refuse creating the denied name: {stdout}"
        );
        assert!(
            !denied.exists(),
            "the denied name was created on the host: {}",
            denied.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Building an envelope must leave the host tree exactly as it found it.
    /// Masking an absent deny under a *writable* identity bind had bwrap
    /// `mkdir` its mountpoint straight onto the host: one launch under
    /// `deny: ['cwd:/.env']` and the user's project held an `.env` directory
    /// their own tools could then never write.  Only a spawn sees it — the
    /// options were well-formed, and the mount was correct inside the
    /// namespace.
    #[test]
    fn building_an_envelope_never_creates_a_denied_name_on_the_host() {
        let dir = workdir("deny-absent-spawn-rw");
        let denied = dir.join("not-yet");

        let Some(env) = envelope_launches(&deny_within(&dir, &[])) else {
            return;
        };
        let out =
            run_confined(&env, &deny_within(&dir, &[&denied]), "echo READY").expect("spawn bwrap");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("READY"),
            "an absent deny stopped the envelope from launching: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !denied.exists(),
            "building the envelope created the denied name on the host: {}",
            denied.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No options assertion can catch a mask that makes bwrap exit before it
    /// execs anything, so this one spawns the envelope too.
    #[test]
    fn a_denied_path_refuses_every_access_while_the_body_still_runs() {
        let dir = workdir("deny-spawn");
        let key = dir.join("id_rsa");
        let git = dir.join(".git");
        let readable = dir.join("README");
        std::fs::write(&key, "PRIVATE-KEY-BYTES").unwrap();
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("config"), "GIT-CONFIG-BYTES").unwrap();
        std::fs::write(&readable, "README-BYTES").unwrap();

        let Some(env) = envelope_launches(&deny_within(&dir, &[])) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             cat '{key}' 2>/dev/null || echo KEY-READ-REFUSED\n\
             echo pwned > '{key}' 2>/dev/null || echo KEY-WRITE-REFUSED\n\
             cat '{config}' 2>/dev/null || echo GIT-READ-REFUSED\n\
             touch '{planted}' 2>/dev/null || echo GIT-WRITE-REFUSED\n\
             chmod 700 '{git}' 2>/dev/null; touch '{planted}' 2>/dev/null\n\
             cat '{readable}' 2>/dev/null\n",
            key = key.display(),
            config = git.join("config").display(),
            git = git.display(),
            planted = git.join("planted").display(),
            readable = readable.display(),
        );
        let out =
            run_confined(&env, &deny_within(&dir, &[&key, &git]), &script).expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        assert!(
            stdout.contains("READY"),
            "the deny masks stopped the envelope from launching, so nothing was confined: {stderr}"
        );
        assert!(
            !stdout.contains("PRIVATE-KEY-BYTES"),
            "a denied file's bytes reached the child: {stdout}"
        );
        assert!(
            !stdout.contains("GIT-CONFIG-BYTES"),
            "a denied directory's contents reached the child: {stdout}"
        );
        for refusal in [
            "KEY-READ-REFUSED",
            "KEY-WRITE-REFUSED",
            "GIT-READ-REFUSED",
            "GIT-WRITE-REFUSED",
        ] {
            assert!(stdout.contains(refusal), "missing {refusal}: {stdout}");
        }
        assert!(
            stdout.contains("README-BYTES"),
            "the grant's own writable prefix stopped being readable: {stdout}"
        );

        // The script's last move is the mask's known limit: a child that
        // chmods the tmpfs it owns can write inside it.  What must hold is
        // that none of it reaches the host, which the two assertions below
        // are — the deny is over the real directory, not over the memory a
        // child spends on believing otherwise.
        assert_eq!(
            std::fs::read_to_string(&key).unwrap(),
            "PRIVATE-KEY-BYTES",
            "the denied file was overwritten on the host"
        );
        assert!(
            !git.join("planted").exists(),
            "a child planted a file inside a denied directory"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only a spawn shows the trampoline's `execve` reaching the file the user
    /// named: `~/.local/bin/uv`, a link into `~/.local/share/uv`.  The image is
    /// shown by the reads alone, at its real path: a grant that reads it runs
    /// it by either name, and one that does not has the `execve` fail, the
    /// image being absent inside.
    #[test]
    fn a_linked_image_runs_through_the_read_that_shows_its_real_path() {
        let (dir, real) = linked_tool("image-link-spawn");
        std::fs::write(&real, "#!/bin/sh\necho READY\n").unwrap();
        let elsewhere = workdir("image-link-spawn-elsewhere");
        let tool = || Program::file(dir.join("linkdir/tool")).expect("the tool exists");
        let reading = restricted(&[&dir.join("real")], &[], &[]);
        let Some(env) = envelope_launches(&reading) else {
            return;
        };
        let out = run_admitted(&env, &reading, &admitted(tool(), &[])).expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("READY"),
            "the spelled name did not run inside the envelope: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let apart = deny_within(&elsewhere, &[]);
        let out = run_admitted(&env, &apart, &admitted(tool(), &[])).expect("spawn bwrap");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("READY")
                && String::from_utf8_lossy(&out.stderr).contains(&real),
            "an image no read shows ran: {out:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// A grant keeps the real path it froze: a spelling retargeted since
    /// shows the frozen object, never the new target.
    #[test]
    fn a_spelling_retargeted_after_freezing_shows_its_frozen_object() {
        let dir = workdir("binds-retarget");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (first, second, link) = (dir.join("first"), dir.join("second"), dir.join("link"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::os::unix::fs::symlink("first", &link).unwrap();
        let policy = restricted(&[&link], &[], &[]);
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("second", &link).unwrap();
        let shown = plan(&policy);
        let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let objects: Vec<_> = (shown.binds.iter())
            .map(|bind| bind.object.as_str().to_string())
            .filter(|object| std::path::Path::new(object).starts_with(&dir))
            .collect();
        assert!(objects.contains(&name(&first)), "{objects:?}");
        assert!(!objects.contains(&name(&second)), "{objects:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The handles are opened when the launch is built, so a prefix swapped
    /// for a symlink before the spawn still shows what the grant named.
    #[test]
    fn a_prefix_swapped_for_a_symlink_after_the_open_shows_what_was_opened() {
        let dir = workdir("swap-race");
        let (data, elsewhere) = (dir.join("data"), dir.join("elsewhere"));
        for (at, bytes) in [(&data, "ORIGINAL-BYTES"), (&elsewhere, "SWAPPED-BYTES")] {
            std::fs::create_dir(at).unwrap();
            std::fs::write(at.join("f"), bytes).unwrap();
        }
        let policy = restricted(&[&data], &[], &[]);
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!("echo READY\ncat '{}'\n", data.join("f").display());
        let out = run_after(&env, &policy, &sh_c(&script), || {
            std::fs::rename(&data, dir.join("moved")).unwrap();
            std::os::unix::fs::symlink(&elsewhere, &data).unwrap();
        })
        .expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("READY") && stdout.contains("ORIGINAL-BYTES"),
            "the opened prefix was not what the child saw: {stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !stdout.contains("SWAPPED-BYTES"),
            "the swap redirected the mount: {stdout}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A destination is still a name bwrap resolves inside: one the host
    /// turned into a symlink after the open kills the launch, never moves
    /// the mount.
    #[test]
    fn a_destination_turned_into_a_symlink_after_the_open_runs_nothing() {
        let dir = workdir("dest-link");
        let (outer, inner) = (dir.join("outer"), dir.join("outer/inner"));
        std::fs::create_dir_all(&inner).unwrap();
        let policy = restricted(&[&outer], &[&inner], &[]);
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let out = run_after(&env, &policy, &sh_c("echo READY"), || {
            std::fs::rename(&inner, outer.join("moved")).unwrap();
            std::os::unix::fs::symlink("moved", &inner).unwrap();
        })
        .expect("spawn bwrap");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("READY"),
            "a mount followed a symlinked destination: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fallback is the same `/dev`, and only where the host refuses one.
    #[test]
    fn a_host_that_refuses_dev_gets_the_same_shape_by_hand() {
        let dir = workdir("dev-render");
        let policy = deny_within(&dir, &[]);

        let mounted = options_for(&policy);
        assert!(
            position_of(&mounted, &["--dev", "/dev"]).is_some(),
            "a host that mounts --dev must be given it: {mounted:?}"
        );

        let by_hand = options_on(
            HostEnvelope {
                virtual_dev: false,
                ..WHOLE
            },
            &policy,
            Ownership::Kept,
        );
        assert!(
            position_of(&by_hand, &["--dev", "/dev"]).is_none(),
            "the refused mount must not be asked for: {by_hand:?}"
        );
        let dev = position_of(&by_hand, &["--tmpfs", "/dev"]).expect("a /dev to fill");
        let shm = position_of(&by_hand, &["--tmpfs", "/dev/shm"]).expect("a fresh /dev/shm");
        assert!(dev < shm, "/dev's tmpfs would bury /dev/shm's: {by_hand:?}");
        for piece in [
            ["--dev-bind", "/dev/null", "/dev/null"],
            ["--dev-bind", "/dev/urandom", "/dev/urandom"],
            ["--symlink", "pts/ptmx", "/dev/ptmx"],
            ["--symlink", "/proc/self/fd/1", "/dev/stdout"],
        ] {
            assert!(
                position_of(&by_hand, &piece).is_some(),
                "the by-hand /dev is missing {piece:?}: {by_hand:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Forced rather than probed: where `--dev` mounts, nothing else would
    /// exercise this arm.
    #[test]
    fn the_by_hand_dev_serves_a_confined_body() {
        let dir = workdir("dev-by-hand");
        let policy = deny_within(&dir, &[]);
        let Some(env) = envelope_launches(&policy) else {
            return;
        };

        let by_hand = Envelope {
            host: HostEnvelope {
                virtual_dev: false,
                ..env.host
            },
            ..env
        };
        let out = run_confined(
            &by_hand,
            &policy,
            "echo x > /dev/null && echo NULL-OK\n\
             head -c 1 /dev/urandom > /dev/null && echo URANDOM-OK\n\
             : > /dev/shm/probe && echo SHM-OK\n\
             exec 3<>/dev/ptmx && echo PTMX-OK\n",
        )
        .expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        for ok in ["NULL-OK", "URANDOM-OK", "SHM-OK"] {
            assert!(
                stdout.contains(ok),
                "the by-hand /dev is missing {ok}: {stdout} {stderr}"
            );
        }
        // The pty is the host devpts's, opened at its own `ptmx`, which that
        // devpts's `ptmxmode` may close: the envelope serves one exactly where
        // the host's devpts does.
        let host_serves = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/pts/ptmx")
            .is_ok();
        assert_eq!(
            stdout.contains("PTMX-OK"),
            host_serves,
            "the by-hand /dev/ptmx must be the host devpts's own: {stdout} {stderr}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The namespaces key on there being an envelope; the pid and cgroup ones
    /// on the host fact each.
    #[test]
    fn every_envelope_owns_its_namespaces_and_the_probed_ones_where_the_host_allows() {
        let restricted = deny_within(&workdir("namespaces"), &[]);
        let masked = HostEnvelope {
            private_pids: false,
            private_cgroup: false,
            ..WHOLE
        };
        for (host, probed) in [(WHOLE, true), (masked, false)] {
            for policy in [&unrestricted(), &restricted] {
                for ownership in [Ownership::Kept, Ownership::Surrendered] {
                    let args = options_on(host, policy, ownership);
                    for flag in ["--unshare-ipc", "--unshare-uts"] {
                        assert!(
                            args.iter().any(|a| a == flag),
                            "{flag} must be on every envelope: {args:?}"
                        );
                    }
                    for flag in ["--unshare-pid", "--unshare-cgroup"] {
                        assert_eq!(
                            args.iter().any(|a| a == flag),
                            probed,
                            "{flag} must follow the host fact alone: {args:?}"
                        );
                    }
                }
            }
        }
    }
    /// `/sys` describes the host — `class/net` is the mounter's netns whatever
    /// `--unshare-net` did — so only what sizes a program is bound, and the
    /// cgroup tree goes on last as the payload's own: under the namespace its
    /// `/proc/self/cgroup` reads `0::/`, and a tree rooted anywhere else
    /// answers that path with somebody else's limits.
    #[test]
    fn sys_is_narrowed_to_what_sizes_a_program_and_the_cgroup_tree_is_the_payloads() {
        use std::os::unix::fs::MetadataExt;
        let dir = workdir("sys");
        let dir_s = dir.to_string_lossy();
        let own = format!("{CGROUP}{}", super::own_cgroup().expect("a cgroup2 host"));
        // The tree's handle, by the inode it opened.
        let tree = |host: HostEnvelope, policy: &SandboxProjection| {
            let (binds, _handoff) = lent(policy, host);
            let (slot, bind) = (binds.0.iter())
                .find(|(_, bind)| bind.dest.as_str() == CGROUP)
                .expect("a cgroup bind");
            assert_eq!(bind.layer, Layer::Over);
            stat_of(*slot).st_ino
        };
        let ino = |path: &str| std::fs::metadata(path).expect("the tree exists").ino();
        for (label, policy) in [
            ("unrestricted", unrestricted()),
            ("restricted", deny_within(&dir, &[])),
        ] {
            let args = options_for(&policy);
            assert!(
                bound(&args, "--ro-bind-fd", "/sys").is_none(),
                "{label}: the host's /sys must not be bound wholesale: {args:?}"
            );
            let projection = position_of(&args, &["--dev-bind", "/", "/"])
                .or_else(|| bound(&args, "--bind-fd", &dir_s))
                .expect("the projection's bind");
            let cgroup = bound(&args, "--ro-bind-fd", CGROUP)
                .unwrap_or_else(|| panic!("{label}: no cgroup tree: {args:?}"));
            assert!(
                projection < cgroup,
                "{label}: the tree goes over the projection's binds: {args:?}"
            );
            assert_eq!(tree(WHOLE, &policy), ino(&own), "{label}: not re-rooted");
        }
        let restricted = options_for(&deny_within(&dir, &[]));
        assert!(
            bound(&restricted, "--ro-bind-fd", "/sys/devices/system/cpu").is_some(),
            "a program must still count its cpus: {restricted:?}"
        );
        let shared = HostEnvelope {
            private_cgroup: false,
            ..WHOLE
        };
        assert_eq!(
            tree(shared, &deny_within(&dir, &[])),
            ino(CGROUP),
            "without the namespace the host's tree is the true one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the options cannot show: that a program inside reads its own limits
    /// where its `/proc/self/cgroup` says they are, and finds no interface
    /// the netns it is in does not have.
    #[test]
    fn a_confined_program_reads_its_own_cgroup_and_none_of_the_hosts_interfaces() {
        use std::os::unix::fs::MetadataExt;
        let restricted = deny_within(&workdir("sys-spawn"), &[]);
        let script = "echo READY\n\
                      ls /sys/class/net >/dev/null 2>&1 && echo NET-VISIBLE\n\
                      echo NPROC=$(nproc)\n\
                      echo CGROUP=$(cat /proc/self/cgroup)\n\
                      echo TREE=$(stat -c %i /sys/fs/cgroup)\n";
        for (label, policy) in [
            ("unrestricted", &unrestricted()),
            ("restricted", &restricted),
        ] {
            let Some(env) = envelope_launches(policy) else {
                continue;
            };
            let host = env.host;
            let out = run_confined(&env, policy, script).expect("spawn bwrap");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("READY"),
                "{label}: the envelope did not launch: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            if label == "restricted" {
                assert!(
                    !emitted(&stdout, "NET-VISIBLE"),
                    "the host's interfaces were listed under a restricted fs: {stdout}"
                );
            }
            let nproc: u32 = field(&stdout, "NPROC=")
                .and_then(|n| n.parse().ok())
                .expect("a cpu count");
            assert!(
                nproc > 0,
                "{label}: a program must still count its cpus: {stdout}"
            );
            let Some(own) = super::own_cgroup() else {
                continue;
            };
            let expected = if host.private_cgroup {
                assert_eq!(
                    field(&stdout, "CGROUP="),
                    Some("0::/"),
                    "{label}: the payload must be the root of its own namespace: {stdout}"
                );
                format!("/sys/fs/cgroup{own}")
            } else {
                "/sys/fs/cgroup".to_string()
            };
            let expected = std::fs::metadata(&expected)
                .expect("ral's own cgroup")
                .ino();
            assert_eq!(
                field(&stdout, "TREE=").and_then(|n| n.parse().ok()),
                Some(expected),
                "{label}: /sys/fs/cgroup inside must be the tree /proc/self/cgroup names: {stdout}"
            );
        }
    }

    /// The bound root, or a prefix of `/`, carries the host's `/proc` and
    /// `/dev`; the envelope's own must land after it, or `HostEnvelope`
    /// reports a hidden table that is the host's.  `/tmp` goes the other
    /// way: a write prefix beneath it lands inside the tmpfs.
    #[test]
    fn the_fresh_proc_and_dev_go_over_every_projection_bind() {
        let dir = workdir("proc-over-binds");
        let dir_s = dir.to_string_lossy().into_owned();
        let rooted = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read: vec![prefix("/")],
                write: vec![prefix(&dir_s)],
                ..FsRules::default()
            }),
            net: true,
            exec: ExecProjection::default(),
        };
        // `Unrestricted` dev-binds `/` wholesale and mounts no `/dev` of its own.
        for (label, policy, binds, fresh) in [
            (
                "unrestricted",
                unrestricted(),
                vec![["--dev-bind", "/"]],
                vec![["--proc", "/proc"]],
            ),
            (
                "restricted",
                rooted,
                vec![["--ro-bind-fd", "/"], ["--bind-fd", dir_s.as_str()]],
                vec![["--proc", "/proc"], ["--dev", "/dev"], ["--tmpfs", "/tmp"]],
            ),
        ] {
            let args = options_for(&policy);
            let at = |&[op, dest]: &[&str; 2]| {
                match op {
                    "--ro-bind-fd" | "--bind-fd" => bound(&args, op, dest),
                    "--dev-bind" => position_of(&args, &[op, dest, dest]),
                    _ => position_of(&args, &[op, dest]),
                }
                .unwrap_or_else(|| panic!("{label}: no {op} {dest} in the options: {args:?}"))
            };
            for bind in &binds {
                for mount in &fresh {
                    let ordered = if mount[1] == "/tmp" {
                        at(mount) < at(bind)
                    } else {
                        at(bind) < at(mount)
                    };
                    assert!(
                        ordered,
                        "{label}: {bind:?} and {mount:?} are in the wrong order: {args:?}"
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The launcher is the file pinned at boot, named by descriptor: no
    /// `PATH` — the shell's override included — has any say in what runs.
    /// Its own argv names, each by the slot it was lent at, the parcel its
    /// options arrive in, the trampoline — ral's pin — and the warrant.
    #[test]
    fn the_launcher_is_the_pinned_envelope_and_never_a_name() {
        let pin = stand_in();
        let (handoff, warrant) = null_warrant();
        let (cmd, _info_fd) = bwrap_command(
            &on(&pin, WHOLE),
            plan(&unrestricted()),
            handoff,
            warrant,
            &unrestricted().rendered().expect("renders"),
            None,
            Ownership::Kept,
        )
        .expect("ASCII paths render");
        let program = cmd.get_program().to_string_lossy();
        assert!(
            program.starts_with("/proc/self/fd/"),
            "the envelope must be exec'd by pinned descriptor: {program}"
        );
        let args: Vec<_> = (cmd.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let [opt, options, dashes, trampoline, flag, at] = &args[..] else {
            panic!("not `bwrap --args <a> -- /proc/self/fd/<t> --warrant <w>`: {args:?}");
        };
        assert_eq!(
            [opt.as_str(), dashes, flag, at],
            ["--args", "--", Role::Warrant.flag(), &warrant.to_string()],
            "{args:?}"
        );
        let options = Slot::parse(options).expect("`--args` names a slot");
        let parcel = std::fs::read(crate::path::proc_fd_path(options.fd()))
            .expect("the options' parcel is open");
        assert!(parcel.starts_with(b"--new-session\0"), "{parcel:?}");
        let trampoline = (trampoline.strip_prefix("/proc/self/fd/"))
            .and_then(|at| Slot::parse(at).ok())
            .expect("the trampoline by its slot");
        let lent = stat_of(trampoline);
        let pinned = rustix::fs::fstat(pin.fd()).expect("fstat the pin");
        assert_eq!(
            (lent.st_dev, lent.st_ino),
            (pinned.st_dev, pinned.st_ino),
            "the trampoline must be ral's pin"
        );
    }

    /// Whatever the projection bound, both files a launch execs go read-only
    /// over it by their own descriptors at their current names, before the
    /// masks.  The two names coincide here, the stand-in envelope and
    /// trampoline both being this test binary, so one bind locks both.
    #[test]
    fn the_envelope_binary_is_read_only_inside_every_envelope() {
        let pin = pin_name();
        let pin = pin.to_string_lossy();
        let dir = workdir("envelope-ro");
        let dir_s = dir.to_string_lossy();
        let denied = dir.join("secret");
        std::fs::write(&denied, "x").unwrap();

        for (label, policy) in [
            ("unrestricted", unrestricted()),
            ("restricted", deny_within(&dir, &[&denied])),
        ] {
            let args = options_for(&policy);
            let bound_at = position_of(&args, &["--dev-bind", "/", "/"])
                .or_else(|| bound(&args, "--bind-fd", &dir_s))
                .expect("the projection's bind");
            let ro = bound(&args, "--ro-bind-fd", &pin)
                .unwrap_or_else(|| panic!("{label}: no read-only bind of the pins: {args:?}"));
            assert!(bound_at < ro, "{label}: the pins' bind must win: {args:?}");
            assert_eq!(args.iter().filter(|a| **a == *pin).count(), 1, "{label}");
            if let Some(mask) = position_of(
                &args,
                &["--ro-bind", "/dev/null", &denied.to_string_lossy()],
            ) {
                assert!(ro < mask, "{label}: masks go on last: {args:?}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A kept child's envelope is tied to our death and a surrendered one's
    /// must not be, while every other confinement flag stays identical.
    #[test]
    fn only_the_two_ownership_ties_distinguish_a_surrendered_launch() {
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read: vec![prefix("/usr")],
                ..FsRules::default()
            }),
            net: false,
            exec: ExecProjection::default(),
        };
        let binds = binds(&policy, WHOLE);
        let kept = render(&policy, &binds, None, Ownership::Kept, WHOLE);
        let surrendered = render(&policy, &binds, None, Ownership::Surrendered, WHOLE);

        assert!(
            kept.contains(&"--die-with-parent".to_string()),
            "a child we keep must not outlive us: {kept:?}"
        );
        let info = info_slot().to_string();
        assert!(
            kept.windows(2).any(|w| w == ["--info-fd", info.as_str()]),
            "a kept launch must carry an `--info-fd` naming its payload's session: {kept:?}"
        );
        assert!(
            !surrendered.contains(&"--die-with-parent".to_string()),
            "a survivor must not be killed by our death: {surrendered:?}"
        );
        assert!(
            !surrendered.contains(&"--info-fd".to_string()),
            "a surrendered launch has no session left here to address: {surrendered:?}"
        );
        let mut untied = kept;
        let at = (untied.iter().position(|a| a == "--info-fd")).expect("an --info-fd");
        untied.drain(at..at + 2);
        untied.retain(|a| a != "--die-with-parent");
        assert_eq!(
            untied, surrendered,
            "the two launches must otherwise be confined identically"
        );
    }

    /// T3 (design doc §6): the grace signal must reach the confined payload's
    /// own session, not bwrap's mortal monitor.  Spawns through the same
    /// `Launch` + `RunningChild` path a real command takes, so the `--info-fd`
    /// leader in `Launch::spawn` is exercised end to end, then cancels with
    /// `Explicit` and checks the trap ran well inside `TEARDOWN_GRACE`.
    ///
    /// The trap's `exit 0` makes the outcome a plain exit; the file is the
    /// witness that the signal arrived.  `Unrestricted`: the ladder keys on
    /// there being an envelope, not on which axis was attenuated.
    #[test]
    fn a_confined_payload_gets_its_grace_signal_not_the_monitors() {
        use crate::process::{CancelCause, DurableRoot};

        if envelope_launches(&unrestricted()).is_none() {
            return;
        }

        let dir = workdir("receipt-grace");
        let flag = dir.join("grace");
        let script = format!(
            "trap 'echo GRACE > {} ; exit 0' TERM\nsleep 30\n",
            flag.display()
        );

        let scope = DurableRoot::default().worker();
        let (running, _) = spawn_confined(&script, &scope);

        let striker = scope.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            striker.cancel(CancelCause::Cancelled);
        });

        let t0 = std::time::Instant::now();
        let waited = running.wait();
        let elapsed = t0.elapsed();
        let (outcome, cause) = (waited.outcome, waited.cause);
        waited.settle(&scope);
        canceller.join().expect("canceller thread");

        assert!(
            elapsed.as_secs() < 5,
            "teardown must not fall back to the payload's own 30 s sleep: took {elapsed:?}"
        );
        assert_eq!(
            cause,
            Some(CancelCause::Cancelled),
            "the cancel must have reached this wait"
        );
        assert_eq!(
            outcome,
            crate::process::WaitOutcome::Exited(0),
            "the trap's own exit must be reported, not a SIGKILL of an \
             unreached payload"
        );
        assert_eq!(
            std::fs::read_to_string(&flag).unwrap_or_default().trim(),
            "GRACE",
            "the trap must have run: the grace signal reached the payload \
             itself, not just bwrap's monitor"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bwrap writes `child-pid` before that process's `setsid`, and a kill
    /// sent to a group not yet made is lost.  The spawn hands the group out
    /// only once it exists, so an abort's one kill, struck before the wait,
    /// ends a confined `sleep 30` at once.
    #[test]
    fn a_confined_payloads_group_exists_when_its_spawn_returns() {
        use crate::process::{CancelCause, DurableRoot};

        if envelope_launches(&unrestricted()).is_none() {
            return;
        }
        let scope = DurableRoot::default().worker();
        let (running, pgid) = spawn_confined("sleep 30", &scope);
        assert!(
            rustix::process::test_kill_process_group(pgid.as_pid()).is_ok(),
            "the payload's group {pgid} must exist once its spawn returns"
        );

        scope.cancel(CancelCause::Aborted);
        let t0 = std::time::Instant::now();
        let waited = running.wait();
        let elapsed = t0.elapsed();
        let outcome = waited.outcome.enveloped(true);
        waited.settle(&scope);
        assert!(
            elapsed.as_secs() < 5,
            "an abort's one kill must end the payload, not wait out its sleep: took {elapsed:?}"
        );
        assert_eq!(
            outcome,
            crate::process::WaitOutcome::Enveloped(128 + libc::SIGKILL),
            "the payload must die of the abort's kill"
        );
    }

    fn field<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
        stdout.lines().find_map(|line| line.strip_prefix(key))
    }

    /// A whole line: the script's own text echoes back in `/proc/1/cmdline`.
    fn emitted(stdout: &str, marker: &str) -> bool {
        stdout.lines().any(|line| line == marker)
    }

    /// Where the host cannot build the namespace, the shared table is
    /// asserted rather than skipped, so a masked host cannot read as a pass.
    #[test]
    fn no_host_pid_is_nameable_inside_the_envelope() {
        let restricted = deny_within(&workdir("pidns"), &[]);
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a host-side sleeper");
        // Nameability is asked of the table, never of `kill -0`: the Landlock
        // signal scope refuses a host pid whether or not it is nameable, so a
        // signal cannot tell the two apart — that is what makes the scope the
        // hole's second closure (`a_confined_child_cannot_signal_a_host_process`).
        let script = format!(
            "echo READY\n\
             [ -d /proc/{me} ] && echo HOST-PID-VISIBLE\n\
             kill -TERM {sleeper} 2>/dev/null && echo SLEEPER-SIGNALLED\n\
             echo PIDS=$(ls /proc | grep -c '^[0-9]')\n\
             echo INIT=$(tr '\\0' ' ' < /proc/1/cmdline)\n",
            me = std::process::id(),
            sleeper = sleeper.id(),
        );
        for policy in [&unrestricted(), &restricted] {
            let Some(env) = envelope_launches(policy) else {
                continue;
            };
            let host = env.host;
            let out = run_confined(&env, policy, &script).expect("spawn bwrap");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("READY"),
                "the envelope did not launch: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            if host.private_pids {
                assert!(
                    !emitted(&stdout, "HOST-PID-VISIBLE") && !emitted(&stdout, "SLEEPER-SIGNALLED"),
                    "a host pid was nameable inside the envelope: {stdout}"
                );
                assert!(
                    sleeper.try_wait().expect("poll the sleeper").is_none(),
                    "the host-side sleeper was killed from inside the envelope"
                );
                let pids: u32 = field(&stdout, "PIDS=")
                    .and_then(|n| n.parse().ok())
                    .expect("a pid count");
                assert!(
                    pids <= 8,
                    "the table inside must be the envelope's own: {stdout}"
                );
                assert!(
                    field(&stdout, "INIT=")
                        .and_then(|init| init.split_whitespace().next())
                        .is_some_and(|argv0| {
                            std::path::Path::new(argv0).file_name() == Some("bwrap".as_ref())
                        }),
                    "pid 1 inside must be bwrap's init: {stdout}"
                );
            } else {
                assert!(
                    emitted(&stdout, "HOST-PID-VISIBLE"),
                    "the datum says the table is shared, yet the test's own pid was not nameable: {stdout}"
                );
            }
        }
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }

    /// A bind of the host's `/proc` under a pid namespace would make
    /// `/proc/self` somebody else's.
    #[test]
    fn proc_self_is_the_payloads_own_on_every_projection() {
        let restricted = deny_within(&workdir("procself"), &[]);
        let shell = std::fs::canonicalize("/bin/sh").expect("resolve /bin/sh");
        // `read` is a builtin, so `/proc/self` is the shell's; `cat`'s would be its own.
        let script = "echo READY\n\
                      echo SELF=$$\n\
                      read pid _ < /proc/self/stat; echo STAT=$pid\n\
                      echo EXE=$(readlink /proc/$$/exe)\n";
        for policy in [&unrestricted(), &restricted] {
            let Some(env) = envelope_launches(policy) else {
                continue;
            };
            let host = env.host;
            let out = run_confined(&env, policy, script).expect("spawn bwrap");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("READY"),
                "the envelope did not launch: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let own: u32 = field(&stdout, "SELF=")
                .and_then(|n| n.parse().ok())
                .expect("the shell's pid");
            assert_eq!(
                field(&stdout, "STAT="),
                Some(own.to_string().as_str()),
                "/proc/self must be the reader's own entry: {stdout}"
            );
            // By name: the restricted envelope binds `/bin` beside `/usr`, so
            // the path is whichever mount the exec went through.
            assert_eq!(
                field(&stdout, "EXE=").and_then(|exe| std::path::Path::new(exe).file_name()),
                shell.file_name(),
                "/proc/<pid>/exe must name the shell's own binary: {stdout}"
            );
            if host.private_pids {
                assert!(
                    own < 64,
                    "the shell's pid must be namespace-local: {stdout}"
                );
            }
        }
    }

    // ── The Landlock layer the trampoline enters ─────────────────────────

    /// `exec` as the sole axis under test.  The fs stays open on purpose: a
    /// prefix that also hid the planted binary would let these tests pass on a
    /// layer that confines nothing.
    fn exec_policy(exec: ExecProjection) -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Unrestricted,
            net: true,
            exec,
        }
    }

    /// The command directories, plus whatever a test adds, by their real
    /// paths as a grant freezes them, and the directory coreutils really live
    /// in (`cat`'s), which a distribution may link `/usr/bin` into from a
    /// multi-call image elsewhere, and the linkers a launch's support
    /// supplies: nothing else is admitted, ral's own binary included.
    fn admitting(dirs: &[&str], paths: &[&str]) -> ExecProjection {
        use crate::test_helper::real;
        let coreutils = real("/usr/bin/cat").as_path().parent().map(real);
        let dirs = ["/bin", "/usr/bin"]
            .iter()
            .chain(dirs)
            .map(real)
            .chain(coreutils)
            .map(|d| dir_rule(&d, true));
        let files = paths.iter().map(|p| file_rule(&real(p), true));
        let linkers = crate::authority::sandbox::carriers::linkers()
            .into_iter()
            .map(|linker| file_rule(&linker, true));
        ExecProjection::Restricted(dirs.chain(files).chain(linkers).collect())
    }

    /// Landlock's own precondition, alongside `envelope_launches`: without a
    /// layer to enter there is nothing here to prove either way.
    fn landlock_at_least(need: super::landlock::Abi) -> bool {
        use super::landlock::Landlock;
        match Landlock::probe() {
            Landlock::Absent => {
                eprintln!(
                    "skipping: this kernel has no Landlock (not built in, or absent \
                     from the boot LSM list)"
                );
                false
            }
            // A host whose probe fails is broken, not a host without Landlock.
            Landlock::Unprobed(errno) => panic!(
                "the Landlock probe failed: {}",
                std::io::Error::from_raw_os_error(errno)
            ),
            Landlock::At(abi) if abi < need => {
                eprintln!(
                    "skipping: this kernel's Landlock is ABI {abi}, and this test needs {need}"
                );
                false
            }
            Landlock::At(_) => true,
        }
    }

    /// A runnable copy of `/bin/true` under `dir`, which no test admits.
    fn planted_binary(dir: &std::path::Path) -> std::path::PathBuf {
        let copy = dir.join("planted");
        std::fs::copy("/bin/true", &copy).expect("copy /bin/true");
        copy
    }

    /// The interpreter bypass the in-process guard cannot see: `sh -c` re-execs
    /// whatever it likes, and only the kernel is still looking.
    #[test]
    fn an_interpreter_cannot_exec_a_binary_outside_the_admits() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-bypass");
        let copy = planted_binary(&dir);
        let policy = exec_policy(admitting(&[], &[]));
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             '{copy}' && echo PLANTED-RAN\n\
             /bin/true && echo ADMITTED-RAN\n",
            copy = copy.display(),
        );
        let out = run_confined(&env, &policy, &script).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("READY"),
            "the envelope did not launch: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !emitted(&stdout, "PLANTED-RAN"),
            "a binary outside every admit ran: {stdout}"
        );
        assert!(
            emitted(&stdout, "ADMITTED-RAN"),
            "the layer denied an admitted command, so the deny proves nothing: {stdout}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The domain is inherited, so nesting interpreters buys nothing.
    #[test]
    fn a_nested_interpreter_inherits_the_layer() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-nested");
        let copy = planted_binary(&dir);
        let policy = exec_policy(admitting(&[], &[]));
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             sh -c '{copy}' && echo PLANTED-RAN\n\
             sh -c /bin/true && echo ADMITTED-RAN\n",
            copy = copy.display(),
        );
        let out = run_confined(&env, &policy, &script).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("READY"),
            "the envelope did not launch: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !emitted(&stdout, "PLANTED-RAN"),
            "a second `sh -c` escaped the layer: {stdout}"
        );
        assert!(
            emitted(&stdout, "ADMITTED-RAN"),
            "the layer denied an admitted command two levels down: {stdout}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// [`admitting`] less `blocks`, which Landlock renders by subtracting
    /// them from the allowed directories that hold them.
    fn admitting_less(dirs: &[&str], blocks: impl IntoIterator<Item = Rule>) -> ExecProjection {
        let ExecProjection::Restricted(rules) = admitting(dirs, &[]) else {
            unreachable!("admitting restricts");
        };
        ExecProjection::Restricted(rules.join(blocks.into_iter().collect()))
    }

    /// A command of `/usr/bin` with an inode of its own: a hard link or a
    /// multi-call alias would share an inode the layer admits.
    fn own_command(names: &[&str]) -> Option<std::path::PathBuf> {
        use std::os::unix::fs::MetadataExt;
        (names.iter())
            .map(|name| std::path::Path::new("/usr/bin").join(name))
            .find(|p| {
                p.symlink_metadata()
                    .is_ok_and(|m| m.is_file() && m.nlink() == 1)
            })
    }

    /// A deny and a veto inside an allowed directory hold in the kernel, so
    /// `sh -c`, which the guard never sees, reaches neither at any depth.
    #[test]
    fn an_exec_block_inside_an_allowed_directory_holds_for_grandchildren() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let (Some(denied), Some(vetoed)) = (
            own_command(&["id", "whoami", "tty"]),
            own_command(&["uname", "nproc", "logname"]),
        ) else {
            eprintln!("skipping: /usr/bin has no command with an inode of its own to block");
            return;
        };
        let name = vetoed.file_name().expect("a name").to_string_lossy();
        // A restricted fs that no write reaches `/usr/bin` through: a name
        // veto is walked only there.
        let work = workdir("exec-block");
        let policy = SandboxProjection {
            exec: admitting_less(
                &[],
                [
                    file_rule(&crate::test_helper::real(&denied), false),
                    veto_rule(&name),
                ],
            ),
            ..deny_within(&work, &[])
        };
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             sh -c '{denied}' >/dev/null 2>&1 && echo DENIED-RAN\n\
             sh -c \"sh -c '{denied}'\" >/dev/null 2>&1 && echo DENIED-NESTED-RAN\n\
             sh -c '{vetoed}' >/dev/null 2>&1 && echo VETOED-RAN\n\
             sh -c \"sh -c '{vetoed}'\" >/dev/null 2>&1 && echo VETOED-NESTED-RAN\n\
             sh -c /usr/bin/true && echo ADMITTED-RAN\n\
             sh -c 'sh -c /usr/bin/true' && echo ADMITTED-NESTED-RAN\n",
            denied = denied.display(),
            vetoed = vetoed.display(),
        );
        let out = run_confined(&env, &policy, &script).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("READY"),
            "the envelope did not launch: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        for ran in [
            "DENIED-RAN",
            "DENIED-NESTED-RAN",
            "VETOED-RAN",
            "VETOED-NESTED-RAN",
        ] {
            assert!(!emitted(&stdout, ran), "a blocked command ran: {stdout}");
        }
        for ran in ["ADMITTED-RAN", "ADMITTED-NESTED-RAN"] {
            assert!(
                emitted(&stdout, ran),
                "the layer denied an admitted sibling, so the blocks prove nothing: {stdout}"
            );
        }
    }

    /// An expanded directory is the tree as it stood at launch, so a program
    /// added there is denied until the next launch; a directory with no block
    /// beneath it is one hierarchy rule, which admits it at once.
    #[test]
    fn a_program_added_after_launch_runs_only_under_a_hierarchy_admit() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-added");
        let (expanded, whole) = (dir.join("expanded"), dir.join("whole"));
        for at in [&expanded, &whole] {
            std::fs::create_dir(at).expect("create an admitted dir");
        }
        let present = expanded.join("present");
        let blocked = expanded.join("blocked");
        for copy in [&present, &blocked] {
            std::fs::copy("/bin/true", copy).expect("copy /bin/true");
        }
        let policy = SandboxProjection {
            exec: admitting_less(
                &[&expanded.to_string_lossy(), &whole.to_string_lossy()],
                [file_rule(&crate::test_helper::real(&blocked), false)],
            ),
            ..restricted(&[], &[&expanded, &whole], &[])
        };
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             '{present}' && echo PRESENT-RAN\n\
             '{blocked}' && echo BLOCKED-RAN\n\
             cp /bin/true '{expanded}/added' && cp /bin/true '{whole}/added' || echo COPY-FAILED\n\
             '{expanded}/added' && echo EXPANDED-ADDED-RAN\n\
             '{whole}/added' && echo WHOLE-ADDED-RAN\n",
            present = present.display(),
            blocked = blocked.display(),
            expanded = expanded.display(),
            whole = whole.display(),
        );
        let out = run_confined(&env, &policy, &script).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("READY") && !emitted(&stdout, "COPY-FAILED"),
            "nothing was added, so nothing was tested: {stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            emitted(&stdout, "PRESENT-RAN"),
            "the expanded directory admitted nothing: {stdout}"
        );
        assert!(
            !emitted(&stdout, "BLOCKED-RAN"),
            "the block did not hold: {stdout}"
        );
        assert!(
            !emitted(&stdout, "EXPANDED-ADDED-RAN"),
            "a program added after launch ran in an expanded directory: {stdout}"
        );
        assert!(
            emitted(&stdout, "WHOLE-ADDED-RAN"),
            "a program added after launch did not run under a hierarchy admit: {stdout}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the grant lets run it lets read, as on macOS: a run place outside
    /// every fs prefix is shown, and without one it is not.
    #[test]
    fn a_run_place_is_readable_outside_every_fs_prefix() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-parity");
        let tools = dir.join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(tools.join("readme"), "PARITY-BYTES").unwrap();
        let elsewhere = workdir("exec-parity-elsewhere");
        let script = format!("echo READY\ncat '{}'\n", tools.join("readme").display());
        for (placed, exec) in [
            (true, admitting(&[&tools.to_string_lossy()], &[])),
            (false, admitting(&[], &[])),
        ] {
            let mut policy = SandboxProjection {
                exec,
                ..deny_within(&elsewhere, &[])
            };
            if let FsProjection::Restricted(rules) = &mut policy.fs {
                rules
                    .read
                    .extend(placed.then(|| FsAllow::RunPlace(crate::test_helper::real(&tools))));
            }
            let Some(env) = envelope_launches(&policy) else {
                return;
            };
            let out = run_confined(&env, &policy, &script).expect("spawn");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("READY"),
                "the envelope did not launch: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                stdout.contains("PARITY-BYTES"),
                placed,
                "a run place is shown exactly when the read region lists it: {stdout}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// A write into `dir/bin`, a write beside it in `dir`, and a run of the
    /// tool already in `dir/bin`, each marked on stdout; a refusal carries
    /// its reason.
    fn authoring(dir: &std::path::Path) -> String {
        format!(
            "echo READY\n\
             why=$(touch '{d}/bin/new' 2>&1) && echo ADMIT-WROTE || echo \"REFUSED: $why\"\n\
             touch '{d}/other' && echo PREFIX-WROTE\n\
             '{d}/bin/tool' && echo TOOL-RAN\n",
            d = dir.display()
        )
    }

    /// `authoring`'s output under `policy`, `None` where this host builds no
    /// envelope.
    fn authored(policy: &SandboxProjection, dir: &std::path::Path) -> Option<String> {
        let env = envelope_launches(policy)?;
        let out = run_confined(&env, policy, &authoring(dir)).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            stdout.contains("READY"),
            "the envelope did not launch: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            emitted(&stdout, "TOOL-RAN"),
            "the admit runs nothing: {stdout}"
        );
        Some(stdout)
    }

    /// Nothing is frozen: a write prefix that covers an admit without naming
    /// it lets a child author what the admit runs, the declared limit of a
    /// grant that writes where it runs, and takes the writes around it too.
    #[test]
    fn a_write_that_covers_an_admit_is_the_grants_to_give() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-unfrozen");
        let bin = dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::copy("/bin/true", bin.join("tool")).expect("copy /bin/true");
        let under = |write: &std::path::Path| SandboxProjection {
            exec: admitting(&[&bin.to_string_lossy()], &[]),
            ..deny_within(write, &[])
        };
        for write in [&dir, &bin] {
            let Some(wrote) = authored(&under(write), &dir) else {
                return;
            };
            assert!(
                emitted(&wrote, "ADMIT-WROTE"),
                "a write that reaches an admit was frozen out of it: {wrote}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_covered_file_is_as_writable_as_the_write_that_covers_it() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let tool = dir.join("tool");
        for named in [false, true] {
            std::fs::copy("/bin/true", &tool).unwrap();
            let mut writes = vec![dir.as_path()];
            writes.extend(named.then_some(tool.as_path()));
            let policy = SandboxProjection {
                exec: admitting(&[], &[&tool.to_string_lossy()]),
                ..restricted(&[], &writes, &[])
            };
            let Some(env) = envelope_launches(&policy) else {
                return;
            };
            let script = format!(
                "echo READY; echo x > '{t}' && echo REWRITTEN; echo y > '{d}/other' && echo SIBLING",
                t = tool.display(),
                d = dir.display()
            );
            let out = run_confined(&env, &policy, &script).unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("READY") && stdout.contains("SIBLING"),
                "{out:?}"
            );
            assert!(stdout.contains("REWRITTEN"), "named: {named}: {out:?}");
        }
    }

    /// Writes `dir/sibling` and `dir/script`, and returns the script's path.
    /// The script prints its `$0`, then the sibling it finds by `${0%/*}`, the
    /// lexical `dirname "$0"`, using builtins alone so that nothing else need
    /// be admitted.
    fn script_beside_sibling(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("script");
        let content = "#!/bin/sh\nprintf '%s\\n' \"$0\"; read -r l < \"${0%/*}/sibling\"; printf '%s\\n' \"$l\"\n";
        std::fs::write(&script, content).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("sibling"), "SIBLING\n").unwrap();
        script
    }

    /// The trampoline `execve`s the real path, so the kernel hands a `#!`
    /// script that path as `$0`, and the lexical idiom finds its siblings.
    #[test]
    fn a_confined_script_sees_its_real_path_and_its_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let script = script_beside_sibling(&dir);
        let program = admitted(Program::file(script.clone()).unwrap(), &[]);
        let policy = SandboxProjection {
            exec: admitting(&[], &[&script.to_string_lossy()]),
            ..restricted(&[&dir], &[], &[])
        };
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let out = run_admitted(&env, &policy, &program).unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("{}\nSIBLING\n", script.display())
        );
    }

    /// A deny masks the file with an unopenable node, which the kernel's
    /// `execve` refuses; `exec` names the real path.
    #[test]
    fn a_masked_script_does_not_run() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let script = script_beside_sibling(&dir);
        let program = admitted(Program::file(script.clone()).unwrap(), &[]);
        let policy = SandboxProjection {
            exec: admitting(&[], &[&script.to_string_lossy()]),
            ..restricted(&[&dir], &[], &[&script])
        };
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let out = run_admitted(&env, &policy, &program).unwrap();
        assert!(!out.status.success() && out.stdout.is_empty(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(&*script.to_string_lossy()),
            "{out:?}"
        );
    }

    /// Under an unrestricted fs, a veto in force or not, the tool the exec
    /// grant admits still runs, and the child still writes beside it: every
    /// write reaches the veto, so it is not walked.
    #[test]
    fn an_unrestricted_fs_with_a_veto_still_runs_what_the_exec_grant_admits() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-unrestricted-veto");
        let bin = dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::copy("/bin/true", bin.join("tool")).expect("copy /bin/true");
        for veto in [true, false] {
            let _ = std::fs::remove_file(bin.join("new"));
            let vetoes = veto.then(|| veto_rule("n"));
            let policy = exec_policy(admitting_less(&[&bin.to_string_lossy()], vetoes));
            let Some(stdout) = authored(&policy, &dir) else {
                return;
            };
            assert!(
                emitted(&stdout, "ADMIT-WROTE") && emitted(&stdout, "PREFIX-WROTE"),
                "veto: {veto}: {stdout}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A runnable copy of `/bin/true` at `dir/name`, which it creates.
    fn copy_true(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let copy = dir.join(name);
        std::fs::copy("/bin/true", &copy).expect("copy /bin/true");
        copy
    }

    /// `Any` under a restricted fs is a rule on every bind root, less the
    /// exclusions inside each: a program in a bind runs, the excluded one
    /// beside it does not, and an exclusion elsewhere changes neither.
    #[test]
    fn every_program_is_lowered_onto_each_bind_less_the_exclusions() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-any");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let bind = dir.join("w/deep/bind");
        let (prog, excluded) = (copy_true(&bind, "prog"), copy_true(&bind, "excluded"));
        let unrelated = copy_true(&dir.join("elsewhere"), "unrelated");
        let real = |p: &std::path::Path| crate::test_helper::real(p);
        let policy = SandboxProjection {
            exec: ExecProjection::Restricted(
                [
                    Rule::Allow(ExecAllow::Any),
                    file_rule(&real(&excluded), false),
                    file_rule(&real(&unrelated), false),
                ]
                .into_iter()
                .collect(),
            ),
            ..restricted(&[&bind], &[], &[])
        };
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             '{prog}' && echo PROG-RAN\n\
             '{excluded}' && echo EXCLUDED-RAN\n",
            prog = prog.display(),
            excluded = excluded.display(),
        );
        let out = run_confined(&env, &policy, &script).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            emitted(&stdout, "READY") && emitted(&stdout, "PROG-RAN"),
            "Any admitted nothing in the bind: {stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !emitted(&stdout, "EXCLUDED-RAN"),
            "an exclusion inside the bind did not hold: {stdout}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory admission reaches binds whose roots lie inside it, which a
    /// rule on the directory itself would never meet: an alias, a symlink to
    /// a subdirectory that the envelope binds at both names, and a canonical
    /// sub-bind under a grant that reads only that subdirectory.
    #[test]
    fn a_directory_admission_reaches_the_binds_inside_it() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-lowered");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (tools, sub) = (dir.join("tools"), dir.join("tools/sub"));
        copy_true(&sub, "prog");
        std::os::unix::fs::symlink("tools/sub", dir.join("alias")).unwrap();
        let exec = admitting(&[&tools.to_string_lossy()], &[]);
        for (label, read, run) in [
            ("alias", dir.join("alias"), dir.join("alias/prog")),
            ("sub-bind", sub.clone(), sub.join("prog")),
        ] {
            let policy = SandboxProjection {
                exec: exec.clone(),
                ..restricted(&[&read], &[], &[])
            };
            let Some(env) = envelope_launches(&policy) else {
                return;
            };
            let script = format!("echo READY\n'{}' && echo PROG-RAN\n", run.display());
            let out = run_confined(&env, &policy, &script).expect("spawn");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                emitted(&stdout, "READY") && emitted(&stdout, "PROG-RAN"),
                "{label}: the admission missed the bind: {stdout} {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A layer about exec must leave the two ordinary shapes alone: a dynamic
    /// binary, whose loader needs `Execute` of its own, and a `#!` script,
    /// which needs it on both the script and its interpreter.
    #[test]
    fn an_admitted_dynamic_binary_and_shebang_script_both_run() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = workdir("exec-shebang");
        let script_file = dir.join("hello.sh");
        std::fs::write(&script_file, "#!/bin/sh\necho SCRIPT-RAN\n").expect("write script");
        std::fs::set_permissions(&script_file, std::fs::Permissions::from_mode(0o755))
            .expect("make the script runnable");
        let policy = exec_policy(admitting(&[&dir.to_string_lossy()], &[]));
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             cat /dev/null && echo DYNAMIC-RAN\n\
             '{file}'\n",
            file = script_file.display(),
        );
        let out = run_confined(&env, &policy, &script).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        for ran in ["READY", "DYNAMIC-RAN", "SCRIPT-RAN"] {
            assert!(stdout.contains(ran), "missing {ran}: {stdout} {stderr}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A domain that does not *handle* `Refer` refuses every cross-directory
    /// rename with `EXDEV`, which a layer about exec must not do.  Only a
    /// spawn sees it, and only the inode does: `mv` turns that refusal into a
    /// copy and an unlink.
    #[test]
    fn a_cross_directory_rename_inside_the_write_prefix_still_works() {
        if !landlock_at_least(super::landlock::Abi::REFER) {
            return;
        }
        let dir = workdir("exec-refer");
        let (from, to) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("x"), "MOVED-BYTES").unwrap();
        let policy = exec_policy(admitting(&[], &[]));
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             set -- $(ls -i '{from}/x'); echo INODE-BEFORE=$1\n\
             mv '{from}/x' '{to}/x' && echo MOVED\n\
             set -- $(ls -i '{to}/x'); echo INODE-AFTER=$1\n",
            from = from.display(),
            to = to.display(),
        );
        let out = run_confined(&env, &policy, &script).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            emitted(&stdout, "MOVED"),
            "the exec layer refused a rename it has no business refusing: {stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(to.join("x").exists(), "the rename did not land on the host");
        let before = field(&stdout, "INODE-BEFORE=").expect("an inode before the move");
        assert_eq!(
            field(&stdout, "INODE-AFTER="),
            Some(before),
            "`mv` fell back to copy-and-unlink, so the rename itself was refused: {stdout}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/tmp` is a confined child's own writable ground on every projection,
    /// so nothing but the exec layer stops it dropping a binary there and
    /// running it.  Both halves are asserted: an admit that changes nothing
    /// would read as a pass against half of this.
    #[test]
    fn a_binary_written_to_tmp_runs_only_where_tmp_is_admitted() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-tmp");
        let planted = dir.join("planted");
        let script = format!(
            "echo READY\n\
             cp /bin/true '{planted}' || echo COPY-FAILED\n\
             '{planted}' && echo TMP-RAN\n",
            planted = planted.display(),
        );
        let script = script.as_str();
        for (admits_tmp, dirs) in [(false, &[][..]), (true, &["/tmp"][..])] {
            let policy = exec_policy(admitting(dirs, &[]));
            let Some(env) = envelope_launches(&policy) else {
                return;
            };
            let out = run_confined(&env, &policy, script).expect("spawn");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("READY") && !emitted(&stdout, "COPY-FAILED"),
                "the drop into /tmp did not happen, so nothing was tested: {stdout} {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                emitted(&stdout, "TMP-RAN"),
                admits_tmp,
                "/tmp exec must follow the admits alone: {stdout}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The scope rides every launch, whatever the exec axis says: without it a
    /// confined child signals same-uid host processes wherever the host cannot
    /// build a pid namespace.  Which errno the refusal carries is the kernel's
    /// business — `ESRCH` behind a pid namespace, `EPERM` without — so only
    /// the refusal is asserted.
    #[test]
    fn a_confined_child_cannot_signal_a_host_process() {
        if !landlock_at_least(super::landlock::Abi::SIGNAL_SCOPE) {
            return;
        }
        let dir = workdir("exec-scope");
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a host-side sleeper");
        let script = format!(
            "echo READY\n\
             kill -0 {pid} 2>/dev/null && echo SLEEPER-VISIBLE\n\
             kill -TERM {pid} 2>/dev/null && echo SLEEPER-SIGNALLED\n",
            pid = sleeper.id(),
        );
        let bare = ExecProjection::Restricted(
            [Rule::Allow(ExecAllow::Any), veto_rule("rm")]
                .into_iter()
                .collect(),
        );
        for exec in [ExecProjection::Unrestricted, admitting(&[], &[]), bare] {
            let policy = exec_policy(exec);
            let Some(env) = envelope_launches(&policy) else {
                break;
            };
            let out = run_confined(&env, &policy, &script).expect("spawn");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("READY"),
                "the envelope did not launch: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                !emitted(&stdout, "SLEEPER-VISIBLE") && !emitted(&stdout, "SLEEPER-SIGNALLED"),
                "a host process was reachable from inside the envelope: {stdout}"
            );
            assert!(
                sleeper.try_wait().expect("poll the sleeper").is_none(),
                "the host-side sleeper was killed from inside the envelope"
            );
        }
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing is tightened that the projection did not ask for.
    #[test]
    fn an_unrestricted_exec_projection_leaves_every_binary_runnable() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-open");
        let copy = planted_binary(&dir);
        let policy = exec_policy(ExecProjection::Unrestricted);
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n'{copy}' && echo PLANTED-RAN\n",
            copy = copy.display()
        );
        let out = run_confined(&env, &policy, &script).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            emitted(&stdout, "PLANTED-RAN"),
            "an unrestricted exec projection must confine no exec at all: {stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ral's binary is an ordinary file: it runs where the table admits it,
    /// by path or by directory, and nowhere else, a byte-identical copy
    /// beside it included.
    #[test]
    fn ral_runs_only_where_the_table_admits_it() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let own = crate::authority::sandbox::reexec::own().expect("ral pins itself");
        let dir = workdir("exec-self");
        let copy = dir.join("ral-copy");
        std::fs::copy(own.arg0(), &copy).expect("copy ral");
        let own_path = own.arg0().to_string_lossy().into_owned();
        let script = format!(
            "echo READY\n\
             '{own}' --list >/dev/null && echo SELF-RAN\n\
             '{copy}' --list >/dev/null && echo COPY-RAN\n",
            own = own.arg0().display(),
            copy = copy.display(),
        );
        for (label, exec, self_ran) in [
            ("unnamed", admitting(&[], &[]), false),
            ("by path", admitting(&[], &[&own_path]), true),
        ] {
            let policy = exec_policy(exec);
            let Some(env) = envelope_launches(&policy) else {
                return;
            };
            let out = run_confined(&env, &policy, &script).expect("spawn");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(emitted(&stdout, "READY"), "{label}: {stdout}");
            assert_eq!(emitted(&stdout, "SELF-RAN"), self_ran, "{label}: {stdout}");
            assert!(
                !emitted(&stdout, "COPY-RAN"),
                "{label}: a copy of ral ran, so the admit is not its file: {stdout}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The stack as a launch folds it: exec `allow`ed and `deny`ed by path,
    /// each frozen, a directory when it is one, beside the system's command
    /// directories; `fs` as given.  The program is admitted under it.
    fn folded(
        exec: Option<(&[&std::path::Path], &[&std::path::Path])>,
        fs: Option<FsPolicy>,
        program: &std::path::Path,
        args: &[&str],
    ) -> (SandboxProjection, Admitted) {
        use crate::authority::policy::ExecKey;
        let key = |p: &std::path::Path| {
            let frozen = FrozenPath::from_surface(crate::path::utf8(p).unwrap());
            if p.is_dir() {
                ExecKey::Dir(frozen)
            } else {
                ExecKey::Path(frozen)
            }
        };
        let exec = exec.map(|(allow, deny)| {
            ["/bin", "/usr/bin"]
                .iter()
                .map(|d| key(d.as_ref()))
                .chain(allow.iter().map(|p| key(p)))
                .map(|k| (k, Evidence::ALLOW))
                .chain(deny.iter().map(|p| (key(p), Evidence::DENY)))
                .collect()
        });
        let stack = crate::test_helper::stack([crate::test_helper::layer(Capabilities {
            exec,
            fs,
            ..Capabilities::default()
        })]);
        let baseline = host_system_reads();
        let program = Program::file(program.to_path_buf()).expect("the program exists");
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let admitted = stack
            .admit(&baseline, program, args)
            .unwrap_or_else(|why| panic!("the stack refuses the program: {why:?}"));
        let projection = SandboxProjection::of(&stack, &baseline, Some(&admitted))
            .expect("a restricted stack projects");
        (projection, admitted)
    }

    fn exec_files(projection: &SandboxProjection) -> Vec<RealPath> {
        match &projection.exec {
            ExecProjection::Restricted(rules) => rules.allowed_files().cloned().collect(),
            ExecProjection::Unrestricted => Vec::new(),
        }
    }

    /// An exec exclusion on a linker candidate beats the support that would
    /// admit it; the same grant without it admits the linker.
    #[test]
    fn an_excluded_linker_candidate_is_not_admitted() {
        let Some(linker) = crate::authority::sandbox::carriers::linkers()
            .into_iter()
            .next()
        else {
            eprintln!("skipping: this host has no dynamic linker candidate");
            return;
        };
        let sh = std::path::Path::new("/bin/sh");
        let (open, _) = folded(Some((&[], &[])), None, sh, &[]);
        assert!(
            exec_files(&open).contains(&linker),
            "support admits the linker unless excluded: {linker}"
        );
        let (shut, _) = folded(Some((&[], &[linker.as_path()])), None, sh, &[]);
        assert!(
            !exec_files(&shut).contains(&linker),
            "an excluded linker candidate was admitted: {linker}"
        );
    }

    /// An fs exclusion on a linker candidate beats the support that would
    /// admit it, as an exec exclusion does.
    #[test]
    fn an_fs_excluded_linker_candidate_is_not_admitted() {
        let Some(linker) = crate::authority::sandbox::carriers::linkers()
            .into_iter()
            .next()
        else {
            eprintln!("skipping: this host has no dynamic linker candidate");
            return;
        };
        let sh = std::path::Path::new("/bin/sh");
        let fs = |deny: Vec<FrozenPath>| FsPolicy {
            read_prefixes: vec![FrozenPath::from_surface(
                crate::path::utf8(std::path::Path::new("/")).unwrap(),
            )],
            write_prefixes: Vec::new(),
            deny_paths: deny,
        };
        let (open, _) = folded(Some((&[], &[])), Some(fs(Vec::new())), sh, &[]);
        assert!(
            exec_files(&open).contains(&linker),
            "support admits the linker unless excluded: {linker}"
        );
        let denied = fs(vec![FrozenPath::from_surface(
            crate::path::utf8(linker.as_path()).unwrap(),
        )]);
        let (shut, _) = folded(Some((&[], &[])), Some(denied), sh, &[]);
        assert!(
            !exec_files(&shut).contains(&linker),
            "an fs-excluded linker candidate was admitted: {linker}"
        );
    }

    /// An exec exclusion on a script's interpreter keeps it out of the read
    /// region's places, where support would otherwise carry it.
    #[test]
    fn an_exec_excluded_carrier_is_not_read() {
        let Some((script, carrier)) = crate::authority::sandbox::carriers::system_script() else {
            eprintln!("skipping: this host has no system script");
            return;
        };
        let reads = || FsPolicy {
            read_prefixes: vec![FrozenPath::from_surface(
                crate::path::utf8(&std::env::temp_dir()).unwrap(),
            )],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        };
        let carried = |deny: &[&std::path::Path]| {
            let (policy, _) = folded(Some((&[], deny)), Some(reads()), script.as_path(), &[]);
            let rules = policy.fs.rules().expect("a restricted fs").read.clone();
            rules.contains(&FsAllow::RunPlace(carrier.clone()))
        };
        assert!(carried(&[]), "support carries the interpreter: {carrier}");
        assert!(
            !carried(&[carrier.as_path()]),
            "an exec-excluded carrier was read: {carrier}"
        );
    }

    /// A `#!` script at `dir/script` whose interpreter, a copy of the shell,
    /// is `dir/interp`.
    fn scripted(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let interp = dir.join("interp");
        std::fs::copy("/bin/sh", &interp).expect("copy /bin/sh");
        let script = dir.join("script");
        std::fs::write(
            &script,
            format!("#!{}\necho INTERP-RAN\n", interp.display()),
        )
        .expect("write the script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make the script runnable");
        (script, interp)
    }

    /// A script whose interpreter is excluded, by exec or by fs, fails in the
    /// kernel, and the hint names the exclusion rather than a missing admit.
    #[test]
    fn a_script_whose_interpreter_is_excluded_fails_in_the_kernel_naming_it() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-interp-excluded");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (script, interp) = scripted(&dir);
        let fs = FsPolicy {
            read_prefixes: vec![FrozenPath::from_surface(crate::path::utf8(&dir).unwrap())],
            write_prefixes: Vec::new(),
            deny_paths: vec![FrozenPath::from_surface(
                crate::path::utf8(&interp).unwrap(),
            )],
        };
        for (axis, exec, fs) in [
            (
                "exec",
                Some((&[dir.as_path()][..], &[interp.as_path()][..])),
                None,
            ),
            ("fs", Some((&[dir.as_path()][..], &[][..])), Some(fs)),
        ] {
            let (policy, admitted) = folded(exec, fs, &script, &[]);
            let Some(env) = envelope_launches(&policy) else {
                return;
            };
            let out = run_admitted(&env, &policy, &admitted).expect("spawn");
            let (stdout, stderr) = (
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
            assert!(
                !stdout.contains("INTERP-RAN") && !out.status.success(),
                "{axis}: an excluded interpreter ran: {stdout} {stderr}"
            );
            assert!(
                stderr.contains("interpreter") && stderr.contains("is excluded by"),
                "{axis}: the hint does not name the exclusion: {stderr}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A trusted script starts under a restricted fs whose reads name neither
    /// it nor its interpreter, with exec supplied and omitted.
    #[test]
    fn a_trusted_script_starts_through_live_supports_reads() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let Some((script, _)) = crate::authority::sandbox::carriers::system_script() else {
            eprintln!("skipping: this host has no system script");
            return;
        };
        let dir = workdir("trusted-script");
        let reads = || FsPolicy {
            read_prefixes: vec![FrozenPath::from_surface(crate::path::utf8(&dir).unwrap())],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        };
        // The trampoline's refusal of the launch, the script's or its
        // interpreter's, unread or unadmitted; the script's children are its own.
        let refused = format!("ral: {script}:");
        for (label, exec) in [
            ("exec supplied", Some((&[script.as_path()][..], &[][..]))),
            ("exec omitted", None),
        ] {
            let (policy, admitted) = folded(exec, Some(reads()), script.as_path(), &["--help"]);
            let Some(env) = envelope_launches(&policy) else {
                return;
            };
            let out = run_admitted(&env, &policy, &admitted).expect("spawn");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(!stderr.contains(&refused), "{label}: {stderr}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exec policies a descendant meets ral under, each with whether ral
    /// starts: admitted by path, not admitted, its folder excluded.
    fn ral_exec_policies(work: &std::path::Path) -> [(&'static str, SandboxProjection, bool); 3] {
        let own = crate::authority::sandbox::reexec::own().expect("ral pins itself");
        let home = own.arg0().parent().expect("ral has a folder");
        let own_path = own.arg0().to_string_lossy().into_owned();
        let under = |exec, deny: &[&std::path::Path]| SandboxProjection {
            exec,
            ..deny_within(work, deny)
        };
        [
            ("admitted", under(admitting(&[], &[&own_path]), &[]), true),
            ("unadmitted", under(admitting(&[], &[]), &[]), false),
            (
                "excluded folder",
                under(admitting(&[&home.to_string_lossy()], &[]), &[home]),
                false,
            ),
        ]
    }

    /// A descendant starts ral neither under an exec allow-list that does not
    /// admit it, nor by name where its folder is fs-excluded, nor as
    /// `/proc/self/exe`.
    #[test]
    fn a_descendant_cannot_start_ral() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let own = crate::authority::sandbox::reexec::own().expect("ral pins itself");
        let home = own.arg0().parent().expect("ral has a folder");
        let name = own.arg0().file_name().expect("ral has a name");
        let work = workdir("exec-ral");
        let script = format!(
            "echo READY\n\
             PATH='{home}' '{name}' --list >/dev/null && echo SELF-RAN\n\
             env /proc/self/exe -c true && echo PROC-RAN\n",
            home = home.display(),
            name = name.to_string_lossy(),
        );
        for (label, policy, self_ran) in ral_exec_policies(&work) {
            let Some(env) = envelope_launches(&policy) else {
                return;
            };
            let out = run_confined(&env, &policy, &script).expect("spawn");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(emitted(&stdout, "READY"), "{label}: {stdout}");
            assert_eq!(emitted(&stdout, "SELF-RAN"), self_ran, "{label}: {stdout}");
            assert!(!emitted(&stdout, "PROC-RAN"), "{label}: {stdout}");
        }
        let _ = std::fs::remove_dir_all(&work);
    }

    /// Under the same policies a bundled tool, run by the launcher, still runs.
    #[cfg(feature = "coreutils")]
    #[cfg_attr(
        ctor_without_argv,
        ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
    )]
    #[test]
    fn a_bundled_tool_runs_where_a_descendant_cannot_start_ral() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let work = workdir("exec-ral-tool");
        let tool = admitted(
            Program::Tool("basename".into()),
            &["/x/TOOL-RAN".to_string()],
        );
        for (label, policy, _) in ral_exec_policies(&work) {
            let Some(env) = envelope_launches(&policy) else {
                return;
            };
            let out = run_admitted(&env, &policy, &tool).expect("spawn");
            assert!(
                emitted(&String::from_utf8_lossy(&out.stdout), "TOOL-RAN"),
                "{label}: a bundled tool did not run: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&work);
    }

    /// A hard link made before launch at an excluded path to an admitted file
    /// runs: Landlock admits inodes, and the exclusion names a path (§7.6).
    #[test]
    fn a_preexisting_hard_link_at_an_excluded_path_runs() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-hard-link");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let admitted_file = copy_true(&dir, "admitted");
        let linked = dir.join("linked");
        std::fs::hard_link(&admitted_file, &linked).expect("hard link");
        let planted = copy_true(&dir, "planted");
        let policy = exec_policy(admitting_less(
            &[],
            [
                file_rule(&crate::test_helper::real(&admitted_file), true),
                file_rule(&crate::test_helper::real(&linked), false),
            ],
        ));
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             '{linked}' && echo LINK-RAN\n\
             '{planted}' && echo PLANTED-RAN\n",
            linked = linked.display(),
            planted = planted.display(),
        );
        let out = run_confined(&env, &policy, &script).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(emitted(&stdout, "READY"), "{stdout}");
        assert!(
            emitted(&stdout, "LINK-RAN"),
            "the declared limit moved: {stdout}"
        );
        assert!(
            !emitted(&stdout, "PLANTED-RAN"),
            "an unadmitted copy ran, so the link proves nothing: {stdout}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Refused while building, before bwrap is ever spawned: the stand-in
    /// envelope would run this test binary if it were.
    #[test]
    fn an_exec_restricting_launch_without_landlock_spawns_nothing() {
        let host = HostEnvelope {
            landlock: super::landlock::Landlock::Absent,
            ..WHOLE
        };
        let sh = Program::file("/bin/sh".into()).expect("/bin/sh exists");
        let launch = enveloped(
            &on(&stand_in(), host),
            &exec_policy(admitting(&[], &[])),
            &Baseline::default(),
            &admitted(sh, &["-c".to_string(), "echo RAN".to_string()]),
            None,
            Ownership::Kept,
        );
        let Err(e) = launch else {
            panic!("a restricted exec grant needs Landlock");
        };
        assert!(e.to_string().contains("Landlock"), "{e}");
    }

    /// A warrant promising a ruleset is the trampoline's whole word on it, so
    /// one that never arrived — at a slot the launch held but never handed
    /// down — is a launch it refuses, not one it runs open.
    #[test]
    fn a_promised_ruleset_that_never_arrived_runs_nothing() {
        use crate::authority::sandbox::warrant::Warrant;
        use std::os::fd::AsFd;
        let own = crate::authority::sandbox::reexec::own().expect("ral pins itself");
        let sh = Program::file("/bin/sh".into()).expect("/bin/sh exists");
        let null = std::fs::File::open("/dev/null").expect("open /dev/null");
        let mut withheld = Handoff::default();
        let at = withheld.lend(null.as_fd()).expect("lends");
        let promise = Some(super::landlock::Landlocked {
            refer_root: false,
            at,
        });
        let parcel = Warrant::new(
            promise,
            &admitted(sh, &["-c".to_string(), "echo RAN".to_string()]),
            &unrestricted(),
        )
        .parcel()
        .expect("parcels");
        let mut handoff = Handoff::default();
        let warrant = handoff.lend(parcel.as_fd()).expect("lends");
        let mut cmd = own.command();
        cmd.arg(Role::Warrant.flag())
            .arg(warrant.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        handoff.install(&mut cmd);
        let out = cmd.output().expect("spawn the trampoline");
        drop(withheld);
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(out.status.code(), Some(126), "{stdout} {stderr}");
        assert!(!emitted(&stdout, "RAN"), "the program ran unconfined");
        assert!(stderr.contains(&format!("fd {at}")), "{stderr}");
    }

    // ── The trampoline by descriptor ──────────────────────────────────────

    /// The bind table of a launch whose trampoline is `own`.
    fn over(own: &Pinned) -> Vec<String> {
        let bwrap = stand_in();
        let env = Envelope {
            trampoline: own,
            ..on(&bwrap, WHOLE)
        };
        let binds = Binds::open(&env, plan(&unrestricted()), &mut Handoff::default())
            .expect("the binds open");
        (binds.layer(Layer::Over))
            .map(|(_, bind)| bind.dest.as_str().to_string())
            .collect()
    }

    /// Whether `pin`, unlinked, says so through its descriptor: FUSE (rootless
    /// podman's fuse-overlayfs) keeps the link count it cached, so a pin there
    /// never loses its name and an unlink cannot be tested on it.
    fn unlinked(pin: &Pinned) -> bool {
        let unlinked = pin.names().expect("read the pin").is_none();
        if !unlinked {
            eprintln!(
                "skipping: the filesystem under {} hides an unlink from an open descriptor",
                std::env::temp_dir().display()
            );
        }
        unlinked
    }

    /// A pin is locked at the name its inode holds now, never the one it was
    /// pinned at; unlinked, it has none, and nothing is bound for it.
    #[test]
    fn a_pin_is_locked_at_its_current_name_and_nowhere_once_unlinked() {
        let dir = workdir("pin-names");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let (pinned_at, moved) = (dir.join("pinned"), dir.join("moved"));
        std::fs::write(&pinned_at, "x").unwrap();
        let pin = Pinned::open(pinned_at.clone()).expect("the file pins");
        std::fs::rename(&pinned_at, &moved).unwrap();
        assert_eq!(pin.names().expect("read the pin"), Some(moved.clone()));
        let locked = over(&pin);
        let name = |p: &std::path::Path| p.to_string_lossy().into_owned();
        assert!(locked.contains(&name(&moved)), "{locked:?}");
        assert!(!locked.contains(&name(&pinned_at)), "{locked:?}");
        std::fs::remove_file(&moved).unwrap();
        if unlinked(&pin) {
            assert_eq!(
                over(&pin),
                over(&stand_in()),
                "an unlinked pin binds nothing"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A copy of this test binary in `dir`, pinned as a trampoline.
    fn pinned_copy(dir: &std::path::Path) -> (std::path::PathBuf, Pinned) {
        let copy = dir.join("ral");
        std::fs::copy(std::env::current_exe().expect("own path"), &copy).expect("copy ral");
        let pin = Pinned::open(copy.clone()).expect("the copy pins");
        (copy, pin)
    }

    /// [`pinned_copy`], swapped as an atomic `cargo install` swaps it: an
    /// impostor renamed over its name, the pinned inode left with none.
    fn swapped_trampoline(dir: &std::path::Path) -> (std::path::PathBuf, Pinned) {
        use std::os::unix::fs::PermissionsExt;
        let (copy, pin) = pinned_copy(dir);
        let impostor = dir.join("impostor");
        std::fs::write(&impostor, "#!/bin/sh\necho IMPOSTOR\n").unwrap();
        std::fs::set_permissions(&impostor, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&impostor, &copy).unwrap();
        (copy, pin)
    }

    /// [`run_admitted`] with `own` as the trampoline in place of ral's own
    /// pin; the exec projection must be unrestricted, as nothing admits
    /// `own`.
    fn run_trampoline(
        env: &Envelope<'_>,
        own: &Pinned,
        policy: &SandboxProjection,
        admitted: &Admitted,
    ) -> Option<std::process::Output> {
        use crate::authority::sandbox::warrant::Warrant;
        use std::os::fd::AsFd;
        let parcel = Warrant::new(None, admitted, policy)
            .parcel()
            .expect("parcels");
        let mut handoff = Handoff::default();
        let warrant = handoff.lend(parcel.as_fd()).expect("lends");
        let rendered = policy.rendered().expect("renders");
        let env = Envelope {
            trampoline: own,
            ..*env
        };
        let (mut cmd, info_fd) = bwrap_command(
            &env,
            plan(policy),
            handoff,
            warrant,
            &rendered,
            None,
            Ownership::Kept,
        )
        .expect("the launch builds");
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().ok();
        drop(info_fd);
        out
    }

    /// Swapped on disk after its pin, the trampoline still runs the pinned
    /// bytes, with no name left to lock: bwrap execs the inode by descriptor,
    /// and nothing on Linux re-stats the name to refuse the launch first.
    /// Needs a filesystem that execs an unlinked file (M16: not virtiofs).
    #[test]
    fn a_trampoline_swapped_by_rename_still_runs_the_pinned_bytes() {
        let policy = unrestricted();
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let dir = workdir("trampoline-swap");
        let (_, own) = swapped_trampoline(&dir);
        if unlinked(&own) {
            let out = run_trampoline(&env, &own, &policy, &sh_c("echo READY")).expect("spawn");
            let (stdout, stderr) = (
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
            assert!(emitted(&stdout, "READY"), "{stdout} {stderr}");
            assert!(!stdout.contains("IMPOSTOR"), "the name ran: {stdout}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Inside, the trampoline's `/proc/self/exe` is the pinned inode, which
    /// after the swap has no name but the one it lost: a bundled tool runs in
    /// the trampoline itself.
    #[cfg(feature = "coreutils")]
    #[cfg_attr(
        ctor_without_argv,
        ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
    )]
    #[test]
    fn the_trampoline_inside_is_the_pinned_inode() {
        let policy = unrestricted();
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let dir = workdir("trampoline-exe");
        let (copy, own) = swapped_trampoline(&dir);
        if unlinked(&own) {
            let readlink = admitted(
                Program::Tool("readlink".into()),
                &["/proc/self/exe".to_string()],
            );
            let out = run_trampoline(&env, &own, &policy, &readlink).expect("spawn");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim_end(),
                format!("{} (deleted)", copy.display()),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Inside a writable prefix holding it, the trampoline's file is the one
    /// name a confined child can neither write, link, rename nor unlink, and
    /// the host's bytes are intact after; a sibling stays writable.
    #[test]
    fn a_pinned_binary_inside_a_writable_prefix_is_locked() {
        let dir = workdir("pin-locked");
        let dir = std::fs::canonicalize(&dir).expect("the work dir is real");
        let policy = deny_within(&dir, &[]);
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let (ral, own) = pinned_copy(&dir);
        let script = format!(
            "cd '{dir}'\n\
             printf x > ral && echo WRITE-DONE\n\
             ln ral linked && echo LINK-DONE\n\
             mv ral moved && echo RENAME-DONE\n\
             rm -f ral && echo UNLINK-DONE\n\
             touch sibling && echo SIBLING-DONE\n",
            dir = dir.display()
        );
        let out = run_trampoline(&env, &own, &policy, &sh_c(&script)).expect("spawn");
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(emitted(&stdout, "SIBLING-DONE"), "{stdout} {stderr}");
        for refused in ["WRITE-DONE", "LINK-DONE", "RENAME-DONE", "UNLINK-DONE"] {
            assert!(!emitted(&stdout, refused), "{refused}: {stdout} {stderr}");
        }
        let exe = std::env::current_exe().expect("own path");
        assert!(
            std::fs::read(&ral).expect("still there") == std::fs::read(exe).expect("read self"),
            "the pinned bytes changed on the host"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── The seccomp deny-set ──────────────────────────────────────────────
    //
    // The filter is projection-independent (`seccomp::Filter::ENVELOPE` is
    // keyed on nothing in the grant), so the open exec projection is the
    // right one to prove it on — these tests are not about exec at all.

    fn on_path(tool: &str) -> bool {
        let path = std::env::var("PATH").unwrap_or_default();
        crate::path::which::locate(tool, Some(&path), None).is_some()
    }

    /// `RC=159` is `128 + SIGSYS`: an `EPERM` here (`RC=32`, the errno for a
    /// missing `CAP_SYS_ADMIN`) would mean the filter never ran and only the
    /// lack of capability refused the mount.  Requires `mount`.
    #[test]
    fn a_mount_inside_the_envelope_is_killed() {
        if !on_path("mount") {
            eprintln!("skipping: no `mount` on this host's PATH");
            return;
        }
        let policy = exec_policy(ExecProjection::Unrestricted);
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let out = run_confined(
            &env,
            &policy,
            "mount -t tmpfs none /tmp 2>/dev/null; echo RC=$?",
        )
        .expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            field(&stdout, "RC="),
            Some("159"),
            "mount inside the envelope must be killed with SIGSYS, not merely denied: {stdout}"
        );
    }

    /// A user namespace is refused with `EPERM`, not killed: `unshare`'s own
    /// self-check must print "Operation not permitted", and the exit must be
    /// neither a clean `0` nor a `159` SIGSYS. Requires `unshare` (util-linux).
    #[test]
    fn a_user_namespace_is_refused_not_killed() {
        if !on_path("unshare") {
            eprintln!("skipping: no `unshare` on this host's PATH");
            return;
        }
        let policy = exec_policy(ExecProjection::Unrestricted);
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let out =
            run_confined(&env, &policy, "unshare -U true 2>&1; echo RC=$?").expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("Operation not permitted"),
            "unshare -U must fail with EPERM, worded by unshare itself: {stdout}"
        );
        let rc = field(&stdout, "RC=");
        assert!(
            rc.is_some_and(|rc| rc != "0" && rc != "159"),
            "a user namespace must be refused, neither allowed nor killed: {stdout}"
        );
    }

    /// `strace` opens with `PTRACE_TRACEME`/`ptrace`, killed with SIGSYS; then
    /// `describe_denial` must name it from the very syscall number the kernel
    /// logged. Requires `strace`.
    #[test]
    fn a_confined_ptrace_is_killed_and_named() {
        if !on_path("strace") {
            eprintln!("skipping: no `strace` on this host's PATH");
            return;
        }
        let policy = exec_policy(ExecProjection::Unrestricted);
        let Some(env) = envelope_launches(&policy) else {
            return;
        };
        let out = run_confined(
            &env,
            &policy,
            "strace -o /dev/null true 2>/dev/null; echo RC=$?",
        )
        .expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            field(&stdout, "RC="),
            Some("159"),
            "a confined ptrace must be killed with SIGSYS: {stdout}"
        );

        let described = crate::authority::sandbox::diag::platform::describe_denial(
            &libc::SYS_ptrace.to_string(),
        );
        assert!(
            described.is_some_and(|d| d.contains("ptrace")),
            "describe_denial must name ptrace from its syscall number"
        );
    }
}
