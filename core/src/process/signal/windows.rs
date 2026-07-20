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
//!     including descendants spawned by stages.  The job is also
//!     associated with an IO completion port so the parent can wait for
//!     `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` — the signal that *every*
//!     member has exited, not just the leader process.
//!   * The list of member pids per pipeline is kept in `GROUPS` keyed by
//!     the leader's pid (the same value `Pgid` carries).  The Ctrl-C
//!     handler walks that list directly to fan Ctrl-Break out to every
//!     stage individually, since console process groups can't be joined
//!     post-spawn.  No separate "active relay" list — the registry IS the
//!     live-pipelines set.
//!
//! ## Group ownership
//!
//! `register_prepared_group` (called from the Windows launch boundary in
//! `process::launch` after a `NewLeader` child is placed in its job)
//! registers a Windows group for both standalone foreground externals and
//! pipeline first stages.  The two cases differ only in who releases the
//! group:
//!
//!   * **Standalone**: the [`crate::runtime::command::RunningChild`]
//!     owns its own group and releases it on drop / wait completion.
//!     Aborting a standalone calls `TerminateJobObject` on that group
//!     rather than `child.kill()`, so descendants spawned by the
//!     external (e.g. `cmd /c ...`) die too.
//!   * **Pipeline**: stages borrow the group from
//!     [`crate::runtime::pipeline::group::PipelineGroup`], which owns
//!     the lifetime through `Drop`.  Per-stage `RunningChild`s do not
//!     release the group on their own.
//!
//! ## Ctrl-C escalation
//!
//! [`install_handlers`] installs the base disposition a plain `ral`
//! process gets with no interactive frontend layered on top: 1st Ctrl-C
//! (or Ctrl-Break): increment the signal counter and fan `CTRL_BREAK_EVENT`
//! out to every active member of every live group.  2nd: `TerminateJobObject`
//! on every live group (hard kill that reaches descendants too, detached
//! workers included -- a deliberate termination request gets no exemption).
//! 3rd: `ExitProcess(130)`.
//!
//! [`relay_interrupt`] is the non-escalating alternative a frontend with its
//! own turn-cancel ladder (exarch) calls directly instead: it cancels the
//! foreground scope and fans `CTRL_BREAK_EVENT` out to every live
//! *non-detached* group, never touching the counter above -- the Windows
//! analogue of Unix's non-escalating `relay_handler`.  A detached
//! background worker's group is never in that fan-out, matching Unix's
//! `RELAY_PGIDS` (a detached worker's pgid is never registered there
//! either).

use std::sync::atomic::Ordering;

use super::{ESCALATION, Pgid, PgidPolicy};
use crate::process::cancel::{CancelCause, request_foreground_cancel};
use windows_sys::Win32::Foundation::HANDLE;

// ── Signal handler installation ────────────────────────────────────────────

pub fn install_handlers() {
    // SetConsoleCtrlHandler via the ctrlc crate.  The handler translates
    // the delivery into an [`Interrupt`](super::CancelCause::Interrupt) on
    // the published foreground cancel slot — the delivery every poll point
    // (the evaluator's `check`, a blocked external's wait loop) observes —
    // returns TRUE so Windows does not terminate the process, and fans
    // Ctrl-Break / TerminateJobObject out depending on how many deliveries
    // the escalation ladder has seen.  The boundary `clear` resets the
    // ladder per active interrupt sequence, not for the lifetime of the
    // shell.
    let _ = ctrlc::set_handler(|| {
        request_foreground_cancel(CancelCause::Interrupt);
        let prev = ESCALATION.fetch_add(1, Ordering::Relaxed);
        match prev {
            // First delivery: cooperative cancel via Ctrl-Break.
            0 => win_groups::break_all(),
            // Second delivery: stop hoping the children cooperate.
            1 => win_groups::terminate_all(),
            // Third delivery: drop everything and exit.  Use `ExitProcess`
            // — the Win32 analogue of the Unix `_exit` — rather than
            // `std::process::exit`: the handler runs on a worker thread,
            // and running CRT atexit handlers / destructors from there
            // while the main thread holds a lock can deadlock. The same
            // rationale the Unix handler cites for `_exit`.
            _ => unsafe {
                windows_sys::Win32::System::Threading::ExitProcess(130);
            },
        }
    });
}

