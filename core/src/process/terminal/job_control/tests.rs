use super::*;
use crate::process::{DurableRoot, Pgid, Scope, TerminalAccess, TerminalLease};

#[test]
fn command_follows_the_four_owner_rows() {
    crate::test_helper::subprocess::isolated(
        "process::terminal::job_control::tests::command_follows_the_four_owner_rows",
        || {
            let home = DurableRoot::default().child();
            let group = Pgid::from_raw(1).expect("positive group");
            let placed = Placement::command(GroupOwner::Stage(Membership::new(
                group,
                Scope::clone(&home),
            )));
            assert!(matches!(placed, Placement::Join(m) if m.group() == group));
            let lease = TerminalLease::mint_at_startup(true).expect("lease");
            for interactive in [false, true] {
                let permit = TerminalAccess::Leased.reach(Some(&lease), &home, true);
                assert!(matches!(
                    Placement::command(GroupOwner::Session {
                        permit,
                        interactive
                    }),
                    Placement::Tenant(_)
                ));
                let placed = Placement::command(GroupOwner::Session {
                    permit: None,
                    interactive,
                });
                assert!(matches!(
                    (interactive, placed),
                    (true, Placement::Inherit) | (false, Placement::NewSession)
                ));
            }
        },
    );
}

#[test]
fn input_follows_every_requested_placement() {
    crate::test_helper::subprocess::isolated(
        "process::terminal::job_control::tests::input_follows_every_requested_placement",
        || {
            let home = DurableRoot::default().child();
            let lease = TerminalLease::mint_at_startup(true).expect("lease");
            let permit = TerminalAccess::Leased
                .reach(Some(&lease), &home, true)
                .expect("permit");
            for (placement, readable) in [
                (Placement::Inherit, true),
                (Placement::NewLeader, false),
                (Placement::NewSession, false),
                (
                    Placement::Join(Membership::new(
                        Pgid::from_raw(1).unwrap(),
                        Scope::clone(&home),
                    )),
                    true,
                ),
                (
                    Placement::Tenant(PreparedTerminalLoan::capture(permit)),
                    true,
                ),
            ] {
                assert_eq!(
                    matches!(placement.input(), TerminalInput::Readable),
                    readable
                );
                #[cfg(unix)]
                if matches!(placement, Placement::Tenant(_)) {
                    assert!(matches!(
                        placement.step(),
                        crate::process::group::Step::NewLeader
                    ));
                }
            }
        },
    );
}
