//! What this host's `bwrap` can actually build.
//!
//! A rootless container's mount layer refuses bwrap a fresh devpts, so `--dev`
//! dies in setup there.  Probed once and carried as a value: the argv render
//! stays pure in it, and a test can name a host it is not running on.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// The envelope pieces this host grants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostEnvelope {
    /// Whether `--dev /dev` mounts.  False under rootless podman, crun and
    /// Docker, where [`super::render_dev`] stands in for it.
    pub(crate) virtual_dev: bool,
}

impl HostEnvelope {
    /// Probed once: 2–4 ms a spawn, and no answer changes under a running ral.
    pub(crate) fn probe() -> Self {
        static PROBED: OnceLock<HostEnvelope> = OnceLock::new();
        *PROBED.get_or_init(|| Self {
            virtual_dev: bwrap_builds(&["--dev", "/dev"]),
        })
    }
}

/// Whether bwrap builds an envelope carrying `pieces` and reaches its payload.
/// Streams are discarded: a refusal is our datum, not a message to the user.
/// A host without bwrap answers no to everything, harmlessly — the launch that
/// follows fails on the same missing binary.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:bwrap-host-probe] setup-time host capability probe against /bin/true, not a model exec image"
)]
fn bwrap_builds(pieces: &[&str]) -> bool {
    Command::new(super::BWRAP)
        .args(["--ro-bind", "/", "/"])
        .args(pieces)
        .args(["--", "/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
