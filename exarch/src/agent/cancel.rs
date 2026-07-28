//! Exarch's per-exchange cancellation, chained onto ral's signal handling.
//!
//! Every agent holds one sticky [`Token`] for its life, registered in the fleet
//! so the subtree cascade always reaches the live exchange; the attend loop
//! [`reset`](Token::reset)s it at each genuine exchange boundary, so a prior
//! exchange's Esc never bleeds into the next.  The cause a token carries decides
//! its reach: an `Interrupt` drops only the in-flight exchange and the agent
//! re-parks, anything stronger ends the agent.
//!
//! Esc and Ctrl-C are a per-tab exchange interrupt — never a cascade, never an
//! agent's death, and they never tick ral's escalation ladder.  The trunk routes
//! through [`raise_interrupt`] and the published slot; any other focused tab
//! through [`crate::fleet::registry::AgentRegistry::interrupt`], which cancels
//! that agent's own token and whatever foreground scope its run holds, never its
//! durable root.  Only the trunk publishes to the slot.
//!
//! On Unix `install` chains SIGINT into ral's *non-escalating* `relay_handler`,
//! never the `term_handler` whose third delivery `_exit`s: a stray SIGINT
//! reaching the supervising TUI must cancel the exchange, not kill exarch.
//! SIGTERM and SIGHUP keep `term_handler` and stamp the token `Terminate`, so a
//! park reading the token agrees with ral's root about why the agent is ending.
//!
//! Windows has no single disposition to replace — `SetConsoleCtrlHandler` keeps
//! a list, run last-registered-first until one returns `TRUE` — so there
//! `install` must run after `ral_core::process::install_handlers` to sit ahead
//! of ral's `ctrlc` routine, a correctness requirement rather than the Unix
//! convention.  Esc and Ctrl-C never reach that list anyway: raw mode clears
//! `ENABLE_PROCESSED_INPUT` and both arrive as ordinary key events.  The
//! registration earns its keep for Ctrl-Break, which raises a console event
//! regardless, and for the termination events, which have no key-event twin.
//! On both, [`crate::bootstrap::boot_shell`] owns the install ceremony.

use ral_core::process::CancelCause;
use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, AtomicU8, Ordering};

