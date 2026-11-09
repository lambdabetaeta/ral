use super::*;
use crate::process::{DurableRoot, TerminalAccess, TerminalLease};
use crate::test_helper::subprocess;

#[cfg(unix)]
#[path = "tests/unix.rs"]
mod unix;

fn prepared(home: &ScopeNode) -> PreparedTerminalLoan {
    let lease = TerminalLease::mint_at_startup(true).unwrap();
    let permit = TerminalAccess::ExplicitLoan
        .reach(Some(&lease), home, false)
        .unwrap();
    PreparedTerminalLoan::capture(permit)
}

fn interrupt() -> Signal {
    #[cfg(unix)]
    {
        Signal::new(libc::SIGINT)
    }
    #[cfg(windows)]
    {
        Signal::new(windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT)
    }
}

#[test]
fn default_hold_is_inert() {
    let mut terminal = TerminalHold::default();
    assert!(matches!(terminal.0, Hold::Unheld));
    terminal.hear(interrupt());
    assert_eq!(terminal.pressed(), None);
    terminal.reclaim(Some(interrupt()));
}

#[test]
fn lent_reclaim_hears_only_a_present_last_gesture() {
    subprocess::isolated(
        "process::terminal::loan::tests::lent_reclaim_hears_only_a_present_last_gesture",
        || {
            for last in [None, Some(interrupt())] {
                let home = DurableRoot::default().child();
                let terminal = prepared(&home).assumed();
                assert!(matches!(terminal.0, Hold::Lent(_)));
                assert_eq!(terminal.pressed(), None);
                assert_eq!(home.cause(), None);
                terminal.reclaim(last);
                #[cfg(unix)]
                assert_eq!(home.cause(), last.map(|_| CancelCause::Interrupted));
                // Console deaths report no new key.
                #[cfg(windows)]
                assert_eq!(home.cause(), None);
            }
        },
    );
}
