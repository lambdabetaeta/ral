//! Signal handling: SIGINT, SIGTERM, SIGHUP set a flag that the
//! evaluator checks between statements, triggering unwinding.
//!
//! First signal: begin unwinding (guard cleanup runs).
//! Second signal during unwind: deferred until cleanup completes.
//! Third signal: immediate process exit.
//!
//! For mixed internal/external pipelines, PipelineRelay claims a slot in
//! RELAY_PGIDS so that sigint_relay forwards Ctrl+C to that process group.
//! The relay handler is installed permanently by the interactive shell
//! (replacing SIG_IGN); when no slots are active it does nothing.

#[cfg(unix)]
use std::sync::atomic::{AtomicI32, AtomicU64};
use std::sync::atomic::{AtomicU8, Ordering};

/// 0 = normal, 1 = interrupted, 2 = second signal, >=3 = force exit.
static SIGNAL_COUNT: AtomicU8 = AtomicU8::new(0);

/// Check whether the current evaluation should unwind.
///
/// Two reasons can fire:
///   * a process-level signal (SIGINT / SIGTERM / SIGHUP) — increments
///     the global `SIGNAL_COUNT`;
///   * a structured-concurrency cancel — the shell's `CancelScope` (or
///     any of its ancestors) has been cancelled, e.g. by a
///     `RunningPipeline::Drop` on the abort path.
///
/// Both unwind via the same `EvalSignal::Error` so callers don't need to
/// distinguish them — the message is "interrupted" vs "cancelled".
pub fn check(shell: &crate::types::Shell) -> Result<(), crate::types::EvalSignal> {
    if SIGNAL_COUNT.load(Ordering::Relaxed) >= 1 {
        return Err(crate::types::EvalSignal::Error(
            crate::types::Error::new("interrupted", 130)
                .at(shell.location.line, shell.location.col),
        ));
    }
    if shell.cancel.is_cancelled() {
        return Err(crate::types::EvalSignal::Error(
            crate::types::Error::new("cancelled", 130)
                .at(shell.location.line, shell.location.col),
        ));
    }
    Ok(())
}

/// Clear the signal flag (e.g., after handling in interactive mode).
pub fn clear() {
    SIGNAL_COUNT.store(0, Ordering::Relaxed);
}

/// Returns true if a signal is pending.
pub fn is_interrupted() -> bool {
    SIGNAL_COUNT.load(Ordering::Relaxed) >= 1
}

/// Install signal handlers for SIGINT, SIGTERM, SIGHUP.
/// Must be called once at program startup.  Snapshots inherited SIG_IGN
/// dispositions *before* installing ral's own handlers so the nohup rule
/// (preserve dispositions the parent deliberately ignored) can be honored
/// in spawned children — see `reset_child_signals`.
#[cfg(unix)]
pub fn install_handlers() {
    snapshot_inherited_ignored();
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, handler as *const () as libc::sighandler_t);
    }
}

#[cfg(unix)]
extern "C" fn handler(_sig: libc::c_int) {
    let prev = SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
    if prev >= 2 {
        // Third signal: force exit. Use _exit to avoid atexit deadlocks.
        unsafe { libc::_exit(128 + _sig) };
    }
    // Forward the same signal to any active pipeline groups so external
    // children die too.
    // In the interactive shell the relay handler is installed instead; this
    // path is reached only in batch mode (non-interactive).
    for slot in &RELAY_PGIDS {
        let pgid = slot.load(Ordering::Acquire);
        if pgid != 0 {
            unsafe {
                libc::kill(-pgid, _sig);
            }
        }
    }
}

/// Return the handler function pointer for selective signal installation.
#[cfg(unix)]
pub fn term_handler() -> extern "C" fn(libc::c_int) {
    handler
}

#[cfg(windows)]
pub fn install_handlers() {
    // SetConsoleCtrlHandler via the ctrlc crate.  The handler increments
    // the same SIGNAL_COUNT flag the evaluator polls between statements,
    // returns TRUE so Windows does not terminate the process, and fans
    // Ctrl-Break out to every active pipeline group (the Windows
    // analogue of the Unix relay handler).
    let _ = ctrlc::set_handler(|| {
        let prev = SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
        if prev >= 2 {
            std::process::exit(130);
        }
        win_relay_fire();
    });
}

