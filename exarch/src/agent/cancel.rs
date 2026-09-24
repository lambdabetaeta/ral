//! Exarch's per-exchange cancellation, and the process signals it hears.
//!
//! Every agent holds one sticky [`Token`] for its life, registered in the fleet
//! so the subtree cascade always reaches the live exchange; the attend loop
//! [`reset`](Token::reset)s it at each genuine exchange boundary, so a prior
//! exchange's Esc never bleeds into the next.  The cause a token carries decides
//! its reach: an `Interrupt` drops only the in-flight exchange and the agent
//! re-parks, anything stronger ends the agent.
//!
//! Esc and Ctrl-C are a per-tab exchange interrupt — never a cascade, never an
//! agent's death, and they never tick ral's escalation ladder.  Every focused
//! tab routes through [`crate::agent::Agent::interrupt`], which cancels that
//! agent's own token and, through `Control`, its dispatch in flight, never its
//! durable root; the trunk additionally calls [`raise_interrupt`], which on
//! Unix re-creates the SIGINT a foreground external child would have received
//! and on Windows raises ral's interrupt alone.
//!
//! No engine faces the process's signals itself.  ral's handlers raise ambient
//! causes, and [`face`] forwards them to the trunk as that agent's own
//! interrupt or cancel.  On Unix [`install`] points SIGINT at ral's
//! *non-escalating* `interrupt_handler`, never the `term_handler` whose third
//! delivery `_exit`s: a stray SIGINT reaching the supervising TUI must cancel
//! the exchange, not kill exarch.  SIGTERM and SIGHUP keep `term_handler`, and
//! reach the trunk as `Terminate`, so a park reading the token agrees with the
//! engine's root about why the agent is ending.
//!
//! Windows has no single disposition to replace — `SetConsoleCtrlHandler` keeps
//! a list, run last-registered-first until one returns `TRUE` — so there
//! `install` must run after `ral_core::process::install_handlers` to sit ahead
//! of ral's `ctrlc` routine, a correctness requirement rather than the Unix
//! convention.  Esc and Ctrl-C never reach that list anyway: raw mode clears
//! `ENABLE_PROCESSED_INPUT` and both arrive as ordinary key events.  The
//! registration earns its keep for Ctrl-Break, which raises a console event
//! regardless, and for the termination events, which have no key-event twin.
//! Neither path signals a process: a tool child, a console group of its own,
//! hears Ctrl-Break only from the teardown of the run that owns it, so a
//! detached worker's children are never reached.
//! On both, [`crate::bootstrap::face_process_signals`] owns the install
//! ceremony, once per process.

use ral_core::process::{Ambient, AmbientForward, CancelCause};
use ral_core::sync::LockExt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Weak};

/// An agent's cancellation handle.
///
/// Clones share one flag, so a cascade and the attend loop hold the same
/// token: cancelling either halts the agent's exchange.
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
            CancelCause::Interrupt as u8,
            0,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

/// Forward this process's signals to `trunk` for as long as the guard lives:
/// an interrupt unwinds its exchange, a shutdown ends it.
pub(crate) fn face(trunk: &Arc<crate::agent::Agent>) -> AmbientForward {
    let trunk = Arc::downgrade(trunk);
    ral_core::process::forward_ambient(move |ambient| hear(&trunk, ambient))
}

fn hear(trunk: &Weak<crate::agent::Agent>, ambient: Ambient) {
    let Some(trunk) = trunk.upgrade() else { return };
    match ambient {
        Ambient::Interrupt => trunk.interrupt(),
        Ambient::Root(cause) => trunk.cancel(cause),
    }
}

/// Point SIGINT at ral's non-escalating `interrupt_handler`, leaving SIGTERM
/// and SIGHUP on the escalating `term_handler` `install_handlers` set.
#[cfg(unix)]
pub fn install() {
    // SAFETY: `interrupt_handler` is a plain fn item doing one atomic
    // read-modify-write and a `write(2)` — async-signal-safe throughout.
    unsafe {
        libc::signal(
            libc::SIGINT,
            ral_core::process::interrupt_handler() as *const () as libc::sighandler_t,
        );
    }
}

