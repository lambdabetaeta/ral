//! Unix signal handling and process-group machinery.
//!
//! Three concerns interlock here:
//!
//!   * **Termination signals** (SIGINT/SIGTERM/SIGHUP) are translated
//!     into a [`CancelCause`](super::CancelCause) on the published cancel
//!     slots — the same delivery every other cancellation uses — and tick
//!     the escalation ladder whose third delivery forces `_exit`.
//!   * **Pipeline relays** keep the controlling tty with the shell while
//!     mixed pipelines run, fanning Ctrl+C out to every external pgid.
//!   * **Process-group placement** is the discipline applied at fork:
//!     every external child gets `setpgid` + `reset_child_signals` via a
//!     single `pre_exec` funnel.

use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use super::{ESCALATION, Pgid, PgidPolicy};

// ── Termination handler ────────────────────────────────────────────────────

/// Install handlers for SIGINT, SIGTERM, SIGHUP.
///
/// Snapshots inherited
/// `SIG_IGN` dispositions *before* installing ral's own handlers so the
/// nohup rule (preserve dispositions the parent deliberately ignored) can
/// be honored in spawned children — see [`reset_child_signals`].
pub fn install_handlers() {
    snapshot_inherited_ignored();
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, handler as *const () as libc::sighandler_t);
    }
}