#[cfg(not(any(unix, windows)))]
pub fn install_handlers() {}

// ── Pipeline relay ───────────────────────────────────────────────────────────
//
// When a pipeline has both internal (thread) and external (process) stages,
// we cannot hand the terminal to the external process group — internal threads
// live in the shell process and would receive SIGTTIN.  Instead we keep the
// terminal with the shell and forward SIGINT to every active external group.
//
// RELAY_PGIDS is a fixed slot array.  Each PipelineRelay claims one slot with
// CAS; the handler iterates all slots and sends to any non-zero entry.  When
// no slots are active the handler does nothing, which is effectively SIG_IGN
// for the shell.  The handler is installed once at startup and never removed,
// so there is no install/uninstall race.

#[cfg(unix)]
const MAX_RELAY: usize = 8;

#[cfg(unix)]
static RELAY_PGIDS: [AtomicI32; MAX_RELAY] = [const { AtomicI32::new(0) }; MAX_RELAY];

#[cfg(unix)]
extern "C" fn sigint_relay(_: libc::c_int) {
    for slot in &RELAY_PGIDS {
        let pgid = slot.load(Ordering::Acquire);
        if pgid != 0 {
            unsafe {
                libc::kill(-pgid, libc::SIGINT);
            }
        }
    }
}

/// Return the relay handler for installation by the interactive shell.
#[cfg(unix)]
pub fn relay_handler() -> extern "C" fn(libc::c_int) {
    sigint_relay
}

/// RAII guard: holds a slot in RELAY_PGIDS for the duration of a mixed pipeline.
/// Clearing the slot on drop is the only cleanup needed.
#[cfg(unix)]
pub struct PipelineRelay(usize);

#[cfg(unix)]
impl PipelineRelay {
    /// Claim an empty slot and record `pgid`.  Returns `None` if all slots are
    /// full (should not happen in practice; 8 concurrent mixed pipelines).
    pub fn install(pgid: libc::pid_t) -> Option<Self> {
        for (i, slot) in RELAY_PGIDS.iter().enumerate() {
            if slot
                .compare_exchange(0, pgid, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return Some(PipelineRelay(i));
            }
        }
        None
    }
}

#[cfg(unix)]
impl Drop for PipelineRelay {
    fn drop(&mut self) {
        RELAY_PGIDS[self.0].store(0, Ordering::Release);
    }
}

// ── Windows pipeline-group state ────────────────────────────────────────────
//
// Windows has neither POSIX pgids nor `kill(-pgid, …)`.  The pieces we
// build out of Win32 primitives:
//
//   * Each external pipeline stage is spawned with
//     `CREATE_NEW_PROCESS_GROUP` so it's individually addressable via
//     `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)` (Ctrl-C is
//     deliberately suppressed in groups created this way; Ctrl-Break
//     is always delivered).
//   * A single Job Object per pipeline.  Every stage is
//     `AssignProcessToJobObject`'d into it, so a single
//     `TerminateJobObject` on the abort path takes the whole tree —
//     including descendants spawned by stages.
//   * The list of member pids per pipeline is kept in `GROUPS` keyed
//     by the leader's pid (the same value `Pgid` carries).  The Ctrl-C
//     handler walks that list directly to fan Ctrl-Break out to every
//     stage individually, since console process groups can't be
//     joined post-spawn.  No separate "active relay" list — the
//     registry IS the live-pipelines set.
//
// `GROUPS` uses a `Mutex` rather than the lock-free atomic-array trick
// used for `RELAY_PGIDS` on Unix.  Windows `SetConsoleCtrlHandler`
// callbacks run on a worker thread, not in async-signal context, so a
// mutex is safe and the hot paths (pipeline start / Ctrl-C arrival)
// are infrequent enough that lock contention is irrelevant.

