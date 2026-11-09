use super::*;
use crate::process::{Membership, Pgid, TerminalLease};

fn leased_shell() -> (Shell, Mooring) {
    let mut shell = crate::test_helper::core_shell();
    shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
    let mooring = Mooring {
        terminal_access: TerminalAccess::Leased,
        ..Mooring::adrift()
    };
    (shell, mooring)
}

#[test]
fn unavailable_preparation_still_leads_a_group_without_terminal_input() {
    // Windows can fail to capture every console mode despite lease authority.
    for interactive in [false, true] {
        let placed = Placement::from_preparation(LoanPreparation::Unavailable, interactive);
        assert!(matches!(
            placed,
            Placement::Unloaned {
                terminal_input: TerminalInput::Withhold
            }
        ));
        assert!(matches!(placed.policy(), PgidPolicy::NewLeader));
        assert!(matches!(placed.stdin(&Source::Terminal), Source::Empty));
        assert!(placed.into_loan().is_none());
    }
}

#[test]
fn ineligible_preparation_inherits_only_for_interactive_commands() {
    for interactive in [false, true] {
        let placed = Placement::from_preparation(LoanPreparation::NotEligible, interactive);
        assert_eq!(matches!(placed, Placement::Inherit), interactive);
        assert_eq!(matches!(placed, Placement::Headless), !interactive);
        assert_eq!(
            matches!(placed.stdin(&Source::Terminal), Source::Terminal),
            interactive
        );
        assert!(placed.into_loan().is_none());
    }
}

#[test]
fn joined_pipelines_never_prepare_another_loan_even_when_explicit() {
    let (mut shell, mut mooring) = leased_shell();
    shell.session.stage = Some(Membership::new(
        Pgid::from_raw(1).expect("positive group"),
        shell.scope.clone(),
    ));
    mooring.terminal_access = TerminalAccess::ExplicitLoan;
    assert!(matches!(
        prepare_terminal_loan(&shell, &mooring),
        LoanPreparation::NotEligible
    ));
}

#[test]
fn shared_preparation_requires_both_authority_and_a_foreground_destination() {
    let (mut shell, mut mooring) = leased_shell();
    shell.io.stdout = crate::io::new_buffer().0;
    assert!(matches!(
        prepare_terminal_loan(&shell, &mooring),
        LoanPreparation::NotEligible
    ));
    mooring.terminal_access = TerminalAccess::ExplicitLoan;
    shell.session.terminal_lease = None;
    assert!(matches!(
        prepare_terminal_loan(&shell, &mooring),
        LoanPreparation::NotEligible
    ));
    shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
    shell.io.stdout = Sink::Stdout;
    mooring.terminal_access = TerminalAccess::Denied;
    assert!(matches!(
        prepare_terminal_loan(&shell, &mooring),
        LoanPreparation::NotEligible
    ));
}

#[cfg(unix)]
#[test]
fn shared_preparation_allows_explicit_capture_and_noninteractive_scripts() {
    let (mut shell, mut mooring) = leased_shell();
    shell.session.interactive = false;
    assert!(matches!(
        prepare_terminal_loan(&shell, &mooring),
        LoanPreparation::Prepared(_)
    ));
    shell.io.stdout = crate::io::new_buffer().0;
    mooring.terminal_access = TerminalAccess::ExplicitLoan;
    assert!(matches!(
        prepare_terminal_loan(&shell, &mooring),
        LoanPreparation::Prepared(_)
    ));
}

#[cfg(unix)]
#[test]
fn every_mode_preserves_nonterminal_sources() {
    let (shell, mooring) = leased_shell();
    let (reader, _writer) = crate::process::cloexec_pipe().expect("pipe");
    let reader = Source::Reader(crate::io::SourceReader::pipe(reader));
    let modes = [
        Placement::Joined(Membership::new(
            Pgid::from_raw(1).expect("positive group"),
            shell.scope.clone(),
        )),
        Placement::for_command(&shell, &mooring, false),
        Placement::Inherit,
        Placement::Headless,
        Placement::Unloaned {
            terminal_input: TerminalInput::Retain,
        },
        Placement::Unloaned {
            terminal_input: TerminalInput::Withhold,
        },
    ];
    assert!(matches!(modes[1], Placement::Foreground(_)));
    for mode in modes {
        assert!(std::ptr::eq(mode.stdin(&reader), &raw const reader));
        let empty = Source::Empty;
        assert!(std::ptr::eq(mode.stdin(&empty), &raw const empty));
    }
}
