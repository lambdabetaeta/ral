//! The guest process jail.
//!
//! [`GuestJail::plan`] decides who a spawned command becomes — a fresh
//! unprivileged uid/gid, a fresh transient cgroup. On a real Linux guest the
//! sequence number behind both is guest-global, read from the counter file
//! `linux::next_guest_seq` locks and increments — every booted engine mints
//! off the one file, since two engines sharing a uid or a cgroup name is
//! exactly the collision a hatched child must not risk. Off Linux, where
//! `GuestJail` is never actually installed, `plan` falls back to a plain
//! in-process counter so this module's own tests stay portable; the `linux`
//! submodule remains the only place a decision reaches the kernel.

use std::io;
use std::path::PathBuf;
#[cfg(not(target_os = "linux"))]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "linux")]
pub(crate) mod linux;

/// Cgroup limits applied to every jailed exec.  The numbers are policy for an
/// office workload, not architecture: retune them freely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JailLimits {
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

/// The transient cgroup a jailed exec was placed in.  Deliberately free of
/// `cfg`, so `RunningChild` carries an `Option<JailCgroup>` on every platform
/// — `None` anywhere but a real Linux guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JailCgroup {
    path: PathBuf,
}

/// One exec's whole jail decision, reached without touching the kernel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JailPlan {
    pub uid: u32,
    pub gid: u32,
    pub cgroup: PathBuf,
    pub limits: JailLimits,
}

/// The guest-global counter file every booted engine's [`GuestJail`] mints
/// its sequence numbers from, on a real Linux guest.
const GUEST_SEQ_FILE: &str = "/run/ral/jail.seq";

/// The guest-wide jail: one per booted engine, shared by `Arc` across every
/// fork and worker thread.
///
/// `engine_pid` groups a booted engine's own cgroups under one directory —
/// grouping only, since the sequence file already makes `seq` itself
/// guest-global — so teardown can reach exactly one engine's tree without
/// disturbing a sibling's.
pub struct GuestJail {
    cgroup_root: PathBuf,
    base_uid: u32,
    limits: JailLimits,
    engine_pid: u32,
    /// Read only by the Linux [`Self::next_seq`]; off Linux, where
    /// `GuestJail` is never actually installed, nothing reads it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    seq_file: PathBuf,
    /// Unused on a real Linux guest, where [`Self::next_seq`] reads
    /// [`Self::seq_file`] instead; kept so this module's own tests — which
    /// run on every host, not just Linux — still exercise a working `plan`.
    #[cfg(not(target_os = "linux"))]
    counter: AtomicU64,
}

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

    /// Mint the next exec's uid/gid and cgroup.
    ///
    /// # Errors
    /// Returns the sequence file's io error, on a Linux guest, if it cannot
    /// be opened, locked, read, or written.
    pub fn plan(&self) -> io::Result<JailPlan> {
        let seq = self.next_seq()?;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a session's exec count never approaches u32::MAX"
        )]
        let uid = self.base_uid.wrapping_add(seq as u32);
        Ok(JailPlan {
            uid,
            gid: uid,
            cgroup: self
                .cgroup_root
                .join(format!("engine-{}", self.engine_pid))
                .join(format!("exec-{seq}")),
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
mod tests {
    use super::*;

    fn jail() -> GuestJail {
        GuestJail::new(
            PathBuf::from("/sys/fs/cgroup/ral-exec"),
            100_000,
            JailLimits::default(),
        )
    }

    /// The property concurrent spawns from sibling shells depend on.
    #[test]
    fn plan_yields_pairwise_distinct_uids_and_cgroups() {
        let jail = jail();
        let plans: Vec<_> = (0..8).map(|_| jail.plan().expect("plan")).collect();
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
        let a = jail.plan().expect("plan");
        let b = jail.plan().expect("plan");
        assert!(b.uid > a.uid);
        assert_ne!(a.uid, 0);
        assert_ne!(a.gid, 0);
        assert_ne!(b.uid, 0);
    }

    #[test]
    fn plan_roots_the_cgroup_and_carries_the_jails_limits() {
        let plan = jail().plan().expect("plan");
        assert!(plan.cgroup.starts_with("/sys/fs/cgroup/ral-exec"));
        assert_eq!(plan.limits, JailLimits::default());
    }
}
