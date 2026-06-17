//! Unix backend for the pipeline gate / report protocol.
//!
//! Channels are Unix-domain socketpairs.  Inheritance into a child
//! `Command` is by raw fd: the fd number is passed in an env var and a
//! `pre_exec` hook on the child clears `FD_CLOEXEC` so the fd survives
//! `execve`; the helper / trampoline consumes the env var, wraps the fd
//! inside `from_raw_fd`, and re-applies CLOEXEC.  All of this is wrapped
//! behind the [`pair`] / [`pass`] / [`reader`] backend functions so the
//! rest of the protocol module never reaches into `AsRawFd` directly.
//!
//! Clearing `FD_CLOEXEC` in the *child's* `pre_exec` rather than on the
//! parent's fd is deliberate: the parent's copy keeps `FD_CLOEXEC` set
//! throughout, so a `Command::spawn` racing on another thread inherits
//! the channel fd at `fork` (fork copies the whole fd table) but closes
//! it at its own `execve`.  Only the helper this `pass` targets — whose
//! `pre_exec` clears the bit — keeps the fd past exec.  Clearing the bit
//! on the parent's fd instead would open a window from that `fcntl` until
//! the parent dropped its copy in which any unrelated concurrent spawn
//! would leak the channel into a foreign child, hanging the reader on a
//! never-arriving EOF.

use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

use super::super::helper::{JOB_FD_ENV, REPORT_FD_ENV, VALUE_IN_FD_ENV, VALUE_OUT_FD_ENV};
use super::common::{EnvNames, FrameReader, pipe_error};
use crate::types::{Break, Settled};

/// Unix-side channel type: a Unix-domain socketpair end.  Reads and
/// writes are blocking; `serde_json` length-prefixed frames flow over
/// it.  Both halves are owned `UnixStream`s so dropping closes the fd
/// deterministically.
pub(crate) type Channel = std::os::unix::net::UnixStream;

/// Allocate one socketpair.  Module-public because the Unix layer of
/// the pipeline (for value-edge transport between adjacent ral
/// helpers) wants the same socketpair primitive without going through
/// a [`FrameGate`].
pub(crate) fn pair() -> Result<(Channel, Channel), Break> {
    Channel::pair().map_err(pipe_error)
}

/// Stash the child-end fd number in `env` on `cmd` and register a
/// child-side `pre_exec` that clears `FD_CLOEXEC` on that fd so it
/// survives `execve`.  Composes the two operations the launcher used to
/// do inline at every gate-wire site.
///
/// Registering the CLOEXEC clear as a `pre_exec` hook — rather than an
/// `fcntl` on the parent's fd — confines the inherit window to this one
/// child (see the module comment).
pub(crate) fn pass(cmd: &mut Command, env: &str, ch: &Channel) -> Settled<()> {
    let fd = ch.as_raw_fd();
    cmd.env(env, fd.to_string());
    // SAFETY: the closure runs post-`fork`, pre-`execve`, and performs
    // only an async-signal-safe `fcntl` on an already-open inherited fd
    // — the same discipline `spawn_with_pgid_after` documents for its
    // own `pre_exec` hook.
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// Spawn a [`FrameReader`] reading from the given channel.  Wrapping
/// the spawn here keeps the reader-thread plumbing inside the
/// platform module instead of in `common.rs`.
pub(crate) fn reader<T>(ch: Channel, panic_msg: &'static str) -> FrameReader<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    FrameReader::spawn(ch, panic_msg)
}

/// Env-var names this backend uses to pass channel fds to the helper.
pub(crate) const ENV: EnvNames = EnvNames {
    job: JOB_FD_ENV,
    report: REPORT_FD_ENV,
    value_in: VALUE_IN_FD_ENV,
    value_out: VALUE_OUT_FD_ENV,
};
