//! Serializes platform-specific descriptor and handle inheritance windows.
//!
//! Apple has neither `pipe2` nor `SOCK_CLOEXEC`: `os_pipe::pipe()` and std's
//! `UnixStream::pair()` create the fd and only *afterwards* `fcntl(FD_CLOEXEC)`
//! it, so a `fork` on another thread in that window inherits the fd without
//! `CLOEXEC`, and the exec'd grandchild keeps a pipe or socket end open
//! forever.
//!
//! A process-wide `RwLock` closes the window: every `fork(2)` takes the
//! exclusive side, every non-atomic fd creation takes the shared side.
//!
//! Windows uses one mutex for raw launches' temporary inheritable handles
//! and helper-backed `Command::spawn`: an explicit handle list constrains
//! only the raw child, not an unrelated inheriting child. Dependency spawns
//! outside these entries do not participate. Linux needs neither lock.
//!
//! Never hold the exclusive side across anything but the `spawn()` call
//! itself — it is process-wide and every stage thread may want the shared
//! side at once.
//!
//! Both locks protect admission, not child lifetimes. Poison carries no
//! information, since there is no state to tear; use the workspace policy.

use crate::process::site::Silent;
use crate::site::site;
#[cfg(windows)]
use crate::sync::LockExt as _;
#[cfg(target_vendor = "apple")]
use crate::sync::RwLockExt as _;

#[cfg(target_vendor = "apple")]
static LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

#[cfg(windows)]
static INHERITANCE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold through process creation and restoration of temporary inherit flags.
#[cfg(windows)]
pub(crate) fn lock_inheritance() -> std::sync::MutexGuard<'static, ()> {
    INHERITANCE_LOCK.lock_ignore_poison()
}

/// A pipe whose reader and writer are never open across an unsynchronised
/// fork on another thread.
///
/// # Errors
/// Returns the pipe's own creation error.
pub fn cloexec_pipe() -> std::io::Result<(os_pipe::PipeReader, os_pipe::PipeWriter)> {
    #[cfg(target_vendor = "apple")]
    let _guard = LOCK.read_ignore_poison();
    site!(Silent::CloexecPipe, os_pipe::pipe())
}

/// A socketpair whose two ends are never open across an unsynchronised fork
/// on another thread.
///
/// # Errors
/// Returns the socketpair's own creation error.
#[cfg(unix)]
pub fn cloexec_socketpair() -> std::io::Result<(
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
)> {
    #[cfg(target_vendor = "apple")]
    let _guard = LOCK.read_ignore_poison();
    site!(
        Silent::CloexecSocketpair,
        std::os::unix::net::UnixStream::pair()
    )
}

/// The standard spawn door: locks cover `Command::spawn` and nothing else.
///
/// `.output()`/`.status()` are fork *plus wait-for-exit*, so a caller needing
/// either must call this and wait outside it — see [`output`] and [`status`].
///
/// # Errors
/// Returns `Command::spawn`'s own error.
pub fn spawn(cmd: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    #[cfg(target_vendor = "apple")]
    let _guard = LOCK.write_ignore_poison();
    #[cfg(all(test, windows))]
    test_phase::pause();
    #[cfg(windows)]
    let _guard = lock_inheritance();
    site!(Silent::ForkSpawn, cmd => spawn())
}

/// `Command::output`, with the wait for exit outside the fork lock: stdout
/// and stderr captured, stdin closed, as `output` itself wires them.
///
/// # Errors
/// Returns `spawn`'s error, or the child's own wait error.
pub fn output(cmd: &mut std::process::Command) -> std::io::Result<std::process::Output> {
    use std::process::Stdio;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(cmd)?.wait_with_output()
}

/// `Command::status`, with the wait for exit outside the fork lock.
///
/// # Errors
/// Returns `spawn`'s error, or the child's own wait error.
pub fn status(cmd: &mut std::process::Command) -> std::io::Result<std::process::ExitStatus> {
    let mut child = spawn(cmd)?;
    site!(Silent::StatusWait, &mut child => wait())
}

/// A thread-local, one-shot barrier at the two real inheritance entries.
#[cfg(all(test, windows))]
pub(crate) mod test_phase {
    use std::cell::RefCell;
    use std::sync::mpsc::{Receiver, Sender};

    thread_local! {
        static PHASE: RefCell<Option<(Sender<()>, Receiver<()>)>> = const { RefCell::new(None) };
    }

    pub(crate) fn install(reached: Sender<()>, release: Receiver<()>) {
        PHASE.with(|phase| *phase.borrow_mut() = Some((reached, release)));
    }

    pub(crate) fn pause() {
        if let Some((reached, release)) = PHASE.with(|phase| phase.borrow_mut().take()) {
            reached.send(()).expect("phase observer alive");
            release
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("release inheritance phase");
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use rustix::io::{FdFlags, fcntl_getfd};
    use std::process::{Command, Stdio};

    fn cloexec(fd: impl std::os::fd::AsFd) -> bool {
        fcntl_getfd(fd).expect("fcntl").contains(FdFlags::CLOEXEC)
    }

    #[test]
    fn pipe_ends_are_cloexec() {
        let (r, w) = cloexec_pipe().expect("pipe");
        assert!(cloexec(&r) && cloexec(&w));
    }

    #[test]
    fn socketpair_ends_are_cloexec() {
        let (a, b) = cloexec_socketpair().expect("socketpair");
        assert!(cloexec(&a) && cloexec(&b));
    }

    /// Streams preset to something else are overridden; `-ef` tells fd 0 is
    /// `/dev/null`, not the piped stdin it was given.
    #[test]
    fn output_nulls_stdin_and_captures_both_streams() {
        let out = output(
            Command::new("sh")
                .args([
                    "-c",
                    "[ /dev/stdin -ef /dev/null ] && echo out; echo err >&2",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit()),
        )
        .expect("output");
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    }
}
