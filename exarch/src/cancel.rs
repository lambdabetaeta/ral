//! Exarch's per-turn cancellation, layered on top of ral's SIGINT handling.
//!
//! ral's `signal::install_handlers` sets `SIGNAL_COUNT` so the
//! evaluator unwinds between statements; that interrupts an in-flight
//! tool call but leaves exarch's turn loop free to keep going.  Here we
//! add a cancellation [`Token`]: every agent holds one sticky token for its
//! life (registered in the fleet so the subtree cascade always reaches the
//! live turn), and the drive loop [`reset`](Token::reset)s its flag at each
//! genuine turn boundary so a prior turn's Esc never bleeds into the next.
//! The token is threaded down through `apply` → dispatch → tools, cancelled
//! by the chained signal handler, by the registry cascade (`agent_cancel`,
//! the ceiling, an Esc on the focused subtree), and raced by the HTTP
//! request future, so one signal stops the turn and returns to the prompt.
//!
//! Ctrl-C and Esc route through [`raise_interrupt`], which cancels
//! the trunk's published token and asks ral to cancel the current turn's
//! foreground scope with [`CancelCause::Interrupt`](ral_core::process::CancelCause):
//! the foreground evaluation unwinds at its next poll, while detached
//! workers — parented at the durable root, not the foreground — are
//! cancelled instead through the registry's subtree cascade, which cancels
//! each agent's token *and* its own session's durable root (only the
//! trunk's session publishes the process signal slots; a sub-agent's eval
//! is reached through that per-session handle).  No global counter is
//! ticked on the Esc path, so there is nothing to escalate toward a
//! force-exit.
//!
//! The signal handler cannot hold a token by value, so the trunk
//! [`publish`]es its token's flag into a process-global *slot* (an
//! `AtomicPtr`, the lock-free ArcSwap analogue — a signal handler must
//! not lock) for the handler to set.  The slot points into the trunk's
//! own sticky [`Token`]'s `Arc<AtomicBool>`; [`publish`] leaks one strong
//! share of that `Arc` so the pointee outlives every guard and every other
//! share, making the published pointer safe for a handler to dereference
//! at any time, including one that loaded it just before the slot was
//! nulled.  A signal-driven cancellation is observed through the threaded
//! [`Token`] every cancel check already holds (`is_set` reads the slot
//! directly, but only in tests).  The slot is published by [`publish`]'s
//! RAII guard and cleared on its drop, which only stops the slot from
//! tracking a retired trunk's token; only the trunk publishes (a
//! sub-agent's token is reached through the fleet registry, never the
//! slot).
//!
//! Install order matters: ral's handlers must be set first; then `install`
//! replaces the disposition with a handler that sets the current root token
//! *and* forwards to ral's, so statement-level unwinding still works.  A
//! forwarded SIGINT goes to ral's *non-escalating* [`relay_handler`]
//! (`ral_core::process::relay_handler`), not the [`term_handler`] whose
//! third signal `_exit`s: a SIGINT reaching the supervising TUI — from a
//! stray child, another process, anything — must only cancel the current
//! turn, never force-exit exarch.  SIGTERM/SIGHUP keep ral's `term_handler`,
//! since those are deliberate termination requests.
//! [`crate::bootstrap::boot_shell`] owns that ceremony for exarch session
//! shells, including `/clear` rebuilds.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

/// An agent's cancellation handle.  Cloning shares the same flag
/// (an `Arc<AtomicBool>`), so the registry entry's clone and the drive
/// loop's clone are one token — cancelling either halts the agent's turn.
#[derive(Clone, Default)]
pub struct Token(Arc<AtomicBool>);

impl Token {
    /// A fresh, un-cancelled token.  Each agent owns one for its life; the
    /// trunk additionally [`publish`]es its token to the signal slot.
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