#[cfg(windows)]
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

    /// Live pipeline groups, keyed by leader pid.  A `Vec` rather than
    /// a map: at most a handful of pipelines are ever simultaneously
    /// live and a linear walk is fine.
    pub(super) static GROUPS: Mutex<Vec<(i32, GroupState)>> = Mutex::new(Vec::new());

    /// Create a fresh Job Object, assign `leader`, and register the
    /// group keyed by the leader's pid.  Returns the leader pid as i32
    /// (== Pgid value).
    pub(super) fn new_leader(leader: &std::process::Child) -> i32 {
        let leader_pid = leader.id() as i32;
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null_mut()) };
        if !job.is_null() {
            // KILL_ON_JOB_CLOSE means: when the last handle to the
            // job is closed (we hold exactly one), every still-alive
            // member is terminated.  Wires the abort path to plain
            // RAII — `release(leader)` does the kill-and-free.
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
    /// because Windows console process groups can't be joined: each
    /// stage was spawned with its own `CREATE_NEW_PROCESS_GROUP`.
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
    /// `PipelineGroup::Drop`; idempotent (release on a missing key is
    /// a no-op).  Closing the only handle to a job created with
    /// `KILL_ON_JOB_CLOSE` terminates any still-alive members — that's
    /// the abort-path teardown, no explicit `TerminateJobObject` needed.
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
/// `spawn_with_pgid` and cleaned up by `release_win_group` from
/// `PipelineGroup::Drop`.  `install` exists only to keep the
/// cross-platform call site in `pipeline.rs` free of cfg gates.
#[cfg(windows)]
pub struct PipelineRelay;

#[cfg(windows)]
impl PipelineRelay {
    pub fn install(_pgid: i32) -> Option<Self> {
        Some(Self)
    }
}

/// Fire from the ctrlc handler: walk the live pipeline groups and
/// send Ctrl-Break to every member of every group.  Locking inside a
/// Win32 console handler is fine — the OS calls handlers on a
/// dedicated worker thread, not in signal-handler context.
#[cfg(windows)]
fn win_relay_fire() {
    win_groups::signal_all();
}

/// Release the Job Object backing the pipeline group identified by
/// `leader`.  Closing the only handle terminates any still-alive
/// members (the job was created with `KILL_ON_JOB_CLOSE`).
/// Idempotent: safe to call from `PipelineGroup::Drop` even when
/// `PipelineRelay::Drop` already released.
#[cfg(windows)]
pub fn release_win_group(leader: i32) {
    win_groups::release(leader);
}

// ── Cooperative cancellation ────────────────────────────────────────────────
//
// `CancelScope` is the structured-concurrency primitive: a tree of Arc-shared
// flags.  A worker checks `is_cancelled` at well-defined poll points (the same
// places `signal::check` is already called); cancelling an outer scope
// propagates to every inner scope, so a top-level Ctrl-C — or a
// `RunningPipeline::Drop` on the abort path — unwinds every thread that
// inherited the scope at its next poll point.
//
// The chain is walked, not flattened, so subscopes can carry their own
// flag (cancelling only their subtree) while still observing parent
// cancellation.  No mutex, no allocation in the hot path — just an
// `AtomicBool::load` per ancestor.

/// Internal node of the cancel-scope tree.  A scope is cancelled if its
/// own flag is set OR any ancestor's flag is set.
#[derive(Debug)]
struct ScopeNode {
    flag: AtomicU8,
    parent: Option<std::sync::Arc<ScopeNode>>,
}

/// A handle into the cancel-scope tree.  Cheap to clone (one `Arc`
/// bump); cheap to check (chain of atomic loads).
///
/// Construction:
///   * `root()` — a fresh top-level scope with no parent.
///   * `child()` — a new scope nested under `self`; cancelling `self` (or
///     any of its ancestors) cancels the child too.
///   * `share()` — clone the *same* scope (no new node); used when
///     handing the current scope to a spawned thread.
///
/// Cancellation is one-way: once cancelled, a scope stays cancelled.
#[derive(Debug, Clone)]
pub struct CancelScope(std::sync::Arc<ScopeNode>);

impl CancelScope {
    /// A fresh root scope.  Used by the default `Shell` and by tests
    /// that don't need cancellation.
    pub fn root() -> Self {
        Self(std::sync::Arc::new(ScopeNode {
            flag: AtomicU8::new(0),
            parent: None,
        }))
    }

    /// A new scope nested under `self`.  Cancelling any ancestor (or
    /// `self`) cancels the returned child.  `RunningPipeline` creates
    /// one of these per pipeline so cancelling the pipeline doesn't
    /// reach the parent shell.
    pub fn child(&self) -> Self {
        Self(std::sync::Arc::new(ScopeNode {
            flag: AtomicU8::new(0),
            parent: Some(self.0.clone()),
        }))
    }

    /// Set this scope's flag.  Idempotent.  Visible to every share /
    /// child of this scope at the next `is_cancelled` poll.
    pub fn cancel(&self) {
        self.0.flag.store(1, Ordering::Release);
    }

    /// Walk the parent chain, returning true if any node's flag is set.
    pub fn is_cancelled(&self) -> bool {
        let mut node: &std::sync::Arc<ScopeNode> = &self.0;
        loop {
            if node.flag.load(Ordering::Acquire) != 0 {
                return true;
            }
            match &node.parent {
                Some(p) => node = p,
                None => return false,
            }
        }
    }
}

impl Default for CancelScope {
    fn default() -> Self {
        Self::root()
    }
}

// ── Process-group placement ──────────────────────────────────────────────────
//
// Every spawned external process makes one of three choices about its
// process group: stay in the parent's group, become a leader of a fresh
// group, or join an existing group as a non-leader.  `PgidPolicy` makes
// that choice explicit at the type level — no `pid_t == 0` sentinel, no
// "did the caller remember to setpgid?" comments.

/// Newtype wrapping a process-group identifier.
///
/// On Unix: a POSIX pgid (== leader's pid).  Addressable via
/// `kill(-pgid, sig)` to fan a signal out across every member.
///
/// On Windows: the leader's pid.  Windows console process groups can't
/// be joined post-spawn — every external pipeline stage is its own
/// group leader (spawned with `CREATE_NEW_PROCESS_GROUP`).  The
/// `Pgid` here is the *first* stage's pid; the `PipelineGroup` keeps
/// the full member list and a Job Object that ties them together for
/// abort-time `TerminateJobObject`.
///
/// Constructed only from a real pid; no `0` sentinel encodes "no pgid"
/// — the `Option<Pgid>` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pgid(pub i32);

/// Process-group placement decision applied via `pre_exec` before `execve`.
///
/// Single-command foreground jobs and pipeline first stages use
/// `NewLeader`; subsequent pipeline stages use `Join(leader_pgid)`;
/// non-pipeline non-foreground children use `Inherit`.
#[derive(Clone, Copy, Debug)]
pub enum PgidPolicy {
    /// Inherit the parent's pgid — no `setpgid` call.
    Inherit,
    /// Become the leader of a fresh process group (`setpgid(0, 0)`).
    NewLeader,
    /// Join an existing pgid as a non-leader (`setpgid(0, leader)`).
    Join(Pgid),
}

#[cfg(unix)]
impl PgidPolicy {
    /// Apply this policy from inside a `pre_exec` closure.  Safe to call from
    /// the post-fork pre-exec context; no allocation, no stdlib mutex use.
    ///
    /// Callers should not invoke `setpgid` directly — `spawn_with_pgid` is
    /// the single funnel that installs this policy and mirrors it in the
    /// parent.  Searching for `setpgid` should yield this method plus the
    /// parent-side mirror inside `spawn_with_pgid`.
    pub fn apply(self) {
        unsafe {
            match self {
                PgidPolicy::Inherit => {}
                PgidPolicy::NewLeader => {
                    libc::setpgid(0, 0);
                }
                PgidPolicy::Join(Pgid(leader)) => {
                    libc::setpgid(0, leader);
                }
            }
        }
    }
}

#[cfg(not(unix))]
impl PgidPolicy {
    pub fn apply(self) {}
}

/// Spawn `cmd` with a single, canonical pre-exec discipline:
///
///   1. inside the child (post-fork, pre-exec): apply `pgid`, then
///      `reset_child_signals` (with the nohup rule);
///   2. inside the parent (post-spawn): mirror the `setpgid` so the
///      child's pgid is established regardless of which side wins
///      the race.
///
/// Returns the child plus its leader pgid: `Some` for `NewLeader` /
/// `Join`, `None` for `Inherit`.  Callers that need the leader pgid for
/// later `wait_handling_stop` or for the pipeline group simply read it
/// off the return value — there is no separate registration step.
///
/// Ordering: `pre_exec` closures run in registration order, so any
/// caller-installed `pre_exec` (sandbox `RLIMIT`, `2>&1` dup2) runs
/// *before* the closure this function adds.  That order is intentional:
/// fd plumbing and rlimits are independent of pgid placement, and
/// `reset_child_signals` is the last thing we want to happen before
/// `execve` clears the slate.
#[cfg(unix)]
pub fn spawn_with_pgid(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
) -> std::io::Result<(std::process::Child, Option<Pgid>)> {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(move || {
            pgid.apply();
            reset_child_signals();
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    let leader = match pgid {
        PgidPolicy::Inherit => None,
        PgidPolicy::NewLeader => {
            let pid = child.id() as libc::pid_t;
            unsafe { libc::setpgid(pid, pid) };
            Some(Pgid(pid))
        }
        PgidPolicy::Join(p) => {
            let pid = child.id() as libc::pid_t;
            unsafe { libc::setpgid(pid, p.0) };
            Some(p)
        }
    };
    Ok((child, leader))
}

/// Windows arm: there's no `pre_exec`, no pgid, no `setpgid`.  Three
/// gestures replace them:
///
///   * `CREATE_NEW_PROCESS_GROUP` so the child can be addressed
///     individually by `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)`
///     — Windows console process groups can't be joined post-spawn,
///     so every external pipeline stage becomes its own group leader.
///   * Job Object membership so the whole pipeline (including
///     descendants) can be torn down with one `TerminateJobObject` on
///     the abort path.  `NewLeader` creates a fresh job and assigns
///     this stage; `Join` looks up the leader's job and assigns there.
///   * No signal-disposition reset — Windows children inherit the
///     parent's console-control handlers, and Rust's runtime does not
///     install console handlers we need to undo.
///
/// The returned `Pgid` is the leader pid (the *first* stage's pid),
/// matching the value `pipeline/group.rs` already keys off.
#[cfg(windows)]
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

/// Bare fallback for `cfg(not(any(unix, windows)))` — wasm and the
/// like.  Keeps the compile path open without trying to emulate
/// process-group semantics on platforms where they don't exist.
#[cfg(not(any(unix, windows)))]
pub fn spawn_with_pgid(
    cmd: &mut std::process::Command,
    _pgid: PgidPolicy,
) -> std::io::Result<(std::process::Child, Option<Pgid>)> {
    let child = cmd.spawn()?;
    Ok((child, None))
}

/// Stub `PipelineRelay` for `cfg(not(any(unix, windows)))` so callers
/// can use it unconditionally.
#[cfg(not(any(unix, windows)))]
pub struct PipelineRelay;

#[cfg(not(any(unix, windows)))]
impl PipelineRelay {
    pub fn install(_pgid: i32) -> Option<Self> {
        None
    }
}

/// The signals whose startup disposition we snapshot in
/// `snapshot_inherited_ignored` and consult in `reset_child_signals`.
/// Listed once so the two consumers cannot drift.
///
/// SIGPIPE is deliberately absent: Rust's runtime sets SIGPIPE=IGN at
/// startup so panics on broken-pipe writes are graceful, and that
/// disposition would falsely register as "parent intent" by the time
/// ral reads it.  SIGPIPE is reset unconditionally to SIG_DFL — see
/// `reset_child_signals`.
#[cfg(unix)]
const MANAGED_SIGNALS: &[libc::c_int] = &[
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
    libc::SIGHUP,
];

/// Bitmask of signals that were SIG_IGN when ral started, indexed by
/// signal number.  Captured by `install_handlers` before any of ral's
/// own dispositions are installed.  Read in `reset_child_signals` to
/// honor the POSIX nohup rule: a signal the parent deliberately set to
/// SIG_IGN (e.g. `nohup ral …` ignoring SIGHUP, or `cmd | ral …` where
/// `cmd` ignored SIGPIPE) must remain SIG_IGN in our spawned children.
///
/// All managed signal numbers are < 64, so a single u64 suffices.
#[cfg(unix)]
static INHERITED_IGNORED: AtomicU64 = AtomicU64::new(0);

/// Record which `MANAGED_SIGNALS` are currently SIG_IGN.  Idempotent
/// once the shell process has installed its own handlers — subsequent
/// calls would see ral's handlers, not the inherited dispositions, so
/// this must run exactly once at startup, before `install_handlers`
/// touches anything.
#[cfg(unix)]
fn snapshot_inherited_ignored() {
    let mut mask: u64 = 0;
    for &sig in MANAGED_SIGNALS {
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) };
        if rc == 0 && old.sa_sigaction == libc::SIG_IGN {
            mask |= 1u64 << sig;
        }
    }
    INHERITED_IGNORED.store(mask, Ordering::Release);
}

/// True iff `sig` was SIG_IGN when ral started.  Async-signal-safe: a
/// single atomic load.  Safe to call from a `pre_exec` closure.
#[cfg(unix)]
fn was_inherited_ignored(sig: libc::c_int) -> bool {
    let mask = INHERITED_IGNORED.load(Ordering::Acquire);
    (mask & (1u64 << sig)) != 0
}

/// Restore the appropriate disposition for the signals that ral overrides
/// or ignores in its own process.  Must run from the post-fork pre-exec
/// closure of every external-child spawn — it is *not* a foreground-only
/// concern.
///
/// Without an explicit reset, every spawned external would inherit ral's
/// handler pointers.  execve(2) resets handler pointers to SIG_DFL
/// automatically, so this would mostly work — but SIG_IGN survives
/// execve.  Anything whose disposition should be SIG_IGN must therefore
/// be set *explicitly* to SIG_IGN here.
///
/// The nohup rule: a signal that was already SIG_IGN when ral started —
/// recorded in `INHERITED_IGNORED` — must be SIG_IGN in our children
/// too.  The parent (e.g. `nohup`) deliberately set it; that intent has
/// to survive ral.  For every other managed signal, SIG_DFL is the
/// right disposition because external commands expect default behavior.
///
/// SIGPIPE is special-cased to SIG_DFL unconditionally.  Rust's runtime
/// sets SIGPIPE=IGN at startup; that's not user intent and must not
/// propagate — pipeline producers need SIGPIPE-DFL to die cleanly when
/// their reader closes (`yes | head`).
///
/// Universal — applies to standalone externals, pipeline external
/// stages, foreground or backgrounded.
#[cfg(unix)]
pub fn reset_child_signals() {
    for &sig in MANAGED_SIGNALS {
        let target = if was_inherited_ignored(sig) {
            libc::SIG_IGN
        } else {
            libc::SIG_DFL
        };
        unsafe {
            libc::signal(sig, target);
        }
    }
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
pub fn reset_child_signals() {}

// ── Wait handling for stopped children ──────────────────────────────────────
//
// `Child::wait()` calls `waitpid(pid, &status, 0)` which only returns on
// termination.  A child stopped by SIGTSTP (Ctrl-Z), SIGSTOP, or SIGTTIN
// stays stopped indefinitely — the wait blocks, the controlling tty stays
// owned by the stopped pgid, and ral hangs.
//
// `wait_handling_stop` uses `waitpid(pid, ..., WUNTRACED)` so the wait
// returns on stop too.  ral has no job control yet, so the response is
// to kill the entire pgid (so the rest of a pipeline dies together) and
// loop to reap.  The eventual return is always an exited or signalled
// status — never stopped.

/// Wait for `child` to terminate, killing its pgid if it stops.
///
/// Why this exists: see the module-level commentary above.  Returning a
/// stopped status would just push the deadlock up one level — the caller
/// has no way to express "stopped" in the pipeline result type, and
/// without job-control machinery there's nothing useful for it to do
/// with such a status.
#[cfg(unix)]
pub fn wait_handling_stop(
    child: &mut std::process::Child,
    pgid: Option<Pgid>,
) -> std::io::Result<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    let pid = child.id() as libc::pid_t;
    loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        if libc::WIFSTOPPED(status) {
            crate::dbg_trace!(
                "fg",
                "pid {pid} stopped (signal {}); killing pgid {:?}",
                libc::WSTOPSIG(status),
                pgid
            );
            match pgid {
                Some(Pgid(p)) => unsafe {
                    libc::kill(-p, libc::SIGKILL);
                },
                None => {
                    let _ = child.kill();
                }
            }
            // Loop to reap the now-dying child.
            continue;
        }
        return Ok(std::process::ExitStatus::from_raw(status));
    }
}

