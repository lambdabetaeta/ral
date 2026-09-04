//! Unix reaper: one `SIGCHLD` handler, one self-pipe, one thread.
//!
//! The handler does a single async-signal-safe `write` and nothing else; the
//! reaper thread blocks reading the pipe and, per wake, scans every watched
//! pid with two `waitid` calls:
//!
//! 1. `WSTOPPED | WNOHANG` — a stop is *consumed* and posted, or it would be
//!    re-reported on every wake.
//! 2. `WEXITED | WNOHANG | WNOWAIT` — an exit is posted once and the pid
//!    marked; the zombie is left for [`Watch::reap`], or the pid would be
//!    free for reuse while its owner may still signal it.
//!
//! A child exiting between the two calls is caught on the next `SIGCHLD`,
//! which its own exit raises.  No signal-mask discipline is needed: the
//! handler coexists with `std::process`, the test harness's own threads, and
//! children ral never registers (`Command::output()`, tests) — `waitid` is
//! never called on a pid that was not `watch`ed.

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock, PoisonError};

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal as NixSignal, sigaction};
use rustix::process::{Pid as RawPid, WaitId, WaitIdOptions, WaitIdStatus};

use crate::process::cloexec_pipe;
use crate::process::outcome::{Signal, WaitOutcome, WaitPoll};

type Poster = Box<dyn Fn(WaitPoll) + Send>;

struct Entry {
    poster: Poster,
    /// Set once `Done` has been posted, so a scan stops calling `waitid` for
    /// a pid whose exit is already reported and just awaiting `reap`.
    exited: bool,
}

static SUBS: OnceLock<Mutex<HashMap<u32, Entry>>> = OnceLock::new();
static WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Idempotent: the first call installs the handler, the self-pipe, and the
/// reaper thread. Called both from `watch` (so standalone use, including
/// tests, needs no wiring) and from `install_handlers` (so the handler is
/// armed at startup, ahead of any subscriber).
pub(crate) fn ensure_installed() {
    subs();
}

fn subs() -> &'static Mutex<HashMap<u32, Entry>> {
    SUBS.get_or_init(|| {
        let (reader, writer) = cloexec_pipe().expect("create reaper self-pipe");
        // Nonblocking write end: a full pipe means a wake is already pending
        // and unread, so the handler drops the byte instead of blocking.
        rustix::fs::fcntl_setfl(&writer, rustix::fs::OFlags::NONBLOCK)
            .expect("set reaper pipe write end nonblocking");
        WAKE_WRITE_FD.store(writer.as_raw_fd(), Ordering::Release);
        // Leaked deliberately: the self-pipe lives for the process, like the
        // handler itself — there is no shutdown path.
        std::mem::forget(writer);
        install_sigchld();
        spawn_reaper_thread(reader);
        Mutex::new(HashMap::new())
    })
}

fn install_sigchld() {
    let action = SigAction::new(SigHandler::Handler(sigchld_handler), SaFlags::SA_RESTART, SigSet::empty());
    // SA_NOCLDSTOP deliberately absent: a stop must reach the handler too.
    unsafe { sigaction(NixSignal::SIGCHLD, &action) }.expect("install SIGCHLD handler");
}

/// Async-signal-safe: one relaxed load and one `write(2)`, ignoring both the
/// fd not yet being set and the write's own result.
extern "C" fn sigchld_handler(_sig: libc::c_int) {
    let fd = WAKE_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte: u8 = 0;
        unsafe {
            libc::write(fd, std::ptr::addr_of!(byte).cast(), 1);
        }
    }
}

fn spawn_reaper_thread(reader: os_pipe::PipeReader) {
    std::thread::Builder::new()
        .name("ral-reaper".into())
        .spawn(move || run_with_restart(reader))
        .expect("spawn reaper thread");
}

/// `catch_unwind` as the second line of defence: a bug in one scan must not
/// silently stop reaping every other watched pid.
fn run_with_restart(mut reader: os_pipe::PipeReader) {
    loop {
        let _ =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reaper_loop(&mut reader)));
    }
}

fn reaper_loop(reader: &mut os_pipe::PipeReader) -> ! {
    let mut buf = [0u8; 64];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {}
            Ok(_) => scan_all(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {}
        }
    }
}

fn scan_all() {
    let mut table = subs().lock().unwrap_or_else(PoisonError::into_inner);
    for (&pid, entry) in table.iter_mut() {
        scan_one(pid, entry);
    }
}

