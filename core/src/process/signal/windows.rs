//! Windows signal handling and pipeline-group machinery — counterpart to the
//! `unix` sibling in this directory.
//!
//! With no pgid and no `kill(-pgid, …)`, a group here is a Job Object plus a
//! member-pid list in `win_groups::GROUPS`, keyed by the leader's pid — the
//! value `Pgid` carries.  Stages spawn `CREATE_NEW_PROCESS_GROUP`, addressable
//! one at a time by `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)` (such a
//! group suppresses Ctrl-C but always takes Ctrl-Break), and go into the job, so
//! one `TerminateJobObject` reaches descendants too.  A console group cannot be
//! joined after spawn: hence a fan-out over the pid list, not one call.
//!
//! Releasing a group falls to the caller: `RunningChild` owns a standalone
//! external's, while pipeline stages borrow one owned by `PipelineGroup`.
//!
//! [`install_handlers`] gives a bare `ral` the escalating disposition —
//! Ctrl-Break, `TerminateJobObject`, `ExitProcess(130)`.  A frontend with its own
//! cancel ladder (exarch) calls [`relay_interrupt`] instead, which never ticks
//! [`ESCALATION`] and never reaches a detached worker.

use std::sync::atomic::Ordering;

use super::{ESCALATION, Pgid, PgidPolicy};
use crate::process::cancel::{CancelCause, request_foreground_cancel};
use windows_sys::Win32::Foundation::HANDLE;

// ── Signal handler installation ────────────────────────────────────────────

pub fn install_handlers() {
    // `SetConsoleCtrlHandler` via the ctrlc crate, whose handler returns TRUE so
    // Windows leaves the process alive.  Under exarch this ladder lies dormant:
    // exarch registers later, Windows runs the newest handler first, and its
    // Ctrl-C arm claims the event before this one runs.
    let _ = ctrlc::set_handler(|| {
        request_foreground_cancel(CancelCause::Interrupt);
        let prev = ESCALATION.fetch_add(1, Ordering::Relaxed);
        match prev {
            0 => win_groups::break_all(),
            1 => win_groups::terminate_all(),
            // `ExitProcess`, not `std::process::exit`: the handler runs on a
            // worker thread, and running CRT atexit handlers there while the main
            // thread holds a lock can deadlock — the Unix handler reaches for
            // `_exit` on the same reasoning.
            _ => unsafe {
                windows_sys::Win32::System::Threading::ExitProcess(130);
            },
        }
    });
}

/// Cancel the foreground scope and fan `CTRL_BREAK_EVENT` out to every live,
/// non-detached group — the Windows analogue of Unix's `relay_handler`.
///
/// A frontend with its own exchange-cancel ladder (exarch) calls this in-process
/// rather than re-injecting a console event, which would re-enter the escalating
/// disposition [`install_handlers`] registers.
pub fn relay_interrupt() {
    request_foreground_cancel(CancelCause::Interrupt);
    win_groups::break_foreground();
}

// ── Inherited dispositions / child-signal reset ────────────────────────────

/// No-op: Windows children inherit the parent's console-control handlers, and
/// Rust's runtime installs none we would need to undo.
pub fn reset_child_signals() {}

// ── Pipeline-group state ───────────────────────────────────────────────────
//
// `GROUPS` can be a plain `Mutex` where Unix's `RELAY_PGIDS` needs a lock-free
// slot array: `SetConsoleCtrlHandler` callbacks run on a worker thread, not in
// async-signal context.