/// Non-escalating turn-cancel relay: cancels the current turn's foreground
/// scope and fans `CTRL_BREAK_EVENT` out to every live, non-detached
/// pipeline group.  The Windows analogue of Unix's non-escalating
/// `relay_handler` (`sigint_relay`) -- unlike the ladder [`install_handlers`]
/// installs, this never ticks [`ESCALATION`] and never reaches a detached
/// worker's group.  A frontend with its own turn-cancel contract (exarch)
/// calls this directly, in-process, for a Ctrl-C/Ctrl-Break it has already
/// decided to treat as a turn-cancel, instead of re-injecting a console
/// event that would re-enter `install_handlers`'s escalating disposition.
pub fn relay_interrupt() {
    request_foreground_cancel(CancelCause::Interrupt);
    win_groups::break_foreground();
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
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
        JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectAssociateCompletionPortInformation, JobObjectBasicLimitInformation,
        JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::SystemServices::JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };

    use super::ReapStatus;

    /// Per-pipeline state: the Job Object, an IO completion port the job
    /// is associated with, the duplicated leader handle (kept around so
    /// we can read an exit code after the job's last process is gone),
    /// the duplicated member handles (so member kills survive `Child`
    /// drop), and the member pids.  The completion port fires
    /// `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` when the *whole job*
    /// completes — not just the leader process — so reap reflects what
    /// the user expects from `yes | head` (the slowest stage's exit,
    /// not the first stage's premature SIGPIPE-style termination).
    pub(super) struct GroupState {
        pub job: HANDLE,
        pub completion_port: HANDLE,
        pub leader_handle: HANDLE,
        /// Duplicated handles for every member except the leader, so
        /// the registry can `WaitForSingleObject` / `GetExitCodeProcess`
        /// independently of the spawner's `Child` lifetime.  Closed in
        /// `release`.
        pub member_handles: Vec<HANDLE>,
        pub members: Vec<u32>,
        /// Latched once the job has reported `ACTIVE_PROCESS_ZERO` so
        /// the next reap call returns immediately rather than blocking
        /// on the (potentially closed) completion port.
        pub all_done: bool,
        /// True for a detached background worker's group (spawned under
        /// `PgidPolicy::NewSession`), false for a foreground job or
        /// pipeline leader (`PgidPolicy::NewLeader`).  [`break_foreground`]
        /// (the turn-cancel relay) filters on this so a trunk interrupt
        /// never reaches a detached worker; [`break_all`]/[`terminate_all`]
        /// (the escalation ladder for a genuine termination request) do
        /// not filter, and reach every group regardless.
        pub detached: bool,
    }

    // SAFETY: `HANDLE` is a raw pointer; we never share it outside the
    // Mutex.  CloseHandle / TerminateJobObject / AssignProcessToJobObject
    // are documented to be safe to call from any thread.
    unsafe impl Send for GroupState {}

    /// Live pipeline groups, keyed by leader pid.  A `Vec` rather than a
    /// map: at most a handful of pipelines are ever simultaneously live
    /// and a linear walk is fine.
    pub(super) static GROUPS: Mutex<Vec<(i32, GroupState)>> = Mutex::new(Vec::new());

    /// Duplicate `child`'s OS handle into one that survives `Child::wait`
    /// and `Child::drop`.  The duplicate is closed by [`release`].  Null
    /// on failure — every consumer treats null as "no handle" and
    /// degrades to a missing-handle read instead of crashing.
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
                &raw const info as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
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
            assoc.CompletionKey = job as *mut _;
            assoc.CompletionPort = port;
            let ok = SetInformationJobObject(
                job,
                JobObjectAssociateCompletionPortInformation,
                &raw const assoc as *const _,
                std::mem::size_of::<JOBOBJECT_ASSOCIATE_COMPLETION_PORT>() as u32,
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
            /// Carried through from the `PgidPolicy` that requested this
            /// group, so [`register`] can tag the resulting `GroupState`
            /// -- see its doc for why the distinction matters.
            detached: bool,
        },
        Join {
            leader: i32,
            job: HANDLE,
        },
    }

    // SAFETY: `PreparedGroup` is held across launch in one thread and owns or
    // borrows raw Win32 handles; the handles themselves are kernel objects.
    unsafe impl Send for PreparedGroup {}

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
            super::PgidPolicy::Join(super::Pgid(leader)) => {
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

    /// Register a child that has already been placed in its Job Object before
    /// user code was allowed to run. Returns the leader pid as i32.
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
                let leader_pid = child_pid as i32;
                let leader_handle = duplicate_process_handle(child_handle);
                let mut groups = GROUPS.lock().unwrap();
                groups.push((
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

    /// Send `CTRL_BREAK_EVENT` to every member of every live pipeline
    /// group.  Called on the first Ctrl-C arrival — gives the children a
    /// chance to clean up.
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

    /// Send `CTRL_BREAK_EVENT` to every member of every live,
    /// non-detached pipeline group — [`break_all`] narrowed to skip a
    /// detached background worker's group (`GroupState::detached`).  The
    /// turn-cancel relay ([`super::relay_interrupt`]) uses this instead of
    /// `break_all` so a trunk interrupt can never reach a detached worker.
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

    /// `TerminateJobObject` every live pipeline group.  Called on the
    /// second Ctrl-C arrival — children that ignore Ctrl-Break die now.
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

    /// Remove the group entry and close the Job handle plus all
    /// duplicated process handles.  Called from `PipelineGroup::Drop`
    /// (or the standalone group's drop); idempotent.  Closing the only
    /// handle to a job created with `KILL_ON_JOB_CLOSE` terminates any
    /// still-alive members — that's the abort-path teardown, no
    /// explicit `TerminateJobObject` needed.
    pub(super) fn release(leader: i32) {
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

    /// Wait on the *whole job* via its completion port.  Returns once
    /// `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` arrives (every member has
    /// exited) or the timeout elapses.  Reads the leader's exit code
    /// after that point because Windows freezes the leader's exit code
    /// on its handle even after the process is gone, so it is the
    /// canonical "exit status of the pipeline" — matching shells'
    /// `${PIPESTATUS[-1]}`-equivalent for the first stage.
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
                // Fallback to the leader handle when the completion port
                // is unavailable (rare; only if completion-port creation
                // failed at registration).
                return wait_leader_only(leader_handle, timeout_ms);
            }
            if !drain_completion_port(port, leader, timeout_ms) {
                return ReapStatus::Running;
            }
        }
        leader_exit_code(leader_handle)
    }

    /// Pump the completion port until either we see
    /// `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO` or the deadline expires.
    /// Returns `true` when the job is fully done.  Latches the
    /// `all_done` flag in `GROUPS` so subsequent calls short-circuit.
    fn drain_completion_port(port: HANDLE, leader: i32, timeout_ms: u32) -> bool {
        let deadline = if timeout_ms == INFINITE {
            None
        } else {
            Some(std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.into()))
        };
        loop {
            let remaining = match deadline {
                Some(d) => match d.checked_duration_since(std::time::Instant::now()) {
                    Some(dur) => dur.as_millis().min(u32::MAX.into()) as u32,
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
                // Timeout or error.  Treat both as "not done yet"; a
                // truly broken port will be observed on the next call
                // and the caller can fall back to leader-handle wait.
                return false;
            }
            if bytes == JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO {
                let mut groups = GROUPS.lock().unwrap();
                if let Some((_, state)) = groups.iter_mut().find(|(p, _)| *p == leader) {
                    state.all_done = true;
                }
                return true;
            }
            // Other JOB_OBJECT_MSG_* messages (process new / process
            // exit / abnormal / memory-limit / etc) are not interesting
            // here; keep draining until the terminal message arrives or
            // the deadline expires.
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
        // The wait_job contract is to call only after ACTIVE_PROCESS_ZERO,
        // so a 259 / 0x103 here is a genuine user exit code, not the
        // STILL_ACTIVE still-running sentinel, and passes through as-is.
        ReapStatus::Exited(code as i32)
    }

    /// Waits on the leader handle alone.  Strictly less correct than the
    /// completion-port wait, since the leader may exit while later stages
    /// are still live; used only when completion-port creation failed at
    /// registration.
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

    /// `CTRL_BREAK_EVENT` to every member of one group — the
    /// [`break_all`] fan-out narrowed to a single leader, for a caller
    /// (`JobTable::cleanup`) that wants the cooperative signal scoped to
    /// the job it is tearing down rather than every live pipeline.
    /// Idempotent: no-op when the group is already gone.
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

    /// `TerminateJobObject` the group: every member dies immediately
    /// with the given exit code (1 — there's no signal number to pass
    /// through, and it lines up with what `kill -9` would surface as).
    /// Idempotent: no-op when the group is already gone.
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

    /// Apply an active-process limit to the group's Job Object.  Used by
    /// the grant pathway when a child is already a member of a pipeline
    /// group: assigning the child to a *second* job is forbidden by
    /// Windows unless one is a nested job, and we don't arrange that, so
    /// the right move is to set the limit on the existing job.
    ///
    /// Returns `true` when the limit was applied; `false` when the group
    /// is unknown or the underlying call failed (caller may then surface
    /// a diagnostic).
    pub(crate) fn apply_active_process_limit(leader: i32, limit: u32) -> bool {
        let groups = GROUPS.lock().unwrap();
        let Some((_, state)) = groups.iter().find(|(p, _)| *p == leader) else {
            return false;
        };
        if state.job.is_null() {
            return false;
        }
        set_active_process_limit(state.job, limit)
    }

    /// Set an active-process limit on a Job Object handle.  The shared
    /// kernel-call body behind both the per-child sandbox job and the
    /// pipeline-group limit; returns `true` when `SetInformationJobObject`
    /// succeeded.
    pub(crate) fn set_active_process_limit(job: HANDLE, limit: u32) -> bool {
        unsafe {
            let mut info: JOBOBJECT_BASIC_LIMIT_INFORMATION = std::mem::zeroed();
            info.LimitFlags =
                windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            info.ActiveProcessLimit = limit;
            SetInformationJobObject(
                job,
                JobObjectBasicLimitInformation,
                &raw const info as *const _,
                std::mem::size_of::<JOBOBJECT_BASIC_LIMIT_INFORMATION>() as u32,
            ) != 0
        }
    }

    /// Clear `KILL_ON_JOB_CLOSE` and release the group.  After this
    /// returns, the (now-orphaned) members keep running — `disown`
    /// semantics: drop the bookkeeping without killing.  Idempotent.
    pub(crate) fn disown(leader: i32) {
        {
            let groups = GROUPS.lock().unwrap();
            if let Some((_, state)) = groups.iter().find(|(p, _)| *p == leader)
                && !state.job.is_null()
            {
                unsafe {
                    let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                    // LimitFlags = 0 clears KILL_ON_JOB_CLOSE — no other
                    // limits are set on this job, so a zeroed struct is
                    // the right "no limits" payload.
                    SetInformationJobObject(
                        state.job,
                        JobObjectExtendedLimitInformation,
                        &raw const info as *const _,
                        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    );
                }
            }
        }
        release(leader);
    }

    /// Convenience wrapper for unbounded waits.
    pub(crate) fn wait_job_blocking(leader: i32) -> ReapStatus {
        wait_job(leader, INFINITE)
    }
}

/// Windows analogue of the Unix `PipelineRelay`: a no-op marker.  The
/// live-pipelines set is `win_groups::GROUPS` itself, populated by
/// `register_prepared_group` and cleaned up by [`release_win_group`] from
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

/// Apply `limit` to the active-process count of the pipeline group's
/// Job Object.  Used by `sandbox::apply_child_limits` when a grant fires
/// inside a pipeline: the child cannot be added to a second job, so the
/// limit lands on the existing pipeline job instead.  Returns `true`
/// when applied.
pub fn apply_group_active_process_limit(leader: i32, limit: u32) -> bool {
    win_groups::apply_active_process_limit(leader, limit)
}

/// Set an active-process limit on a Job Object handle the caller owns.
/// Used by `sandbox::apply_child_limits` for the per-child job it
/// creates.  Returns `true` when applied.
pub fn set_active_process_limit(job: windows_sys::Win32::Foundation::HANDLE, limit: u32) -> bool {
    win_groups::set_active_process_limit(job, limit)
}

/// True when the leader is a known live pipeline group.  Used to decide
/// between assigning a fresh per-child job (no group) and applying the
/// limit to an existing group.
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
    win_groups::register(prepared, child).map(Pgid)
}

