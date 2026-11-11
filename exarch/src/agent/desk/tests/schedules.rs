//! The schedules family.

use super::*;

/// Refused before anything is armed, naming the flag that grants — and
/// naming the tag the model typed, not a wire word it never saw.
#[test]
fn every_schedule_tag_is_refused_in_the_models_own_vocabulary() {
    for (request, verb) in [
        (add_req("1s", "nightly", "wake"), "exarch-schedules `add"),
        (
            Request::Schedules(Schedules::List),
            "exarch-schedules `list",
        ),
        (remove_req("sched-0"), "exarch-schedules `remove"),
    ] {
        with_desk(|desk, _agent| {
            let err = desk
                .ask(request)
                .expect_err("every schedule tag is refused without the grant");
            assert_eq!(
                err.reason,
                ral_core::fact::Reason::Host(Box::new(ral_core::fact::HostFailure::Exarch(
                    ral_core::fact::ExarchFailure::Schedule(
                        ral_core::fact::ScheduleFailure::NoGrant(
                            ral_core::fact::ScheduleFailureNoGrant { verb: verb.into() }
                        )
                    )
                )))
            );
            assert_eq!(err.hints(), []);
            assert_eq!(
                err.describe(),
                format!("exarch: schedule: {verb}: scheduling is not allowed in this session")
            );
            assert!(
                err.plain().contains("--allow-schedule"),
                "must name the grant flag, got: {}",
                err.describe()
            );
        });
    }
}

/// Arm one, see the answer already list it with the fields it was given,
/// remove it by label, see the table empty again — every tag answers the
/// table, so no tag needs a second call to learn what it did.
#[test]
fn schedules_lists_what_add_registered() {
    with_granted_desk(|desk, _agent| {
        let rows = table(
            desk.ask(add_req("2h", "nightly", "wake"))
                .expect("a valid `add must succeed"),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(str_field(&rows[0], "label"), Some("nightly"));
        assert_eq!(str_field(&rows[0], "trigger"), Some("after 2h"));
        assert!(rows[0].field("id").is_none(), "the table carries no id");

        let listed = table(
            desk.ask(Request::Schedules(Schedules::List))
                .expect("`list must succeed"),
        );
        assert_eq!(listed.len(), 1, "`list sees what `add armed");

        let after_removal = table(
            desk.ask(remove_req("nightly"))
                .expect("`remove must succeed"),
        );
        assert!(
            after_removal.is_empty(),
            "the wakeup must be gone from the very table `remove answers"
        );
    });
}

/// A duplicate label is refused and registers nothing.  Its row goes up after
/// the registry call precisely so `failed` can carry the outcome; emitted
/// before, every schedule would read as one that landed.
#[test]
fn a_duplicate_schedule_is_refused_and_tiers_its_act_row() {
    let (tx, rx) = channel();
    with_granted_desk(|mut desk, _agent| {
        desk.loan
            .get()
            .unwrap()
            .log
            .record_emitter()
            .attach(Box::new(FleetSink {
                id: AgentId::new(0),
                tx: tx.downgrade(),
                meter: crate::bus::UsageMeter::default(),
            }));
        desk.call.emit = Emitter::new(tx, AgentId::new(0));
        desk.ask(add_req("1s", "nightly", "wake"))
            .expect("the first schedule must succeed");
        let err = desk
            .ask(add_req("1s", "nightly", "wake"))
            .expect_err("a duplicate label must be refused");
        assert!(
            err.describe().contains("nightly"),
            "got: {}",
            err.describe()
        );
        assert_eq!(
            table(desk.ask(Request::Schedules(Schedules::List)).unwrap()).len(),
            1
        );

        let acts: Vec<(String, bool)> = crate::bus::drain_records(&rx)
            .into_iter()
            .filter_map(|rec| match rec {
                Record::Display(Display::HarnessCall {
                    verb,
                    subject,
                    payload,
                    failed,
                }) if verb == "schedule" => {
                    assert_eq!(subject.as_deref(), Some("nightly"));
                    Some((payload, failed))
                }
                _ => None,
            })
            .collect();
        assert_eq!(acts.len(), 2, "both attempts draw a row: {acts:?}");
        assert!(!acts[0].1, "the schedule that landed is not tiered");
        assert_eq!(acts[0].0, "after 1s", "a landed row carries its trigger");
        assert!(acts[1].1, "the refused schedule is tiered hot");
        assert!(
            acts[1].0.starts_with("refused: "),
            "a refusal states itself in the payload: {:?}",
            acts[1].0
        );
    });
}

#[test]
#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "method items do not satisfy try_transact's higher-ranked lifetime"
)]
fn schedules_list_reads_the_cell_even_when_the_timer_resource_is_empty() {
    with_granted_desk(|desk, agent| {
        desk.ask(add_req("2h", "fold-backed", "wake")).unwrap();
        // Dropping timers changes no recorded booking.
        desk.loan.get().unwrap().schedules.clear();
        let rows = table(desk.ask(Request::Schedules(Schedules::List)).unwrap());
        assert_eq!(rows.len(), 1);
        assert_eq!(str_field(&rows[0], "label"), Some("fold-backed"));
        assert_eq!(str_field(&rows[0], "trigger"), Some("after 2h"));
        agent.mailbox.try_transact(|txn| txn.clear()).unwrap();
        assert_eq!(
            table(desk.ask(Request::Schedules(Schedules::List)).unwrap()),
            Vec::<FOValue>::new()
        );
    });
}