    /// Clear the cancellation flag — the per-turn reset the drive loop runs
    /// at a genuine turn boundary, so a prior turn's Esc never bleeds into
    /// the next.  Each agent holds one sticky token (registered once, so the
    /// subtree cascade always reaches the live turn); the boundary clears its
    /// flag rather than swapping the `Arc`.
    pub fn reset(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// The trunk's token flag, published for the signal handler.  Null when no
/// trunk is publishing.  Holds a pointer into the [`Token`]'s `Arc`, made
/// immortal by [`publish`]'s deliberate leak so the pointee outlives every
/// guard.
static CURRENT: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());

/// Publish an existing token to the signal slot for as long as the returned
/// guard lives.  The trunk (the parent-less agent) calls it once, holding the
/// guard for its whole drive, so a SIGINT/SIGTERM/Ctrl-C cancels the trunk's
/// current turn through the token it already threads — without the
/// per-turn-mint dance, because the boundary [`reset`](Token::reset)s the same
/// sticky token instead of swapping it.  A signal handler that has already
/// loaded the slot's pointer may dereference it at any later instant,
/// including after the guard has dropped and nulled the slot, so the
/// published allocation must outlive the guard rather than merely the
/// publishing interval: this leaks one strong share of the token's `Arc`,
/// making the pointee live for the rest of the process.  That leak is
/// bounded and deliberate — production calls this once per process, with
/// the trunk holding the guard for its whole drive, plus a handful of calls
/// from tests.
pub fn publish(token: &Token) -> SlotGuard {
    std::mem::forget(token.0.clone());
    let flag: *const AtomicBool = Arc::as_ptr(&token.0);
    CURRENT.store(flag as *mut AtomicBool, Ordering::Release);
    SlotGuard
}

/// RAII handle for the published slot: nulls [`CURRENT`] on drop so the slot
/// stops tracking a retired trunk's token.  The pointee is immortal (leaked
/// by [`publish`]), so the guard bounds only *when* the slot fires, not how
/// long the allocation behind it lives.
pub struct SlotGuard;

impl Drop for SlotGuard {
    fn drop(&mut self) {
        CURRENT.store(std::ptr::null_mut(), Ordering::Release);
    }
}

/// True if the trunk's published slot reads cancelled.  A test probe for the
/// signal-handler slot: production code observes cancellation through the
/// threaded [`Token`] directly, so the slot's boolean is read only here.
/// False when no trunk is publishing.
#[cfg(test)]
pub(crate) fn is_set() -> bool {
    let p = CURRENT.load(Ordering::Acquire);
    // SAFETY: a non-null slot points into the allocation `publish`
    // deliberately leaked, so the pointee is live for the rest of the
    // process — the guard's null-on-drop only bounds when the slot fires,
    // not the pointee's lifetime.
    !p.is_null() && unsafe { (*p).load(Ordering::Relaxed) }
}

/// Set the trunk's published token (signal-handler safe: a single atomic load
/// of the slot plus an atomic store).  A no-op when no trunk is publishing.
fn raise() {
    let p = CURRENT.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: a non-null slot points into the allocation `publish`
        // deliberately leaked, so the pointee is live for the rest of the
        // process — the guard's null-on-drop only bounds when the slot fires,
        // not the pointee's lifetime.
        unsafe { (*p).store(true, Ordering::Relaxed) };
    }
}

pub fn raise_interrupt() {
    raise();
    deliver_interrupt();
}