// ── Job-table primitives ───────────────────────────────────────────────────
//
// These are the Windows analogues of `kill(-pgid, …)` / `waitpid(-pgid, …)`
// used by the cross-platform [`crate::JobTable`] in `ral/src/jobs.rs`.
// Each function operates by leader pid (== `Pgid` value) and consults the
// `win_groups::GROUPS` registry; an unknown leader is treated as "already
// gone", matching the Unix path's tolerance of `ESRCH`.

/// Reap-poll outcome for a job-table leader.
pub enum ReapStatus {
    /// The group is unknown — never registered or already released.
    Unknown,
    /// At least one member is still alive.
    Running,
    /// Every member has exited; the leader's exit code is reported.
    Exited(i32),
}

/// Block until the *whole* pipeline-group job completes
/// (`JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO`), returning the leader's exit
/// code.  Used by `JobTable::wait_foreground` and `wait_foreground` on
/// Windows.  Returns `Unknown` when the group is no longer tracked.
pub fn wait_leader_blocking(pgid: Pgid) -> ReapStatus {
    win_groups::wait_job_blocking(pgid.0)
}

/// Non-blocking poll on the pipeline-group's whole-job completion.
/// Used by `JobTable::reap`.
pub fn try_reap_leader(pgid: Pgid) -> ReapStatus {
    win_groups::wait_job(pgid.0, 0)
}

