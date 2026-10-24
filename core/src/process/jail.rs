//! The guest process jail.
//!
//! [`GuestJail::plan`] decides who a spawned command becomes — a fresh
//! unprivileged uid/gid, a fresh transient cgroup. On a real Linux guest the
//! sequence number behind both is guest-global, read from the counter file
//! `linux::next_guest_seq` locks and increments — every booted engine mints
//! off the one file, since two engines sharing a uid or a cgroup name is
//! exactly the collision a child engine must not risk. Off Linux, where
//! `GuestJail` is never actually installed, `plan` falls back to a plain
//! in-process counter so this module's own tests stay portable; the `linux`
//! submodule remains the only place a decision reaches the kernel.

use std::io;
use std::path::PathBuf;
#[cfg(not(target_os = "linux"))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::sandbox::Ownership;

#[cfg(target_os = "linux")]
pub(crate) mod linux;

/// Cgroup limits applied to every jailed exec.  The numbers are policy for an
/// office workload, not architecture: retune them freely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JailLimits {
    pub(crate) memory_max: u64,
    pub(crate) pids_max: u32,
    pub(crate) cpu_quota_pct: u32,
}

impl Default for JailLimits {
    fn default() -> Self {
        Self {
            memory_max: 2 * 1024 * 1024 * 1024,
            pids_max: 256,
            cpu_quota_pct: 200,
        }
    }
}

static GUEST: OnceLock<Option<Arc<GuestJail>>> = OnceLock::new();

/// Install this process's jail, once: `Some` only under `RAL_GUEST`, which
/// only ral-daemon's closed environment sets, so every engine is jailed alike.
/// Its value is the cgroup root the jail places commands under.
/// Called by `Engine::boot` alone, so no recipe learns that jails exist.
pub(crate) fn install_guest() {
    GUEST.get_or_init(|| {
        #[cfg(target_os = "linux")]
        if let Some(root) = crate::host::var_os("RAL_GUEST") {
            return Some(Arc::new(GuestJail::new(
                PathBuf::from(root),
                100_000,
                JailLimits::default(),
            )));
        }
        None
    });
}

/// The jail this process runs under, `None` anywhere but a real Linux guest
/// or before an engine has booted.
pub(crate) fn guest() -> Option<Arc<GuestJail>> {
    GUEST.get().cloned().flatten()
}

/// The transient cgroup a jailed exec was placed in, owned: dropping it kills
/// whatever is left inside and removes it.  Deliberately free of `cfg`, so
/// `RunningChild` carries an `Option<JailCgroup>` on every platform — `None`
/// anywhere but a real Linux guest.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct JailCgroup {
    path: PathBuf,
}

impl JailCgroup {
    /// A jail over `path`, for tests that stand a plain directory in for the
    /// cgroup.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Kill everything in this cgroup (`cgroup.kill`), leaving removal to [`Drop`].
    #[cfg_attr(
        not(target_os = "linux"),
        allow(clippy::unused_self, reason = "no cgroup exists off Linux")
    )]
    pub(crate) fn kill(&self) {
        #[cfg(target_os = "linux")]
        linux::kill(self);
    }

    /// Leave the cgroup to a detached survivor, unfinished: nobody left can
    /// tell when it is empty, so it keeps the survivor's limits for good.
    #[cfg(unix)]
    pub(crate) fn surrender(self) {
        // Skips the finish, not the path's memory.
        let mut kept = std::mem::ManuallyDrop::new(self);
        drop(std::mem::take(&mut kept.path));
    }
}

#[cfg(target_os = "linux")]
impl Drop for JailCgroup {
    fn drop(&mut self) {
        linux::kill(self);
        linux::remove(self);
    }
}

/// One exec's whole jail decision, reached without touching the kernel.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct JailPlan {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) cgroup: PathBuf,
    pub(crate) limits: JailLimits,
}