mod win_groups {
    use crate::process::ChildHandle;
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, FALSE, HANDLE, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    use windows_sys::Win32::System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
    };
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_ASSOCIATE_COMPLETION_PORT, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectAssociateCompletionPortInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::SystemServices::JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };

    use super::ReapStatus;

    /// Per-group state.  The completion port fires `ACTIVE_PROCESS_ZERO` when the
    /// *whole job* drains, which is what reap waits on; the duplicated handles
    /// outlive the spawner's `Child`, so an exit code stays readable after.
    pub(super) struct GroupState {
        pub job: HANDLE,
        pub completion_port: HANDLE,
        pub leader_handle: HANDLE,
        pub member_handles: Vec<HANDLE>,
        pub members: Vec<u32>,
        /// Latched once the job reported `ACTIVE_PROCESS_ZERO`, so a later reap
        /// answers from here instead of pumping a possibly closed port.
        pub all_done: bool,
        /// Set for a `PgidPolicy::NewSession` group — a detached background
        /// worker.  [`break_foreground`] skips these; the escalation ladder's
        /// [`break_all`] / [`terminate_all`] deliberately do not.
        pub detached: bool,
    }

    // SAFETY: `HANDLE` is a raw pointer, but never escapes the Mutex, and the
    // Win32 calls made on these handles are documented as thread-agnostic.
    unsafe impl Send for GroupState {}

    /// Live groups, keyed by leader pid.  A `Vec`: a handful at most are ever
    /// live at once, so the linear walk costs nothing.
    pub(super) static GROUPS: Mutex<Vec<(i32, GroupState)>> = Mutex::new(Vec::new());

    /// `size_of::<T>()` as the `u32` a job-information length argument wants —
    /// written once rather than as a bare `as` cast at each call.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "called only for Win32 information-class structs — fixed layouts of a few dozen bytes, so size_of is a compile-time constant nowhere near u32::MAX"
    )]
    const fn cb<T>() -> u32 {
        std::mem::size_of::<T>() as u32
    }

    /// Duplicate a process handle so it outlives `Child::wait` / `Child::drop`;
    /// [`release`] closes the copy.  Null on failure, which every consumer
    /// tolerates as "no handle".
    fn duplicate_process_handle(handle: HANDLE) -> HANDLE {
        let mut dup: HANDLE = std::ptr::null_mut();
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                handle,
                GetCurrentProcess(),
                &raw mut dup,
                0,
                FALSE,
                DUPLICATE_SAME_ACCESS,
            );
        }
        dup
    }

    fn install_kill_on_close(job: HANDLE) {
        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                cb::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
            );
        }
    }

    fn associate_completion_port(job: HANDLE) -> HANDLE {
        unsafe {
            let port = CreateIoCompletionPort(
                windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
                std::ptr::null_mut(),
                0,
                1,
            );
            if port.is_null() {
                return std::ptr::null_mut();
            }
            let mut assoc: JOBOBJECT_ASSOCIATE_COMPLETION_PORT = std::mem::zeroed();
            assoc.CompletionKey = job.cast();
            assoc.CompletionPort = port;
            let ok = SetInformationJobObject(
                job,
                JobObjectAssociateCompletionPortInformation,
                (&raw const assoc).cast(),
                cb::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>(),
            );
            if ok == 0 {
                CloseHandle(port);
                return std::ptr::null_mut();
            }
            port
        }
    }

    pub(crate) enum PreparedGroup {
        None,
        NewLeader {
            job: HANDLE,
            completion_port: HANDLE,
            /// Carried from the requesting `PgidPolicy` so [`register`] can tag
            /// the resulting `GroupState` — see the field there.
            detached: bool,
        },
        Join {
            leader: i32,
            job: HANDLE,
        },
    }

    // SAFETY: the raw handles are kernel objects, valid in whichever thread
    // completes the launch that prepared them.
    unsafe impl Send for PreparedGroup {}

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the Join arm holds the registry guard across `DuplicateHandle` on purpose — see the comment there"
    )]
    pub(super) fn prepare(policy: super::PgidPolicy) -> std::io::Result<PreparedGroup> {
        match policy {
            super::PgidPolicy::Inherit => Ok(PreparedGroup::None),
            super::PgidPolicy::NewLeader | super::PgidPolicy::NewSession => {
                let detached = matches!(policy, super::PgidPolicy::NewSession);
                let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null_mut()) };
                if job.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                install_kill_on_close(job);
                let completion_port = associate_completion_port(job);
                if completion_port.is_null() {
                    unsafe {
                        CloseHandle(job);
                    }
                    return Err(std::io::Error::last_os_error());
                }
                Ok(PreparedGroup::NewLeader {
                    job,
                    completion_port,
                    detached,
                })
            }
            super::PgidPolicy::Join(group) => {
                let leader = group.as_raw();
                // `state.job` is the registry's, not ours: the lock is all that
                // keeps `release` from closing it before the `DuplicateHandle`.
                let groups = GROUPS.lock().unwrap();
                let Some((_, state)) = groups.iter().find(|(p, _)| *p == leader) else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("pipeline group {leader} is no longer live"),
                    ));
                };
                if state.job.is_null() {
                    return Err(std::io::Error::other(format!(
                        "pipeline group {leader} has no Job Object"
                    )));
                }
                let job = duplicate_process_handle(state.job);
                if job.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(PreparedGroup::Join { leader, job })
            }
        }
    }

    pub(super) fn prepared_job(prepared: &PreparedGroup) -> Option<HANDLE> {
        match prepared {
            PreparedGroup::None => None,
            PreparedGroup::NewLeader { job, .. } | PreparedGroup::Join { job, .. } => Some(*job),
        }
    }

    /// Close a prepared group that never reached a child: a failed or abandoned
    /// launch.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "by-value is the whole contract — `PreparedGroup`'s fields are `Copy` raw handles, so only taking ownership says the handles are dead on return"
    )]
    pub(super) fn close_prepared(prepared: PreparedGroup) {
        unsafe {
            match prepared {
                PreparedGroup::None => {}
                PreparedGroup::NewLeader {
                    job,
                    completion_port,
                    ..
                } => {
                    if !completion_port.is_null() {
                        CloseHandle(completion_port);
                    }
                    if !job.is_null() {
                        CloseHandle(job);
                    }
                }
                PreparedGroup::Join { job, .. } => {
                    if !job.is_null() {
                        CloseHandle(job);
                    }
                }
            }
        }
    }

    /// Register a child already assigned to its Job Object: `launch`'s Windows
    /// arm spawns suspended, assigns, then resumes, so no user code has run yet.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "by-value is the whole contract — the prepared group's handles either move into `GROUPS` or are closed here, so the value must not outlive the call"
    )]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the Join arm holds the registry guard across `CloseHandle` on purpose — see the comment there"
    )]
    pub(super) fn register(prepared: PreparedGroup, child: &ChildHandle) -> Option<i32> {
        let child_pid = child.id();
        let child_handle = child.raw_process_handle();
        match prepared {
            PreparedGroup::None => None,
            PreparedGroup::NewLeader {
                job,
                completion_port,
                detached,
            } => {
                let leader_pid = child_pid.cast_signed();
                let leader_handle = duplicate_process_handle(child_handle);
                GROUPS.lock().unwrap().push((
                    leader_pid,
                    GroupState {
                        job,
                        completion_port,
                        leader_handle,
                        member_handles: Vec::new(),
                        members: vec![child_pid],
                        all_done: false,
                        detached,
                    },
                ));
                Some(leader_pid)
            }
            PreparedGroup::Join { leader, job } => {
                // Lock held across the `CloseHandle` below: this duplicate may be
                // the handle whose close trips `KILL_ON_JOB_CLOSE`, which must not
                // interleave with a concurrent `release` of the same job.
                let mut groups = GROUPS.lock().unwrap();
                if let Some((_, state)) = groups.iter_mut().find(|(p, _)| *p == leader) {
                    let dup = duplicate_process_handle(child_handle);
                    if !dup.is_null() {
                        state.member_handles.push(dup);
                    }
                    state.members.push(child_pid);
                }
                unsafe {
                    CloseHandle(job);
                }
                Some(leader)
            }
        }
    }

    pub(super) fn break_all() {
        let groups = GROUPS.lock().unwrap();
        for (_, state) in groups.iter() {
            for &pid in &state.members {
                unsafe {
                    GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
                }
            }
        }
    }

    /// [`break_all`] minus detached workers' groups, so an exchange-cancel
    /// ([`super::relay_interrupt`]) can never reach one.
    pub(super) fn break_foreground() {
        let groups = GROUPS.lock().unwrap();
        for (_, state) in groups.iter().filter(|(_, s)| !s.detached) {
            for &pid in &state.members {
                unsafe {
                    GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
                }
            }
        }
    }

    pub(super) fn terminate_all() {
        let groups = GROUPS.lock().unwrap();
        for (_, state) in groups.iter() {
            if !state.job.is_null() {
                unsafe {
                    TerminateJobObject(state.job, 1);
                }
            }
        }
    }

    /// Drop the group entry and close its handles; idempotent.  Closing the last
    /// handle to a `KILL_ON_JOB_CLOSE` job kills any surviving member, so this
    /// doubles as the abort-path teardown — no explicit `TerminateJobObject`.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard is deliberately held across the CloseHandle teardown — see the comment inside"
    )]
    pub(super) fn release(leader: i32) {
        // Removal and teardown are one step under the lock: no thread may see the
        // group gone while the job that still owns its members is alive.
        let mut groups = GROUPS.lock().unwrap();
        if let Some(idx) = groups.iter().position(|(p, _)| *p == leader) {
            let (_, state) = groups.swap_remove(idx);
            unsafe {
                if !state.completion_port.is_null() {
                    CloseHandle(state.completion_port);
                }
                if !state.job.is_null() {
                    CloseHandle(state.job);
                }
                if !state.leader_handle.is_null() {
                    CloseHandle(state.leader_handle);
                }
                for h in state.member_handles {
                    if !h.is_null() {
                        CloseHandle(h);
                    }
                }
            }
        }
    }

    /// Wait for `ACTIVE_PROCESS_ZERO` — every member gone, not merely the leader
    /// — or for the timeout, then report the leader's exit code, which Windows
    /// freezes on the handle after exit.
    pub(crate) fn wait_job(leader: i32, timeout_ms: u32) -> ReapStatus {
        let (port, leader_handle, all_done) = {
            let groups = GROUPS.lock().unwrap();
            match groups.iter().find(|(p, _)| *p == leader) {
                Some((_, state)) => (state.completion_port, state.leader_handle, state.all_done),
                None => return ReapStatus::Unknown,
            }
        };
        if !all_done {
            if port.is_null() {
                // No completion port — its creation failed at registration — so
                // the leader handle is all there is to wait on.
                return wait_leader_only(leader_handle, timeout_ms);
            }
            if !drain_completion_port(port, leader, timeout_ms) {
                return ReapStatus::Running;
            }
        }
        leader_exit_code(leader_handle)
    }

    /// Pump `port` until `ACTIVE_PROCESS_ZERO` or the deadline; `true` means the
    /// job is done, and latches `all_done` so later calls short-circuit.
    fn drain_completion_port(port: HANDLE, leader: i32, timeout_ms: u32) -> bool {
        fn latch_all_done(leader: i32) {
            let mut groups = GROUPS.lock().unwrap();
            if let Some((_, state)) = groups.iter_mut().find(|(p, _)| *p == leader) {
                state.all_done = true;
            }
        }

        let deadline = if timeout_ms == INFINITE {
            None
        } else {
            Some(std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.into()))
        };
        loop {
            let remaining = match deadline {
                Some(d) => match d.checked_duration_since(std::time::Instant::now()) {
                    Some(dur) => u32::try_from(dur.as_millis()).unwrap_or(u32::MAX),
                    None => 0,
                },
                None => INFINITE,
            };
            let mut bytes: u32 = 0;
            let mut key: usize = 0;
            let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
            let ok = unsafe {
                GetQueuedCompletionStatus(
                    port,
                    &raw mut bytes,
                    &raw mut key,
                    &raw mut overlapped,
                    remaining,
                )
            };
            if ok == 0 {
                // Timeout and error alike mean "not done yet"; a truly broken port
                // surfaces on the next call, which falls back to the leader.
                return false;
            }
            if bytes == JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO {
                latch_all_done(leader);
                return true;
            }
            // Every other JOB_OBJECT_MSG_* is uninteresting here; keep draining.
        }
    }

    fn leader_exit_code(leader_handle: HANDLE) -> ReapStatus {
        if leader_handle.is_null() {
            return ReapStatus::Unknown;
        }
        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(leader_handle, &raw mut code) };
        if ok == 0 {
            return ReapStatus::Unknown;
        }
        // Only reached once the process is gone, so 259 (`STILL_ACTIVE`) here is
        // a genuine exit code, not the still-running sentinel.
        ReapStatus::Exited(code.cast_signed())
    }

    /// Fallback for a group whose completion port could not be created: the
    /// leader can exit while later stages still run, so this is strictly weaker.
    fn wait_leader_only(handle: HANDLE, timeout_ms: u32) -> ReapStatus {
        if handle.is_null() {
            return ReapStatus::Unknown;
        }
        let r = unsafe { WaitForSingleObject(handle, timeout_ms) };
        if r == WAIT_TIMEOUT {
            return ReapStatus::Running;
        }
        if r != WAIT_OBJECT_0 {
            return ReapStatus::Unknown;
        }
        leader_exit_code(handle)
    }

    /// [`break_all`] narrowed to one leader, for a caller tearing down just that
    /// job.  No-op when the group is already gone.
    pub(super) fn break_group(leader: i32) {
        let groups = GROUPS.lock().unwrap();
        if let Some((_, state)) = groups.iter().find(|(p, _)| *p == leader) {
            for &pid in &state.members {
                unsafe {
                    GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
                }
            }
        }
    }

    /// `TerminateJobObject`: every member dies at once with exit code 1, there
    /// being no signal number to pass through.  No-op when the group is gone.
    pub(crate) fn kill_group(leader: i32) {
        let groups = GROUPS.lock().unwrap();
        if let Some((_, state)) = groups.iter().find(|(p, _)| *p == leader)
            && !state.job.is_null()
        {
            unsafe {
                TerminateJobObject(state.job, 1);
            }
        }
    }

    /// Apply an active-process limit to an existing group's job.  Windows refuses
    /// to put a child in a second job unless the jobs are nested, which this file
    /// never arranges, so a grant landing on a pipeline member must constrain the
    /// job the member already sits in.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard is deliberately held across the kernel call — see the comment inside"
    )]
    pub(crate) fn apply_active_process_limit(leader: i32, limit: u32) -> bool {
        // Held across `SetInformationJobObject`: the job belongs to the registry,
        // so dropping the lock first would let `release` close what we configure.
        let groups = GROUPS.lock().unwrap();
        let Some((_, state)) = groups.iter().find(|(p, _)| *p == leader) else {
            return false;
        };
        if state.job.is_null() {
            return false;
        }
        set_active_process_limit(state.job, limit)
    }

    /// The shared kernel call behind both the per-child sandbox job
    /// (`sandbox::windows::apply_job_limits`) and the pipeline-group limit.
    ///
    /// Read-modify-write, because one `LimitFlags` word carries every limit the
    /// job has: assigning it would strip `KILL_ON_JOB_CLOSE` off a group's job
    /// and leave [`release`] unable to kill the members it still owns.
    pub(crate) fn set_active_process_limit(job: HANDLE, limit: u32) -> bool {
        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            let mut returned: u32 = 0;
            if QueryInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw mut info).cast(),
                cb::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
                &raw mut returned,
            ) == 0
            {
                return false;
            }
            info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.BasicLimitInformation.ActiveProcessLimit = limit;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                cb::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
            ) != 0
        }
    }

    /// Forget the group and let the orphaned members run on.  Idempotent.
    pub(crate) fn disown(leader: i32) {
        {
            let groups = GROUPS.lock().unwrap();
            if let Some((_, state)) = groups.iter().find(|(p, _)| *p == leader)
                && !state.job.is_null()
            {
                unsafe {
                    let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                    // A zeroed struct is `LimitFlags = 0` — every limit cleared,
                    // `KILL_ON_JOB_CLOSE` among them.
                    SetInformationJobObject(
                        state.job,
                        JobObjectExtendedLimitInformation,
                        (&raw const info).cast(),
                        cb::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
                    );
                }
            }
        }
        release(leader);
    }

    pub(crate) fn wait_job_blocking(leader: i32) -> ReapStatus {
        wait_job(leader, INFINITE)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A grant's process cap lands on a job already armed by
        /// [`install_kill_on_close`], and [`release`] kills surviving members by
        /// closing the last handle to it — so the cap must leave that arm alone.
        #[test]
        fn process_cap_preserves_kill_on_close() {
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null_mut()) };
            assert!(!job.is_null());
            install_kill_on_close(job);
            assert!(set_active_process_limit(job, 7));

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            let mut returned: u32 = 0;
            let queried = unsafe {
                QueryInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&raw mut info).cast(),
                    cb::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>(),
                    &raw mut returned,
                )
            };
            unsafe {
                CloseHandle(job);
            }
            assert_ne!(queried, 0);

            let flags = info.BasicLimitInformation.LimitFlags;
            assert_ne!(flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, 0);
            assert_ne!(flags & JOB_OBJECT_LIMIT_ACTIVE_PROCESS, 0);
            assert_eq!(info.BasicLimitInformation.ActiveProcessLimit, 7);
        }
    }
}

