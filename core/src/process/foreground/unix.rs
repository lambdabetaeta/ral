//! Unix foreground ownership: lending the controlling terminal to a child's
//! group and taking it back.

use nix::sys::signal::{SigSet, SigmaskHow};
use rustix::io::Errno;
use rustix::process::Pid;
use rustix::termios::{OptionalActions, Termios};

use crate::process::Signal;
use crate::process::cancel::{CancelCause, ForegroundScope};
use crate::process::signal::gesture;

/// Capture stdin's line-discipline state; `None` when stdin is not a tty.  The
/// restore is the caller's, the right `tcsetattr` flush mode being site-specific.
pub fn termios_snapshot() -> Option<Termios> {
    rustix::termios::tcgetattr(rustix::stdio::stdin()).ok()
}

// ── Foreground ownership ───────────────────────────────────────────────────
//
// A foreground child leaves two pieces of tty state behind.  Miss the process
// group and ral sits in a background pgroup whose next read returns EIO; miss
// the termios and we inherit whatever raw/cooked/ONLCR mode the child last
// wrote — anything calling `cfmakeraw` — for the classic Unix staircase output.

/// One lending of the session's [`TerminalLease`] to a group, for a run: it
/// snapshots both on acquire and restores both on drop.
///
/// It cannot be constructed without borrowing a [`TerminalLease`]: holding one
/// is the proof that ral owns the controlling terminal's foreground, and only a
/// run whose terminal policy grants the handoff can obtain the borrow
/// (`Shell::terminal_lease`).
///
/// While lent, a key pressed on the terminal reaches the tenant, never ral;
/// the loan hears it back from the tenant and, on return, strikes it on the
/// run's frame.
///
/// [`TerminalLease`]: crate::process::TerminalLease
pub struct TerminalLoan {
    saved_pgid: Pid,
    saved_termios: Option<Termios>,
    /// The run the terminal was lent for; struck with `pressed` on return.
    frame: ForegroundScope,
    pressed: Option<CancelCause>,
}

impl TerminalLoan {
    /// Hand the controlling tty to `target` for the run under `frame`,
    /// recording the prior pgid and termios for the restore.  `None` when the
    /// pgid handoff itself fails, so there is then nothing to restore; a
    /// failed termios snapshot is not fatal and leaves only the pgid half to
    /// put back on drop.
    pub(crate) fn try_acquire(
        target: i32,
        _lease: &crate::process::TerminalLease,
        frame: &ForegroundScope,
    ) -> Option<Self> {
        if target <= 0 {
            return None;
        }
        let target = Pid::from_raw(target)?;
        let saved = rustix::process::getpgrp();
        #[cfg_attr(
            not(debug_assertions),
            allow(
                unused_variables,
                reason = "`dbg_trace!` discards its arguments in release builds"
            )
        )]
        if let Err(err) = rustix::termios::tcsetpgrp(rustix::stdio::stdin(), target) {
            crate::dbg_trace!(
                "fg",
                "acquire: tcsetpgrp({}) failed: {err}",
                target.as_raw_nonzero()
            );
            return None;
        }
        let saved_termios = termios_snapshot();
        if saved_termios.is_none() {
            crate::dbg_trace!("fg", "acquire: tcgetattr failed");
        }
        Some(Self {
            saved_pgid: saved,
            saved_termios,
            frame: frame.clone(),
            pressed: None,
        })
    }

    /// A loan with no handoff, so the strike can be tested alone: restoring
    /// the foreground as found is a no-op.
    #[cfg(test)]
    pub(crate) fn for_test(frame: &ForegroundScope) -> Self {
        Self {
            saved_pgid: rustix::termios::tcgetpgrp(rustix::stdio::stdin())
                .unwrap_or_else(|_| rustix::process::getpgrp()),
            saved_termios: None,
            frame: frame.clone(),
            pressed: None,
        }
    }

    /// Read `signal`, reported by the tenant, as the key it hands back.
    pub(crate) fn hear(&mut self, signal: Signal) -> Option<CancelCause> {
        let key = gesture(signal);
        self.pressed = self.pressed.max(key);
        key
    }

    /// The strongest key heard so far.
    pub(crate) fn pressed(&self) -> Option<CancelCause> {
        self.pressed
    }

    /// Take the terminal back from a tenant whose death was by `last`.
    pub(crate) fn reclaim(mut self, last: Option<Signal>) -> Option<CancelCause> {
        if let Some(signal) = last {
            self.hear(signal);
        }
        self.pressed
    }
}

