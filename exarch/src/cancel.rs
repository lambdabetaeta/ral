//! Exarch's per-root-turn cancellation, layered on top of ral's SIGINT
//! handling.
//!
//! ral's `signal::install_handlers` sets `SIGNAL_COUNT` so the
//! evaluator unwinds between statements; that interrupts an in-flight
//! tool call but leaves exarch's turn loop free to keep going.  Here we
//! add a cancellation [`Token`] minted once per *root* turn
//! ([`crate::session::Session::run_turn`]) and threaded down through
//! `apply` → dispatch → tools → child sessions.  A sub-agent shares the
//! parent's token rather than minting its own, so a single Ctrl-C / Esc
//! cancels the whole call tree.  The token is cancelled by the chained
//! signal handler and raced by the HTTP request future, so one signal
//! stops the turn and returns to the prompt.
//!
//! Ctrl-C and Esc route through [`raise_interrupt`], which cancels
//! the per-turn token and asks ral to cancel the current turn's
//! foreground scope with [`CancelCause::Interrupt`](ral_core::process::CancelCause):
//! the foreground evaluation unwinds at its next poll, while detached
//! workers — parented at the durable root, not the foreground — are
//! spared.  No global counter is ticked on the Esc path, so there is
//! nothing to escalate toward a force-exit.
//!
//! The signal handler cannot hold a token by value, so the root turn
//! publishes its token's flag into a process-global *slot* (an
//! `AtomicPtr`, the lock-free ArcSwap analogue — a signal handler must
//! not lock) for the handler to set.  The slot points into the live
//! token's own `Arc<AtomicBool>`, so a signal-driven cancellation is
//! observed through the threaded [`Token`] every cancel check already
//! holds (`is_set` reads the slot directly, but only in tests).  The slot is
//! published by [`mint_root`]'s RAII guard and cleared on its drop, so a
//! sub-agent turn (which runs `apply` directly, not `run_turn`) never
//! touches it: minting is the *only* reset, replacing X5's clear-at-every-
//! `apply` which erased a just-pressed Esc before a sub-agent saw it.
//!
//! Install order matters: ral's handler must be set first; then `install`
//! replaces the disposition with a handler that sets the current root token
//! *and* forwards to ral's, so statement-level unwinding still works.
//! [`crate::bootstrap::boot_shell`] owns that ceremony for exarch session
//! shells, including `/clear` rebuilds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// A per-root-turn cancellation handle.  Cloning shares the same flag
/// (an `Arc<AtomicBool>`), so a child session handed a clone is cancelled
/// the instant the root token is — the whole tree halts on one Esc.
#[derive(Clone, Default)]
pub struct Token(Arc<AtomicBool>);

impl Token {
    /// A fresh, un-cancelled token that is **not** published to the signal
    /// slot.  This is the handle a background worker (an async `agent`)
    /// receives: its cancellation is its own — `agent_cancel`, `/clear`, or
    /// its worker ceiling — never an Esc, which targets the foreground turn
    /// alone through [`mint_root`]'s published slot.
    pub fn new() -> Self {
        Token(Arc::new(AtomicBool::new(false)))
    }

    /// True once this token (or, since clones share the flag, any of its
    /// shares) has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Cancel this token and every share of it.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The current root turn's flag, published for the signal handler.  Null
/// between turns.  Holds a borrowed pointer into the [`Token`]'s `Arc`,
/// kept alive for the slot's tenure by [`RootGuard`].
static CURRENT: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());

/// Mint a fresh root-turn token and publish it for the signal handler.
/// The returned guard owns the token and clears the slot on drop, so the
/// turn's end (normal, error, or unwind) always retires the published
/// pointer before the `Arc` it borrows can drop.  A brand-new token
/// starts un-cancelled, so minting is itself the reset — there is no
/// separate clear on the root path.
pub fn mint_root() -> RootGuard {
    let token = Token(Arc::new(AtomicBool::new(false)));
    // Borrow the flag for the signal handler.  The `Arc` lives inside the
    // guard for the slot's whole tenure, so the pointer stays valid until
    // the guard's drop nulls the slot.
    let flag: *const AtomicBool = Arc::as_ptr(&token.0);
    CURRENT.store(flag as *mut AtomicBool, Ordering::Release);
    RootGuard { token }
}