/// Windows analogue of the Unix `PipelineRelay`, holding nothing.
///
/// `win_groups::GROUPS` already *is* the live-group set, so `install` exists
/// only to keep its caller in `runtime::pipeline::group` free of cfg gates.
pub struct PipelineRelay;

impl PipelineRelay {
    pub fn install(_pgid: i32) -> Option<Self> {
        Some(Self)
    }
}

/// Release the Job Object backing the group led by `leader`.
///
/// Surviving members die with it.  Idempotent, and needs to be —
/// `PipelineGroup::Drop`, `JobTable::cleanup` and a standalone `RunningChild`
/// all reach for it.
pub fn release_win_group(leader: i32) {
    win_groups::release(leader);
}

/// Cap the active-process count of the pipeline group's Job Object.
///
/// For `sandbox::apply_child_limits_in_pipeline`: a child inside a pipeline
/// cannot join a second job, so a grant's limit lands on the one it is
/// already in.
pub fn apply_group_active_process_limit(leader: i32, limit: u32) -> bool {
    win_groups::apply_active_process_limit(leader, limit)
}

/// Cap the active-process count of a Job Object the caller owns — the per-child
/// job `sandbox::windows::apply_job_limits` creates.
pub fn set_active_process_limit(job: windows_sys::Win32::Foundation::HANDLE, limit: u32) -> bool {
    win_groups::set_active_process_limit(job, limit)
}