#[cfg(not(unix))]
pub fn wait_handling_stop(
    child: &mut std::process::Child,
    _pgid: Option<Pgid>,
) -> std::io::Result<std::process::ExitStatus> {
    child.wait()
}

// ── Foreground ownership ─────────────────────────────────────────────────────
//
// `tcsetpgrp` hands the controlling tty to a target process group; it must be
// reversed before the next REPL read or that read returns EIO (ral ignores
// SIGTTIN, see repl.rs).  `ForegroundGuard` makes the restoration unconditional
// under any early return: acquiring the guard performs `tcsetpgrp(target)`,
// dropping it performs `tcsetpgrp(saved)`.
//
// On Windows the guard is a zero-sized no-op — and that is correct, not
// a stub.  Windows has a single shared console; every attached process
// reads/writes it concurrently, with no notion of a "foreground process
// group" that owns the tty.  Console programs (fzf, less, vim) talk to
// the Console API directly and work without any handoff from ral.  The
// internal-stage SIGTTIN deadlock that motivates the Mixed-pipeline
// gate on Unix simply does not exist here.

/// RAII guard for terminal foreground ownership.
///
/// `try_acquire` performs `tcsetpgrp(STDIN_FILENO, target)` and remembers the
/// previous foreground pgid; `drop` restores it.  Returns `None` when the
/// shell isn't interactive, stdin isn't a tty, or the syscall fails — in
/// those cases there's nothing to restore.
#[cfg(unix)]
pub struct ForegroundGuard {
    saved_pgid: libc::pid_t,
}

