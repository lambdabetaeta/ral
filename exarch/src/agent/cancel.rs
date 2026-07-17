//! Exarch's per-turn cancellation, layered on top of ral's SIGINT handling.
//!
//! ral's `signal::install_handlers` translates a delivered signal into a
//! [`CancelCause`](ral_core::process::CancelCause) on the published
//! cancel slots, so the evaluator unwinds at its next poll; that
//! interrupts an in-flight tool call but leaves exarch's turn loop free
//! to keep going.  Here we add a cancellation [`Token`]: every agent holds one sticky token for its
//! life (registered in the fleet so the subtree cascade always reaches the
//! live turn), and the drive loop [`reset`](Token::reset)s its flag at each
//! genuine turn boundary so a prior turn's Esc never bleeds into the next.
//! The token is threaded down through `apply` → dispatch → tools, cancelled
//! by the chained signal handler, by a per-tab turn interrupt
//! (`AgentRegistry::interrupt`), by the registry cascade (`agent_cancel`, the
//! ceiling, `/clear`), and raced by the HTTP request future, so one
//! cancellation stops the turn and returns to the prompt.  The cause a token
//! carries decides how far it reaches: an
//! [`Interrupt`](ral_core::process::CancelCause) unwinds only the in-flight
//! turn and the agent re-parks; any stronger cause terminates the agent.
//!
//! Ctrl-C and Esc are a *per-tab turn interrupt* — they unwind the focused
//! tab's current turn, never cascade to descendants, never end the agent.  On
//! the trunk they route through [`raise_interrupt`], which cancels the trunk's
//! published token and asks ral to cancel the current turn's foreground scope
//! with [`CancelCause::Interrupt`](ral_core::process::CancelCause), so the
//! foreground evaluation unwinds at its next poll; on any other focused tab
//! they route through `AgentRegistry::interrupt`, which cancels that agent's
//! own token *and* its session's durable root with the same cause (only the
//! trunk's session publishes the process signal slots; a sub-agent's eval is
//! reached through that per-session handle).  No global counter is ticked on
//! the interrupt path, so there is nothing to escalate toward a force-exit.
//!
//! The signal handler cannot hold a token by value, so the trunk
//! [`publish`]es its token's flag into a process-global *slot* (an
//! `AtomicPtr`, the lock-free `ArcSwap` analogue — a signal handler must
//! not lock) for the handler to set.  The slot points into the trunk's
//! own sticky [`Token`]'s `Arc<AtomicU8>`; [`publish`] leaks one strong
//! share of that `Arc` so the pointee outlives every guard and every other
//! share, making the published pointer safe for a handler to dereference
//! at any time, including one that loaded it just before the slot was
//! restored.  A signal-driven cancellation is observed through the threaded
//! [`Token`] every cancel check already holds (`is_set` reads the slot
//! directly, but only in tests).  The slot is published by [`publish`]'s
//! RAII guard and restored to its prior publication on drop, which only
//! stops the slot from tracking a retired trunk's token; only the trunk
//! publishes (a sub-agent's token is reached through the fleet registry,
//! never the slot).
//!
//! Install order matters: ral's handlers must be set first; then `install`
//! replaces the disposition with a handler that sets the current root token
//! *and* forwards to ral's, so statement-level unwinding still works.  A
//! forwarded SIGINT goes to ral's *non-escalating* [`relay_handler`]
//! (`ral_core::process::relay_handler`), not the [`term_handler`] whose
//! third signal `_exit`s: a SIGINT reaching the supervising TUI — from a
//! stray child, another process, anything — must only cancel the current
//! turn, never force-exit exarch.  SIGTERM/SIGHUP keep ral's `term_handler`,
//! since those are deliberate termination requests: it cancels the durable
//! root with `Terminate` — reaching the foreground turn and every detached
//! worker — and force-exits on the third delivery; the chained handler
//! stamps the trunk's own token with the same `Terminate` cause, so a park
//! reading the token agrees with ral's root about why the agent is ending.
//! [`crate::bootstrap::boot_shell`] owns that ceremony for exarch session
//! shells, including `/clear` rebuilds.
//!
//! Windows has no single process-wide disposition to capture and replace:
//! `SetConsoleCtrlHandler` instead keeps a list of handler routines, run
//! last-registered-first until one returns `TRUE`.  `install` registers its
//! own routine there — strictly after `ral_core::process::install_handlers`
//! runs (the same ordering requirement as Unix), which puts exarch's
//! routine ahead of ral's `ctrlc`-installed one in that list.  On Ctrl-C or
//! Ctrl-Break — the same two events [`cancels_turn`] recognises — it calls
//! [`raise`] and [`ral_core::process::relay_interrupt`] directly: the
//! non-escalating relay that cancels the current turn's foreground scope
//! and fans a Ctrl-Break out to every live, non-detached pipeline group,
//! the same contract Unix's [`relay_handler`] gives a forwarded SIGINT.  It
//! then returns `TRUE` ("handled"), so ral's own `ctrlc`-installed
//! disposition — whose ladder ticks a counter toward `TerminateJobObject`
//! and `ExitProcess` — never runs for these two events: a trunk interrupt
//! can only ever cancel the turn, never escalate, and never reaches a
//! detached worker's group.  Every other console event (window close,
//! logoff, shutdown) is a genuine termination request, not a turn-cancel:
//! [`cancels_turn`] answers `false` for those, exarch's handler returns
//! `FALSE` in turn, and ral's escalating disposition runs exactly as it
//! would without exarch installed — the Windows analogue of SIGTERM/SIGHUP
//! staying on Unix's escalating [`term_handler`].
//!
//! The Esc key never reaches `SetConsoleCtrlHandler` at all: the TUI's own
//! read loop captures it as a raw key event (raw mode disables
//! `ENABLE_PROCESSED_INPUT`, so Windows stops turning Ctrl-C into a console
//! event too — both arrive as ordinary key events instead) and calls
//! [`raise_interrupt`] directly, which reaches the same non-escalating
//! relay through [`deliver_interrupt`].  `console_ctrl_handler`'s
//! registration still earns its keep for Ctrl-Break, which — unlike
//! Ctrl-C — always raises a console event regardless of
//! `ENABLE_PROCESSED_INPUT`, and for the termination events, which have no
//! raw-mode key-event counterpart at all.

