//! Exarch's per-exchange cancellation.
//!
//! One sticky [`Token`] per agent, and the [`InterruptTarget`] its seat
//! republishes so a cancel reaches the eval in flight.  The process's signals,
//! and the trunk that hears them, are [`crate::signals`]'.
//!
//! The attend loop [`reset`](Token::reset)s the token at each genuine
//! exchange boundary, so a prior exchange's Esc never bleeds into the next.
//! The cause a token carries decides its reach: an `Interrupt` drops only the
//! in-flight exchange and the agent re-parks, anything stronger ends the
//! agent.

use ral_core::process::CancelCause;
use ral_core::sync::LockExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// An agent's cancellation handle.
///
/// Clones share one flag, so a cascade and the attend loop hold the same
/// token: cancelling either halts the agent's exchange.  The flag is the
/// [`CancelCause`]'s own discriminant — explicitly numbered in severity order,
/// with 0 meaning uncancelled — so a later, stronger cause escalates by a
/// plain `fetch_max`.
#[derive(Clone, Default)]
pub struct Token(Arc<AtomicU8>);

impl Token {
    /// A fresh, un-cancelled token.  Each agent owns one for its life.
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }

    /// True once cancelled for *any* cause — what `deliberate` and the provider
    /// poll to unwind the exchange in flight.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed) != 0
    }

    /// True when a terminate-class cause is in force — anything but
    /// [`Interrupt`](CancelCause).  A non-`Held` park ends the agent on this.
    pub fn terminated(&self) -> bool {
        let flag = self.0.load(Ordering::Relaxed);
        flag != 0 && flag != CancelCause::Interrupted as u8
    }

    /// Cancel this token and every share of it, recording `cause`.  Monotone,
    /// like `CancelScope::cancel`: a weaker cause arriving later (an Esc
    /// `Interrupt` after an `` exarch-agents `cancel `` `Explicit`) can never mask a
    /// stronger one already in force.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.fetch_max(cause as u8, Ordering::Relaxed);
    }

    /// Clear a bare interrupt, leaving any terminate-class cause in force: an
    /// exchange boundary must never erase a cascade cancellation that landed
    /// between the attend loop's pop and this reset.
    pub fn reset(&self) {
        let _ = self.0.compare_exchange(
            CancelCause::Interrupted as u8,
            0,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

/// Where an agent's interrupt and terminate land: its seat's current control
/// sender, republished at every rebuild, so the cell outlives any one engine.
#[derive(Clone)]
pub(crate) struct InterruptTarget(Arc<std::sync::Mutex<ral_core::carrier::ControlSender>>);

impl InterruptTarget {
    pub(crate) fn new(control: ral_core::carrier::ControlSender) -> Self {
        Self(Arc::new(std::sync::Mutex::new(control)))
    }

    pub(crate) fn republish(&self, control: ral_core::carrier::ControlSender) {
        *self.0.lock_ignore_poison() = control;
    }

    /// Unwind the in-flight run without ending the agent.
    pub(crate) fn interrupt(&self) {
        self.0.lock_ignore_poison().interrupt();
    }

    /// End every run and detached worker of the agent's engine, for good.
    pub(crate) fn terminate(&self) {
        self.0.lock_ignore_poison().terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_is_monotone_and_never_downgrades() {
        let token = Token::new();
        token.cancel(CancelCause::Cancelled);
        token.cancel(CancelCause::Interrupted);
        assert!(
            token.terminated(),
            "a later Interrupt must not downgrade an already-recorded Explicit"
        );
        token.cancel(CancelCause::TimedOut);
        assert_eq!(
            token.0.load(Ordering::Relaxed),
            CancelCause::TimedOut as u8,
            "a stronger later cause still escalates"
        );
    }

    #[test]
    fn reset_clears_only_a_bare_interrupt() {
        let token = Token::new();
        token.cancel(CancelCause::Interrupted);
        token.reset();
        assert!(!token.is_cancelled(), "reset clears a bare interrupt");

        let token = Token::new();
        token.cancel(CancelCause::Cancelled);
        token.reset();
        assert!(
            token.terminated(),
            "reset must never erase a terminate-class cause"
        );
        assert_eq!(
            token.0.load(Ordering::Relaxed),
            CancelCause::Cancelled as u8,
            "the recorded cause survives the reset unchanged"
        );
    }

    #[test]
    fn reset_is_a_no_op_on_an_uncancelled_token() {
        let token = Token::new();
        token.reset();
        assert!(!token.is_cancelled());
    }
}