/// Install the chained signal handler.  Must run *after*
/// `ral_core::process::install_handlers` — we capture ral's dispositions
/// and forward into them so its cancel semantics are preserved.  SIGINT
/// forwards into the non-escalating [`relay_handler`], which requests the
/// cooperative foreground unwind and relays to external pipeline groups but
/// never `_exit`s; SIGTERM/SIGHUP forward into the escalating
/// [`term_handler`], the right ladder for a deliberate termination request.
#[cfg(unix)]
/// Phase 2 Task 7 (signal relocation): When using `WireTransport`,
/// the front-end owns signal handlers and translates SIGINT/SIGTSTP/
/// SIGWINCH into `Control` frames sent through the `ControlSender`.
/// The `RAL_SIGINT_HANDLER`/`RAL_TERM_HANDLER` cross-process chaining
/// below is for the `IdentityTransport` path only — WireTransport
/// must not install these; the engine receives cancellation through
/// the wire, not a shared flag.
#[cfg(unix)]
pub fn install() {
    RAL_SIGINT_HANDLER.store(
        ral_core::process::relay_handler() as *mut (),
        Ordering::Release,
    );
    RAL_TERM_HANDLER.store(
        ral_core::process::term_handler() as *mut (),
        Ordering::Release,
    );
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

/// ral's prior SIGINT disposition — the non-escalating [`relay_handler`].
/// Forwarding SIGINT here cancels the current turn's foreground scope and
/// relays to external pipeline groups without ever ticking the third-signal
/// `_exit` counter, so a forwarded SIGINT can only cancel the turn.
/// `install` always stores the same pointer, so this is a set-once slot
/// read only from the signal handler — an `AtomicPtr`, the
/// signal-handler-safe analogue of [`CURRENT`], since a handler must not
/// lock and a `static mut` read is unsound under concurrent install.
#[cfg(unix)]
static RAL_SIGINT_HANDLER: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

/// ral's prior SIGTERM/SIGHUP disposition — the escalating [`term_handler`].
/// These are deliberate termination requests, so ral's
/// statement-unwind-then-force-exit ladder is the correct disposition.
#[cfg(unix)]
static RAL_TERM_HANDLER: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

#[cfg(unix)]
extern "C" fn chained(sig: libc::c_int) {
    raise();
    let slot = if sig == libc::SIGINT {
        &RAL_SIGINT_HANDLER
    } else {
        &RAL_TERM_HANDLER
    };
    forward_into_ral(slot, sig);
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

/// Feed a signal into the captured ral disposition for `slot`.
#[cfg(unix)]
fn forward_into_ral(slot: &AtomicPtr<()>, sig: libc::c_int) {
    let p = slot.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: `install` publishes a `relay_handler`/`term_handler`
        // `extern "C" fn` pointer into these slots and nowhere else, so a
        // non-null slot is exactly that pointer cast back to its
        // function-pointer type.
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
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    //! Tests for exarch's raw-mode cancel path.
    //!
    //! Unix-only: the chained-handler design under test exists only on
    //! Unix.  On Windows `install` is a no-op and `deliver_interrupt`
    //! emits an asynchronous `CTRL_C_EVENT` that the OS routes through
    //! the console subsystem, so a synchronous unit assertion against
    //! `ral_core::process` state is not meaningful there.

    use super::*;
    use crate::agent::Agent;
    use crate::bootstrap::Scratch;
    use std::sync::Mutex;

    /// Both tests touch process-global state (`SIGNAL_COUNT`, the
    /// `CURRENT` slot, and the handler slots that `install` publishes), so
    /// they must not run concurrently.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// `install` publishes ral's dispositions into the two handler slots:
    /// the non-escalating `relay_handler` for SIGINT, the escalating
    /// `term_handler` for SIGTERM/SIGHUP.  Both are set-once — a second
    /// `install` (the `/clear` re-chain) republishes the very same
    /// pointers — so the chained handler forwards each signal into the
    /// right ral disposition.
    #[test]
    fn install_publishes_split_handlers_as_set_once() {
        let _g = SERIAL.lock().unwrap();
        let expected_int = ral_core::process::relay_handler() as *mut ();
        let expected_term = ral_core::process::term_handler() as *mut ();
        install();
        assert_eq!(
            RAL_SIGINT_HANDLER.load(Ordering::Acquire),
            expected_int,
            "install publishes ral's non-escalating relay_handler for SIGINT"
        );
        assert_eq!(
            RAL_TERM_HANDLER.load(Ordering::Acquire),
            expected_term,
            "install publishes ral's term_handler for SIGTERM/SIGHUP"
        );
        install();
        assert_eq!(
            RAL_SIGINT_HANDLER.load(Ordering::Acquire),
            expected_int,
            "re-install republishes the same SIGINT pointer"
        );
        assert_eq!(
            RAL_TERM_HANDLER.load(Ordering::Acquire),
            expected_term,
            "re-install republishes the same SIGTERM/SIGHUP pointer"
        );
    }

    /// Esc cancels the trunk's published token (and, via `deliver_interrupt`,
    /// the current turn's foreground scope — exercised by ral_core's own
    /// slot tests), but no longer escalates ral's process-global interrupt
    /// counter: detached workers are cancelled through the registry cascade,
    /// not the foreground, so they survive an Esc that stops the trunk alone.
    #[test]
    fn esc_cancels_token_without_ticking_global_counter() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        let token = Token::new();
        let _slot = publish(&token);
        raise_interrupt();
        assert!(token.is_cancelled(), "the trunk token should be set");
        assert!(is_set(), "the published slot should report cancelled");
        assert!(
            !ral_core::process::is_interrupted(),
            "Esc cancels the trunk's token, not ral's global interrupt counter"
        );
        ral_core::process::clear();
    }

    /// Esc routes through `raise_interrupt`; pressing it repeatedly must
    /// never escalate toward a force-exit.  The Esc path cancels the
    /// trunk's token and the foreground scope and never touches the
    /// process-global counter, so non-escalation holds by construction —
    /// the counter stays un-ticked no matter how many times Esc is pressed.
    #[test]
    fn repeated_interrupt_never_force_exits() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        let token = Token::new();
        let _slot = publish(&token);
        for _ in 0..5 {
            raise_interrupt();
        }
        assert!(is_set(), "the trunk token should be set");
        assert!(
            !ral_core::process::is_interrupted(),
            "the Esc path never ticks ral's global counter, so it cannot escalate"
        );
        ral_core::process::clear();
    }

    /// The turn-boundary reset clears a prior turn's Esc so it does not bleed
    /// into the next.  The trunk holds one sticky published token; the drive
    /// loop [`Token::reset`]s its flag at each genuine boundary rather than
    /// swapping the `Arc`, and the slot keeps tracking that same token.
    #[test]
    fn reset_clears_prior_turn_cancel() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        let token = Token::new();
        let _slot = publish(&token);
        raise_interrupt();
        assert!(token.is_cancelled(), "Esc cancels the published token");
        assert!(is_set(), "the slot reports cancelled");
        // The drive loop's per-turn reset clears the flag for the next turn.
        token.reset();
        assert!(
            !token.is_cancelled(),
            "the boundary reset clears the prior turn's Esc"
        );
        assert!(
            !is_set(),
            "the slot still tracks the (now-cleared) sticky token"
        );
        ral_core::process::clear();
    }

    /// The drive loop threads *clones* of the published token into
    /// `apply`/dispatch/tools; cancelling the published token cancels every
    /// clone, so an Esc landing mid-turn halts the in-flight tool call.
    #[test]
    fn published_token_clone_shares_cancellation() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        let token = Token::new();
        let _slot = publish(&token);
        let threaded = token.clone();
        assert!(!threaded.is_cancelled());
        raise_interrupt();
        assert!(
            threaded.is_cancelled(),
            "a clone of the published token is cancelled with it"
        );
        ral_core::process::clear();
    }

    /// `boot_shell` installs ral's bare handlers, then re-chains exarch's
    /// cancel handler before returning.  Without the re-install, the bare
    /// ral `term_handler` would run alone after any shell rebuild: `raise`
    /// (the token half of the chain) would never fire, and the delivered
    /// SIGINT would `fetch_add` ral's escalating counter.  With the chain
    /// in place a SIGINT sets the token and routes into the *non-escalating*
    /// `relay_handler`, which cancels the foreground turn without ever
    /// ticking the third-signal `_exit` counter — so the two observable
    /// signatures (token set, counter un-ticked) together prove the chain
    /// is installed and a delivered SIGINT can only cancel, never force-exit.
    #[test]
    fn boot_shell_restores_the_chain_after_handler_clobber() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        // A raw ral install models any clobber before the exarch session
        // constructor runs.
        ral_core::process::install_handlers();
        let _shell = crate::bootstrap::boot_shell();
        let token = Token::new();
        let _slot = publish(&token);
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0, "raise SIGINT");
        assert!(
            token.is_cancelled(),
            "boot_shell should return with exarch's cancel chain installed"
        );
        assert!(
            !ral_core::process::is_interrupted(),
            "the re-chained SIGINT routes into the non-escalating relay handler, \
             not ral's force-exit counter"
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
        let mut session = Agent::for_test(&dir, "system").expect("test session");

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

        let token = Token::new();
        let _slot = publish(&token);
        raise_interrupt();
        assert!(
            token.is_cancelled(),
            "/clear must still re-chain exarch's cancel handler"
        );

        ral_core::process::clear();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(scratch.path());
    }
}