#[test]
fn schedule_logical_refusal_is_not_a_recording_transaction_failure() {
    use ral_core::fact::{
        ExarchFailure, HostFailure, Reason, ScheduleFailure, ScheduleFailureDuplicateLiveLabel,
    };
    with_granted_desk(|desk, _| {
        desk.ask(add_req("1h", "same", "first")).unwrap();
        let error = desk.ask(add_req("1h", "same", "second")).unwrap_err();
        assert_eq!(
            error.reason,
            Reason::Host(Box::new(HostFailure::Exarch(ExarchFailure::Schedule(
                ScheduleFailure::DuplicateLiveLabel(ScheduleFailureDuplicateLiveLabel {
                    label: "same".into()
                })
            ))))
        );
        assert!(desk.ask(remove_req("absent")).is_ok());
    });
}

#[test]
fn schedule_inner_append_and_outer_transaction_failures_keep_label_trigger_and_cause() {
    use ral_core::fact::{ExarchFailure, HostFailure, Reason, ScheduleFailure};
    for poison in [false, true] {
        let actual = crate::agent::Agent::for_test_with(crate::agent::TestLaunch {
            allow_schedule: true,
            ..crate::agent::TestLaunch::new("system")
        })
        .unwrap();
        with_granted_desk(|mut desk, _| {
            desk.call.handle = actual.handle();
            desk.call.fleet = actual.fleet.clone();
            let recorder = actual.recorder();
            if poison {
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _ = actual
                            .handle()
                            .0
                            .address
                            .transact(|_| panic!("poison schedule transaction"));
                    }))
                    .is_err()
                );
            } else {
                recorder.fail_after(0);
            }
            let Request::Schedules(Schedules::Add(add)) = add_req("1h", "never-armed", "wake")
            else {
                unreachable!()
            };
            let error = desk.schedule(add).unwrap_err();
            let why = recorder.check_available().unwrap_err().failure();
            assert!(matches!(&error.reason, Reason::Host(host)
                if matches!(&**host, HostFailure::Exarch(ExarchFailure::Schedule(ScheduleFailure::Recording(failure)))
                    if failure.label.as_deref() == Some("never-armed") && failure.trigger.as_deref() == Some("after 1h") && failure.cause == why)));
            assert_eq!(error.hints(), []);
        });
    }
}