/// RAII owner of the current root token: holds the `Arc` alive while its
/// flag pointer is published in [`CURRENT`], and nulls the slot on drop.
pub struct RootGuard {
    token: Token,
}

impl RootGuard {
    /// The token to thread into `apply` and on to child sessions.
    pub fn token(&self) -> &Token {
        &self.token
    }
}

impl Drop for RootGuard {
    fn drop(&mut self) {
        CURRENT.store(std::ptr::null_mut(), Ordering::Release);
    }
}

/// True if the current root turn's published slot reads cancelled.  A test
/// probe for the signal-handler slot: production code observes cancellation
/// through the threaded [`Token`] directly, so the slot's boolean is read
/// only here.  False when no root turn is active.
#[cfg(test)]
pub(crate) fn is_set() -> bool {
    let p = CURRENT.load(Ordering::Acquire);
    // SAFETY: a non-null slot points into the `Arc<AtomicBool>` the live
    // `RootGuard` holds; the guard nulls the slot on drop before the
    // `Arc` can be freed, so a non-null read is always live.
    !p.is_null() && unsafe { (*p).load(Ordering::Relaxed) }
}

/// Set the current root token (signal-handler safe: a single atomic load
/// of the slot plus an atomic store).  A no-op between turns.
fn raise() {
    let p = CURRENT.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: a non-null slot points into the `Arc<AtomicBool>` the live
        // `RootGuard` holds; the guard nulls the slot on drop before the
        // `Arc` can be freed, so a non-null read is always live.
        unsafe { (*p).store(true, Ordering::Relaxed) };
    }
}

pub fn raise_interrupt() {
    raise();
    deliver_interrupt();
}

/// Install the chained signal handler.  Must run *after*
/// `ral_core::process::install_handlers` — we capture ral's handler and
/// forward to it so its `SIGNAL_COUNT` semantics are preserved.
#[cfg(unix)]
pub fn install() {
    let ral = ral_core::process::term_handler();
    RAL_HANDLER.store(ral as *mut (), Ordering::Release);
    unsafe {
        libc::signal(libc::SIGINT, chained as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, chained as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, chained as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
pub fn install() {}

#[cfg(unix)]
type RalHandler = extern "C" fn(libc::c_int);

/// ral's prior SIGINT/SIGTERM/SIGHUP disposition, published as a thin
/// function pointer for the chained handler to forward into.  `install`
/// always stores the same `term_handler` pointer, so this is a set-once
/// slot read only from the signal handler — an `AtomicPtr`, the
/// signal-handler-safe analogue of [`CURRENT`], since a handler must not
/// lock and a `static mut` read is unsound under concurrent install.
#[cfg(unix)]
static RAL_HANDLER: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

#[cfg(unix)]
extern "C" fn chained(sig: libc::c_int) {
    raise();
    forward_into_ral(sig);
}

/// Raw mode disables `ISIG`, so pressing Ctrl-C no longer causes the
/// kernel to deliver SIGINT to the foreground job.  Recreate that
/// missing terminal behaviour, then cancel the current turn's foreground
/// scope so the evaluator unwinds the foreground at its next poll.
///
/// A foreground external child still gets a real SIGINT via its own
/// process group (`interrupt_foreground_child`) — killing *another* group
/// carries no escalation.  For ral itself we cancel the current turn's
/// foreground scope with [`CancelCause::Interrupt`](ral_core::process::CancelCause):
/// the evaluator unwinds the foreground at its next poll, while detached
/// workers — parented at the durable root, not the foreground — are
/// spared, and there is no global counter to escalate toward a
/// third-signal `_exit`.
#[cfg(unix)]
fn deliver_interrupt() {
    ral_core::process::interrupt_foreground_child();
    ral_core::process::request_foreground_cancel(ral_core::process::CancelCause::Interrupt);
}

/// Feed a signal into ral's Unix interrupt handler.
#[cfg(unix)]
fn forward_into_ral(sig: libc::c_int) {
    let p = RAL_HANDLER.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: `install` publishes `term_handler`'s `extern "C" fn`
        // pointer here and nowhere else, so a non-null slot is exactly
        // that pointer cast back to its function-pointer type.
        let h: RalHandler = unsafe { std::mem::transmute(p) };
        h(sig);
    }
}

/// Windows raw mode suppresses the console's automatic Ctrl-C event.
/// Re-inject it into the current console process group so ral's own
/// Windows handler runs and standalone foreground children receive the
/// same interrupt they would have seen outside raw mode.
#[cfg(windows)]
fn deliver_interrupt() {
    unsafe {
        let _ = windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent(
            windows_sys::Win32::System::Console::CTRL_C_EVENT,
            0,
        );
    }
}

#[cfg(not(any(unix, windows)))]
fn deliver_interrupt() {}

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "[io-door:test] test fs/process scaffolding")]
mod tests {
    //! Tests for exarch's raw-mode cancel path.
    //!
    //! Unix-only: the chained-handler design under test exists only on
    //! Unix.  On Windows `install` is a no-op and `deliver_interrupt`
    //! emits an asynchronous `CTRL_C_EVENT` that the OS routes through
    //! the console subsystem, so a synchronous unit assertion against
    //! `ral_core::process` state is not meaningful there.

