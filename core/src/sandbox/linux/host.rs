//! What this host's `bwrap` can actually build.  A container runtime that
//! masks `/proc` (crun, runc, Docker) makes the kernel refuse a fresh procfs
//! in a user namespace, and refuses bwrap a fresh devpts; neither is a promise
//! a grant makes, so each is an invariant held where the host allows and
//! stated, not refused, where not.  Probed once, so the argv render stays
//! pure in it.

use super::landlock::{Landlock, open_nosym};
use crate::sandbox::reexec::Pinned;
use crate::sandbox::warrant::{Handoff, Slot};
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
    /// `--ro-bind-fd` mounts a lent descriptor (bubblewrap 0.8.0): the only
    /// way the envelope mounts anything.
    pub(crate) binds_by_fd: bool,
}

impl HostEnvelope {
    /// 2–4 ms a spawn, and no answer changes under a running ral.
    pub(crate) fn probe(envelope: &Pinned) -> Self {
        static PROBED: OnceLock<HostEnvelope> = OnceLock::new();
        *PROBED.get_or_init(|| Self {
            private_pids: bwrap_builds(
                envelope,
                &["--unshare-pid", "--proc", "/proc"],
                Handoff::default(),
            ),
            virtual_dev: bwrap_builds(envelope, &["--dev", "/dev"], Handoff::default()),
            private_cgroup: bwrap_builds(envelope, &["--unshare-cgroup"], Handoff::default()),
            landlock: Landlock::probe(),
            binds_by_fd: binds_by_fd(envelope),
        })
    }
}

/// Whether the pinned bwrap mounts `/` from a descriptor lent at the first
/// mount slot, as every launch's binds are.
fn binds_by_fd(envelope: &Pinned) -> bool {
    let Ok(root) = open_nosym("/".as_ref(), rustix::fs::OFlags::empty()) else {
        return false;
    };
    let mut handoff = Handoff::default();
    handoff.lend(Slot::Mount(0), root.as_fd());
    let at = Slot::Mount(0).fd().to_string();
    bwrap_builds(envelope, &["--ro-bind-fd", &at, "/"], handoff)
}

/// Whether the pinned bwrap builds an envelope carrying `pieces`, handed
/// `handoff`, and reaches its payload.  Streams are discarded: a refusal is
/// our datum, not a message to the user.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:bwrap-host-probe] setup-time host capability probe against /bin/true, not a model exec image"
)]
fn bwrap_builds(envelope: &Pinned, pieces: &[&str], handoff: Handoff<'_>) -> bool {
    let mut cmd = envelope.command();
    cmd.args(["--ro-bind", "/", "/"])
        .args(pieces)
        .args(["--", "/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    handoff.install(&mut cmd).is_ok() && cmd.status().is_ok_and(|status| status.success())
}