/// Thread-local guard blocking SIGTTOU while ral takes tty state back.
///
/// POSIX counts `tcsetpgrp` / `tcsetattr` from a background process group as
/// terminal output, and at the default disposition the kernel stops the caller
/// mid-restore — exactly ral's position, since a foreground job puts it in the
/// background by construction.  Parent-local: children have SIGTTOU reset by
/// [`reset_child_signals`](crate::process::reset_child_signals).
struct SigttouBlock {
    /// `None` when the block never took, leaving nothing to restore.
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

impl Drop for TerminalLoan {
    /// Restore the terminal, then strike what was heard on the run it was
    /// lent for.
    fn drop(&mut self) {
        self.restore();
        if let Some(cause) = self.pressed {
            self.frame.cancel(cause);
        }
    }
}

impl TerminalLoan {
    /// Restore the foreground pgid and termios recorded at acquisition.
    ///
    /// Pgid first, since a missed restore leaves ral in a background pgroup
    /// whose next tty read returns EIO.  Termios second, with `Drain` so the
    /// child's last buffered output leaves under the child's own settings:
    /// `Now` would clobber those bytes' line discipline, and `Flush` would
    /// discard input typed during the child's final frame.
    fn restore(&self) {
        let _sigttou = SigttouBlock::new();
        for _ in 0..3 {
            match rustix::termios::tcsetpgrp(rustix::stdio::stdin(), self.saved_pgid) {
                Ok(()) => break,
                Err(err) => {
                    if err != Errno::INTR {
                        crate::dbg_trace!(
                            "fg",
                            "release: tcsetpgrp({}) failed: {err}",
                            self.saved_pgid.as_raw_nonzero()
                        );
                        break;
                    }
                }
            }
        }
        let cur = rustix::termios::tcgetpgrp(rustix::stdio::stdin())
            .map_or(-1, |fg| fg.as_raw_nonzero().get());
        if cur != self.saved_pgid.as_raw_nonzero().get() {
            crate::dbg_trace!(
                "fg",
                "release: tty fg is {cur}, want {} (next tty read may EIO)",
                self.saved_pgid.as_raw_nonzero()
            );
        }
        if let Some(t) = self.saved_termios.as_ref() {
            for _ in 0..3 {
                match rustix::termios::tcsetattr(rustix::stdio::stdin(), OptionalActions::Drain, t)
                {
                    Ok(()) => return,
                    Err(err) => {
                        if err != Errno::INTR {
                            crate::dbg_trace!("fg", "release: tcsetattr failed: {err}");
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Deliver the SIGINT that raw mode swallowed, but only to a foreground job
/// that is an *external* child.
///
/// A frontend in raw mode clears `ISIG`, so a keypress no longer makes the
/// kernel SIGINT whichever group owns the tty.  When an external child holds it
/// this recreates that delivery, so the child dies and the evaluator's blocking
/// `wait` returns; when the shell itself holds it there is nothing external to
/// signal and the frontend's cooperative [`Mooring::check`](crate::types::Mooring::check) unwinds the
/// in-process eval instead.  Signalling *another* group never ticks ral's
/// escalation ladder, so no third-signal force-exit can follow from this.
pub fn interrupt_foreground_child() {
    let Ok(fg) = rustix::termios::tcgetpgrp(rustix::stdio::stdin()) else {
        return;
    };
    if fg != rustix::process::getpgrp() {
        let _ = rustix::process::kill_process_group(fg, rustix::process::Signal::INT);
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

    /// What a loan struck on its frame when it returned, after `heard`.
    fn struck_after(heard: &[i32]) -> Option<CancelCause> {
        let frame = crate::process::DurableRoot::new().worker();
        let mut loan = TerminalLoan::for_test(&frame);
        for &n in heard {
            loan.hear(Signal::new(n));
        }
        drop(loan);
        frame.cause()
    }

    /// A key heard is struck when the terminal returns; a signal no terminal
    /// sends is not; a stronger key outranks a weaker one.
    #[test]
    fn a_returned_loan_strikes_the_key_it_heard() {
        assert_eq!(
            struck_after(&[libc::SIGINT]),
            Some(CancelCause::Interrupted)
        );
        assert_eq!(struck_after(&[libc::SIGTERM]), None);
        assert_eq!(
            struck_after(&[libc::SIGINT, libc::SIGQUIT]),
            Some(CancelCause::Aborted)
        );
    }
}
