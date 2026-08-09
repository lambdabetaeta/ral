//! Unix backend for the pipeline gate / report protocol.  Channels are
//! socketpairs, inherited by raw fd number passed in an env var, which
//! `helper::UnixTransport` reads back and re-secures with `FD_CLOEXEC`
//! against the stage's own children.
//!
//! The clear of `FD_CLOEXEC` happens in the *child's* `pre_exec` rather
//! than by `fcntl` on the parent's fd, so the parent's copy stays
//! close-on-exec: a `Command::spawn` racing on another thread inherits
//! the fd at `fork` (which copies the whole fd table) but drops it at its
//! own `execve`.  Clearing the bit on the parent's copy would instead
//! leave any such foreign child holding the channel open, hanging the
//! reader on an EOF that never arrives.

use std::os::fd::AsRawFd;

use super::super::helper::{JOB_FD_ENV, REPORT_FD_ENV};
use super::common::{EnvNames, pipe_error};
use crate::types::{Break, Settled};

/// One end of a socketpair, owned, carrying blocking length-prefixed frames.
pub(crate) type Channel = std::os::unix::net::UnixStream;

/// Allocate one socketpair.  The anchor and the frame protocol both want a
/// bare pair with no `FrameGate`.
pub(crate) fn pair() -> Result<(Channel, Channel), Break> {
    Channel::pair().map_err(pipe_error)
}

/// Stash the child-end fd number in `env` on `cmd` and register the
/// child-side `pre_exec` that clears `FD_CLOEXEC` on it.
#[allow(
    clippy::unnecessary_wraps,
    reason = "one of the platform `pass` backends behind `platform::pass`; the fallback variant genuinely returns `Err`, so the `Settled<()>` signature is fixed across the backend family."
)]
pub(crate) fn pass(cmd: &mut crate::process::Launch, env: &str, ch: &Channel) -> Settled<()> {
    let fd = ch.as_raw_fd();
    cmd.env(env, fd.to_string());
    cmd.clear_cloexec_on_spawn(fd);
    Ok(())
}

pub(crate) const ENV: EnvNames = EnvNames {
    job: JOB_FD_ENV,
    report: REPORT_FD_ENV,
};