/// A stop or exit already pending when `pid` is registered raised its
/// `SIGCHLD` before the table knew about it, so no later wake will re-report
/// it — `watch` calls this once, synchronously, right after inserting.
fn scan_one(pid: u32, entry: &mut Entry) {
    if entry.exited {
        return;
    }
    if let Some(sig) = poll_stop(pid) {
        (entry.poster)(WaitPoll::Stopped(sig));
    }
    if let Some(outcome) = poll_exit(pid) {
        entry.exited = true;
        (entry.poster)(WaitPoll::Done(outcome));
    }
}

fn poll_stop(pid: u32) -> Option<Signal> {
    let target = RawPid::from_raw(pid.cast_signed())?;
    let status = rustix::process::waitid(
        WaitId::Pid(target),
        WaitIdOptions::STOPPED | WaitIdOptions::NOHANG,
    )
    .ok()??;
    status.stopping_signal().map(Signal::new)
}

fn poll_exit(pid: u32) -> Option<WaitOutcome> {
    let target = RawPid::from_raw(pid.cast_signed())?;
    let status = rustix::process::waitid(
        WaitId::Pid(target),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
    )
    .ok()??;
    Some(outcome_from_waitid(status))
}

fn outcome_from_waitid(status: WaitIdStatus) -> WaitOutcome {
    if let Some(code) = status.exit_status() {
        WaitOutcome::Exited(code)
    } else if let Some(sig) = status.terminating_signal() {
        WaitOutcome::Signaled(Signal::new(sig))
    } else {
        // WEXITED only reports exited, killed, or dumped; the last two are
        // both `terminating_signal`, so this arm is unreachable in practice.
        WaitOutcome::NativeCode(1)
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Process-wide capability to watch children for exit and stop.
pub struct Reaper;

impl Reaper {
    /// The one reaper, started on first use.
    pub fn global() -> &'static Self {
        ensure_installed();
        static SINGLETON: Reaper = Reaper;
        &SINGLETON
    }

    /// Deliver `pid`'s events to `tx`, shaped by `f`.
    pub fn watch<E: Send + 'static>(
        &self,
        pid: u32,
        tx: Sender<E>,
        f: impl Fn(WaitPoll) -> E + Send + 'static,
    ) -> Watch {
        ensure_installed();
        let poster: Poster = Box::new(move |poll| {
            let _ = tx.send(f(poll));
        });
        let mut table = subs().lock().unwrap_or_else(PoisonError::into_inner);
        let entry = table.entry(pid).or_insert(Entry { poster, exited: false });
        // A stop or exit that raced ahead of this registration raised its
        // `SIGCHLD` before the table knew to look; catch it here rather
        // than waiting for a wake that may never come.
        scan_one(pid, entry);
        drop(table);
        Watch { pid }
    }
}

/// A subscription on one watched pid. Dropping it unsubscribes and reaps;
/// [`Self::reap`] does the same explicitly, returning the outcome.
#[must_use]
pub struct Watch {
    pid: u32,
}

impl Watch {
    /// Signal the watched child — the only sanctioned way to, since a bare
    /// pid held outside a `Watch` is racy against reuse.
    ///
    /// # Errors
    /// Returns `Err` if the platform `kill` fails.
    pub fn signal(&self, sig: Signal) -> io::Result<()> {
        let ret = unsafe { libc::kill(self.pid as libc::pid_t, sig.number()) };
        if ret == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Unsubscribe and block for the child's exit, consuming its zombie.
    ///
    /// # Errors
    /// Returns `Err` if the reaping wait fails.
    pub fn reap(self) -> io::Result<WaitOutcome> {
        let pid = self.pid;
        std::mem::forget(self);
        finish(pid)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = finish(self.pid);
    }
}

fn finish(pid: u32) -> io::Result<WaitOutcome> {
    subs()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&pid);
    blocking_reap(pid)
}