extern "C" fn handler(_sig: libc::c_int) {
    let prev = ESCALATION.fetch_add(1, Ordering::Relaxed);
    if prev >= 2 {
        // Third signal: force exit. Use _exit to avoid atexit deadlocks.
        unsafe { libc::_exit(128 + _sig) };
    }
    // Translate the signal into a cause on the published cancel slots —
    // async-signal-safe (a slot load plus a `fetch_max`).  SIGINT is an
    // interrupt of the current foreground turn; SIGTERM/SIGHUP are a
    // shutdown request and land on the durable root, reaching detached
    // workers too.  Every wait loop already polls its scope, so this is
    // what preempts a blocked external child.
    if _sig == libc::SIGINT {
        super::request_foreground_cancel(super::CancelCause::Interrupt);
    } else {
        super::request_root_cancel(super::CancelCause::Terminate);
    }
    // Forward the same signal to any active pipeline groups so external
    // children die too.  The interactive shell rebinds SIGINT to the relay
    // handler instead, so SIGINT reaches this loop only in batch mode;
    // SIGTERM/SIGHUP always reach it.
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
pub fn term_handler() -> extern "C" fn(libc::c_int) {
    handler
}

// ── Pipeline relay ─────────────────────────────────────────────────────────
//
// When a pipeline has both internal (thread) and external (process) stages,
// we cannot hand the terminal to the external process group — internal threads
// live in the shell process and would receive SIGTTIN.  Instead we keep the
// terminal with the shell and forward SIGINT to every active external group.
//
// `RELAY_PGIDS` is a fixed slot array.  Each `PipelineRelay` claims one slot
// with CAS; the handler iterates all slots and sends to any non-zero entry.
// The handler is installed once at startup and never removed, so there is no
// install/uninstall race.  Beyond the relay, the handler also cancels the
// current turn's foreground scope so an in-process foreground computation
// unwinds on Ctrl-C; between turns that cancel is a no-op and the relay slots
// are typically empty, so an idle Ctrl-C at the prompt is left to the line
// editor.

const MAX_RELAY: usize = 8;

static RELAY_PGIDS: [AtomicI32; MAX_RELAY] = [const { AtomicI32::new(0) }; MAX_RELAY];

extern "C" fn sigint_relay(_: libc::c_int) {
    // Cancel the current turn's foreground scope so in-process foreground
    // work unwinds at its next poll; a no-op between turns (idle Ctrl-C at
    // the prompt is still handled by the line editor). Detached workers poll
    // their own scopes, not the foreground, so they are spared.
    super::request_foreground_cancel(super::CancelCause::Interrupt);
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
pub fn relay_handler() -> extern "C" fn(libc::c_int) {
    sigint_relay
}

extern "C" fn sigquit_handler(_: libc::c_int) {
    // Ctrl-\ in cooked mode: reap the whole session. Cancel the durable
    // root, reaching the foreground turn and every detached worker. A no-op
    // between turns (null slot), so an idle Ctrl-\ does not core-dump.
    super::request_root_cancel(super::CancelCause::RootAbort);
}

/// The SIGQUIT handler for the interactive shell to install for the
/// "reap everything" gesture.
pub fn quit_handler() -> extern "C" fn(libc::c_int) {
    sigquit_handler
}

/// RAII guard: holds a slot in `RELAY_PGIDS` for the duration of a mixed
/// pipeline.  Clearing the slot on drop is the only cleanup needed.
pub struct PipelineRelay(usize);

impl PipelineRelay {
    /// Claim an empty slot and record `pgid`.  Returns `None` if all slots
    /// are full (should not happen in practice; 8 concurrent mixed pipelines).
    pub fn install(pgid: libc::pid_t) -> Option<Self> {
        for (i, slot) in RELAY_PGIDS.iter().enumerate() {
            if slot
                .compare_exchange(0, pgid, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return Some(Self(i));
            }
        }
        None
    }
}

impl Drop for PipelineRelay {
    fn drop(&mut self) {
        RELAY_PGIDS[self.0].store(0, Ordering::Release);
    }
}

// ── Inherited dispositions and child-signal reset ──────────────────────────

/// The signals whose startup disposition we snapshot in
/// [`snapshot_inherited_ignored`] and consult in [`reset_child_signals`].
/// Listed once so the two consumers cannot drift.
///
/// SIGPIPE is deliberately absent: Rust's runtime sets SIGPIPE=IGN at startup
/// so panics on broken-pipe writes are graceful, and that disposition would
/// falsely register as "parent intent" by the time ral reads it.  SIGPIPE is
/// reset unconditionally to `SIG_DFL` — see [`reset_child_signals`].
const MANAGED_SIGNALS: &[libc::c_int] = &[
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
    libc::SIGHUP,
];

/// Bitmask of signals that were `SIG_IGN` when ral started, indexed by signal
/// number.  Captured by [`install_handlers`] before any of ral's own
/// dispositions are installed.  Read in [`reset_child_signals`] to honor the
/// POSIX nohup rule: a signal the parent deliberately set to `SIG_IGN` must
/// remain `SIG_IGN` in spawned children.
///
/// All managed signal numbers are < 64, so a single u64 suffices.
static INHERITED_IGNORED: AtomicU64 = AtomicU64::new(0);

fn snapshot_inherited_ignored() {
    let mut mask: u64 = 0;
    for &sig in MANAGED_SIGNALS {
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::sigaction(sig, std::ptr::null(), &raw mut old) };
        if rc == 0 && old.sa_sigaction == libc::SIG_IGN {
            mask |= 1u64 << sig;
        }
    }
    INHERITED_IGNORED.store(mask, Ordering::Release);
}

fn was_inherited_ignored(sig: libc::c_int) -> bool {
    let mask = INHERITED_IGNORED.load(Ordering::Acquire);
    (mask & (1u64 << sig)) != 0
}

/// Restore the appropriate disposition for the signals that ral overrides
/// or ignores in its own process.  Must run from the post-fork pre-exec
/// closure of every external-child spawn.
///
/// Without an explicit reset, every spawned external would inherit ral's
/// handler pointers.  `execve(2)` resets handler pointers to `SIG_DFL`
/// automatically, so this would mostly work — but `SIG_IGN` survives
/// `execve`.  Anything whose disposition should be `SIG_IGN` must therefore
/// be set *explicitly* to `SIG_IGN` here.
///
/// The nohup rule: a signal that was already `SIG_IGN` when ral started —
/// recorded in `INHERITED_IGNORED` — must be `SIG_IGN` in our children too.
/// The parent deliberately set it; that intent has to survive ral.  For
/// every other managed signal, `SIG_DFL` is the right disposition.
///
/// SIGPIPE is special-cased to `SIG_DFL` unconditionally.  Rust's runtime
/// sets SIGPIPE=IGN at startup; that's not user intent and must not
/// propagate — pipeline producers need SIGPIPE=DFL to die cleanly when
/// their reader closes (`yes | head`).
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

// ── Process-group placement ────────────────────────────────────────────────

impl PgidPolicy {
    /// Apply this policy from inside a `pre_exec` closure.  Safe to call
    /// from the post-fork pre-exec context; no allocation, no stdlib mutex
    /// use (`io::Error::last_os_error` only reads `errno`).
    ///
    /// A failed `setpgid` is an error: a child left in the wrong pgid
    /// would be invisible to the SIGINT relay and the abort path's
    /// group-wide SIGTERM.
    ///
    /// Callers should not invoke `setpgid` directly — [`spawn_with_pgid`]
    /// is the single funnel that installs this policy and mirrors it in
    /// the parent.  Searching for `setpgid` should yield this method plus
    /// the parent-side mirror inside [`spawn_with_pgid`].
    ///
    /// # Errors
    /// Returns `Err` if the `setpgid` / `setsid` syscall fails (`errno` via
    /// `last_os_error`); [`Inherit`](PgidPolicy::Inherit) makes no syscall
    /// and never fails.
    pub fn apply(self) -> std::io::Result<()> {
        // `setsid` returns the new sid (the caller's pid) on success;
        // `setpgid` returns 0.  Both signal failure with `-1`.
        let rc = unsafe {
            match self {
                Self::Inherit => 0,
                Self::NewLeader => libc::setpgid(0, 0),
                Self::NewSession => libc::setsid(),
                Self::Join(Pgid(leader)) => libc::setpgid(0, leader),
            }
        };
        if rc == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Spawn `cmd` with a single, canonical pre-exec discipline:
///
///   1. inside the child (post-fork, pre-exec): apply `pgid`, then
///      [`reset_child_signals`] (with the nohup rule);
///   2. inside the parent (post-spawn): mirror the `setpgid` so the child's
///      pgid is established regardless of which side wins the race.  A
///      `NewSession` child has no such mirror — only the child may `setsid`
///      itself — so it relies on its own `pre_exec`.
///
/// Returns the child plus its leader pgid: `Some` for `NewLeader` /
/// `NewSession` / `Join`, `None` for `Inherit`.  Callers that need the leader pgid for
/// later [`wait_handling_stop`] or for the pipeline group simply read it
/// off the return value — there is no separate registration step.
///
/// Ordering: `pre_exec` closures run in registration order, so any caller-
/// installed `pre_exec` (sandbox `RLIMIT`, `2>&1` dup2) runs *before* the
/// closure this function adds.  That order is intentional: fd plumbing
/// and rlimits are independent of pgid placement, and `reset_child_signals`
/// is the last thing we want to happen before `execve` clears the slate.
///
/// # Errors
/// Returns `Err` if the spawn fails — either the child's `setpgid` / `setsid`
/// in the pre-exec closure, or the `fork` / `exec` itself.
pub fn spawn_with_pgid(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
) -> std::io::Result<(std::process::Child, Option<Pgid>)> {
    spawn_with_pgid_after(cmd, pgid, || Ok(()))
}

/// Spawn `cmd` with canonical pgid placement and one caller-supplied
/// post-reset hook in the child.
///
/// The hook runs inside the child, after `setpgid` and signal reset, still
/// before `execve`. Callers must therefore restrict it to async-signal-safe
/// work such as `read`, `close`, or `dup2` on already-open fds.
///
/// # Errors
/// Returns `Err` if the child's `setpgid` / `setsid` fails, if the `after`
/// hook returns an error, or if the `fork` / `exec` itself fails.
pub fn spawn_with_pgid_after<F>(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
    after: F,
) -> std::io::Result<(std::process::Child, Option<Pgid>)>
where
    F: Fn() -> std::io::Result<()> + Send + Sync + 'static,
{
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(move || {
            pgid.apply()?;
            reset_child_signals();
            after()?;
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    // The mirror's `setpgid` result is deliberately ignored: by the time
    // it runs, either the child already applied the policy (a mirror
    // failure is then the benign post-`execve` `EACCES` race) or the
    // child's `pre_exec` failed and the spawn above returned the error.
    #[allow(clippy::cast_possible_wrap, reason = "child.id() is a live OS pid: positive and well below i32::MAX, so the u32→pid_t reinterpretation never wraps")]
    let leader = match pgid {
        PgidPolicy::Inherit => None,
        PgidPolicy::NewLeader => {
            let pid = child.id() as libc::pid_t;
            unsafe { libc::setpgid(pid, pid) };
            Some(Pgid(pid))
        }
        PgidPolicy::NewSession => {
            // `setsid` can run only in the child — a process cannot move
            // another into a new session — so there is no parent-side
            // mirror to close the race: the child's `pre_exec` is the sole
            // authority.  The new session's pgid equals the child's pid.
            Some(Pgid(child.id() as libc::pid_t))
        }
        PgidPolicy::Join(p) => {
            let pid = child.id() as libc::pid_t;
            unsafe { libc::setpgid(pid, p.0) };
            Some(p)
        }
    };
    Ok((child, leader))
}

// ── Wait handling for stopped children ─────────────────────────────────────
//
// `Child::wait()` calls `waitpid(pid, &status, 0)` which only returns on
// termination.  A child stopped by SIGTSTP (Ctrl-Z), SIGSTOP, or SIGTTIN
// stays stopped indefinitely — the wait blocks, the controlling tty stays
// owned by the stopped pgid, and ral hangs.
//
// `wait_handling_stop` uses `waitpid(pid, ..., WUNTRACED)` so the wait
// returns on stop too.  Behaviour on `WIFSTOPPED` depends on whether the
// caller opted into job-control parking:
//
//   * `park_on_stop = true` + `pgid = Some(_)`: return `Stopped` without
//     killing.  The caller (eventually `RunningChild::wait`) surfaces this
//     as `Escape::Stopped`; the REPL parks the pgid in the job table
//     and `fg` later resumes it.  Pump threads detach and clean up when
//     the child eventually completes or is killed.
//   * otherwise: legacy kill-then-reap.  SIGKILL the pgid (or fall back to
//     `child.kill()` when no pgid is tracked), drain the wait, return
//     `StoppedThenKilled`.

/// Wait for `child` to terminate or stop.  See module-level comment above
/// for the two stop-handling modes.
pub(super) fn wait_handling_stop(
    child: &mut std::process::Child,
    pgid: Option<Pgid>,
    park_on_stop: bool,
) -> std::io::Result<crate::process::WaitOutcome> {
    #[allow(clippy::cast_possible_wrap, reason = "child.id() is a live OS pid: positive and well below i32::MAX, so the u32→pid_t reinterpretation never wraps")]
    let pid = child.id() as libc::pid_t;
    let (r, status) = waitpid_eintr(pid, libc::WUNTRACED);
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    classify_wait_status(status, pid, pgid, park_on_stop, child)
}

/// `waitpid(target, flags)` with implicit `EINTR` retry.
///
/// Returns the raw
/// `(ret, status)`: `ret > 0` reaped a child, `ret == 0` is `WNOHANG` with
/// nothing pending, `ret < 0` is a terminal errno (e.g. `ECHILD`) with
/// `errno` set.  `EINTR` is retried and never reaches the caller, so a
/// signal delivery cannot be mistaken for `ECHILD` and flip a live job to
/// "gone".  Shared by the blocking wait here and the REPL job table's reap.
pub fn waitpid_eintr(target: libc::pid_t, flags: libc::c_int) -> (libc::pid_t, libc::c_int) {
    loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(target, &raw mut status, flags) };
        if r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return (r, status);
    }
}

/// Non-blocking variant of [`wait_handling_stop`] for cancel-aware
/// polling: returns `Ok(None)` when nothing is pending.  Crucially uses
/// `WUNTRACED` so a SIGSTOP'd child does not look "still running" — the
/// stop notification is consumed and classified just like in the
/// blocking path (parked when `park_on_stop`, otherwise killed and
/// reaped).  Without this, the pre-wait poll in
/// `RunningChild::wait` would spin forever on a self-stopping child.
pub(super) fn try_wait_handling_stop(
    child: &mut std::process::Child,
    pgid: Option<Pgid>,
    park_on_stop: bool,
) -> std::io::Result<Option<crate::process::WaitOutcome>> {
    #[allow(clippy::cast_possible_wrap, reason = "child.id() is a live OS pid: positive and well below i32::MAX, so the u32→pid_t reinterpretation never wraps")]
    let pid = child.id() as libc::pid_t;
    let mut status: libc::c_int = 0;
    let r = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG | libc::WUNTRACED) };
    if r < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            return Ok(None);
        }
        return Err(err);
    }
    if r == 0 {
        return Ok(None);
    }
    classify_wait_status(status, pid, pgid, park_on_stop, child).map(Some)
}