/// The guest-global counter file every booted engine's [`GuestJail`] mints
/// its sequence numbers from, on a real Linux guest.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const GUEST_SEQ_FILE: &str = "/run/ral/jail.seq";

/// The tree a detached survivor lives in: beside every engine's, so no
/// engine's teardown reaches it and only the machine's halt does.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DETACHED: &str = "detached";

/// The guest-wide jail: one per booted engine, shared by `Arc` across every
/// fork and worker thread.
///
/// `engine_pid` groups a booted engine's own cgroups under one directory —
/// grouping only, since the sequence file already makes `seq` itself
/// guest-global — so teardown can reach exactly one engine's tree without
/// disturbing a sibling's, or a survivor in [`DETACHED`].
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct GuestJail {
    cgroup_root: PathBuf,
    base_uid: u32,
    limits: JailLimits,
    engine_pid: u32,
    /// Read only by the Linux [`Self::next_seq`]; off Linux, where
    /// `GuestJail` is never actually installed, nothing reads it.
    seq_file: PathBuf,
    /// Unused on a real Linux guest, where [`Self::next_seq`] reads
    /// [`Self::seq_file`] instead; kept so this module's own tests — which
    /// run on every host, not just Linux — still exercise a working `plan`.
    #[cfg(not(target_os = "linux"))]
    counter: AtomicU64,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl GuestJail {
    pub fn new(cgroup_root: PathBuf, base_uid: u32, limits: JailLimits) -> Self {
        Self {
            cgroup_root,
            base_uid,
            limits,
            engine_pid: std::process::id(),
            seq_file: PathBuf::from(GUEST_SEQ_FILE),
            #[cfg(not(target_os = "linux"))]
            counter: AtomicU64::new(0),
        }
    }

    /// Mint the next exec's uid/gid and cgroup: in this engine's tree for a
    /// child it keeps, in [`DETACHED`] for one it surrenders.
    ///
    /// # Errors
    /// Returns the sequence file's io error, on a Linux guest, if it cannot
    /// be opened, locked, read, or written.
    pub(crate) fn plan(&self, ownership: Ownership) -> io::Result<JailPlan> {
        let seq = self.next_seq()?;
        let tree = match ownership {
            Ownership::Kept => format!("engine-{}", self.engine_pid),
            #[cfg(unix)]
            Ownership::Surrendered => DETACHED.to_owned(),
        };
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a session's exec count never approaches u32::MAX"
        )]
        let uid = self.base_uid.wrapping_add(seq as u32);
        Ok(JailPlan {
            uid,
            gid: uid,
            cgroup: self.cgroup_root.join(tree).join(format!("exec-{seq}")),
            limits: self.limits,
        })
    }

    #[cfg(target_os = "linux")]
    fn next_seq(&self) -> io::Result<u64> {
        linux::next_guest_seq(&self.seq_file)
    }

    /// Relaxed suffices: uniqueness wants only the atomicity of the
    /// increment, never an ordering.
    #[cfg(not(target_os = "linux"))]
    #[allow(
        clippy::unnecessary_wraps,
        reason = "matches the Linux arm's fallible signature, which genuinely touches a file"
    )]
    fn next_seq(&self) -> io::Result<u64> {
        Ok(self.counter.fetch_add(1, Ordering::Relaxed))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn jail() -> GuestJail {
        GuestJail::new(
            PathBuf::from("/sys/fs/cgroup/ral-exec"),
            100_000,
            JailLimits::default(),
        )
    }

    /// `None` for exactly the one failure these tests treat as nothing to
    /// test rather than something to report: `/run/ral/jail.seq` needs a
    /// real guest boot's root, absent on an unprivileged CI runner or dev
    /// container. Any other error is still a bug and still panics.
    fn unwrap_or_skip<T>(result: io::Result<T>) -> Option<T> {
        match result {
            Ok(v) => Some(v),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping: {e}; /run/ral/jail.seq needs a real guest boot's \
                     root, absent on this host"
                );
                None
            }
            Err(e) => panic!("plan: {e}"),
        }
    }

    /// The property concurrent spawns from sibling shells depend on.
    #[test]
    fn plan_yields_pairwise_distinct_uids_and_cgroups() {
        let jail = jail();
        let Some(plans) = unwrap_or_skip(
            (0..8)
                .map(|_| jail.plan(Ownership::Kept))
                .collect::<io::Result<Vec<_>>>(),
        ) else {
            return;
        };
        let mut uids: Vec<_> = plans.iter().map(|p| p.uid).collect();
        uids.sort_unstable();
        uids.dedup();
        assert_eq!(uids.len(), plans.len(), "a uid repeated");
        let mut cgroups: Vec<_> = plans.iter().map(|p| p.cgroup.clone()).collect();
        cgroups.sort();
        cgroups.dedup();
        assert_eq!(cgroups.len(), plans.len(), "a cgroup path repeated");
    }

    #[test]
    fn plan_uids_strictly_increase_and_never_yield_root() {
        let jail = jail();
        let Some((a, b)) = unwrap_or_skip(
            jail.plan(Ownership::Kept)
                .and_then(|a| Ok((a, jail.plan(Ownership::Kept)?))),
        ) else {
            return;
        };
        assert!(b.uid > a.uid);
        assert_ne!(a.uid, 0);
        assert_ne!(a.gid, 0);
        assert_ne!(b.uid, 0);
    }

    #[test]
    fn plan_roots_the_cgroup_and_carries_the_jails_limits() {
        let Some(plan) = unwrap_or_skip(jail().plan(Ownership::Kept)) else {
            return;
        };
        assert!(plan.cgroup.starts_with("/sys/fs/cgroup/ral-exec"));
        assert_eq!(plan.limits, JailLimits::default());
    }

    #[cfg(unix)]
    #[test]
    fn plan_puts_a_survivor_beyond_every_engines_tree() {
        let jail = jail();
        let Some((kept, gone)) = unwrap_or_skip(
            jail.plan(Ownership::Kept)
                .and_then(|k| Ok((k, jail.plan(Ownership::Surrendered)?))),
        ) else {
            return;
        };
        let root = PathBuf::from("/sys/fs/cgroup/ral-exec");
        assert!(
            kept.cgroup
                .starts_with(root.join(format!("engine-{}", std::process::id())))
        );
        assert_eq!(gone.cgroup.parent(), Some(root.join(DETACHED).as_path()));
    }

    /// A plain directory stands in for the cgroup: the finish's `cgroup.kill`
    /// write lands in it as a file, which keeps it there to be seen.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_dropped_jail_is_finished_and_a_surrendered_one_is_not() {
        let root = tempfile::tempdir().unwrap();
        let leaf = |name: &str| {
            let path = root.path().join(name);
            std::fs::create_dir(&path).unwrap();
            JailCgroup::at(path)
        };
        drop(leaf("dropped"));
        leaf("surrendered").surrender();
        let killed = |name: &str| root.path().join(name).join("cgroup.kill").exists();
        assert!(killed("dropped"), "a dropped jail must be finished");
        assert!(
            !killed("surrendered"),
            "a surrendered jail is its survivor's"
        );
    }

    /// The cgroup is owned from its `mkdir`: a setup failing after it (here
    /// at `cgroup.procs`, which a plain directory lacks) finishes it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_jail_whose_setup_fails_part_way_is_finished() {
        let root = tempfile::tempdir().unwrap();
        let plan = JailPlan {
            uid: 100_000,
            gid: 100_000,
            cgroup: root.path().join("exec-0"),
            limits: JailLimits::default(),
        };
        assert!(
            linux::prepare(&plan).is_err(),
            "a plain directory has no cgroup.procs"
        );
        assert!(
            plan.cgroup.join("cgroup.kill").exists(),
            "the half-made jail must be finished"
        );
    }
}
