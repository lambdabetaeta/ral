//! Windows signal handling and pipeline-group machinery.
//!
//! Windows has neither POSIX pgids nor `kill(-pgid, …)`.  The pieces we
//! build out of Win32 primitives:
//!
//!   * Each external pipeline stage is spawned with
//!     `CREATE_NEW_PROCESS_GROUP` so it's individually addressable via
//!     `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)` (Ctrl-C is
//!     deliberately suppressed in groups created this way; Ctrl-Break is
//!     always delivered).
//!   * A single Job Object per pipeline.  Every stage is
//!     `AssignProcessToJobObject`'d into it, so a single
//!     `TerminateJobObject` on the abort path takes the whole tree —
//!     including descendants spawned by stages.
//!   * The list of member pids per pipeline is kept in `GROUPS` keyed by
//!     the leader's pid (the same value `Pgid` carries).  The Ctrl-C
//!     handler walks that list directly to fan Ctrl-Break out to every
//!     stage individually, since console process groups can't be joined
//!     post-spawn.  No separate "active relay" list — the registry IS the
//!     live-pipelines set.

use std::sync::atomic::Ordering;

use super::{Pgid, PgidPolicy, SIGNAL_COUNT};

// ── Signal handler installation ────────────────────────────────────────────

pub fn install_handlers() {
    // SetConsoleCtrlHandler via the ctrlc crate.  The handler increments
    // the same SIGNAL_COUNT flag the evaluator polls between statements,
    // returns TRUE so Windows does not terminate the process, and fans
    // Ctrl-Break out to every active pipeline group (the Windows analogue
    // of the Unix relay handler).
    let _ = ctrlc::set_handler(|| {
        let prev = SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
        if prev >= 2 {
            std::process::exit(130);
        }
        win_groups::signal_all();
    });
}

// ── Inherited dispositions / child-signal reset ────────────────────────────

/// Windows children inherit the parent's console-control handlers, and
/// Rust's runtime does not install console handlers we need to undo.  No-op.
pub fn reset_child_signals() {}

// ── Pipeline-group state ───────────────────────────────────────────────────
//
// `GROUPS` uses a `Mutex` rather than the lock-free atomic-array trick used
// for `RELAY_PGIDS` on Unix.  Windows `SetConsoleCtrlHandler` callbacks run
// on a worker thread, not in async-signal context, so a mutex is safe and
// the hot paths (pipeline start / Ctrl-C arrival) are infrequent enough
// that lock contention is irrelevant.

mod win_groups {
    use std::os::windows::io::AsRawHandle;
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    /// Per-pipeline state: the Job Object and the member pids (each its
    /// own console-group leader).
    pub(super) struct GroupState {
        pub job: HANDLE,
        pub members: Vec<u32>,
    }

    // SAFETY: `HANDLE` is a raw pointer; we never share it outside the
    // Mutex.  CloseHandle / TerminateJobObject / AssignProcessToJobObject
    // are documented to be safe to call from any thread.
    unsafe impl Send for GroupState {}

    /// Live pipeline groups, keyed by leader pid.  A `Vec` rather than a
    /// map: at most a handful of pipelines are ever simultaneously live
    /// and a linear walk is fine.
    pub(super) static GROUPS: Mutex<Vec<(i32, GroupState)>> = Mutex::new(Vec::new());