use ral_core::process::CancelCause;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

/// An agent's cancellation handle.
///
/// Cloning shares the same flag (an
/// `Arc<AtomicU8>`, holding the [`CancelCause`] a cancel was raised with, `0`
/// while un-cancelled), so the registry entry's clone and the drive loop's
/// clone are one token — cancelling either halts the agent's turn.
#[derive(Clone, Default)]
pub struct Token(Arc<AtomicU8>);

impl Token {
    /// A fresh, un-cancelled token.  Each agent owns one for its life; the
    /// trunk additionally [`publish`]es its token to the signal slot.
    pub fn new() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }

    /// True once this token (or, since clones share the flag, any of its
    /// shares) has been cancelled — for *any* cause.  The `apply` loop and the
    /// provider poll this to unwind whatever turn is in flight.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed) != 0
    }

    /// True when a *terminate*-cause cancel is in force — any cause but an
    /// [`Interrupt`](CancelCause).  A non-`Held` park ends the agent on this;
    /// a bare interrupt only drops the in-flight turn and the agent re-parks.
    pub fn terminated(&self) -> bool {
        let flag = self.0.load(Ordering::Relaxed);
        flag != 0 && flag != CancelCause::Interrupt as u8
    }

    /// Cancel this token and every share of it, recording `cause`.  Raises
    /// the flag to the maximum of its current value and `cause` — the same
    /// monotone escalation `CancelScope::cancel` gives ral's own scopes — so
    /// a later, weaker cause (an Esc `Interrupt` arriving after an
    /// `agent_cancel` `Explicit`) can never mask a stronger one already in
    /// force.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.fetch_max(cause as u8, Ordering::Relaxed);
    }

    /// Clear a bare interrupt — the per-turn reset the drive loop runs at a
    /// genuine turn boundary, so a prior turn's Esc never bleeds into the
    /// next.  A compare-exchange from exactly [`Interrupt`](CancelCause::Interrupt)
    /// to `0`: any terminate-class cause already recorded (`Explicit`,
    /// `Deadline`, ...) is left in force, since a turn boundary must never
    /// erase a cascade cancellation that landed between the drive loop's
    /// pop and this reset.  Each agent holds one sticky token (registered
    /// once, so the subtree cascade always reaches the live turn); the
    /// boundary clears its flag rather than swapping the `Arc`.
    pub fn reset(&self) {
        let _ = self.0.compare_exchange(
            CancelCause::Interrupt as u8,
            0,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

/// The trunk's token flag, published for the signal handler.  Null when no
/// trunk is publishing.  Holds a pointer into the [`Token`]'s `Arc`, made
/// immortal by [`publish`]'s deliberate leak so the pointee outlives every
/// guard.
static CURRENT: AtomicPtr<AtomicU8> = AtomicPtr::new(std::ptr::null_mut());

/// Publish an existing token to the signal slot for as long as the returned
/// guard lives.
///
/// The trunk (the parent-less agent) calls it once, holding the
/// guard for its whole drive, so a SIGINT/SIGTERM/Ctrl-C cancels the trunk's
/// current turn through the token it already threads — without the
/// per-turn-mint dance, because the boundary [`reset`](Token::reset)s the same
/// sticky token instead of swapping it.  A signal handler that has already
/// loaded the slot's pointer may dereference it at any later instant,
/// including after the guard has dropped and restored the slot, so the
/// published allocation must outlive the guard rather than merely the
/// publishing interval: this leaks one strong share of the token's `Arc`,
/// making the pointee live for the rest of the process.  That leak is
/// bounded and deliberate — production calls this once per process, with
/// the trunk holding the guard for its whole drive, plus a handful of calls
/// from tests.
pub fn publish(token: &Token) -> SlotGuard {
    std::mem::forget(token.0.clone());
    let flag: *const AtomicU8 = Arc::as_ptr(&token.0);
    let prev = CURRENT.swap(flag.cast_mut(), Ordering::Release);
    SlotGuard { prev }
}

/// RAII handle for the published slot: restores the prior publication on drop.
///
/// A swap, not a clear, so an inner publication nested inside an outer one —
/// an overlapping `publish` — reveals the outer token again rather than
/// leaving the slot null underneath it.  The pointee is immortal (leaked by
/// [`publish`]), so the guard bounds only *when* the slot fires, not how long
/// the allocation behind it lives.
pub struct SlotGuard {
    prev: *mut AtomicU8,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        CURRENT.store(self.prev, Ordering::Release);
    }
}

/// True if the trunk's published slot reads cancelled.  A test probe for the
/// signal-handler slot: production code observes cancellation through the
/// threaded [`Token`] directly, so the slot's boolean is read only here.
/// False when no trunk is publishing.
#[cfg(all(test, unix))]
pub(crate) fn is_set() -> bool {
    let p = CURRENT.load(Ordering::Acquire);
    // SAFETY: a non-null slot points into the allocation `publish` leaks, so
    // the pointee is live for the rest of the process (see `publish`).
    !p.is_null() && unsafe { (*p).load(Ordering::Relaxed) } != 0
}

/// Raise `cause` on the trunk's published token (signal-handler safe: a
/// single atomic load of the slot plus a `fetch_max`, so a weaker cause can
/// never mask a stronger one already recorded).  A no-op when no trunk is
/// publishing.
fn raise(cause: CancelCause) {
    let p = CURRENT.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: a non-null slot points into the allocation `publish` leaks,
        // so the pointee is live for the rest of the process (see `publish`).
        unsafe { (*p).fetch_max(cause as u8, Ordering::Relaxed) };
    }
}