/// True when `leader` names a live group.
/// `sandbox::apply_child_limits_in_pipeline` asks before choosing between a fresh
/// per-child job and constraining this one.
///
/// # Panics
///
/// On a poisoned registry mutex.  Nothing under that lock can panic, so a
/// poisoned one means this file's invariants are already gone.
pub fn is_known_group(leader: i32) -> bool {
    let groups = win_groups::GROUPS.lock().unwrap();
    groups.iter().any(|(p, _)| *p == leader)
}

pub(crate) type PreparedGroup = win_groups::PreparedGroup;

pub(crate) fn prepare_group(policy: PgidPolicy) -> std::io::Result<PreparedGroup> {
    win_groups::prepare(policy)
}

pub(crate) fn prepared_job(prepared: &PreparedGroup) -> Option<HANDLE> {
    win_groups::prepared_job(prepared)
}

pub(crate) fn close_prepared_group(prepared: PreparedGroup) {
    win_groups::close_prepared(prepared);
}

pub(crate) fn register_prepared_group(
    prepared: PreparedGroup,
    child: &crate::process::ChildHandle,
) -> Option<Pgid> {
    win_groups::register(prepared, child).and_then(Pgid::from_raw)
}

// ── Job-table primitives ───────────────────────────────────────────────────
//
// The Windows analogues of `kill(-pgid, …)` / `waitpid(-pgid, …)`, for the
// cross-platform `JobTable` over in the `ral` crate.  Each takes a leader pid
// (the `Pgid` value) and consults `win_groups::GROUPS`; an unknown leader means
// "already gone", matching the Unix path's tolerance of `ESRCH`.

