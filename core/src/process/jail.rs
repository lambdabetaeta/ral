//! The guest process jail.
//!
//! [`GuestJail::plan`] decides who a spawned command becomes — a fresh
//! unprivileged uid/gid, a kept cgroup or shared survivor membership. On Linux the
//! sequence number behind both is guest-global, read from the counter file
//! `linux::next_guest_seq` locks and increments — every booted engine mints
//! off the one file, since two engines sharing a uid or a kept cgroup name is
//! exactly the collision a child engine must not risk. Off Linux, where
//! `GuestJail` is never actually installed, `plan` falls back to a plain
//! in-process counter so this module's own tests stay portable; the `linux`
//! submodule remains the only place a decision reaches the kernel.

use crate::fact::{JailFailure, JailFailureSequence, LaunchFailure, SequenceFailure};
#[cfg(any(test, seccomp))]
use std::ffi::OsString;
use std::path::PathBuf;
#[cfg(seccomp)]
use std::sync::OnceLock;
#[cfg(not(target_os = "linux"))]
use std::sync::atomic::{AtomicU32, Ordering};

use crate::authority::policy::Net;
use crate::authority::sandbox::Ownership;

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

#[cfg(seccomp)]
static GUEST: OnceLock<GuestJail> = OnceLock::new();

/// Why the guest jail made no child of a launch: nothing ran.
#[cfg(seccomp)]
pub(crate) fn refused(why: LaunchFailure) -> crate::error::Error {
    super::launch::refused(why)
}

#[cfg(seccomp)]
pub(crate) fn install_guest(args: &[OsString]) -> Result<(), String> {
    // SAFETY: geteuid has no preconditions.
    let root = guest_root(args, unsafe { libc::geteuid() })?;
    GUEST.get_or_init(|| GuestJail::new(root, JailLimits::default()));
    Ok(())
}

#[cfg(any(test, seccomp))]
fn guest_root(args: &[OsString], euid: u32) -> Result<PathBuf, String> {
    let [arg] = args else {
        return Err("a guest engine takes one argument, the jail's cgroup root".into());
    };
    let root = PathBuf::from(arg);
    if !crate::path::is_absolute(&root) {
        return Err(format!(
            "the jail's cgroup root must be an absolute path, not {}",
            crate::path::shown(&root)
        ));
    }
    if euid != 0 {
        return Err(format!(
            "a guest engine runs as root, since its jail drops each command to a fresh uid; \
             this one is uid {euid}"
        ));
    }
    Ok(root)
}

#[cfg(seccomp)]
pub(crate) fn guest() -> Option<&'static GuestJail> {
    GUEST.get()
}

pub(crate) const BASE_UID: u32 = 100_000;
pub(super) const SPAN: u32 = u32::MAX - BASE_UID;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Seq(u32);