/// Convert a raw `waitpid` status into a [`WaitOutcome`].  Stopped
/// statuses fan out to [`handle_stopped`]; exited / signaled / native
/// statuses map straight through.  Shared between the blocking
/// `wait_handling_stop` and the non-blocking `try_wait_handling_stop`.
fn classify_wait_status(
    status: libc::c_int,
    pid: libc::pid_t,
    pgid: Option<Pgid>,
    park_on_stop: bool,
    child: &mut std::process::Child,
) -> std::io::Result<crate::process::WaitOutcome> {
    if libc::WIFSTOPPED(status) {
        return handle_stopped(libc::WSTOPSIG(status), pid, pgid, park_on_stop, child);
    }
    if libc::WIFEXITED(status) {
        return Ok(crate::process::WaitOutcome::Exited(libc::WEXITSTATUS(
            status,
        )));
    }
    if libc::WIFSIGNALED(status) {
        return Ok(crate::process::WaitOutcome::Signaled(
            crate::process::Signal::new(libc::WTERMSIG(status)),
        ));
    }
    Ok(crate::process::WaitOutcome::NativeCode(status))
}

/// Handle a `WIFSTOPPED` outcome: either park (when `park_on_stop &&
/// pgid.is_some()`) returning [`WaitOutcome::Stopped`], or SIGKILL the
/// pgid (falling back to the direct pid) and reap before returning
/// [`WaitOutcome::StoppedThenKilled`].
fn handle_stopped(
    stopped_signum: libc::c_int,
    pid: libc::pid_t,
    pgid: Option<Pgid>,
    park_on_stop: bool,
    child: &mut std::process::Child,
) -> std::io::Result<crate::process::WaitOutcome> {
    let stopped_by = crate::process::Signal::new(stopped_signum);
    if park_on_stop && pgid.is_some() {
        crate::dbg_trace!(
            "fg",
            "pid {pid} stopped (signal {}); parking pgid {:?}",
            stopped_by.display(),
            pgid
        );
        return Ok(crate::process::WaitOutcome::Stopped(stopped_by));
    }
    crate::dbg_trace!(
        "fg",
        "pid {pid} stopped (signal {}); killing pgid {:?}",
        stopped_by.display(),
        pgid,
    );
    match pgid {
        Some(Pgid(p)) => unsafe {
            libc::kill(-p, libc::SIGKILL);
        },
        None => {
            let _ = child.kill();
        }
    }
    reap_killed(pid, stopped_by)
}

