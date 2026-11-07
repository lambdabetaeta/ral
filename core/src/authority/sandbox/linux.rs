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
//! whole — so `SandboxProjection::net` is one veto, not a list.
//!
//! Every object the envelope shows is opened here, never through a symlink,
//! and mounted by that handle ([`Binds`]): bwrap resolves no source name, so
//! a name swapped for a symlink after the grant was rendered shows nothing
//! it did not name.  The GPU's nodes and sysfs are the exception, bound by
//! name as `/dev`'s own nodes are ([`gpu`]).  Each directory that holds a
//! deny in place is bound onto its own name ([`Layer::Spine`]), so no
//! confined child moves the deny's object to a name no deny covers.
//!
//! The payload is always ral's own trampoline (`super::launch`), which enters
//! the [`landlock`] ruleset it inherits and only then runs the target, as a
//! child it supervises so as to report its exact end, which bwrap would fold
//! into an exit status: that order is forced, a Landlock domain handling any
//! fs right forbidding the `mount(2)` bwrap opens with.

mod gpu;
mod host;
pub(crate) mod landlock;
pub(crate) mod payload;
pub(crate) mod seccomp;
mod shown;

use gpu::GpuNodes;
use host::Builds;
pub(crate) use host::HostEnvelope;
use shown::Layer;
pub(crate) use shown::{Binds, Shown};

use super::launch::Ownership;
use super::reexec::Pinned;
use super::warrant::{Handoff, Slot, Slots, nul_terminated, parcel};
use super::{FsProjection, RenderedProjection, unavailable};
use crate::error::Error;
use crate::path::{CwdPath, LexicalPath, RealPath};
use std::ffi::{OsStr, OsString};
use std::io::Read;
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
/// `bwrap --args <a> -- /proc/self/fd/<t> --warrant <w> <r> <in> <out> <err>`, every
/// descriptor lent to `handoff` and named by its slot: the options in a
/// sealed parcel, ral's pin, the seccomp programs, every bind's handle and the
/// `--info-fd` peer, beside the caller's (the warrant and report, at `slots`,
/// and the Landlock ruleset).  bwrap takes options alone from `--args`, so the payload rides
/// its argv, and runs the pinned inode whatever now holds its name.
/// `shown` is the plan the Landlock ruleset was built against, binds and
/// masks alike.
///
/// The third element reserves stream slots for the launch's early capture
/// hook. The second is `Kept`'s `--info-fd` peer — see
/// [`InfoFd`].  Refused under a bwrap that builds no envelope or cannot mount
/// by descriptor.
pub(crate) fn bwrap_command(
    env: &Envelope<'_>,
    shown: Shown,
    mut handoff: Handoff,
    mut slots: Slots,
    policy: &RenderedProjection,
    chdir: Option<&CwdPath>,
    ownership: Ownership,
) -> Result<
    (
        Command,
        Option<InfoFd>,
        crate::process::launch::EnvelopeStdio,
    ),
    Error,
> {
    if let Builds::Nothing(why) = env.host.builds {
        return Err(unavailable(format!(
            "bwrap at {} cannot build an envelope on this host: {why}",
            crate::path::shown(env.bwrap.arg0())
        )));
    }
    let (stdio, streams) = handoff.streams().map_err(Error::new)?;
    slots.streams = Some(streams);
    let binds = Binds::open(env, shown, &mut handoff).map_err(Error::new)?;
    let (info, address) = (ownership == Ownership::Kept)
        .then(|| InfoFd::open(&mut handoff))
        .transpose()
        .map_err(Error::new)?
        .unzip();
    let mut lend_parcel = |name: &str, bytes: &[u8]| {
        let parcel =
            parcel(name, bytes).map_err(|e| format!("sandbox: cannot parcel {name}: {e}"))?;
        handoff.lend(parcel.as_fd())
    };
    let seccomp = (seccomp_programs().map_err(Error::new)?.into_iter())
        .map(|program| lend_parcel("ral-seccomp", &program))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Error::new)?;
    let options = bwrap_argv(
        policy,
        chdir,
        address,
        env.host,
        &binds,
        &GpuNodes::discover(),
        &seccomp,
    )
    .map_err(Error::new)?;
    let args = lend_parcel(
        "ral-bwrap-args",
        &nul_terminated(options.iter().map(|arg| arg.as_bytes())).map_err(Error::new)?,
    )
    .map_err(Error::new)?;
    let trampoline = handoff.lend(env.trampoline.fd()).map_err(Error::new)?;
    let mut c = env.bwrap.command();
    c.args(["--args", &args.to_string(), "--"])
        .arg(crate::path::proc_fd_path(trampoline.fd()))
        .args(slots.argv());
    handoff.install(&mut c);
    Ok((c, info, stdio))
}

