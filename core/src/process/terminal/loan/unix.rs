//! Unix terminal state capture, handoff, and restoration.

use std::io;

use nix::sys::signal::{SigSet, SigmaskHow};
use rustix::process::Pid;
use rustix::termios::{OptionalActions, Termios};

use crate::process::Pgid;

#[derive(Debug)]
pub(super) struct Saved {
    foreground: Option<Pid>,
    termios: Option<Termios>,
}

impl Saved {
    pub(super) fn capture() -> Self {
        // A non-controlling tty still has modes worth saving.
        Self {
            foreground: rustix::termios::tcgetpgrp(rustix::stdio::stdin()).ok(),
            termios: rustix::termios::tcgetattr(rustix::stdio::stdin()).ok(),
        }
    }
}

pub(super) fn hand_over(leader: Pgid) -> io::Result<()> {
    rustix::termios::tcsetpgrp(rustix::stdio::stdin(), leader.as_pid())?;
    // A pre-handoff tty access can stop descendants as well as the leader.
    leader.signal_group(crate::process::Signal::new(libc::SIGCONT));
    Ok(())
}

impl Drop for Saved {
    fn drop(&mut self) {
        let _sigttou = SigttouBlock::new();
        // Foreground first; drain under the child's modes without flushing input.
        if let Some(foreground) = self.foreground
            && let Err(err) = rustix::io::retry_on_intr(|| {
                rustix::termios::tcsetpgrp(rustix::stdio::stdin(), foreground)
            })
        {
            crate::dbg_trace!(
                "fg",
                "release: tcsetpgrp({}) failed: {err}",
                foreground.as_raw_nonzero()
            );
        }
        if let Some(termios) = self.termios.as_ref()
            && let Err(err) = rustix::io::retry_on_intr(|| {
                rustix::termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Drain, termios)
            })
        {
            crate::dbg_trace!("fg", "release: tcsetattr failed: {err}");
        }
    }
}

/// Block SIGTTOU while restoring from a background group, then restore the mask.
struct SigttouBlock {
    old: Option<SigSet>,
}

impl SigttouBlock {
    fn new() -> Self {
        let mut block = SigSet::empty();
        block.add(nix::sys::signal::Signal::SIGTTOU);
        Self {
            old: block.thread_swap_mask(SigmaskHow::SIG_BLOCK).ok(),
        }
    }
}

impl Drop for SigttouBlock {
    fn drop(&mut self) {
        if let Some(old) = &self.old {
            let _ = old.thread_set_mask();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sigttou_is_blocked() -> bool {
        SigSet::thread_get_mask()
            .expect("pthread_sigmask query failed")
            .contains(nix::sys::signal::Signal::SIGTTOU)
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
}
