//! Linux sandbox: the bubblewrap (`bwrap`) options that confine one external
//! child.  bwrap is never the payload: its monitor clones a namespace init
//! which forks the payload, whose `setsid` (`--new-session`) makes its pid the
//! group of everything inside — the group ral addresses, read over
//! `--info-fd`; the monitor leads an inert group nobody signals.  The
//! envelope is the child's whole world on every projection: own ipc and uts
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

pub(crate) use host::HostEnvelope;

use super::launch::Ownership;
use super::reexec::Pinned;
use super::warrant::{Handoff, Slot, nul_terminated, parcel};
use crate::capability::ExecRules;
use crate::path::{
    Object, PathShape, RealPath, Rendered, render_objects, render_paths, render_real,
};
use crate::types::{ExecProjection, FsProjection, SandboxProjection};
use rustix::fs::OFlags;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::OnceLock;

/// The bubblewrap binary's name: what [`pin_envelope`] walks `PATH` for,
/// and what a message calls it.  Never handed to `exec` — every launch goes
/// through the [`Pinned`] envelope.
pub(super) const BWRAP: &str = "bwrap";

static ENVELOPE: OnceLock<Pinned> = OnceLock::new();

/// Pin bwrap for the rest of the process's life, found on the `PATH` this
/// process was started with — never a shell's, none exists yet — and only in
/// its absolute entries, each valid UTF-8 or skipped: the search takes a
/// string.  Idempotent, and silent where bwrap is absent: [`envelope`] says
/// so at the first launch that needs it.
pub(super) fn pin_envelope() {
    if ENVELOPE.get().is_some() {
        return;
    }
    #[allow(clippy::disallowed_methods, reason = "PATH, not an XDG basedir")]
    let host_path = std::env::var_os("PATH").unwrap_or_default();
    let absolute: Vec<String> = std::env::split_paths(&host_path)
        .filter_map(|entry| entry.into_os_string().into_string().ok())
        .filter(|entry| crate::path::is_absolute(entry))
        .collect();
    let located = crate::path::which::locate(
        BWRAP,
        Some(&absolute.join(":")),
        crate::path::which::SearchCwd::nowhere(),
    );
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

/// The [`Command`] that runs `own`'s trampoline confined under `policy`:
/// `bwrap --args 98 -- <own> --warrant`, the options in a sealed parcel at
/// their slot beside the seccomp programs, every bind's handle, the
/// `--info-fd` peer and the caller's `handoff` (the warrant and Landlock
/// ruleset), every descriptor through one [`Handoff`].  bwrap takes options
/// alone from `--args`, so the payload rides its argv.  `image` is the host
/// file the trampoline execs in turn, shown read-only.
///
/// The second element of the return is `Kept`'s `--info-fd` peer — see
/// [`InfoFd`].  Refused under a bwrap that cannot mount by descriptor, and
/// where the envelope's pin sits on one of the slots.
#[allow(
    clippy::too_many_arguments,
    reason = "the launch's whole input: two pins, the image, the handoff, the projection and three envelope facts"
)]
pub(crate) fn bwrap_command(
    envelope: &Pinned,
    own: &Pinned,
    image: Option<&RealPath>,
    handoff: Handoff<'_>,
    policy: &SandboxProjection<Rendered>,
    chdir: Option<&str>,
    ownership: Ownership,
    host: HostEnvelope,
) -> Result<(Command, Option<InfoFd>), String> {
    if !host.binds_by_fd {
        return Err(super::confinement_unavailable(&format!(
            "bwrap at {} does not take --ro-bind-fd (bubblewrap 0.8.0 or newer); ral mounts \
             only by descriptor, so no confined command can run under it",
            envelope.arg0().display()
        ))
        .message);
    }
    let binds = Binds::open(image, policy, host)?;
    let programs = seccomp_programs()?;
    let options = bwrap_argv(
        envelope,
        policy,
        chdir,
        ownership,
        host,
        &binds,
        programs.len(),
    )?;
    let parcel = |name: &str, bytes: &[u8]| {
        parcel(name, bytes).map_err(|e| format!("sandbox: cannot parcel {name}: {e}"))
    };
    let args = parcel(
        "ral-bwrap-args",
        &nul_terminated(options.iter().map(|arg| arg.as_bytes()))?,
    )?;
    let seccomp = programs
        .into_iter()
        .map(|program| parcel("ral-seccomp", program))
        .collect::<Result<Vec<_>, _>>()?;
    // Rebound, so it may borrow this frame's parcels and handles.
    let mut handoff = handoff;
    handoff.lend(Slot::Args, args.as_fd());
    for (i, program) in seccomp.iter().enumerate() {
        handoff.lend(Slot::seccomp(i)?, program.as_fd());
    }
    binds.lend(&mut handoff);
    let kept = ownership == Ownership::Kept;
    // ral's own pin needs no such check: bwrap execs the trampoline by name.
    let pin = envelope.raw_fd();
    let info = kept.then_some(Slot::Info.fd());
    if handoff.targets().chain(info).any(|at| at == pin) {
        return Err(format!(
            "sandbox: ral's pinned bwrap sits on descriptor {pin}, a slot the confined \
             launch hands down; refusing rather than exec what lands there (was ral \
             started with that many files open?)"
        ));
    }
    let mut c = envelope.command();
    let slot = Slot::Args.fd().to_string();
    c.args(["--args", &slot, "--"])
        .arg(own.arg0())
        .arg(super::WARRANT_FLAG);
    if !kept {
        handoff.install(&mut c)?;
        return Ok((c, None));
    }
    let (reader, writer) = crate::process::cloexec_socketpair()
        .map_err(|e| format!("sandbox: cannot open bwrap's --info-fd: {e}"))?;
    let (reader, writer) = handoff.install_addressed(&mut c, (reader, writer.into()))?;
    Ok((c, Some(InfoFd { reader, writer })))
}