/// Blocking [`waitpid_eintr`] that translates the terminal status into a
/// [`WaitOutcome::StoppedThenKilled`] (or, for an already-detached /
/// exited child, [`WaitOutcome::NativeCode`]).
fn reap_killed(
    pid: libc::pid_t,
    stopped_by: crate::process::Signal,
) -> std::io::Result<crate::process::WaitOutcome> {
    let (r, status) = waitpid_eintr(pid, 0);
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if libc::WIFSIGNALED(status) {
        return Ok(crate::process::WaitOutcome::StoppedThenKilled {
            stopped_by,
            killed_by: crate::process::Signal::new(libc::WTERMSIG(status)),
        });
    }
    if libc::WIFEXITED(status) {
        // The child raced the kill and exited on its own in the
        // stop→kill window — report the real exit, not a kill signal the
        // kernel never delivered.
        return Ok(crate::process::WaitOutcome::Exited(libc::WEXITSTATUS(
            status,
        )));
    }
    Ok(crate::process::WaitOutcome::NativeCode(status))
}

/// Capture stdin's line-discipline state via `tcgetattr`.
///
/// `None` when stdin is not a tty (ENOTTY) or the call otherwise fails.
/// The single snapshot primitive for every save/restore of terminal
/// state in the workspace — the foreground guard, the panic hook's
/// restore, and the cursor-position raw-mode probe — so the `tcgetattr`
/// into a zeroed `termios` lives in one place.  The restore is left to
/// the caller, whose `tcsetattr` flush mode (`TCSADRAIN` to drain the
/// child's last frame, `TCSANOW` for an immediate hand-back) is
/// site-specific.
pub fn termios_snapshot() -> Option<libc::termios> {
    unsafe {
        let mut t = std::mem::zeroed::<libc::termios>();
        (libc::tcgetattr(libc::STDIN_FILENO, &raw mut t) == 0).then_some(t)
    }
}