/// Reap-poll outcome for a job-table leader.
pub enum ReapStatus {
    /// Never registered, or already released.
    Unknown,
    /// At least one member is still alive.
    Running,
    /// Every member has exited; the payload is the *leader's* exit code.
    Exited(i32),
}

/// Block until every member of the group has exited, then report the leader's
/// exit code.  `Unknown` once the group is no longer tracked.
pub fn wait_leader_blocking(pgid: Pgid) -> ReapStatus {
    win_groups::wait_job_blocking(pgid.as_raw())
}

/// Non-blocking poll of the whole-job completion [`wait_leader_blocking`] blocks
/// on; `JobTable::reap` drives it.
pub fn try_reap_leader(pgid: Pgid) -> ReapStatus {
    win_groups::wait_job(pgid.as_raw(), 0)
}

/// `SIGKILL` analogue: terminate every member of the group.  Idempotent.
pub fn kill_pipeline_group(pgid: Pgid) {
    win_groups::kill_group(pgid.as_raw());
}

/// `SIGTERM` analogue: `CTRL_BREAK_EVENT` to every member — the grace
/// `JobTable::cleanup` gives before escalating to [`kill_pipeline_group`].
pub fn break_pipeline_group(pgid: Pgid) {
    win_groups::break_group(pgid.as_raw());
}