impl Seq {
    fn new(n: u32) -> Option<Self> {
        (n < SPAN).then_some(Self(n))
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn uid(self) -> u32 {
        BASE_UID + self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Count(u32);

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Count {
    fn encode(self) -> [u8; 10] {
        let mut digits = [b'0'; 10];
        let mut n = self.0;
        for digit in digits.iter_mut().rev() {
            *digit += u8::try_from(n % 10).expect("a decimal digit fits in u8");
            n /= 10;
        }
        digits
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() {
            return Some(Self(0));
        }
        let digits: &[u8; 10] = bytes.try_into().ok()?;
        digits
            .iter()
            .try_fold(0_u32, |n, &digit| {
                digit.is_ascii_digit().then_some(())?;
                n.checked_mul(10)?.checked_add(u32::from(digit - b'0'))
            })
            .map(Self)
    }

    fn take(self) -> Option<(Seq, Self)> {
        Seq::new(self.0).map(|seq| (seq, Self(self.0 + 1)))
    }
}

fn exhausted() -> SequenceFailure {
    SequenceFailure::Exhausted
}

/// The transient cgroup a jailed exec was placed in, owned: dropping it kills
/// whatever is left inside and removes it.  Deliberately free of `cfg`, so
/// an external stage carries an `Option<JailCgroup>` on every platform —
/// `None` anywhere but a real Linux guest.
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
    pub(crate) ownership: Ownership,
    pub(crate) seq: Seq,
    pub(crate) net: Option<Net>,
    pub(crate) cgroup: PathBuf,
    pub(crate) limits: JailLimits,
}

/// The guest-global counter file every booted engine's [`GuestJail`] mints
/// its sequence numbers from, on a real Linux guest.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const GUEST_SEQ_FILE: &str = "/run/ral/jail.seq";

/// The one cgroup every guest survivor shares: beside every engine's tree,
/// beyond its teardown, and limited as one exec is.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DETACHED: &str = "detached";

/// The guest-wide jail, installed once per engine and borrowed by its workers.
///
/// `engine_pid` groups a booted engine's own cgroups under one directory —
/// grouping only, since the sequence file already makes `seq` itself
/// guest-global — so teardown can reach exactly one engine's tree without
/// disturbing a sibling's, or a survivor in [`DETACHED`].
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct GuestJail {
    cgroup_root: PathBuf,
    limits: JailLimits,
    engine_pid: u32,
    /// Read only by the Linux [`Self::next_seq`]; off Linux, where
    /// `GuestJail` is never actually installed, nothing reads it.
    seq_file: PathBuf,
    /// Unused on a real Linux guest, where [`Self::next_seq`] reads
    /// [`Self::seq_file`] instead; kept so this module's own tests — which
    /// run on every host, not just Linux — still exercise a working `plan`.
    #[cfg(not(target_os = "linux"))]
    counter: AtomicU32,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl GuestJail {
    pub fn new(cgroup_root: PathBuf, limits: JailLimits) -> Self {
        Self {
            cgroup_root,
            limits,
            engine_pid: std::process::id(),
            seq_file: PathBuf::from(GUEST_SEQ_FILE),
            #[cfg(not(target_os = "linux"))]
            counter: AtomicU32::new(0),
        }
    }

    /// Mint the next exec's uid/gid and cgroup: in this engine's tree for a
    /// child it keeps, in [`DETACHED`] for one it surrenders.
    ///
    /// # Errors
    /// Returns the sequence file's io error, on a Linux guest, if it cannot
    /// be opened, locked, read, or written.
    pub(crate) fn plan(
        &self,
        ownership: Ownership,
        net: Option<Net>,
    ) -> Result<JailPlan, LaunchFailure> {
        let seq = self.next_seq()?;
        let cgroup = match ownership {
            Ownership::Kept => self
                .cgroup_root
                .join(format!("engine-{}", self.engine_pid))
                .join(format!("exec-{}", seq.0)),
            #[cfg(unix)]
            Ownership::Surrendered => self.cgroup_root.join(DETACHED),
        };
        Ok(JailPlan {
            ownership,
            seq,
            net,
            cgroup,
            limits: self.limits,
        })
    }

    #[cfg(target_os = "linux")]
    fn next_seq(&self) -> Result<Seq, LaunchFailure> {
        linux::next_guest_seq(&self.seq_file).map_err(|cause| {
            LaunchFailure::Jail(JailFailure::Sequence(JailFailureSequence {
                path: crate::path::shown(&self.seq_file),
                cause,
            }))
        })
    }

    /// Relaxed suffices: uniqueness wants only the atomicity of the
    /// increment, never an ordering.
    #[cfg(not(target_os = "linux"))]
    fn next_seq(&self) -> Result<Seq, LaunchFailure> {
        self.counter
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Seq::new(n).map(|_| n + 1)
            })
            .ok()
            .and_then(Seq::new)
            .ok_or_else(|| {
                LaunchFailure::Jail(JailFailure::Sequence(JailFailureSequence {
                    path: crate::path::shown(&self.seq_file),
                    cause: exhausted(),
                }))
            })
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn jail() -> GuestJail {
        GuestJail::new(
            PathBuf::from("/sys/fs/cgroup/ral-exec"),
            JailLimits::default(),
        )
    }

    /// `None` for exactly the one failure these tests treat as nothing to
    /// test rather than something to report: `/run/ral/jail.seq` needs a
    /// real guest boot's root, absent on an unprivileged CI runner or dev
    /// container. Any other error is still a bug and still panics.
    fn unwrap_or_skip<T>(result: Result<T, LaunchFailure>) -> Option<T> {
        match result {
            Ok(v) => Some(v),
            Err(e)
                if matches!(&e, LaunchFailure::Jail(JailFailure::Sequence(JailFailureSequence { cause: SequenceFailure::CreateParent(cause) | SequenceFailure::Open(cause), .. }))
                if matches!(cause.leaf.as_ref(), crate::fact::IoLeaf::Os(os) if os.kind == crate::fact::IoKind::PermissionDenied)) =>
            {
                eprintln!(
                    "skipping: {}; /run/ral/jail.seq needs a real guest boot's \
                     root, absent on this host",
                    e.describe()
                );
                None
            }
            Err(e) => panic!("plan: {}", e.describe()),
        }
    }

    /// The property concurrent spawns from sibling shells depend on.
    #[test]
    fn plan_yields_pairwise_distinct_uids_and_cgroups() {
        let jail = jail();
        let Some(plans) = unwrap_or_skip(
            (0..8)
                .map(|_| jail.plan(Ownership::Kept, None))
                .collect::<Result<Vec<_>, _>>(),
        ) else {
            return;
        };
        let mut uids: Vec<_> = plans.iter().map(|p| p.seq.uid()).collect();
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
            jail.plan(Ownership::Kept, None)
                .and_then(|a| Ok((a, jail.plan(Ownership::Kept, None)?))),
        ) else {
            return;
        };
        assert!(b.seq.uid() > a.seq.uid());
        assert_ne!(a.seq.uid(), 0);
        assert_ne!(b.seq.uid(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn plan_puts_a_survivor_beyond_every_engines_tree() {
        let jail = jail();
        let Some((kept, gone)) = unwrap_or_skip(
            jail.plan(Ownership::Kept, None)
                .and_then(|k| Ok((k, jail.plan(Ownership::Surrendered, None)?))),
        ) else {
            return;
        };
        let root = PathBuf::from("/sys/fs/cgroup/ral-exec");
        assert!(
            kept.cgroup
                .starts_with(root.join(format!("engine-{}", std::process::id())))
        );
        assert_eq!(gone.cgroup, root.join(DETACHED));
        let Some(other) = unwrap_or_skip(jail.plan(Ownership::Surrendered, None)) else {
            return;
        };
        assert_eq!(gone.cgroup, other.cgroup);
        assert_ne!(gone.seq.uid(), other.seq.uid());
    }

    /// A missing `cgroup.procs` fails after limit writes. The ordinary
    /// directory records whether the real teardown consumer wrote `cgroup.kill`.
    #[cfg(target_os = "linux")]
    #[test]
    fn partial_preparation_finishes_only_a_kept_cgroup() {
        let root = tempfile::tempdir().unwrap();
        for ownership in [Ownership::Kept, Ownership::Surrendered] {
            let plan = JailPlan {
                ownership,
                seq: Seq::new(0).unwrap(),
                net: None,
                cgroup: root.path().join(format!("{ownership:?}")),
                limits: JailLimits::default(),
            };
            assert!(linux::prepare_cgroup(&plan).is_err());
            assert_eq!(
                std::fs::read_to_string(plan.cgroup.join("memory.max")).unwrap(),
                plan.limits.memory_max.to_string(),
                "preparation must reach the fallible setup"
            );
            assert_eq!(
                plan.cgroup.join("cgroup.kill").exists(),
                ownership == Ownership::Kept,
                "only a kept launch may finish the half-prepared cgroup"
            );
        }
    }

    #[test]
    fn counts_round_trip_and_reject_corruption() {
        for n in [0, 1, 9, 10, 99, 999_999_999, SPAN - 1, SPAN, u32::MAX] {
            let count = Count(n);
            assert_eq!(Count::decode(&count.encode()), Some(count));
        }
        assert_eq!(Count::decode(b""), Some(Count(0)));
        for bytes in [
            b"abc".as_slice(),
            b"000000000",
            b"00000000000",
            b"4294967296",
            b"00000000x0",
        ] {
            assert_eq!(Count::decode(bytes), None, "{bytes:?}");
        }
    }

    #[test]
    fn a_torn_count_never_reissues_an_issued_number() {
        for old in [0, 9, 99, 999_999_999, SPAN - 1] {
            let new = old + 1;
            for k in 0..=10 {
                let mut torn = Count(old).encode();
                torn[..k].copy_from_slice(&Count(new).encode()[..k]);
                let count = Count::decode(&torn).unwrap();
                assert!(
                    count == Count(old) || count.0 >= new,
                    "{old} -> {new}, prefix {k}"
                );
            }
        }
        for k in 1..10 {
            assert_eq!(Count::decode(&Count(1).encode()[..k]), None);
        }
    }

    #[test]
    fn the_uid_line_ends_before_the_unchanged_sentinel() {
        let (seq, count) = Count(SPAN - 1).take().unwrap();
        assert_eq!(seq.uid(), u32::MAX - 1);
        assert_eq!(count, Count(SPAN));
        assert_eq!(count.take(), None);
        assert_eq!(Seq::new(SPAN), None);
        assert_eq!(Seq::new(u32::MAX), None);
        assert_eq!(Seq::new(0).unwrap().uid(), BASE_UID);
    }
}