// ── Foreground ownership ───────────────────────────────────────────────────
//
// Two pieces of tty state must be returned to the shell when a foreground
// child exits: the foreground process group (`tcsetpgrp`) and the line
// discipline's termios (`tcsetattr`).  Missing the first puts ral into a
// background pgroup whose next read returns EIO; missing the second
// inherits whatever cooked/raw/ONLCR settings the child last wrote — vim,
// less, fzf, `stty -onlcr`, anything calling `cfmakeraw` — yielding the
// classic Unix staircase output or stuck raw mode.  `ForegroundGuard`
// snapshots both on acquire and restores both on Drop.

/// RAII guard for terminal foreground ownership and line-discipline state.
///
/// `try_acquire` performs `tcsetpgrp(STDIN_FILENO, target)`, snapshots the
/// current termios, and remembers the previous foreground pgid; `drop`
/// restores both.  It is *uncallable* without a [`TerminalLease`] borrow:
/// holding `&TerminalLease` is the proof that ral owns the controlling
/// terminal's foreground, replacing the old internal `startup_foreground`
/// re-check.  Returns `None` only when the pgid handoff itself fails (target
/// `0`, or `tcsetpgrp` errors) — in which case there is nothing to restore.
/// Termios snapshot may itself fail (ENOTTY on an exotic fd); that is
/// non-fatal and only the pgid half is restored on Drop.
///
/// [`TerminalLease`]: crate::process::TerminalLease
pub struct ForegroundGuard {
    saved_pgid: libc::pid_t,
    saved_termios: Option<libc::termios>,
}