/// The bwrap options that confine a payload under `policy`: `binds` by
/// slot, layer by layer, and their denies masked last, as planned, at every
/// name a bind shows them; a deny hiding `/proc`, which bwrap execs the trampoline
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
/// `host`, `gpu` and descriptors are data, so a test can assert options for a
/// host it is not running on.  The disk is read for two things only: the
/// real name of `chdir`, against the denies, and, without a virtual `/dev`,
/// which device nodes exist.  Each deny's shape was read once, into [`Shown`].
pub(crate) fn bwrap_argv(
    policy: &RenderedProjection,
    chdir: Option<&CwdPath>,
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
    if policy.net.is_some() {
        c.arg("--unshare-net");
    }
    if let Some(dir) = chdir {
        c.arg("--chdir").arg(dir.as_path());
    }
    match &policy.fs {
        FsProjection::Restricted(_) => {
            // Before the prefix binds: a write prefix under `/tmp` lands inside it.
            c.args(["--tmpfs", "/tmp"]);
            // `--chdir` needs a directory to enter: a cwd outside the grant gets
            // an empty unwritable one, as macOS leaves it unreadable.  Before the
            // binds, which may hide it; `/` and `/tmp` are the envelope's own.
            if let Some(dir) =
                chdir.filter(|dir| !matches!(dir.as_path().as_os_str().as_bytes(), b"/" | b"/tmp"))
            {
                c.args(["--perms", "0555", "--tmpfs"]).arg(dir.as_path());
            }
            binds.render(&mut c, Layer::Shown);
            // After the binds it pins, before every mask: a bind shows the
            // host's tree, so a pin of `p` laid after `p/secret`'s mask buries it.
            binds.render(&mut c, Layer::Spine);
            // After the binds, or a prefix of `/` or `/proc` re-binds the host's over these.
            c.args(["--proc", "/proc"]);
            render_dev(&mut c, host);
            if policy.gpu.is_none() {
                gpu.admit(&mut c);
            }
        }
        FsProjection::Unrestricted => {
            // `--bind` would skip device nodes.  `/proc` goes over the root:
            // a fresh table under `--unshare-pid`, the host's bound without,
            // so `/proc/self` is the payload's own either way.
            c.args(["--dev-bind", "/", "/"]);
            c.args(["--proc", "/proc"]);
            if policy.gpu.is_some() {
                gpu.withhold(&mut c);
            }
        }
    }
    binds.render(&mut c, Layer::Over);
    // Masks go on after every bind: last mount wins.
    if !binds.denies.is_empty() {
        // By stored name, where a mask lands, not folded as an exclusion is;
        // a cwd that resolves nowhere is left to bwrap's `--chdir`.
        let cwd = chdir.and_then(|dir| {
            Some((
                dir,
                RealPath::of(&LexicalPath::absolute(dir.as_path())?).ok()?,
            ))
        });
        for deny in &binds.denies {
            if let Some((dir, at)) = &cwd
                && deny.name.holds_position(at)
            {
                return Err(format!(
                    "sandbox: the working directory {} is excluded by {}, and no confined \
                     command can start inside an exclusion; run it from another directory \
                     (`cd` or `within [dir: …]`)",
                    crate::path::shown(dir.as_path()),
                    deny.name.as_str()
                ));
            }
            for name in binds.names(&deny.name) {
                if name.holds_spelled("/proc") {
                    return Err(format!(
                        "sandbox: the grant denies {}, which hides /proc, and the envelope \
                         needs /proc to start its program; did you mean a path inside it?",
                        name.as_str()
                    ));
                }
                deny.mask.render(&mut c, name.as_str());
            }
        }
    }
    for program in seccomp {
        c.args(["--add-seccomp-fd", &program.to_string()]);
    }
    Ok(c.0)
}

/// The envelope's seccomp programs, each stacked by its own
/// `--add-seccomp-fd` (bwrap refuses that beside `--seccomp`): the kernel
/// applies every installed filter and keeps the most severe result, so which
/// program lands on which slot carries no meaning.  None off the two arches
/// the deny-set is compiled for.
pub(crate) fn seccomp_programs() -> Result<Vec<Vec<u8>>, String> {
    #[cfg(seccomp)]
    {
        seccomp::Filter::ENVELOPE
            .programs()
            .map(|programs| programs.iter().map(serialise).collect())
            .map_err(|e| format!("sandbox: {e}"))
    }
    #[cfg(not(seccomp))]
    {
        Ok(Vec::new())
    }
}

/// Eight bytes per instruction, without a layout-dependent cast.
#[cfg(seccomp)]
fn serialise(prog: seccompiler::BpfProgramRef<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(prog.len() * 8);
    for insn in prog {
        out.extend_from_slice(&insn.code.to_le_bytes());
        out.push(insn.jt);
        out.push(insn.jf);
        out.extend_from_slice(&insn.k.to_le_bytes());
    }
    out
}

/// The envelope's `/dev`: bwrap's `--dev`, or its shape by hand where the
/// host refuses a fresh devpts.
///
/// By hand there is no devpts: the payload keeps the terminals it inherits
/// and allocates none, as any view of the host's shared devpts leaks host
/// terminals.  An absent node is dropped: binding one costs the launch.
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
    ] {
        if crate::path::exists(node) {
            c.args(["--dev-bind", node, node]);
        }
    }
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
    /// group exists, or for every holder's death. Empty means bwrap died first;
    /// an unreadable or malformed record refuses the spawn.
    pub(super) fn payload_pgid(self) -> Result<Option<crate::process::Pgid>, String> {
        #[derive(serde::Deserialize)]
        struct Info {
            #[serde(rename = "child-pid")]
            child_pid: i32,
        }
        let Self {
            mut reader,
            writers,
        } = self;
        drop(writers);
        let mut record = Vec::new();
        reader
            .read_to_end(&mut record)
            .map_err(|e| format!("sandbox: cannot read bwrap's --info-fd record: {e}"))?;
        if record.is_empty() {
            return Ok(None);
        }
        let info: Info = serde_json::from_slice(&record)
            .map_err(|e| format!("sandbox: malformed bwrap --info-fd record: {e}"))?;
        crate::process::Pgid::from_raw(info.child_pid)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "sandbox: invalid bwrap --info-fd child-pid: {}",
                    info.child_pid
                )
            })
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
