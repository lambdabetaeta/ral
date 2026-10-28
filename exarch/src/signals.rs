//! The process's signals, and the roots that hear them.
//!
//! No engine faces the process's signals itself.  ral's handlers raise ambient
//! causes, and [`face`] forwards them to every live root of the fleet as that
//! agent's own interrupt or cancel: OS signals reach every root.
//!
//! Esc and Ctrl-C are the other way round: a per-tab exchange interrupt on the
//! one focused agent, never a cascade, never an agent's death, and they never
//! tick ral's escalation ladder.  They route through
//! [`crate::agent::AgentHandle::interrupt`], which cancels only its exchange.
//!
//! On Unix [`install`] points SIGINT at ral's *non-escalating*
//! `interrupt_handler`, never the `term_handler` whose third delivery
//! `_exit`s: a stray SIGINT reaching the supervising TUI must cancel the
//! exchange, not kill exarch.  SIGTERM and SIGHUP keep `term_handler`, and
//! cancel the fleet root, so every owner and dispatch observes the same cause.
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
//! worker's children are never reached.
//!
//! On both, [`crate::boot::face_process_signals`] owns the install
//! ceremony, once per process.

use crate::agent::fleet::Fleet;
use ral_core::process::{Ambient, AmbientForward};
use std::sync::Arc;

/// Forward this process's signals to every live root of `fleet` for as long
/// as the guard lives: an interrupt unwinds each exchange, a shutdown ends it.
pub(crate) fn face(fleet: &Arc<Fleet>) -> AmbientForward {
    let fleet = Arc::clone(fleet);
    ral_core::process::forward_ambient(move |ambient| match ambient {
        Ambient::Interrupt => {
            for root in fleet.roots() {
                root.interrupt();
            }
        }
        Ambient::Root(cause) => fleet.end(cause),
    })
}

/// Point SIGINT at ral's non-escalating `interrupt_handler`, leaving SIGTERM
/// and SIGHUP on the escalating `term_handler` `install_handlers` set.
#[cfg(unix)]
pub(crate) fn install() {
    ral_core::process::install(libc::SIGINT, ral_core::process::interrupt_handler());
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
#[cfg(any(windows, test))]
fn cancels_exchange(ctrl_type: u32) -> bool {
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
pub(crate) fn install() {
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
/// are all it needs.  A Ctrl-C/Ctrl-Break is handled here in full — the
/// non-escalating interrupt, fanned out by [`face`] to every root — and
/// reported `TRUE`; every other event reports `FALSE`, deferring to
/// ral's escalating disposition unchanged.
#[cfg(windows)]
extern "system" fn console_ctrl_handler(ctrl_type: u32) -> windows_sys::core::BOOL {
    if cancels_exchange(ctrl_type) {
        ral_core::process::request_interrupt();
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

#[cfg(all(test, unix))]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    //! Tests for exarch's signal dispositions.
    //!
    //! Unix-only: the disposition design under test is Unix's, and Windows
    //! registers a console-handler routine instead — one no test can make
    //! `SetConsoleCtrlHandler` invoke.

    use super::*;
    use crate::agent::testkit::{TestAgentSpec, test_agent};
    use ral_core::test_helper::eventually;
    use std::time::Duration;

    /// Without the entry ceremony, ral's bare handler would run alone: a
    /// delivered SIGINT would tick the escalation ladder. With it, the SIGINT
    /// reaches the faced trunk as an interrupt, and a SIGTERM as its end.
    #[test]
    #[ignore = "delivers a real process-wide SIGINT and SIGTERM: driven in its own process by signal_delivery_tests_own_their_process"]
    fn a_delivered_signal_reaches_the_faced_trunk_without_escalating() {
        ral_core::process::clear();
        // A raw ral install models any clobber before the exarch session
        // constructor runs.
        ral_core::process::install_handlers();
        crate::boot::face_process_signals(&ral_core::io::terminal::TerminalState::default());
        let fleet = crate::agent::fleet::Fleet::for_test();
        let trunk = test_agent(&fleet, TestAgentSpec::new("trunk")).expect("a fresh trunk");
        trunk.steer("open an exchange".into()).unwrap();
        assert!(
            trunk
                .inbox
                .draw(|_| crate::bus::ParkMode::Quiesce)
                .is_some()
        );
        trunk.inbox.open_exchange();
        let exchange = trunk.inbox.exchange();
        let _signals = face(&fleet);

        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0, "raise SIGINT");
        assert!(
            // A forwarded signal lands on the forwarder's own thread.
            eventually(Duration::from_secs(10), || exchange
                .is_cancelled()
                .then_some(()))
            .is_some(),
            "a delivered SIGINT must reach the faced trunk"
        );
        assert!(
            trunk.is_live(),
            "a SIGINT interrupts the trunk, never ends it"
        );
        assert!(
            !ral_core::process::escalation_pending(),
            "SIGINT routes into the non-escalating handler, never the force-exit ladder"
        );

        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0, "raise SIGTERM");
        assert!(
            eventually(Duration::from_secs(10), || (!trunk.is_live()).then_some(())).is_some(),
            "a delivered SIGTERM must end the faced trunk"
        );
        ral_core::process::clear();
    }

    /// Drive the `#[ignore]`d test above in a child process of its own.
    /// Delivered signals are process-wide, so in the parallel test binary they
    /// would reach whatever *other* test is mid-exchange.
    #[test]
    fn signal_delivery_tests_own_their_process() {
        let exe = std::env::current_exe().expect("test binary path");
        let name = "signals::tests::a_delivered_signal_reaches_the_faced_trunk_without_escalating";
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