pub fn raise_interrupt() {
    raise(CancelCause::Interrupt);
    deliver_interrupt();
}

/// Install the chained signal handler.
///
/// Must run *after*
/// `ral_core::process::install_handlers` — we capture ral's dispositions
/// and forward into them so its cancel semantics are preserved.  SIGINT
/// forwards into the non-escalating [`relay_handler`], which requests the
/// cooperative foreground unwind and relays to external pipeline groups but
/// never `_exit`s; SIGTERM/SIGHUP forward into the escalating
/// [`term_handler`], the right ladder for a deliberate termination request.
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

#[cfg(not(any(unix, windows)))]
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
/// root-terminate-then-force-exit disposition is correct.
#[cfg(unix)]
static RAL_TERM_HANDLER: AtomicPtr<()> = AtomicPtr::new(std::ptr::null_mut());

#[cfg(unix)]
extern "C" fn chained(sig: libc::c_int) {
    let (cause, slot) = if sig == libc::SIGINT {
        (CancelCause::Interrupt, &RAL_SIGINT_HANDLER)
    } else {
        (CancelCause::Terminate, &RAL_TERM_HANDLER)
    };
    raise(cause);
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

/// Raw mode suppresses the console's automatic Ctrl-C handling — Esc and
/// Ctrl-C both surface as ordinary key events instead (see the module
/// doc).  Call ral's non-escalating relay directly, in-process: it cancels
/// the current turn's foreground scope and fans a Ctrl-Break out to every
/// live, non-detached pipeline group, exactly what [`console_ctrl_handler`]
/// does for a real console event.  A `GenerateConsoleCtrlEvent` re-injection
/// was tried and rejected here: it broadcasts to the whole console group and
/// re-enters `SetConsoleCtrlHandler`'s chain, ticking ral's escalation
/// counter on every trunk interrupt — exactly the contract this module must
/// not violate.
#[cfg(windows)]
fn deliver_interrupt() {
    ral_core::process::relay_interrupt();
}

#[cfg(not(any(unix, windows)))]
fn deliver_interrupt() {}

/// Whether a delivered Windows console-control event is a turn-cancel
/// gesture — Ctrl-C or Ctrl-Break, the same two events the `ctrlc` crate
/// reacts to — that exarch's handler fully handles itself.
///
/// Pulled out of [`console_ctrl_handler`] as a pure function of the event
/// code so the decision is unit-testable without a real console handler —
/// `SetConsoleCtrlHandler` cannot be exercised in a test, but the decision
/// it drives can, the same reason [`Token`]'s cause is a plain `u8` rather
/// than something only readable inside a handler.
///
/// The single bool doubles as both "does this cancel the turn" and "does
/// exarch report the event as handled": for Ctrl-C/Ctrl-Break exarch
/// performs the whole non-escalating relay itself and must stop the event
/// from reaching ral's escalating disposition next in the handler list, so
/// the two questions have one answer.  Every other event (window close,
/// logoff, shutdown) is a genuine termination request with no turn to
/// cancel, and exarch leaves it unhandled so ral's own disposition still
/// applies its escalation ladder — see the module doc.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "only called from console_ctrl_handler, which is cfg(windows); \
                  exercised directly by this module's own tests on every host"
    )
)]
pub(crate) fn cancels_turn(ctrl_type: u32) -> bool {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT
}