/// The process-wide half of the trunk's interrupt, beside the per-agent one
/// every focused tab gets.
///
/// Recreate the SIGINT raw mode swallowed — `ISIG` is off, so the kernel no
/// longer delivers one to the foreground job. A foreground external child gets a real SIGINT through its own process group
/// ([`ral_core::process::interrupt_foreground_child`]); signalling *another*
/// group carries no escalation.  ral itself hears the interrupt through the
/// trunk's `Control`, which spares detached workers and ticks no counter
/// toward a third-signal `_exit`.
#[cfg(unix)]
pub fn raise_interrupt() {
    ral_core::process::interrupt_foreground_child();
}

/// Raise ral's interrupt in-process, as its own console handler would.
///
/// Raw mode suppresses the console's automatic Ctrl-C handling, so Esc and
/// Ctrl-C both surface as ordinary key events.  Re-injecting with
/// `GenerateConsoleCtrlEvent` would broadcast to the whole console group and
/// re-enter `SetConsoleCtrlHandler`'s chain, ticking ral's escalation counter
/// on every trunk interrupt.  A child hears it only through the teardown of
/// the scope that owns it.
#[cfg(windows)]
pub fn raise_interrupt() {
    ral_core::process::request_interrupt();
}

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
/// same disposition, so without this each call would add a copy.
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
/// non-escalating interrupt `raise_interrupt` raises for a raw-mode key event
/// — and reported `TRUE`; every other event reports `FALSE`, deferring to
/// ral's escalating disposition unchanged.
#[cfg(windows)]
extern "system" fn console_ctrl_handler(ctrl_type: u32) -> windows_sys::core::BOOL {
    if cancels_exchange(ctrl_type) {
        ral_core::process::request_interrupt();
        return windows_sys::Win32::Foundation::TRUE;
    }
    windows_sys::Win32::Foundation::FALSE
}

/// Where an agent's interrupt and terminate land: its seat's current control
/// sender, republished at every rebuild, so the cell outlives any one engine.
#[derive(Clone)]
pub(crate) struct InterruptTarget(Arc<std::sync::Mutex<ral_core::protocol::ControlSender>>);

impl InterruptTarget {
    pub(crate) fn new(control: ral_core::protocol::ControlSender) -> Self {
        Self(Arc::new(std::sync::Mutex::new(control)))
    }