#[cfg(unix)]
impl ForegroundGuard {
    /// Hand the controlling tty to `target`, recording the prior pgid for
    /// the eventual restore.  Returns `None` when no handoff is appropriate.
    pub fn try_acquire(target: libc::pid_t, shell: &crate::types::Shell) -> Option<Self> {
        if !shell.io.interactive || !shell.io.terminal.startup_stdin_tty || target == 0 {
            return None;
        }
        let saved = unsafe { libc::getpgrp() };
        let rc = unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, target) };
        if rc != 0 {
            #[cfg(debug_assertions)]
            crate::dbg_trace!(
                "fg",
                "acquire: tcsetpgrp({target}) failed: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        Some(Self { saved_pgid: saved })
    }
}

#[cfg(unix)]
impl Drop for ForegroundGuard {
    /// Restore the foreground pgid recorded at acquisition.
    ///
    /// Critical: a missed restore puts ral into a background pgroup whose
    /// next tty read returns EIO.  Retry on EINTR; on persistent failure
    /// log via `dbg_trace` and verify with `tcgetpgrp` so the silent-loss
    /// case is at least observable.
    fn drop(&mut self) {
        for _ in 0..3 {
            let rc = unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, self.saved_pgid) };
            if rc == 0 {
                return;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                crate::dbg_trace!(
                    "fg",
                    "release: tcsetpgrp({}) failed: {err}",
                    self.saved_pgid
                );
                break;
            }
        }
        let cur = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        if cur != self.saved_pgid {
            crate::dbg_trace!(
                "fg",
                "release: tty fg is {cur}, want {} (next tty read may EIO)",
                self.saved_pgid
            );
        }
    }
}