/// Set-once guard for [`install`]'s `SetConsoleCtrlHandler` registration.
/// Unlike Unix's `libc::signal`, which simply overwrites the same
/// disposition on every call, `SetConsoleCtrlHandler(_, TRUE)` *appends* a
/// new handler routine to the process's list each time it is called — so
/// without this guard, every `/clear` rebuild (each of which re-runs
/// [`crate::bootstrap::boot_shell`], and so `install`) would register
/// another copy.  Harmless individually ([`raise`] is idempotent), but
/// unbounded growth over repeated rebuilds is not the "install once, keep
/// working" contract `install` has on Unix.
#[cfg(windows)]
static WIN_CTRL_HANDLER_INSTALLED: std::sync::Once = std::sync::Once::new();

/// Register exarch's console-ctrl handler.  Must run after
/// `ral_core::process::install_handlers` (see the module doc for why); the
/// handler translates Ctrl-C/Ctrl-Break into a turn-cancel via
/// [`cancels_turn`] and reports those two events as handled so ral's own
/// escalating disposition never runs for them, deferring to it only for
/// the genuine termination events.
#[cfg(windows)]
pub fn install() {
    WIN_CTRL_HANDLER_INSTALLED.call_once(|| {
        // SAFETY: `console_ctrl_handler` matches `PHANDLER_ROUTINE`'s
        // `unsafe extern "system" fn(u32) -> BOOL` signature; `TRUE` (`1`)
        // adds it to the process's handler list rather than removing it.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                Some(console_ctrl_handler),
                1,
            );
        }
    });
}