    /// Create a fresh Job Object, assign `leader`, and register the group
    /// keyed by the leader's pid.  Returns the leader pid as i32 (== Pgid
    /// value).
    pub(super) fn new_leader(leader: &std::process::Child) -> i32 {
        let leader_pid = leader.id() as i32;
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null_mut()) };
        if !job.is_null() {
            // KILL_ON_JOB_CLOSE means: when the last handle to the job is
            // closed (we hold exactly one), every still-alive member is
            // terminated.  Wires the abort path to plain RAII —
            // `release(leader)` does the kill-and-free.
            unsafe {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &raw const info as *const _,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                AssignProcessToJobObject(job, leader.as_raw_handle() as HANDLE);
            }
        }
        let mut groups = GROUPS.lock().unwrap();
        groups.push((
            leader_pid,
            GroupState {
                job,
                members: vec![leader.id()],
            },
        ));
        leader_pid
    }

    /// Add `member` to the group identified by `leader`, if any.  No-op
    /// when the group has already been released (race against drop).
    pub(super) fn join(leader: i32, member: &std::process::Child) {
        let mut groups = GROUPS.lock().unwrap();
        if let Some((_, state)) = groups.iter_mut().find(|(p, _)| *p == leader) {
            if !state.job.is_null() {
                unsafe {
                    AssignProcessToJobObject(state.job, member.as_raw_handle() as HANDLE);
                }
            }
            state.members.push(member.id());
        }
    }

    /// Send `CTRL_BREAK_EVENT` to every member of every live pipeline
    /// group.  Called from the Ctrl-C handler.  The fan-out is per-pid
    /// because Windows console process groups can't be joined: each stage
    /// was spawned with its own `CREATE_NEW_PROCESS_GROUP`.
    pub(super) fn signal_all() {
        let groups = GROUPS.lock().unwrap();
        for (_, state) in groups.iter() {
            for &pid in &state.members {
                unsafe {
                    GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
                }
            }
        }
    }

    /// Remove the group entry and close the Job handle.  Called from
    /// `PipelineGroup::Drop`; idempotent.  Closing the only handle to a
    /// job created with `KILL_ON_JOB_CLOSE` terminates any still-alive
    /// members — that's the abort-path teardown, no explicit
    /// `TerminateJobObject` needed.
    pub(super) fn release(leader: i32) {
        let mut groups = GROUPS.lock().unwrap();
        if let Some(idx) = groups.iter().position(|(p, _)| *p == leader) {
            let (_, state) = groups.swap_remove(idx);
            if !state.job.is_null() {
                unsafe {
                    CloseHandle(state.job);
                }
            }
        }
    }
}

/// Windows analogue of the Unix `PipelineRelay`: a no-op marker.  The
/// live-pipelines set is `win_groups::GROUPS` itself, populated by
/// [`spawn_with_pgid`] and cleaned up by [`release_win_group`] from
/// `PipelineGroup::Drop`.  `install` exists only to keep the cross-platform
/// call site in `pipeline.rs` free of cfg gates.
pub struct PipelineRelay;

impl PipelineRelay {
    pub fn install(_pgid: i32) -> Option<Self> {
        Some(Self)
    }
}

/// Release the Job Object backing the pipeline group identified by
/// `leader`.  Closing the only handle terminates any still-alive members
/// (the job was created with `KILL_ON_JOB_CLOSE`).  Idempotent: safe to
/// call from `PipelineGroup::Drop` even when `PipelineRelay::Drop` already
/// released.
pub fn release_win_group(leader: i32) {
    win_groups::release(leader);
}

// ── Process-group placement ────────────────────────────────────────────────

/// Windows arm: there's no `pre_exec`, no pgid, no `setpgid`.  Three
/// gestures replace them:
///
///   * `CREATE_NEW_PROCESS_GROUP` so the child can be addressed individually
///     by `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)`.
///   * Job Object membership so the whole pipeline can be torn down with one
///     `TerminateJobObject` on the abort path.  `NewLeader` creates a fresh
///     job and assigns this stage; `Join` looks up the leader's job and
///     assigns there.
///   * No signal-disposition reset.
///
/// The returned `Pgid` is the leader pid (the *first* stage's pid),
/// matching the value `pipeline/group.rs` already keys off.
pub fn spawn_with_pgid(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
) -> std::io::Result<(std::process::Child, Option<Pgid>)> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
    match pgid {
        PgidPolicy::Inherit => {
            let child = cmd.spawn()?;
            Ok((child, None))
        }
        PgidPolicy::NewLeader => {
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
            let child = cmd.spawn()?;
            let leader = win_groups::new_leader(&child);
            Ok((child, Some(Pgid(leader))))
        }
        PgidPolicy::Join(p) => {
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
            let child = cmd.spawn()?;
            win_groups::join(p.0, &child);
            Ok((child, Some(p)))
        }
    }
}

/// Windows has no SIGTSTP analogue; the standard `Child::wait` is enough.
pub fn wait_handling_stop(
    child: &mut std::process::Child,
    _pgid: Option<Pgid>,
) -> std::io::Result<std::process::ExitStatus> {
    child.wait()
}

// ── Foreground ownership ───────────────────────────────────────────────────

/// Windows shares one console across attached processes, so there is
/// nothing to acquire and nothing to release.  Console programs (fzf,
/// less, vim) talk to the Console API directly and work without any
/// handoff from ral.  The internal-stage SIGTTIN deadlock that motivates
/// the Mixed-pipeline gate on Unix simply does not exist here.
pub struct ForegroundGuard;

impl ForegroundGuard {
    pub fn try_acquire(_target: i32, _shell: &crate::types::Shell) -> Option<Self> {
        None
    }
}
