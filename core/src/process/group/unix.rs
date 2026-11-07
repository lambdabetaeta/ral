//! Unix process-group placement: the `pre_exec` discipline every external
//! child is spawned under.

use rustix::process::Pid;

use super::{Landing, Pgid, PgidPolicy};
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

/// Why a spawn made no process, told by the mark its child makes as the
/// last step before `execve`.
#[derive(Debug)]
pub(crate) enum SpawnError {
    /// One of ral's steps failed: the fork, the placement, a `pre_exec` hook.
    Prepare(std::io::Error),
    /// The `execve` failed: the program's own.
    Exec(std::io::Error),
}

/// Spawn `cmd` under the one canonical pre-exec discipline.
///
/// Apply `pgid` then [`reset_child_signals`] in the child, and mirror the
/// `setpgid` in the parent so the placement holds whichever side wins the
/// race.  `NewSession` has no mirror — only a process can `setsid` itself —
/// and rests on its `pre_exec`.  The [`Landing`] returned is the whole
/// registration: what addresses the child from now on.
///
/// `pre_exec` closures run in registration order, so a caller's own hooks
/// (rlimits, the guest jail, `2>&1`) run before this one, whose last step
/// marks the child cleared for `execve`.
///
/// # Errors
/// [`SpawnError::Exec`] if the `execve` fails; [`SpawnError::Prepare`] if
/// anything before it does: the fork, the placement, or any hook.
pub(crate) fn spawn_with_pgid(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
) -> Result<(std::process::Child, Landing), SpawnError> {
    spawn_with_pgid_after(cmd, pgid, || Ok(()))
}