/// The routine Windows calls on Ctrl-C/Ctrl-Break/console-close events.
/// Runs on a dedicated OS thread (not signal context), so plain atomics —
/// the same ones [`raise`] already uses for the Unix handler — are all it
/// needs. A safe `extern "system" fn` coerces to `PHANDLER_ROUTINE`'s
/// `unsafe extern "system" fn` pointer type, the same way `chained` (the
/// Unix counterpart, just above) is a plain `extern "C" fn`.
///
/// A Ctrl-C/Ctrl-Break is handled here in full — [`raise`] plus
/// [`ral_core::process::relay_interrupt`], the same non-escalating relay
/// [`deliver_interrupt`] calls for a raw-mode key event — and reported
/// `TRUE` so ral's own escalating disposition never runs for it.  Every
/// other event reports `FALSE`, deferring to ral's disposition unchanged.
#[cfg(windows)]
extern "system" fn console_ctrl_handler(ctrl_type: u32) -> windows_sys::core::BOOL {
    if cancels_turn(ctrl_type) {
        raise(CancelCause::Interrupt);
        ral_core::process::relay_interrupt();
        return windows_sys::Win32::Foundation::TRUE;
    }
    windows_sys::Win32::Foundation::FALSE
}

/// [`cancels_turn`] is a plain function of a `u32` event code, so it is
/// exercised natively on every platform this crate builds for — no
/// `cfg(windows)`, no real console handler required.
#[cfg(test)]
mod cancels_turn_tests {
    use super::cancels_turn;

    #[test]
    fn ctrl_c_and_ctrl_break_cancel_the_turn() {
        assert!(cancels_turn(0), "CTRL_C_EVENT cancels the turn");
        assert!(cancels_turn(1), "CTRL_BREAK_EVENT cancels the turn");
    }

    #[test]
    fn other_console_events_never_cancel_the_turn() {
        // CTRL_CLOSE_EVENT=2, CTRL_LOGOFF_EVENT=5, CTRL_SHUTDOWN_EVENT=6.
        for ctrl_type in [2, 5, 6] {
            assert!(
                !cancels_turn(ctrl_type),
                "event {ctrl_type} is not a turn-cancel signal, so ral's \
                 escalating disposition must still see it"
            );
        }
    }
}