/// Non-unix `ForegroundGuard`: a zero-sized type whose `try_acquire`
/// always returns `None`.  See the module-level comment above —
/// Windows shares one console across attached processes, so there is
/// nothing to acquire and nothing to release.
#[cfg(not(unix))]
pub struct ForegroundGuard;

#[cfg(not(unix))]
impl ForegroundGuard {
    pub fn try_acquire(_target: i32, _shell: &crate::types::Shell) -> Option<Self> {
        None
    }
}

/// Count of currently occupied relay slots (for testing).
#[cfg(all(unix, test))]
fn active_relay_slots() -> usize {
    RELAY_PGIDS
        .iter()
        .filter(|s| s.load(Ordering::Acquire) != 0)
        .count()
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, Mutex};

    // All relay tests share a process-wide lock because RELAY_PGIDS is a
    // global.  Tests run concurrently in the same process by default; without
    // this they would steal each other's slots.
    static RELAY_TEST_LOCK: Mutex<()> = Mutex::new(());

    // ── Slot allocation ──────────────────────────────────────────────────────

    #[test]
    fn slots_fill_and_drain() {
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        // Claim all 8 slots with distinct pgids.
        let guards: Vec<_> = (1..=MAX_RELAY as i32)
            .map(|pgid| PipelineRelay::install(pgid).expect("slot should be free"))
            .collect();
        assert_eq!(active_relay_slots(), MAX_RELAY);

        // A 9th install must fail.
        assert!(PipelineRelay::install(99).is_none());

        // Drop all; every slot must be released.
        drop(guards);
        assert_eq!(active_relay_slots(), 0);
    }

    #[test]
    fn released_slot_is_reusable() {
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        let g1 = PipelineRelay::install(1).unwrap();
        drop(g1);
        // The same pgid should be installable again immediately.
        let g2 = PipelineRelay::install(1).unwrap();
        drop(g2);
        assert_eq!(active_relay_slots(), 0);
    }

    // ── Concurrency stress ───────────────────────────────────────────────────

    #[test]
    fn concurrent_install_drop_stress() {
        // 8 threads race to claim all slots simultaneously, hold briefly,
        // release.  Repeat 500 times.  No slot should ever be double-claimed
        // or leaked.
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        const ROUNDS: usize = 500;

        for round in 0..ROUNDS {
            let barrier = Arc::new(Barrier::new(MAX_RELAY));
            let handles: Vec<_> = (0..MAX_RELAY)
                .map(|t| {
                    let b = barrier.clone();
                    std::thread::spawn(move || {
                        b.wait(); // all threads start together
                        let pgid = (round * MAX_RELAY + t + 1) as i32;
                        let g = PipelineRelay::install(pgid)
                            .expect("slot unavailable — possible double-claim");
                        std::thread::yield_now();
                        drop(g);
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(active_relay_slots(), 0, "slot leak after round {round}");
        }
    }

    #[test]
    fn overflow_returns_none_not_panic() {
        // Fill all slots, then hammer install from many threads simultaneously.
        // Every extra install must return None, never panic or corrupt state.
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        let _guards: Vec<_> = (1..=MAX_RELAY as i32)
            .map(|p| PipelineRelay::install(p).unwrap())
            .collect();

        let handles: Vec<_> = (0..32)
            .map(|_| {
                std::thread::spawn(|| {
                    for pgid in 100..200i32 {
                        let _ = PipelineRelay::install(pgid); // must not panic
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    // ── Signal forwarding ────────────────────────────────────────────────────

    #[test]
    fn relay_delivers_sigint_to_child_group() {
        // Spawn `sleep 1000` as a child in its own process group (via
        // pre_exec).  Claim a relay slot for the child's pgid and call
        // sigint_relay directly — equivalent to the shell receiving SIGINT
        // while the pipeline runs.  Verify the child was killed by the signal.
        //
        // We use Command + pre_exec rather than fork() to avoid the hazards of
        // forking inside a multithreaded test binary.
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        use std::os::unix::process::CommandExt;

        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("1000");
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn sleep");
        let child_pid = child.id() as libc::pid_t;

        // Parent mirrors setpgid to close the race.
        unsafe {
            libc::setpgid(child_pid, child_pid);
        }

        // Claim relay slot and fire the handler directly.
        let _relay = PipelineRelay::install(child_pid).expect("slot");
        sigint_relay(libc::SIGINT);

        let status = child.wait().expect("wait");
        // sleep was killed by SIGINT; exit code should be non-zero.
        assert!(!status.success(), "child should have been killed by SIGINT");
    }
}