/// The one raw `waitpid` in this module: a real, consuming wait, called only
/// after unsubscribing so the reaper thread's own `WNOWAIT` scan cannot race
/// it for the same pid's exit status. By raw pid, not through a
/// `std::process::Child`, since this module never holds one.
fn blocking_reap(pid: u32) -> io::Result<WaitOutcome> {
    let mut status: libc::c_int = 0;
    loop {
        let ret = unsafe { libc::waitpid(pid as libc::pid_t, &raw mut status, 0) };
        if ret == -1 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        break;
    }
    use std::os::unix::process::ExitStatusExt;
    Ok(WaitOutcome::from_exit_status(std::process::ExitStatus::from_raw(status)))
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    clippy::zombie_processes,
    reason = "[io-door:test] test process scaffolding; the reaper itself reaps these children by raw pid, not through `Child`"
)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    fn spawn_sleep(secs: &str) -> std::process::Child {
        std::process::Command::new("sleep")
            .arg(secs)
            .spawn()
            .expect("spawn sleep")
    }

    fn recv_timeout<E>(rx: &std::sync::mpsc::Receiver<E>) -> E {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("event within timeout")
    }

    /// An exit is delivered through the reaper, without the subscriber
    /// itself ever calling `waitpid`.
    #[test]
    fn exit_is_delivered() {
        let mut child = spawn_sleep("0");
        let pid = child.id();
        let (tx, rx) = channel();
        let watch = Reaper::global().watch(pid, tx, std::convert::identity);

        match recv_timeout(&rx) {
            WaitPoll::Done(outcome) => assert!(outcome.is_success(), "sleep 0 exits cleanly"),
            other @ WaitPoll::Stopped(_) => panic!("expected an exit, got {other:?}"),
        }
        watch.reap().expect("reap");
        let _ = child.kill();
    }

    /// A stop is delivered, and `signal(CONT)` resumes the child: the sleep
    /// keeps running afterwards, rather than staying parked.
    #[test]
    fn stop_then_cont_resumes() {
        let mut child = spawn_sleep("5");
        let pid = child.id();
        let (tx, rx) = channel();
        let watch = Reaper::global().watch(pid, tx, std::convert::identity);

        watch.signal(Signal::new(libc::SIGSTOP)).expect("SIGSTOP");
        match recv_timeout(&rx) {
            WaitPoll::Stopped(sig) => assert_eq!(sig.number(), libc::SIGSTOP),
            other @ WaitPoll::Done(_) => panic!("expected a stop, got {other:?}"),
        }

        watch.signal(Signal::new(libc::SIGCONT)).expect("SIGCONT");
        // No further event: SIGCONT is an answer, not an exit. Kill to end
        // the test and confirm the child was actually running, not parked.
        watch.signal(Signal::new(libc::SIGKILL)).expect("SIGKILL");
        match recv_timeout(&rx) {
            WaitPoll::Done(outcome) => assert!(!outcome.is_success()),
            other @ WaitPoll::Stopped(_) => panic!("expected the kill's exit, got {other:?}"),
        }
        watch.reap().expect("reap");
        let _ = child.kill();
    }

    /// Exit-then-`reap` pins the pid: `kill(pid, 0)` still succeeds between
    /// `Done` and `reap`, proving the zombie is held rather than reaped by
    /// the scanner itself.
    #[test]
    fn exit_then_reap_pins_the_pid() {
        let mut child = spawn_sleep("0");
        let pid = child.id();
        let (tx, rx) = channel();
        let watch = Reaper::global().watch(pid, tx, std::convert::identity);

        match recv_timeout(&rx) {
            WaitPoll::Done(outcome) => assert!(outcome.is_success()),
            other @ WaitPoll::Stopped(_) => panic!("expected an exit, got {other:?}"),
        }
        assert_eq!(
            unsafe { libc::kill(pid as libc::pid_t, 0) },
            0,
            "the zombie keeps the pid valid until reap"
        );
        watch.reap().expect("reap");
        let _ = child.kill();
    }

    /// Two watches on two children interleave correctly: each receives only
    /// its own exit.
    #[test]
    fn two_watches_interleave() {
        let mut a = spawn_sleep("0");
        let mut b = spawn_sleep("0");
        let pid_a = a.id();
        let pid_b = b.id();
        let (tx_a, rx_a) = channel();
        let (tx_b, rx_b) = channel();
        let watch_a = Reaper::global().watch(pid_a, tx_a, std::convert::identity);
        let watch_b = Reaper::global().watch(pid_b, tx_b, std::convert::identity);

        match recv_timeout(&rx_a) {
            WaitPoll::Done(outcome) => assert!(outcome.is_success()),
            other @ WaitPoll::Stopped(_) => panic!("expected a's exit, got {other:?}"),
        }
        match recv_timeout(&rx_b) {
            WaitPoll::Done(outcome) => assert!(outcome.is_success()),
            other @ WaitPoll::Stopped(_) => panic!("expected b's exit, got {other:?}"),
        }
        watch_a.reap().expect("reap a");
        watch_b.reap().expect("reap b");
        let _ = a.kill();
        let _ = b.kill();
    }
}
