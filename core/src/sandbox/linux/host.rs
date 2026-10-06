//! What this host's `bwrap` can actually build.  A container runtime that
//! masks `/proc` (crun, runc, Docker) makes the kernel refuse a fresh procfs
//! in a user namespace, and refuses bwrap a fresh devpts; neither is a promise
//! a grant makes, so each is an invariant held where the host allows and
//! stated, not refused, where not.  Probed once, so the argv render stays
//! pure in it.

use super::landlock::Landlock;
use crate::sandbox::reexec::Pinned;
use std::process::Stdio;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostEnvelope {
    /// `--unshare-pid` with a fresh `/proc` mounts.
    pub(crate) private_pids: bool,
    /// `--dev /dev` mounts; otherwise [`super::render_dev`] stands in.
    pub(crate) virtual_dev: bool,
    /// `--unshare-cgroup` builds, so [`super::render_cgroup`] re-roots the
    /// tree; otherwise the payload sees the host's.
    pub(crate) private_cgroup: bool,
    /// What the kernel's Landlock version probe answered.
    pub(crate) landlock: Landlock,
}

impl HostEnvelope {
    /// 2–4 ms a spawn, and no answer changes under a running ral.
    pub(crate) fn probe(envelope: &Pinned) -> Self {
        static PROBED: OnceLock<HostEnvelope> = OnceLock::new();
        *PROBED.get_or_init(|| Self {
            private_pids: bwrap_builds(envelope, &["--unshare-pid", "--proc", "/proc"]),
            virtual_dev: bwrap_builds(envelope, &["--dev", "/dev"]),
            private_cgroup: bwrap_builds(envelope, &["--unshare-cgroup"]),
            landlock: Landlock::probe(),
        })
    }
}

/// Whether the pinned bwrap builds an envelope carrying `pieces` and reaches
/// its payload.  Streams are discarded: a refusal is our datum, not a message
/// to the user.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:bwrap-host-probe] setup-time host capability probe against /bin/true, not a model exec image"
)]
fn bwrap_builds(envelope: &Pinned, pieces: &[&str]) -> bool {
    envelope
        .command()
        .args(["--ro-bind", "/", "/"])
        .args(pieces)
        .args(["--", "/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
