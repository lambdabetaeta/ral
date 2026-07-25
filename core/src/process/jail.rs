//! The guest process jail: a pure decision layer, plus (Linux only) the
//! thin syscall edge that carries a decision out.
//!
//! [`GuestJail::plan`] decides who a spawned command becomes — a fresh
//! unprivileged uid/gid and a fresh transient cgroup — from nothing but
//! an atomic counter; it performs no syscall, so it is exercised directly
//! in unit tests on every platform.  `linux` is the thin edge that
//! turns one [`JailPlan`] into the mkdir/write/setuid sequence the kernel
//! enforces, mirroring the `sandbox/linux.rs` / `signal/unix.rs` split of
//! a platform-only edge from a portable decision.  [`JailCgroup`] is
//! plain data with no methods and no `cfg`, so `RunningChild` can carry
//! an `Option<JailCgroup>` uniformly on every platform — simply always
//! `None` off a real guest.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "linux")]
pub(crate) mod linux;

/// Cgroup limits applied to every jailed exec.
///
/// Conservative starting values; the exact numbers are a policy decision
/// for the office-workload profile (`LibreOffice` conversion, pandoc, a
/// Python pass over a big spreadsheet), not an architectural one.
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

/// The transient cgroup a jailed exec was placed in.
///
/// Plain data: no methods, no `cfg`, so it sits on `RunningChild`
/// uniformly on every platform and is simply `None` everywhere but a real
/// Linux guest.  Opaque outside this crate: an embedder can only thread
/// it through [`Launch::spawn`](crate::process::Launch::spawn)'s tuple,
/// or ignore it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JailCgroup {
    path: PathBuf,
}

/// The whole per-exec decision: which uid/gid a spawned command drops to,
/// which cgroup confines it, and under what limits.  Printable and
/// comparable so it is testable without a syscall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JailPlan {
    pub uid: u32,
    pub gid: u32,
    pub cgroup: PathBuf,
    pub limits: JailLimits,
}

/// The guest-wide jail: one per booted engine, shared by `Arc` across
/// every fork and spawned worker so concurrent spawns from sibling Shells
/// still mint distinct uids and cgroups off the same counter.
pub struct GuestJail {
    cgroup_root: PathBuf,
    base_uid: u32,
    limits: JailLimits,
    counter: AtomicU64,
}

impl GuestJail {
    pub fn new(cgroup_root: PathBuf, base_uid: u32, limits: JailLimits) -> Self {
        Self {
            cgroup_root,
            base_uid,
            limits,
            counter: AtomicU64::new(0),
        }
    }

    /// Decide the next exec's uid/gid and cgroup.  One atomic increment,
    /// no syscalls — exercised directly in unit tests on every platform.
    pub fn plan(&self) -> JailPlan {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a session's exec count never approaches u32::MAX"
        )]
        let uid = self.base_uid.wrapping_add(seq as u32);
        JailPlan {
            uid,
            gid: uid,
            cgroup: self.cgroup_root.join(format!("exec-{seq}")),
            limits: self.limits,
        }
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

    /// Successive plans never repeat a uid or a cgroup path — the whole
    /// point of the counter, and the property concurrent spawns depend on.
    #[test]
    fn plan_yields_pairwise_distinct_uids_and_cgroups() {
        let jail = jail();
        let plans: Vec<_> = (0..8).map(|_| jail.plan()).collect();
        let mut uids: Vec<_> = plans.iter().map(|p| p.uid).collect();
        uids.sort_unstable();
        uids.dedup();
        assert_eq!(uids.len(), plans.len(), "a uid repeated");
        let mut cgroups: Vec<_> = plans.iter().map(|p| p.cgroup.clone()).collect();
        cgroups.sort();
        cgroups.dedup();
        assert_eq!(cgroups.len(), plans.len(), "a cgroup path repeated");
    }

    /// Uids strictly increase with the sequence, and neither uid nor gid
    /// is ever root.
    #[test]
    fn plan_uids_strictly_increase_and_never_yield_root() {
        let jail = jail();
        let a = jail.plan();
        let b = jail.plan();
        assert!(b.uid > a.uid);
        assert_ne!(a.uid, 0);
        assert_ne!(a.gid, 0);
        assert_ne!(b.uid, 0);
    }

    /// A plan's cgroup lives under the jail's root, and the plan carries
    /// the jail's own limits, not a default of its own.
    #[test]
    fn plan_roots_the_cgroup_and_carries_the_jails_limits() {
        let plan = jail().plan();
        assert!(plan.cgroup.starts_with("/sys/fs/cgroup/ral-exec"));
        assert_eq!(plan.limits, JailLimits::default());
    }
}