impl ForegroundGuard {
    /// Hand the controlling tty to `target`, recording the prior pgid and
    /// termios for the eventual restore.  Returns `None` when no handoff is
    /// appropriate.
    pub fn try_acquire(
        target: libc::pid_t,
        _lease: &crate::process::TerminalLease,
    ) -> Option<Self> {
        if target == 0 {
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
        let saved_termios = termios_snapshot();
        if saved_termios.is_none() {
            crate::dbg_trace!(
                "fg",
                "acquire: tcgetattr failed: {}",
                std::io::Error::last_os_error()
            );
        }
        Some(Self {
            saved_pgid: saved,
            saved_termios,
        })
    }
}

/// Thread-local guard that blocks SIGTTOU while ral restores tty ownership.
///
/// POSIX treats `tcsetpgrp`/`tcsetattr` by a background process group as
/// terminal output; with SIGTTOU at its default disposition the kernel stops
/// the caller instead of completing the restore.  Foreground jobs necessarily
/// put ral in the background while they run, so the release path blocks
/// SIGTTOU for the tiny restore window.  Children still get SIGTTOU reset by
/// [`reset_child_signals`]; this guard is parent-local.
struct SigttouBlock {
    old: libc::sigset_t,
    active: bool,
}

impl SigttouBlock {
    /// Block SIGTTOU for the current thread, remembering the previous mask.
    fn new() -> Self {
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut old: libc::sigset_t = unsafe { std::mem::zeroed() };
        let active = unsafe {
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, libc::SIGTTOU);
            libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, &raw mut old) == 0
        };
        Self { old, active }
    }
}

impl Drop for SigttouBlock {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                libc::pthread_sigmask(libc::SIG_SETMASK, &raw const self.old, std::ptr::null_mut());
            }
        }
    }
}