    use super::*;
    use crate::bootstrap::Scratch;
    use crate::session::Session;
    use std::sync::Mutex;

    /// Both tests touch process-global state (`SIGNAL_COUNT`, the
    /// `CURRENT` slot, and the `RAL_HANDLER` slot that `install`
    /// publishes), so they must not run concurrently.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// `install` publishes ral's handler into the `RAL_HANDLER` slot, and
    /// always the same `term_handler` pointer: a second `install` (the
    /// `/clear` re-chain) republishes the very same value, so the slot is
    /// a true set-once.  The slot must round-trip to `term_handler`'s
    /// pointer so the chained handler forwards into ral's own disposition.
    #[test]
    fn install_publishes_term_handler_as_set_once() {
        let _g = SERIAL.lock().unwrap();
        let expected = ral_core::process::term_handler() as *mut ();
        install();
        assert_eq!(
            RAL_HANDLER.load(Ordering::Acquire),
            expected,
            "install publishes ral's term_handler pointer"
        );
        install();
        assert_eq!(
            RAL_HANDLER.load(Ordering::Acquire),
            expected,
            "re-install republishes the same pointer"
        );
    }

    /// Esc cancels exarch's per-turn token (and, via `deliver_interrupt`,
    /// the current turn's foreground scope — exercised by ral_core's own
    /// slot tests), but no longer escalates ral's process-global interrupt
    /// counter: detached workers poll their own scopes, not the foreground,
    /// so they must survive an Esc that stops the foreground agent.
    #[test]
    fn esc_cancels_token_without_ticking_global_counter() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        let root = mint_root();
        raise_interrupt();
        assert!(root.token().is_cancelled(), "the root token should be set");
        assert!(is_set(), "the published slot should report cancelled");
        assert!(
            !ral_core::process::is_interrupted(),
            "Esc cancels the per-turn token, not ral's global interrupt counter"
        );
        ral_core::process::clear();
    }

    /// Esc routes through `raise_interrupt`; pressing it repeatedly must
    /// never escalate toward a force-exit.  The Esc path cancels the
    /// per-turn token and the foreground scope and never touches the
    /// process-global counter, so non-escalation holds by construction —
    /// the counter stays un-ticked no matter how many times Esc is pressed.
    #[test]
    fn repeated_interrupt_never_force_exits() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        let _root = mint_root();
        for _ in 0..5 {
            raise_interrupt();
        }
        assert!(is_set(), "the root token should be set");
        assert!(
            !ral_core::process::is_interrupted(),
            "the Esc path never ticks ral's global counter, so it cannot escalate"
        );
        ral_core::process::clear();
    }

    /// A fresh mint replaces the prior turn's token: a cancel raised in one
    /// root turn does not bleed into the next.  This is the X5 fix — the
    /// reset is minting, not a clear at every `apply`, so a sub-agent's
    /// `apply` (which never mints) cannot erase a just-pressed Esc.
    #[test]
    fn fresh_mint_does_not_inherit_prior_cancel() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        {
            let first = mint_root();
            raise_interrupt();
            assert!(first.token().is_cancelled());
        }
        // Prior guard dropped; the slot is null until the next mint.
        assert!(!is_set(), "no token is published between turns");
        let second = mint_root();
        assert!(
            !second.token().is_cancelled(),
            "a freshly minted root turn starts un-cancelled"
        );
        assert!(!is_set(), "the published slot reports the fresh token");
        ral_core::process::clear();
    }

    /// A child token is a clone of the parent's: cancelling either
    /// (here, the published root via a signal) cancels both, so an Esc
    /// landing just before a sub-agent dispatch still halts the child.
    #[test]
    fn child_token_shares_parent_cancellation() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        let root = mint_root();
        let child = root.token().clone();
        assert!(!child.is_cancelled());
        raise_interrupt();
        assert!(
            child.is_cancelled(),
            "a child sharing the parent token is cancelled with it"
        );
        ral_core::process::clear();
    }

    /// `boot_shell` installs ral's bare handlers, then re-chains exarch's
    /// cancel handler before returning.  Without the re-install, the bare
    /// ral handler would run alone after any shell rebuild and `raise` (the
    /// token half of the chain) would never fire.
    #[test]
    fn boot_shell_restores_the_chain_after_handler_clobber() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        // A raw ral install models any clobber before the exarch session
        // constructor runs.
        ral_core::process::install_handlers();
        let _shell = crate::bootstrap::boot_shell();
        let root = mint_root();
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0, "raise SIGINT");
        assert!(
            root.token().is_cancelled(),
            "boot_shell should return with exarch's cancel chain installed"
        );
        assert!(
            ral_core::process::is_interrupted(),
            "the re-chained handler still forwards into ral"
        );
        ral_core::process::clear();
    }

    /// `/clear` can be the first action after Esc.  Esc leaves ral's
    /// process interrupt flag set, and the shell rebuild evaluates the
    /// embedded agent library before any ordinary tool-call cleanup runs;
    /// the exarch shell constructor must therefore discard that stale
    /// interrupt before loading the library.
    #[test]
    fn clear_discards_stale_ral_interrupt_before_reboot() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();

        let dir =
            std::env::temp_dir().join(format!("exarch-clear-interrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let scratch = Scratch::new().expect("scratch directory");
        let mut session = Session::for_test(&dir, "system").expect("test session");

        ral_core::process::interrupt();
        assert!(
            ral_core::process::is_interrupted(),
            "the test must start with the stale interrupt `/clear` used to inherit"
        );

        session
            .clear(&scratch)
            .expect("/clear should reboot despite a stale interrupt");
        assert!(
            !ral_core::process::is_interrupted(),
            "/clear must leave the next prompt un-interrupted"
        );

        let root = mint_root();
        raise_interrupt();
        assert!(
            root.token().is_cancelled(),
            "/clear must still re-chain exarch's cancel handler"
        );

        ral_core::process::clear();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(scratch.path());
    }
}
