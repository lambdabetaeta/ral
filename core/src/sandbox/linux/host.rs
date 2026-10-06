//! What this host's `bwrap` can actually build.  A container runtime that
//! masks `/proc` (crun, runc, Docker) makes the kernel refuse a fresh procfs
//! in a user namespace, and refuses bwrap a fresh devpts; neither is a promise
//! a grant makes, so each is an invariant held where the host allows and
//! stated, not refused, where not.  Probed once, so the argv render stays
//! pure in it.

use super::landlock::Landlock;
use crate::runtime::pipeline::helper::ANCHOR_FLAG;
use crate::sandbox::reexec::Pinned;
use crate::sandbox::warrant::{Handoff, Slot};
use rustix::fs::OFlags;
use std::os::fd::AsFd;
use std::process::Stdio;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent facts, each its own probe"
)]
pub(crate) struct HostEnvelope {
    /// `--unshare-pid` with a fresh `/proc` mounts.
    pub(crate) private_pids: bool,
    /// `--dev /dev` mounts; otherwise [`super::render_dev`] stands in.
    pub(crate) virtual_dev: bool,
    /// `--unshare-cgroup` builds, so the cgroup tree is re-rooted on ral's
    /// own; otherwise the payload sees the host's.
    pub(crate) private_cgroup: bool,
    /// What the kernel's Landlock version probe answered.
    pub(crate) landlock: Landlock,
    pub(crate) builds: Builds,
}

/// What the pinned bwrap builds on this host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Builds {
    /// Envelopes mounting lent descriptors (`--ro-bind-fd`, bubblewrap
    /// 0.8.0): the only way the envelope mounts anything.
    ByFd,
    /// Envelopes, but none mounting a descriptor.
    ByName,
    /// No envelope at all, for the reason bwrap gave.
    Nothing(&'static str),
}

impl HostEnvelope {
    /// 2–4 ms a spawn, and no answer changes under a running ral.
    pub(crate) fn probe(bwrap: &Pinned, trampoline: &Pinned) -> Self {
        static PROBED: OnceLock<HostEnvelope> = OnceLock::new();
        *PROBED.get_or_init(|| {
            let builds = |pieces: &[&str]| {
                bwrap_builds(bwrap, trampoline, pieces, Handoff::default()).is_ok()
            };
            Self {
                private_pids: builds(&["--unshare-pid", "--proc", "/proc"]),
                virtual_dev: builds(&["--dev", "/dev"]),
                private_cgroup: builds(&["--unshare-cgroup"]),
                landlock: Landlock::probe(),
                builds: Builds::probe(bwrap, trampoline),
            }
        })
    }
}

impl Builds {
    /// A bare envelope first, so a bwrap that builds none is never blamed
    /// on its version; then `/` mounted from a descriptor lent at the first
    /// mount slot, as every launch's binds are.
    fn probe(bwrap: &Pinned, trampoline: &Pinned) -> Self {
        if let Err(why) = bwrap_builds(bwrap, trampoline, &[], Handoff::default()) {
            // Leaked once: the probe runs once a process, and keeps the fact `Copy`.
            return Self::Nothing(why.leak());
        }
        let Ok(Some(root)) = super::open_real("/".as_ref(), OFlags::empty()) else {
            return Self::Nothing("ral cannot open / to lend bwrap a descriptor");
        };
        let mut handoff = Handoff::default();
        handoff.lend(Slot::Mount(0), root.as_fd());
        let at = Slot::Mount(0).fd().to_string();
        match bwrap_builds(bwrap, trampoline, &["--ro-bind-fd", &at, "/"], handoff) {
            Ok(()) => Self::ByFd,
            Err(_) => Self::ByName,
        }
    }
}

/// Whether the pinned bwrap builds an envelope carrying `pieces`, handed
/// `handoff`, and execs `trampoline` in it by descriptor, as a launch does;
/// as a pipeline anchor, it reads its empty stdin and exits.  bwrap's own
/// words where not, for the one probe that reports them.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:bwrap-host-probe] setup-time host capability probe running ral's own pin as an anchor, not a model exec image"
)]
fn bwrap_builds<'a>(
    bwrap: &Pinned,
    trampoline: &'a Pinned,
    pieces: &[&str],
    mut handoff: Handoff<'a>,
) -> Result<(), String> {
    handoff.lend(Slot::Trampoline, trampoline.fd());
    let mut cmd = bwrap.command();
    cmd.args(["--ro-bind", "/", "/"])
        .args(pieces)
        .arg("--")
        .arg(crate::path::proc_fd_path(Slot::Trampoline.fd()))
        .arg(ANCHOR_FLAG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    handoff.install(&mut cmd)?;
    let out = cmd.output().map_err(|e| format!("it did not start: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(match String::from_utf8_lossy(&out.stderr).trim() {
        "" => format!("it exited with {}", out.status),
        said => said.to_owned(),
    })
}
