use super::*;
use crate::carrier::testkit::{BARE, boot_at};
use crate::process::{CancelCause, DurableRoot};
use crate::source::SourceOrigin;
use crate::test_helper::eventually;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(5);
const SLEEP: &str = "sh -c 'touch running; exec sleep 30'";

fn at(dir: &std::path::Path) -> IdentityTransport {
    boot_at(
        &BARE,
        &Attach::new(
            "bare",
            EnginePath::Host(dir.to_path_buf()),
            EnginePath::Host(dir.to_path_buf()),
        ),
    )
}

fn sleep(transport: &IdentityTransport, within: &Scope) -> Report {
    dispatch_to_report(
        transport,
        Run::captured(SLEEP, SourceOrigin::named("<test>")),
        Arc::new(()),
        Some(within),
    )
    .expect("an identity dispatch answers")
}

#[test]
fn cancelling_within_interrupts_an_in_flight_identity_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let transport = at(dir.path());
    let within = DurableRoot::default().child();

    let report = std::thread::scope(|threads| {
        let running = threads.spawn(|| sleep(&transport, &within));
        assert!(
            eventually(WAIT, || dir.path().join("running").exists().then_some(())).is_some(),
            "the child must start before cancellation"
        );
        within.cancel(CancelCause::Interrupted);
        assert!(
            eventually(WAIT, || running.is_finished().then_some(())).is_some(),
            "cancelling within must unwind the sleeping dispatch"
        );
        running.join().unwrap()
    });

    assert!(
        matches!(
            report,
            Report::Ran {
                ending: Ending::Stopped {
                    cause: CancelCause::Interrupted,
                    ..
                },
                ..
            }
        ),
        "the run must report Interrupted: {report:?}"
    );
    assert_eq!(transport.session_ended().unwrap(), None);
    assert!(matches!(
        crate::carrier::testkit::eval(&transport, "$[1 + 1]"),
        Report::Ran {
            ending: Ending::Settled { status: 0, .. },
            ..
        }
    ));
}

#[test]
fn terminate_reaches_a_foreign_scope_before_open_and_mid_run() {
    for before_open in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let transport = at(dir.path());
        let within = DurableRoot::default().child();
        if before_open {
            transport.control().terminate();
        }

        let report = std::thread::scope(|threads| {
            let running = threads.spawn(|| sleep(&transport, &within));
            if !before_open {
                assert!(
                    eventually(WAIT, || dir.path().join("running").exists().then_some(()))
                        .is_some(),
                    "the child must start before Terminate"
                );
                transport.control().terminate();
            }
            assert!(
                eventually(WAIT, || running.is_finished().then_some(())).is_some(),
                "Terminate must end a dispatch under a foreign scope; before_open={before_open}"
            );
            running.join().unwrap()
        });

        assert!(
            matches!(
                report,
                Report::Ran {
                    ending: Ending::Stopped {
                        cause: CancelCause::Terminated,
                        ..
                    },
                    ..
                }
            ),
            "the run must report Terminated; before_open={before_open}: {report:?}"
        );
        assert_eq!(dir.path().join("running").exists(), !before_open);
        assert_eq!(within.cause(), None, "the foreign parent stays live");
        assert_eq!(transport.session_ended().unwrap(), Some(143));
    }
}
