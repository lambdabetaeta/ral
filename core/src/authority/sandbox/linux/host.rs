//! What this host's `bwrap` can actually build.  A container runtime that
//! masks `/proc` (crun, runc, Docker) makes the kernel refuse a fresh procfs
//! in a user namespace, and refuses bwrap a fresh devpts; neither is a promise
//! a grant makes, so each is an invariant held where the host allows and
//! stated, not refused, where not.  Probed once, so the argv render stays
//! pure in it.

use super::landlock::Landlock;
use crate::Role;
use crate::authority::sandbox::reexec::Pinned;
use crate::authority::sandbox::warrant::Handoff;
use crate::path::RealPath;
use crate::site::{Silent, site};
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
    /// on its version; then `/` mounted from a lent descriptor, as every
    /// launch's binds are.
    fn probe(bwrap: &Pinned, trampoline: &Pinned) -> Self {
        if let Err(why) = bwrap_builds(bwrap, trampoline, &[], Handoff::default()) {
            // Leaked once: the probe runs once a process, and keeps the fact `Copy`.
            return Self::Nothing(why.leak());
        }
        let Ok(Some(root)) = super::open_dir_nofollow(&RealPath::universal()) else {
            return Self::Nothing("ral cannot open / to lend bwrap a descriptor");
        };
        let mut handoff = Handoff::default();
        let at = match handoff.lend(root.as_fd()) {
            Ok(at) => at,
            // Leaked once, as above: it carries `lift`'s `ulimit -n` hint.
            Err(why) => return Self::Nothing(why.leak()),
        };
        match bwrap_builds(
            bwrap,
            trampoline,
            &["--ro-bind-fd", &at.to_string(), "/"],
            handoff,
        ) {
            Ok(()) => Self::ByFd,
            Err(why) => {
                let unknown = why.contains("--ro-bind-fd")
                    && (why.contains("Unknown option")
                        || why.contains("unknown option")
                        || why.contains("unrecognized option"));
                let why = if unknown {
                    format!("{why}; is bubblewrap older than 0.8.0?")
                } else {
                    why
                };
                Self::Nothing(why.leak())
            }
        }
    }
}

/// Whether the pinned bwrap builds an envelope carrying `pieces`, handed
/// `handoff`, and execs `trampoline` in it by descriptor, as a launch does;
/// as a pipeline anchor, it reads its empty stdin and exits.  bwrap's own
/// words where not, for the one probe that reports them.
fn bwrap_builds(
    bwrap: &Pinned,
    trampoline: &Pinned,
    pieces: &[&str],
    mut handoff: Handoff,
) -> Result<(), String> {
    let at = handoff.lend(trampoline.fd())?;
    let mut cmd = bwrap.command();
    cmd.args(["--ro-bind", "/", "/"])
        .args(pieces)
        .arg("--")
        .arg(crate::path::proc_fd_path(at.fd()))
        .arg(Role::PipelineAnchor.flag())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    handoff.install(&mut cmd);
    let out = site!(Silent::BwrapHostProbe, &mut cmd => output())
        .map_err(|e| format!("it did not start: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(match String::from_utf8_lossy(&out.stderr).trim() {
        "" => format!("it exited with {}", out.status),
        said => said.to_owned(),
    })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_failed_fd_probe_keeps_bwraps_reason_and_versions_only_unknown_options() {
        let dir = tempfile::tempdir().unwrap();
        let trampoline = Pinned::open(std::env::current_exe().unwrap()).unwrap();
        for (index, reason, outdated) in [
            (0, "bwrap: Unknown option --ro-bind-fd", true),
            (
                1,
                "bwrap: Creating new namespace failed: Operation not permitted",
                false,
            ),
            (2, "bwrap: --ro-bind-fd: Bad file descriptor", false),
        ] {
            let path = dir.path().join(format!("bwrap-{index}"));
            std::fs::write(&path, format!(
                "#!/bin/sh\ncase \"$*\" in\n*--ro-bind-fd*) printf '%s\\n' '{reason}' >&2; exit 1;;\nesac\nexit 0\n"
            )).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            let bwrap = Pinned::open(path).unwrap();
            // The script interpreter must reopen the pin after exec.
            assert_eq!(
                unsafe { libc::fcntl(bwrap.fd().as_raw_fd(), libc::F_SETFD, 0) },
                0
            );
            let Builds::Nothing(why) = Builds::probe(&bwrap, &trampoline) else {
                panic!("a failed descriptor mount must refuse the envelope");
            };
            assert!(why.contains(reason), "{why}");
            assert_eq!(
                why.contains("is bubblewrap older than 0.8.0?"),
                outdated,
                "{why}"
            );
        }
    }
}