/// An agent's cancellation handle.
///
/// Clones share one flag, so the registry's entry and the attend loop hold the
/// same token: cancelling either halts the agent's exchange.
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
        flag != 0 && flag != CancelCause::Interrupt as u8
    }

    /// Cancel this token and every share of it, recording `cause`.  Monotone,
    /// like `CancelScope::cancel`: a weaker cause arriving later (an Esc
    /// `Interrupt` after an `agent-cancel` `Explicit`) can never mask a stronger
    /// one already in force.
    pub fn cancel(&self, cause: CancelCause) {
        self.0.fetch_max(cause as u8, Ordering::Relaxed);
    }

    /// Clear a bare interrupt, leaving any terminate-class cause in force: an
    /// exchange boundary must never erase a cascade cancellation that landed
    /// between the attend loop's pop and this reset.
    pub fn reset(&self) {
        let _ = self.0.compare_exchange(
            CancelCause::Interrupt as u8,
            0,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

/// The trunk's token flag, published for the signal handler; null when no trunk
/// is publishing.  The pointee is immortal, by [`publish`]'s deliberate leak.
static CURRENT: AtomicPtr<AtomicU8> = AtomicPtr::new(std::ptr::null_mut());

/// Publish `token`'s flag to the signal slot for as long as the guard lives.
///
/// A handler that has already loaded the slot's pointer may dereference it at
/// any later instant, including after the guard dropped and restored the slot,
/// so the pointee must outlive the guard rather than the publishing interval:
/// this leaks one strong share of the token's `Arc`.  Bounded — the trunk calls
/// this once, holding the guard for its whole attend loop.
pub fn publish(token: &Token) -> SlotGuard {
    std::mem::forget(token.0.clone());
    let flag: *const AtomicU8 = Arc::as_ptr(&token.0);
    let prev = CURRENT.swap(flag.cast_mut(), Ordering::Release);
    SlotGuard { prev }
}

/// RAII handle for the published slot: restores the prior publication on drop.
///
/// A swap, not a clear, so an inner publication nested inside an outer one
/// reveals the outer token again rather than leaving the slot null beneath it.
pub struct SlotGuard {
    prev: *mut AtomicU8,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        CURRENT.store(self.prev, Ordering::Release);
    }
}

/// True if the trunk's published slot reads cancelled — a test probe only, since
/// production observes cancellation through the threaded [`Token`].
#[cfg(all(test, unix))]
pub(crate) fn is_set() -> bool {
    let p = CURRENT.load(Ordering::Acquire);
    // SAFETY: a non-null slot points into the allocation `publish` leaks, so the
    // pointee outlives the process.
    !p.is_null() && unsafe { (*p).load(Ordering::Relaxed) } != 0
}

/// Raise `cause` on the trunk's published token, or nothing when none is
/// published.  Signal-handler safe: one atomic load plus a `fetch_max`.
fn raise(cause: CancelCause) {
    let p = CURRENT.load(Ordering::Acquire);
    if !p.is_null() {
        // SAFETY: a non-null slot points into the allocation `publish` leaks, so
        // the pointee outlives the process.
        unsafe { (*p).fetch_max(cause as u8, Ordering::Relaxed) };
    }
}

/// The trunk-only half of Esc/Ctrl-C: `Interrupt` on the published token, then
/// [`deliver_interrupt`] to unwind the exchange.  Any other focused tab goes
/// through [`crate::fleet::registry::AgentRegistry::interrupt`] instead, which
/// reaches that agent's own token rather than the slot.
pub fn raise_interrupt() {
    raise(CancelCause::Interrupt);
    deliver_interrupt();
}

/// Install the chained signal handler: SIGINT into ral's non-escalating
/// `relay_handler`, SIGTERM/SIGHUP into its escalating `term_handler`.
///
/// [`chained`] forwards through those static accessors rather than a captured
/// disposition, so running after `ral_core::process::install_handlers` is
/// convention, not a correctness requirement.
#[cfg(unix)]
pub fn install() {
    // SAFETY: `chained`'s body is a `fetch_max` on the published slot plus a
    // direct call into `relay_handler`/`term_handler`, both plain fn items —
    // async-signal-safe throughout.
    unsafe {
        libc::signal(libc::SIGINT, chained as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, chained as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, chained as *const () as libc::sighandler_t);
    }
}

#[cfg(not(any(unix, windows)))]
pub fn install() {}

#[cfg(unix)]
extern "C" fn chained(sig: libc::c_int) {
    if sig == libc::SIGINT {
        raise(CancelCause::Interrupt);
        ral_core::process::relay_handler()(sig);
    } else {
        raise(CancelCause::Terminate);
        ral_core::process::term_handler()(sig);
    }
}

/// Recreate the SIGINT raw mode swallowed — `ISIG` is off, so the kernel no
/// longer delivers one to the foreground job — then unwind ral's foreground.
///
/// A foreground external child gets a real SIGINT through its own process group
/// ([`ral_core::process::interrupt_foreground_child`]); signalling *another*
/// group carries no escalation.  For ral itself we cancel the run's foreground
/// scope, sparing detached workers (parented at the durable root) and ticking no
/// counter toward a third-signal `_exit`.
#[cfg(unix)]
fn deliver_interrupt() {
    ral_core::process::interrupt_foreground_child();
    ral_core::process::request_foreground_cancel(ral_core::process::CancelCause::Interrupt);
}

/// Raw mode suppresses the console's automatic Ctrl-C handling, so Esc and
/// Ctrl-C both surface as ordinary key events.  Call ral's non-escalating relay
/// in-process: re-injecting with `GenerateConsoleCtrlEvent` would broadcast to
/// the whole console group and re-enter `SetConsoleCtrlHandler`'s chain, ticking
/// ral's escalation counter on every trunk interrupt.
#[cfg(windows)]
fn deliver_interrupt() {
    ral_core::process::relay_interrupt();
}

#[cfg(not(any(unix, windows)))]
fn deliver_interrupt() {}

/// Whether a delivered Windows console-control event is an exchange-cancel
/// gesture — Ctrl-C or Ctrl-Break — that exarch's handler fully handles itself.
///
/// A pure function of the event code, so the decision is unit-testable on every
/// host: `SetConsoleCtrlHandler` cannot be exercised in a test, but what it
/// drives can.  The single bool answers "does this cancel the exchange" and
/// "does exarch report the event handled" at once, because exarch performs the
/// whole relay itself and must stop the event from reaching ral's escalating
/// disposition next in the list.
#[cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "only called from console_ctrl_handler, which is cfg(windows); \
                  exercised directly by this module's own tests on every host"
    )
)]
pub(crate) fn cancels_exchange(ctrl_type: u32) -> bool {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT
}

/// Set-once guard for `install`'s registration: `SetConsoleCtrlHandler(_, TRUE)`
/// *appends* a routine on every call, where Unix's `libc::signal` overwrites the
/// same disposition, so without this each `/clear` rebuild would add a copy.
#[cfg(windows)]
static WIN_CTRL_HANDLER_INSTALLED: std::sync::Once = std::sync::Once::new();

/// Register exarch's console-ctrl handler, once.
///
/// Must run after `ral_core::process::install_handlers`, so this routine sits
/// ahead of ral's in the list Windows runs newest-first.
#[cfg(windows)]
pub fn install() {
    WIN_CTRL_HANDLER_INSTALLED.call_once(|| {
        // SAFETY: `console_ctrl_handler` matches `PHANDLER_ROUTINE`'s
        // `unsafe extern "system" fn(u32) -> BOOL` signature; `TRUE` (`1`) adds
        // it to the process's handler list rather than removing it.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                Some(console_ctrl_handler),
                1,
            );
        }
    });
}