/// The bwrap options that confine a payload under `policy`: `binds` by
/// slot, layer by layer, `deny_paths` masked last.  Descriptors appear by
/// slot number alone, one `--add-seccomp-fd` per program of `seccomp`;
/// [`bwrap_command`] lends them.
///
/// Every name is taken from `policy`, rendered, so a rule lands on each
/// spelling the kernel might present rather than on the one the grant author
/// happened to write — a deny naming a symlink masks the target the resolved
/// twin names.  `policy.exec` reaches the options only as the binds that show
/// what it admits: bwrap filters mounts and syscalls, not exec by path, so
/// [`landlock`] builds its ruleset in the host, and the payload enters it
/// *inside* this envelope.
///
/// `chdir` is the in-sandbox cwd — bwrap starts the child in its
/// mount-namespace root — so a per-command launch passes the target's
/// logical cwd; a launch with none to carry passes `None`.
///
/// `ownership` decides two ties between the session and the envelope:
/// death (`--die-with-parent`) and address (`--info-fd`).  A surrendered
/// (detached) launch carries neither: the survivor must not be killed by our
/// death, and there is no session left here to address once we stop watching
/// it.  Confinement itself — the mounts, the seccomp filter — is otherwise
/// identical.
///
/// The render is pure in `host` and in descriptors, so a test can assert
/// options for a host it is not running on.
pub(crate) fn bwrap_argv(
    envelope: &Pinned,
    policy: &SandboxProjection<Rendered>,
    chdir: Option<&str>,
    ownership: Ownership,
    host: HostEnvelope,
    binds: &Binds,
    seccomp: usize,
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
    if ownership == Ownership::Kept {
        c.arg("--die-with-parent");
        c.args(["--info-fd", &Slot::Info.fd().to_string()]);
    }
    if !policy.net {
        c.arg("--unshare-net");
    }
    if let Some(dir) = chdir {
        c.args(["--chdir", dir]);
    }
    match &policy.fs {
        FsProjection::Restricted(_) => {
            // Before the prefix binds: a write prefix under `/tmp` lands inside it.
            c.args(["--tmpfs", "/tmp"]);
            // `--chdir` needs a directory to enter, and a cwd outside the
            // grant has none: an empty unwritable stand-in, as macOS leaves it
            // unreadable.  A bind over the name hides it.
            if let Some(dir) = chdir.filter(|dir| *dir != "/") {
                c.args(["--perms", "0555", "--tmpfs", dir]);
            }
            binds.render(&mut c, Layer::Shown);
            // After them, or a prefix of `/` or `/proc` re-binds the host's over these.
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
    for own in pinned_binaries(envelope)? {
        c.arg("--ro-bind").arg(&own).arg(&own);
    }
    // Masks go on after every bind: last mount wins.  `pinned_dirs` goes
    // unused because a mask is anchored to the inode, so a renamed ancestor
    // carries it — the pin macOS renders explicitly, Linux gets for free.
    if let Some(rules) = policy.fs.rules() {
        let mut denied: Vec<_> = rules.deny_paths.iter().collect();
        denied.sort();
        for path in denied {
            DenyMask::over(path).render(&mut c);
        }
    }
    for i in 0..seccomp {
        c.args(["--add-seccomp-fd", &Slot::seccomp(i)?.fd().to_string()]);
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
/// the cgroup tree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Layer {
    Shown,
    Over,
}

/// What the envelope shows at one name, by a handle on the object.
pub(crate) struct Bind {
    dest: Rendered,
    access: Access,
    layer: Layer,
}

/// The binds and their handles, in bwrap's order: `binds[i]` rides
/// `Slot::Mount(i)`.
pub(crate) struct Binds {
    binds: Vec<Bind>,
    handles: Vec<OwnedFd>,
}

impl Binds {
    /// Plan and open: the one place a launch opens what it mounts.  Each
    /// object is opened once, and each bind lends its own copy, bwrap
    /// closing a descriptor after the one mount it serves.
    fn open(
        image: Option<&RealPath>,
        policy: &SandboxProjection<Rendered>,
        host: HostEnvelope,
    ) -> Result<Self, String> {
        let mut planned: Vec<_> = (shown(image, policy)?.into_iter())
            .map(Shown::planned)
            .collect();
        if let Some(tree) = cgroup_tree(host)
            && let (Some(object), Some(dest)) = (object(&tree)?, object(CGROUP)?)
        {
            let bind = Bind {
                dest,
                access: Access::ReadOnly,
                layer: Layer::Over,
            };
            planned.push((object, bind));
        }
        let slots = usize::from(u16::MAX) + 1;
        if planned.len() > slots {
            return Err(format!(
                "sandbox: the grant asks for {} mounts, more than the {slots} a launch can lend",
                planned.len()
            ));
        }
        let mut opened = BTreeMap::new();
        for (object, _) in &planned {
            if !opened.contains_key(object) {
                opened.insert(object.clone(), open_object(object)?);
            }
        }
        let (mut binds, mut handles) = (Vec::new(), Vec::new());
        for (object, bind) in planned {
            // Absent: a name that shows nothing binds nothing.
            if let Some(Some(handle)) = opened.get(&object) {
                handles.push(handle.try_clone().map_err(|e| {
                    format!(
                        "sandbox: cannot copy the handle on {}: {e}",
                        object.as_str()
                    )
                })?);
                binds.push(bind);
            }
        }
        Ok(Self { binds, handles })
    }

    /// Each handle at its bind's slot.
    fn lend<'a>(&'a self, handoff: &mut Handoff<'a>) {
        for ((slot, _), handle) in self.slots().zip(&self.handles) {
            handoff.lend(slot, handle.as_fd());
        }
    }

    fn slots(&self) -> impl Iterator<Item = (Slot, &Bind)> {
        (0..=u16::MAX).map(Slot::Mount).zip(&self.binds)
    }

    fn layer(&self, layer: Layer) -> impl Iterator<Item = (Slot, &Bind)> {
        self.slots().filter(move |(_, bind)| bind.layer == layer)
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

/// One name the projection shows, and the object it reaches there.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Shown {
    access: Access,
    dest: Rendered,
    object: Rendered,
}

impl Shown {
    /// A name other than its object's own: the host shows a symlink there.
    fn is_spelling(&self) -> bool {
        self.dest != self.object
    }

    fn planned(self) -> (Rendered, Bind) {
        let Self {
            access,
            dest,
            object,
        } = self;
        (
            object,
            Bind {
                dest,
                access,
                layer: Layer::Shown,
            },
        )
    }

    /// Whether `self` already shows `inner`.  A read-only bind laid inside a
    /// writable one would deny writes the grant admits, so only a writable
    /// bind covers a writable object; a spelling within any bind is the
    /// host's own symlink there, onto which bwrap will not mount.
    fn covers(&self, inner: &Self) -> bool {
        if self.dest == inner.dest {
            return self.access == Access::Writable && inner.access == Access::ReadOnly;
        }
        self.dest.holds(inner.dest.as_str())
            && (inner.access == Access::ReadOnly
                || inner.is_spelling()
                || self.access == Access::Writable)
    }
}

/// What a restricted projection shows: the system's read-only defaults, the
/// read and write prefixes, the image and every file and directory the exec
/// table admits, each name by the object it reaches, less what another
/// bind already shows; read-only before writable, each sorted by name, so
/// a parent precedes its children and a writable prefix inside a read-only
/// one wins.
fn shown(
    image: Option<&RealPath>,
    policy: &SandboxProjection<Rendered>,
) -> Result<Vec<Shown>, String> {
    let FsProjection::Restricted(rules) = &policy.fs else {
        return Ok(Vec::new());
    };
    let mut read = render_paths(default_ro_binds())?;
    read.extend(rules.read_prefixes.iter().cloned());
    let mut names = objects(&read, Access::ReadOnly)?;
    names.extend(objects(&rules.write_prefixes, Access::Writable)?);
    let exec = match &policy.exec {
        ExecProjection::Restricted(rules) => ExecRules::from_kernel(rules),
        ExecProjection::Unrestricted => ExecRules::default(),
    };
    for real in image
        .into_iter()
        .chain(exec.allowed_files())
        .chain(exec.allowed_dirs())
    {
        names.extend(render_real(real)?.into_iter().map(|name| Shown {
            access: Access::ReadOnly,
            dest: name.clone(),
            object: name,
        }));
    }
    names.sort();
    names.dedup();
    Ok(names
        .iter()
        .filter(|inner| !names.iter().any(|outer| outer.covers(inner)))
        .cloned()
        .collect())
}

/// `names`, as rendered, each by the object it reaches.  Rendering put every
/// object among them, so one reached now outside them was moved since.
fn objects(names: &[Rendered], access: Access) -> Result<Vec<Shown>, String> {
    let mut shown = Vec::new();
    for Object { real, spellings } in render_objects(names)? {
        if !names.contains(&real) {
            let name = spellings.iter().map(Rendered::as_str).collect::<Vec<_>>();
            return Err(landlock::Error::Race {
                name: name.join(", "),
            }
            .to_string());
        }
        let dests = std::iter::once(real.clone()).chain(spellings);
        shown.extend(dests.map(|dest| Shown {
            access,
            dest,
            object: real.clone(),
        }));
    }
    Ok(shown)
}

/// `name` as the one object it reaches.
fn object(name: &str) -> Result<Option<Rendered>, String> {
    Ok(render_objects(&[name])?.into_iter().next().map(|o| o.real))
}

/// `object`, opened in the host to be mounted; `None` where it names nothing.
fn open_object(object: &Rendered) -> Result<Option<OwnedFd>, String> {
    let name = object.as_str();
    landlock::open_admit(name.as_ref(), OFlags::empty()).map_err(|e| match e {
        landlock::Error::Admit { source, .. } => {
            format!("sandbox: cannot open {name} to mount it: {source}")
        }
        race => race.to_string(),
    })
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

/// The files a launch execs — the envelope, and the trampoline bwrap execs by
/// its on-disk name — bound read-only over whatever the projection bound, so
/// no confined child rewrites a pinned inode in place, nor moves it: a
/// mountpoint cannot be renamed.
fn pinned_binaries(envelope: &Pinned) -> Result<Vec<std::path::PathBuf>, String> {
    let mut paths = vec![
        envelope
            .current_path()
            .map_err(|e| format!("bwrap: the pinned envelope has no path any more: {e}"))?,
    ];
    if let Some(own) = super::reexec::OWN.get() {
        paths.push(
            own.current_path()
                .map_err(|e| format!("ral: the pinned trampoline has no path any more: {e}"))?,
        );
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The envelope's `/dev`: bwrap's `--dev`, or its shape by hand where the
/// host refuses a fresh devpts.
///
/// By hand, `/dev/pts` is the host's, so a pty opened inside is not the
/// envelope's own.  An absent node is dropped: binding one costs the launch.
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
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:own-cgroup] reads ral's own /proc/self/cgroup to re-root the envelope's cgroup tree; envelope construction, not the model's data I/O"
)]
fn own_cgroup() -> Option<String> {
    let table = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let mut lines = table.lines();
    let own = lines.next()?.strip_prefix("0::")?;
    lines
        .next()
        .is_none()
        .then(|| own.trim_end_matches('/').to_string())
}

/// Both ends of a `Kept` launch's `--info-fd` socketpair — a socket, which
/// unlike a pipe no same-uid process can reopen through `/proc/<pid>/fd` to
/// forge a `child-pid`.  Our copy of bwrap's end, the lifted one, must
/// outlive the fork that gives bwrap its own, then go: a read reaches EOF
/// only once every copy of that end is closed.
pub(super) struct InfoFd {
    reader: UnixStream,
    writer: OwnedFd,
}

impl InfoFd {
    /// The payload's process group — its pid, `--new-session` having made it
    /// a leader — from bwrap's `child-pid`.  `None` if bwrap died before
    /// writing it; never a block, since EOF follows bwrap's own close.
    pub(super) fn payload_pgid(self) -> Option<crate::process::Pgid> {
        #[derive(serde::Deserialize)]
        struct Info {
            #[serde(rename = "child-pid")]
            child_pid: i32,
        }
        let Self { reader, writer } = self;
        drop(writer);
        let info: Info = serde_json::from_reader(reader).ok()?;
        crate::process::Pgid::from_raw(info.child_pid)
    }
}

/// The mount that masks one denied path, bwrap having no negative path rule.
/// The target's shape forces which one, and getting it wrong costs the launch
/// rather than the deny — `--tmpfs` over a regular file dies in `mkdir` before
/// the body execs — so [`Self::over`] is the only constructor.
///
/// Every mask goes over a name that already exists: a mount bwrap must first
/// `mkdir` is not available to us, for the reason [`Self::LeftAbsent`] gives.
enum DenyMask<'p> {
    /// An empty directory with no permission bits, which refuse the owner as
    /// much as anyone.  The bits are the whole of it: the tmpfs is the
    /// sandboxed uid's own, so a child that deliberately `chmod`s them back
    /// has scratch memory at that name — never the denied directory, and
    /// never the host.  An immutable mode needs a read-only mount, and bwrap
    /// gives that only for a bind, from a source this process would hold.
    EmptyDir(&'p str),
    /// A device node bound without `MS_DEV`: unopenable, `EACCES` either way.
    UnopenableNode(&'p str),
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

impl<'p> DenyMask<'p> {
    fn over(path: &'p Rendered) -> Self {
        match crate::path::shape(path.as_str()) {
            PathShape::Symlink => Self::OnItsTarget,
            PathShape::NonDir => Self::UnopenableNode(path.as_str()),
            PathShape::Dir => Self::EmptyDir(path.as_str()),
            PathShape::Absent => Self::LeftAbsent,
        }
    }

    fn render(self, c: &mut Options) {
        match self {
            Self::EmptyDir(path) => {
                c.args(["--perms", "0000", "--tmpfs", path]);
            }
            Self::UnopenableNode(path) => {
                c.args(["--ro-bind", "/dev/null", path]);
            }
            Self::OnItsTarget | Self::LeftAbsent => {}
        }
    }
}

/// System paths always bound read-only.  `/etc` and `/sys` wholesale are
/// excluded — only what dynamic linking, name resolution, user lookup,
/// toolchain resolution and hardware sizing need.  The rest of `/sys`
/// describes the host, not the envelope: `class/net` is the mounter's netns
/// whatever `--unshare-net` did, `class/dmi` and `bus` name the machine.  A
/// grant that wants it reads `/sys` by name.
fn default_ro_binds() -> &'static [&'static str] {
    &[
        "/bin",
        "/usr",
        "/lib",
        "/lib64",
        // `/dev`, `/proc` and `/sys/fs/cgroup` are absent: the mounts
        // emitted around these supply the envelope's own, and a real bind
        // here would shadow them.
        "/sys/devices/system/cpu",
        "/sys/kernel/mm/transparent_hugepage",
        "/etc/ld.so.conf",
        "/etc/ld.so.conf.d",
        "/etc/ld.so.cache",
        "/etc/resolv.conf",
        "/etc/nsswitch.conf",
        "/etc/hosts",
        "/etc/ssl",
        "/etc/ca-certificates",
        "/etc/pki",
        // getpwuid/getgrgid sit in libc startup paths: without these many
        // programs cannot even resolve HOME.
        "/etc/passwd",
        "/etc/group",
        // Debian/Ubuntu toolchain symlinks (cc → gcc-13, etc.).
        "/etc/alternatives",
        // Linuxbrew: a system prefix that happens to live under /home.
        "/home/linuxbrew/.linuxbrew",
    ]
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::{
        Access, Binds, CGROUP, HostEnvelope, Layer, Pinned, bwrap_argv, bwrap_command,
        seccomp_programs,
    };
    use crate::capability::{Admitted, Program};
    use crate::path::RealPath;
    use crate::sandbox::launch::{Ownership, admitted, enveloped};
    use crate::sandbox::warrant::{Handoff, Slot};
    use crate::types::{ExecProjection, ExecRule, FsProjection, FsRules, SandboxProjection};
    use std::process::Stdio;

    /// A bare Linux host.
    const WHOLE: HostEnvelope = HostEnvelope {
        private_pids: true,
        virtual_dev: true,
        private_cgroup: true,
        landlock: super::landlock::Landlock::At(super::landlock::Abi::SIGNAL_SCOPE),
        binds_by_fd: true,
    };

    fn workdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ral-bwrap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create work dir");
        dir
    }

    fn deny_within(dir: &std::path::Path, denied: &[&std::path::Path]) -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                write_prefixes: vec![dir.to_string_lossy().into_owned()],
                deny_paths: denied
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
                ..FsRules::default()
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        }
    }

    fn unrestricted() -> SandboxProjection {
        SandboxProjection {
            fs: FsProjection::Unrestricted,
            net: true,
            exec: crate::types::ExecProjection::default(),
        }
    }

    /// This test binary stands in for bwrap where only the options are read:
    /// the render tests need no bwrap on the host, and no payload shares its
    /// name.
    fn stand_in() -> Pinned {
        Pinned::open(std::env::current_exe().expect("own path")).expect("own binary pins")
    }

    /// `policy`'s binds, opened over this host's tree.
    fn binds(image: Option<&RealPath>, policy: &SandboxProjection, host: HostEnvelope) -> Binds {
        let rendered = policy.rendered().expect("ASCII paths render");
        Binds::open(image, &rendered, host).expect("the binds open")
    }

    fn options(
        image: Option<&RealPath>,
        policy: &SandboxProjection,
        chdir: Option<&str>,
        ownership: Ownership,
        host: HostEnvelope,
    ) -> Vec<String> {
        let seccomp = seccomp_programs().expect("seccomp programs compile");
        let rendered = policy.rendered().expect("ASCII paths render");
        let binds = Binds::open(image, &rendered, host).expect("the binds open");
        bwrap_argv(
            &stand_in(),
            &rendered,
            chdir,
            ownership,
            host,
            &binds,
            seccomp.len(),
        )
        .expect("ASCII paths render")
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
    }

    fn options_on(
        host: HostEnvelope,
        policy: &SandboxProjection,
        ownership: Ownership,
    ) -> Vec<String> {
        options(None, policy, None, ownership, host)
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
    /// hide it where they cover the name — and the root and an unrestricted
    /// envelope, which already have one, get none.
    #[test]
    fn a_cwd_outside_the_grant_is_stood_up_empty_and_unwritable() {
        let dir = workdir("cwd-stand-in");
        let dir_s = dir.to_string_lossy().into_owned();
        let granted = deny_within(&dir, &[]);
        for (label, policy, cwd, stood_up) in [
            ("ungranted", &granted, "/ral-ungranted/cwd", true),
            ("root", &granted, "/", false),
            ("unrestricted", &unrestricted(), "/ral-ungranted/cwd", false),
        ] {
            let args = options(None, policy, Some(cwd), Ownership::Kept, WHOLE);
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

        let read_only = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read_prefixes: vec![config.to_string_lossy().into_owned()],
                deny_paths: vec![under_read.to_string_lossy().into_owned()],
                ..FsRules::default()
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        };
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

    /// The envelope shows an interpreter only on the system's word: a script
    /// the user can write binds nothing beside itself.
    #[test]
    fn a_script_the_user_can_write_has_no_interpreter_bound() {
        let dir = workdir("carrier-bind");
        let interp = dir.join("interp");
        std::fs::write(&interp, "\x7fELF").unwrap();
        let script = dir.join("run");
        std::fs::write(&script, format!("#!{}\n", interp.display())).unwrap();
        let script = RealPath::of(&script).expect("the script exists");
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsRules::default()),
            net: true,
            exec: crate::types::ExecProjection::default(),
        };
        let args = options(Some(&script), &policy, None, Ownership::Kept, WHOLE);
        let real = std::fs::canonicalize(&interp)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            !args.contains(&real),
            "an interpreter the user could have swapped in must not be bound: {args:?}"
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

    /// The trampoline execs the target by the real path the guard judged, so
    /// outside every bind the envelope shows that file by its handle and
    /// nothing at the name it was spelled by; within a bind, the bind shows
    /// it.  The payload starts in the logical cwd.
    #[test]
    fn the_target_is_bound_by_its_real_name_and_entered_from_the_cwd() {
        let (dir, real) = linked_tool("target-bind");
        let image = RealPath::of(std::path::Path::new(&real)).expect("the tool exists");
        let elsewhere = workdir("target-bind-elsewhere");
        let args = options(
            Some(&image),
            &deny_within(&elsewhere, &[]),
            Some("/"),
            Ownership::Kept,
            WHOLE,
        );
        assert!(
            bound(&args, "--ro-bind-fd", &real).is_some(),
            "the host image must be shown read-only by its handle: {args:?}"
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
        let args = options(
            Some(&image),
            &deny_within(&dir, &[]),
            None,
            Ownership::Kept,
            WHOLE,
        );
        assert!(
            bound(&args, "--ro-bind-fd", &real).is_none(),
            "a read-only bind inside the grant's writable one would deny its writes: {args:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    fn restricted(read: &[&std::path::Path], write: &[&std::path::Path]) -> SandboxProjection {
        let names = |paths: &[&std::path::Path]| {
            (paths.iter())
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        };
        SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read_prefixes: names(read),
                write_prefixes: names(write),
                ..FsRules::default()
            }),
            net: true,
            exec: ExecProjection::default(),
        }
    }

    /// The `Shown` binds beneath `dir`, the defaults left out.
    fn shown_within(binds: &Binds, dir: &std::path::Path) -> Vec<(String, Access)> {
        (binds.layer(Layer::Shown))
            .map(|(_, bind)| (bind.dest.as_str().to_string(), bind.access))
            .filter(|(dest, _)| std::path::Path::new(dest).starts_with(dir))
            .collect()
    }

    /// One bind per name the projection shows and another bind does not:
    /// an absent prefix shows nothing; a read-only one within another bind,
    /// or an exec directory within the grant's writable one, adds nothing; a
    /// writable one within a read-only one wins; a file mounts as a file; an
    /// exec directory outside every prefix is shown, as on macOS.
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
        let mut policy = restricted(&[&ro, &inner, &file, &absent], &[&w]);
        policy.exec = ExecProjection::Restricted(
            [&tools, &w.join("bin")]
                .into_iter()
                .map(|d| ExecRule::Dir {
                    path: RealPath::of(d).expect("an exec dir exists"),
                    allow: true,
                })
                .collect(),
        );
        let binds = binds(None, &policy, WHOLE);
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
        let (_, handle) = (binds.slots().zip(&binds.handles))
            .find(|((_, bind), _)| bind.dest.as_str() == name(&file))
            .map(|((slot, _), handle)| (slot, handle))
            .expect("the file's bind");
        let opened = rustix::fs::fstat(handle).expect("fstat the handle");
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
        let both = binds(None, &restricted(&[&link], &[]), WHOLE);
        assert_eq!(
            shown_within(&both, &dir),
            [
                (name(&link), Access::ReadOnly),
                (name(&real), Access::ReadOnly)
            ]
        );
        let covered = binds(None, &restricted(&[&dir, &link], &[]), WHOLE);
        assert_eq!(
            shown_within(&covered, &dir),
            [(name(&dir), Access::ReadOnly)]
        );
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
        let rendered = restricted(&[&parent.join("data")], &[])
            .rendered()
            .expect("ASCII paths render");
        std::fs::rename(&parent, dir.join("moved")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &parent).unwrap();
        let Err(why) = Binds::open(None, &rendered, WHOLE) else {
            panic!("a symlinked component would show its target");
        };
        assert!(why.contains("a race"), "{why}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every handle by its slot, in layer order: the projection's view, then
    /// the envelope's own `/proc` and `/dev`, then what goes over everything,
    /// then the masks.  No source is a name bwrap resolves but the pins' (by
    /// name until they mount by their own descriptors) and `/dev/null`.
    #[test]
    fn every_bind_is_a_handle_by_slot_in_layer_order() {
        let dir = workdir("argv-order");
        let (ro, w) = (dir.join("ro"), dir.join("ro/w"));
        let secret = w.join("secret");
        std::fs::create_dir_all(&w).unwrap();
        std::fs::write(&secret, "x").unwrap();
        let mut policy = restricted(&[&ro], &[&w]);
        if let FsProjection::Restricted(rules) = &mut policy.fs {
            rules.deny_paths = vec![secret.to_string_lossy().into_owned()];
        }
        let args = options_for(&policy);
        let at = |piece: &[&str]| {
            position_of(&args, piece).unwrap_or_else(|| panic!("no {piece:?}: {args:?}"))
        };
        let by_handle = |op: &str, dest: &std::path::Path| {
            bound(&args, op, &dest.to_string_lossy())
                .unwrap_or_else(|| panic!("no {op} at {}: {args:?}", dest.display()))
        };
        let order = [
            by_handle("--ro-bind-fd", &ro),
            by_handle("--bind-fd", &w),
            at(&["--proc", "/proc"]),
            at(&["--dev", "/dev"]),
            by_handle("--ro-bind-fd", std::path::Path::new(CGROUP)),
            at(&["--ro-bind", "/dev/null", &secret.to_string_lossy()]),
        ];
        assert!(order.is_sorted(), "{order:?}: {args:?}");
        let slots: Vec<_> = (args.windows(2))
            .filter(|w| w[0] == "--ro-bind-fd" || w[0] == "--bind-fd")
            .map(|w| w[1].clone())
            .collect();
        let expected: Vec<_> = (0..u16::try_from(slots.len()).unwrap())
            .map(|i| Slot::Mount(i).fd().to_string())
            .collect();
        assert_eq!(slots, expected, "one slot per bind, in order");
        let pins = super::pinned_binaries(&stand_in()).expect("the pins have names");
        for w in args.windows(3).filter(|w| w[0] == "--ro-bind") {
            assert!(
                w[1] == "/dev/null"
                    || (w[1] == w[2] && pins.iter().any(|p| p.as_os_str() == w[1].as_str())),
                "a source bwrap would resolve by name: {w:?}"
            );
        }
        assert!(
            !args.iter().any(|a| a == "--bind" || a == "--symlink"),
            "{args:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without `--ro-bind-fd` nothing could be shown but by name, so nothing
    /// is launched at all.
    #[test]
    fn a_bwrap_that_cannot_mount_by_descriptor_launches_nothing() {
        let host = HostEnvelope {
            binds_by_fd: false,
            ..WHOLE
        };
        let rendered = unrestricted().rendered().expect("renders");
        let envelope = stand_in();
        let Err(why) = bwrap_command(
            &envelope,
            &envelope,
            None,
            Handoff::default(),
            &rendered,
            None,
            Ownership::Kept,
            host,
        ) else {
            panic!("a bwrap without --ro-bind-fd must be refused");
        };
        let path = envelope.arg0().display().to_string();
        assert!(why.contains(&path) && why.contains("0.8.0"), "{why}");
    }

    /// `/bin/sh -c script`, through [`run_admitted`].
    fn run_confined(
        envelope: &Pinned,
        host: HostEnvelope,
        policy: &SandboxProjection,
        script: &str,
    ) -> Option<std::process::Output> {
        run_admitted(envelope, host, policy, &sh_c(script))
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
        envelope: &Pinned,
        host: HostEnvelope,
        policy: &SandboxProjection,
        admitted: &Admitted,
    ) -> Option<std::process::Output> {
        run_after(envelope, host, policy, admitted, || ())
    }

    /// [`run_admitted`], built now and spawned after `between`: the window in
    /// which a same-uid writer races the launch.
    fn run_after(
        envelope: &Pinned,
        host: HostEnvelope,
        policy: &SandboxProjection,
        admitted: &Admitted,
        between: impl FnOnce(),
    ) -> Option<std::process::Output> {
        let (mut cmd, info_fd) = enveloped(envelope, host, policy, admitted, None, Ownership::Kept)
            .expect("the launch builds");
        between();
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = cmd.output().ok();
        // Kept open across the fork inside `output()` for `pre_exec`'s `dup2`.
        drop(info_fd);
        out
    }

    /// This host's pinned bwrap, where it can build an envelope at all.  Where
    /// it cannot — bwrap absent, or user namespaces unavailable — a spawning
    /// test proves nothing either way and says so on the way out.
    fn envelope_launches(policy: &SandboxProjection) -> Option<&'static Pinned> {
        super::pin_envelope();
        let Ok(envelope) = super::envelope() else {
            eprintln!("skipping: this host has no bwrap to pin");
            return None;
        };
        let control = run_confined(
            envelope,
            HostEnvelope::probe(envelope),
            policy,
            "echo READY",
        );
        if control
            .as_ref()
            .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("READY"))
        {
            return Some(envelope);
        }
        let why = control.map_or_else(
            || "the envelope did not spawn".to_string(),
            |o| String::from_utf8_lossy(&o.stderr).trim().to_string(),
        );
        eprintln!("skipping: this host cannot build a bwrap envelope: {why}");
        None
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

        let policy = |deny_paths: Vec<String>| SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                read_prefixes: vec![config.to_string_lossy().into_owned()],
                write_prefixes: vec![work.to_string_lossy().into_owned()],
                deny_paths,
                ..FsRules::default()
            }),
            net: true,
            exec: crate::types::ExecProjection::default(),
        };
        let Some(envelope) = envelope_launches(&policy(vec![])) else {
            return;
        };

        let script = format!(
            "echo READY\n\
             mkdir -p '{denied}' 2>/dev/null || echo DENY-CREATE-REFUSED\n",
            denied = denied.display(),
        );
        let out = run_confined(
            envelope,
            HostEnvelope::probe(envelope),
            &policy(vec![denied.to_string_lossy().into_owned()]),
            &script,
        )
        .expect("spawn bwrap");
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

        let Some(envelope) = envelope_launches(&deny_within(&dir, &[])) else {
            return;
        };
        let out = run_confined(
            envelope,
            HostEnvelope::probe(envelope),
            &deny_within(&dir, &[&denied]),
            "echo READY",
        )
        .expect("spawn bwrap");
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

        let Some(envelope) = envelope_launches(&deny_within(&dir, &[])) else {
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
        let out = run_confined(
            envelope,
            HostEnvelope::probe(envelope),
            &deny_within(&dir, &[&key, &git]),
            &script,
        )
        .expect("spawn bwrap");
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
    /// named: `~/.local/bin/uv`, a link into `~/.local/share/uv` with neither
    /// granted, reported 127.  The real file is shown alone, by its handle:
    /// its siblings stay out of sight.
    #[test]
    fn a_linked_image_outside_the_grant_runs_and_shows_none_of_its_siblings() {
        let (dir, real) = linked_tool("image-link-spawn");
        let secret = std::path::Path::new(&real).with_file_name("secret");
        std::fs::write(&secret, "SECRET-BYTES").unwrap();
        std::fs::write(
            &real,
            format!(
                "#!/bin/sh\n\
                 echo READY\n\
                 cat '{secret}' 2>/dev/null || echo SECRET-REFUSED\n",
                secret = secret.display(),
            ),
        )
        .unwrap();
        let elsewhere = workdir("image-link-spawn-elsewhere");
        let policy = deny_within(&elsewhere, &[]);
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };

        let tool = Program::file(dir.join("linkdir/tool")).expect("the tool exists");
        let out = run_admitted(
            envelope,
            HostEnvelope::probe(envelope),
            &policy,
            &admitted(tool, &[]),
        )
        .expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);

        assert!(
            stdout.contains("READY"),
            "the spelled name did not run inside the envelope: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !stdout.contains("SECRET-BYTES"),
            "a sibling of the real file reached the child: {stdout}"
        );
        assert!(
            stdout.contains("SECRET-REFUSED"),
            "the real file's sibling must stay out of sight: {stdout}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
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
        let policy = restricted(&[&data], &[]);
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };
        let script = format!("echo READY\ncat '{}'\n", data.join("f").display());
        let host = HostEnvelope::probe(envelope);
        let out = run_after(envelope, host, &policy, &sh_c(&script), || {
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
        let policy = restricted(&[&outer], &[&inner]);
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };
        let host = HostEnvelope::probe(envelope);
        let out = run_after(envelope, host, &policy, &sh_c("echo READY"), || {
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
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };

        let out = run_confined(
            envelope,
            HostEnvelope {
                virtual_dev: false,
                ..HostEnvelope::probe(envelope)
            },
            &policy,
            "echo x > /dev/null && echo NULL-OK\n\
             head -c 1 /dev/urandom > /dev/null && echo URANDOM-OK\n\
             : > /dev/shm/probe && echo SHM-OK\n\
             exec 3<>/dev/ptmx && echo PTMX-OK\n",
        )
        .expect("spawn bwrap");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        for ok in ["NULL-OK", "URANDOM-OK", "SHM-OK", "PTMX-OK"] {
            assert!(
                stdout.contains(ok),
                "the by-hand /dev is missing {ok}: {stdout} {stderr}"
            );
        }
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
            let binds = binds(None, policy, host);
            let ((_, bind), handle) = (binds.slots().zip(&binds.handles))
                .find(|((_, bind), _)| bind.dest.as_str() == CGROUP)
                .expect("a cgroup bind");
            assert_eq!(bind.layer, Layer::Over);
            rustix::fs::fstat(handle).expect("fstat the tree").st_ino
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
            let Some(envelope) = envelope_launches(policy) else {
                continue;
            };
            let host = HostEnvelope::probe(envelope);
            let out = run_confined(envelope, host, policy, script).expect("spawn bwrap");
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
                read_prefixes: vec!["/".to_string()],
                write_prefixes: vec![dir_s.clone()],
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
    /// Its own argv is the slot its options arrive on, then the trampoline.
    #[test]
    fn the_launcher_is_the_pinned_envelope_and_never_a_name() {
        let (envelope, own) = (stand_in(), stand_in());
        let (cmd, _info_fd) = bwrap_command(
            &envelope,
            &own,
            None,
            Handoff::default(),
            &unrestricted().rendered().expect("renders"),
            None,
            Ownership::Kept,
            WHOLE,
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
        let slot = Slot::Args.fd().to_string();
        let own = own.arg0().to_string_lossy();
        let flag = crate::sandbox::WARRANT_FLAG;
        assert_eq!(args, ["--args", &slot, "--", &own, flag], "{args:?}");
    }

    /// A pin on a handoff slot would be closed by the `dup2` before bwrap's
    /// exec, which would then run whatever landed there.
    #[test]
    fn a_pin_on_a_handoff_slot_is_refused() {
        use std::os::fd::AsFd;
        let pin = stand_in().lifted(Slot::Warrant.fd());
        let at = pin.raw_fd();
        if at > Slot::Info.fd() {
            eprintln!(
                "skipping: descriptors up to {} are all busy",
                Slot::Info.fd()
            );
            return;
        }
        let lent = std::fs::File::open("/dev/null").expect("open /dev/null");
        let mut handoff = Handoff::default();
        handoff.lend(Slot::Warrant, lent.as_fd());
        let Err(e) = bwrap_command(
            &pin,
            &pin,
            None,
            handoff,
            &unrestricted().rendered().expect("renders"),
            None,
            Ownership::Kept,
            WHOLE,
        ) else {
            panic!("a pin on a handoff slot must be refused");
        };
        assert!(e.contains(&format!("descriptor {at}")), "{e}");
    }

    /// Whatever the projection bound, both files a launch execs go read-only
    /// over it, before the masks.  The two names coincide here, the stand-in
    /// envelope being this test binary.
    #[test]
    fn the_envelope_binary_is_read_only_inside_every_envelope() {
        crate::sandbox::reexec::pin_self();
        let own = std::fs::canonicalize(stand_in().arg0()).expect("resolve the stand-in");
        let own = own.to_string_lossy().into_owned();
        let trampoline = crate::sandbox::reexec::OWN
            .get()
            .expect("ral pins itself")
            .current_path()
            .expect("the pinned trampoline still has a path");
        let trampoline = trampoline.to_string_lossy().into_owned();
        let dir = workdir("envelope-ro");
        let dir_s = dir.to_string_lossy();
        let denied = dir.join("secret");
        std::fs::write(&denied, "x").unwrap();

        for (label, policy) in [
            ("unrestricted", unrestricted()),
            ("restricted", deny_within(&dir, &[&denied])),
        ] {
            let args = options_for(&policy);
            let bound = position_of(&args, &["--dev-bind", "/", "/"])
                .or_else(|| bound(&args, "--bind-fd", &dir_s))
                .expect("the projection's bind");
            let mask = position_of(
                &args,
                &["--ro-bind", "/dev/null", &denied.to_string_lossy()],
            );
            for (what, path) in [
                ("envelope", own.as_str()),
                ("trampoline", trampoline.as_str()),
            ] {
                let ro = position_of(&args, &["--ro-bind", path, path]).unwrap_or_else(|| {
                    panic!("{label}: no read-only bind of the {what}: {args:?}")
                });
                assert!(bound < ro, "{label}: the {what}'s bind must win: {args:?}");
                if let Some(mask) = mask {
                    assert!(ro < mask, "{label}: masks go on last: {args:?}");
                }
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
                read_prefixes: vec!["/usr".to_string()],
                ..FsRules::default()
            }),
            net: false,
            exec: crate::types::ExecProjection::default(),
        };
        let kept = options_on(WHOLE, &policy, Ownership::Kept);
        let surrendered = options_on(WHOLE, &policy, Ownership::Surrendered);

        assert!(
            kept.contains(&"--die-with-parent".to_string()),
            "a child we keep must not outlive us: {kept:?}"
        );
        let info = Slot::Info.fd().to_string();
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
        let ownership_ties = ["--die-with-parent", "--info-fd", info.as_str()];
        assert_eq!(
            kept.iter()
                .filter(|a| !ownership_ties.contains(&a.as_str()))
                .collect::<Vec<_>>(),
            surrendered.iter().collect::<Vec<_>>(),
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
        use crate::process::{CancelCause, CancelScope, Group, PgidPolicy};
        use crate::runtime::command::{Pumps, RunningChild};

        let policy = unrestricted();
        if envelope_launches(&policy).is_none() {
            return;
        }

        let dir = workdir("receipt-grace");
        let flag = dir.join("grace");
        let script = format!(
            "trap 'echo GRACE > {} ; exit 0' TERM\nsleep 30\n",
            flag.display()
        );

        let shell = crate::types::Shell::default();
        let scope = CancelScope::root();
        let sh = Program::file("/bin/sh".into()).expect("/bin/sh exists");
        let mut launch = crate::sandbox::sandboxed_command(
            &policy,
            &admitted(sh, &["-c".to_string(), script]),
            Ownership::Kept,
            &shell,
            &scope,
        )
        .expect("build confined launch");

        let (child, pgid, jail) = launch
            .spawn(PgidPolicy::NewLeader)
            .expect("spawn confined sh");

        let running = RunningChild::assemble_with_owner(
            child,
            "sh".to_string(),
            Pumps::default(),
            pgid.map(Group::Owns),
            scope.clone(),
            jail,
        );

        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            scope.cancel(CancelCause::Explicit);
        });

        let t0 = std::time::Instant::now();
        let waited = running.wait();
        let elapsed = t0.elapsed();
        let (outcome, cause) = (waited.outcome, waited.cause);
        waited.settle();
        canceller.join().expect("canceller thread");

        assert!(
            elapsed.as_secs() < 5,
            "teardown must not fall back to the payload's own 30 s sleep: took {elapsed:?}"
        );
        assert_eq!(
            cause,
            Some(CancelCause::Explicit),
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
            let Some(envelope) = envelope_launches(policy) else {
                continue;
            };
            let host = HostEnvelope::probe(envelope);
            let out = run_confined(envelope, host, policy, &script).expect("spawn bwrap");
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
            let Some(envelope) = envelope_launches(policy) else {
                continue;
            };
            let host = HostEnvelope::probe(envelope);
            let out = run_confined(envelope, host, policy, script).expect("spawn bwrap");
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
    /// paths as a grant freezes them.  The loader and ral's own binary need
    /// no naming: `landlock::build` admits them.
    fn admitting(dirs: &[&str], paths: &[&str]) -> ExecProjection {
        let real = |p: &str| RealPath::of(std::path::Path::new(p)).expect("an admit exists");
        let dirs = ["/bin", "/usr/bin"]
            .iter()
            .chain(dirs)
            .map(|d| ExecRule::Dir {
                path: real(d),
                allow: true,
            });
        let files = paths.iter().map(|p| ExecRule::File {
            path: real(p),
            allow: true,
        });
        ExecProjection::Restricted(dirs.chain(files).collect())
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
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             '{copy}' && echo PLANTED-RAN\n\
             /bin/true && echo ADMITTED-RAN\n",
            copy = copy.display(),
        );
        let out =
            run_confined(envelope, HostEnvelope::probe(envelope), &policy, &script).expect("spawn");
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
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             sh -c '{copy}' && echo PLANTED-RAN\n\
             sh -c /bin/true && echo ADMITTED-RAN\n",
            copy = copy.display(),
        );
        let out =
            run_confined(envelope, HostEnvelope::probe(envelope), &policy, &script).expect("spawn");
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
    fn admitting_less(dirs: &[&str], blocks: impl IntoIterator<Item = ExecRule>) -> ExecProjection {
        let ExecProjection::Restricted(mut rules) = admitting(dirs, &[]) else {
            unreachable!("admitting restricts");
        };
        rules.extend(blocks);
        ExecProjection::Restricted(rules)
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
        let policy = exec_policy(admitting_less(
            &[],
            [
                ExecRule::File {
                    path: RealPath::of(&denied).expect("the command exists"),
                    allow: false,
                },
                ExecRule::Veto(crate::path::command_name_key(&name)),
            ],
        ));
        let Some(envelope) = envelope_launches(&policy) else {
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
        let out =
            run_confined(envelope, HostEnvelope::probe(envelope), &policy, &script).expect("spawn");
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
        let policy = exec_policy(admitting_less(
            &[&expanded.to_string_lossy(), &whole.to_string_lossy()],
            [ExecRule::File {
                path: RealPath::of(&blocked).expect("the block exists"),
                allow: false,
            }],
        ));
        let Some(envelope) = envelope_launches(&policy) else {
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
        let out =
            run_confined(envelope, HostEnvelope::probe(envelope), &policy, &script).expect("spawn");
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

    /// What the grant lets run it lets read, as on macOS: an exec directory
    /// outside every fs prefix is shown, and without the exec grant it is not.
    #[test]
    fn an_exec_allowed_directory_is_readable_outside_every_fs_prefix() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let dir = workdir("exec-parity");
        let tools = dir.join("tools");
        std::fs::create_dir(&tools).unwrap();
        std::fs::write(tools.join("readme"), "PARITY-BYTES").unwrap();
        let elsewhere = workdir("exec-parity-elsewhere");
        let script = format!("echo READY\ncat '{}'\n", tools.join("readme").display());
        for (admitted, exec) in [
            (true, admitting(&[&tools.to_string_lossy()], &[])),
            (false, admitting(&[], &[])),
        ] {
            let policy = SandboxProjection {
                exec,
                ..deny_within(&elsewhere, &[])
            };
            let Some(envelope) = envelope_launches(&policy) else {
                return;
            };
            let out = run_confined(envelope, HostEnvelope::probe(envelope), &policy, &script)
                .expect("spawn");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.contains("READY"),
                "the envelope did not launch: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                stdout.contains("PARITY-BYTES"),
                admitted,
                "an exec directory is shown exactly when the grant admits it: {stdout}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&elsewhere);
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
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             cat /dev/null && echo DYNAMIC-RAN\n\
             '{file}'\n",
            file = script_file.display(),
        );
        let out =
            run_confined(envelope, HostEnvelope::probe(envelope), &policy, &script).expect("spawn");
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
        let Some(envelope) = envelope_launches(&policy) else {
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
        let out =
            run_confined(envelope, HostEnvelope::probe(envelope), &policy, &script).expect("spawn");
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
            let Some(envelope) = envelope_launches(&policy) else {
                return;
            };
            let out = run_confined(envelope, HostEnvelope::probe(envelope), &policy, script)
                .expect("spawn");
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
        for exec in [ExecProjection::Unrestricted, admitting(&[], &[])] {
            let policy = exec_policy(exec);
            let Some(envelope) = envelope_launches(&policy) else {
                break;
            };
            let out = run_confined(envelope, HostEnvelope::probe(envelope), &policy, &script)
                .expect("spawn");
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
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n'{copy}' && echo PLANTED-RAN\n",
            copy = copy.display()
        );
        let out =
            run_confined(envelope, HostEnvelope::probe(envelope), &policy, &script).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            emitted(&stdout, "PLANTED-RAN"),
            "an unrestricted exec projection must confine no exec at all: {stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ral re-execs itself inside, so the grant need not name it; the admit
    /// is the pinned inode, so a byte-identical copy beside it stays denied.
    #[test]
    fn ral_runs_unnamed_by_the_grant_and_a_copy_of_it_does_not() {
        if !landlock_at_least(super::landlock::Abi::EXEC) {
            return;
        }
        let own = crate::sandbox::reexec::own().expect("ral pins itself");
        let dir = workdir("exec-self");
        let copy = dir.join("ral-copy");
        std::fs::copy(own.arg0(), &copy).expect("copy ral");
        let policy = exec_policy(admitting(&[], &[]));
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };
        let script = format!(
            "echo READY\n\
             '{own}' --list >/dev/null && echo SELF-RAN\n\
             '{copy}' --list >/dev/null && echo COPY-RAN\n",
            own = own.arg0().display(),
            copy = copy.display(),
        );
        let out =
            run_confined(envelope, HostEnvelope::probe(envelope), &policy, &script).expect("spawn");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            emitted(&stdout, "SELF-RAN"),
            "ral's own binary did not run inside: {stdout} {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !emitted(&stdout, "COPY-RAN"),
            "a copy of ral ran, so the admit is not its inode: {stdout}"
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
            &stand_in(),
            host,
            &exec_policy(admitting(&[], &[])),
            &admitted(sh, &["-c".to_string(), "echo RAN".to_string()]),
            None,
            Ownership::Kept,
        );
        let Err(crate::types::Break::Error(e)) = launch else {
            panic!("a restricted exec grant needs Landlock");
        };
        assert!(e.message.contains("Landlock"), "{}", e.message);
    }

    /// A warrant promising a ruleset is the trampoline's whole word on it, so
    /// one that never arrived is a launch it refuses, not one it runs open.
    #[test]
    fn a_promised_ruleset_that_never_arrived_runs_nothing() {
        use crate::sandbox::warrant::Warrant;
        use std::os::fd::AsFd;
        let own = crate::sandbox::reexec::own().expect("ral pins itself");
        let sh = Program::file("/bin/sh".into()).expect("/bin/sh exists");
        let promise = Some(super::landlock::Landlocked { refer_root: false });
        let warrant = Warrant::new(
            promise,
            &admitted(sh, &["-c".to_string(), "echo RAN".to_string()]),
        )
        .parcel()
        .expect("parcels");
        let mut cmd = own.command();
        cmd.arg(crate::sandbox::WARRANT_FLAG)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut handoff = Handoff::default();
        handoff.lend(Slot::Warrant, warrant.as_fd());
        handoff.install(&mut cmd).expect("installs");
        let out = cmd.output().expect("spawn the trampoline");
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(out.status.code(), Some(126), "{stdout} {stderr}");
        assert!(!emitted(&stdout, "RAN"), "the program ran unconfined");
        let at = format!("fd {}", Slot::Ruleset.fd());
        assert!(stderr.contains(&at), "{stderr}");
    }

    // ── The seccomp deny-set ──────────────────────────────────────────────
    //
    // The filter is projection-independent (`seccomp::Filter::ENVELOPE` is
    // keyed on nothing in the grant), so the open exec projection is the
    // right one to prove it on — these tests are not about exec at all.

    fn on_path(tool: &str) -> bool {
        let path = std::env::var("PATH").unwrap_or_default();
        crate::path::which::locate(tool, Some(&path), crate::path::which::SearchCwd::nowhere())
            .is_some()
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
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };
        let out = run_confined(
            envelope,
            HostEnvelope::probe(envelope),
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
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };
        let out = run_confined(
            envelope,
            HostEnvelope::probe(envelope),
            &policy,
            "unshare -U true 2>&1; echo RC=$?",
        )
        .expect("spawn bwrap");
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
        let Some(envelope) = envelope_launches(&policy) else {
            return;
        };
        let out = run_confined(
            envelope,
            HostEnvelope::probe(envelope),
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

        let described =
            crate::sandbox::diag::platform::describe_denial(&libc::SYS_ptrace.to_string());
        assert!(
            described.is_some_and(|d| d.contains("ptrace")),
            "describe_denial must name ptrace from its syscall number"
        );
    }
}