impl Drop for ForegroundGuard {
    /// Restore the foreground pgid and termios recorded at acquisition.
    ///
    /// Pgid first: a missed restore puts ral into a background pgroup whose
    /// next tty read returns EIO.  The restore blocks SIGTTOU because the
    /// foreground child made ral a background process group by construction;
    /// without that mask a batch launcher (`claude.ral`) is itself stopped
    /// while giving the terminal back.  Termios second, with `TCSADRAIN` so
    /// the child's last buffered output drains under the child's settings
    /// before ours reapply (`TCSANOW` would clobber those bytes' line
    /// discipline; `TCSAFLUSH` would discard input typed during the child's
    /// final frame).  Both syscalls retry on EINTR; persistent failure is
    /// logged via `dbg_trace` so silent-loss is at least observable.
    fn drop(&mut self) {
        let _sigttou = SigttouBlock::new();
        for _ in 0..3 {
            let rc = unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, self.saved_pgid) };
            if rc == 0 {
                break;
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
        if let Some(t) = self.saved_termios.as_ref() {
            for _ in 0..3 {
                let rc = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, t) };
                if rc == 0 {
                    return;
                }
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::EINTR) {
                    crate::dbg_trace!("fg", "release: tcsetattr failed: {err}");
                    return;
                }
            }
        }
    }
}