/// [`spawn_with_pgid`] plus one caller hook, run in the child after the
/// signal reset and before the mark.
///
/// The hook must therefore keep to async-signal-safe work such as `read`,
/// `close`, or `dup2` on already-open fds.
///
/// # Errors
/// As [`spawn_with_pgid`]; `after` failing is [`SpawnError::Prepare`].
fn spawn_with_pgid_after<F>(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
    after: F,
) -> Result<(std::process::Child, Landing), SpawnError>
where
    F: Fn() -> std::io::Result<()> + Send + Sync + 'static,
{
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let (cleared, mark) = crate::process::cloexec_socketpair().map_err(SpawnError::Prepare)?;
    // SAFETY: async-signal-safe throughout; the closure owns `mark`, so the
    // fd it writes is open for as long as `cmd` lives.
    unsafe {
        cmd.pre_exec(move || {
            pgid.apply()?;
            reset_child_signals();
            after()?;
            let byte = 0u8;
            if libc::write(mark.as_raw_fd(), (&raw const byte).cast(), 1) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = crate::process::spawn(cmd).map_err(|e| attributed(e, &cleared))?;
    // The mirror's result is ignored: either the child already applied the
    // policy, and a failure here is the benign post-`execve` `EACCES` race, or
    // its `pre_exec` failed and the spawn above already returned that error.
    let landing = match pgid {
        PgidPolicy::Inherit => Landing::Shares,
        PgidPolicy::NewLeader => {
            let pid = Pid::from_child(&child);
            let _ = rustix::process::setpgid(Some(pid), Some(pid));
            Landing::Owns(Pgid::from_pid(pid))
        }
        PgidPolicy::NewSession => {
            // The new session's pgid equals the child's pid.
            Landing::Owns(Pgid::from_pid(Pid::from_child(&child)))
        }
        PgidPolicy::Join(group) => {
            let _ = rustix::process::setpgid(Some(Pid::from_child(&child)), Some(group.as_pid()));
            Landing::Joins
        }
    };
    Ok((child, landing))
}

/// `e`, the `execve`'s if the child marked itself cleared before failing.
/// No wait: the mark precedes the errno `std` already read.
fn attributed(e: std::io::Error, cleared: &std::os::unix::net::UnixStream) -> SpawnError {
    match rustix::net::recv(cleared, &mut [0u8], rustix::net::RecvFlags::DONTWAIT) {
        Ok((1, _)) => SpawnError::Exec(e),
        _ => SpawnError::Prepare(e),
    }
}

/// A detached survivor, which has certainly exec'd: named by its pid, or not.
#[derive(Debug)]
pub enum Survivor {
    Named(u32),
    /// Running, but its pid never came back.
    Unnamed(std::io::Error),
}

/// Spawn `cmd` so that the surviving process is this process's
/// *grandchild*, and return it.
///
/// The hook of [`spawn_with_pgid_after`] forks again; the intermediate writes
/// the grandchild's pid down a pipe and `_exit`s inside the hook, short of the
/// mark, which only the grandchild makes.  It is reaped below — already
/// dead, so that wait cannot block and no zombie exists — leaving the
/// grandchild it orphans to be reparented onto init.
///
/// **The intermediate's [`Landing`] is dropped on the floor, and that
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
/// Nothing was born: [`SpawnError::Exec`] if the grandchild's `execve`
/// fails, [`SpawnError::Prepare`] if either fork, the `setsid`, or any step
/// before them does.  Every later failure is [`Survivor::Unnamed`]: the
/// survivor runs.
pub fn spawn_detached(cmd: &mut std::process::Command) -> Result<Survivor, SpawnError> {
    use std::os::fd::AsRawFd;

    let (receipt, handshake) = crate::process::cloexec_pipe().map_err(SpawnError::Prepare)?;
    let fd = handshake.as_raw_fd();
    let (intermediate, _its_landing) =
        spawn_with_pgid_after(cmd, PgidPolicy::NewSession, move || {
            // SAFETY: a spawn's hook, async-signal-safe throughout: fork,
            // write, _exit, setsid, fcntl.
            if let crate::process::Forked::Parent(pid) = unsafe { crate::process::fork() }? {
                let bytes = pid.as_raw_pid().to_ne_bytes();
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
    Ok(match survivor_pid(intermediate, receipt) {
        Ok(pid) => Survivor::Named(pid),
        Err(why) => Survivor::Unnamed(why),
    })
}

/// Reap the intermediate and read the grandchild's pid off the handshake.
fn survivor_pid(
    intermediate: std::process::Child,
    mut receipt: impl std::io::Read,
) -> std::io::Result<u32> {
    let (tx, rx) = std::sync::mpsc::channel();
    let watch =
        crate::process::ChildHandle::from_std(intermediate).into_watch(tx, std::convert::identity);
    let born = rx
        .recv()
        .map_err(|_| std::io::Error::other("the reaper never reported the intermediate's exit"))?;
    watch.reap()?;
    if born != crate::process::WaitOutcome::Exited(0) {
        return Err(std::io::Error::other(format!(
            "the intermediate process ended as {born:?} instead of exiting 0"
        )));
    }
    let mut pid = [0u8; size_of::<libc::pid_t>()];
    receipt
        .read_exact(&mut pid)
        .map_err(|err| std::io::Error::other(format!("the pid never came back ({err})")))?;
    u32::try_from(libc::pid_t::from_ne_bytes(pid))
        .map_err(|_| std::io::Error::other("the pid handed back is not a process id"))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::{Landing, Pgid, PgidPolicy, SpawnError, Survivor, spawn_detached, spawn_with_pgid};

    // ── Whose failure ──────────────────────────────────────────────────────

    /// A step of ral's failing before the mark is ral's: no pid reaches
    /// `i32::MAX`, so no group answers to it.
    #[test]
    fn a_join_into_a_group_nobody_leads_is_ral_s_failure() {
        let nobody = Pgid::from_raw(i32::MAX).expect("positive");
        let err = spawn_with_pgid(
            &mut std::process::Command::new("/bin/sh"),
            PgidPolicy::Join(nobody),
        )
        .expect_err("a group nobody leads cannot be joined");
        assert!(
            matches!(&err, SpawnError::Prepare(e) if e.raw_os_error() == Some(libc::EPERM)),
            "{err:?}"
        );
    }

    /// Past the mark, only the `execve` is left to fail: the program's own.
    #[test]
    fn a_missing_program_is_the_program_s_failure() {
        let err = spawn_with_pgid(
            &mut std::process::Command::new("ral-no-such-program-exists"),
            PgidPolicy::NewLeader,
        )
        .expect_err("a program that does not exist cannot be born");
        assert!(
            matches!(&err, SpawnError::Exec(e) if e.kind() == std::io::ErrorKind::NotFound),
            "{err:?}"
        );
    }

    /// Where a child lands is decided by its spawn: a group it leads, one it
    /// joined, or ral's own.
    #[test]
    fn a_spawn_says_where_its_child_landed() {
        let sleeper = || {
            let mut cmd = std::process::Command::new("sleep");
            cmd.arg("30");
            cmd
        };
        let landed = |policy| {
            let (child, landing) = spawn_with_pgid(&mut sleeper(), policy).expect("spawn");
            (child.id(), landing, child)
        };
        let (leader, landing, mut lead) = landed(PgidPolicy::NewLeader);
        let group = Pgid::from_raw(leader.cast_signed()).expect("a pid is positive");
        assert_eq!(landing, Landing::Owns(group));
        let (_, landing, mut member) = landed(PgidPolicy::Join(group));
        assert_eq!(landing, Landing::Joins);
        let (_, landing, mut shared) = landed(PgidPolicy::Inherit);
        assert_eq!(landing, Landing::Shares);
        for child in [&mut lead, &mut member, &mut shared] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

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
        let Survivor::Named(pid) = spawn_detached(&mut cmd).expect("detached birth") else {
            panic!("pid lost")
        };
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
        let Survivor::Named(pid) = spawn_detached(&mut cmd).expect("detached birth") else {
            panic!("pid lost")
        };
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
    /// fork, so `Ok` really does mean the grandchild exec'd.  It is the
    /// program's failure: the grandchild made the mark, as the intermediate
    /// never does.
    #[test]
    fn detached_missing_program_reports_not_found() {
        let err = spawn_detached(&mut detachable("ral-no-such-program-exists"))
            .expect_err("a program that does not exist cannot be born");
        assert!(
            matches!(&err, SpawnError::Exec(e) if e.kind() == std::io::ErrorKind::NotFound),
            "{err:?}"
        );
    }
}