/// `disown` analogue: release the group without terminating it.  Idempotent.
pub fn disown_pipeline_group(pgid: Pgid) {
    win_groups::disown(pgid.as_raw());
}

// ── Wait handling ──────────────────────────────────────────────────────────

/// Windows has no SIGTSTP, so the plain `Child::wait` is enough.  Reached only
/// through `ChildHandle::wait_handling_stop`.
#[allow(clippy::disallowed_methods)]
pub(super) fn wait_handling_stop(
    child: &mut std::process::Child,
    _pgid: Option<Pgid>,
    _park_on_stop: bool,
) -> std::io::Result<crate::process::WaitOutcome> {
    child
        .wait()
        .map(crate::process::WaitOutcome::from_exit_status)
}

/// Non-blocking peer of `wait_handling_stop`, keeping the Unix counterpart's
/// shape.  Reached only through `ChildHandle::try_wait_handling_stop`.
#[allow(clippy::disallowed_methods)]
pub(super) fn try_wait_handling_stop(
    child: &mut std::process::Child,
    _pgid: Option<Pgid>,
    _park_on_stop: bool,
) -> std::io::Result<Option<crate::process::WaitOutcome>> {
    child
        .try_wait()
        .map(|opt| opt.map(crate::process::WaitOutcome::from_exit_status))
}