/// `SIGKILL` analogue: terminate every member of the group.  Used by
/// `JobTable::cleanup` on shell exit.  Idempotent.
pub fn kill_pipeline_group(pgid: Pgid) {
    win_groups::kill_group(pgid.0);
}

/// `SIGTERM` analogue: `CTRL_BREAK_EVENT` to every member of the group,
/// giving well-behaved children the chance to wind down on their own
/// before `JobTable::cleanup` escalates to [`kill_pipeline_group`] — the
/// same grace Unix's `SIGTERM`-then-`SIGKILL` pair gives.  Idempotent.
pub fn break_pipeline_group(pgid: Pgid) {
    win_groups::break_group(pgid.0);
}

/// `disown` analogue: clear `KILL_ON_JOB_CLOSE` and release the group
/// without terminating members.  After this returns, the children keep
/// running; ral has forgotten them.  Idempotent.
pub fn disown_pipeline_group(pgid: Pgid) {
    win_groups::disown(pgid.0);
}

// ── Wait handling ──────────────────────────────────────────────────────────

/// Windows has no SIGTSTP analogue; the standard `Child::wait` is enough.
/// Called only via `ChildHandle::wait_handling_stop`, which is the single
/// platform-agnostic entry point.
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

/// Non-blocking peek matching the Unix counterpart's shape.  No SIGSTOP
/// analogue on Windows, so this is a thin wrapper over `try_wait`.
/// Reached only via `ChildHandle::try_wait_handling_stop`.
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

// ── Foreground ownership ───────────────────────────────────────────────────

/// Windows shares one console across attached processes, so there is
/// nothing to acquire and nothing to release.  Console programs (fzf,
/// less, vim) talk to the Console API directly and work without any
/// handoff from ral.  The internal-stage SIGTTIN deadlock that motivates
/// the Mixed-pipeline gate on Unix simply does not exist here.
pub struct ForegroundGuard;

impl ForegroundGuard {
    pub fn try_acquire(_target: i32, _lease: &crate::process::TerminalLease) -> Option<Self> {
        None
    }
}