    pub(crate) fn republish(&self, control: ral_core::protocol::ControlSender) {
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

/// `hear` is the whole mapping from an ambient cause to the trunk, so it is
/// exercised directly: raising a real root request would latch it for every
/// later forwarder in the test binary.
#[cfg(test)]
mod hear_tests {
    use super::*;
    use crate::agent::testkit::{TestAgentSpec, test_agent};

    #[test]
    fn each_ambient_cause_reaches_the_trunk_as_its_own() {
        let fleet = crate::fleet::Fleet::new();
        let trunk = test_agent(&fleet, TestAgentSpec::new("trunk")).expect("a fresh trunk");
        let weak = Arc::downgrade(&trunk);

        hear(&weak, Ambient::Interrupt);
        assert_eq!(
            trunk.cancel_token().0.load(Ordering::Relaxed),
            CancelCause::Interrupt as u8,
            "SIGINT interrupts the trunk's exchange"
        );
        assert!(
            !trunk.cancel_token().terminated(),
            "a SIGINT-driven Interrupt never terminates the agent"
        );

        hear(&weak, Ambient::Root(CancelCause::Terminate));
        assert_eq!(
            trunk.cancel_token().0.load(Ordering::Relaxed),
            CancelCause::Terminate as u8,
            "SIGTERM stamps Terminate, not Interrupt"
        );
        assert!(
            trunk.cancel_token().terminated(),
            "a SIGTERM-driven Terminate ends the agent"
        );
    }

    #[test]
    fn a_settled_trunk_hears_nothing() {
        let fleet = crate::fleet::Fleet::new();
        let trunk = test_agent(&fleet, TestAgentSpec::new("trunk")).expect("a fresh trunk");
        let weak = Arc::downgrade(&trunk);
        drop(trunk);
        hear(&weak, Ambient::Root(CancelCause::Terminate));
    }
}

#[cfg(all(test, unix))]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    //! Tests for exarch's signal dispositions.
    //!
    //! Unix-only: the disposition design under test is Unix's, and Windows
    //! registers a console-handler routine instead — one no test can make
    //! `SetConsoleCtrlHandler` invoke.

    use super::*;
    use crate::agent::Avatar;
    use crate::agent::testkit::{TestAgentSpec, test_agent};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// These tests touch the escalation ladder, so they must not run
    /// concurrently.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// The ladder is ral's road to a force-exit; however many times Esc is
    /// pressed, it must never take a step down it.
    #[test]
    fn raise_interrupt_never_ticks_the_escalation_ladder() {
        let _g = SERIAL.lock().unwrap();
        ral_core::process::clear();
        install();
        for _ in 0..5 {
            raise_interrupt();
        }
        assert!(
            !ral_core::process::escalation_pending(),
            "the Esc path never ticks the escalation ladder, so it cannot force-exit"
        );
        ral_core::process::clear();
    }

    /// Poll `done` until it holds or a generous deadline passes: a forwarded
    /// signal lands on the forwarder's own thread.
    fn eventually(done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        done()
    }

    /// Without the entry ceremony, ral's bare handler would run alone: a
    /// delivered SIGINT would tick the escalation ladder. With it, the SIGINT
    /// reaches the faced trunk as an interrupt, and a SIGTERM as its end.
    #[test]
    #[ignore = "delivers a real process-wide SIGINT and SIGTERM — driven in its own process by signal_delivery_tests_own_their_process"]
    fn a_delivered_signal_reaches_the_faced_trunk_without_escalating() {
        ral_core::process::clear();
        // A raw ral install models any clobber before the exarch session
        // constructor runs.
        ral_core::process::install_handlers();
        crate::bootstrap::face_process_signals(&ral_core::io::TerminalState::default());
        let fleet = crate::fleet::Fleet::new();
        let trunk = test_agent(&fleet, TestAgentSpec::new("trunk")).expect("a fresh trunk");
        let _signals = face(&trunk);

        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0, "raise SIGINT");
        assert!(
            eventually(|| trunk.cancel_token().is_cancelled()),
            "a delivered SIGINT must reach the faced trunk"
        );
        assert!(
            !trunk.cancel_token().terminated(),
            "a SIGINT interrupts the trunk, never ends it"
        );
        assert!(
            !ral_core::process::escalation_pending(),
            "SIGINT routes into the non-escalating handler, never the force-exit ladder"
        );

        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0, "raise SIGTERM");
        assert!(
            eventually(|| trunk.cancel_token().terminated()),
            "a delivered SIGTERM must end the faced trunk"
        );
        ral_core::process::clear();
    }

    /// A signal's cooperative delivery dies with the run that unwound on it, but
    /// its escalation tick would outlive the run — leaving a rebuilt session one
    /// delivery closer to the third-signal `_exit`.
    #[test]
    #[ignore = "invokes the SIGTERM handler, latching the process's shutdown request — driven in its own process by signal_delivery_tests_own_their_process"]
    fn clear_resets_the_escalation_ladder_on_reboot() {
        ral_core::process::clear();

        let mut session = Avatar::for_test("system").expect("test session");

        // Seed the ladder exactly as a delivered SIGTERM would.  Nothing
        // forwards the ambient causes here, so the delivery is the tick alone.
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
        ral_core::process::clear();
    }

    /// Drive each `#[ignore]`d test above in a child process of its own.
    /// Delivered signals and the shutdown latch are process-wide, so in the
    /// parallel test binary they would reach whatever *other* test is
    /// mid-exchange, and each other.
    #[test]
    fn signal_delivery_tests_own_their_process() {
        let exe = std::env::current_exe().expect("test binary path");
        for name in [
            "agent::cancel::tests::a_delivered_signal_reaches_the_faced_trunk_without_escalating",
            "agent::cancel::tests::clear_resets_the_escalation_ladder_on_reboot",
        ] {
            let out = std::process::Command::new(&exe)
                .args(["--exact", name, "--ignored"])
                .output()
                .expect("spawn the child test process");
            let stdout = String::from_utf8_lossy(&out.stdout);
            // Without the pass count, a renamed test would make the child
            // silently run nothing and still exit 0.
            assert!(
                out.status.success() && stdout.contains("1 passed"),
                "child signal test {name} failed or did not run:\n{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
