//! Unix process-group placement: the `pre_exec` discipline every external
//! child is spawned under.

use rustix::process::Pid;

use super::{Pgid, PgidPolicy};
use crate::process::reset_child_signals;

impl PgidPolicy {
    /// Apply this policy from inside a post-fork `pre_exec` closure: no
    /// allocation and no stdlib lock (`last_os_error` only reads `errno`).  The
    /// failure must not be swallowed — a child left in the wrong group is
    /// invisible to every teardown that addresses the group as a whole.
    /// Reach this through [`spawn_with_pgid`], the single funnel that also
    /// mirrors the call in the parent.
    ///
    /// # Errors
    /// Returns `Err` if `setpgid` / `setsid` fails; `Inherit` makes no syscall.
    pub fn apply(self) -> std::io::Result<()> {
        // `setsid` returns the new sid and `setpgid` 0; both fail with `-1`.
        let rc = unsafe {
            match self {
                Self::Inherit => 0,
                Self::NewLeader => libc::setpgid(0, 0),
                Self::NewSession => libc::setsid(),
                Self::Join(group) => libc::setpgid(0, group.as_raw()),
            }
        };
        if rc == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Spawn `cmd` under the one canonical pre-exec discipline.
///
/// Apply `pgid` then [`reset_child_signals`] in the child, and mirror the
/// `setpgid` in the parent so the placement holds whichever side wins the
/// race.  `NewSession` has no mirror — only a process can `setsid` itself —
/// and rests on its `pre_exec`.  The returned leader pgid (`None` only for
/// `Inherit`) is the whole registration: callers keep it for a later wait
/// or for the pipeline group.
///
/// `pre_exec` closures run in registration order, so a caller's own hook
/// (sandbox `RLIMIT`, `2>&1` dup2) runs *before* this one — deliberately, since
/// the signal reset should be the last thing standing before `execve`.
///
/// # Errors
/// Returns `Err` if the child's `setpgid` / `setsid`, or the `fork` / `exec`
/// itself, fails.
pub fn spawn_with_pgid(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
) -> std::io::Result<(std::process::Child, Option<Pgid>)> {
    spawn_with_pgid_after(cmd, pgid, || Ok(()))
}

/// [`spawn_with_pgid`] plus one caller hook, run in the child after the
/// signal reset and before `execve`.
///
/// The hook must therefore keep to async-signal-safe work such as `read`,
/// `close`, or `dup2` on already-open fds.
///
/// # Errors
/// Returns `Err` if the child's `setpgid` / `setsid` fails, if `after` returns
/// an error, or if the `fork` / `exec` itself fails.
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
    let child = crate::process::spawn(cmd)?;
    // The mirror's result is ignored: either the child already applied the
    // policy, and a failure here is the benign post-`execve` `EACCES` race, or
    // its `pre_exec` failed and the spawn above already returned that error.
    let leader = match pgid {
        PgidPolicy::Inherit => None,
        PgidPolicy::NewLeader => {
            let pid = Pid::from_child(&child);
            let _ = rustix::process::setpgid(Some(pid), Some(pid));
            Some(Pgid::from_pid(pid))
        }
        PgidPolicy::NewSession => {
            // The new session's pgid equals the child's pid.
            Some(Pgid::from_pid(Pid::from_child(&child)))
        }
        PgidPolicy::Join(group) => {
            let _ = rustix::process::setpgid(Some(Pid::from_child(&child)), Some(group.as_pid()));
            Some(group)
        }
    };
    Ok((child, leader))
}

/// Spawn `cmd` so that the surviving process is this process's
/// *grandchild*, and return its pid.
///
/// The hook of [`spawn_with_pgid_after`] forks again; the intermediate writes
/// the grandchild's pid down a pipe and `_exit`s, and is reaped below — already
/// dead, so that wait cannot block and no zombie exists — leaving the
/// grandchild it orphans to be reparented onto init.
///
/// **The intermediate's leader [`Pgid`] is dropped on the floor, and that
/// discard is the point:** nothing in this process holds a pgid naming the
/// survivor, so no teardown path can reach it with `kill(-pgid, …)`, and its
/// own `setsid` leaves pid == pgid == sid so the dead intermediate's recyclable
/// pid cannot name the group either.
///
/// The survivor keeps no descriptor back to us, the handshake fd being re-armed
/// close-on-exec, so its standard streams are the caller's to point somewhere
/// that outlives this process.  `Ok` still proves the *grandchild* exec'd:
/// `std`'s close-on-exec errno pipe is read until every copy shuts, and the
/// grandchild's shuts only on a successful `execve`.
///
/// # Errors
/// Returns `Err` if either fork, the `setsid`, or the `execve` fails, if
/// the intermediate exits non-zero, or if the pid handshake comes up short.
pub fn spawn_detached(cmd: &mut std::process::Command) -> std::io::Result<u32> {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    let (mut receipt, handshake) = crate::process::cloexec_pipe()?;
    let fd = handshake.as_raw_fd();
    let (intermediate, _its_pgid) =
        spawn_with_pgid_after(cmd, PgidPolicy::NewSession, move || {
            // Async-signal-safe throughout: fork, write, _exit, setsid, fcntl.
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if pid > 0 {
                let bytes = pid.to_ne_bytes();
                unsafe {
                    let _ = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
                    libc::_exit(0);
                }
            }
            if unsafe { libc::setsid() } == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // `std` dup2'd the stdio fds before this hook ran and dup2 clears
            // FD_CLOEXEC on its target, so re-arm rather than trust `os_pipe`.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        })?;
    drop(handshake);
    let pid = intermediate.id();
    let (tx, rx) = std::sync::mpsc::channel();
    let watch = crate::process::reaper::watch(pid, tx, std::convert::identity);
    // Dropping a `std::process::Child` neither kills nor reaps: the watch
    // above is what now owns its wait.
    drop(intermediate);
    let born = rx.recv().map_err(|_| {
        std::io::Error::other("could not detach: the reaper never reported the intermediate's exit")
    })?;
    watch.reap()?;
    if born != crate::process::WaitOutcome::Exited(0) {
        return Err(std::io::Error::other(format!(
            "could not detach: the intermediate process ended as {born:?} instead of exiting 0, so nothing here knows the pid of what it started"
        )));
    }
    let mut pid = [0u8; size_of::<libc::pid_t>()];
    receipt.read_exact(&mut pid).map_err(|err| {
        std::io::Error::other(format!(
            "could not detach: the process was started but its pid never came back ({err}); it may be running, with nothing left to name it"
        ))
    })?;
    u32::try_from(libc::pid_t::from_ne_bytes(pid)).map_err(|_| {
        std::io::Error::other("could not detach: the pid handed back is not a process id")
    })
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::spawn_detached;

    // ── Detached birth ─────────────────────────────────────────────────────

    /// A `Command` with all three standard streams pointed away from the
    /// harness — the precondition [`spawn_detached`] states.
    fn detachable(program: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new(program);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd
    }

    /// The survivor belongs to nobody here: it leads its own group, no wait
    /// from this process can name it, and — where `/proc` can say so — its
    /// parent is no longer us.
    #[test]
    fn detached_survivor_is_orphaned_and_leads_its_own_group() {
        let mut cmd = detachable("sleep");
        cmd.arg("30");
        let pid = spawn_detached(&mut cmd).expect("detached birth");
        let raw = i32::try_from(pid).expect("a live pid fits an i32");

        assert_eq!(
            unsafe { libc::getpgid(raw) },
            raw,
            "the survivor's own setsid must leave pid == pgid == sid"
        );

        let reaped = unsafe { libc::waitpid(raw, std::ptr::null_mut(), libc::WNOHANG) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(reaped, -1, "the survivor must not be waitable from here");
        assert_eq!(
            errno,
            Some(libc::ECHILD),
            "the survivor is a grandchild: no wait here can name it, and none can leak it as a zombie"
        );

        #[cfg(target_os = "linux")]
        {
            // /proc/<pid>/stat is `pid (comm) state ppid …` and `comm` may
            // itself hold spaces and parens, so read fields past the last ')'.
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("survivor stat");
            let tail = &stat[stat.rfind(')').expect("stat has a comm field") + 1..];
            let ppid: i32 = tail
                .split_whitespace()
                .nth(1)
                .expect("stat has a ppid field")
                .parse()
                .expect("ppid is a number");
            assert_ne!(
                ppid,
                i32::try_from(std::process::id()).expect("our own pid fits an i32"),
                "the intermediate's exit must have reparented the survivor away from us"
            );
        }

        unsafe { libc::kill(raw, libc::SIGKILL) };
    }

    /// The intermediate is reaped inside the birth, leaving the caller no child
    /// to wait for and no zombie to accumulate.
    #[test]
    fn detached_birth_leaves_nothing_to_reap() {
        let mut cmd = detachable("sleep");
        cmd.arg("30");
        let pid = spawn_detached(&mut cmd).expect("detached birth");
        let raw = i32::try_from(pid).expect("a live pid fits an i32");

        assert_eq!(
            unsafe { libc::kill(raw, 0) },
            0,
            "the pid handed back must be the surviving grandchild, not the exited intermediate"
        );
        let reaped = unsafe { libc::waitpid(raw, std::ptr::null_mut(), libc::WNOHANG) };
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(reaped, -1);
        assert_eq!(errno, Some(libc::ECHILD));

        unsafe { libc::kill(raw, libc::SIGKILL) };
    }

    /// Redirected stdio reaches the file.  There is nothing to wait on —
    /// that is the point of the verb — so the assertion polls.
    #[test]
    fn detached_stdout_lands_in_its_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("out");
        let mut cmd = detachable("/bin/echo");
        cmd.arg("hello")
            .stdout(std::fs::File::create(&out).expect("create the stdout file"));
        spawn_detached(&mut cmd).expect("detached birth");

        for _ in 0..300 {
            if std::fs::read_to_string(&out).is_ok_and(|text| text == "hello\n") {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the survivor's stdout never reached {}", out.display());
    }

    /// A program that does not exist comes back `NotFound`, not as a pid —
    /// the proof that `std`'s close-on-exec errno pipe survives the second
    /// fork, so `Ok` really does mean the grandchild exec'd.
    #[test]
    fn detached_missing_program_reports_not_found() {
        let err = spawn_detached(&mut detachable("ral-no-such-program-exists"))
            .expect_err("a program that does not exist cannot be born");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