/// The routine Windows calls on Ctrl-C/Ctrl-Break/console-close events.
///
/// Runs on a dedicated OS thread rather than in signal context, so plain atomics
/// are all it needs.  A Ctrl-C/Ctrl-Break is handled here in full — the same
/// non-escalating relay `deliver_interrupt` calls for a raw-mode key event —
/// and reported `TRUE`; every other event reports `FALSE`, deferring to ral's
/// escalating disposition unchanged.
#[cfg(windows)]
extern "system" fn console_ctrl_handler(ctrl_type: u32) -> windows_sys::core::BOOL {
    if cancels_exchange(ctrl_type) {
        raise(CancelCause::Interrupt);
        ral_core::process::relay_interrupt();
        return windows_sys::Win32::Foundation::TRUE;
    }
    windows_sys::Win32::Foundation::FALSE
}

/// `cancels_exchange` is a plain function of a `u32` event code, so it is
/// exercised natively on every platform this crate builds for.
#[cfg(test)]
mod cancels_exchange_tests {
    use super::cancels_exchange;

    #[test]
    fn ctrl_c_and_ctrl_break_cancel_the_exchange() {
        assert!(cancels_exchange(0), "CTRL_C_EVENT cancels the exchange");
        assert!(cancels_exchange(1), "CTRL_BREAK_EVENT cancels the exchange");
    }

    #[test]
    fn other_console_events_never_cancel_the_exchange() {
        // CTRL_CLOSE_EVENT=2, CTRL_LOGOFF_EVENT=5, CTRL_SHUTDOWN_EVENT=6.
        for ctrl_type in [2, 5, 6] {
            assert!(
                !cancels_exchange(ctrl_type),
                "event {ctrl_type} is not an exchange-cancel signal, so ral's \
                 escalating disposition must still see it"
            );
        }
    }
}

/// `Token::cancel`/`Token::reset` are plain atomics over a `CancelCause`
/// encoding, so their escalation and reset semantics are exercised natively on
/// every platform — no signal handler, no slot.
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
    //! Unix-only: the chained-disposition design under test is Unix's, and
    //! Windows registers a console-handler routine instead — one no test can
    //! make `SetConsoleCtrlHandler` invoke.

    use super::*;
    use crate::agent::Agent;
    use std::sync::Mutex;

    /// These tests touch process-global state (the escalation ladder, the
    /// `CURRENT` slot), so they must not run concurrently.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// The ladder is ral's road to a force-exit; an Esc must never take a step
    /// down it.
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

    /// Non-escalation must hold however many times Esc is pressed, not just for
    /// the first.
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

    /// The boundary clears the sticky token's flag rather than swapping the
    /// `Arc`, so the slot keeps tracking that same token across exchanges.
    #[test]
    fn reset_clears_prior_exchange_cancel() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        let token = Token::new();
        let _slot = publish(&token);
        raise_interrupt();
        assert!(token.is_cancelled(), "Esc cancels the published token");
        assert!(is_set(), "the slot reports cancelled");
        token.reset();
        assert!(
            !token.is_cancelled(),
            "the boundary reset clears the prior exchange's Esc"
        );
        assert!(
            !is_set(),
            "the slot still tracks the (now-cleared) sticky token"
        );
        ral_core::process::clear();
    }

    /// The attend loop threads *clones* into `deliberate`/`run_batch`/tools, so
    /// an Esc landing mid-exchange must halt the in-flight tool call.
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

    /// Without `boot_shell`'s re-chaining, ral's bare handler would run alone
    /// after a rebuild: a delivered SIGINT would miss the token and tick the
    /// escalation ladder.
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

    /// A signal's cooperative delivery dies with the run that unwound on it, but
    /// its escalation tick would outlive the run — leaving a rebuilt session one
    /// delivery closer to the third-signal `_exit`.
    #[test]
    #[ignore = "invokes the SIGTERM handler, root-cancelling every published slot — driven in its own process by signal_delivery_tests_own_their_process"]
    fn clear_resets_the_escalation_ladder_on_reboot() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();

        let dir =
            std::env::temp_dir().join(format!("exarch-clear-interrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut session = Agent::for_test(&dir, "system").expect("test session");

        // Seed the ladder exactly as a delivered SIGTERM would.  No run is
        // running, so the published cancel slots are null and the delivery is
        // the tick alone.
        ral_core::process::term_handler()(libc::SIGTERM);
        assert!(
            ral_core::process::escalation_pending(),
            "the test must start with the stale escalation tick `/clear` used to inherit"
        );

        session
            .clear()
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
    }

    /// Drive the two `#[ignore]`d tests above in a child process they own.
    /// Delivered signals are process-wide, so in the parallel test binary they
    /// terminate whatever *other* test is mid-exchange; `SERIAL` cannot help,
    /// since the victims are readers that never know to take it.
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
        // Without the pass count, a renamed test would make the child silently
        // run nothing and still exit 0.
        assert!(
            out.status.success() && stdout.contains("2 passed"),
            "child signal tests failed or did not both run:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

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

    /// Stamping every signal `Interrupt` would misreport the cause during a real
    /// termination and let a park `UntilCancelled` on the trunk token survive
    /// its own SIGTERM.
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