/// [`Token::cancel`]/[`Token::reset`] are plain atomics over a
/// [`CancelCause`] encoding, so their escalation and reset semantics are
/// exercised natively on every platform this crate builds for — no signal
/// handler, no slot, required.
#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn cancel_is_monotone_and_never_downgrades() {
        let token = Token::new();
        token.cancel(CancelCause::Explicit);
        token.cancel(CancelCause::Interrupt);
        assert!(
            token.terminated(),
            "a later Interrupt must not downgrade an already-recorded Explicit"
        );
        token.cancel(CancelCause::Deadline);
        assert_eq!(
            token.0.load(Ordering::Relaxed),
            CancelCause::Deadline as u8,
            "a stronger later cause still escalates"
        );
    }

    #[test]
    fn reset_clears_only_a_bare_interrupt() {
        let token = Token::new();
        token.cancel(CancelCause::Interrupt);
        token.reset();
        assert!(!token.is_cancelled(), "reset clears a bare interrupt");

        let token = Token::new();
        token.cancel(CancelCause::Explicit);
        token.reset();
        assert!(
            token.terminated(),
            "reset must never erase a terminate-class cause"
        );
        assert_eq!(
            token.0.load(Ordering::Relaxed),
            CancelCause::Explicit as u8,
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

    /// Both tests touch process-global state (the escalation ladder, the
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
    /// the current turn's foreground scope — exercised by `ral_core`'s own
    /// slot tests), but never ticks ral's escalation ladder: detached
    /// workers are cancelled through the registry cascade, not the
    /// foreground, so they survive an Esc that stops the trunk alone.
    #[test]
    fn esc_cancels_token_without_ticking_escalation_ladder() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        let token = Token::new();
        let _slot = publish(&token);
        raise_interrupt();
        assert!(token.is_cancelled(), "the trunk token should be set");
        assert!(is_set(), "the published slot should report cancelled");
        assert!(
            !ral_core::process::escalation_pending(),
            "Esc cancels the trunk's token, never ral's escalation ladder"
        );
        ral_core::process::clear();
    }

    /// Esc routes through `raise_interrupt`; pressing it repeatedly must
    /// never escalate toward a force-exit.  The Esc path cancels the
    /// trunk's token and the foreground scope and never touches the
    /// escalation ladder, so non-escalation holds by construction — the
    /// ladder stays un-ticked no matter how many times Esc is pressed.
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
            !ral_core::process::escalation_pending(),
            "the Esc path never ticks the escalation ladder, so it cannot force-exit"
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
        let threaded = token;
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
    /// SIGINT would tick ral's escalation ladder.  With the chain in place
    /// a SIGINT sets the token and routes into the *non-escalating*
    /// `relay_handler`, which cancels the foreground turn without ever
    /// ticking the third-signal `_exit` ladder — so the two observable
    /// signatures (token set, ladder un-ticked) together prove the chain
    /// is installed and a delivered SIGINT can only cancel, never force-exit.
    #[test]
    #[ignore = "delivers a real process-wide SIGINT — driven in its own process by signal_delivery_tests_own_their_process"]
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
            !ral_core::process::escalation_pending(),
            "the re-chained SIGINT routes into the non-escalating relay handler, \
             never the force-exit ladder"
        );
        ral_core::process::clear();
    }

    /// `/clear` can be the first action after a delivered termination
    /// signal.  The signal's cooperative delivery (a cause on the cancel
    /// slots) dies with the turn that unwound on it, but its escalation
    /// tick would otherwise outlive the turn — leaving the rebuilt
    /// session one delivery closer to the third-signal `_exit`.  The
    /// exarch shell constructor resets the ladder before loading the
    /// library.
    #[test]
    #[ignore = "invokes the SIGTERM handler, root-cancelling every published slot — driven in its own process by signal_delivery_tests_own_their_process"]
    fn clear_resets_the_escalation_ladder_on_reboot() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();

        let dir =
            std::env::temp_dir().join(format!("exarch-clear-interrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let scratch = Scratch::for_test("clear-interrupt").expect("scratch directory");
        let mut session = Agent::for_test(&dir, "system").expect("test session");

        // Seed the ladder exactly as a delivered SIGTERM would: through
        // ral's own term handler.  No turn is running, so the published
        // cancel slots are null and the delivery is the tick alone.
        ral_core::process::term_handler()(libc::SIGTERM);
        assert!(
            ral_core::process::escalation_pending(),
            "the test must start with the stale escalation tick `/clear` used to inherit"
        );

        session
            .clear(&scratch)
            .expect("/clear should reboot despite a stale escalation tick");
        assert!(
            !ral_core::process::escalation_pending(),
            "/clear must reset the escalation ladder"
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

    /// Drive the two `#[ignore]`d signal-delivery tests above in a child
    /// process they own outright.  Delivered signals are process-wide — a
    /// raised SIGINT cancels the process's published foreground turn, and
    /// the SIGTERM handler root-cancels every published slot — so inside
    /// the parallel test binary they terminate whatever *other* test
    /// happens to be mid-turn.  The `SERIAL` lock cannot help: the victims
    /// are readers that never know to take it.  Re-execing the test binary
    /// filtered to exactly these tests gives them the singleton process the
    /// signal machinery is designed around.
    #[test]
    fn signal_delivery_tests_own_their_process() {
        let exe = std::env::current_exe().expect("test binary path");
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "agent::cancel::tests::boot_shell_restores_the_chain_after_handler_clobber",
                "agent::cancel::tests::clear_resets_the_escalation_ladder_on_reboot",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .expect("spawn the child test process");
        let stdout = String::from_utf8_lossy(&out.stdout);
        // The pass-count check guards the filters: a renamed test would
        // otherwise make the child silently run nothing and still exit 0.
        assert!(
            out.status.success() && stdout.contains("2 passed"),
            "child signal tests failed or did not both run:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// An inner `publish` nested inside an outer one restores the outer
    /// publication on drop, rather than leaving the slot null underneath
    /// it — the same nesting discipline `ral_core`'s own `CancelSlot`
    /// gives its scope publications.
    #[test]
    fn inner_publish_restores_the_outer_token_on_drop() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        let outer = Token::new();
        let _outer_slot = publish(&outer);
        {
            let inner = Token::new();
            let _inner_slot = publish(&inner);
            raise_interrupt();
            assert!(
                inner.is_cancelled(),
                "the inner publication must shadow the outer"
            );
            assert!(
                !outer.is_cancelled(),
                "the outer token is untouched while shadowed"
            );
        }
        raise_interrupt();
        assert!(
            outer.is_cancelled(),
            "the inner guard's drop must restore the outer publication, \
             not null the slot"
        );
        ral_core::process::clear();
    }

    /// The chained handler maps each signal to its own cause: SIGINT is a
    /// per-tab interrupt, SIGTERM/SIGHUP a genuine termination request —
    /// stamping every signal `Interrupt` would misreport the cause during
    /// a real termination and let a park `UntilCancelled` on the trunk
    /// token survive its own SIGTERM.
    #[test]
    fn chained_maps_each_signal_to_its_own_cause() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();

        let token = Token::new();
        let _slot = publish(&token);
        chained(libc::SIGINT);
        assert_eq!(
            token.0.load(Ordering::Relaxed),
            CancelCause::Interrupt as u8,
            "SIGINT stamps Interrupt"
        );
        assert!(
            !token.terminated(),
            "a SIGINT-driven Interrupt never terminates the agent"
        );
        ral_core::process::clear();

        let token = Token::new();
        let _slot = publish(&token);
        chained(libc::SIGTERM);
        assert_eq!(
            token.0.load(Ordering::Relaxed),
            CancelCause::Terminate as u8,
            "SIGTERM stamps Terminate, not Interrupt"
        );
        assert!(
            token.terminated(),
            "a SIGTERM-driven Terminate ends the agent"
        );
        ral_core::process::clear();
    }
}