/// Deliver the SIGINT that raw mode swallowed — but only to a foreground
/// job that is an *external* child, not the shell itself.
///
/// A frontend that puts the terminal in raw mode disables `ISIG`, so a
/// keypress no longer makes the kernel SIGINT whichever process group
/// owns the controlling tty.  When an external child holds the terminal
/// (its pgid differs from ours) this recreates that delivery, so the
/// child dies and the evaluator's blocking `wait` returns.  When the
/// shell itself owns the terminal there is no external process to
/// signal — the frontend's cooperative [`interrupt`](super::interrupt)
/// unwinds the in-process eval instead.
///
/// Signalling *another* group never touches ral's own escalating
/// counter, so this carries no risk of the third-signal force-exit.
pub fn interrupt_foreground_child() {
    let fg = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
    if fg <= 0 {
        return;
    }
    let me = unsafe { libc::getpgrp() };
    if fg != me {
        unsafe { libc::kill(-fg, libc::SIGINT) };
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// Count of currently occupied relay slots (for testing).
#[cfg(test)]
fn active_relay_slots() -> usize {
    RELAY_PGIDS
        .iter()
        .filter(|s| s.load(Ordering::Acquire) != 0)
        .count()
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::super::{
        CancelCause, CancelScope, SLOT_SERIAL, clear, escalation_pending, publish_durable_root,
        publish_foreground,
    };
    use super::*;
    use std::sync::{Arc, Barrier, Mutex};

    // All relay tests share a process-wide lock because `RELAY_PGIDS` is a
    // global.  Tests run concurrently in the same process by default;
    // without this they would steal each other's slots.
    static RELAY_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn sigttou_is_blocked() -> bool {
        let mut current: libc::sigset_t = unsafe { std::mem::zeroed() };
        let rc =
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &raw mut current) };
        assert_eq!(rc, 0, "pthread_sigmask query failed");
        unsafe { libc::sigismember(&raw const current, libc::SIGTTOU) == 1 }
    }

    #[test]
    fn sigttou_block_restores_signal_mask() {
        let was_blocked = sigttou_is_blocked();
        {
            let _block = SigttouBlock::new();
            assert!(sigttou_is_blocked());
        }
        assert_eq!(sigttou_is_blocked(), was_blocked);
    }

    // ── Slot allocation ────────────────────────────────────────────────────

    #[test]
    fn slots_fill_and_drain() {
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        // Claim all 8 slots with distinct pgids.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, reason = "MAX_RELAY is the const 8 and the loop arithmetic stays in the low thousands, so the usize→i32 conversion neither truncates nor wraps")]
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

    // ── Concurrency stress ─────────────────────────────────────────────────

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
                        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, reason = "MAX_RELAY is the const 8 and the loop arithmetic stays in the low thousands, so the usize→i32 conversion neither truncates nor wraps")]
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
        // Fill all slots, then hammer install from many threads
        // simultaneously.  Every extra install must return None, never panic
        // or corrupt state.
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap, reason = "MAX_RELAY is the const 8 and the loop arithmetic stays in the low thousands, so the usize→i32 conversion neither truncates nor wraps")]
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

    // ── Signal translation ─────────────────────────────────────────────────

    /// The batch/term handler translates SIGINT into a foreground
    /// [`Interrupt`](super::super::CancelCause::Interrupt): the current
    /// turn unwinds (and its blocked external wakes at the next scope
    /// poll), while the durable root — and with it every detached
    /// worker — is left untouched.  The delivery also ticks the
    /// escalation ladder, which is what backs the third-signal `_exit`.
    #[test]
    fn handler_translates_sigint_into_foreground_interrupt() {
        let _relay = RELAY_TEST_LOCK.lock().unwrap();
        let _slots = SLOT_SERIAL.lock().unwrap();
        clear();

        let foreground = CancelScope::root();
        let root = CancelScope::root();
        let _fg = publish_foreground(&foreground);
        let _rt = publish_durable_root(&root);

        handler(libc::SIGINT);
        assert_eq!(
            foreground.cause(),
            Some(CancelCause::Interrupt),
            "SIGINT must interrupt the published foreground scope"
        );
        assert_eq!(root.cause(), None, "SIGINT must not reach the durable root");
        assert!(
            escalation_pending(),
            "a delivered signal must tick the escalation ladder"
        );
        clear();
    }

    /// SIGTERM and SIGHUP are shutdown requests: the handler translates
    /// them into a root [`Terminate`](super::super::CancelCause::Terminate),
    /// reaching the foreground turn *and* every detached worker parented
    /// under the durable root — the semantics a `timeout(1)`- or
    /// systemd-style SIGTERM expects.  The foreground slot itself is not
    /// written; the foreground observes the cause through its root
    /// ancestry.
    #[test]
    fn handler_translates_sigterm_and_sighup_into_root_terminate() {
        let _relay = RELAY_TEST_LOCK.lock().unwrap();
        let _slots = SLOT_SERIAL.lock().unwrap();
        clear();

        let root = CancelScope::root();
        let foreground = root.child();
        let _fg = publish_foreground(&foreground);
        let _rt = publish_durable_root(&root);

        handler(libc::SIGTERM);
        assert_eq!(
            root.cause(),
            Some(CancelCause::Terminate),
            "SIGTERM must terminate the published durable root"
        );
        assert_eq!(
            foreground.cause(),
            Some(CancelCause::Terminate),
            "the foreground observes the termination through its root ancestry"
        );

        handler(libc::SIGHUP);
        assert_eq!(
            root.cause(),
            Some(CancelCause::Terminate),
            "SIGHUP is the same shutdown request as SIGTERM"
        );
        clear();
    }

    // ── Signal forwarding ──────────────────────────────────────────────────

    #[test]
    fn relay_delivers_sigint_to_child_group() {
        // Spawn `sleep 1000` as a child in its own process group (via
        // pre_exec).  Claim a relay slot for the child's pgid and call
        // sigint_relay directly — equivalent to the shell receiving SIGINT
        // while the pipeline runs.  Verify the child was killed by the
        // signal.
        //
        // We use Command + pre_exec rather than fork() to avoid the hazards
        // of forking inside a multithreaded test binary.
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
        #[allow(clippy::cast_possible_wrap, reason = "child.id() is a live OS pid: positive and well below i32::MAX, so the u32→pid_t reinterpretation never wraps")]
        let child_pid = child.id() as libc::pid_t;

        // Parent mirrors setpgid to close the race.
        unsafe {
            libc::setpgid(child_pid, child_pid);
        }

        // Claim relay slot and fire the handler directly.
        let _relay = PipelineRelay::install(child_pid).expect("slot");
        sigint_relay(libc::SIGINT);

        #[allow(clippy::disallowed_methods)]
        let status = child.wait().expect("wait");
        // sleep was killed by SIGINT; exit code should be non-zero.
        assert!(!status.success(), "child should have been killed by SIGINT");
    }
}