/// The instant the kernel recorded `process`'s exit, or `None` while it still
/// runs — `GetProcessTimes` leaves the exit time undefined until then, so the
/// zero-timeout wait is what makes the answer meaningful.  Reached only through
/// `ChildHandle::exited_at`.
pub(super) fn exit_time(process: HANDLE) -> Option<std::time::SystemTime> {
    use std::time::{Duration, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::{FILETIME, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{GetProcessTimes, WaitForSingleObject};

    if process.is_null() || unsafe { WaitForSingleObject(process, 0) } != WAIT_OBJECT_0 {
        return None;
    }
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
    let ok = unsafe {
        GetProcessTimes(
            process,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    // FILETIME counts 100 ns ticks from 1601-01-01, 11_644_473_600 s before
    // the Unix epoch.
    const EPOCH_TICKS: u64 = 11_644_473_600 * 10_000_000;
    let ticks = u64::from(exit.dwHighDateTime) << 32 | u64::from(exit.dwLowDateTime);
    let nanos = ticks.checked_sub(EPOCH_TICKS)?.checked_mul(100)?;
    Some(UNIX_EPOCH + Duration::from_nanos(nanos))
}

// ── Foreground ownership ───────────────────────────────────────────────────

/// Windows shares one console across every attached process, so there is nothing
/// to acquire: console programs (fzf, less, vim) drive the Console API directly
/// and need no handoff from ral.
pub struct ForegroundGuard;

impl ForegroundGuard {
    pub fn try_acquire(_target: i32, _lease: &crate::process::TerminalLease) -> Option<Self> {
        None
    }
}
